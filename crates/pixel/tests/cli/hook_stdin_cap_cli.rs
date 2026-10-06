// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Every hook entry point reads stdin through one shared cap.
//!
//! `guard`, `prompt-submit`, `post-tool-use`, `metrics` and `task-event` each
//! read the host's payload on a pipe the host owns and then must answer.
//! Before #752 the first four read to EOF, so an oversized payload was
//! allocated in full before any decision. The cap refuses a payload past
//! `MAX_HOOK_INPUT` after `cap + 1` bytes and turns it into the silent exit 0
//! each entry point already uses for a payload it cannot use — the property
//! these tests pin: the native tool proceeds untouched, in every provider's
//! own output shape, with no Pixel decision in it. `task-event` is the one
//! exception: it has no silent path, so an over-cap payload takes its
//! unavailable envelope, which denies `PreToolUse` on an enforced session.

use std::io::Write;
use std::path::Path;
use std::process::{Output, Stdio};

use crate::support::{Scratch, pixel_command};

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

/// The guard must not rewrite, deny or advise on an over-cap payload: the
/// host's own permission decision is the one that has to survive.
#[test]
fn guard_should_leave_the_native_tool_untouched_on_an_oversized_payload() {
    let output = feed(&["run-hook", "guard"], &oversized_payload());
    assert!(output.status.success(), "guard must exit 0: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "",
        "an over-cap guard payload must emit no Pixel decision at all"
    );
}

/// The guard's provider-qualified paths rewrite and deny, so the cap has to
/// hold before the provider rewrite too, not only on the legacy parse below it.
#[test]
fn provider_guard_should_leave_the_native_command_untouched_on_an_oversized_payload() {
    for provider in ["claude", "codex", "devin", "antigravity"] {
        let output = feed(
            &["run-hook", "guard", "--provider", provider],
            &oversized_payload(),
        );
        assert!(
            output.status.success(),
            "guard --provider {provider}: {output:?}"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "",
            "an over-cap payload must not rewrite the native command for {provider}"
        );
    }
}

/// `prompt-submit` injects task context into a prompt. An over-cap payload is
/// a prompt it cannot scope, and a prompt it cannot scope must reach the model
/// exactly as typed.
#[test]
fn prompt_submit_should_leave_the_prompt_untouched_on_an_oversized_payload() {
    let payload = format!(
        r#"{{"session_id":"s","cwd":"/tmp","hook_event_name":"UserPromptSubmit","prompt":"{filler}","transcript_path":"/tmp/t.jsonl"}}"#,
        filler = "a".repeat(MAX_HOOK_INPUT + 1)
    );
    let output = feed(&["run-hook", "prompt-submit"], &payload);
    assert!(output.status.success(), "prompt-submit: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "",
        "an over-cap prompt-submit payload must inject no context"
    );
}

/// `post-tool-use` emits a blast-radius advisory after a write. Skipping it on
/// an over-cap payload is the correct fail-open: the advisory is a note to the
/// next turn, and a note computed from a payload this hook cannot read is a
/// note it must not send.
#[test]
fn post_tool_use_should_emit_no_advisory_on_an_oversized_payload() {
    let payload = format!(
        r#"{{"session_id":"s","cwd":"/tmp","hook_event_name":"PostToolUse","tool_name":"Edit","tool_input":{{"file_path":"{filler}","old_string":"a","new_string":"b"}},"tool_response":{{"success":true}}}}"#,
        filler = "a".repeat(MAX_HOOK_INPUT + 1)
    );
    let output = feed(&["run-hook", "post-tool-use"], &payload);
    assert!(output.status.success(), "post-tool-use: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "",
        "an over-cap post-tool-use payload must emit no blast-radius advisory"
    );
}

/// `metrics` replays the action log's line as `additionalContext`. An over-cap
/// payload must not become a metric line in a transcript Pixel did not measure.
#[test]
fn metrics_should_emit_no_line_on_an_oversized_payload() {
    let payload = format!(
        r#"{{"session_id":"s","cwd":"/tmp","hook_event_name":"PostToolUse","tool_name":"Edit","tool_input":{{"file_path":"/tmp/x.rs"}},"tool_response":{{"success":true}},"toolUseID":"{filler}","toolUseResult":{{"success":true}}}}"#,
        filler = "a".repeat(MAX_HOOK_INPUT + 1)
    );
    for provider in ["claude", "devin"] {
        let output = feed(&["run-hook", "metrics", "--provider", provider], &payload);
        assert!(
            output.status.success(),
            "metrics --provider {provider}: {output:?}"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "",
            "an over-cap metrics payload must relay no line for {provider}"
        );
    }
}

/// A directory with a `.pixel` marker, which is all the guard's Grep advisory
/// needs to consider the repo indexed (`find_up(cwd, ".pixel")`); no git repo
/// and no built index are required.
fn dot_pixel_dir(tag: &str) -> Scratch {
    let dir = Scratch::for_test("pixel-hook-cap", tag);
    std::fs::create_dir_all(dir.join(".pixel")).unwrap();
    dir
}

/// A guard payload the hook *answers*: a Grep call carrying a filter Pixel
/// search cannot preserve (`output_mode`), in `cwd`. The guard's advisory is
/// the observable a read produces — a payload the cap refused emits nothing —
/// and it does not depend on the pattern, so `padding` can size the payload
/// without changing the outcome.
fn grep_payload(cwd: &Path, padding: usize) -> String {
    serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Grep",
        "tool_input": {"pattern": "a".repeat(padding), "output_mode": "content"},
        "cwd": cwd.to_str().unwrap(),
    })
    .to_string()
}

/// A git repo with a `.pixel/base.shard` and a source file, which is all the
/// Pixel-first guidance keys off — no built index required.
fn indexed_dir(tag: &str) -> Scratch {
    let dir = Scratch::for_test("pixel-hook-cap", tag);
    crate::support::git(&dir, &["init", "-q"]);
    std::fs::create_dir_all(dir.join(".pixel")).unwrap();
    std::fs::write(dir.join(".pixel/base.shard"), "").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "fn needle() {}\n").unwrap();
    dir
}

/// A `prompt-submit` payload the hook *answers*: the prompt asks about code
/// (so the guidance rides) and carries `padding` bytes of filler, so the size
/// is exact and the answer does not depend on it.
fn prompt_payload(cwd: &Path, padding: usize) -> String {
    let prompt = format!(
        "find the caller of saved and explain its behavior {}",
        "a".repeat(padding)
    );
    serde_json::json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": prompt,
        "cwd": cwd.to_str().unwrap(),
    })
    .to_string()
}

/// A payload at or under the cap is still *read and answered*: the cap must
/// reject only what is over it, or a real host with a large payload loses
/// every hook silently. Asserting the guard's advisory — not just exit 0,
/// which a refused payload also returns — is what separates a read from a
/// refusal.
#[test]
fn a_payload_under_the_cap_should_still_reach_the_hook() {
    let repo = dot_pixel_dir("under-cap");

    // A small payload is read and answered.
    let small = grep_payload(&repo, 1);
    let output = feed(&["run-hook", "guard"], &small);
    assert!(output.status.success(), "guard under cap: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("pixel-guard advisory"),
        "an under-cap payload must be read and answered, not silently dropped: {stdout}"
    );

    // The boundary is the cap, not a smaller limit: a payload of exactly
    // `MAX_HOOK_INPUT` bytes is read and answered too. The padding is computed
    // from the cap minus the envelope, so the size is exact rather than
    // "roughly one kibibyte under".
    let envelope = grep_payload(&repo, 0).len();
    let at_cap = grep_payload(&repo, MAX_HOOK_INPUT - envelope);
    assert_eq!(
        at_cap.len(),
        MAX_HOOK_INPUT,
        "the boundary payload must be exactly the cap"
    );
    let output = feed(&["run-hook", "guard"], &at_cap);
    assert!(output.status.success(), "guard at cap: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("pixel-guard advisory"),
        "a payload exactly at the cap must still be read: {stdout}"
    );
}

/// `prompt-submit` answers an indexed repository's prompt with Pixel-first
/// guidance, and it must keep doing so at the cap: the guidance is the
/// observable a read produces, since a payload the cap refused exits 0 with
/// no output at all. Both a small payload and one sized to exactly the cap
/// are checked, so the boundary is the cap and not some smaller limit.
#[test]
fn prompt_submit_should_still_inject_its_guidance_at_the_cap() {
    let dir = indexed_dir("prompt-under-cap");

    let small = prompt_payload(&dir, 1);
    let output = feed(
        &["run-hook", "prompt-submit", "--provider", "devin"],
        &small,
    );
    assert!(
        output.status.success(),
        "prompt-submit under cap: {output:?}"
    );
    let response: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("a read payload answers with JSON");
    let context = response["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("an indexed repository's prompt must be answered with guidance");
    assert!(
        context.contains("Pixel-first retrieval"),
        "an under-cap prompt-submit payload must be read and answered: {context}"
    );

    let envelope = prompt_payload(&dir, 0).len();
    let at_cap = prompt_payload(&dir, MAX_HOOK_INPUT - envelope);
    assert_eq!(
        at_cap.len(),
        MAX_HOOK_INPUT,
        "the boundary payload must be exactly the cap"
    );
    let output = feed(
        &["run-hook", "prompt-submit", "--provider", "devin"],
        &at_cap,
    );
    assert!(output.status.success(), "prompt-submit at cap: {output:?}");
    let response: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("a read payload answers with JSON");
    let context = response["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("an indexed repository's prompt must be answered with guidance");
    assert!(
        context.contains("Pixel-first retrieval"),
        "a payload exactly at the cap must still be read and answered: {context}"
    );
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

/// The cap is shared, so every entry point refuses the same payload. A reader
/// that kept its own private limit would drift from the others, and one host
/// would then be measured against a bound the rest of Pixel does not hold to.
/// This does not by itself prove the cap is shared — a private 1 MiB limit per
/// entry point would pass it — `the_shared_cap_should_equal_the_task_event_cap`
/// is what proves that.
#[test]
fn every_entry_point_should_refuse_the_same_oversized_payload() {
    let payload = oversized_payload();
    for (entry, args) in [
        ("guard", vec!["run-hook", "guard"]),
        ("post-tool-use", vec!["run-hook", "post-tool-use"]),
        ("metrics", vec!["run-hook", "metrics"]),
    ] {
        let output = feed(&args, &payload);
        assert!(
            output.status.success() && output.stdout.is_empty(),
            "{entry} must share the cap and fail open: {output:?}"
        );
    }
}
