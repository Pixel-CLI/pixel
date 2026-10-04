// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Native hook deadlines and interrupted telemetry delivery through the real CLI.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pixel_task::Store;
use pixel_task::replay::{Observation, TelemetryEvent, summarize_trajectory};
use serde_json::{Value, json};

use super::support::{Scratch, git, pixel_command};

fn repo(tag: &str) -> Scratch {
    let root = Scratch::for_test("task-watchdog", tag);
    git(&root, &["init", "-q"]);
    fs::write(root.join("source.txt"), "original\n").unwrap();
    fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "initial"]);
    fs::create_dir(root.join(".pixel")).unwrap();
    root
}

fn command(root: &Path) -> Command {
    let mut command = pixel_command();
    command
        .current_dir(root)
        .env("PIXEL_METRICS", "0")
        .env("PIXEL_TASK_POLICY", "gates")
        .env_remove("PIXEL_TASK_CONTRACT")
        .env_remove("PIXEL_TASK_TELEMETRY_PATH");
    command
}

fn hook(root: &Path, event: &str, mut payload: Value, sink: Option<&Path>) -> Value {
    payload["cwd"] = json!(root);
    hook_raw(root, event, &payload.to_string(), sink)
}

fn hook_raw(root: &Path, event: &str, raw: &str, sink: Option<&Path>) -> Value {
    let mut command = command(root);
    if let Some(sink) = sink {
        command.env("PIXEL_TASK_TELEMETRY_PATH", sink);
    }
    let mut child = command
        .args([
            "run-hook",
            "task-event",
            "--provider",
            "codex",
            "--event",
            event,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(raw.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "hook failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid native response: {error}: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn native_hook_payload_boundary_should_preserve_reads_and_reject_oversized_events() {
    let root = repo("payload-boundary");
    for (size, allowed) in [(1_048_576, true), (1_048_577, false)] {
        let mut payload = json!({"cwd":root.as_ref(),"session_id":"size-session","tool_name":"Read","padding":""});
        let empty_length = payload.to_string().len();
        payload["padding"] = json!("x".repeat(size - empty_length));
        assert_eq!(payload.to_string().len(), size);
        let result = hook_raw(&root, "pre-tool-use", &payload.to_string(), None);
        if allowed {
            assert_eq!(result, json!({}));
            // A complete JSON value followed by one extra whitespace byte
            // still exceeds the host-event bound; truncating to MAX hides it.
            let oversized = format!("{payload} ");
            assert_eq!(
                hook_raw(&root, "pre-tool-use", &oversized, None)["hookSpecificOutput"]["permissionDecision"],
                "deny"
            );
        } else {
            assert_eq!(result["hookSpecificOutput"]["permissionDecision"], "deny");
        }
    }
}

#[test]
fn imported_claude_task_entries_should_stay_silent_without_suppressing_codex() {
    let root = repo("imported-entry");
    for (provider, imported) in [("claude", true), ("claude", false), ("codex", true)] {
        let mut command = command(&root);
        command.args([
            "run-hook",
            "task-event",
            "--provider",
            provider,
            "--event",
            "stop",
        ]);
        if imported {
            command.env("DEVIN_PROJECT_DIR", root.as_ref());
        } else {
            command.env_remove("DEVIN_PROJECT_DIR");
        }
        // Empty stdin invokes the fail-closed native Stop envelope, except for
        // an imported Claude entry whose actual host owns the task lifecycle.
        let output = command.stdin(Stdio::null()).output().unwrap();
        assert_eq!(output.status.code(), Some(0));
        if provider == "claude" && imported {
            assert_eq!(output.stdout, b"");
        } else {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["continue"], false);
        }
    }
}

/// Holds the session's binding lock so the task evaluator stalls, sends one
/// `Write` through the real hook, and returns its output once the holder is
/// released. Asserts the hook answered on its watchdog, inside the host deadline.
fn stalled_write(tag: &str, settings: Option<&str>) -> (Scratch, Value) {
    let root = repo(tag);
    if let Some(settings) = settings {
        fs::write(root.join(".pixel/config.yaml"), settings).unwrap();
    }
    let directory = root.join(".pixel/tasks/session-locks");
    fs::create_dir_all(&directory).unwrap();
    let lock_name = pixel_task::digest(&("codex", "stalled-session")).unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(lock_name))
        .unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    let released = Arc::new(AtomicBool::new(false));
    let holder_released = Arc::clone(&released);
    let (release, wait) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        // A removed watchdog must fail the assertion, rather than hang the suite.
        let _ = wait.recv_timeout(Duration::from_secs(9));
        holder_released.store(true, Ordering::SeqCst);
        fs2::FileExt::unlock(&lock).unwrap();
    });
    let started = Instant::now();
    let output = hook(
        &root,
        "pre-tool-use",
        json!({"session_id":"stalled-session","event_id":"stalled-edit","tool_use_id":"edit-1","tool_name":"Write","tool_input":{"file_path":"source.txt","content":"modified"}}),
        None,
    );
    let elapsed = started.elapsed();
    let exited_while_locked = !released.load(Ordering::SeqCst);
    let _ = release.send(());
    holder.join().unwrap();
    assert!(
        exited_while_locked,
        "hook waited for the locked evaluator: {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_secs(5),
        "fixture did not reach the watchdog: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(9),
        "hook exceeded its host deadline margin: {elapsed:?}"
    );
    assert_eq!(
        fs::read_to_string(root.join("source.txt")).unwrap(),
        "original\n"
    );
    (root, output)
}

#[test]
fn stalled_native_gate_should_deny_enforced_edits_before_the_host_deadline() {
    let (_root, output) =
        stalled_write("held-session-lock", Some("task:\n  enforcement: enforce\n"));
    assert_eq!(output["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(output["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(
        output["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("unavailable")
    );
}

#[test]
fn stalled_native_gate_should_only_observe_an_unenforced_session() {
    // An answering ledger only observes edits without enforcement; a stalled
    // one must not deny more.
    let (_root, output) = stalled_write("held-unenforced-lock", None);
    assert_eq!(output, json!({}));
}

fn requested(events: &[TelemetryEvent]) -> Vec<&TelemetryEvent> {
    events
        .iter()
        .filter(|event| {
            matches!(
                &event.observation,
                Observation::ToolRequested { request_id, .. } if request_id == "read-1"
            )
        })
        .collect()
}

#[test]
fn telemetry_export_retry_should_recover_the_event_and_count_one_request() {
    let root = repo("telemetry-redelivery");
    hook(
        &root,
        "prompt-submit",
        json!({"session_id":"retry-session","event_id":"prompt-1","prompt":"fix source"}),
        None,
    );
    let store = Store::open(&root).unwrap();
    let task = store
        .find_session("codex", "retry-session")
        .unwrap()
        .unwrap();
    let sink = root.join(".pixel/telemetry.jsonl");
    fs::create_dir(&sink).unwrap();
    let payload = json!({"session_id":"retry-session","event_id":"request-1","tool_use_id":"read-1","tool_name":"Read","tool_input":{"file_path":"source.txt"}});
    assert_eq!(
        hook(&root, "pre-tool-use", payload.clone(), Some(&sink)),
        json!({})
    );
    let journal: Vec<TelemetryEvent> = store
        .events(&task.task_id)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "observation" && event.data["kind"] == "telemetry")
        .map(|event| serde_json::from_value(event.data["data"].clone()).unwrap())
        .collect();
    let durable_request = requested(&journal);
    assert_eq!(
        durable_request.len(),
        1,
        "the failed export must already be durable"
    );
    assert!(sink.is_dir(), "the first destination must reject append");
    fs::remove_dir(&sink).unwrap();

    assert_eq!(
        hook(&root, "pre-tool-use", payload.clone(), Some(&sink)),
        json!({})
    );
    let recovered: Vec<TelemetryEvent> = fs::read_to_string(&sink)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        requested(&recovered),
        durable_request,
        "retry must export the exact committed event"
    );

    assert_eq!(hook(&root, "pre-tool-use", payload, Some(&sink)), json!({}));
    let redelivered: Vec<TelemetryEvent> = fs::read_to_string(&sink)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let copies = requested(&redelivered);
    assert_eq!(
        copies.len(),
        2,
        "the fixture must exercise duplicate physical delivery"
    );
    assert_eq!(copies[0], copies[1]);
    let summary = summarize_trajectory(&redelivered).unwrap();
    assert_eq!(summary.observed_model_tool_requests, 1);
    assert_eq!(
        summary.model_tool_requests, None,
        "partial native coverage is not a complete count"
    );

    let output = command(&root)
        .args(["task", "events", &task.task_id, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(events["trajectory"]["observed_model_tool_requests"], 1);
}
