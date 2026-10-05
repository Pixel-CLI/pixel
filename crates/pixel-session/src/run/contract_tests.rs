// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for `sniper run`: how a wrapped command is classified,
//! and what the session store records for a pass, a TypeScript failure
//! and an unrecognised failure.

use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// A scratch directory removed on drop (the crate has no `tempfile`).
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Scratch {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "pixel-session-run-contract-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_owned()).collect()
}

/// A shell script named `name` in its own directory, so classification by
/// program name can be exercised with a real child process.
fn script(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

fn store() -> (Scratch, Scratch, Store) {
    let project = Scratch::new();
    let state = Scratch::new();
    let store = Store::open_at(project.path(), state.path()).unwrap();
    (project, state, store)
}

/// Test runners are recognised by name or path, `cargo`/`bun` only with a
/// `test` subcommand; everything else is a build.
#[test]
fn classify_should_recognise_test_runners_by_name_path_and_subcommand() {
    assert_eq!(
        classify(&argv(&["python", "-m", "pytest"])),
        CommandClass::Test
    );
    assert_eq!(
        classify(&argv(&["/usr/local/bin/jest", "--ci"])),
        CommandClass::Test
    );
    assert_eq!(
        classify(&argv(&["/home/u/.cargo/bin/cargo", "test"])),
        CommandClass::Test
    );
    assert_eq!(
        classify(&argv(&["/opt/homebrew/bin/bun", "test"])),
        CommandClass::Test
    );
    assert_eq!(classify(&argv(&["cargo", "build"])), CommandClass::Build);
    assert_eq!(
        classify(&argv(&["bun", "run", "test"])),
        CommandClass::Build
    );
    assert_eq!(classify(&argv(&["make", "test"])), CommandClass::Build);
    assert_eq!(classify(&argv(&["vitest-like"])), CommandClass::Build);
}

/// No program is a usage error, before anything is spawned or recorded.
#[test]
fn run_wrapped_should_refuse_an_empty_command() {
    let (_p, _s, store) = store();
    assert_eq!(
        run_wrapped(&store, None, &[]),
        Err("no command given (usage: sniper run [--label name] -- <cmd> [args...])".to_string())
    );
}

/// A program that cannot be started is an error naming it.
#[test]
fn run_wrapped_should_report_a_program_that_cannot_start() {
    let (_p, _s, store) = store();
    let err = run_wrapped(&store, None, &argv(&["/nonexistent/pixel-no-such-binary"])).unwrap_err();
    assert!(
        err.starts_with("spawn /nonexistent/pixel-no-such-binary:"),
        "{err}"
    );
}

/// An unrecognised failure records one run-wrapper error with the exit
/// code and the tail of stdout then stderr, and returns that exit code.
#[test]
fn run_wrapped_should_record_a_generic_failure_with_both_streams() {
    let (_p, _s, store) = store();
    let code = run_wrapped(
        &store,
        Some("lint"),
        &argv(&["sh", "-c", "echo to-stdout; echo to-stderr >&2; exit 3"]),
    )
    .unwrap();
    assert_eq!(code, 3);
    let errors = store.last_errors(10, Some(Surface::RunWrapper)).unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].message, "lint exited 3");
    assert_eq!(errors[0].kind.as_deref(), Some("exit-3"));
    let tail = errors[0].extra.as_ref().unwrap()["tail"].to_string();
    assert!(
        tail.contains("to-stdout") && tail.contains("to-stderr"),
        "{tail}"
    );
    assert!(errors[0].run_id.as_deref().unwrap().starts_with("run-"));
}

/// A failure printed on stderr only is still recorded whole.
#[test]
fn run_wrapped_should_record_stderr_only_output() {
    let (_p, _s, store) = store();
    run_wrapped(
        &store,
        None,
        &argv(&["sh", "-c", "echo only-err >&2; exit 1"]),
    )
    .unwrap();
    let errors = store.last_errors(10, Some(Surface::RunWrapper)).unwrap();
    assert_eq!(
        errors[0].message, "sh exited 1",
        "the program is the default label"
    );
    let tail = errors[0].extra.as_ref().unwrap()["tail"].to_string();
    assert!(tail.contains("only-err"), "{tail}");
}

/// tsc diagnostics on stderr (stdout empty) are parsed into per-code
/// errors and a summary, not a generic failure.
#[test]
fn run_wrapped_should_parse_tsc_diagnostics_from_stderr_when_stdout_has_none() {
    let bin = Scratch::new();
    let tsc = script(
        bin.path(),
        "tsc",
        "echo \"src/a.ts(3,5): error TS2322: Type 'x' is not assignable.\" >&2; exit 2",
    );
    let (_p, _s, store) = store();
    assert_eq!(
        run_wrapped(&store, Some("types"), &argv(&[&tsc])).unwrap(),
        2
    );
    let errors = store.last_errors(10, Some(Surface::Tsc)).unwrap();
    let kinds: Vec<Option<String>> = errors.iter().map(|e| e.kind.clone()).collect();
    assert!(kinds.contains(&Some("TS2322".to_string())), "{kinds:?}");
    assert!(kinds.contains(&Some("summary".to_string())), "{kinds:?}");
    assert!(
        store
            .last_errors(10, Some(Surface::RunWrapper))
            .unwrap()
            .is_empty(),
        "parsed output is not also a generic failure"
    );
}

/// A passing test runner records a test-pass event; a passing build a
/// build-ok event; both carry the label and argv.
#[test]
fn run_wrapped_should_record_the_pass_event_for_the_command_class() {
    let bin = Scratch::new();
    let cargo = script(bin.path(), "cargo", "exit 0");
    let (_p, _s, store) = store();
    assert_eq!(
        run_wrapped(&store, Some("unit"), &argv(&[&cargo, "test"])).unwrap(),
        0
    );
    assert_eq!(
        run_wrapped(&store, None, &argv(&["sh", "-c", "true"])).unwrap(),
        0
    );
    let events = store.events_between(0, i64::MAX).unwrap();
    let kinds: Vec<(EventKind, String)> = events
        .iter()
        .map(|e| {
            (
                e.kind,
                e.data.as_ref().unwrap()["label"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            (EventKind::TestPass, "unit".to_string()),
            (EventKind::BuildOk, "sh".to_string())
        ]
    );
    assert_eq!(events[0].data.as_ref().unwrap()["argv"][1], "test");
}
