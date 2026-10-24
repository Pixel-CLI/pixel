// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel doctor` — checks install state, binary path, daemon health, and
//! index/graph/history freshness, reporting green/yellow/red per check.
//!
//! Every check is listed in [`CHECKS`] with the command that repairs it, so a
//! caller can select checks by id (`--only`, `--skip`) and act on a finding
//! without parsing its prose. [`repair_plan`] folds the flagged checks into
//! one run of each distinct command, which `pixel doctor --fix` executes.
//!
//! This module reads files only. What needs the daemon or the stores (the
//! `daemon.*` and `*.freshness` checks) is run by the `pixel` binary through the
//! `live` argument of [`doctor`], on the same [`Runner`], selection and report.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::InstallError;
use crate::config;
use crate::hosts::Host;
use crate::install;
use crate::routing;

pub type Result<T> = std::result::Result<T, InstallError>;

pub use crate::install::CheckStatus;

/// One check the doctor knows: its stable id and the command that repairs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CheckSpec {
    pub id: &'static str,
    /// Shell command that repairs a yellow or red outcome, `{root}` standing
    /// for the quoted repository root; `None` when no command can.
    pub fix: Option<&'static str>,
}

/// A catalogue entry; keeps [`CHECKS`] one line per check.
const fn entry(id: &'static str, fix: Option<&'static str>) -> CheckSpec {
    CheckSpec { id, fix }
}

/// Command that rewrites everything `pixel setup` deploys in the home.
const FIX_INSTALL: Option<&str> = Some("pixel setup");
/// Command that builds the text index and the graph, and warms the daemon.
const FIX_PREPARE: Option<&str> = Some("pixel prepare {root}");

/// Every check, in the order a run reports them. Ids are stable: scripts
/// select them with `--only`/`--skip`, and a check missing from this list
/// panics when it runs.
pub const CHECKS: &[CheckSpec] = &[
    entry("binary.path", None),
    entry("binary.executable", None),
    entry("binary.shell-path", None),
    entry("install.claude-hooks", FIX_INSTALL),
    entry("install.codex-hooks", FIX_INSTALL),
    // Only the user can review a hook (`/hooks` in Codex); the outcome says so.
    entry("install.codex-hook-review", None),
    entry("install.devin", FIX_INSTALL),
    entry("install.gemini", FIX_INSTALL),
    entry("install.antigravity", FIX_INSTALL),
    entry("install.opencode", FIX_INSTALL),
    entry("install.pi-package", FIX_INSTALL),
    entry("install.skill", FIX_INSTALL),
    entry("repo.config", None),
    entry("daemon.health", Some("pixel daemon start {root}")),
    entry("index.freshness", FIX_PREPARE),
    entry("graph.freshness", FIX_PREPARE),
    entry("history.freshness", Some("pixel prepare --history {root}")),
];

/// One doctor check.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub id: String,
    pub status: CheckStatus,
    pub required: bool,
    pub duration_ms: u64,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    /// Shell command that repairs this check; only on a yellow or red one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// The argv after `pixel` of each command in `fix`, when `fix` is the
    /// catalogue command `--fix` may run by itself; `None` for a command only
    /// one outcome names (a path to remove), which stays the user's call.
    #[serde(skip)]
    pub repair: Option<Vec<Vec<String>>>,
}

/// The full doctor report.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub version: String,
    pub ok: bool,
    pub executable_path: String,
    pub home: String,
    pub checks: Vec<DoctorCheck>,
    pub summary: DoctorSummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorSummary {
    pub green: usize,
    pub yellow: usize,
    pub red: usize,
    /// Checks left out by `only`/`skip`.
    pub skipped: usize,
}

impl DoctorReport {
    /// Whether any check sits at or above `threshold`: the CLI exits 1 then.
    #[must_use]
    pub fn fails(&self, threshold: CheckStatus) -> bool {
        self.checks.iter().any(|c| c.status >= threshold)
    }
}

/// The terminal form: one tally line, then every yellow or red check, red
/// first, each with its `fix:` line when a command repairs it.
impl fmt::Display for DoctorReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = &self.summary;
        write!(f, "pixel doctor: ran {} check(s)", self.checks.len())?;
        if s.skipped > 0 {
            write!(f, ", skipped {}", s.skipped)?;
        }
        writeln!(
            f,
            " — {} green, {} yellow, {} red",
            s.green, s.yellow, s.red
        )?;
        let mut flagged: Vec<&DoctorCheck> = self
            .checks
            .iter()
            .filter(|c| c.status != CheckStatus::Green)
            .collect();
        // Stable: checks of one status keep their run order.
        flagged.sort_by_key(|c| std::cmp::Reverse(c.status));
        for c in flagged {
            let message = c.reason.as_deref().unwrap_or(&c.summary);
            writeln!(f, "  [{}] {}: {}", c.status, c.id, one_line(message))?;
            if let Some(fix) = &c.fix {
                writeln!(f, "    fix: {fix}")?;
            }
        }
        Ok(())
    }
}

/// The terminal form of `pixel doctor --list`: one line per check, its id
/// padded to a column, then its repair command or `-` when none exists.
#[must_use]
pub fn render_catalogue(checks: &[CheckSpec]) -> String {
    let width = checks.iter().map(|c| c.id.len()).max().unwrap_or(0);
    checks
        .iter()
        .map(|c| format!("{:<width$}  {}\n", c.id, c.fix.unwrap_or("-")))
        .collect()
}

/// Options controlling a doctor run.
#[derive(Debug, Clone, Default)]
pub struct DoctorOptions {
    /// Path to the pixel binary to check. Defaults to the current exe.
    pub executable_path: Option<PathBuf>,
    /// Home directory. Defaults to `$HOME`.
    pub home: Option<PathBuf>,
    /// Repo root to check config, daemon and freshness for. If None, only
    /// install-state checks run.
    pub repo_root: Option<PathBuf>,
    /// Shell whose `PATH` the `binary.shell-path` check asks, as a
    /// `$SHELL`-style value. Defaults to the account's login shell.
    pub shell: Option<String>,
    /// Check ids to run; empty runs every check. Ids come from [`CHECKS`].
    pub only: Vec<String>,
    /// Check ids to leave out.
    pub skip: Vec<String>,
}

/// What a check found: the sentence and, when useful, the machine-readable detail.
#[derive(Debug)]
pub struct DoctorCheckDetail {
    pub summary: String,
    pub detail: Option<serde_json::Value>,
}

/// Which repair command a yellow or red outcome carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remedy {
    /// The check's command in [`CHECKS`].
    Catalogue,
    /// No command: nothing to repair, or another check carries the fix.
    Manual,
}

/// Longest stderr excerpt a check reason quotes from a child process.
const STDERR_EXCERPT_CHARS: usize = 256;

/// Longest the login-shell probe waits for the shell to answer. A login
/// shell runs its startup files first, and a startup command that stalls
/// must not park `pixel doctor`; the daemon health probe uses the same
/// budget.
const SHELL_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a bounded probe did not produce a process status.
#[derive(Debug)]
enum ProbeFailure {
    /// The deadline passed before the child exited; the child was killed
    /// and reaped.
    Timeout,
    /// The child could not be spawned.
    Spawn(std::io::Error),
}

/// Bounded stand-in for `Command::output()`. `output()` waits for the child
/// to exit *and* both pipes to reach EOF, so a login shell stalled in a
/// startup file — or a descendant that inherited the stdout pipe — blocks
/// the caller with no deadline. This spawns with piped stdout/stderr, reads
/// each on its own detached thread, polls `try_wait` under `timeout`, and on
/// expiry kills and reaps the child. The readers are detached on purpose: a
/// lingering descendant cannot hold the caller past the deadline.
// mutants: the polling ticks and the deadline comparison are
// timing-equivalent under mutation; `bounded_output_times_out_on_a_stalled_shell`
// pins the timeout contract the mutations cannot change.
#[cfg_attr(test, mutants::skip)]
fn bounded_output(
    command: &mut Command,
    timeout: Duration,
) -> std::result::Result<std::process::Output, ProbeFailure> {
    use std::io::Read;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(ProbeFailure::Spawn)?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stdout.read_to_end(&mut bytes);
        let _ = out_tx.send(read.map(|_| bytes));
    });
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stderr.read_to_end(&mut bytes);
        let _ = err_tx.send(read.map(|_| bytes));
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProbeFailure::Timeout);
            }
        }
    };
    // The child exited; give each reader the rest of the deadline. A
    // descendant can keep a pipe open, so a reader that misses it yields no
    // bytes rather than blocking the check.
    // `Result` here is this crate's alias `Result<T, InstallError>`; the
    // reader threads send `std::io::Result<Vec<u8>>`, so `.ok()` must name
    // the io error type or the function pointer does not type-check on MSRV.
    let stdout = out_rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
        .and_then(std::result::Result::<Vec<u8>, std::io::Error>::ok)
        .unwrap_or_default();
    let stderr = err_rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
        .and_then(std::result::Result::<Vec<u8>, std::io::Error>::ok)
        .unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// The `binary.shell-path` check body, with the shell resolved from
/// `--shell` (or the login shell). Green when the shell answers the lookup
/// with a path; yellow when it runs but resolves nothing — pixel itself
/// works, the agent's environment is what is missing — and red when the
/// shell process cannot run at all. Split out so the three outcomes have
/// direct unit tests instead of depending on the CLI suite's parse of the
/// rendered report.
pub(crate) fn shell_path_check(
    shell_override: Option<&str>,
) -> std::result::Result<(CheckStatus, DoctorCheckDetail), String> {
    shell_path_check_within(shell_override, SHELL_PROBE_TIMEOUT)
}

/// The probe body, with the deadline as a parameter so a test drives the
/// timeout arm in milliseconds instead of the production five seconds.
fn shell_path_check_within(
    shell_override: Option<&str>,
    timeout: Duration,
) -> std::result::Result<(CheckStatus, DoctorCheckDetail), String> {
    let shell = install::resolve_shell(shell_override);
    // Login shell mode: interactive shells load rc files that
    // non-interactive shells do not, and the PATH is their work.
    let lookup = match install::shell_kind_from(&shell) {
        install::ShellKind::Fish => ["-l", "-c", "which pixel"],
        install::ShellKind::Posix => ["-l", "-c", "command -v pixel"],
    };
    let mut command = Command::new(&shell);
    command.args(lookup);
    let out = match bounded_output(&mut command, timeout) {
        Ok(o) if o.status.success() => o,
        // `command -v` exits 1 without resolving; a spawn failure or a
        // stalled startup is a different failure (the shell itself is broken).
        Ok(o) => {
            return Ok((
                CheckStatus::Yellow,
                DoctorCheckDetail {
                    summary: "the shell pixel is installed for does not resolve it".to_string(),
                    detail: Some(serde_json::json!({
                        "shell": shell,
                        "exit_status": o.status.to_string(),
                        "stderr": capped(
                            &one_line(&String::from_utf8_lossy(&o.stderr)),
                            STDERR_EXCERPT_CHARS,
                        ),
                    })),
                },
            ));
        }
        Err(ProbeFailure::Spawn(e)) => {
            return Err(format!(
                "failed to run shell {shell}: {e}; the login shell itself is broken"
            ));
        }
        Err(ProbeFailure::Timeout) => {
            return Err(format!(
                "shell {shell} did not answer within {timeout:?}; the login shell itself is broken"
            ));
        }
    };
    let resolved = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if resolved.is_empty() {
        return Ok((
            CheckStatus::Yellow,
            DoctorCheckDetail {
                summary: "the shell pixel is installed for does not resolve it".to_string(),
                detail: Some(serde_json::json!({ "shell": shell })),
            },
        ));
    }
    Ok((
        CheckStatus::Green,
        DoctorCheckDetail {
            summary: format!("{shell} resolves pixel as {resolved}"),
            detail: Some(serde_json::json!({
                "shell": shell,
                "resolved": resolved,
            })),
        },
    ))
}

/// Runs the checks the selection keeps and records each outcome.
pub struct Runner<'a> {
    only: &'a [String],
    skip: &'a [String],
    root: Option<&'a Path>,
    checks: Vec<DoctorCheck>,
    skipped: usize,
}

impl Runner<'_> {
    /// Run a check that is green on `Ok` and red on `Err`.
    pub fn check(
        &mut self,
        id: &'static str,
        run: impl FnOnce() -> std::result::Result<DoctorCheckDetail, String>,
    ) {
        self.record(id, || {
            run().map(|d| (CheckStatus::Green, d, Remedy::Catalogue))
        });
    }

    /// Like `check`, but the closure may also report a non-fatal `Yellow`
    /// status (e.g. a stale-but-valid index) in addition to `Green`/`Red`.
    pub fn check_status(
        &mut self,
        id: &'static str,
        run: impl FnOnce() -> std::result::Result<(CheckStatus, DoctorCheckDetail), String>,
    ) {
        self.record(id, || {
            run().map(|(status, d)| (status, d, Remedy::Catalogue))
        });
    }

    /// Run `id` unless the selection leaves it out, timing it and attaching
    /// its repair command.
    pub fn record(
        &mut self,
        id: &'static str,
        run: impl FnOnce() -> std::result::Result<(CheckStatus, DoctorCheckDetail, Remedy), String>,
    ) {
        let spec = spec(id);
        if !selected(self.only, self.skip, id) {
            self.skipped += 1;
            return;
        }
        let started = Instant::now();
        let outcome = run();
        let duration_ms = started.elapsed().as_millis() as u64;
        let check = match outcome {
            Ok((status, d, remedy)) => DoctorCheck {
                id: id.into(),
                status,
                required: true,
                duration_ms,
                summary: d.summary,
                reason: None,
                detail: d.detail,
                repair: repair_for(spec, status, &remedy, self.root),
                fix: fix_for(spec, status, remedy, self.root),
            },
            Err(reason) => DoctorCheck {
                id: id.into(),
                status: CheckStatus::Red,
                required: true,
                duration_ms,
                summary: "check failed".into(),
                reason: Some(reason),
                detail: None,
                repair: repair_for(spec, CheckStatus::Red, &Remedy::Catalogue, self.root),
                fix: fix_for(spec, CheckStatus::Red, Remedy::Catalogue, self.root),
            },
        };
        self.checks.push(check);
    }
}

/// The catalogue entry for `id`.
///
/// # Panics
///
/// When `id` is missing from [`CHECKS`]: a check the catalogue does not list
/// could be neither selected nor repaired, which is a bug in this module.
fn spec(id: &str) -> &'static CheckSpec {
    CHECKS
        .iter()
        .find(|spec| spec.id == id)
        .unwrap_or_else(|| panic!("doctor check `{id}` is missing from CHECKS"))
}

/// Whether the `--only`/`--skip` value `selector` names `id`: the id itself,
/// or its group as `<group>.*` (`install.*` for every home-install check), so
/// a caller does not have to list, and keep up with, each id of a group.
fn names_check(selector: &str, id: &str) -> bool {
    selector.strip_suffix(".*").map_or(selector == id, |group| {
        id.strip_prefix(group)
            .is_some_and(|rest| rest.starts_with('.'))
    })
}

/// Whether `id` runs: named by `only` (or `only` is empty) and not by `skip`.
fn selected(only: &[String], skip: &[String], id: &str) -> bool {
    (only.is_empty() || only.iter().any(|o| names_check(o, id)))
        && !skip.iter().any(|s| names_check(s, id))
}

/// Refuse a selection that names no check, or one both kept and left out:
/// either would silently run fewer checks than the caller meant.
fn validate_selection(only: &[String], skip: &[String]) -> Result<()> {
    if let Some(id) = only
        .iter()
        .chain(skip)
        .find(|id| !CHECKS.iter().any(|spec| names_check(id, spec.id)))
    {
        return Err(InstallError::UnknownDoctorCheck(id.clone()));
    }
    if let Some(id) = only.iter().find(|id| skip.contains(id)) {
        return Err(InstallError::ConflictingDoctorSelection(id.clone()));
    }
    Ok(())
}

/// The repair command a check reports: none on green, otherwise what the
/// outcome names, with `{root}` in a catalogue command replaced by the quoted
/// repository root (`.` when there is none).
fn fix_for(
    spec: &CheckSpec,
    status: CheckStatus,
    remedy: Remedy,
    root: Option<&Path>,
) -> Option<String> {
    if status == CheckStatus::Green {
        return None;
    }
    match remedy {
        Remedy::Catalogue => spec.fix.map(|template| {
            let root = root.map_or_else(|| ".".to_owned(), crate::routing::quoted_executable);
            catalogue_steps(template, &root)
                .iter()
                .map(|argv| format!("pixel {}", argv.join(" ")))
                .collect::<Vec<_>>()
                .join(" && ")
        }),
        Remedy::Manual => None,
    }
}

/// The argv `--fix` runs for a check: the same commands as [`fix_for`], with
/// the root unquoted, and only for a flagged check whose outcome kept the
/// catalogue command.
fn repair_for(
    spec: &CheckSpec,
    status: CheckStatus,
    remedy: &Remedy,
    root: Option<&Path>,
) -> Option<Vec<Vec<String>>> {
    if status == CheckStatus::Green || *remedy != Remedy::Catalogue {
        return None;
    }
    let root = root.map_or_else(|| ".".to_owned(), |r| r.to_string_lossy().into_owned());
    spec.fix.map(|template| catalogue_steps(template, &root))
}

/// The argv after `pixel` of each `&&`-joined command in `template`, with
/// `{root}` replaced by `root`.
fn catalogue_steps(template: &str, root: &str) -> Vec<Vec<String>> {
    template
        .split(" && ")
        .map(|command| {
            command
                .split_whitespace()
                .skip(1)
                .map(|word| {
                    if word == "{root}" {
                        root.to_owned()
                    } else {
                        word.to_owned()
                    }
                })
                .collect()
        })
        .collect()
}

/// One command `pixel doctor --fix` runs, and the flagged checks it repairs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Repair {
    /// The command as the report prints it in `fix`.
    pub command: String,
    /// The argv after `pixel` of each `&&`-joined step.
    #[serde(skip)]
    pub steps: Vec<Vec<String>>,
    /// Ids of the flagged checks this command repairs, in report order.
    pub checks: Vec<String>,
}

/// `plan` split into the repairs a `--fix` runs and those it leaves to the
/// managed `pixel`. A side build (`side_build`) never runs one that rewrites
/// the home install — a `pixel setup` step without `--repo`: that install
/// is the managed binary's, and running it through `pixel-dev` would move
/// every repository's hooks and prompts onto the side build. The checks it
/// targets keep their `fix:` line and their colour.
#[must_use]
pub fn split_home_repairs(plan: Vec<Repair>, side_build: bool) -> (Vec<Repair>, Vec<Repair>) {
    plan.into_iter().partition(|repair| {
        !side_build
            || !repair.steps.iter().any(|argv| {
                argv.first().is_some_and(|verb| verb == "setup")
                    && !argv.iter().any(|arg| arg == "--repo")
            })
    })
}

/// The repairs `--fix` runs for `report`: one per distinct catalogue command
/// among the flagged checks, in catalogue order (home install, repo install,
/// daemon, index), so a command shared by twelve checks runs once. A check
/// whose fix only its outcome can name (a file to remove) or that has none is
/// left out and keeps its `fix:` line.
#[must_use]
pub fn repair_plan(report: &DoctorReport) -> Vec<Repair> {
    let mut plan: Vec<Repair> = Vec::new();
    for check in &report.checks {
        let (Some(steps), Some(command)) = (&check.repair, &check.fix) else {
            continue;
        };
        if let Some(repair) = plan.iter_mut().find(|r| r.steps == *steps) {
            repair.checks.push(check.id.clone());
        } else {
            plan.push(Repair {
                command: command.clone(),
                steps: steps.clone(),
                checks: vec![check.id.clone()],
            });
        }
    }
    plan
}

/// Run `repair`'s steps in order with `exe` (the pixel binary), stopping at
/// the first that fails, as `&&` would. Output is captured: stdout belongs to
/// the report, and a failure quotes the step's stderr.
///
/// # Errors
///
/// The failing step, its exit status and an excerpt of its stderr, or why it
/// could not start.
pub fn run_repair(exe: &Path, repair: &Repair) -> std::result::Result<(), String> {
    for argv in &repair.steps {
        let step = format!("pixel {}", argv.join(" "));
        let out = Command::new(exe)
            .args(argv)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("`{step}` could not start: {e}"))?;
        if !out.status.success() {
            let stderr = capped(
                &one_line(&String::from_utf8_lossy(&out.stderr)),
                STDERR_EXCERPT_CHARS,
            );
            let excerpt = if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            };
            return Err(format!("`{step}` failed ({}){excerpt}", out.status));
        }
    }
    Ok(())
}

/// How a repair ended, judged against the report taken after every repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairStatus {
    /// It succeeded and every check it targeted is green now.
    Fixed,
    /// It succeeded, yet a check it targeted is still yellow or red.
    NotConverged,
    /// A step exited non-zero or could not start.
    Failed,
}

impl fmt::Display for RepairStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Fixed => "fixed",
            Self::NotConverged => "not converged",
            Self::Failed => "failed",
        })
    }
}

/// A repair `--fix` ran and what the checks said afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepairOutcome {
    pub command: String,
    pub checks: Vec<String>,
    pub status: RepairStatus,
    /// Targeted checks still yellow or red (or no longer reported) after
    /// every repair ran.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub still_flagged: Vec<String>,
    /// Why the repair failed, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Judge `repair` from its run and the report `after` it. A command that
/// exits 0 is not taken at its word: it is `fixed` only when the checks it
/// targeted re-run green.
#[must_use]
pub fn judge_repair(
    repair: Repair,
    run: std::result::Result<(), String>,
    after: &DoctorReport,
) -> RepairOutcome {
    let still_flagged: Vec<String> = repair
        .checks
        .iter()
        .filter(|id| {
            after
                .checks
                .iter()
                .find(|c| c.id == **id)
                .is_none_or(|c| c.status != CheckStatus::Green)
        })
        .cloned()
        .collect();
    let (status, error) = match run {
        Err(error) => (RepairStatus::Failed, Some(error)),
        Ok(()) if still_flagged.is_empty() => (RepairStatus::Fixed, None),
        Ok(()) => (RepairStatus::NotConverged, None),
    };
    RepairOutcome {
        command: repair.command,
        checks: repair.checks,
        status,
        still_flagged,
        error,
    }
}

/// The terminal form of a `--fix` run: a tally line, then one line per
/// repair with the checks it targeted, and what is left when it did not end
/// `fixed`.
#[must_use]
pub fn render_repairs(outcomes: &[RepairOutcome]) -> String {
    if outcomes.is_empty() {
        return "pixel doctor --fix: nothing to repair automatically\n".to_owned();
    }
    let count = |status| outcomes.iter().filter(|o| o.status == status).count();
    let mut text = format!(
        "pixel doctor --fix: ran {} repair(s) — {} fixed, {} not converged, {} failed\n",
        outcomes.len(),
        count(RepairStatus::Fixed),
        count(RepairStatus::NotConverged),
        count(RepairStatus::Failed),
    );
    for outcome in outcomes {
        text.push_str(&format!(
            "  [{}] {} ({})\n",
            outcome.status,
            outcome.command,
            outcome.checks.join(", ")
        ));
        if let Some(error) = &outcome.error {
            text.push_str(&format!("    error: {error}\n"));
        }
        if !outcome.still_flagged.is_empty() {
            text.push_str(&format!(
                "    still flagged: {}\n",
                outcome.still_flagged.join(", ")
            ));
        }
    }
    text
}

/// `text` on one line: whitespace runs and control characters (a child's
/// newlines, a terminal escape) collapse to single spaces.
fn one_line(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text` cut to `max_chars` characters, an ellipsis marking the cut.
fn capped(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// What the file-based checks need to know about the machine.
struct Machine {
    home: PathBuf,
    /// Whether the caller pinned the home (a test or `--home`), so `$CODEX_HOME`
    /// and the like are not consulted.
    explicit: bool,
    exe: PathBuf,
}

impl Machine {
    fn codex_home(&self) -> PathBuf {
        crate::codex_config::codex_home(&self.home, self.explicit)
    }
}

/// A check outcome that stays green with the catalogue's repair.
fn green(summary: String, detail: Option<Value>) -> (CheckStatus, DoctorCheckDetail, Remedy) {
    (
        CheckStatus::Green,
        DoctorCheckDetail { summary, detail },
        Remedy::Catalogue,
    )
}

/// A yellow outcome carrying the catalogue's repair.
fn yellow(summary: String, detail: Option<Value>) -> (CheckStatus, DoctorCheckDetail, Remedy) {
    (
        CheckStatus::Yellow,
        DoctorCheckDetail { summary, detail },
        Remedy::Catalogue,
    )
}

type Outcome = std::result::Result<(CheckStatus, DoctorCheckDetail, Remedy), String>;

/// The hook entries of a host that shares Claude's settings schema (Claude
/// Code, Codex, Devin): registered once each, running this binary, with the
/// brief in as many parts as `brief_parts` says; foreign hooks remain intact.
fn schema_hooks(machine: &Machine, host: Host, path: &Path, configured: bool) -> Outcome {
    let name = host.as_str();
    if !configured {
        return Ok(green(
            format!("{name} is not configured here; no hooks expected"),
            None,
        ));
    }
    if !path.is_file() {
        return Err(format!("{} not found — run `pixel setup`", path.display()));
    }
    let value = install::read_settings(path).map_err(|error| error.to_string())?;
    let exe = &machine.exe;
    let parts = routing::prompt_parts(&value, host, exe);
    if !routing::hooks_registered(&value, host, exe) {
        // A brief split over several hooks is whole only with every part:
        // name the one that went missing.
        let missing = match &parts {
            Err(reason) if reason != routing::NO_PROMPT_HOOK => reason.clone(),
            _ => "lifecycle hooks".to_owned(),
        };
        return Err(format!(
            "missing pixel hooks in {}: {missing} — run `pixel setup`",
            path.display()
        ));
    }
    let stacked = routing::stacked_pixel_hooks(&value, exe);
    if !stacked.is_empty() {
        return Err(format!(
            "pixel hooks registered more than once in {}: {} — run `pixel setup`",
            path.display(),
            stacked.join(", ")
        ));
    }
    let detail = Some(serde_json::json!({ "path": path.display().to_string() }));
    let others = routing::pixel_hooks_running_other_binaries(&value, exe);
    if !others.is_empty() && !config::is_side_build(exe) {
        let others: Vec<String> = others.iter().map(|p| p.display().to_string()).collect();
        return Ok(yellow(
            format!(
                "{name} hooks in {} run {}, not this pixel ({}); every session uses that binary — run `pixel setup` to point them here",
                path.display(),
                others.join(", "),
                exe.display()
            ),
            Some(serde_json::json!({ "path": path.display().to_string(), "running": others })),
        ));
    }
    if matches!(host, Host::Claude | Host::Codex)
        && let Ok(installed) = parts
        && let Some(drift) =
            brief_parts_drift(path, installed, config::brief_parts(&machine.home, None))
    {
        return Ok(yellow(drift.summary, drift.detail));
    }
    Ok(green(
        format!("{name} hooks configured in {}", path.display()),
        detail,
    ))
}

/// Yellow outcome when the brief is installed in `installed` hooks but the
/// configuration asks for `configured`: the settings predate the change of
/// `brief_parts`, and `pixel setup` rewrites them.
fn brief_parts_drift(path: &Path, installed: u8, configured: u8) -> Option<DoctorCheckDetail> {
    (installed != configured).then(|| DoctorCheckDetail {
        summary: format!(
            "the prompt brief is registered in {installed} hook(s) in {}, but `brief_parts` is {configured} — run `pixel setup`",
            path.display()
        ),
        detail: Some(serde_json::json!({
            "path": path.display().to_string(),
            "installed_parts": installed,
            "configured_parts": configured,
        })),
    })
}

/// Gemini CLI: an entry per event, running this binary.
fn gemini_hooks(machine: &Machine) -> Outcome {
    let path = machine.home.join(config::GEMINI_SETTINGS_FILE);
    if !path.is_file() {
        return Ok(green(
            "Gemini CLI is not configured here; no hooks expected".into(),
            None,
        ));
    }
    let value = install::read_settings(&path).map_err(|error| error.to_string())?;
    let missing: Vec<&str> = routing::REGISTERED
        .iter()
        .filter_map(|&event| {
            let host_event = Host::Gemini.registration(event)?;
            let expected = format!("{} --host gemini", event.as_str());
            let present = value["hooks"][host_event]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
                .filter_map(|hook| hook["command"].as_str())
                .any(|command| {
                    routing::pixel_hook_verb(command, &machine.exe) == Some(expected.as_str())
                });
            (!present).then_some(host_event)
        })
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing pixel hooks in {}: {} — run `pixel setup`",
            path.display(),
            missing.join(", ")
        ));
    }
    Ok(green(
        format!("Gemini CLI hooks configured in {}", path.display()),
        Some(serde_json::json!({ "path": path.display().to_string() })),
    ))
}

/// Antigravity: the plugin directories and the global brief entry.
fn antigravity(machine: &Machine) -> Outcome {
    let (mut summary, detail) =
        crate::antigravity::check_antigravity_install(&machine.home, &machine.exe)?;
    let hooks_path = crate::antigravity::hooks_path(&machine.home);
    if hooks_path.is_file() {
        let hooks = install::read_settings(&hooks_path).map_err(|error| error.to_string())?;
        let missing = crate::antigravity::brief_drift(&hooks, &machine.exe);
        if !missing.is_empty() {
            return Err(format!(
                "missing or malformed PIXEL Antigravity hooks in {}: {} — run `pixel setup`",
                hooks_path.display(),
                missing.join(", ")
            ));
        }
        summary =
            "Antigravity PIXEL prompt brief and plugin configured; native tools preserved".into();
    }
    Ok(green(summary, Some(detail)))
}

/// OpenCode: the plugin as setup writes it for this binary.
fn opencode(machine: &Machine) -> Outcome {
    let dir = crate::opencode_config::opencode_config_dir(&machine.home, machine.explicit);
    if !dir.is_dir() {
        return Ok(green(
            "OpenCode is not configured here; no plugin expected".into(),
            None,
        ));
    }
    let plugin = dir.join(crate::opencode_config::brief_plugin_path());
    let detail = Some(serde_json::json!({ "plugin": plugin.display().to_string() }));
    if !plugin.exists() {
        return Err(format!(
            "{} not found — run `pixel setup`",
            plugin.display()
        ));
    }
    if !crate::opencode_config::is_pixel_plugin(&plugin) {
        return Ok((
            CheckStatus::Yellow,
            DoctorCheckDetail {
                summary: format!("{} is not pixel's; left untouched", plugin.display()),
                detail,
            },
            Remedy::Manual,
        ));
    }
    if fs::read_to_string(&plugin).ok().as_deref()
        != Some(crate::opencode_config::brief_source(&machine.exe).as_str())
    {
        return Err(format!(
            "{} is stale or names a different binary — run `pixel setup`",
            plugin.display()
        ));
    }
    Ok(green(
        format!("OpenCode brief plugin current in {}", plugin.display()),
        detail,
    ))
}

/// pi: the package, its settings entry and its skill.
fn pi_package(machine: &Machine) -> Outcome {
    let paths = crate::pi_global::PiPaths::resolve(&machine.home, machine.explicit);
    let detail = Some(serde_json::json!({
        "settings": paths.settings.display().to_string(),
        "package": paths.package_dir.display().to_string(),
    }));
    let installed = !machine.explicit && crate::pi_global::pi_on_path();
    match crate::pi_global::check(&paths, &machine.exe, installed) {
        crate::pi_global::PiImpactState::NotInstalled => Ok(green(
            "pi is not installed; no package expected".into(),
            None,
        )),
        crate::pi_global::PiImpactState::Current => Ok(green(
            format!("pixel package declared in {}", paths.settings.display()),
            detail,
        )),
        crate::pi_global::PiImpactState::Manual(reason) => Ok((
            CheckStatus::Yellow,
            DoctorCheckDetail {
                summary: reason,
                detail,
            },
            Remedy::Manual,
        )),
        crate::pi_global::PiImpactState::NeedsInstall(reason) => {
            Err(format!("{reason}; run `pixel setup`"))
        }
    }
}

/// The `pixel` skill in every root setup writes: present, and what setup wrote.
fn skill(machine: &Machine) -> Outcome {
    let mut missing = Vec::new();
    let mut edited = Vec::new();
    let roots = crate::skill::skill_dirs(&machine.home, None);
    for file in crate::skill::skill_files(&roots) {
        match fs::read_to_string(&file) {
            Ok(text) if text == crate::skill::PIXEL_SKILL => {}
            Ok(_) => edited.push(file.display().to_string()),
            Err(_) => missing.push(file.display().to_string()),
        }
    }
    let detail = Some(serde_json::json!({ "roots": roots.len(), "differing": edited }));
    if missing.len() == roots.len() {
        return Ok(yellow(
            "the pixel skill is not installed; `pixel setup` has not run on this machine".into(),
            detail,
        ));
    }
    if !missing.is_empty() {
        return Err(format!(
            "the pixel skill is missing at {} — run `pixel setup`",
            missing.join(", ")
        ));
    }
    if !edited.is_empty() {
        return Ok(yellow(
            format!(
                "the pixel skill differs from this release's at {} (edited, or from another release) — run `pixel setup`, which keeps a backup of yours",
                edited.join(", ")
            ),
            detail,
        ));
    }
    Ok(green(
        format!(
            "the pixel skill is current in {} harness dir(s)",
            roots.len()
        ),
        detail,
    ))
}

/// The repository's `.pixel/config.yaml`, when it has one.
fn repo_config(root: &Path) -> std::result::Result<DoctorCheckDetail, String> {
    config::load_config(root).map(|_| DoctorCheckDetail {
        summary: format!("{} reads", root.join(".pixel/config.yaml").display()),
        detail: None,
    })
}

/// Runs `pixel doctor`: the file-based checks of the machine, then `live` for
/// what only the binary with the daemon and the stores can answer (the
/// daemon, the freshness of the indexes), on the same selection and report.
///
/// # Errors
///
/// Fails before any check runs when `only` or `skip` names an id missing from
/// [`CHECKS`] or both name the same id, when no home directory resolves, or
/// when the current executable cannot be located.
pub fn doctor(options: &DoctorOptions, live: impl FnOnce(&mut Runner<'_>)) -> Result<DoctorReport> {
    validate_selection(&options.only, &options.skip)?;
    let home = options
        .home
        .clone()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .ok_or(InstallError::NoHome)?;
    let executable_path = match &options.executable_path {
        Some(p) => p.clone(),
        None => std::env::current_exe().map_err(InstallError::CurrentExe)?,
    };
    let exe = install::stable_exe_path(executable_path);
    let machine = Machine {
        home: home.clone(),
        explicit: options.home.is_some(),
        exe: exe.clone(),
    };
    let mut runner = Runner {
        only: &options.only,
        skip: &options.skip,
        root: options.repo_root.as_deref(),
        checks: Vec::new(),
        skipped: 0,
    };

    runner.check("binary.path", || {
        if !exe.is_file() {
            return Err(format!("binary not found at {}", exe.display()));
        }
        Ok(DoctorCheckDetail {
            summary: format!("binary present at {}", exe.display()),
            detail: Some(serde_json::json!({ "path": exe.display().to_string() })),
        })
    });
    runner.check("binary.executable", || {
        match Command::new(&exe).arg("--version").output() {
            Ok(o) if o.status.success() => Ok(DoctorCheckDetail {
                summary: format!("binary runs: {}", String::from_utf8_lossy(&o.stdout).trim()),
                detail: None,
            }),
            Ok(o) => Err(format!(
                "binary exited {}: {}",
                o.status,
                capped(
                    &one_line(&String::from_utf8_lossy(&o.stderr)),
                    STDERR_EXCERPT_CHARS
                )
            )),
            Err(e) => Err(format!("failed to run binary: {e}")),
        }
    });
    // The agent shells inherit the login shell's environment, and a coding
    // agent that cannot resolve `pixel` silently works without it. Ask the
    // resolved shell itself, the authority on the profile it loads.
    let shell = options.shell.clone();
    runner.check_status("binary.shell-path", || shell_path_check(shell.as_deref()));

    runner.record("install.claude-hooks", || {
        let path = home.join(".claude/settings.json");
        let configured = home.join(".claude").is_dir();
        schema_hooks(&machine, Host::Claude, &path, configured)
    });
    let codex_home = machine.codex_home();
    runner.record("install.codex-hooks", || {
        let path = codex_home.join(crate::codex_config::HOOKS_FILE);
        schema_hooks(&machine, Host::Codex, &path, codex_home.is_dir())
    });
    runner.check_status("install.codex-hook-review", || {
        let hooks_path = codex_home.join(crate::codex_config::HOOKS_FILE);
        if !hooks_path.is_file() {
            return Ok((
                CheckStatus::Green,
                DoctorCheckDetail {
                    summary: "Codex has no pixel hooks to review".into(),
                    detail: None,
                },
            ));
        }
        let review = crate::codex_review::pixel_hook_review(&codex_home, &hooks_path, &exe)?;
        let (status, summary) = crate::codex_review::hook_review_outcome(&review, &hooks_path);
        Ok((
            status,
            DoctorCheckDetail {
                summary,
                detail: Some(serde_json::json!({
                    "hooks": hooks_path.display().to_string(),
                    "pixel": review.pixel,
                    "unreviewed": review.unreviewed,
                })),
            },
        ))
    });
    runner.record("install.devin", || {
        let path = home
            .join(config::DEVIN_CONFIG_DIR)
            .join(config::DEVIN_CONFIG_FILE);
        let configured = home.join(config::DEVIN_CONFIG_DIR).is_dir();
        schema_hooks(&machine, Host::Devin, &path, configured)
    });
    runner.record("install.gemini", || gemini_hooks(&machine));
    runner.record("install.antigravity", || antigravity(&machine));
    runner.record("install.opencode", || opencode(&machine));
    runner.record("install.pi-package", || pi_package(&machine));
    runner.record("install.skill", || skill(&machine));

    if let Some(root) = &options.repo_root {
        runner.check("repo.config", || repo_config(root));
        live(&mut runner);
    }
    let Runner {
        checks, skipped, ..
    } = runner;
    Ok(doctor_report(&exe, &home, checks, skipped))
}

fn doctor_report(
    exe: &Path,
    home: &Path,
    checks: Vec<DoctorCheck>,
    skipped: usize,
) -> DoctorReport {
    let count = |status| checks.iter().filter(|c| c.status == status).count();
    let summary = DoctorSummary {
        green: count(CheckStatus::Green),
        yellow: count(CheckStatus::Yellow),
        red: count(CheckStatus::Red),
        skipped,
    };
    DoctorReport {
        version: "v1".into(),
        ok: summary.red == 0,
        executable_path: exe.display().to_string(),
        home: home.display().to_string(),
        checks,
        summary,
    }
}

#[cfg(test)]
mod tests;
