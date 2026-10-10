// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel recall` end to end on a scratch corpus: one Claude transcript
//! under a throwaway `HOME`, indexed by the binary, then read back through
//! `search`, `show`, `status` and `context`. These are the commands the
//! agent prompt's recall method documents; each assertion is on the stdout
//! contract an agent parses, so a command that silently prints nothing
//! (or the wrong session) fails here.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::support::{Scratch, fake_recall_daemon, pixel_command};

const SESSION_ID: &str = "0123abcd-0000-4000-8000-000000000001";
/// The human turn: the needle, then enough text that its full-turn block
/// cannot fit a 100-token context budget while the header line can.
const NEEDLE: &str = "please fix the streamed needle";
const TAIL: &str = " it fails on the second retry and the log shows nothing useful";

/// A scratch HOME and recall dir holding one two-turn Claude session: a
/// human prompt with the needle and a harness-injected reminder turn.
struct Corpus {
    home: Scratch,
}

impl Corpus {
    fn new(tag: &str) -> Self {
        Self::with_human_turn(tag, &format!("{NEEDLE}{}", TAIL.repeat(8)))
    }

    /// `new`'s fixture with the human turn text under the caller's control:
    /// `new` sizes it for the context budget, the stdout-cap test needs one
    /// far past the cap.
    fn with_human_turn(tag: &str, text: &str) -> Self {
        let corpus = Self::write_fixture(tag, text);
        let out = corpus.run(&["recall", "index", "--source", "claude"]);
        assert!(out.status.success(), "index: {out:?}");
        corpus
    }

    /// The transcript files only — no index: the corpus starts cold so a
    /// test can prove a query path catches up on demand.
    fn write_fixture(tag: &str, text: &str) -> Self {
        let home = Scratch::for_test("recall-cli", tag);
        let slug = home.join(".claude/projects/-work-pixel");
        std::fs::create_dir_all(&slug).unwrap();
        let lines = [
            serde_json::json!({
                "type": "user", "cwd": "/work/pixel", "gitBranch": "develop",
                "timestamp": "2025-10-09T08:53:20.000Z",
                "message": {"content": [{"type": "text", "text": text}]},
            })
            .to_string(),
            serde_json::json!({
                "type": "user", "cwd": "/work/pixel",
                "timestamp": "2025-10-09T08:54:20.000Z",
                "message": {"content": [{"type": "text", "text": "<system-reminder>injected context</system-reminder>"}]},
            })
            .to_string(),
        ];
        std::fs::write(
            slug.join(format!("{SESSION_ID}.jsonl")),
            format!("{}\n", lines.join("\n")),
        )
        .unwrap();
        Self { home }
    }

    fn command(&self) -> Command {
        let mut cmd = pixel_command();
        cmd.env("HOME", self.home.as_ref() as &Path)
            .env("PIXEL_RECALL_DIR", self.home.join("recall"))
            .env_remove("PIXEL_RECALL_MODEL")
            .current_dir(self.home.as_ref() as &Path);
        cmd
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.command().args(args).output().unwrap()
    }

    /// `run` with extra environment variables: `PIXEL_OUTPUT_CAP_BYTES` is a
    /// per-invocation setting, so a cap test cannot put it on the shared
    /// command builder without changing every other test's output.
    fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
        let mut cmd = self.command();
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd.args(args).output().unwrap()
    }

    fn stdout(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "pixel {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.stdout(args)).unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }
}

#[test]
fn ask_groups_the_matching_session_from_the_lexical_channel() {
    // No model here: `--lexical-only` answers from the corpus in-process, the
    // path `ask` takes when no recall daemon is listening.
    let corpus = Corpus::new("ask");
    let out = corpus.json(&[
        "recall",
        "ask",
        "streamed needle",
        "--lexical-only",
        "--json",
    ]);
    let groups = out["groups"].as_array().expect("groups");
    assert_eq!(groups.len(), 1, "{out}");
    assert_eq!(groups[0]["source_session_id"], SESSION_ID);
    assert_eq!(groups[0]["matched_lexical"], true, "{out}");

    let text = corpus.stdout(&["recall", "ask", "streamed needle", "--lexical-only"]);
    assert!(text.starts_with("claude:0123abcd #"), "{text}");
    assert!(text.contains("[lex]"), "{text}");
    let miss = corpus.stdout(&["recall", "ask", "no-such-token-xyzzy", "--lexical-only"]);
    assert!(miss.contains("no matching sessions"), "{miss}");
}

/// AR-01: the daemon probe must never open a repository `Service` on the
/// corpus directory. `recall daemon status` used to go through the
/// auto-starting `try_daemon`, which spawned `pixel daemon start
/// <recall_dir> --foreground`: the socket was then held by a repo daemon that
/// answers `Ping` and refuses `Recall`, and `Service::open` left
/// `.pixel/base.shard` in the corpus. Auto-start is deliberately left enabled
/// here — the probe must not depend on `PIXEL_DAEMON_AUTO_START=0`.
#[test]
fn daemon_status_never_starts_a_repo_service_on_the_corpus() {
    let home = Scratch::for_test("recall-cli", "daemon-status");
    let recall_dir = home.join("recall");
    let out = Command::new(env!("CARGO_BIN_EXE_pixel"))
        .env("HOME", home.as_ref() as &Path)
        .env("PIXEL_RECALL_DIR", &recall_dir)
        .current_dir(home.as_ref() as &Path)
        .args(["recall", "daemon", "status"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("daemon not running"), "{stdout}");
    assert!(
        !recall_dir.join(".pixel/base.shard").exists(),
        "a repo Service was opened on the corpus: {stdout}"
    );
}

/// HO-08: a recall daemon that answers an op with an error is named on stderr
/// and the answer still comes from the corpus in-process. A daemon with a
/// corrupt vector store used to be indistinguishable from no daemon at all,
/// so the op was silently redone (model load included) and the failure never
/// reached the caller.
#[test]
fn search_names_a_failing_recall_daemon_then_answers_in_process() {
    let corpus = Corpus::new("daemon-failure");
    let recall_dir = corpus.home.join("recall");
    let server = fake_recall_daemon(
        &recall_dir,
        pixel_daemon::api::failure_response("recall", "vector store is corrupt"),
    );
    let out = corpus.run(&["recall", "search", "streamed needle", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("vector store is corrupt"), "{stderr}");
    let value: Value = serde_json::from_slice(&out.stdout).expect("hits from the in-process path");
    assert_eq!(value["hits"].as_array().map(Vec::len), Some(1), "{value}");
    server.join().unwrap();
    let _ = std::fs::remove_file(pixel_daemon::daemon::socket_path(&recall_dir));
}

/// A slow `recall` line in `actions.jsonl` must say where its seconds went:
/// the catch-up and model load (`open_ms`), the query (`handle_ms`), or the
/// daemon's round trip. The search catches up in process before asking the
/// daemon, so its daemon step still carries `open_ms`.
#[test]
fn recall_commands_record_how_they_were_served_in_the_action_log() {
    let corpus = Corpus::new("serve-steps");
    corpus.stdout(&["recall", "search", "streamed needle"]);
    corpus.stdout(&["recall", "ask", "streamed needle", "--lexical-only"]);
    corpus.stdout(&["recall", "context", "streamed needle", "--lexical-only"]);
    let recall_dir = corpus.home.join("recall");
    let server = fake_recall_daemon(
        &recall_dir,
        pixel_daemon::Response::success("recall", serde_json::json!({"text": "from daemon\n"})),
    );
    let served = corpus.stdout(&["recall", "search", "streamed needle"]);
    server.join().unwrap();
    let _ = std::fs::remove_file(pixel_daemon::daemon::socket_path(&recall_dir));
    assert_eq!(served, "from daemon\n");

    let log = std::fs::read_to_string(corpus.home.join(".pixel/actions.jsonl")).unwrap();
    // The fixture's own `recall index` is logged first; it routes nothing.
    let steps: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["args"] != "recall index --source claude")
        .map(|event| {
            let serve = event["serve"]
                .as_array()
                .unwrap_or_else(|| panic!("serve steps: {event}"))
                .clone();
            assert_eq!(serve.len(), 1, "{event}");
            serve[0].clone()
        })
        .collect();
    assert_eq!(steps.len(), 4, "{log}");
    for (step, reason) in steps[..3]
        .iter()
        .zip(["daemon_absent", "daemon_absent", "not_routed"])
    {
        assert_eq!(step["route"], "in_process", "{step}");
        assert_eq!(step["reason"], reason, "{step}");
        assert!(
            step["open_ms"].is_u64() && step["handle_ms"].is_u64(),
            "{step}"
        );
    }
    // No daemon: the ping was paid, no request was sent.
    assert!(steps[0]["probe_ms"].is_u64(), "{}", steps[0]);
    assert!(steps[0].get("request_ms").is_none(), "{}", steps[0]);
    let daemon = &steps[3];
    assert_eq!(daemon["route"], "daemon", "{daemon}");
    for phase in ["probe_ms", "request_ms", "open_ms"] {
        assert!(daemon[phase].is_u64(), "{phase}: {daemon}");
    }
    assert!(daemon.get("handle_ms").is_none(), "{daemon}");
}

/// `recall show --json` is a machine document and travels under the global
/// stdout cap: at `PIXEL_OUTPUT_CAP_BYTES=4096` a 16 KiB turn must come out
/// capped, still one JSON object, and marked `truncated` instead of dumping
/// the whole turn; without the cap the same document is complete.
#[test]
fn show_json_is_capped_on_the_stdout_cap_and_complete_without_it() {
    let turn = format!("{NEEDLE} {}", "retry noise ".repeat(1400));
    let corpus = Corpus::with_human_turn("show-cap", &turn);
    let args = ["recall", "show", "claude:0123abcd", "--json"];

    let capped = corpus.run_with_env(&args, &[("PIXEL_OUTPUT_CAP_BYTES", "4096")]);
    assert!(capped.status.success(), "{capped:?}");
    assert!(
        capped.stdout.len() <= 4097,
        "the cap holds: {} bytes",
        capped.stdout.len()
    );
    let value: Value = serde_json::from_slice(&capped.stdout).expect("one JSON document");
    assert_eq!(value["truncated"], true, "{value}");
    assert_eq!(value["cap_bytes"], 4096);
    assert_eq!(value["session"]["source_session_id"], SESSION_ID);

    let full = corpus.run_with_env(&args, &[("PIXEL_OUTPUT_CAP_BYTES", "0")]);
    assert!(full.status.success(), "{full:?}");
    let value: Value = serde_json::from_slice(&full.stdout).expect("one JSON document");
    assert!(value["truncated"].is_null(), "not capped: {value}");
    assert_eq!(value["turns"].as_array().map(Vec::len), Some(2), "{value}");
    assert_eq!(value["turns"][0]["text"].as_str(), Some(turn.as_str()));
}

#[test]
fn search_returns_the_matching_turn_with_its_session_reference() {
    let corpus = Corpus::new("search");
    let out = corpus.json(&["recall", "search", "streamed needle", "--json"]);
    let hits = out["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 1, "{out}");
    assert_eq!(hits[0]["agent"], "claude");
    assert_eq!(hits[0]["source_session_id"], SESSION_ID);
    assert_eq!(hits[0]["role"], "user");
    assert_eq!(hits[0]["cwd"], "/work/pixel");
    assert!(
        hits[0]["snippet"]
            .as_str()
            .unwrap()
            .contains("streamed needle"),
        "{out}"
    );
    assert_eq!(out["truncated"], false);
    assert_eq!(out["turns_considered"].as_u64().map(|n| n >= 1), Some(true));

    let text = corpus.stdout(&["recall", "search", "streamed needle"]);
    assert!(
        text.starts_with("claude:0123abcd #"),
        "text line names the session ref: {text}"
    );
    assert!(
        text.contains("user \"please fix the streamed needle"),
        "{text}"
    );

    let miss = corpus.stdout(&["recall", "search", "no-such-token-xyzzy"]);
    assert!(miss.starts_with("no matches ("), "{miss}");
}

/// The lazy contract end to end: a query on a corpus that was never
/// indexed ingests on demand — no `recall index`, no daemon — instead of
/// answering from an empty store. The window filters on the file's mtime
/// (just written → recent), not the timestamps inside the records.
#[test]
fn search_on_a_cold_corpus_ingests_on_demand() {
    let corpus = Corpus::write_fixture("lazy", &format!("{NEEDLE}{}", TAIL.repeat(8)));
    let out = corpus.json(&["recall", "search", "streamed needle", "--json"]);
    let hits = out["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 1, "cold search must self-ingest: {out}");
    assert_eq!(hits[0]["source_session_id"], SESSION_ID);
}

/// The store-only commands catch up on demand too: sessions, show,
/// and export each run against their own never-indexed corpus, so
/// a missing catch-up in one cannot hide behind another's warm-up.
#[test]
fn cold_corpus_sessions_show_and_export() {
    let content = || format!("{NEEDLE}{}", TAIL.repeat(8));

    let corpus = Corpus::write_fixture("lazy-sessions", &content());
    let sessions = corpus.json(&["recall", "sessions", "--json"]);
    assert_eq!(
        sessions["sessions"].as_array().map(Vec::len),
        Some(1),
        "{sessions}"
    );

    let corpus = Corpus::write_fixture("lazy-show", &content());
    let shown = corpus.stdout(&["recall", "show", "claude:0123abcd"]);
    assert!(shown.contains("streamed needle"), "{shown}");

    let corpus = Corpus::write_fixture("lazy-export", &content());
    let out_dir = corpus.home.join("export");
    let out = corpus.run(&[
        "recall",
        "export",
        "--out",
        out_dir.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);
    assert!(out.status.success(), "export: {out:?}");
    let written = std::fs::read_dir(&out_dir).unwrap().flatten().count();
    assert_eq!(written, 1, "one session file exported");
}

#[test]
fn search_role_filter_is_validated_and_applied() {
    let corpus = Corpus::new("search-role");
    let bad = corpus.run(&["recall", "search", "needle", "--role", "bogus"]);
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("--role must be user, assistant, or tool"),
        "{bad:?}"
    );
    let user = corpus.json(&["recall", "search", "needle", "--role", "user", "--json"]);
    assert_eq!(user["hits"].as_array().map(Vec::len), Some(1), "{user}");
    let tool = corpus.json(&["recall", "search", "needle", "--role", "tool", "--json"]);
    assert_eq!(tool["hits"].as_array().map(Vec::len), Some(0), "{tool}");
}

#[test]
fn show_prints_the_turns_and_marks_harness_injected_ones() {
    let corpus = Corpus::new("show");
    let out = corpus.json(&["recall", "show", "claude:0123abcd", "--json"]);
    assert_eq!(out["session"]["source_session_id"], SESSION_ID);
    let turns = out["turns"].as_array().expect("turns");
    assert_eq!(turns.len(), 2, "{out}");
    assert_eq!(turns[0]["seq"], 0);
    assert!(
        turns[0]["text"].as_str().unwrap().starts_with(NEEDLE),
        "{out}"
    );
    assert_eq!(turns[0]["intent_source"], "human");
    assert_eq!(turns[1]["intent_source"], "orchestrator");
    assert_eq!(turns[1]["truncated"], false);

    let text = corpus.stdout(&["recall", "show", "claude:0123abcd"]);
    let mut lines = text.lines();
    assert!(
        lines.next().unwrap().starts_with("claude:0123abcd #"),
        "{text}"
    );
    assert_eq!(lines.next(), Some("branch: develop"));
    assert!(
        text.contains("--- #0 user 2025-10-09 08:53 ---\nplease fix the streamed needle"),
        "{text}"
    );
    assert!(
        text.contains("--- #1 user 2025-10-09 08:54 (orchestrator) ---\n"),
        "the injected turn is labelled: {text}"
    );

    let ranged = corpus.stdout(&["recall", "show", "claude:0123abcd", "--turn", "1..1"]);
    assert!(ranged.contains("--- #1 user"), "{ranged}");
    assert!(!ranged.contains("--- #0 user"), "{ranged}");
    let empty = corpus.stdout(&["recall", "show", "claude:0123abcd", "--turn", "5..9"]);
    assert!(empty.trim_end().ends_with("(no turns in range)"), "{empty}");
}

#[test]
fn status_reports_the_corpus_counts_and_location() {
    let corpus = Corpus::new("status");
    let out = corpus.json(&["recall", "status", "--json"]);
    assert_eq!(out["total_turns"], 2, "{out}");
    assert_eq!(
        out["unsegmented_turns"], 0,
        "index segments what it ingests"
    );
    assert_eq!(out["lexical_segments"], 1);
    assert_eq!(
        out["location"],
        Value::String(corpus.home.join("recall").display().to_string())
    );
    let agents = out["agents"].as_array().expect("agents");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["agent"], "claude");
    assert_eq!(agents[0]["sessions"], 1);
    assert_eq!(agents[0]["turns"], 2);

    let text = corpus.stdout(&["recall", "status"]);
    assert!(text.starts_with("corpus: "), "{text}");
    assert!(text.contains("claude"), "{text}");
}

#[test]
fn context_packs_the_matching_session_within_the_budget() {
    let corpus = Corpus::new("context");
    let text = corpus.stdout(&[
        "recall",
        "context",
        "streamed needle",
        "--lexical-only",
        "--budget",
        "500",
    ]);
    assert!(
        text.starts_with("recall context for: streamed needle\n"),
        "{text}"
    );
    assert!(text.contains("- [claude:0123abcd #"), "L0 header: {text}");
    assert!(
        text.contains(&format!("--- session #1 turn 0 (user) ---\n{NEEDLE}")),
        "L2 full turn: {text}"
    );
    assert!(text.contains("\nfitted: budget=500 used="), "{text}");
    assert!(text.trim_end().ends_with("dropped_blocks=0"), "{text}");

    // Every filter narrows the pack: the wrong agent, another repo or a
    // window after the session each leave no session group.
    for extra in [
        ["--agent", "codex"],
        ["--repo", "/elsewhere"],
        ["--since", "1h"],
        ["--until", "2020-01-01"],
    ] {
        let mut args = vec!["recall", "context", "streamed needle", "--lexical-only"];
        args.extend(extra);
        let filtered = corpus.stdout(&args);
        assert!(
            !filtered.contains("- [claude:"),
            "{extra:?} must exclude the session: {filtered}"
        );
    }

    // A budget that fits the header line but not the full turn drops the
    // turn and says so; `used` stays within the budget.
    let tight = corpus.stdout(&[
        "recall",
        "context",
        "streamed needle",
        "--lexical-only",
        "--budget",
        "100",
    ]);
    assert!(
        tight.contains("- [claude:0123abcd #"),
        "header kept: {tight}"
    );
    assert!(
        !tight.contains("--- session #"),
        "the full turn block is dropped: {tight}"
    );
    let footer = tight.lines().last().unwrap();
    assert!(footer.ends_with("dropped_blocks=1"), "{footer}");
    let used: usize = footer
        .split("used=")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap();
    assert!(used <= 100, "{footer}");

    let err = corpus.run(&["recall", "context", "x", "--budget", "10"]);
    assert!(!err.status.success());
    assert!(
        String::from_utf8_lossy(&err.stderr).contains("--budget must be between"),
        "{err:?}"
    );
}
