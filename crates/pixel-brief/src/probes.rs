// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Brief probes: scripts of the plugin host (capability `brief-probe`) that add
//! blocks to the prompt-submit brief.
//!
//! After the brief is built, every probe whose `when` fits the question runs in
//! parallel, each inside its own `budget_ms` and inside what is left of the
//! brief's window, with `{prompt, repo_root, brief, kind, session_files}` as
//! JSON on stdin. What it prints is a list of blocks (`### header`, then
//! lines) and `drop: <regex>` / `first: <regex>` directives; [`merge`] folds
//! them into the brief. A probe that times out, fails or prints nothing is
//! skipped and logged; the brief is never worse for a probe that broke.
//!
//! [`merge`] is pinned by the golden cases of `tests/fixtures/probe-merge.json`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pixel_plugin::{Host, Probe, ProbeMode};
use regex::Regex;
use serde_json::json;

/// Environment opt-out: `0`, `false` or `off` runs no probe.
pub const PROBES_ENV: &str = "PIXEL_BRIEF_PROBES";
/// Most bytes of a probe's output that are read; the rest is dropped.
const MAX_OUTPUT_BYTES: u64 = 1 << 20;
/// Lines that close a brief and stay last whatever is merged before them.
const FOOTER_PREFIXES: &[&str] = &[
    "Answer from this context.",
    "These are the regions closest",
    "pack incomplete",
    "confidence:",
];
/// The tag a brief opens on; an empty brief gets it back when probes add to it.
const BRIEF_TAG: &str = "[PIXEL:BRIEF]";

/// What the journal keeps of one probe run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeRun {
    pub name: String,
    pub ms: u64,
    pub bytes: usize,
    /// `ok`, `empty`, `timeout`, `failed` or `skipped`.
    pub outcome: &'static str,
}

/// What the probes are given at least after the chain: a hook gives them this
/// much when the chain used its window up; the `brief` command gives nothing
/// (OpenCode kills it at 1.2 s).
pub(crate) const PROBE_FLOOR: Duration = Duration::from_millis(1_200);
/// No probe outlives this after the prompt arrived: under Pi's six-second hook
/// decision limit, with room to print, and under what the parts after the first
/// wait for part 1.
const PROBE_CEILING: Duration = Duration::from_millis(5_000);
/// Bytes a merged brief may hold when it is the `brief` command's single part:
/// OpenCode's extension drops a longer output.
pub(crate) const COMMAND_MAX_BYTES: usize = 3_990;

/// When the probes of a brief that started at `started`, whose window ends at
/// `window_end`, must be done: the end of the window, or `floor` from `now`
/// when the chain used it up, and never past the ceiling.
pub(crate) fn probe_deadline(
    started: Instant,
    window_end: Instant,
    now: Instant,
    floor: Duration,
) -> Instant {
    window_end.max(now + floor).min(started + PROBE_CEILING)
}

/// The repository whose brief is probed and the plugin host that offers the
/// probes (the user's own plus the repository's trusted ones).
pub(crate) struct ProbeSetup {
    pub(crate) host: Host,
    pub(crate) root: PathBuf,
    /// Characters the merged brief may hold: the hook parts the host takes,
    /// whatever the tier shrank the region pack to (a maybe's pack is one
    /// part, but a probe that answers it may fill all of them).
    pub(crate) cap: usize,
    /// What the probes are given at least after the chain.
    pub(crate) floor: Duration,
    /// Bytes the merged brief may hold when the host drops a longer one.
    pub(crate) max_bytes: Option<usize>,
}

impl ProbeSetup {
    /// The setup for a brief of `parts` hooks (or the `brief` command) in
    /// `root`, probed through `host`.
    pub(crate) fn new(
        host: Host,
        root: PathBuf,
        command: bool,
        parts: usize,
        chars: usize,
    ) -> Self {
        Self {
            host,
            root,
            cap: parts.max(1) * chars,
            floor: if command { Duration::ZERO } else { PROBE_FLOOR },
            max_bytes: (command && parts <= 1).then_some(COMMAND_MAX_BYTES),
        }
    }
}

/// What the brief hands the probe stage.
pub(crate) struct Stage<'a> {
    pub(crate) host: &'a Host,
    pub(crate) root: &'a Path,
    pub(crate) prompt: &'a str,
    pub(crate) kind: Option<&'a str>,
    pub(crate) gate_closed: bool,
    /// The deadline of the brief's window: no probe outlives it.
    pub(crate) deadline: Instant,
    /// Characters the merged brief may hold.
    pub(crate) cap: usize,
    /// Bytes it may hold when the host drops a longer one; a merge over it is
    /// not used at all.
    pub(crate) max_bytes: Option<usize>,
}

/// Whether the opt-out switches probes off.
pub fn enabled(value: Option<&str>) -> bool {
    !matches!(value, Some("0" | "false" | "off"))
}

/// `brief` after the probes of `stage`, and the run of each: `None` when no
/// probe added anything.
pub(crate) fn apply(stage: &Stage, brief: &str) -> (Option<String>, Vec<ProbeRun>) {
    let Ok(set) = pixel_plugin::probes(stage.host) else {
        return (None, Vec::new());
    };
    let due: Vec<&Probe> = set
        .probes
        .iter()
        .filter(|probe| probe.spec.applies(stage.kind, stage.prompt))
        .filter(|probe| !stage.gate_closed || probe.spec.when_gate_closed)
        .collect();
    if due.is_empty() {
        return (None, Vec::new());
    }
    let input = if brief.trim().is_empty() {
        format!("{BRIEF_TAG}\n")
    } else {
        brief.to_owned()
    };
    let request = json!({
        "prompt": stage.prompt,
        "repo_root": stage.root,
        "brief": input,
        "kind": stage.kind,
        "session_files": [],
    })
    .to_string();
    let exe = std::env::current_exe().unwrap_or_default();
    let results: Vec<(Option<String>, ProbeRun)> = std::thread::scope(|scope| {
        let handles: Vec<_> = due
            .iter()
            .map(|probe| scope.spawn(|| run_one(probe, &request, stage.root, &exe, stage.deadline)))
            .collect();
        handles
            .into_iter()
            .zip(&due)
            .map(|(handle, probe)| {
                handle.join().unwrap_or_else(|_| {
                    (
                        None,
                        ProbeRun {
                            name: probe.name.clone(),
                            ms: 0,
                            bytes: 0,
                            outcome: "failed",
                        },
                    )
                })
            })
            .collect()
    });
    let runs: Vec<ProbeRun> = results.iter().map(|(_, run)| run.clone()).collect();
    let outputs: Vec<(String, ProbeMode, String)> = results
        .into_iter()
        .zip(&due)
        .filter_map(|((stdout, _), probe)| {
            stdout.map(|out| (probe.name.clone(), probe.spec.mode, out))
        })
        .collect();
    if outputs.is_empty() {
        return (None, runs);
    }
    let merged = merge(&input, &outputs, stage.cap);
    if stage.max_bytes.is_some_and(|max| merged.len() > max) {
        return (None, runs);
    }
    (Some(merged), runs)
}

/// Run one probe inside `min(budget, what is left of the window)`.
fn run_one(
    probe: &Probe,
    request: &str,
    root: &Path,
    exe: &Path,
    deadline: Instant,
) -> (Option<String>, ProbeRun) {
    let started = Instant::now();
    let run = |outcome: &'static str, bytes: usize| ProbeRun {
        name: probe.name.clone(),
        ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        bytes,
        outcome,
    };
    let limit = (started + Duration::from_millis(probe.spec.budget_ms)).min(deadline);
    if started >= limit {
        return (None, run("skipped", 0));
    }
    let path = std::env::var_os("PATH")
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin".into());
    let mut command = Command::new(&probe.program);
    command
        .current_dir(root)
        .env_clear()
        .env("PATH", path)
        .env("LANG", "C.UTF-8")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("PIXEL_BIN", exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(home) = std::env::var_os("HOME") {
        command.env("HOME", home);
    }
    in_own_group(&mut command);
    let Ok(mut child) = command.spawn() else {
        return (None, run("failed", 0));
    };
    // The probe leads a group of its own, whose id is its pid: everything it
    // starts (a `sleep` its shell left behind, a helper it daemonised, a
    // `git` still running) is killed with it.
    let group = child.id();
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let body = request.to_owned();
    // Detached: a probe that never reads its input must not hold the brief.
    std::thread::spawn(move || {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(body.as_bytes());
        }
    });
    let (sender, receiver) = std::sync::mpsc::channel();
    // Detached on purpose: a grandchild that keeps the pipe open (a daemonised helper, a `sleep` the shell left
    // behind) must not hold the brief past the probe's budget.
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(stdout) = stdout {
            let _ = stdout.take(MAX_OUTPUT_BYTES).read_to_end(&mut bytes);
        }
        let _ = sender.send(bytes);
    });
    // The answer is complete when every process holding the probe's stdout
    // has closed it, or never when the budget runs out first. Either way the
    // whole group is killed then, before its leader is reaped, so that the
    // id still names the group and no process of the probe outlives its run.
    let read = receiver.recv_timeout(limit.saturating_duration_since(Instant::now()));
    kill_group(group);
    let _ = child.kill();
    let status = reap(child, limit.max(Instant::now()) + REAP_GRACE);
    let Ok(raw) = read else {
        return (None, run("timeout", 0));
    };
    if !status.is_some_and(|status| status.success()) {
        return (None, run("failed", 0));
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    if text.trim().is_empty() {
        return (None, run("empty", 0));
    }
    let bytes = text.len();
    (Some(text), run("ok", bytes))
}

/// How long past its limit a probe's leader, once killed, is waited on for
/// its status: a kill ends a process at once unless it sleeps in the kernel
/// (a hung network or FUSE read), and that one is reaped later off the
/// brief's path.
const REAP_GRACE: Duration = Duration::from_millis(50);

/// Starts the command in a new process group, its own pid the group's id.
fn in_own_group(command: &mut Command) {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    #[cfg(not(unix))]
    let _ = command;
}

/// Whether signalling a group would target init or PIXEL itself.
fn protected_group(pid: libc::pid_t, own: libc::pid_t) -> bool {
    pid <= 1 || pid == own
}

/// Sends SIGKILL only to a foreign process group; an absent group is harmless.
fn kill_group(group: u32) {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(group) else {
            return;
        };
        // SAFETY: `getpgrp` takes no argument and cannot fail.
        if protected_group(pid, unsafe { libc::getpgrp() }) {
            return;
        }
        // SAFETY: a negative pid signals the process group `pid`; the call
        // touches no memory of ours, and an absent group only sets ESRCH.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = group;
}

/// The exit status of `child`, waited for no later than `until`: a leader a
/// kill has not ended by then gives none, and a thread of its own reaps it
/// whenever it ends, so the brief never waits past its window.
fn reap(mut child: std::process::Child, until: Instant) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(1)),
            Ok(None) => break,
            Err(_) => return None,
        }
    }
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    None
}

/// A probe's output: its blocks, and the directives.
struct Output {
    blocks: Vec<Vec<String>>,
    drops: Vec<String>,
    firsts: Vec<String>,
}

fn parse_output(text: &str) -> Output {
    let mut out = Output {
        blocks: Vec::new(),
        drops: Vec::new(),
        firsts: Vec::new(),
    };
    for line in text.split('\n') {
        if line.starts_with("### ") {
            out.blocks.push(vec![line.to_owned()]);
        } else if let Some(rx) = line.strip_prefix("drop: ") {
            out.drops.push(rx.to_owned());
        } else if let Some(rx) = line.strip_prefix("first: ") {
            out.firsts.push(rx.to_owned());
        } else if let Some(block) = out.blocks.last_mut() {
            block.push(line.to_owned());
        }
    }
    for block in &mut out.blocks {
        while block.last().is_some_and(String::is_empty) {
            block.pop();
        }
    }
    out
}

/// `(head, blocks, footer)` of a brief.
fn split_brief(text: &str) -> (Vec<String>, Vec<Vec<String>>, Vec<String>) {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let mut footer: Vec<String> = Vec::new();
    while lines
        .last()
        .is_some_and(|line| FOOTER_PREFIXES.iter().any(|p| line.starts_with(p)))
    {
        footer.insert(0, lines.pop().unwrap_or_default().to_owned());
    }
    let (mut head, mut blocks): (Vec<String>, Vec<Vec<String>>) = (Vec::new(), Vec::new());
    for line in lines {
        if line.starts_with("### ") {
            blocks.push(vec![line.to_owned()]);
        } else if let Some(block) = blocks.last_mut() {
            block.push(line.to_owned());
        } else {
            head.push(line.to_owned());
        }
    }
    (head, blocks, footer)
}

fn header(block: &[String]) -> &str {
    block[0].get(4..).unwrap_or("")
}

/// Whether `pattern` matches somewhere in `text`; a pattern that is not a
/// regex matches nothing.
fn found(pattern: &str, text: &str) -> bool {
    Regex::new(pattern).is_ok_and(|re| re.is_match(text))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Who {
    Lead,
    Brief,
    Append,
}

/// The brief after its probes. `outputs` are `(name, mode, stdout)` in any
/// order (they are applied by name). Drops and firsts act on the brief's own
/// blocks only; a probe block whose header is already held is skipped; over
/// `cap` characters the append blocks go first, then the brief's last blocks,
/// then the lead blocks, whole, and the first block is never removed.
pub(crate) fn merge(brief: &str, outputs: &[(String, ProbeMode, String)], cap: usize) -> String {
    let (mut head, mut existing, footer) = split_brief(brief);
    if head.is_empty() {
        head.push(BRIEF_TAG.to_owned());
    }
    let mut sorted: Vec<&(String, ProbeMode, String)> = outputs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let (mut lead, mut append) = (Vec::new(), Vec::new());
    let (mut drops, mut firsts) = (Vec::new(), Vec::new());
    for (_, mode, stdout) in sorted {
        let out = parse_output(stdout);
        drops.extend(out.drops);
        firsts.extend(out.firsts);
        match mode {
            ProbeMode::Lead => lead.extend(out.blocks),
            ProbeMode::Append => append.extend(out.blocks),
        }
    }
    existing.retain(|block| !drops.iter().any(|rx| found(rx, header(block))));
    for rx in firsts.iter().rev() {
        let (moved, rest): (Vec<_>, Vec<_>) =
            existing.into_iter().partition(|b| found(rx, header(b)));
        existing = moved.into_iter().chain(rest).collect();
    }
    let mut held: std::collections::HashSet<String> =
        existing.iter().map(|b| header(b).to_owned()).collect();
    let mut fresh = |blocks: Vec<Vec<String>>| -> Vec<Vec<String>> {
        blocks
            .into_iter()
            .filter(|b| held.insert(header(b).to_owned()))
            .collect()
    };
    let lead = fresh(lead);
    let append = fresh(append);
    let mut order: Vec<(Who, Vec<String>)> = lead
        .into_iter()
        .map(|b| (Who::Lead, b))
        .chain(existing.into_iter().map(|b| (Who::Brief, b)))
        .chain(append.into_iter().map(|b| (Who::Append, b)))
        .collect();
    let render = |items: &[(Who, Vec<String>)]| -> String {
        let mut lines: Vec<&str> = head.iter().map(String::as_str).collect();
        lines.extend(items.iter().flat_map(|(_, b)| b.iter().map(String::as_str)));
        lines.extend(footer.iter().map(String::as_str));
        lines.join("\n") + "\n"
    };
    let chars = |text: &str| text.chars().count();
    for victim in [Who::Append, Who::Brief, Who::Lead] {
        while chars(&render(&order)) > cap && order.len() > 1 {
            let Some(index) = order.iter().rposition(|(who, _)| *who == victim) else {
                break;
            };
            order.remove(index);
        }
    }
    if chars(&render(&order)) > cap && !order.is_empty() {
        let last = order.len() - 1;
        while order[last].1.len() > 1 && chars(&render(&order)) > cap {
            order[last].1.pop();
        }
    }
    render(&order)
}

#[cfg(test)]
mod tests;
