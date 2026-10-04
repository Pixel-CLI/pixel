// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel ai-cli-readify` at the command line: the contract a caller sees.
//!
//! The command's job is to say what is true, so the tests here are about
//! what it refuses to claim as much as what it reports. Every run in this
//! file is pointed at an empty HOME with no provider key, which is the one
//! state that needs no network: one classified failure and four agents that
//! could not reach a model. A test that reached a real provider would be a
//! test about the machine, not about the command.

use std::path::Path;

use crate::support::{Scratch, neutral_home, pixel_command};

/// Run the command against `home`, every provider key removed and a PATH that
/// reaches no agent binary, so nothing it does depends on the machine it runs
/// on.
///
/// Every run in this file goes through here — including the `--approve`
/// tests, which used to build their own command and so kept `OLLAMA_API_KEY`:
/// on a developer machine that exports one, the run sent a real completion to
/// `https://ollama.com` and the result depended on the account's quota.
fn readify_in(home: &Path, args: &[&str]) -> std::process::Output {
    let mut command = pixel_command();
    command.args(args);
    command.env("HOME", home).env("PATH", empty_path());
    for key in [
        "OLLAMA_API_KEY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_CUSTOM_HEADERS",
        "ANTHROPIC_BASE_URL",
    ] {
        command.env_remove(key);
    }
    command.output().expect("pixel ai-cli-readify runs")
}

/// [`readify_in`] against the shared neutral HOME: right for a run that reads
/// no file back, and the only HOME that can be shared, because it is created
/// once per process.
fn readify(args: &[&str]) -> std::process::Output {
    readify_in(neutral_home(), args)
}

/// The JSON report as a value, asserting the run produced one at all.
fn report_in(home: &Path, args: &[&str]) -> serde_json::Value {
    let out = readify_in(home, args);
    assert!(
        out.status.success(),
        "a run that establishes nothing is still a successful run: {out:?}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {stdout}"))
}

fn report(args: &[&str]) -> serde_json::Value {
    report_in(neutral_home(), args)
}

#[test]
fn the_json_report_names_the_provider() {
    let report = report(&["ai-cli-readify", "--json", "--timeout", "1"]);
    let providers: Vec<&str> = report["providers"]
        .as_array()
        .expect("a providers array")
        .iter()
        .map(|row| row["provider"].as_str().unwrap())
        .collect();
    assert_eq!(providers, ["ollama"]);
}

#[test]
fn a_provider_with_no_key_is_reported_as_a_missing_key() {
    let report = report(&["ai-cli-readify", "--json", "--timeout", "1"]);
    for row in report["providers"].as_array().unwrap() {
        assert_eq!(row["ready"], false, "{row}");
        assert_eq!(
            row["failure"], "no key",
            "an absent key is its own condition, not an unreachable provider: {row}"
        );
    }
}

#[test]
fn no_provider_answering_selects_none_and_rewrites_nothing() {
    let report = report(&["ai-cli-readify", "--json", "--timeout", "1"]);
    assert_eq!(report["selected"], serde_json::Value::Null);
    assert!(
        report["applied"].as_array().unwrap().is_empty(),
        "nothing may be rewritten when no provider answered: {report}"
    );
}

#[test]
fn every_agent_is_reported_when_no_agent_flag_was_given() {
    let report = report(&["ai-cli-readify", "--json", "--timeout", "1"]);
    let agents: Vec<&str> = report["agents"]
        .as_array()
        .expect("an agents array")
        .iter()
        .map(|row| row["agent"].as_str().unwrap())
        .collect();
    assert_eq!(agents, ["codex", "claude", "antigravity", "devin"]);
}

#[test]
fn the_agent_flag_narrows_the_report_to_the_agents_named() {
    let report = report(&[
        "ai-cli-readify",
        "--json",
        "--timeout",
        "1",
        "--agent",
        "devin",
    ]);
    let agents: Vec<&str> = report["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["agent"].as_str().unwrap())
        .collect();
    assert_eq!(agents, ["devin"]);
    // The envelope's `agents` is what was probed, not a fixed list of the
    // four: a snapshot that named all of them would claim a run this command
    // never made.
    assert_eq!(
        report["snapshot"]["agents"],
        serde_json::json!(["devin"]),
        "{report}"
    );
}

#[test]
fn the_json_report_discloses_a_live_non_deterministic_observation() {
    // Every op carries the envelope, and for this one the disclosure is the
    // whole point: the JSON has to say outright that the rows above came from
    // live probes of this machine at this moment, not from a store that would
    // answer the same way tomorrow. `closed_world`, `lower_bound`, `basis`
    // and `confidence` are the fields `classify::document` emits; the
    // `deterministic` of the snapshot is the one that answers "would a second
    // run of this command agree".
    let report = report(&["ai-cli-readify", "--json", "--timeout", "1"]);
    assert_eq!(report["snapshot"]["deterministic"], false, "{report}");
    assert_eq!(
        report["snapshot"]["providers"],
        serde_json::json!(["ollama"]),
        "the snapshot names what was probed: {report}"
    );
    assert_eq!(
        report["snapshot"]["agents"],
        serde_json::json!(["codex", "claude", "antigravity", "devin"]),
        "{report}"
    );
    assert_eq!(report["epistemics"]["closed_world"], false, "{report}");
    assert_eq!(
        report["epistemics"]["lower_bound"], true,
        "one probe proves a provider answered and proves nothing about the next one: {report}"
    );
    assert!(
        report["epistemics"]["basis"]
            .as_str()
            .unwrap()
            .contains("live probe"),
        "the basis has to say where the rows came from: {report}"
    );
    assert!(
        report["epistemics"].get("staleness_ms").is_none(),
        "nothing here measures a stored snapshot, so a staleness would be invented: {report}"
    );
    assert_eq!(
        report["epistemics"]["confidence"], "unready",
        "the envelope's confidence is the verdict the human report prints: {report}"
    );
}

#[test]
fn an_agent_that_cannot_reach_a_provider_is_not_reported_ready() {
    let report = report(&["ai-cli-readify", "--json", "--timeout", "1"]);
    for row in report["agents"].as_array().unwrap() {
        assert_eq!(row["ready"], false, "{row}");
        // The reason has to be in the row: "not ready" alone is the
        // narrowing this command exists to refuse.
        assert!(
            !row["detail"].as_str().unwrap().trim().is_empty(),
            "a not-ready agent must say why: {row}"
        );
    }
}

#[test]
fn devin_is_verified_and_never_rewritten() {
    let report = report(&["ai-cli-readify", "--json", "--timeout", "1"]);
    let verified: Vec<String> = report["verified_only"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap().to_string())
        .collect();
    assert!(
        verified.iter().any(|line| line.starts_with("devin:")),
        "Devin's absence from the rewrite list must read as a decision: {report}"
    );
    // The path has to be the real one under the HOME the run was given. A
    // line that only says "devin: something verified" is also what an empty
    // home directory produces, and then the report names a file the user
    // does not have.
    let home = neutral_home();
    assert!(
        verified.iter().any(
            |line| line.contains(&home.join(".config/devin/config.json").display().to_string())
        ),
        "the verified path is under the HOME the run was given ({}): {report}",
        home.display()
    );
}

#[test]
fn the_non_json_report_states_the_overall_verdict() {
    let out = readify(&["ai-cli-readify", "--timeout", "1"]);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("providers"), "{stdout}");
    assert!(stdout.contains("agents"), "{stdout}");
    assert!(
        stdout.contains("overall: not ready"),
        "with no key and no provider the verdict must not be ready: {stdout}"
    );
    assert!(
        !stdout.contains("overall: ready"),
        "an unready run must not print the ready verdict anywhere: {stdout}"
    );
}

#[test]
fn apply_writes_nothing_when_no_provider_answered() {
    // `--apply` is the one flag that touches the user's files, so the case
    // where it must do nothing is asserted against a HOME it could have
    // written into, and the files are read back afterwards rather than
    // trusted. `neutral_home` is shared, so the check is that the run added
    // nothing rather than that the directory is empty.
    // The paths are the ones `--apply` would write, not rough names for
    // them: a `codex/config.toml` that no code ever touches is missing from
    // every HOME, so asserting on it passes without proving anything.
    let home = neutral_home();
    let before: Vec<std::path::PathBuf> = [
        home.join(".codex/config.toml"),
        home.join(".claude/settings.json"),
        home.join(".gemini/antigravity-cli/settings.json"),
    ]
    .to_vec();
    for path in &before {
        assert!(
            !path.exists(),
            "this test needs a HOME without agent configs, found {}",
            path.display()
        );
    }
    let out = readify(&["ai-cli-readify", "--apply", "--timeout", "1"]);
    assert!(out.status.success(), "{out:?}");
    for path in &before {
        assert!(
            !path.exists(),
            "--apply must not create {} when no provider was chosen for it",
            path.display()
        );
    }
}

#[test]
fn approvals_are_not_attempted_without_the_flag_and_the_report_says_so() {
    let report = report(&["ai-cli-readify", "--json", "--timeout", "1"]);
    assert_eq!(
        report["approvals"],
        serde_json::Value::Null,
        "a run that never asked must not report an empty approval list, which is what a run that asked and found nothing reports: {report}"
    );
    let out = readify(&["ai-cli-readify", "--timeout", "1"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("approvals: not attempted"),
        "the absence of the section must read as a decision: {stdout}"
    );
}

#[test]
fn the_approve_flag_clears_only_the_gate_it_can_reach_with_no_binary_on_path() {
    // `--approve` is the one flag that writes a trust decision, so the case
    // where it writes nothing is run with a PATH that cannot reach any agent
    // binary, against a HOME it could have written into. That HOME is this
    // test's own `Scratch`, not the shared `neutral_home`: `--approve` writes
    // `~/.claude.json`, and under the plain `cargo test` harness both
    // `--approve` tests share one process and therefore one HOME, so a test
    // that ran later would read a file this one wrote. What decides each
    // answer is what that agent's gate *is*: Codex's is an exchange with its
    // own app-server, so no binary means no approval; Claude's is a key in
    // `~/.claude.json` that Claude Code reads the next time it runs, so a
    // missing binary is not in its way and the write still happens. An
    // absent binary is neither an approval nor a refusal — each row has to
    // say which of the two it is.
    let home = Scratch::for_test("pixel-ai-cli-readify-approve", "claude");
    let report = report_in(
        &home,
        &["ai-cli-readify", "--json", "--approve", "--timeout", "1"],
    );

    let rows = report["approvals"]
        .as_array()
        .expect("--approve reports one row per agent");
    assert_eq!(rows.len(), 4, "{report}");
    let row = |agent: &str| -> &serde_json::Value {
        rows.iter()
            .find(|row| row["agent"] == agent)
            .unwrap_or_else(|| panic!("no approval row for {agent}: {report}"))
    };

    // Codex runs its gate through its own app-server, so with none on PATH
    // there is nothing to clear — and the row owes the reason rather than a
    // silent success.
    let codex = row("codex");
    assert_eq!(codex["approved"], false, "{codex}");
    assert!(
        !codex["detail"].as_str().unwrap().trim().is_empty(),
        "a gate that was not cleared must say why: {codex}"
    );

    // Claude's gate is that file, and writing it involves no process.
    let claude = row("claude");
    assert_eq!(claude["approved"], true, "{claude}");

    // The two the reference never wrote: their trust state is read-only
    // here, so neither the flag nor a binary could have changed it.
    for agent in ["antigravity", "devin"] {
        let entry = row(agent);
        assert_eq!(entry["approved"], false, "{entry}");
        assert!(
            entry["detail"]
                .as_str()
                .unwrap()
                .contains("no approval path"),
            "{entry}"
        );
    }

    // The report and the disk have to agree: the folder Codex would have
    // written holds no file at all, and Claude's holds the facts its two
    // dialogs ask about, keyed on the workspace. Both paths are under this
    // test's own HOME, so the file read back is the one this run wrote.
    assert!(
        !home.join(".codex/config.toml").exists(),
        "--approve cleared no Codex gate, so it must have written none"
    );
    let written: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.join(".claude.json"))
            .expect("claude's gate was cleared, so the file is there"),
    )
    .expect("the file this command writes is JSON");
    assert_eq!(written["hasCompletedOnboarding"], true, "{written}");
    let projects = written["projects"]
        .as_object()
        .expect("trust is recorded per workspace");
    assert!(!projects.is_empty(), "{written}");
    assert!(
        projects
            .values()
            .all(|entry| entry["hasTrustDialogAccepted"] == true),
        "{written}"
    );
}

#[test]
fn the_agents_with_no_approval_path_say_so_rather_than_reporting_a_failure() {
    // Through `report_in` like every other run here: it removes the provider
    // keys (a developer machine that exports `OLLAMA_API_KEY` otherwise sends
    // a real completion) and asserts the exit status, and its own HOME keeps
    // this run's `~/.claude.json` out of every other test's.
    let home = Scratch::for_test("pixel-ai-cli-readify-approve", "no-path");
    let report = report_in(
        &home,
        &["ai-cli-readify", "--json", "--approve", "--timeout", "1"],
    );
    for agent in ["antigravity", "devin"] {
        let row = report["approvals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["agent"] == agent)
            .unwrap_or_else(|| panic!("no approval row for {agent}: {report}"));
        assert!(
            row["detail"].as_str().unwrap().contains("no approval path"),
            "{agent} has no writer in the reference, and that must read as a decision: {row}"
        );
    }
}

/// A directory with nothing in it, for a run that must not find an agent
/// binary: `PATH` pointing there is how a test proves that a missing
/// executable is reported rather than spawned.
fn empty_path() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("pixel-cli-empty-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the empty PATH directory");
        dir
    })
}

#[test]
fn the_timeout_flag_is_honoured_and_the_run_is_bounded() {
    // Written to fail on a hang rather than on a slow machine: the run with
    // no key never opens a socket, so a second is generous, and a run that
    // ignores its budget would still be waiting here long after it.
    let started = std::time::Instant::now();
    let out = readify(&["ai-cli-readify", "--timeout", "1"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "the run took {:?} with no key to reach any provider",
        started.elapsed()
    );
}
