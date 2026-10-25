// SPDX-FileCopyrightText: The Pixel contributors
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
//! A probe ported to Rust is a [`Step`] (`steps/`): it runs in this process
//! beside the scripts, under the same `when`, budget and merge, and takes the
//! place of the installed script of its name, which is not started. A step is
//! switched off like its script was: `PIXEL_BRIEF_PROBES=0`, or `pixel plugin
//! disable <name>`. A step of the post stage (`stage = "post"` in the manifest
//! of its probe, [`steps::POST`]) runs after the others, on the brief they
//! produced, and its output is merged in turn.
//!
//! [`merge`] is specified by `scripts/bench/probe-test.py` (`merge`) and pinned by
//! the golden cases of `tests/fixtures/probe-merge.json`, which both run.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pixel_plugin::{Config, Host, Place, Probe, ProbeMode};
use regex::Regex;
use serde_json::json;

use super::steps::{self, Ctx, Step};

/// Environment opt-out: `0`, `false` or `off` runs no probe.
pub(crate) const PROBES_ENV: &str = "PIXEL_BRIEF_PROBES";
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

/// What the log keeps of one probe run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProbeRun {
    pub(crate) name: String,
    pub(crate) ms: u64,
    pub(crate) bytes: usize,
    /// `ok`, `empty`, `timeout`, `failed` or `skipped`.
    pub(crate) outcome: &'static str,
}

impl ProbeRun {
    /// The run of `name` that began at `started` and ended now.
    fn since(name: &str, started: Instant, outcome: &'static str, bytes: usize) -> Self {
        Self {
            name: name.to_owned(),
            ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            bytes,
            outcome,
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
pub(crate) fn enabled(value: Option<&str>) -> bool {
    !matches!(value, Some("0" | "false" | "off"))
}

/// `brief` after the probes of `stage`, and the run of each: `None` when no
/// probe added anything.
pub(crate) fn apply(stage: &Stage, brief: &str) -> (Option<String>, Vec<ProbeRun>) {
    apply_staged(stage, brief, steps::ALL, steps::POST)
}

/// [`apply`] with `steps` as the registered ones and no step of the post stage.
#[cfg(test)]
fn apply_with(stage: &Stage, brief: &str, steps: &[&Step]) -> (Option<String>, Vec<ProbeRun>) {
    apply_staged(stage, brief, steps, &[])
}

/// [`apply`] with `steps` as the registered ones, and `post` as the steps that
/// run after them.
fn apply_staged<'a>(
    stage: &Stage,
    brief: &str,
    steps: &[&'a Step],
    post: &[&'a Step],
) -> (Option<String>, Vec<ProbeRun>) {
    let Ok(set) = pixel_plugin::probes(stage.host) else {
        return (None, Vec::new());
    };
    let disabled = Config::at(stage.host.config_file())
        .disabled()
        .unwrap_or_default();
    // A script is not started when a step takes its place; a repository's
    // own probe of the same name is another plugin and still runs.
    let due: Vec<&Probe> = set
        .probes
        .iter()
        .filter(|probe| probe.spec.applies(stage.kind, stage.prompt))
        .filter(|probe| !stage.gate_closed || probe.spec.when_gate_closed)
        .filter(|probe| {
            probe.place != Place::User
                || !steps.iter().chain(post).any(|step| step.name == probe.name)
        })
        .collect();
    let due_in = |registered: &[&'a Step]| -> Vec<&'a Step> {
        registered
            .iter()
            .copied()
            .filter(|step| step.applies(stage.kind, stage.prompt, stage.gate_closed))
            .filter(|step| !disabled.iter().any(|name| name == step.name))
            .collect()
    };
    let due_steps = due_in(steps);
    // A post step runs only where its probe is installed: `pixel setup` never
    // installed `prune-noise`, so a brief that did not run it before still does not.
    let due_post: Vec<&Step> = due_in(post)
        .into_iter()
        .filter(|step| {
            set.probes
                .iter()
                .any(|probe| probe.place == Place::User && probe.name == step.name)
        })
        .collect();
    if due.is_empty() && due_steps.is_empty() && due_post.is_empty() {
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
    let results: Vec<(String, ProbeMode, (Option<String>, ProbeRun))> =
        std::thread::scope(|scope| {
            let scripts: Vec<_> = due
                .iter()
                .map(|probe| {
                    scope.spawn(|| run_one(probe, &request, stage.root, &exe, stage.deadline))
                })
                .collect();
            let in_process: Vec<_> = due_steps
                .iter()
                .map(|step| {
                    scope.spawn(|| {
                        run_step(
                            step,
                            stage.root,
                            stage.prompt,
                            stage.kind,
                            &input,
                            stage.deadline,
                        )
                    })
                })
                .collect();
            let failed = |name: &str| (None, ProbeRun::since(name, Instant::now(), "failed", 0));
            let scripts = scripts.into_iter().zip(&due).map(|(handle, probe)| {
                let done = handle.join().unwrap_or_else(|_| failed(&probe.name));
                (probe.name.clone(), probe.spec.mode, done)
            });
            let in_process = in_process
                .into_iter()
                .zip(&due_steps)
                .map(|(handle, step)| {
                    let done = handle.join().unwrap_or_else(|_| failed(step.name));
                    (step.name.to_owned(), step.mode, done)
                });
            scripts.chain(in_process).collect()
        });
    let mut runs: Vec<ProbeRun> = results.iter().map(|(_, _, (_, run))| run.clone()).collect();
    let outputs: Vec<(String, ProbeMode, String)> = results
        .into_iter()
        .filter_map(|(name, mode, (stdout, _))| stdout.map(|out| (name, mode, out)))
        .collect();
    let mut merged = (!outputs.is_empty()).then(|| merge(&input, &outputs, stage.cap));
    // The post stage: each step reads what the stages before it made.
    for step in due_post {
        let current = merged.as_deref().unwrap_or(&input);
        let (out, run) = run_step(
            step,
            stage.root,
            stage.prompt,
            stage.kind,
            current,
            stage.deadline,
        );
        runs.push(run);
        if let Some(out) = out {
            let outputs = [(step.name.to_owned(), step.mode, out)];
            merged = Some(merge(current, &outputs, stage.cap));
        }
    }
    let Some(merged) = merged else {
        return (None, runs);
    };
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
    let run =
        |outcome: &'static str, bytes: usize| ProbeRun::since(&probe.name, started, outcome, bytes);
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

/// Run one step inside `min(budget, what is left of the window)`. It shares
/// the process, so it cannot be killed at the limit: it stops itself there
/// ([`Ctx::timed_out`]), and an answer that comes later is dropped.
fn run_step(
    step: &Step,
    root: &Path,
    prompt: &str,
    kind: Option<&str>,
    brief: &str,
    deadline: Instant,
) -> (Option<String>, ProbeRun) {
    let started = Instant::now();
    let run =
        |outcome: &'static str, bytes: usize| ProbeRun::since(step.name, started, outcome, bytes);
    let limit = (started + Duration::from_millis(step.budget_ms)).min(deadline);
    if started >= limit {
        return (None, run("skipped", 0));
    }
    let text = (step.run)(&Ctx::new(root, prompt, brief, limit).with_kind(kind));
    if Instant::now() > limit {
        return (None, run("timeout", 0));
    }
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

/// Whether signalling a group would target init or Pixel itself.
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
mod tests {
    use super::*;
    use serde_json::Value;

    /// [`super::apply`] without the registered steps: these tests are about
    /// scripts and the merge, and a step that applies to every brief
    /// (`prune-noise`) would be a run of theirs.
    fn apply(stage: &Stage, brief: &str) -> (Option<String>, Vec<ProbeRun>) {
        apply_with(stage, brief, &[])
    }

    fn mode(text: &str) -> ProbeMode {
        if text == "lead" {
            ProbeMode::Lead
        } else {
            ProbeMode::Append
        }
    }

    #[test]
    fn merge_matches_every_golden_case_of_the_python_specification() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../tests/fixtures/probe-merge.json")).unwrap();
        assert!(cases.len() >= 10);
        for case in cases {
            let outputs: Vec<(String, ProbeMode, String)> = case["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| {
                    (
                        o["name"].as_str().unwrap().to_owned(),
                        mode(o["mode"].as_str().unwrap()),
                        o["stdout"].as_str().unwrap().to_owned(),
                    )
                })
                .collect();
            let got = merge(
                case["brief"].as_str().unwrap(),
                &outputs,
                usize::try_from(case["cap"].as_u64().unwrap()).unwrap(),
            );
            assert_eq!(got, case["expected"].as_str().unwrap(), "{}", case["name"]);
        }
    }

    #[test]
    fn the_opt_out_is_only_an_explicit_off_word() {
        assert!(enabled(None) && enabled(Some("1")) && enabled(Some("")));
        assert!(!enabled(Some("0")) && !enabled(Some("off")) && !enabled(Some("false")));
    }

    fn fixture_probe(home: &Path, name: &str, body: &str, extra: &str) {
        use std::os::unix::fs::PermissionsExt;
        let dir = home.join(".pixel/plugins").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pixel-plugin.toml"),
            format!(
                "name = \"{name}\"\nversion = \"0.1.0\"\napi = 1\ndescription = \"d\"\nrun = \"probe\"\ncapabilities = [\"brief-probe\"]\n\n[brief-probe]\nowner = \"o\"\nbudget_ms = 400\n{extra}"
            ),
        )
        .unwrap();
        let run = dir.join("probe");
        std::fs::write(&run, body).unwrap();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn stage<'a>(host: &'a Host, root: &'a Path, kind: Option<&'a str>, ms: u64) -> Stage<'a> {
        Stage {
            host,
            root,
            prompt: "where is it",
            kind,
            gate_closed: false,
            deadline: Instant::now() + Duration::from_millis(ms),
            cap: 3 * 9_800,
            max_bytes: None,
        }
    }

    /// A home where the probes `names` are installed: a post step runs only there.
    fn installed(dir: &Path, names: &[&str]) -> Host {
        let home = dir.join("home");
        for name in names {
            fixture_probe(&home, name, SCRIPT, "");
        }
        host(&home)
    }

    fn host(home: &Path) -> Host {
        Host {
            home: home.to_path_buf(),
            repo_root: None,
            path: std::ffi::OsString::new(),
            github_base: None,
        }
    }

    #[test]
    fn probes_run_in_parallel_inside_their_budget_and_a_broken_one_costs_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fixture_probe(
            &home,
            "a-add",
            "#!/bin/sh\ncat >/dev/null\nprintf '### a.rs:1-1 — a\\n1| a\\n'\n",
            "",
        );
        fixture_probe(
            &home,
            "b-slow",
            "#!/bin/sh\nsleep 3\nprintf '### b.rs:1-1 — b\\n'\n",
            "",
        );
        fixture_probe(&home, "c-fail", "#!/bin/sh\nexit 3\n", "");
        fixture_probe(&home, "d-quiet", "#!/bin/sh\ncat >/dev/null\n", "");
        fixture_probe(
            &home,
            "e-skipped-kind",
            "#!/bin/sh\nprintf '### e — e\\n'\n",
            "skip_kinds = [\"flow\"]\n",
        );
        let (h, root) = (host(&home), tmp.path().join("repo"));
        std::fs::create_dir_all(&root).unwrap();
        let started = Instant::now();
        let (merged, runs) = apply(
            &stage(&h, &root, Some("flow"), 2_000),
            "[PIXEL:BRIEF]\n### z.rs:1-1 — z\n1| z\n",
        );
        assert!(
            started.elapsed() < Duration::from_millis(1_800),
            "{:?}",
            started.elapsed()
        );
        let outcome = |name: &str| runs.iter().find(|r| r.name == name).map(|r| r.outcome);
        assert_eq!(outcome("a-add"), Some("ok"));
        assert_eq!(outcome("b-slow"), Some("timeout"));
        assert_eq!(outcome("c-fail"), Some("failed"));
        assert_eq!(outcome("d-quiet"), Some("empty"));
        assert_eq!(
            outcome("e-skipped-kind"),
            None,
            "a skipped kind is not even run"
        );
        assert_eq!(
            runs.iter().find(|r| r.name == "a-add").unwrap().bytes,
            "### a.rs:1-1 — a\n1| a\n".len()
        );
        assert_eq!(
            merged.unwrap(),
            "[PIXEL:BRIEF]\n### z.rs:1-1 — z\n1| z\n### a.rs:1-1 — a\n1| a\n"
        );
    }

    /// Whether the process `pid` is gone, or a zombie its new parent has not
    /// reaped yet: either way, nothing of it runs.
    fn gone(pid: i32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim().starts_with('Z'))
    }

    /// A probe is a script, and what it starts is the probe too: a sleeper
    /// it forks dies with it whether the probe answered, hung, or exited and
    /// left the sleeper holding its stdout, and none of them holds the brief
    /// past the probe's budget.
    #[test]
    fn a_probe_and_every_process_it_started_are_killed_and_never_hold_the_brief() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let pids = tmp.path().join("pids");
        std::fs::create_dir_all(&pids).unwrap();
        // `<redirect>` the sleeper's stdout, record its pid, wait until it has.
        let fork = |name: &str, redirect: &str| {
            let file = pids.join(name);
            let file = file.display();
            format!(
                "sh -c 'echo $$ > {file}; exec sleep 30' {redirect} &\nwhile [ ! -s {file} ]; do sleep 0.01; done\n"
            )
        };
        // Answers, its sleeper's stdout elsewhere: complete when it exits.
        fixture_probe(
            &home,
            "a-answers",
            &format!(
                "#!/bin/sh\ncat >/dev/null\n{}printf '### a.rs:1-1 — a\\n1| a\\n'\n",
                fork("a", ">/dev/null 2>&1")
            ),
            "",
        );
        // Hangs, its sleeper holding its stdout: killed at the budget.
        fixture_probe(
            &home,
            "b-hangs",
            &format!("#!/bin/sh\ncat >/dev/null\n{}sleep 30\n", fork("b", "")),
            "",
        );
        // Exits at once, its sleeper holding its stdout: nothing is complete
        // before the budget.
        fixture_probe(
            &home,
            "c-exits",
            &format!(
                "#!/bin/sh\ncat >/dev/null\n{}printf '### c — c\\n'\n",
                fork("c", "")
            ),
            "",
        );
        let (h, root) = (host(&home), tmp.path().join("repo"));
        std::fs::create_dir_all(&root).unwrap();
        let started = Instant::now();
        let (merged, runs) = apply(&stage(&h, &root, None, 2_000), "[PIXEL:BRIEF]\n");
        let took = started.elapsed();
        // Each budget is 400 ms (`fixture_probe`); the sleepers sleep 30 s.
        assert!(
            took < Duration::from_millis(1_000),
            "the brief waited {took:?}"
        );
        let outcome = |name: &str| runs.iter().find(|r| r.name == name).map(|r| r.outcome);
        assert_eq!(outcome("a-answers"), Some("ok"));
        assert_eq!(outcome("b-hangs"), Some("timeout"));
        assert_eq!(outcome("c-exits"), Some("timeout"));
        assert_eq!(
            merged.as_deref(),
            Some("[PIXEL:BRIEF]\n### a.rs:1-1 — a\n1| a\n")
        );
        for name in ["a", "b", "c"] {
            let pid: i32 = std::fs::read_to_string(pids.join(name))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let until = Instant::now() + Duration::from_secs(5);
            while !gone(pid) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            let alive = !gone(pid);
            if alive {
                // SAFETY: the pid is the sleeper this test started.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            assert!(!alive, "probe {name}'s sleeper outlived its run");
        }
    }

    #[test]
    fn a_merge_over_the_hosts_byte_limit_is_not_used() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fixture_probe(
            &home,
            "wide",
            "#!/bin/sh\ncat >/dev/null\nprintf \'### w.rs:1-1 — w\\n1| \'; head -c 600 /dev/zero | tr \'\\0\' \'é\'; echo\n",
            "",
        );
        let (h, root) = (host(&home), tmp.path().to_path_buf());
        let mut s = stage(&h, &root, None, 2_000);
        s.max_bytes = Some(1_000);
        let (merged, runs) = apply(&s, "[PIXEL:BRIEF]\n");
        assert!(merged.is_none(), "{merged:?}");
        assert_eq!(
            runs[0].outcome, "ok",
            "it ran; only its output was not used"
        );
        s.max_bytes = Some(2_000);
        assert!(apply(&s, "[PIXEL:BRIEF]\n").0.is_some());
    }

    #[test]
    fn nothing_runs_once_the_window_is_over_and_nothing_added_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fixture_probe(&home, "a", "#!/bin/sh\nprintf '### a — a\\n'\n", "");
        let (h, root) = (host(&home), tmp.path().to_path_buf());
        let (merged, runs) = apply(&stage(&h, &root, None, 0), "[PIXEL:BRIEF]\n");
        assert!(merged.is_none());
        assert_eq!(
            runs.iter().map(|r| r.outcome).collect::<Vec<_>>(),
            ["skipped"]
        );
        let empty = host(&tmp.path().join("nowhere"));
        assert_eq!(
            apply(&stage(&empty, &root, None, 500), "x"),
            (None, Vec::new())
        );
    }

    #[test]
    fn a_probe_gets_the_prompt_the_repo_the_kind_and_the_brief_as_json_and_an_empty_brief_the_tag()
    {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fixture_probe(
            &home,
            "echo",
            "#!/bin/sh\npython3 -c 'import json,sys; r=json.load(sys.stdin); print(\"### \"+r[\"kind\"]+\" — \"+r[\"prompt\"]); print(r[\"brief\"].strip()); print(r[\"repo_root\"])'\n",
            "",
        );
        let (h, root) = (host(&home), tmp.path().join("repo"));
        std::fs::create_dir_all(&root).unwrap();
        let (merged, _) = apply(&stage(&h, &root, Some("history"), 2_000), "");
        let text = merged.unwrap();
        assert!(
            text.starts_with("[PIXEL:BRIEF]\n### history — where is it\n[PIXEL:BRIEF]\n"),
            "{text}"
        );
        assert!(text.contains(root.to_str().unwrap()), "{text}");
    }

    #[test]
    fn a_closed_gate_should_run_only_matching_opted_in_probes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let body = "#!/bin/sh\ncat >/dev/null\nprintf '### recovery.rs:1-1 — evidence\\n1| const VALUE: u8 = 7;\\n'\n";
        fixture_probe(&home, "ordinary", body, "");
        fixture_probe(
            &home,
            "recovery",
            body,
            "when_gate_closed = true\nwhen_prompt = \"where\"\n",
        );
        fixture_probe(
            &home,
            "unrelated",
            body,
            "when_gate_closed = true\nwhen_prompt = \"unrelated\"\n",
        );
        fixture_probe(
            &home,
            "wrong-kind",
            body,
            "when_gate_closed = true\nwhen_prompt = \"where\"\nskip_kinds = [\"flow\"]\n",
        );
        let h = host(&home);
        let mut s = stage(&h, tmp.path(), Some("flow"), 1_000);
        s.gate_closed = true;
        let (merged, runs) = apply(&s, "");
        assert_eq!(
            runs.iter()
                .map(|run| (run.name.as_str(), run.outcome))
                .collect::<Vec<_>>(),
            [("recovery", "ok")]
        );
        assert_eq!(
            merged.as_deref(),
            Some("[PIXEL:BRIEF]\n### recovery.rs:1-1 — evidence\n1| const VALUE: u8 = 7;\n")
        );
        s.gate_closed = false;
        assert_eq!(
            apply(&s, "")
                .1
                .iter()
                .map(|run| run.name.as_str())
                .collect::<Vec<_>>(),
            ["ordinary", "recovery"]
        );
    }

    #[test]
    fn audited_group_signals_should_protect_init_and_our_group_independently() {
        for (pid, own, protected) in [
            (0, 100, true),
            (1, 100, true),
            (100, 100, true),
            (101, 100, false),
        ] {
            assert_eq!(protected_group(pid, own), protected);
        }
    }

    #[test]
    fn audited_reaping_should_return_none_after_the_deadline_even_while_the_leader_is_alive() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 0.2"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        assert!(reap(child, Instant::now() - Duration::from_secs(1)).is_none());
        let mut output = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut output).unwrap();
        assert!(output.is_empty());
    }

    /// A step that shows what it was given, and the script it replaces.
    const DEMO: Step = Step {
        name: "demo",
        when_kinds: &["tests"],
        skip_kinds: &[],
        when_prompt: None,
        when_gate_closed: false,
        budget_ms: 2_000,
        mode: ProbeMode::Append,
        run: |ctx| {
            let first = ctx.brief.lines().next().unwrap_or_default();
            format!("### demo.rs:1-1 — {} | {first}\n", ctx.prompt)
        },
    };
    const SCRIPT: &str =
        "#!/bin/sh\ncat >/dev/null\nprintf '### script.rs:1-1 — the script ran\\n'\n";

    fn outcomes(runs: &[ProbeRun]) -> Vec<(&str, &str)> {
        runs.iter()
            .map(|run| (run.name.as_str(), run.outcome))
            .collect()
    }

    #[test]
    fn a_step_takes_the_place_of_the_installed_script_of_its_name() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fixture_probe(&home, "demo", SCRIPT, "");
        fixture_probe(&home, "other", SCRIPT, "");
        let h = host(&home);
        let s = stage(&h, tmp.path(), Some("tests"), 5_000);
        let (merged, runs) = apply_with(&s, "", &[&DEMO]);
        assert_eq!(
            outcomes(&runs),
            [("other", "ok"), ("demo", "ok")],
            "the script of another name runs beside the step, the script of its own does not"
        );
        assert_eq!(
            merged.unwrap(),
            "[PIXEL:BRIEF]\n### demo.rs:1-1 — where is it | [PIXEL:BRIEF]\n### script.rs:1-1 — the script ran\n"
        );
    }

    #[test]
    fn a_step_runs_with_no_script_installed_and_is_given_the_prompt_and_the_brief() {
        let tmp = tempfile::tempdir().unwrap();
        let h = host(&tmp.path().join("empty-home"));
        let s = stage(&h, tmp.path(), Some("tests"), 5_000);
        let (merged, runs) = apply_with(&s, "[PIXEL:BRIEF]\n### a.rs:1-1 — a\n", &[&DEMO]);
        assert_eq!(outcomes(&runs), [("demo", "ok")]);
        assert_eq!(
            merged.unwrap(),
            "[PIXEL:BRIEF]\n### a.rs:1-1 — a\n### demo.rs:1-1 — where is it | [PIXEL:BRIEF]\n"
        );
    }

    #[test]
    fn a_step_is_switched_off_like_its_script_was() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fixture_probe(&home, "demo", SCRIPT, "");
        let h = host(&home);
        let s = stage(&h, tmp.path(), Some("tests"), 5_000);
        pixel_plugin::disable(&h, "demo").unwrap();
        assert_eq!(apply_with(&s, "", &[&DEMO]), (None, Vec::new()));
        pixel_plugin::enable(&h, "demo").unwrap();
        let (_, runs) = apply_with(&s, "", &[&DEMO]);
        assert_eq!(outcomes(&runs), [("demo", "ok")]);
    }

    #[test]
    fn a_step_follows_its_when_and_the_closed_gate_like_a_script() {
        let tmp = tempfile::tempdir().unwrap();
        let h = host(&tmp.path().join("empty-home"));
        let none = (None, Vec::new());
        let flow = stage(&h, tmp.path(), Some("flow"), 5_000);
        assert_eq!(
            apply_with(&flow, "", &[&DEMO]),
            none,
            "a kind it does not name"
        );
        let mut closed = stage(&h, tmp.path(), Some("tests"), 5_000);
        closed.gate_closed = true;
        assert_eq!(
            apply_with(&closed, "", &[&DEMO]),
            none,
            "a closed gate runs only opted-in steps"
        );
        let opted_in = Step {
            name: "demo-closed",
            when_kinds: &[],
            when_prompt: Some("where"),
            when_gate_closed: true,
            ..DEMO
        };
        let (merged, runs) = apply_with(&closed, "", &[&DEMO, &opted_in]);
        assert_eq!(outcomes(&runs), [("demo-closed", "ok")]);
        assert!(merged.is_some());
    }

    #[test]
    fn a_probe_of_the_repository_with_the_name_of_a_step_is_another_plugin_and_still_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, repo) = (tmp.path().join("home"), tmp.path().join("repo"));
        fixture_probe(&repo, "demo", SCRIPT, "");
        let h = Host {
            repo_root: Some(repo.clone()),
            ..host(&home)
        };
        pixel_plugin::trust(&h, "demo").unwrap();
        let s = stage(&h, &repo, Some("tests"), 5_000);
        let (_, runs) = apply_with(&s, "", &[&DEMO]);
        assert_eq!(outcomes(&runs), [("demo", "ok"), ("demo", "ok")]);
    }

    fn step_that(run: fn(&Ctx) -> String) -> Step {
        Step {
            name: "x",
            when_kinds: &[],
            skip_kinds: &[],
            when_prompt: None,
            when_gate_closed: false,
            budget_ms: 40,
            mode: ProbeMode::Append,
            run,
        }
    }

    #[test]
    fn a_step_is_run_inside_its_budget_and_an_answer_after_it_is_dropped() {
        let root = Path::new(".");
        let far = Instant::now() + Duration::from_secs(60);
        let said = step_that(|_| "### a — a\n".to_owned());
        let (text, run) = run_step(&said, root, "p", None, "b", far);
        assert_eq!(text.as_deref(), Some("### a — a\n"));
        assert_eq!((run.name.as_str(), run.outcome, run.bytes), ("x", "ok", 12));
        let quiet = step_that(|_| " \n".to_owned());
        assert_eq!(
            run_step(&quiet, root, "p", None, "b", far).1.outcome,
            "empty"
        );
        let late = step_that(|_| {
            std::thread::sleep(Duration::from_millis(120));
            "### late — late\n".to_owned()
        });
        let (text, run) = run_step(&late, root, "p", None, "b", far);
        assert_eq!((text, run.outcome), (None, "timeout"));
        let (text, run) = run_step(&said, root, "p", None, "b", Instant::now());
        assert_eq!((text, run.outcome), (None, "skipped"));
    }

    #[test]
    fn a_step_is_told_the_question_kind_the_brief_settled_on() {
        let root = Path::new(".");
        let far = Instant::now() + Duration::from_secs(60);
        let echo = step_that(|ctx| format!("### {}\n", ctx.kind.unwrap_or("none")));
        let said = |kind| run_step(&echo, root, "p", kind, "b", far).0;
        assert_eq!(said(Some("config")).as_deref(), Some("### config\n"));
        assert_eq!(said(None).as_deref(), Some("### none\n"));
    }

    #[test]
    fn the_registered_test_bodies_step_runs_and_the_installed_script_of_its_name_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fixture_probe(&home, "test-bodies", SCRIPT, "when_kinds = [\"tests\"]\n");
        let repo = super::super::steps::testing::repo(&[(
            "tests/test_client.py",
            "def test_follows_a_redirect_and_handles_it():\n    assert redirect() == 1\n\n\ndef test_other():\n    pass\n",
        )]);
        let h = host(&home);
        let mut s = stage(&h, repo.path(), Some("tests"), 5_000);
        s.prompt = "where are the tests for redirect handling";
        let (merged, runs) = super::apply(&s, "");
        // `rank-blocks` and `prune-noise` apply to every brief and run beside it.
        let ran: Vec<_> = outcomes(&runs)
            .into_iter()
            .filter(|(name, _)| *name == "test-bodies")
            .collect();
        assert_eq!(ran, [("test-bodies", "ok")]);
        assert_eq!(
            merged.unwrap(),
            "[PIXEL:BRIEF]\n### tests/test_client.py:1-2 — test test_follows_a_redirect_and_handles_it\n1| def test_follows_a_redirect_and_handles_it():\n2|     assert redirect() == 1\n"
        );
    }

    /// A step of the post stage: it reads the brief the others made, and
    /// replaces it.
    const POSTER: Step = Step {
        name: "poster",
        when_kinds: &[],
        skip_kinds: &[],
        when_prompt: None,
        when_gate_closed: false,
        budget_ms: 2_000,
        mode: ProbeMode::Lead,
        run: |ctx| {
            let blocks = ctx
                .brief
                .lines()
                .filter(|line| line.starts_with("### "))
                .count();
            format!(
                "drop: .\n### post.rs:1-1 — {blocks} blocks, kind {:?}\n",
                ctx.kind
            )
        },
    };

    #[test]
    fn a_step_is_given_the_kind_of_the_question() {
        let root = Path::new(".");
        let far = Instant::now() + Duration::from_secs(60);
        let kind = step_that(|ctx| format!("### k.rs:1-1 — {:?}\n", ctx.kind));
        let (text, _) = run_step(&kind, root, "p", Some("flow"), "b", far);
        assert_eq!(text.as_deref(), Some("### k.rs:1-1 — Some(\"flow\")\n"));
        let (text, _) = run_step(&kind, root, "p", None, "b", far);
        assert_eq!(text.as_deref(), Some("### k.rs:1-1 — None\n"));
    }

    #[test]
    fn a_post_step_runs_after_the_others_on_the_brief_they_made() {
        let tmp = tempfile::tempdir().unwrap();
        let h = installed(tmp.path(), &["poster"]);
        let s = stage(&h, tmp.path(), Some("tests"), 5_000);
        let (merged, runs) = apply_staged(
            &s,
            "[PIXEL:BRIEF]\n### a.rs:1-1 — a\n",
            &[&DEMO],
            &[&POSTER],
        );
        assert_eq!(outcomes(&runs), [("demo", "ok"), ("poster", "ok")]);
        assert_eq!(
            merged.unwrap(),
            "[PIXEL:BRIEF]\n### post.rs:1-1 — 2 blocks, kind Some(\"tests\")\n",
            "it saw the brief and the block of the step before it, and knew the kind"
        );
    }

    #[test]
    fn a_post_step_runs_on_the_brief_alone_when_no_other_probe_added_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let h = installed(tmp.path(), &["poster"]);
        let s = stage(&h, tmp.path(), Some("tests"), 5_000);
        let (merged, runs) = apply_staged(&s, "[PIXEL:BRIEF]\n### a.rs:1-1 — a\n", &[], &[&POSTER]);
        assert_eq!(outcomes(&runs), [("poster", "ok")]);
        assert_eq!(
            merged.unwrap(),
            "[PIXEL:BRIEF]\n### post.rs:1-1 — 1 blocks, kind Some(\"tests\")\n"
        );
        let (merged, _) = apply_staged(&s, "", &[], &[&POSTER]);
        assert_eq!(
            merged.unwrap(),
            "[PIXEL:BRIEF]\n### post.rs:1-1 — 0 blocks, kind Some(\"tests\")\n",
            "an empty brief is given as its tag"
        );
    }

    #[test]
    fn a_post_step_with_nothing_to_say_leaves_what_the_others_made() {
        let tmp = tempfile::tempdir().unwrap();
        let h = installed(tmp.path(), &["quiet"]);
        let s = stage(&h, tmp.path(), Some("tests"), 5_000);
        let quiet = Step {
            name: "quiet",
            run: |_| String::new(),
            ..POSTER
        };
        let brief = "[PIXEL:BRIEF]\n### a.rs:1-1 — a\n";
        let (merged, runs) = apply_staged(&s, brief, &[], &[&quiet]);
        assert_eq!((merged, outcomes(&runs)), (None, vec![("quiet", "empty")]));
        let (merged, runs) = apply_staged(&s, brief, &[&DEMO], &[&quiet]);
        assert_eq!(outcomes(&runs), [("demo", "ok"), ("quiet", "empty")]);
        assert_eq!(
            merged.unwrap(),
            "[PIXEL:BRIEF]\n### a.rs:1-1 — a\n### demo.rs:1-1 — where is it | [PIXEL:BRIEF]\n"
        );
    }

    #[test]
    fn a_post_step_takes_the_place_of_its_script_and_is_switched_off_like_one() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fixture_probe(&home, "poster", SCRIPT, "");
        let h = host(&home);
        let s = stage(&h, tmp.path(), Some("tests"), 5_000);
        let (_, runs) = apply_staged(&s, "[PIXEL:BRIEF]\n", &[], &[&POSTER]);
        assert_eq!(
            outcomes(&runs),
            [("poster", "ok")],
            "the script is not started"
        );
        pixel_plugin::disable(&h, "poster").unwrap();
        assert_eq!(
            apply_staged(&s, "[PIXEL:BRIEF]\n", &[], &[&POSTER]),
            (None, Vec::new())
        );
    }

    #[test]
    fn a_post_step_follows_its_when_and_the_closed_gate_and_the_byte_limit_applies_to_its_result() {
        let tmp = tempfile::tempdir().unwrap();
        let h = installed(tmp.path(), &["poster"]);
        let brief = "[PIXEL:BRIEF]\n### a.rs:1-1 — a\n";
        let flow_only = Step {
            when_kinds: &["flow"],
            ..POSTER
        };
        let tests = stage(&h, tmp.path(), Some("tests"), 5_000);
        assert_eq!(
            apply_staged(&tests, brief, &[], &[&flow_only]),
            (None, Vec::new())
        );
        let mut closed = stage(&h, tmp.path(), Some("tests"), 5_000);
        closed.gate_closed = true;
        assert_eq!(
            apply_staged(&closed, brief, &[], &[&POSTER]),
            (None, Vec::new())
        );
        let mut small = stage(&h, tmp.path(), Some("tests"), 5_000);
        small.max_bytes = Some(10);
        let (merged, runs) = apply_staged(&small, brief, &[], &[&POSTER]);
        assert_eq!((merged, outcomes(&runs)), (None, vec![("poster", "ok")]));
    }

    #[test]
    fn a_post_step_whose_probe_is_not_installed_never_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let h = host(&tmp.path().join("empty-home"));
        let s = stage(&h, tmp.path(), Some("tests"), 5_000);
        let brief = "[PIXEL:BRIEF]\n### a.rs:1-1 — a\n";
        assert_eq!(apply_staged(&s, brief, &[], &[&POSTER]), (None, Vec::new()));
        let (_, runs) = apply_staged(&s, brief, &[&DEMO], &[&POSTER]);
        assert_eq!(outcomes(&runs), [("demo", "ok")]);
    }
}
