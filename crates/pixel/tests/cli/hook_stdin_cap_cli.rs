// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The hook entry points under `pixel run-hook`.
//!
//! `task-event` is the only live one: it reads the host's payload on a pipe
//! the host owns through one shared cap and then must answer. An over-cap
//! payload takes its unavailable envelope, which denies `PreToolUse` on an
//! enforced session.
//!
//! The other verbs (`guard`, `composed-guard`, `session-start`,
//! `prompt-submit`, `post-compaction`, `post-tool-use`, `metrics`) are
//! retired. A settings file written by an older release keeps naming them
//! until the user runs `pixel install` again, and a verb that stopped parsing
//! would exit 2, which a host reads as a block on the tool call. They stay
//! accepted, with the flags those releases wrote, and answer nothing.

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

/// Every retired verb, spelled the way each release that registered it wrote
/// it into an agent's settings file.
const RETIRED: &[&[&str]] = &[
    &["guard"],
    &["guard", "--provider", "claude"],
    &["guard", "--provider", "claude", "--delegate-rtk"],
    &["guard", "--provider", "devin"],
    &["guard", "--provider", "zcode"],
    &["guard", "--provider", "antigravity"],
    &["guard", "--provider", "cursor"],
    &[
        "composed-guard",
        "--provider",
        "codex",
        "--backup",
        "/tmp/pixel-backup.json",
    ],
    &["session-start"],
    &["session-start", ".", "--provider", "codex"],
    &["session-start", "--provider", "devin"],
    &["prompt-submit"],
    &["prompt-submit", "--provider", "claude"],
    &["prompt-submit", "--provider", "codex"],
    &["post-compaction", "--provider", "claude"],
    &["post-tool-use", "--provider", "claude"],
    &["metrics", "--provider", "codex"],
    &["metrics", "--provider", "cursor"],
];

/// A retired verb must keep exiting 0 with nothing on stdout: a nonzero exit
/// from a `PreToolUse` hook blocks the host's tool call. The oversized payload
/// proves the verb reads only up to the shared cap before answering, and the
/// `hook` alias is the spelling older settings used for the whole group.
#[test]
fn retired_hook_verbs_should_accept_their_old_flags_and_answer_nothing() {
    let small = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"grep -rn needle src"},"cwd":"/tmp","session_id":"s"}"#;
    let oversized = oversized_payload();
    for verb in RETIRED {
        for group in ["run-hook", "hook"] {
            let mut args = vec![group];
            args.extend_from_slice(verb);
            for payload in [small, "", oversized.as_str()] {
                let output = feed(&args, payload);
                assert!(output.status.success(), "{args:?} must exit 0: {output:?}");
                assert!(
                    output.stdout.is_empty(),
                    "{args:?} must answer nothing: {output:?}"
                );
            }
        }
    }
}
