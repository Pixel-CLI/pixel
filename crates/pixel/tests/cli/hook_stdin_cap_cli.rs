// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The hook entry points under `pixel run-hook`.
//!
//! `task-event` is the only live one: it reads the host's payload on a pipe
//! the host owns through one shared cap and then must answer. An over-cap
//! payload takes its unavailable envelope, which denies `PreToolUse` on an
//! enforced session.
//!

use std::io::Write;
use std::process::{Output, Stdio};

use crate::support::pixel_command;

/// The shared hook cap, spelled here because this integration test links
/// against the binary, not its private modules. `hook_input::tests` pins it
/// equal to the task-event cap, and every payload below is sized from it so
/// the boundary is exact rather than a nearby round number.
const MAX_HOOK_INPUT: usize = 1_048_576;

/// One byte past the shared cap, so the payload is refused by the cap and not
/// by a size the test happened to pick.
fn oversized_payload() -> String {
    let filler = "a".repeat(MAX_HOOK_INPUT + 1);
    format!(
        r#"{{"hook_event_name":"PreToolUse","tool_name":"Grep","tool_input":{{"pattern":"{filler}"}},"cwd":"/tmp","session_id":"s","transcript_path":"/tmp/t.jsonl","permission_mode":"default","hook_type":"PreToolUse"}}"#
    )
}

/// Feed `payload` to `args` on stdin and collect the finished process.
///
/// The payload is written from a thread that can stop early: an entry point
/// that drains the pipe to EOF would otherwise block this test forever instead
/// of failing it, which is the very bug under test. The write side is dropped
/// either way so a correctly-capped reader sees EOF.
fn feed(args: &[&str], payload: &str) -> Output {
    let mut child = pixel_command()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pixel run-hook");
    let mut stdin = child.stdin.take().expect("hook stdin");
    let body = payload.to_string();
    std::thread::spawn(move || {
        let _ = stdin.write_all(body.as_bytes());
        let _ = stdin.flush();
        // Dropping `stdin` closes the pipe: the reader reaches EOF.
    });
    child.wait_with_output().expect("hook output")
}

/// `task-event` reads through the same reader: an over-cap payload takes the
/// same unavailable envelope an unreadable one takes, so the cap cannot be
/// bypassed on this entry point. The envelope is non-empty, so this cannot
/// pass by the hook exiting before it read.
#[test]
fn task_event_should_take_the_same_envelope_for_an_oversized_and_a_malformed_payload() {
    let args = [
        "run-hook",
        "task-event",
        "--provider",
        "claude",
        "--event",
        "pre-tool-use",
    ];
    let oversized = feed(&args, &oversized_payload());
    let malformed = feed(&args, "not json");
    assert!(oversized.status.success(), "task-event: {oversized:?}");
    assert!(
        !oversized.stdout.is_empty(),
        "the unavailable envelope is the observable, not an empty exit"
    );
    assert_eq!(
        oversized.stdout, malformed.stdout,
        "an over-cap payload must take the same unavailable envelope as a malformed one"
    );
}
