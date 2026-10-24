// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

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
fn merge_matches_every_golden_case_of_the_fixture() {
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
fn a_probe_gets_the_prompt_the_repo_the_kind_and_the_brief_as_json_and_an_empty_brief_the_tag() {
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
