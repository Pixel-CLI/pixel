// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The edit-time events of `pixel hook` against a running daemon: the card
//! before an edit, the stale callers after it, and the block at Stop.

use std::io::Write;
use std::process::{Output, Stdio};

use serde_json::{Value, json};

use crate::support::{Fixture, project, stderr, stdout};

/// Stops the fixture's daemon when the test ends, however it ends.
struct Daemon<'a>(&'a Fixture);

impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        let _ = self.0.run(&["daemon", "stop"]);
    }
}

/// Starts the fixture's daemon and waits until it has answered a graph query:
/// the hook ops never start a daemon or wait for one to finish its build, so
/// a hook sent before the first generation is published would stay silent.
fn start(fixture: &Fixture) -> Output {
    let started = fixture.run(&["daemon", "start"]);
    let warmed = fixture.run(&["find-symbol", "run"]);
    assert!(warmed.status.success(), "{}", stderr(&warmed));
    started
}

fn hook(fixture: &Fixture, event: &str, host: &str, payload: &Value) -> Output {
    let mut child = fixture
        .pixel(&["hook", event, "--host", host])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("pixel runs");
    let _ = child
        .stdin
        .take()
        .expect("stdin")
        .write_all(payload.to_string().as_bytes());
    child.wait_with_output().unwrap()
}

fn envelope(output: &Output) -> Value {
    serde_json::from_str(stdout(output).trim())
        .unwrap_or_else(|error| panic!("one JSON object expected: {error}: {}", stdout(output)))
}

fn tool_payload(fixture: &Fixture, name: &str, event: &str) -> Value {
    json!({
        "hook_event_name": event,
        "session_id": "edit-session",
        "cwd": fixture.root(),
        "tool_name": name,
        "tool_input": {
            "file_path": fixture.root().join("src/a.rs"),
            "old_string": "pub fn run(x: u32)",
            "new_string": "pub fn run(x: u32, y: u32)",
        },
    })
}

fn journal(fixture: &Fixture) -> Vec<Value> {
    std::fs::read_to_string(fixture.root().join(".pixel/journal.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn stop_payload(fixture: &Fixture, active: bool) -> Value {
    json!({
        "hook_event_name": "Stop",
        "session_id": "edit-session",
        "cwd": fixture.root(),
        "stop_hook_active": active,
    })
}

#[test]
fn the_edit_events_should_print_nothing_while_edit_hooks_are_off() {
    let fixture = project();
    let started = start(&fixture);
    assert!(started.status.success(), "{}", stderr(&started));
    let _daemon = Daemon(&fixture);
    for (event, name) in [("pre-edit", "PreToolUse"), ("post-edit", "PostToolUse")] {
        let output = hook(
            &fixture,
            event,
            "claude",
            &tool_payload(&fixture, "Edit", name),
        );
        assert!(output.status.success());
        assert_eq!(stdout(&output), "", "{event}");
    }
    let stop = hook(&fixture, "stop", "claude", &stop_payload(&fixture, false));
    assert_eq!(stdout(&stop), "");
    assert!(
        journal(&fixture)
            .iter()
            .all(|line| !["edit", "stale", "guard"].contains(&line["t"].as_str().unwrap_or("")))
    );
}

#[test]
fn an_edit_should_card_go_stale_and_block_stop_once() {
    let fixture = project();
    assert!(
        fixture
            .run(&["config", "edit_hooks", "on"])
            .status
            .success()
    );
    let started = start(&fixture);
    assert!(started.status.success(), "{}", stderr(&started));
    let _daemon = Daemon(&fixture);

    let pre = hook(
        &fixture,
        "pre-edit",
        "claude",
        &tool_payload(&fixture, "Edit", "PreToolUse"),
    );
    assert!(pre.status.success(), "{}", stderr(&pre));
    let card = envelope(&pre);
    assert_eq!(card["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    let text = card["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        text.starts_with("[PIXEL:EDIT] `run` (src/a.rs) changes shape"),
        "{text}"
    );
    assert!(text.contains("src/b.rs:4 in go"), "{text}");
    assert!(
        card["systemMessage"]
            .as_str()
            .unwrap()
            .starts_with("🟩 pixel edit ❀ 1 callers"),
        "{card}"
    );

    // The tool runs.
    fixture.write(
        "src/a.rs",
        "pub fn run(x: u32, y: u32) -> u32 {\n    x + y\n}\n",
    );
    let post = hook(
        &fixture,
        "post-edit",
        "claude",
        &tool_payload(&fixture, "Edit", "PostToolUse"),
    );
    let stale = envelope(&post);
    assert_eq!(stale["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    let text = stale["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(text.starts_with("[PIXEL:STALE-CALLERS]"), "{text}");
    assert!(text.contains("src/b.rs:4 in go"), "{text}");
    assert!(
        stale["systemMessage"]
            .as_str()
            .unwrap()
            .contains("1 stale sites")
    );

    let active = hook(&fixture, "stop", "claude", &stop_payload(&fixture, true));
    assert_eq!(
        stdout(&active),
        "",
        "a stop a stop hook caused is never blocked"
    );
    let blocked = envelope(&hook(
        &fixture,
        "stop",
        "claude",
        &stop_payload(&fixture, false),
    ));
    assert_eq!(blocked["decision"], "block");
    assert!(
        blocked["reason"]
            .as_str()
            .unwrap()
            .contains("src/b.rs:4 in go"),
        "{blocked}"
    );
    let again = hook(&fixture, "stop", "claude", &stop_payload(&fixture, false));
    assert_eq!(stdout(&again), "", "the same set blocks once");

    let lines = journal(&fixture);
    let kinds: Vec<&str> = lines.iter().filter_map(|l| l["t"].as_str()).collect();
    for kind in ["edit", "stale", "guard"] {
        assert!(kinds.contains(&kind), "{kind} in {kinds:?}");
    }
    let guards: Vec<&Value> = lines.iter().filter(|l| l["t"] == "guard").collect();
    assert_eq!(guards.last().unwrap()["silent"], "already_blocked");
    assert!(
        !serde_json::to_string(&lines)
            .unwrap()
            .contains("PIXEL:EDIT")
    );
}

#[test]
fn a_caller_the_agent_updates_should_not_block_stop() {
    let fixture = project();
    assert!(
        fixture
            .run(&["config", "edit_hooks", "on"])
            .status
            .success()
    );
    assert!(start(&fixture).status.success());
    let _daemon = Daemon(&fixture);
    hook(
        &fixture,
        "pre-edit",
        "claude",
        &tool_payload(&fixture, "Edit", "PreToolUse"),
    );
    fixture.write(
        "src/a.rs",
        "pub fn run(x: u32, y: u32) -> u32 {\n    x + y\n}\n",
    );
    hook(
        &fixture,
        "post-edit",
        "claude",
        &tool_payload(&fixture, "Edit", "PostToolUse"),
    );
    fixture.write(
        "src/b.rs",
        "use crate::a::run;\n\npub fn go() -> u32 {\n    run(1, 2)\n}\n",
    );
    let stop = hook(&fixture, "stop", "claude", &stop_payload(&fixture, false));
    assert_eq!(stdout(&stop), "");
}

#[test]
fn pi_should_get_the_card_as_a_bare_decision_and_a_stop_that_continues() {
    let fixture = project();
    assert!(
        fixture
            .run(&["config", "edit_hooks", "on"])
            .status
            .success()
    );
    assert!(start(&fixture).status.success());
    let _daemon = Daemon(&fixture);
    let payload = |event: &str| {
        json!({
            "hook_event_name": event,
            "session_id": "pi-session",
            "cwd": fixture.root(),
            "tool_name": "edit",
            "tool_input": {
                "path": "src/a.rs",
                "edits": [{"oldText": "pub fn run(x: u32)", "newText": "pub fn run(x: u32, y: u32)"}],
            },
        })
    };
    let card = envelope(&hook(&fixture, "pre-edit", "pi", &payload("tool_call")));
    assert!(
        card["context"]
            .as_str()
            .unwrap()
            .starts_with("[PIXEL:EDIT]"),
        "{card}"
    );
    fixture.write(
        "src/a.rs",
        "pub fn run(x: u32, y: u32) -> u32 {\n    x + y\n}\n",
    );
    hook(&fixture, "post-edit", "pi", &payload("tool_result"));
    let stop = envelope(&hook(
        &fixture,
        "stop",
        "pi",
        &json!({"hook_event_name": "agent_end", "session_id": "pi-session", "cwd": fixture.root()}),
    ));
    assert_eq!(stop["continue"], true);
    assert!(
        stop["context"].as_str().unwrap().contains("src/b.rs:4"),
        "{stop}"
    );
}

#[test]
fn with_no_daemon_running_the_edit_events_should_stay_silent_and_say_why() {
    let fixture = project();
    assert!(
        fixture
            .run(&["config", "edit_hooks", "on"])
            .status
            .success()
    );
    let pre = hook(
        &fixture,
        "pre-edit",
        "claude",
        &tool_payload(&fixture, "Edit", "PreToolUse"),
    );
    assert!(pre.status.success());
    assert_eq!(stdout(&pre), "");
    let lines = journal(&fixture);
    let edit: Vec<&Value> = lines.iter().filter(|l| l["t"] == "edit").collect();
    assert_eq!(edit.len(), 1, "{lines:?}");
    assert_eq!(edit[0]["silent"], "no_daemon");
}
