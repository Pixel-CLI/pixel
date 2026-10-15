// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel doctor` — checks install state, binary path, daemon health, and
//! index/graph/facts freshness, reporting green/yellow/red per check.
//!
//! Every check is listed in [`CHECKS`] with the command that repairs it, so a
//! caller can select checks by id (`--only`, `--skip`) and act on a finding
//! without parsing its prose. [`repair_plan`] folds the flagged checks into
//! one run of each distinct command, which `pixel doctor --fix` executes.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::InstallError;
use crate::config;
use crate::install;

pub type Result<T> = std::result::Result<T, InstallError>;

/// Per-check status for the doctor report, ordered from healthy to broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Green,
    Yellow,
    Red,
}

impl fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Green => "green",
            Self::Yellow => "yellow",
            Self::Red => "red",
        })
    }
}

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
const FIX_PREPARE: Option<&str> = Some("pixel prepare-repo {root}");

/// Every check, in the order a run reports them. Ids are stable: scripts
/// select them with `--only`/`--skip`, and a check missing from this list
/// panics when it runs.
pub const CHECKS: &[CheckSpec] = &[
    entry("binary.path", None),
    entry("binary.executable", None),
    entry("binary.shell-path", None),
    entry("install.pi-impact", FIX_INSTALL),
    entry("install.codex-metrics-hook", FIX_INSTALL),
    // Only the user can review a hook (`/hooks` in Codex); the outcome says so.
    entry("install.codex-hook-review", None),
    entry("install.antigravity", FIX_INSTALL),
    entry("install.claude-hooks", FIX_INSTALL),
    // The setup step is interactive and needs the user at a terminal, so
    // the check names it in the message but leaves `--fix` out of it.
    entry("web-search.provider", None),
    entry("repo.task-hook-observations", None),
    entry("daemon.health", Some("pixel daemon start {root}")),
    entry(
        "daemon.epistemics",
        Some("pixel daemon stop {root} && pixel daemon start {root}"),
    ),
    entry("index.freshness", FIX_PREPARE),
    entry("graph.freshness", FIX_PREPARE),
    entry(
        "facts.freshness",
        Some("pixel build-index --history {root}"),
    ),
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
    /// Repo root to check index/graph/facts freshness for. If None, only
    /// install-state checks run.
    pub repo_root: Option<PathBuf>,
    /// Shell whose `PATH` the `binary.shell-path` check asks, as a
    /// `$SHELL`-style value. Defaults to the account's login shell.
    pub shell: Option<String>,
    /// Check ids to run; empty runs every check. Ids come from [`CHECKS`].
    pub only: Vec<String>,
    /// Check ids to leave out.
    pub skip: Vec<String>,
    /// The moved commands a plugin runs here. Defaults to a scan of the
    /// plugin directories under `home` (and `repo_root`) and of `$PATH`.
    pub plugins: Option<pixel_plugin::Available>,
}

/// Run `pixel doctor`.
///
/// # Errors
///
/// Fails before any check runs when `only` or `skip` names an id missing
/// from [`CHECKS`] or both name the same id, when no home directory
/// resolves, or when the current executable cannot be located.
pub fn doctor(options: &DoctorOptions) -> Result<DoctorReport> {
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
    let plugins = options.plugins.clone().unwrap_or_else(|| {
        pixel_plugin::Available::scan(&pixel_plugin::Host {
            home: home.clone(),
            repo_root: options.repo_root.clone(),
            path: std::env::var_os("PATH").unwrap_or_default(),
            github_base: None,
        })
    });

    let mut runner = Runner {
        only: &options.only,
        skip: &options.skip,
        root: options.repo_root.as_deref(),
        checks: Vec::new(),
        skipped: 0,
    };

    runner.check(
        "binary.path",
        || -> std::result::Result<DoctorCheckDetail, String> {
            if !exe.is_file() {
                return Err(format!("binary not found at {}", exe.display()));
            }
            Ok(DoctorCheckDetail {
                summary: format!("binary present at {}", exe.display()),
                detail: Some(serde_json::json!({ "path": exe.display().to_string() })),
            })
        },
    );

    runner.check(
        "binary.executable",
        || -> std::result::Result<DoctorCheckDetail, String> {
            let out = Command::new(&exe).arg("--version").output();
            match out {
                Ok(o) if o.status.success() => Ok(DoctorCheckDetail {
                    summary: format!(
                        "binary runs ({} bytes stdout)",
                        String::from_utf8_lossy(&o.stdout).trim().len()
                    ),
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
        },
    );

    // The agent shells inherit the login shell's environment, and a coding
    // agent that cannot resolve `pixel` silently works without it (measured
    // on a machine where only .bashrc put the install on PATH and the
    // harness ran zsh). Ask the resolved shell itself, the authority on the
    // profile it loads, so the probe uses the same lookup the profile adds.
    let shell_override = options.shell.clone();
    runner.check_status("binary.shell-path", || {
        shell_path_check(shell_override.as_deref())
    });

    let pi_paths = crate::pi_global::PiPaths::resolve(&home, options.home.is_some());
    runner.record("install.pi-impact", || {
        let detail = Some(serde_json::json!({
            "settings": pi_paths.settings.display().to_string(),
            "package": pi_paths.package_dir.display().to_string(),
        }));
        let pi_installed = options.home.is_none() && crate::pi_global::pi_on_path();
        let (status, summary, remedy) = match crate::pi_global::check(&pi_paths, &exe, pi_installed)
        {
            crate::pi_global::PiImpactState::NotInstalled => (
                CheckStatus::Green,
                "Pi is not installed; no explicit impact command expected".to_owned(),
                Remedy::Catalogue,
            ),
            crate::pi_global::PiImpactState::Current => (
                CheckStatus::Green,
                format!(
                    "explicit impact package declared in {}",
                    pi_paths.settings.display()
                ),
                Remedy::Catalogue,
            ),
            crate::pi_global::PiImpactState::Manual(reason) => {
                (CheckStatus::Yellow, reason, Remedy::Manual)
            }
            crate::pi_global::PiImpactState::NeedsInstall(reason) => {
                return Err(format!("{reason}; run `pixel setup`"));
            }
        };
        Ok((status, DoctorCheckDetail { summary, detail }, remedy))
    });

    let codex_home = crate::codex_config::codex_home(&home, options.home.is_some());
    runner.check(
        "install.codex-metrics-hook",
        || -> std::result::Result<DoctorCheckDetail, String> {
            let (summary, detail) = crate::codex_config::check_task_hooks(&codex_home, &exe)?;
            Ok(DoctorCheckDetail {
                summary,
                detail: Some(detail),
            })
        },
    );
    runner.check_status("install.codex-hook-review", || {
        let hooks_path = codex_home.join(crate::codex_config::HOOKS_FILE);
        codex_hook_review(&codex_home, &hooks_path, &exe)
    });

    let exe_for_antigravity = exe.clone();
    let home_for_antigravity = home.clone();
    runner.check(
        "install.antigravity",
        move || -> std::result::Result<DoctorCheckDetail, String> {
            let (mut summary, detail) = crate::antigravity::check_antigravity_install(
                &home_for_antigravity,
                &exe_for_antigravity,
            )?;
            let hooks_path = crate::antigravity::hooks_path(&home_for_antigravity);
            if hooks_path.is_file() {
                let hooks = install::read_settings(&hooks_path).map_err(|e| e.to_string())?;
                let missing = antigravity_hook_drift(&hooks, &exe_for_antigravity);
                if !missing.is_empty() {
                    return Err(format!(
                        "missing or malformed Pixel Antigravity hooks in {}: {} — run `pixel setup`",
                        hooks_path.display(),
                        missing.join(", ")
                    ));
                }
                summary = "Antigravity Pixel prompt brief and deferred metrics relay configured; native tools preserved".into();
            }
            Ok(DoctorCheckDetail {
                summary,
                detail: Some(detail),
            })
        },
    );

    // Verify the task-event suite and its separate metrics relay using the
    // commands this binary's install would write, not a `pixel` substring.
    runner.record(
        "install.claude-hooks",
        || -> std::result::Result<(CheckStatus, DoctorCheckDetail, Remedy), String> {
            let path = home.join(".claude/settings.json");
            if !path.is_file() {
                return Err(format!("{} not found — run `pixel setup`", path.display()));
            }
            let value = install::read_settings(&path).map_err(|e| e.to_string())?;
            let mut missing = Vec::new();
            let parts =
                crate::routing::prompt_parts(&value, crate::routing::Provider::Claude, &exe);
            if !crate::routing::task_hooks_registered(
                &value,
                crate::routing::Provider::Claude,
                &exe,
            ) {
                // A brief split over several hooks is whole only with every
                // part: name the one that went missing.
                missing.push(match &parts {
                    Err(reason) if reason != crate::routing::NO_PROMPT_HOOK => reason.clone(),
                    _ => "task lifecycle gates".to_owned(),
                });
            }
            if !claude_metrics_relay_registered(&value, &exe) {
                missing.push("PostToolUse metrics relay".to_owned());
            }
            if !missing.is_empty() {
                return Err(format!(
                    "missing pixel hooks in {}: {} — run `pixel setup`",
                    path.display(),
                    missing.join(", ")
                ));
            }
            let stacked = crate::routing::stacked_pixel_hooks(&value, &exe);
            if !stacked.is_empty() {
                return Err(format!(
                    "pixel hooks registered more than once in {}: {} — run `pixel setup`",
                    path.display(),
                    stacked.join(", ")
                ));
            }
            let (status, detail) = claude_hooks_owner_check(
                &path,
                &exe,
                &crate::routing::pixel_hooks_running_other_binaries(&value, &exe),
            );
            if status == CheckStatus::Green
                && let Ok(installed) = parts
                && let Some(drift) =
                    brief_parts_drift(&path, installed, crate::config::brief_parts(&home, None))
            {
                return Ok((CheckStatus::Yellow, drift, Remedy::Catalogue));
            }
            Ok((status, detail, Remedy::Catalogue))
        },
    );

    // Which web search provider the installed `pixel` resolves to, read
    // straight from the environment and the global config file — never
    // by spawning the binary, which would re-enter it from inside
    // doctor. Runs for every invocation, not only inside a repository,
    // so a bare `pixel doctor` still reports no provider when one is not
    // configured.
    runner.check_status("web-search.provider", || {
        if !plugins.has("web-search") {
            return Ok(web_search_plugin_absent(&plugins));
        }
        web_search_provider_check(&home)
    });

    if let Some(root) = &options.repo_root {
        runner.check("repo.task-hook-observations", || {
            task_hook_observations(root)
        });
        runner.check(
            "daemon.health",
            || -> std::result::Result<DoctorCheckDetail, String> {
                let sock = pixel_daemon::daemon::socket_path(root);
                if !sock.exists() {
                    return Err(format!("no daemon socket at {}", sock.display()));
                }
                Ok(DoctorCheckDetail {
                    summary: format!("daemon socket present at {}", sock.display()),
                    detail: Some(serde_json::json!({ "socket": sock.display().to_string() })),
                })
            },
        );

        // Epistemics-presence probe: when a daemon answers, one retrieval op
        // should carry an `epistemics` object in its response. Warning-only
        // (Yellow), never red — the envelope is landing concurrently and a
        // daemon built from an older binary is a staleness note, not a
        // broken install.
        // Only an answering daemon without the envelope needs the restart;
        // no daemon at all is `daemon.health`'s finding and carries its fix.
        runner.record("daemon.epistemics", || {
            let sock = pixel_daemon::daemon::socket_path(root);
            if !sock.exists() {
                return Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: "no daemon running — epistemics probe skipped".into(),
                    detail: None,
                }, Remedy::Manual));
            }
            match probe_daemon_epistemics(&sock) {
                Ok(true) => Ok((CheckStatus::Green, DoctorCheckDetail {
                    summary: "daemon retrieval response carries an epistemics object".into(),
                    detail: None,
                }, Remedy::Catalogue)),
                Ok(false) => Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: "daemon retrieval response has NO epistemics object — daemon may predate the epistemics envelope; restart it".into(),
                    detail: None,
                }, Remedy::Catalogue)),
                Err(e) => Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: format!("epistemics probe inconclusive: {e}"),
                    detail: None,
                }, Remedy::Manual)),
            }
        });

        runner.check(
            "index.freshness",
            || -> std::result::Result<DoctorCheckDetail, String> {
                let shard = root
                    .join(pixel_index::index::SHARD_DIR)
                    .join(pixel_index::index::SHARD_FILE);
                if !shard.is_file() {
                    return Err("index not built".into());
                }
                let mtime = fs::metadata(&shard)
                    .map_err(|e| e.to_string())?
                    .modified()
                    .map_err(|e| e.to_string())?;
                let age = age_secs(mtime);
                Ok(DoctorCheckDetail {
                    summary: format!("index present ({age}s old)"),
                    detail: Some(serde_json::json!({ "age_secs": age })),
                })
            },
        );

        runner.check(
            "graph.freshness",
            || -> std::result::Result<DoctorCheckDetail, String> {
                let db = root
                    .join(pixel_index::index::SHARD_DIR)
                    .join(pixel_daemon::api::GRAPH_DB_FILE);
                if !db.is_file() {
                    return Err("graph not built".into());
                }
                let mtime = fs::metadata(&db)
                    .map_err(|e| e.to_string())?
                    .modified()
                    .map_err(|e| e.to_string())?;
                let age = age_secs(mtime);
                Ok(DoctorCheckDetail {
                    summary: format!("graph present ({age}s old)"),
                    detail: Some(serde_json::json!({ "age_secs": age })),
                })
            },
        );

        runner.check_status("facts.freshness", || -> std::result::Result<(CheckStatus, DoctorCheckDetail), String> {
            // History is demand-driven: a db that does not exist, or that
            // holds no commit yet, is a repository that never asked for
            // history, which is healthy. Opening it for writing here would
            // create it, and the old red verdict on the empty db made
            // `--fix` run a full history build nobody had asked for.
            let not_built = || {
                Ok((CheckStatus::Green, DoctorCheckDetail {
                    summary: "history index not built (built on the first history command)"
                        .to_string(),
                    detail: Some(serde_json::json!({ "present": false })),
                }))
            };
            let Some(store) = pixel_facts::FactsStore::open_existing(root).map_err(|e| e.to_string())?
            else {
                return not_built();
            };
            let state = store.index_state();
            // Red: schema version mismatch — the db was written by a different
            // build and must be rebuilt before it can be trusted.
            if state.schema_version != pixel_facts::store::FACTS_SCHEMA_VERSION {
                return Err(format!(
                    "facts schema version mismatch: on-disk {} != expected {} (rebuild required)",
                    state.schema_version,
                    pixel_facts::store::FACTS_SCHEMA_VERSION
                ));
            }
            // Counter-based poisoned detection: mtime and diff_state alone
            // lie (the historical poisoned DB had every commit marked
            // INDEXED with empty hunk text), so measure the actual text.
            let count = |sql: &str| -> i64 {
                store.conn().query_row(sql, [], |r| r.get(0)).unwrap_or(0)
            };
            let hunks_with_text = count(
                "SELECT count(*) FROM hunks WHERE length(added) > 0 OR length(removed) > 0",
            );
            // Ingest writes no row for a hunk without text (a binary file,
            // a rename), so an empty row only ever means lost text.
            let empty_hunks =
                count("SELECT count(*) FROM hunks WHERE added = '' AND removed = ''");
            let verdict = facts_verdict(state.commits_indexed, empty_hunks, state.fresh);
            let repo_commits = pixel_git::GitRunner::new(root)
                .rev_list_count_all()
                .unwrap_or(0);
            let used_bytes = store.used_bytes().unwrap_or(0);
            let budget_bytes = pixel_facts::store::HistoryLimits::from_env().budget_bytes;
            let detail = Some(serde_json::json!({
                "present": true,
                "phase": state.phase,
                "commits_indexed": state.commits_indexed,
                "total_commits": repo_commits.max(state.total_commits),
                "diff_indexed_pct": state.diff_indexed_pct,
                "hunks_with_text": hunks_with_text,
                "used_bytes": used_bytes,
                "budget_bytes": budget_bytes,
                "diffs_evicted": state.diffs_evicted,
                "diff_coverage_since": state.diff_coverage_since,
                "fresh": state.fresh,
                "schema_version": state.schema_version,
            }));
            match verdict {
                FactsVerdict::NotBuilt => not_built(),
                FactsVerdict::Poisoned(reason) => Err(reason),
                // Yellow: stale — ingest has not caught up to the current refs.
                FactsVerdict::Stale => Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: format!(
                        "facts db present but stale (phase {}, {} commits, {:.0}% diff coverage)",
                        state.phase,
                        state.commits_indexed,
                        state.diff_indexed_pct * 100.0
                    ),
                    detail,
                })),
                FactsVerdict::Fresh => Ok((CheckStatus::Green, DoctorCheckDetail {
                    summary: format!(
                        "facts db fresh ({} commits, {:.0}% diff coverage, {} hunks with text, {} of {} budget)",
                        state.commits_indexed,
                        state.diff_indexed_pct * 100.0,
                        hunks_with_text,
                        size_mib(used_bytes),
                        size_mib(budget_bytes)
                    ),
                    detail,
                })),
            }
        });
    }

    let Runner {
        checks, skipped, ..
    } = runner;
    Ok(doctor_report(&exe, &home, checks, skipped))
}

/// The report over finished `checks`: one count per status, and `ok` while
/// no check is red.
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

/// Registration and stored trust are distinct from actual hook observations.
fn task_hook_observations(root: &Path) -> std::result::Result<DoctorCheckDetail, String> {
    let path = root.join(".pixel/task-hook-observations.json");
    let value: serde_json::Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid task hook observations: {error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(format!("cannot read task hook observations: {error}")),
    };
    let hosts = ["claude", "codex", "pi"].map(|provider| {
        let observation = &value[provider];
        let observed = observation["schema_version"] == 1
            && observation["provider"] == provider
            && observation["session_id"].as_str().is_some_and(|id| !id.is_empty())
            && observation["event"].as_str().is_some_and(|event| !event.is_empty())
            && observation["observed_unix"].as_u64().is_some();
        serde_json::json!({"provider":provider,"observed":observed,"evidence":if observed { observation.clone() } else { serde_json::Value::Null }})
    });
    let observed = hosts.iter().filter(|host| host["observed"] == true).count();
    Ok(DoctorCheckDetail {
        summary: format!(
            "{observed}/3 task adapters observed in this repository; unobserved hosts and unsupported tool paths remain unverified"
        ),
        detail: Some(serde_json::json!({"path":path,"hosts":hosts,"complete_tool_coverage":false})),
    })
}

/// `install.codex-hook-review` / `repo.codex-hook-review`: Pixel's hooks in
/// one Codex `hooks.json` that Codex skips until the user reviews them.
fn codex_hook_review(
    codex_home: &Path,
    hooks_path: &Path,
    exe: &Path,
) -> std::result::Result<(CheckStatus, DoctorCheckDetail), String> {
    let review = crate::codex_config::pixel_hook_review(codex_home, hooks_path, exe)?;
    let (status, summary) = crate::codex_config::hook_review_outcome(&review, hooks_path);
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
}

/// The final verdict of `install.claude-hooks` once every hook is present
/// and registered once: yellow when they run `others` rather than `exe`, the
/// managed binary. A side build ([`config::is_side_build`]) never judges it:
/// the home install is the managed binary's, and its hooks running that one
/// are the expected state, not drift.
fn claude_hooks_owner_check(
    path: &Path,
    exe: &Path,
    others: &[PathBuf],
) -> (CheckStatus, DoctorCheckDetail) {
    if others.is_empty() || config::is_side_build(exe) {
        return (
            CheckStatus::Green,
            DoctorCheckDetail {
                summary: format!("claude task-event hooks configured in {}", path.display()),
                detail: Some(serde_json::json!({ "path": path.display().to_string() })),
            },
        );
    }
    let others: Vec<String> = others.iter().map(|p| p.display().to_string()).collect();
    (
        CheckStatus::Yellow,
        DoctorCheckDetail {
            summary: format!(
                "claude task-event hooks in {} run {}, not this pixel ({}); every session uses that binary — run `pixel setup` to point them here",
                path.display(),
                others.join(", "),
                exe.display()
            ),
            detail: Some(serde_json::json!({
                "path": path.display().to_string(),
                "running": others,
            })),
        },
    )
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

/// Whether Claude has exactly the `PostToolUse` metrics relay setup writes.
fn claude_metrics_relay_registered(value: &serde_json::Value, exe: &Path) -> bool {
    value
        .get("hooks")
        .and_then(serde_json::Value::as_object)
        .and_then(|hooks| hooks.get("PostToolUse"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|groups| {
            groups
                .iter()
                .filter(|group| is_claude_metrics_relay_group(group, exe))
                .count()
                == 1
        })
}

fn is_claude_metrics_relay_group(group: &serde_json::Value, exe: &Path) -> bool {
    let Some(hooks) = group.get("hooks").and_then(serde_json::Value::as_array) else {
        return false;
    };
    if group.get("matcher").and_then(serde_json::Value::as_str)
        != Some(crate::routing::Provider::Claude.shell())
        || hooks.len() != 1
    {
        return false;
    }
    let hook = &hooks[0];
    hook.get("type").and_then(serde_json::Value::as_str) == Some("command")
        && hook.get("timeout").and_then(serde_json::Value::as_u64)
            == Some(crate::config::HOOK_TIMEOUT)
        && hook
            .get("command")
            .and_then(serde_json::Value::as_str)
            .and_then(|command| crate::routing::pixel_hook_verb(command, exe))
            == Some("metrics --provider claude")
}

fn antigravity_hook_drift(value: &serde_json::Value, exe: &Path) -> Vec<&'static str> {
    let expected_brief = serde_json::json!({
        "enabled": true,
        "PreInvocation": [{
            "type": "command",
            "command": format!(
                "{} run-hook task-event --provider antigravity --event prompt-submit",
                crate::routing::quoted_executable(exe)
            ),
            "timeout": 10
        }]
    });
    let mut missing = Vec::new();
    let brief = value.get("pixel-brief");
    if brief.is_none_or(|brief| {
        crate::antigravity::is_pixel_brief(brief, exe) && brief != &expected_brief
    }) {
        missing.push("pixel-brief");
    }
    missing
}

#[derive(Debug)]
pub(crate) struct DoctorCheckDetail {
    summary: String,
    detail: Option<serde_json::Value>,
}

/// Which repair command a yellow or red outcome carries.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Remedy {
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

/// `pixel web-search` is the pixel-utilities plugin's: with none installed
/// there is no provider to configure, which is a state and not a defect.
fn web_search_plugin_absent(plugins: &pixel_plugin::Available) -> (CheckStatus, DoctorCheckDetail) {
    let why = plugins.blocked("web-search").map_or_else(
        || {
            format!(
                "install it with `{}`",
                pixel_plugin::install_command("pixel-utilities")
            )
        },
        str::to_owned,
    );
    (
        CheckStatus::Green,
        DoctorCheckDetail {
            summary: format!(
                "`pixel web-search` is the pixel-utilities plugin and does not run here, so there is no provider to check; {why}"
            ),
            detail: Some(serde_json::json!({ "plugin": "pixel-utilities", "runnable": false })),
        },
    )
}

/// Which web search provider `pixel web-search` resolves to for `home`.
/// Green when one is configured; yellow, naming the setup command, when the
/// public chain is the default. The provider is read straight from the
/// environment and the global config file (`~/.pixel/config.yaml`, or the
/// legacy JSON) — never from a spawned `pixel`: the doctor contract tests
/// run the check in-process against the test binary, and a subprocess would
/// re-enter it with `config overview` as a rogue test filter.
pub(crate) fn web_search_provider_check(
    home: &Path,
) -> std::result::Result<(CheckStatus, DoctorCheckDetail), String> {
    web_search_provider_check_with(
        home,
        env_non_empty("PIXEL_WEB_SEARCH_URL"),
        env_non_empty("PERPLEXITY_API_KEY"),
    )
}

/// The check with the environment leg made explicit, so a test can drive
/// the resolution without touching the process environment.
fn web_search_provider_check_with(
    home: &Path,
    searxng_env: bool,
    perplexity_env: bool,
) -> std::result::Result<(CheckStatus, DoctorCheckDetail), String> {
    let doc = load_pixel_config(home)?;
    match web_search_provider_from(searxng_env, perplexity_env, &doc) {
        "none" => Ok((
            CheckStatus::Yellow,
            DoctorCheckDetail {
                summary: "no web search provider configured — the free public chain replies; run `pixel config setup` to configure SearXNG or Perplexity"
                    .to_string(),
                detail: Some(serde_json::json!({ "provider": "none" })),
            },
        )),
        provider => Ok((
            CheckStatus::Green,
            DoctorCheckDetail {
                summary: format!("web search provider: {provider}"),
                detail: Some(serde_json::json!({ "provider": provider })),
            },
        )),
    }
}

/// Resolve the provider the same way `pixel web-search` does: SearXNG when
/// its env var or the stored `web_search.searxng_url` is present, else
/// Perplexity when `PERPLEXITY_API_KEY` or `remote_keys.perplexity` is,
/// else `none` (the public chain is the fallback).
fn web_search_provider_from(searxng_env: bool, perplexity_env: bool, doc: &Value) -> &'static str {
    let searxng_stored = doc
        .get("web_search")
        .and_then(|w| w.get("searxng_url"))
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if searxng_env || searxng_stored {
        return "searxng";
    }
    let perplexity_stored = doc
        .get("remote_keys")
        .and_then(|k| k.get("perplexity"))
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if perplexity_env || perplexity_stored {
        "perplexity"
    } else {
        "none"
    }
}

/// A non-empty environment value, as the CLI reads it. Mirrors
/// `web_search::normalize_base`: a value counts only when it is valid UTF-8
/// and non-empty, so a non-UTF-8 `PIXEL_WEB_SEARCH_URL`/`PERPLEXITY_API_KEY`
/// does not make `pixel doctor` report a provider `pixel web-search` would
/// not select.
fn env_non_empty(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| v.into_string().ok().is_some_and(|s| !s.is_empty()))
}

/// The global pixel config as a JSON value: `~/.pixel/config.yaml` parsed
/// with the same YAML engine the CLI uses, or an empty document when the file
/// does not exist. The two
/// scalar keys the check reads never need more than the mapping shape, but a
/// malformed file is an error rather than a silent `none` — doctor must not
/// hide a config it cannot read.
pub(crate) fn load_pixel_config(home: &Path) -> std::result::Result<Value, String> {
    let yaml = home.join(".pixel/config.yaml");
    if yaml.is_file() {
        let text =
            std::fs::read_to_string(&yaml).map_err(|e| format!("read {}: {e}", yaml.display()))?;
        return parse_pixel_config(&text, &yaml);
    }
    Ok(serde_json::json!({}))
}

/// Parse one pixel config file: `null` (an empty file) reads as an empty
/// document, anything other than a mapping is an error.
fn parse_pixel_config(text: &str, path: &Path) -> std::result::Result<Value, String> {
    let value: Value = serde_saphyr::from_str(text).map_err(|_| {
        format!(
            "invalid configuration {}; expected a YAML mapping",
            path.display()
        )
    })?;
    if value.is_null() {
        return Ok(serde_json::json!({}));
    }
    if !value.is_object() {
        return Err(format!(
            "configuration {} must be a mapping",
            path.display()
        ));
    }
    Ok(value)
}

/// Runs the checks the selection keeps and records each outcome.
struct Runner<'a> {
    only: &'a [String],
    skip: &'a [String],
    root: Option<&'a Path>,
    checks: Vec<DoctorCheck>,
    skipped: usize,
}

impl Runner<'_> {
    /// Run a check that is green on `Ok` and red on `Err`.
    fn check(
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
    fn check_status(
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
    fn record(
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

/// What `facts.freshness` concludes from the history db's counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactsVerdict {
    /// No commit indexed: history was never asked for. Green.
    NotBuilt,
    /// Hunk rows stored without text. Red, with the reason.
    Poisoned(String),
    /// Ingest has not caught up with the refs. Yellow.
    Stale,
    /// Caught up. Green.
    Fresh,
}

/// The `facts.freshness` verdict, factored out of the check so each branch
/// is testable without a real repo.
pub fn facts_verdict(commits_indexed: u64, empty_hunks: i64, fresh: bool) -> FactsVerdict {
    if commits_indexed == 0 {
        return FactsVerdict::NotBuilt;
    }
    if let Some(reason) = facts_poisoned_reason(empty_hunks) {
        return FactsVerdict::Poisoned(reason);
    }
    if fresh {
        FactsVerdict::Fresh
    } else {
        FactsVerdict::Stale
    }
}

/// `bytes` as whole mebibytes, for a check summary.
fn size_mib(bytes: u64) -> String {
    format!("{} MiB", bytes / 1_048_576)
}

/// The poisoned-DB predicate for `facts.freshness`, factored out so it is
/// unit-testable without a real repo: a hunk row without text is the
/// signature of the historical bug that stored every hunk with empty
/// added/removed text, so excavate and diff search returned nothing forever
/// while `diff_state` claimed INDEXED. Ingest writes no row for a hunk that
/// has no text (a binary file, a pure rename), so a repository of binary
/// commits is not flagged.
///
/// An empty db is not dead: history is built on the first history command,
/// so a repository that never ran one has nothing to report.
///
/// Returns `Some(reason)` when the check must go RED.
pub fn facts_poisoned_reason(empty_hunks: i64) -> Option<String> {
    if empty_hunks > 0 {
        return Some(format!(
            "facts db poisoned: {empty_hunks} diff hunks were stored without their text \
             — delete .pixel/history.db or re-run `pixel build-index --history`"
        ));
    }
    None
}

fn age_secs(mtime: SystemTime) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let m = mtime.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    now.saturating_sub(m)
}

/// Extract every `pixel …` command line from the fenced code blocks of a
/// rule document, and every backticked `` `pixel …` `` span of a table row.
/// Trailing `# comments` are stripped from fenced lines and a table cell's
/// `\|` escape becomes `|`; prose and non-pixel lines are ignored.
///
/// Table cells count because agents copy them as literally as the fenced
/// lines: the REPLACEMENT MAP's `pixel new-branch name` had lost its
/// required `--request-id`, and an agent that ran it saw the branch refused
/// and committed on `main`.
pub fn extract_rule_commands(rule_text: &str) -> Vec<String> {
    let mut in_fence = false;
    let mut out = Vec::new();
    for line in rule_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            if trimmed.starts_with('|') {
                out.extend(table_cell_commands(trimmed));
            }
            continue;
        }
        // Strip a trailing shell comment (` # …`) — rule examples annotate
        // commands this way.
        let code = match trimmed.find(" #") {
            Some(i) => trimmed[..i].trim_end(),
            None => trimmed,
        };
        if code.starts_with("pixel ") {
            out.push(code.to_string());
        }
    }
    out
}

/// The backticked `pixel …` spans of one Markdown table row, in order, with
/// the `\|` a cell needs for a literal pipe unescaped.
fn table_cell_commands(row: &str) -> Vec<String> {
    row.split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| span.starts_with("pixel "))
        .map(|span| span.replace("\\|", "|"))
        .collect()
}

/// Normalize one documented `pixel …` line into a parseable argv:
/// - `[…]` optional groups are UNWRAPPED (their flags get tested too);
/// - `a|b|c` alternations pick the first alternative;
/// - `<placeholder>` tokens (quoted or bare) become a dummy value;
/// - bare `N` becomes `3` (numeric flag placeholders);
/// - `/path/to/repo` becomes `.`;
/// - a trailing `...` variadic marker is dropped.
///
/// Returns `None` when the line contains syntax this normalizer cannot
/// handle — the caller reports such lines as "unparsed" instead of silently
/// passing them.
/// The second value a `...` placeholder expands to in
/// [`normalize_rule_command`]. It is distinct from every other dummy so the
/// validator can see which argument it bound to: a flag that takes one value
/// per occurrence pushes it into the next positional (often a defaulted
/// `PATH`), where a plain `x` would parse without a trace.
pub const VARIADIC_SENTINEL: &str = "__pixel_variadic_second_value__";

/// The value a `<placeholder>` stands for in [`normalize_rule_command`]: a
/// number, because it is the one spelling both a text and an integer
/// argument accept (`pixel sniper show <id>` failed on a letter, although
/// an agent substituting a real id would not).
pub const PLACEHOLDER_DUMMY: &str = "1";

pub fn normalize_rule_command(line: &str) -> Option<Vec<String>> {
    // Unwrap bracketed optional groups: brackets may span several
    // whitespace-separated tokens, so strip the characters up front.
    let unbracketed: String = line.chars().filter(|c| *c != '[' && *c != ']').collect();

    // Tokenize, honoring double and single quotes: a prompt single-quotes a
    // uid (`'<uid>'`) so a copied one reaches the shell unexpanded, and each
    // quote character is literal inside the other (`"<the user's words>"`).
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in unbracketed.chars() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), c) if c == open => quote = None,
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            (_, c) => current.push(c),
        }
    }
    if quote.is_some() {
        return None; // unbalanced quotes — can't normalize
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    let mut argv = Vec::with_capacity(tokens.len());
    for token in tokens {
        // A trailing variadic marker (`<f>...`) promises several values
        // after one flag, so it becomes a dummy and `VARIADIC_SENTINEL`: the
        // validator then checks the sentinel bound to an argument that takes
        // several values, as `--files a b` must for the agent that copies it.
        let variadic = token.ends_with("...");
        let token = token.strip_suffix("...").unwrap_or(&token).to_string();
        // Placeholder → dummy value. A quoted multi-word placeholder is one
        // token by now (`<what broke, in the user's words>`).
        let token = if token.starts_with('<') && token.ends_with('>') {
            PLACEHOLDER_DUMMY.to_string()
        } else {
            token
        };
        // Alternation outside placeholders: pick the first alternative
        // (`report|rebase-if-clean` → `report`, `--merge|--stash-first` →
        // `--merge`).
        let token = match token.split_once('|') {
            Some((first, _)) => first.to_string(),
            None => token,
        };
        // Well-known placeholder spellings.
        let token = match token.as_str() {
            "/path/to/repo" => ".".to_string(),
            "N" => "3".to_string(),
            _ => token,
        };
        // Anything still carrying placeholder syntax is beyond this
        // normalizer.
        if token.contains('<') || token.contains('>') || token.contains('…') {
            return None;
        }
        argv.push(token);
        if variadic {
            argv.push(VARIADIC_SENTINEL.to_string());
        }
    }
    if argv.first().map(String::as_str) != Some("pixel") {
        return None;
    }
    Some(argv)
}

/// One NDJSON retrieval round trip against a running daemon socket, checking
/// whether the response carries an `epistemics` object (envelope- or
/// data-level). Short timeouts — this is a health probe, not a query.
fn probe_daemon_epistemics(sock: &Path) -> std::result::Result<bool, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let mut stream = UnixStream::connect(sock).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    let req = serde_json::json!({
        "op": "search",
        "pattern": "fn ",
        "json": true,
        "limit": 1,
    });
    let mut line = req.to_string();
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    stream.flush().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader
        .read_line(&mut buf)
        .map_err(|e| format!("read: {e}"))?;
    let resp: serde_json::Value = serde_json::from_str(&buf).map_err(|e| format!("parse: {e}"))?;
    let has = resp.get("epistemics").is_some()
        || resp
            .get("data")
            .is_some_and(|d| d.get("epistemics").is_some());
    Ok(has)
}

/// Re-export the daemon socket-path helper for the CLI.
pub use pixel_daemon::daemon::socket_path as daemon_socket_path;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::env_non_empty;
    use super::{
        CHECKS, CheckSpec, CheckStatus, DoctorCheck, DoctorReport, PLACEHOLDER_DUMMY, Repair,
        RepairOutcome, RepairStatus, VARIADIC_SENTINEL, age_secs, capped, claude_hooks_owner_check,
        extract_rule_commands, judge_repair, load_pixel_config, names_check,
        normalize_rule_command, one_line, probe_daemon_epistemics, render_catalogue,
        render_repairs, repair_plan, run_repair, selected, shell_path_check, spec,
        split_home_repairs, validate_selection, web_search_provider_check_with,
        web_search_provider_from,
    };
    use super::{FactsVerdict, facts_poisoned_reason, facts_verdict, size_mib};
    use crate::InstallError;

    #[test]
    fn task_observations_should_distinguish_registration_from_real_session_events() {
        let temp = tempfile::tempdir().unwrap();
        let initial = super::task_hook_observations(temp.path()).unwrap();
        assert!(initial.summary.starts_with("0/3"));
        let path = temp.path().join(".pixel/task-hook-observations.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let event = serde_json::json!({"schema_version":1,"provider":"pi","session_id":"real-session","event":"pre-tool-use","observed_unix":123,"coverage":{"native_hooks":true}});
        std::fs::write(
            &path,
            serde_json::json!({"pi":event,"codex":{"registered":true,"trusted":true}}).to_string(),
        )
        .unwrap();
        let observed = super::task_hook_observations(temp.path()).unwrap();
        assert!(observed.summary.starts_with("1/3"));
        let detail = observed.detail.unwrap();
        assert_eq!(detail["hosts"][1]["observed"], false);
        assert_eq!(detail["hosts"][2]["evidence"]["session_id"], "real-session");
        assert_eq!(detail["complete_tool_coverage"], false);
        for (key, invalid) in [
            ("schema_version", serde_json::json!(2)),
            ("provider", serde_json::json!("claude")),
            ("session_id", serde_json::json!("")),
            ("event", serde_json::json!("")),
            ("observed_unix", serde_json::json!("123")),
        ] {
            let mut malformed = event.clone();
            malformed[key] = invalid;
            std::fs::write(&path, serde_json::json!({"pi":malformed}).to_string()).unwrap();
            let detail = super::task_hook_observations(temp.path())
                .unwrap()
                .detail
                .unwrap();
            assert_eq!(detail["hosts"][2]["observed"], false, "{key}");
            assert_eq!(
                detail["hosts"][2]["evidence"],
                serde_json::Value::Null,
                "{key}"
            );
        }
        std::fs::write(&path, "not json").unwrap();
        assert!(super::task_hook_observations(temp.path()).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(super::task_hook_observations(temp.path()).is_err());
    }

    fn finding(
        id: &str,
        status: CheckStatus,
        reason: Option<&str>,
        fix: Option<&str>,
    ) -> DoctorCheck {
        DoctorCheck {
            id: id.into(),
            status,
            required: true,
            duration_ms: 0,
            summary: format!("{id} summary"),
            reason: reason.map(Into::into),
            detail: None,
            fix: fix.map(Into::into),
            repair: None,
        }
    }

    /// A flagged check carrying the catalogue repair `steps`.
    fn repairable(id: &str, status: CheckStatus, fix: &str, steps: &[&[&str]]) -> DoctorCheck {
        DoctorCheck {
            repair: Some(argv(steps)),
            ..finding(id, status, None, Some(fix))
        }
    }

    fn argv(steps: &[&[&str]]) -> Vec<Vec<String>> {
        steps.iter().map(|step| ids(step)).collect()
    }

    fn report(checks: Vec<DoctorCheck>, skipped: usize) -> DoctorReport {
        super::doctor_report(Path::new("/bin/pixel"), Path::new("/home"), checks, skipped)
    }

    #[test]
    fn doctor_report_counts_each_status_and_fails_only_on_red() {
        let passing = report(
            vec![
                finding("a", CheckStatus::Green, None, None),
                finding("b", CheckStatus::Green, None, None),
                finding("c", CheckStatus::Yellow, None, None),
            ],
            4,
        );
        assert!(passing.ok);
        assert_eq!(
            (
                passing.summary.green,
                passing.summary.yellow,
                passing.summary.red,
                passing.summary.skipped
            ),
            (2, 1, 0, 4)
        );
        assert_eq!(passing.executable_path, "/bin/pixel");
        assert_eq!(passing.home, "/home");
        let failing = report(vec![finding("a", CheckStatus::Red, None, None)], 0);
        assert!(!failing.ok);
        assert_eq!(
            (
                failing.summary.green,
                failing.summary.yellow,
                failing.summary.red
            ),
            (0, 0, 1)
        );
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    /// The exit code gates CI and the agent loop: a check exactly at the
    /// threshold must fail it, a milder one must not.
    #[test]
    fn fails_should_trip_at_the_threshold_and_not_below() {
        let green = report(vec![finding("a", CheckStatus::Green, None, None)], 0);
        let yellow = report(
            vec![
                finding("a", CheckStatus::Green, None, None),
                finding("b", CheckStatus::Yellow, None, None),
            ],
            0,
        );
        let red = report(
            vec![finding("c", CheckStatus::Red, Some("broken"), None)],
            0,
        );
        assert!(!green.fails(CheckStatus::Yellow));
        assert!(!green.fails(CheckStatus::Red));
        assert!(yellow.fails(CheckStatus::Yellow));
        assert!(
            !yellow.fails(CheckStatus::Red),
            "yellow alone passes the default gate"
        );
        assert!(red.fails(CheckStatus::Red));
        assert!(red.fails(CheckStatus::Yellow));
    }

    #[test]
    fn check_status_should_display_its_serialized_name() {
        for status in [CheckStatus::Green, CheckStatus::Yellow, CheckStatus::Red] {
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::Value::String(status.to_string())
            );
        }
        assert_eq!(CheckStatus::Yellow.to_string(), "yellow");
    }

    /// The terminal form lists what needs attention, red first, each with the
    /// command that repairs it, and hides the green checks behind the tally.
    #[test]
    fn report_display_should_list_red_before_yellow_with_fix_lines() {
        let text = report(
            vec![
                finding("binary.path", CheckStatus::Green, None, None),
                finding(
                    "facts.freshness",
                    CheckStatus::Yellow,
                    None,
                    Some("pixel build-index --history '/r'"),
                ),
                finding(
                    "install.agent-prompt",
                    CheckStatus::Red,
                    Some("stale\n  prompt"),
                    Some("pixel setup"),
                ),
                finding("daemon.epistemics", CheckStatus::Yellow, None, None),
            ],
            2,
        )
        .to_string();
        let expected = [
            "pixel doctor: ran 4 check(s), skipped 2 — 1 green, 2 yellow, 1 red",
            "  [red] install.agent-prompt: stale prompt",
            "    fix: pixel setup",
            "  [yellow] facts.freshness: facts.freshness summary",
            "    fix: pixel build-index --history '/r'",
            "  [yellow] daemon.epistemics: daemon.epistemics summary",
            "",
        ]
        .join("\n");
        assert_eq!(text, expected);
    }

    #[test]
    fn report_display_should_omit_the_skip_count_when_nothing_was_skipped() {
        let text = report(
            vec![finding("binary.path", CheckStatus::Green, None, None)],
            0,
        )
        .to_string();
        assert_eq!(
            text,
            "pixel doctor: ran 1 check(s) — 1 green, 0 yellow, 0 red\n"
        );
    }

    #[test]
    fn render_catalogue_should_align_ids_and_mark_checks_without_a_fix() {
        let text = render_catalogue(&[
            CheckSpec {
                id: "binary.path",
                fix: None,
            },
            CheckSpec {
                id: "index.freshness",
                fix: Some("pixel prepare-repo {root}"),
            },
        ]);
        assert_eq!(
            text,
            [
                "binary.path      -",
                "index.freshness  pixel prepare-repo {root}",
                ""
            ]
            .join("\n")
        );
    }

    /// `--only`/`--skip` address checks by id, so two entries sharing one
    /// would make a selection ambiguous.
    #[test]
    fn checks_should_have_unique_ids() {
        let mut seen = std::collections::HashSet::new();
        for check in CHECKS {
            assert!(seen.insert(check.id), "duplicate id {}", check.id);
        }
    }

    #[test]
    fn spec_should_return_the_catalogue_entry() {
        assert_eq!(
            spec("facts.freshness").fix,
            Some("pixel build-index --history {root}")
        );
    }

    #[test]
    #[should_panic(expected = "doctor check `nope` is missing from CHECKS")]
    fn spec_should_panic_on_an_uncatalogued_id() {
        let _ = spec("nope");
    }

    /// Hooks that run another binary are yellow for the managed `pixel`,
    /// naming that binary and the `pixel setup` that points them back;
    /// green when they run this one, and green for a side build, whose
    /// sessions are meant to run the managed binary.
    #[test]
    fn claude_hooks_owner_check_should_flag_hooks_running_another_binary() {
        let settings = Path::new("/h/.claude/settings.json");
        let release = Path::new("/h/.local/share/mise/installs/pixel/0.6.1/bin/pixel");
        let dev = PathBuf::from("/h/.local/bin/pixel-dev");
        let (status, detail) = claude_hooks_owner_check(settings, release, &[]);
        assert_eq!(status, CheckStatus::Green);
        assert_eq!(
            detail.summary,
            "claude task-event hooks configured in /h/.claude/settings.json"
        );
        let (status, detail) =
            claude_hooks_owner_check(settings, release, std::slice::from_ref(&dev));
        assert_eq!(status, CheckStatus::Yellow);
        assert_eq!(
            detail.summary,
            "claude task-event hooks in /h/.claude/settings.json run /h/.local/bin/pixel-dev, not this pixel (/h/.local/share/mise/installs/pixel/0.6.1/bin/pixel); every session uses that binary — run `pixel setup` to point them here"
        );
        assert_eq!(
            detail.detail.unwrap()["running"],
            serde_json::json!(["/h/.local/bin/pixel-dev"])
        );
        let (status, _) = claude_hooks_owner_check(settings, &dev, &[release.to_path_buf()]);
        assert_eq!(
            status,
            CheckStatus::Green,
            "a side build does not judge the home install"
        );
    }

    #[test]
    fn selected_should_keep_only_listed_ids_and_drop_skipped_ones() {
        assert!(
            selected(&[], &[], "binary.path"),
            "no selection runs everything"
        );
        assert!(selected(&ids(&["binary.path"]), &[], "binary.path"));
        assert!(!selected(&ids(&["binary.path"]), &[], "daemon.health"));
        assert!(!selected(&[], &ids(&["daemon.health"]), "daemon.health"));
        assert!(selected(&[], &ids(&["daemon.health"]), "binary.path"));
    }

    /// A side build (`pixel-dev`) leaves the home install to the managed
    /// `pixel` with `--skip 'install.*'`: the group must reach every
    /// `install.` check, and nothing else, not even an id that merely starts
    /// with the same letters.
    #[test]
    fn selected_should_treat_a_group_selector_as_every_check_of_that_group() {
        let home: Vec<&str> = CHECKS
            .iter()
            .map(|spec| spec.id)
            .filter(|id| id.starts_with("install."))
            .collect();
        assert_eq!(home.len(), 5, "the install group as catalogued");
        let skip = ids(&["install.*"]);
        for spec in CHECKS {
            assert_eq!(
                selected(&[], &skip, spec.id),
                !home.contains(&spec.id),
                "{}",
                spec.id
            );
        }
        let only = ids(&["repo.*"]);
        assert!(selected(&only, &[], "repo.claude-hooks"));
        assert!(!selected(&only, &[], "install.claude-hooks"));
        assert!(
            !names_check("install.*", "installer.x"),
            "a group ends at its dot"
        );
        assert!(!names_check("install.*", "install"), "a group is not an id");
        assert!(names_check("binary.path", "binary.path"));
        assert!(!names_check("binary.path", "binary.executable"));
    }

    #[test]
    fn validate_selection_should_refuse_unknown_and_conflicting_ids() {
        assert!(validate_selection(&ids(&["binary.path"]), &ids(&["daemon.health"])).is_ok());
        assert!(matches!(
            validate_selection(&ids(&["binary.path", "nope"]), &[]),
            Err(InstallError::UnknownDoctorCheck(id)) if id == "nope"
        ));
        assert!(matches!(
            validate_selection(&[], &ids(&["typo"])),
            Err(InstallError::UnknownDoctorCheck(id)) if id == "typo"
        ));
        assert!(matches!(
            validate_selection(&ids(&["binary.path"]), &ids(&["binary.path"])),
            Err(InstallError::ConflictingDoctorSelection(id)) if id == "binary.path"
        ));
        assert!(validate_selection(&[], &ids(&["install.*"])).is_ok());
        assert!(
            matches!(
                validate_selection(&[], &ids(&["instal.*"])),
                Err(InstallError::UnknownDoctorCheck(id)) if id == "instal.*"
            ),
            "a group no check belongs to is as wrong as a mistyped id"
        );
    }

    /// `--fix` runs a catalogue command by dropping its leading word, so a
    /// template that did not start with `pixel` would run the wrong program.
    #[test]
    fn every_catalogue_command_should_be_a_chain_of_pixel_invocations() {
        for check in CHECKS {
            for command in check.fix.iter().flat_map(|fix| fix.split(" && ")) {
                assert!(command.starts_with("pixel "), "{}: {command}", check.id);
            }
        }
    }

    /// The three outcomes of the shell probe, driven through a script the
    /// test controls: the success guard's two directions and the empty
    /// stdout case each change exactly one of them.
    #[test]
    fn shell_path_check_reports_each_shell_outcome() {
        let dir =
            std::env::temp_dir().join(format!("pixel-doctor-shell-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let make_shell = |tag: &str, lookup: &str, body: &str| {
            let shell = dir.join(tag);
            std::fs::write(
                &shell,
                format!(
                    "#!/bin/sh\n\
                     if [ \"$#\" -ne 3 ] || [ \"$1\" != \"-l\" ] || [ \"$2\" != \"-c\" ] || [ \"$3\" != \"{lookup}\" ]; then\n\
                     \techo \"unexpected lookup args: $*\" >&2\n\
                     \texit 2\n\
                     fi\n\
                     {body}\n"
                ),
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            shell.to_string_lossy().into_owned()
        };
        let posix = "command -v pixel";

        let green = make_shell("green", posix, "echo /fake/bin/pixel; exit 0");
        let (status, detail) = shell_path_check(Some(&green)).unwrap();
        assert_eq!(status, CheckStatus::Green, "{detail:?}");
        assert_eq!(
            detail.detail.as_ref().unwrap()["resolved"],
            "/fake/bin/pixel"
        );

        // Exit 1: `command -v` found nothing — the shell works, pixel is not
        // reachable from it. Yellow, not red: no catalogue command repairs it.
        // The detail carries the shell's own words (an exit 1 a profile
        // script printed an error into is diagnosable), which is what tells
        // this miss apart from an empty-success one.
        let yellow = make_shell("yellow", posix, "exit 1");
        let (status, detail) = shell_path_check(Some(&yellow)).unwrap();
        assert_eq!(status, CheckStatus::Yellow, "{detail:?}");
        let yellow = detail.detail.unwrap();
        assert!(yellow["exit_status"].is_string(), "{yellow}");

        // Success with empty stdout is the same miss, not a green check —
        // and it is not the exit-1 shape either: nothing to report yet.
        let empty = make_shell("empty", posix, "exit 0");
        let (status, detail) = shell_path_check(Some(&empty)).unwrap();
        assert_eq!(status, CheckStatus::Yellow, "{detail:?}");
        assert_eq!(detail.detail.as_ref().unwrap()["shell"], empty);
        assert!(
            detail.detail.as_ref().unwrap()["exit_status"].is_null(),
            "an empty success has no exit status to report: {detail:?}"
        );

        // A shell named `fish` is asked fish's own lookup, `which pixel`;
        // the argument guard fails the probe if the POSIX query is sent.
        let fish = make_shell("fish", "which pixel", "echo /fake/bin/pixel; exit 0");
        let (status, detail) = shell_path_check(Some(&fish)).unwrap();
        assert_eq!(status, CheckStatus::Green, "{detail:?}");
        assert_eq!(
            detail.detail.as_ref().unwrap()["resolved"],
            "/fake/bin/pixel"
        );

        // A shell the OS cannot exec: red, a different failure from a miss.
        let err = shell_path_check(Some(dir.join("no-such-shell").to_string_lossy().as_ref()))
            .expect_err("a spawn failure is red");
        assert!(err.contains("failed to run shell"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A login shell stuck in its startup files is reported, never waited
    /// on: the injectable deadline keeps the test at milliseconds where
    /// production holds the shell to five seconds.
    #[test]
    fn shell_path_check_reports_a_shell_that_does_not_answer_in_time() {
        let dir = std::env::temp_dir().join(format!("pixel-doctor-timeout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let shell = dir.join("slow-zsh");
        std::fs::write(&shell, "#!/bin/sh\nsleep 5\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let timeout = std::time::Duration::from_millis(100);
        let err = super::shell_path_check_within(Some(shell.to_string_lossy().as_ref()), timeout)
            .expect_err("a stalled shell is red, not a hang");
        assert!(err.contains("did not answer within"), "{err}");
        assert!(err.contains(&format!("{timeout:?}")), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The overview line is the only authority the check trusts: the three
    /// The resolution mirrors `pixel web-search`: SearXNG first — env or
    /// stored — then Perplexity, then the public chain; an empty stored
    /// value counts as absent.
    #[test]
    fn web_search_provider_resolution_prefers_searxng_then_perplexity_then_the_chain() {
        let stored = |searxng: bool, perplexity: bool| {
            let mut doc = serde_json::json!({});
            if searxng {
                doc["web_search"] = serde_json::json!({ "searxng_url": "https://sx.test" });
            }
            if perplexity {
                doc["remote_keys"] = serde_json::json!({ "perplexity": "sk-pplx" });
            }
            doc
        };
        assert_eq!(
            web_search_provider_from(true, false, &stored(false, false)),
            "searxng"
        );
        assert_eq!(
            web_search_provider_from(false, false, &stored(true, false)),
            "searxng"
        );
        // A SearXNG env var or stored URL wins over a Perplexity key.
        assert_eq!(
            web_search_provider_from(true, false, &stored(false, true)),
            "searxng"
        );
        assert_eq!(
            web_search_provider_from(false, false, &stored(true, true)),
            "searxng"
        );
        assert_eq!(
            web_search_provider_from(false, true, &stored(false, false)),
            "perplexity"
        );
        assert_eq!(
            web_search_provider_from(false, false, &stored(false, true)),
            "perplexity"
        );
        assert_eq!(
            web_search_provider_from(false, false, &stored(false, false)),
            "none"
        );
        let empty = serde_json::json!({
            "web_search": { "searxng_url": "" },
            "remote_keys": { "perplexity": "" },
        });
        assert_eq!(web_search_provider_from(false, false, &empty), "none");
    }

    /// The env wrapper is the one path the resolution tests cannot reach
    /// with literal flags: a real `pixel doctor` reads the provider env
    /// vars itself. Each assertion pins one guard mutation (non-empty wins,
    /// empty is absent, unset is absent).
    #[test]
    fn web_search_env_flags_drive_the_provider_check() {
        let home = tempfile::tempdir().unwrap();
        let provider = || {
            let (_, check) = super::web_search_provider_check(home.path()).unwrap();
            check.detail.unwrap()["provider"]
                .as_str()
                .unwrap_or("?")
                .to_string()
        };
        // SAFETY: nextest isolates one process per test (this repo's
        // nextest config relies on exactly that), so mutating the process
        // env here races with no other test.
        unsafe {
            std::env::set_var("PIXEL_WEB_SEARCH_URL", "https://sx.test");
        };
        // SAFETY: nextest isolates one process per test (this repo's
        // nextest config relies on exactly that), so mutating the process
        // env here races with no other test.
        unsafe {
            std::env::remove_var("PERPLEXITY_API_KEY");
        };
        assert_eq!(provider(), "searxng");
        // An empty value counts as absent, exactly like the CLI.
        // SAFETY: nextest isolates one process per test (this repo's
        // nextest config relies on exactly that), so mutating the process
        // env here races with no other test.
        unsafe {
            std::env::set_var("PIXEL_WEB_SEARCH_URL", "");
        };
        assert_eq!(provider(), "none");
        // Unset leaves the public chain as the fallback.
        // SAFETY: nextest isolates one process per test (this repo's
        // nextest config relies on exactly that), so mutating the process
        // env here races with no other test.
        unsafe {
            std::env::remove_var("PIXEL_WEB_SEARCH_URL");
        };
        assert_eq!(provider(), "none");
    }

    /// The global config reads as the YAML file (parsed with the CLI's own
    /// engine), and nothing when it does not exist; a file that is not a
    /// mapping is an error, not a silent `none`.
    #[test]
    fn web_search_config_reads_yaml_or_absent() {
        let dir = std::env::temp_dir().join(format!(
            "pixel-doctor-web-search-config-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".pixel")).unwrap();

        assert_eq!(load_pixel_config(&home).unwrap(), serde_json::json!({}));

        std::fs::write(
            home.join(".pixel/config.yaml"),
            "web_search:\n  searxng_url: https://sx.test\nremote_keys:\n  perplexity: pplx\n",
        )
        .unwrap();
        let doc = load_pixel_config(&home).unwrap();
        assert_eq!(doc["web_search"]["searxng_url"], "https://sx.test");
        assert_eq!(doc["remote_keys"]["perplexity"], "pplx");

        // An empty YAML file is an empty document; anything that is not a
        // mapping is an error the check must not gloss over.
        std::fs::write(home.join(".pixel/config.yaml"), "").unwrap();
        assert_eq!(load_pixel_config(&home).unwrap(), serde_json::json!({}));
        std::fs::write(home.join(".pixel/config.yaml"), "web_search: [\n").unwrap();
        assert!(
            load_pixel_config(&home).is_err(),
            "a malformed config is an error, not a silent none"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The check's verdict over a scratch home: `none` is a yellow
    /// suggestion naming `pixel config setup`, a stored or env provider is
    /// green, and an unreadable config is red.
    #[test]
    fn web_search_provider_check_reads_the_config_and_maps_its_verdict() {
        let dir = std::env::temp_dir().join(format!(
            "pixel-doctor-web-search-check-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".pixel")).unwrap();

        let (status, detail) = web_search_provider_check_with(&home, false, false).unwrap();
        assert_eq!(status, CheckStatus::Yellow, "{detail:?}");
        assert_eq!(detail.detail.as_ref().unwrap()["provider"], "none");
        assert!(detail.summary.contains("pixel config setup"), "{detail:?}");

        let (status, detail) = web_search_provider_check_with(&home, false, true).unwrap();
        assert_eq!(status, CheckStatus::Green, "{detail:?}");
        assert_eq!(detail.detail.as_ref().unwrap()["provider"], "perplexity");

        std::fs::write(
            home.join(".pixel/config.yaml"),
            "web_search:\n  searxng_url: https://sx.test\n",
        )
        .unwrap();
        let (status, detail) = web_search_provider_check_with(&home, false, true).unwrap();
        assert_eq!(status, CheckStatus::Green, "{detail:?}");
        assert_eq!(detail.detail.as_ref().unwrap()["provider"], "searxng");

        std::fs::write(home.join(".pixel/config.yaml"), "web_search: [\n").unwrap();
        let err = web_search_provider_check_with(&home, false, false)
            .expect_err("a malformed config is red");
        assert!(err.contains("invalid configuration"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `env_non_empty` drives the env leg of the check: a value counts as
    /// configured only when it is present, valid UTF-8, and non-empty. The
    /// stored-config tests go through `web_search_provider_check_with` with
    /// explicit booleans, so this raw read needs its own cases. A dedicated
    /// name keeps the `PIXEL_WEB_SEARCH_URL`/`PERPLEXITY_API_KEY` the CLI
    /// really reads untouched.
    #[test]
    fn env_non_empty_counts_only_a_present_non_empty_utf8_value() {
        use std::os::unix::ffi::OsStringExt;
        const VAR: &str = "PIXEL_QR_DOCTOR_WEB_SEARCH_PROBE";

        // SAFETY: nextest runs one process per test (this repo's nextest
        // config leans on exactly that), so the process env below races
        // with no other test.
        unsafe { std::env::remove_var(VAR) };
        assert!(!env_non_empty(VAR), "absent counts as not configured");

        // SAFETY: see the comment above; same isolation guarantees.
        unsafe { std::env::set_var(VAR, "") };
        assert!(!env_non_empty(VAR), "empty counts as not configured");

        // SAFETY: see the comment above; same isolation guarantees.
        unsafe { std::env::set_var(VAR, "https://sx.test") };
        assert!(env_non_empty(VAR), "non-empty counts as configured");

        // A value the CLI cannot read as UTF-8 is unusable, so it too counts
        // as not configured rather than selecting a provider.
        let non_utf8 = std::ffi::OsString::from_vec(vec![0xf0, 0x28, 0x8c, 0x28]);
        // SAFETY: see the comment above; same isolation guarantees.
        unsafe { std::env::set_var(VAR, non_utf8) };
        assert!(!env_non_empty(VAR), "non-UTF-8 counts as not configured");

        // SAFETY: see the comment above; same isolation guarantees.
        unsafe { std::env::remove_var(VAR) };
    }

    /// A stalled shell must not outlive the probe: `bounded_output` returns
    /// `Timeout` under the deadline instead of `Command::output()`'s
    /// indefinite wait on a login shell stuck in its startup files.
    #[test]
    fn bounded_output_times_out_on_a_stalled_shell() {
        let started = std::time::Instant::now();
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg("sleep 5");
        let outcome = super::bounded_output(&mut command, std::time::Duration::from_millis(100));
        assert!(
            matches!(&outcome, Err(super::ProbeFailure::Timeout)),
            "a stalled shell is a timeout, got {outcome:?}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "the deadline must bound the wait, took {:?}",
            started.elapsed()
        );
    }

    /// Twelve checks share `pixel setup`: the plan runs it once, lists
    /// every check it repairs, and keeps the report's order.
    /// A side build's `--fix` keeps every repair but the home install's: a
    /// `pixel setup` step without `--repo`, even inside a chain, is left to
    /// the managed pixel, while the managed pixel itself runs everything.
    #[test]
    fn split_home_repairs_should_leave_the_home_install_to_the_managed_pixel() {
        let repair = |command: &str, steps: &[&[&str]]| Repair {
            command: command.into(),
            steps: argv(steps),
            checks: ids(&["x"]),
        };
        let home = repair("pixel setup", &[&["setup", "--shell", "fish"]]);
        let chained = repair(
            "pixel daemon stop && pixel setup",
            &[&["daemon", "stop", "/r"], &["setup"]],
        );
        let repo = repair("pixel setup --repo /r", &[&["setup", "--repo", "/r"]]);
        let prepare = repair("pixel prepare-repo /r", &[&["prepare-repo", "/r"]]);
        let plan = vec![home.clone(), chained.clone(), repo.clone(), prepare.clone()];
        assert_eq!(
            split_home_repairs(plan.clone(), true),
            (vec![repo, prepare], vec![home, chained])
        );
        assert_eq!(split_home_repairs(plan.clone(), false), (plan, vec![]));
    }

    #[test]
    fn repair_plan_should_run_each_command_once_in_report_order() {
        let install: &[&[&str]] = &[&["setup"]];
        let prepare: &[&[&str]] = &[&["prepare-repo", "/r"]];
        let plan = repair_plan(&report(
            vec![
                finding("binary.path", CheckStatus::Green, None, None),
                repairable(
                    "install.agent-prompt",
                    CheckStatus::Red,
                    "pixel setup",
                    install,
                ),
                finding(
                    "install.rtk-backup",
                    CheckStatus::Yellow,
                    None,
                    Some("rm '/h/rtk.json'"),
                ),
                repairable("rule.parity", CheckStatus::Yellow, "pixel setup", install),
                repairable(
                    "index.freshness",
                    CheckStatus::Yellow,
                    "pixel prepare-repo '/r'",
                    prepare,
                ),
                repairable(
                    "graph.freshness",
                    CheckStatus::Red,
                    "pixel prepare-repo '/r'",
                    prepare,
                ),
            ],
            0,
        ));
        assert_eq!(
            plan,
            vec![
                Repair {
                    command: "pixel setup".into(),
                    steps: argv(install),
                    checks: ids(&["install.agent-prompt", "rule.parity"]),
                },
                Repair {
                    command: "pixel prepare-repo '/r'".into(),
                    steps: argv(prepare),
                    checks: ids(&["index.freshness", "graph.freshness"]),
                },
            ]
        );
    }

    fn sh_repair(steps: &[&[&str]]) -> Repair {
        Repair {
            command: "c".into(),
            steps: argv(steps),
            checks: vec![],
        }
    }

    #[test]
    fn run_repair_should_run_every_step_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let append = |word: &str| format!("echo {word} >> '{}'", log.display());
        let (first, second) = (append("one"), append("two"));
        let repair = sh_repair(&[&["-c", &first], &["-c", &second]]);
        assert_eq!(run_repair(Path::new("/bin/sh"), &repair), Ok(()));
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "one\ntwo\n");
    }

    /// Like `&&`: once a step fails the rest of the repair does not run, and
    /// the error names the step and quotes its stderr.
    #[test]
    fn run_repair_should_stop_at_the_first_failing_step() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let touch = format!("touch '{}'", marker.display());
        let repair = sh_repair(&[&["-c", "echo boom >&2; exit 3"], &["-c", &touch]]);
        let error = run_repair(Path::new("/bin/sh"), &repair).unwrap_err();
        assert_eq!(
            error,
            "`pixel -c echo boom >&2; exit 3` failed (exit status: 3): boom"
        );
        assert!(!marker.exists(), "the step after the failure ran");
        let silent = run_repair(Path::new("/bin/sh"), &sh_repair(&[&["-c", "exit 4"]]));
        assert_eq!(
            silent,
            Err("`pixel -c exit 4` failed (exit status: 4)".into())
        );
    }

    #[test]
    fn run_repair_should_report_a_binary_that_cannot_start() {
        let error =
            run_repair(Path::new("/nonexistent/pixel"), &sh_repair(&[&["setup"]])).unwrap_err();
        assert!(
            error.starts_with("`pixel setup` could not start: "),
            "{error}"
        );
    }

    /// A command that exits 0 is not proof: the verdict comes from the checks
    /// re-run after every repair.
    #[test]
    fn judge_repair_should_trust_the_rerun_checks_over_the_exit_code() {
        let after = report(
            vec![
                finding("install.agent-prompt", CheckStatus::Green, None, None),
                finding("index.freshness", CheckStatus::Green, None, None),
                finding("graph.freshness", CheckStatus::Yellow, None, None),
            ],
            0,
        );
        let repair = |checks: &[&str]| Repair {
            command: "pixel x".into(),
            steps: vec![],
            checks: ids(checks),
        };
        let fixed = judge_repair(repair(&["install.agent-prompt"]), Ok(()), &after);
        assert_eq!(
            (fixed.status, fixed.still_flagged),
            (RepairStatus::Fixed, vec![])
        );
        let stuck = judge_repair(
            repair(&["index.freshness", "graph.freshness"]),
            Ok(()),
            &after,
        );
        assert_eq!(
            (stuck.status, stuck.still_flagged, stuck.error),
            (RepairStatus::NotConverged, ids(&["graph.freshness"]), None)
        );
        let vanished = judge_repair(repair(&["daemon.health"]), Ok(()), &after);
        assert_eq!(
            (vanished.status, vanished.still_flagged),
            (RepairStatus::NotConverged, ids(&["daemon.health"]))
        );
        let failed = judge_repair(
            repair(&["install.agent-prompt"]),
            Err("boom".into()),
            &after,
        );
        assert_eq!(
            (failed.status, failed.error.as_deref()),
            (RepairStatus::Failed, Some("boom"))
        );
    }

    #[test]
    fn render_repairs_should_tally_and_detail_every_outcome() {
        let outcome =
            |status, checks: &[&str], still: &[&str], error: Option<&str>| RepairOutcome {
                command: format!("pixel {status}"),
                checks: ids(checks),
                status,
                still_flagged: ids(still),
                error: error.map(Into::into),
            };
        let text = render_repairs(&[
            outcome(RepairStatus::Fixed, &["a", "b"], &[], None),
            outcome(RepairStatus::NotConverged, &["c", "d"], &["d"], None),
            outcome(
                RepairStatus::Failed,
                &["e"],
                &["e"],
                Some("`pixel e` failed"),
            ),
        ]);
        let expected = [
            "pixel doctor --fix: ran 3 repair(s) — 1 fixed, 1 not converged, 1 failed",
            "  [fixed] pixel fixed (a, b)",
            "  [not converged] pixel not converged (c, d)",
            "    still flagged: d",
            "  [failed] pixel failed (e)",
            "    error: `pixel e` failed",
            "    still flagged: e",
            "",
        ]
        .join("\n");
        assert_eq!(text, expected);
        assert_eq!(
            render_repairs(&[]),
            "pixel doctor --fix: nothing to repair automatically\n"
        );
    }

    #[test]
    fn repair_status_should_serialize_in_snake_case() {
        assert_eq!(
            serde_json::to_value(RepairStatus::NotConverged).unwrap(),
            "not_converged"
        );
    }

    #[test]
    fn one_line_should_fold_newlines_and_control_characters_into_single_spaces() {
        assert_eq!(
            one_line("  error:\n\tbad\x1b[31m  thing \r\n"),
            "error: bad [31m thing"
        );
        assert_eq!(one_line("plain"), "plain");
    }

    #[test]
    fn capped_should_keep_text_at_the_limit_and_cut_beyond_it() {
        assert_eq!(capped("abcd", 4), "abcd", "exactly at the cap is kept");
        assert_eq!(capped("abcde", 4), "abc…");
        assert_eq!(
            capped("éééé", 3).chars().count(),
            3,
            "counts characters, not bytes"
        );
    }

    #[test]
    fn age_secs_is_the_seconds_since_the_mtime() {
        let ninety_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(90);
        let age = age_secs(ninety_ago);
        assert!((90..=91).contains(&age), "{age}");
        assert_eq!(
            age_secs(std::time::SystemTime::now() + std::time::Duration::from_secs(60)),
            0
        );
    }

    /// The daemon health probe reads one NDJSON answer and looks for the
    /// epistemics object at either level; a daemon that answers without one
    /// is reported unhealthy, not as an error.
    #[test]
    fn probe_daemon_epistemics_reads_one_answer_from_the_socket() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("pixel-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (answer, expected) in [
            (
                r#"{"ok":true,"op":"search","epistemics":{"basis":"x"}}"#,
                true,
            ),
            (
                r#"{"ok":true,"op":"search","data":{"epistemics":{}}}"#,
                true,
            ),
            (r#"{"ok":true,"op":"search","data":{}}"#, false),
        ] {
            let sock = dir.join("daemon.sock");
            let _ = std::fs::remove_file(&sock);
            let listener = UnixListener::bind(&sock).unwrap();
            // Poll instead of blocking: a probe that never connects must
            // leave a failed assertion, not a hung test.
            listener.set_nonblocking(true).unwrap();
            let sent = answer.to_string();
            let server = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                let stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if std::time::Instant::now() > deadline {
                                return;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(e) => panic!("accept: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let req: serde_json::Value = serde_json::from_str(&request).unwrap();
                assert_eq!(req["op"], "search");
                let mut stream = stream;
                writeln!(stream, "{sent}").unwrap();
            });
            assert_eq!(probe_daemon_epistemics(&sock), Ok(expected), "{answer}");
            server.join().unwrap();
        }
        let err = probe_daemon_epistemics(&dir.join("absent.sock")).unwrap_err();
        assert!(err.starts_with("connect: "), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- rule-vs-binary parity: extraction + normalization ------------------

    const SAMPLE_RULE: &str = r#"
## Scenario 1

```bash
# Deleted or currently-nonexistent code: search all history, stash, and reflog
pixel dig-history --phrase "<what you're looking for>" [--path <path>] [--json]

pixel plan-rollback "<what broke, in the user's words>" /path/to/repo [--json]

pixel plan-rollback --apply <oid> --file <path> /path/to/repo [--merge|--stash-first|--allow-dirty]
```

Prose mentioning `pixel doctor` inline must NOT be extracted.

```bash
pixel scope-task --clear /path/to/repo   # when the task ends
pixel sync-branch /path/to/repo [--strategy report|rebase-if-clean] [--push auto|never]
git clone https://example.com/repo.git
```
"#;

    #[test]
    fn extracts_only_fenced_pixel_lines_and_strips_comments() {
        let commands = extract_rule_commands(SAMPLE_RULE);
        assert_eq!(
            commands,
            vec![
                "pixel dig-history --phrase \"<what you're looking for>\" [--path <path>] [--json]",
                "pixel plan-rollback \"<what broke, in the user's words>\" /path/to/repo [--json]",
                "pixel plan-rollback --apply <oid> --file <path> /path/to/repo [--merge|--stash-first|--allow-dirty]",
                "pixel scope-task --clear /path/to/repo",
                "pixel sync-branch /path/to/repo [--strategy report|rebase-if-clean] [--push auto|never]",
            ],
            "must extract exactly the fenced pixel lines, comment-stripped, no inline prose"
        );
    }

    /// A table cell is copied as literally as a fenced line, so its
    /// `pixel …` spans are extracted too — each span of a row, in order,
    /// with the cell's `\|` unescaped — while a non-pixel span, a span in a
    /// prose line and a `pixel-…` word stay out.
    #[test]
    fn extracts_every_pixel_span_of_a_table_row() {
        let text = "\
| Instead of | Run |
| --- | --- |
| `git checkout -b` / `git fetch` | `pixel new-branch <name> --request-id <id>` / `pixel fetch origin` |
| roles | `pixel who-calls \"X\" --role callers\\|callees` |
| wrapper | `pixel-dev` |
Prose naming `pixel status` is not a table row.
";
        assert_eq!(
            extract_rule_commands(text),
            vec![
                "pixel new-branch <name> --request-id <id>",
                "pixel fetch origin",
                "pixel who-calls \"X\" --role callers|callees",
            ]
        );
    }

    #[test]
    fn normalizes_placeholders_brackets_and_alternations() {
        assert_eq!(
            normalize_rule_command(
                "pixel find-code \"<phrase>\" /path/to/repo [--json] [--limit N]"
            ),
            Some(vec![
                "pixel".into(),
                "find-code".into(),
                PLACEHOLDER_DUMMY.into(),
                ".".into(),
                "--json".into(),
                "--limit".into(),
                "3".into(),
            ])
        );
        assert_eq!(
            normalize_rule_command(
                "pixel sync-branch /path/to/repo [--strategy report|rebase-if-clean] [--push auto|never]"
            ),
            Some(vec![
                "pixel".into(),
                "sync-branch".into(),
                ".".into(),
                "--strategy".into(),
                "report".into(),
                "--push".into(),
                "auto".into(),
            ])
        );
        // A variadic placeholder stands for two values, the second one the
        // sentinel the validator traces.
        assert_eq!(
            normalize_rule_command(
                "pixel commit --files <f>... --message \"<msg>\" --request-id <id> /path/to/repo"
            ),
            Some(vec![
                "pixel".into(),
                "commit".into(),
                "--files".into(),
                PLACEHOLDER_DUMMY.into(),
                VARIADIC_SENTINEL.into(),
                "--message".into(),
                PLACEHOLDER_DUMMY.into(),
                "--request-id".into(),
                PLACEHOLDER_DUMMY.into(),
                ".".into(),
            ])
        );
        // Bracketed flag alternation picks the first flag.
        assert_eq!(
            normalize_rule_command(
                "pixel plan-rollback --apply <oid> --file <path> /path/to/repo [--merge|--stash-first|--allow-dirty]"
            )
            .as_deref()
            .and_then(|v| v.last().cloned()),
            Some("--merge".to_string())
        );
    }

    /// A prompt single-quotes the uids an agent copies so the shell passes
    /// them unexpanded; the normalizer must still check those lines rather
    /// than report them unparsed, and an apostrophe inside double quotes
    /// stays prose, not the start of a quote.
    #[test]
    fn single_quoted_placeholders_are_normalized_like_double_quoted_ones() {
        assert_eq!(
            normalize_rule_command("pixel evaluate path --from '<uid>' --to '<uid>' --json"),
            Some(vec![
                "pixel".into(),
                "evaluate".into(),
                "path".into(),
                "--from".into(),
                PLACEHOLDER_DUMMY.into(),
                "--to".into(),
                PLACEHOLDER_DUMMY.into(),
                "--json".into(),
            ])
        );
        assert_eq!(
            normalize_rule_command("pixel plan-rollback \"<what broke, in the user's words>\""),
            Some(vec![
                "pixel".into(),
                "plan-rollback".into(),
                PLACEHOLDER_DUMMY.into()
            ])
        );
        assert_eq!(
            normalize_rule_command("pixel impact 'it\"s' --json"),
            Some(vec![
                "pixel".into(),
                "impact".into(),
                "it\"s".into(),
                "--json".into(),
            ])
        );
    }

    #[test]
    fn unnormalizable_lines_are_reported_not_silently_passed() {
        // Unbalanced quotes, of either kind.
        assert_eq!(
            normalize_rule_command("pixel search-content \"unclosed"),
            None
        );
        assert_eq!(
            normalize_rule_command("pixel search-content 'unclosed"),
            None
        );
        // Ellipsis placeholder syntax the normalizer doesn't understand.
        assert_eq!(normalize_rule_command("pixel search-content a…b"), None);
        // Half a placeholder is not one: it stays unreadable rather than
        // passing as a dummy value.
        assert_eq!(normalize_rule_command("pixel impact <symbol"), None);
        assert_eq!(normalize_rule_command("pixel impact symbol>"), None);
        // Not a pixel line at all.
        assert_eq!(normalize_rule_command("git status"), None);
    }

    // -- scenario consistency ------------------------------------------------

    #[test]
    fn poisoned_db_signature_is_red() {
        // The real-world poisoned DB: 11 commits marked indexed, 323 hunks all
        // stored with empty text.
        let reason = facts_poisoned_reason(323);
        assert!(
            reason.as_deref().unwrap_or("").contains("poisoned"),
            "hunk rows without text must be flagged poisoned, got {reason:?}"
        );
        assert!(
            facts_poisoned_reason(1).is_some(),
            "one lost hunk is enough"
        );
    }

    /// History that was never asked for, or whose commits carry no text at
    /// all (binary files write no hunk row), is not an error: `doctor --fix`
    /// must not turn a health check into a full history build.
    #[test]
    fn healthy_and_never_built_cases_are_not_red() {
        assert_eq!(facts_poisoned_reason(0), None);
    }

    #[test]
    fn facts_verdict_reads_not_built_then_poisoned_then_freshness() {
        assert_eq!(facts_verdict(0, 0, false), FactsVerdict::NotBuilt);
        assert_eq!(facts_verdict(0, 5, true), FactsVerdict::NotBuilt);
        assert!(matches!(
            facts_verdict(11, 323, true),
            FactsVerdict::Poisoned(r) if r.contains("323 diff hunks")
        ));
        assert_eq!(facts_verdict(21, 0, true), FactsVerdict::Fresh);
        assert_eq!(facts_verdict(21, 0, false), FactsVerdict::Stale);
    }

    #[test]
    fn size_mib_prints_whole_mebibytes() {
        assert_eq!(size_mib(268_435_456), "256 MiB");
        assert_eq!(size_mib(3_145_727), "2 MiB");
        assert_eq!(size_mib(0), "0 MiB");
    }

    #[test]
    fn pi_impact_doctor_should_not_offer_install_for_unparsable_settings_or_a_file_agent_dir() {
        let home = tempfile::tempdir().unwrap();
        let paths = crate::pi_global::PiPaths::resolve(home.path(), true);
        let check = || {
            let report = super::doctor(&super::DoctorOptions {
                home: Some(home.path().to_path_buf()),
                executable_path: Some(PathBuf::from("/opt/pixel/pixel")),
                only: vec!["install.pi-impact".into()],
                ..Default::default()
            })
            .unwrap();
            report.checks.into_iter().next().unwrap()
        };
        std::fs::create_dir_all(&paths.agent_dir).unwrap();
        std::fs::write(&paths.settings, "{ not json").unwrap();
        let unparsable = check();
        assert_eq!(unparsable.status, CheckStatus::Yellow, "{unparsable:?}");
        assert_eq!(unparsable.fix, None);

        std::fs::remove_dir_all(&paths.agent_dir).unwrap();
        std::fs::write(&paths.agent_dir, "user-owned file").unwrap();
        let occupied = check();
        assert_eq!(occupied.status, CheckStatus::Yellow, "{occupied:?}");
        assert_eq!(occupied.fix, None);
        assert!(occupied.summary.contains("not a directory"), "{occupied:?}");
    }

    #[test]
    fn the_web_search_check_should_not_ask_for_a_provider_when_its_plugin_does_not_run() {
        let home = tempfile::tempdir().unwrap();
        let run = |plugins: pixel_plugin::Available| {
            super::doctor(&super::DoctorOptions {
                home: Some(home.path().to_path_buf()),
                executable_path: Some(PathBuf::from("/opt/pixel/pixel")),
                only: vec!["web-search.provider".into()],
                plugins: Some(plugins),
                ..Default::default()
            })
            .unwrap()
            .checks
            .remove(0)
        };
        let absent = run(pixel_plugin::Available::none());
        assert_eq!(absent.status, CheckStatus::Green, "{absent:?}");
        assert!(
            absent
                .summary
                .ends_with("install it with `pixel install github:Pixel-CLI/pixel-utilities`"),
            "{}",
            absent.summary
        );
        // With the plugin the provider is judged as before: none configured is yellow.
        let present = run(pixel_plugin::Available::of(&["web-search"]));
        assert_eq!(present.status, CheckStatus::Yellow, "{present:?}");
        assert!(
            present
                .summary
                .contains("no web search provider configured")
        );
    }

    #[test]
    fn antigravity_doctor_should_preserve_a_user_defined_pixel_brief() {
        let exe = PathBuf::from("/opt/pixel/pixel");
        let hooks = serde_json::json!({
            "pixel-brief": {
                "enabled": true,
                "PreInvocation": [{
                    "type": "command",
                    "command": "user-brief",
                    "timeout": 10
                }]
            }
        });

        assert_eq!(
            super::antigravity_hook_drift(&hooks, &exe),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn antigravity_doctor_should_still_reject_a_disabled_pixel_owned_brief() {
        let exe = PathBuf::from("/opt/pixel/pixel");
        let hooks = serde_json::json!({
            "pixel-brief": {
                "enabled": false,
                "PreInvocation": [{
                    "type": "command",
                    "command": format!(
                        "{} run-hook task-event --provider antigravity --event prompt-submit",
                        crate::routing::quoted_executable(&exe)
                    ),
                    "timeout": 10
                }]
            }
        });

        assert_eq!(
            super::antigravity_hook_drift(&hooks, &exe),
            vec!["pixel-brief"]
        );
    }
}
