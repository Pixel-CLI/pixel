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
//! [`merge`] is specified by `scripts/probe-test.py` (`merge`) and pinned by
//! the golden cases of `tests/fixtures/probe-merge.json`, which both run.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pixel_plugin::{Host, Probe, ProbeMode};
use regex::Regex;
use serde_json::json;

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

/// What the brief hands the probe stage.
pub(crate) struct Stage<'a> {
    pub(crate) host: &'a Host,
    pub(crate) root: &'a Path,
    pub(crate) prompt: &'a str,
    pub(crate) kind: Option<&'a str>,
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
    let Ok(set) = pixel_plugin::probes(stage.host) else {
        return (None, Vec::new());
    };
    let due: Vec<&Probe> = set
        .probes
        .iter()
        .filter(|probe| probe.spec.applies(stage.kind, stage.prompt))
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
    let Ok(mut child) = command.spawn() else {
        return (None, run("failed", 0));
    };
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
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < limit => std::thread::sleep(Duration::from_millis(4)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(_) => break None,
        }
    };
    let Some(status) = status else {
        return (None, run("timeout", 0));
    };
    let Ok(raw) = receiver.recv_timeout(
        limit
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(20)),
    ) else {
        return (None, run("timeout", 0));
    };
    let text = String::from_utf8_lossy(&raw).into_owned();
    if !status.success() {
        return (None, run("failed", 0));
    }
    if text.trim().is_empty() {
        return (None, run("empty", 0));
    }
    let bytes = text.len();
    (Some(text), run("ok", bytes))
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
            deadline: Instant::now() + Duration::from_millis(ms),
            cap: 3 * 9_800,
            max_bytes: None,
        }
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
}
