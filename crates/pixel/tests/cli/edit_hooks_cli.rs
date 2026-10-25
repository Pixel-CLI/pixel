// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel run-hook edit-event`, the optional edit hooks: the card before an
//! edit that strands callers, the note after it, silence when `edit_hooks` is
//! off, and the guarantee every hook gives: whatever goes wrong, exit 0 and
//! nothing on stdout.

use std::io::Write;
use std::path::Path;
use std::process::{Output, Stdio};

use serde_json::{Value, json};

use crate::support::{Scratch, git, pixel_command};

const LIB: &str = "export function alpha(x: number): number { return x + 1; }\n";
const USE: &str =
    "import { alpha } from \"./lib\";\nexport function run(): number { return alpha(1); }\n";

/// An indexed repository: `alpha` in `src/lib.ts`, called by `run` in
/// `src/use.ts`, with `edit_hooks` on.
fn fixture(tag: &str) -> Scratch {
    let dir = Scratch::for_test("edit-hooks-cli", tag);
    git(&dir, &["init", "-q"]);
    std::fs::write(dir.join(".gitignore"), ".pixel/\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.ts"), LIB).unwrap();
    std::fs::write(dir.join("src/use.ts"), USE).unwrap();
    let built = pixel_command()
        .args(["rebuild-graph", "--metrics", "off"])
        .arg(&*dir)
        .output()
        .unwrap();
    assert!(built.status.success(), "{built:?}");
    std::fs::write(dir.join(".pixel/config.yaml"), "edit_hooks: true\n").unwrap();
    dir
}

fn hook_with(
    dir: &Path,
    provider: &str,
    event: &str,
    stdin: &[u8],
    env: &[(&str, &str)],
) -> Output {
    let mut child = pixel_command()
        .args([
            "run-hook",
            "edit-event",
            "--provider",
            provider,
            "--event",
            event,
        ])
        .current_dir(dir)
        .env_remove("PIXEL_EDIT_HOOKS")
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

fn hook(dir: &Path, event: &str, payload: &Value) -> Output {
    hook_with(dir, "claude", event, payload.to_string().as_bytes(), &[])
}

/// A Claude Code `Edit` of `file` replacing `old` with `new`.
fn edit(dir: &Path, file: &str, old: &str, new: &str) -> Value {
    json!({
        "hook_event_name": "PreToolUse", "session_id": "s-1", "cwd": dir,
        "tool_name": "Edit",
        "tool_input": {"file_path": dir.join(file), "old_string": old, "new_string": new},
    })
}

/// What the hook injected, or `None` for the empty stdout of silence.
fn injected(output: &Output, event: &str) -> Option<String> {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    if output.stdout.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["hookSpecificOutput"]["hookEventName"], event);
    Some(
        value["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_owned(),
    )
}

const NEW_LIB: &str = "export function alpha(x: number, y: number): number { return x + y; }\n";

#[test]
fn a_card_should_name_the_callers_a_signature_change_strands_once() {
    let dir = fixture("card");
    let payload = edit(&dir, "src/lib.ts", "(x: number)", "(x: number, y: number)");
    let card = injected(&hook(&dir, "pre-edit", &payload), "PreToolUse").expect("a card");
    assert!(card.starts_with("[PIXEL:EDIT] `alpha`"), "{card}");
    assert!(
        card.contains("was: function alpha(x: number): number"),
        "{card}"
    );
    assert!(
        card.contains("now: function alpha(x: number, y: number): number"),
        "{card}"
    );
    assert!(card.contains("src/use.ts:2 in run"), "{card}");
    assert!(card.len() <= 1200, "{card}");
    // The same change is carded once per session; a body-only change never.
    assert_eq!(
        injected(&hook(&dir, "pre-edit", &payload), "PreToolUse"),
        None
    );
    let body = edit(&dir, "src/lib.ts", "x + 1", "x + 2");
    assert_eq!(injected(&hook(&dir, "pre-edit", &body), "PreToolUse"), None);
}

#[test]
fn the_stale_note_should_name_callers_still_unchanged_and_say_it_once() {
    let dir = fixture("stale");
    let payload = edit(&dir, "src/lib.ts", "(x: number)", "(x: number, y: number)");
    injected(&hook(&dir, "pre-edit", &payload), "PreToolUse").expect("a card");
    std::fs::write(dir.join("src/lib.ts"), NEW_LIB).unwrap();
    let note = injected(&hook(&dir, "post-edit", &payload), "PostToolUse").expect("a note");
    assert!(note.starts_with("[PIXEL:STALE-CALLERS] `alpha`"), "{note}");
    assert!(note.contains("src/use.ts:2 in run"), "{note}");
    assert_eq!(
        injected(&hook(&dir, "post-edit", &payload), "PostToolUse"),
        None
    );
}

#[test]
fn a_caller_updated_before_the_declaration_is_written_should_leave_no_note() {
    let dir = fixture("caller-first");
    let payload = edit(&dir, "src/lib.ts", "(x: number)", "(x: number, y: number)");
    injected(&hook(&dir, "pre-edit", &payload), "PreToolUse").expect("a card");
    std::fs::write(
        dir.join("src/use.ts"),
        USE.replace("alpha(1)", "alpha(1, 2)"),
    )
    .unwrap();
    let caller = edit(&dir, "src/use.ts", "alpha(1)", "alpha(1, 2)");
    assert_eq!(
        injected(&hook(&dir, "post-edit", &caller), "PostToolUse"),
        None
    );
    std::fs::write(dir.join("src/lib.ts"), NEW_LIB).unwrap();
    assert_eq!(
        injected(&hook(&dir, "post-edit", &payload), "PostToolUse"),
        None
    );
}

#[test]
fn an_edit_that_never_landed_should_leave_no_note() {
    let dir = fixture("not-landed");
    let payload = edit(&dir, "src/lib.ts", "(x: number)", "(x: number, y: number)");
    injected(&hook(&dir, "pre-edit", &payload), "PreToolUse").expect("a card");
    assert_eq!(
        injected(&hook(&dir, "post-edit", &payload), "PostToolUse"),
        None
    );
}

#[test]
fn the_switch_off_should_make_both_hooks_silent_and_write_nothing() {
    let dir = fixture("off");
    std::fs::write(dir.join(".pixel/config.yaml"), "edit_hooks: false\n").unwrap();
    let payload = edit(&dir, "src/lib.ts", "(x: number)", "(x: number, y: number)");
    assert_eq!(
        injected(&hook(&dir, "pre-edit", &payload), "PreToolUse"),
        None
    );
    std::fs::write(dir.join("src/lib.ts"), NEW_LIB).unwrap();
    assert_eq!(
        injected(&hook(&dir, "post-edit", &payload), "PostToolUse"),
        None
    );
    assert!(!dir.join(".pixel/edit-hooks").exists());
    // The environment turns them on over the file.
    std::fs::write(dir.join("src/lib.ts"), LIB).unwrap();
    let on = hook_with(
        &dir,
        "claude",
        "pre-edit",
        payload.to_string().as_bytes(),
        &[("PIXEL_EDIT_HOOKS", "1")],
    );
    let card = injected(&on, "PreToolUse").expect("the environment wins over the file");
    assert!(card.contains("src/use.ts:2 in run"), "{card}");
}

#[test]
fn hooks_should_fail_open_on_everything_that_can_go_wrong() {
    let dir = fixture("fail-open");
    let good = edit(&dir, "src/lib.ts", "(x: number)", "(x: number, y: number)");
    let silent = |output: Output, what: &str| {
        assert_eq!(output.status.code(), Some(0), "{what}: {output:?}");
        assert!(output.stdout.is_empty(), "{what}: {output:?}");
    };
    for event in ["pre-edit", "post-edit"] {
        let run = |stdin: &[u8]| hook_with(&dir, "claude", event, stdin, &[]);
        silent(run(b""), "empty");
        silent(run(b"not json"), "text");
        silent(run(&[0xff, 0xfe, 0x00, 0x80]), "binary");
        silent(run(b"{}"), "no tool");
        silent(
            run(b"[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[["),
            "unterminated",
        );
        silent(
            run(json!({"tool_name": "Edit"}).to_string().as_bytes()),
            "no input",
        );
        silent(run(json!({"tool_name": "Read", "cwd": &*dir, "session_id": "s", "tool_input": {"file_path": dir.join("src/lib.ts")}}).to_string().as_bytes()), "not an edit tool");
        silent(run(json!({"tool_name": "Edit", "cwd": &*dir, "session_id": "s", "tool_input": {"file_path": "/etc/hosts", "old_string": "a", "new_string": "b"}}).to_string().as_bytes()), "outside the repository");
        silent(run(json!({"tool_name": "Edit", "cwd": &*dir, "session_id": "s", "tool_input": {"file_path": "../x.ts", "old_string": "a", "new_string": "b"}}).to_string().as_bytes()), "parent path");
        silent(
            hook_with(&dir, "pi", event, good.to_string().as_bytes(), &[]),
            "a host that registers none",
        );
        silent(
            hook_with(
                &dir,
                "claude",
                event,
                good.to_string().as_bytes(),
                &[("PIXEL_HOOK_TEST_PANIC", "edit-event")],
            ),
            "a panic",
        );
    }
    // An argument this binary does not know is clap's exit 2 everywhere else.
    let unknown = hook_with(
        &dir,
        "claude",
        "newer-event",
        good.to_string().as_bytes(),
        &[],
    );
    silent(unknown, "unknown event");

    // A graph that is stale, or missing, gives no card.
    std::fs::write(dir.join("src/use.ts"), format!("{USE}// moved on\n")).unwrap();
    silent(hook(&dir, "pre-edit", &good), "stale graph");
    let unindexed = Scratch::for_test("edit-hooks-cli", "fail-open-unindexed");
    git(&unindexed, &["init", "-q"]);
    std::fs::create_dir_all(unindexed.join(".pixel")).unwrap();
    std::fs::write(unindexed.join(".pixel/config.yaml"), "edit_hooks: true\n").unwrap();
    std::fs::create_dir_all(unindexed.join("src")).unwrap();
    std::fs::write(unindexed.join("src/lib.ts"), LIB).unwrap();
    silent(
        hook(
            &unindexed,
            "pre-edit",
            &edit(
                &unindexed,
                "src/lib.ts",
                "(x: number)",
                "(x: number, y: number)",
            ),
        ),
        "no graph",
    );
}

#[cfg(unix)]
#[test]
fn a_session_store_behind_a_link_should_be_refused_without_failing_the_hook() {
    let dir = fixture("linked-store");
    let elsewhere = Scratch::for_test("edit-hooks-cli", "linked-store-target");
    std::os::unix::fs::symlink(&*elsewhere, dir.join(".pixel/edit-hooks")).unwrap();
    let payload = edit(&dir, "src/lib.ts", "(x: number)", "(x: number, y: number)");
    let out = hook(&dir, "pre-edit", &payload);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(std::fs::read_dir(&*elsewhere).unwrap().count(), 0);
}
