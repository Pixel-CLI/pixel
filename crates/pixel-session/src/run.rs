// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `sniper run -- <cmd>`: spawn a command, tee its output through live, and
//! on failure turn the captured output into structured error records — tsc
//! diagnostics parsed per TS code, Minitest/RSpec failures one record per
//! test, RuboCop offenses one record per offense, everything else a generic
//! tail record. A green Minitest/RSpec run records a `test-pass` event with
//! the run counters, like the vitest reporter does.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::parsers::ruby::TestFailure;
use crate::parsers::{generic, minitest, rspec, rubocop, ruby, tsc};
use crate::store::{Store, now_ms};
use crate::types::{ErrorInput, EventInput, EventKind, Frame, RunInput, Surface};

const TAIL_LINES: usize = 100;

/// How a wrapped command's success/failure is classified from its argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandClass {
    Tsc,
    Test,
    Build,
}

/// Sniff argv: `tsc` anywhere → Tsc; vitest / `cargo test` / `bun test` /
/// jest / pytest → Test; everything else → Build.
pub fn classify(argv: &[String]) -> CommandClass {
    let has = |needle: &str| {
        argv.iter().any(|a| {
            Path::new(a)
                .file_name()
                .is_some_and(|f| f.to_string_lossy() == needle)
                || a == needle
        })
    };
    if has("tsc") {
        return CommandClass::Tsc;
    }
    if has("vitest") || has("jest") || has("pytest") {
        return CommandClass::Test;
    }
    for pair in argv.windows(2) {
        if (pair[0].ends_with("cargo")
            || pair[0] == "cargo"
            || pair[0].ends_with("bun")
            || pair[0] == "bun")
            && pair[1] == "test"
        {
            return CommandClass::Test;
        }
    }
    CommandClass::Build
}

fn sha256_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let digest = Sha256::digest(&bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    Some(hex)
}

/// HEAD of the repository under `root` for the run record; `None` outside
/// a repository or before the first commit.
fn git_head(root: &Path) -> Option<String> {
    pixel_git::GitRunner::new(root).rev_parse_head()
}

/// Programs whose run depends on the Ruby bundle, by file name.
const RUBY_PROGRAMS: &[&str] = &[
    "ruby",
    "bundle",
    "bundler",
    "rake",
    "rails",
    "rspec",
    "rubocop",
    "standardrb",
];

/// True iff `argv` runs a Ruby tool: its program, or the command `bundle
/// exec`/`bin/rails`-style wrappers run, is one of [`RUBY_PROGRAMS`].
pub fn is_ruby_command(argv: &[String]) -> bool {
    argv.iter().take(3).any(|a| {
        Path::new(a)
            .file_name()
            .is_some_and(|f| RUBY_PROGRAMS.contains(&f.to_string_lossy().as_ref()))
    })
}

/// The hash of the lockfile a run's dependencies come from: `Gemfile.lock`
/// first for a Ruby tool (a Rails app also carries a JavaScript lockfile),
/// otherwise the JavaScript and Rust lockfiles in their historical order,
/// with `Gemfile.lock` last so a project that has one of those keeps it.
fn lockfile_hash(root: &Path, ruby: bool) -> Option<String> {
    const LOCKFILES: [&str; 7] = [
        "bun.lock",
        "bun.lockb",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "Cargo.lock",
        "Gemfile.lock",
    ];
    let ruby_first = ruby
        .then(|| sha256_file(&root.join("Gemfile.lock")))
        .flatten();
    ruby_first.or_else(|| {
        LOCKFILES
            .iter()
            .find_map(|name| sha256_file(&root.join(name)))
    })
}

/// Tee one child stream to one of our streams while capturing it. Chunked,
/// so output stays live rather than buffered-then-dumped.
fn tee<R: Read + Send + 'static, W: Write + Send + 'static>(
    mut from: R,
    mut to: W,
    captured: Arc<Mutex<Vec<u8>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = to.write_all(&buf[..n]);
                    let _ = to.flush();
                    if let Ok(mut captured) = captured.lock() {
                        captured.extend_from_slice(&buf[..n]);
                    }
                }
            }
        }
    })
}

fn record_tsc_failure(
    store: &Store,
    run_id: &str,
    label: &str,
    output: &str,
    exit_code: i32,
) -> Result<bool, String> {
    let errors = tsc::parse(output);
    if errors.is_empty() {
        return Ok(false);
    }
    for (code, group) in tsc::group_by_code(&errors) {
        let locations: Vec<String> = group
            .iter()
            .take(50)
            .map(|e| format!("{}:{}:{}", e.file, e.line, e.column))
            .collect();
        store
            .record_error(&ErrorInput {
                surface: Surface::Tsc,
                message: group[0].message.clone(),
                kind: Some(code.clone()),
                stack_raw: None,
                frames: None,
                values: None,
                http: None,
                extra: Some(json!({"count": group.len(), "locations": locations})),
                run_id: Some(run_id.to_owned()),
                ts: None,
            })
            .map_err(|e| e.to_string())?;
    }
    let codes: serde_json::Map<String, serde_json::Value> = tsc::group_by_code(&errors)
        .into_iter()
        .map(|(code, group)| (code, json!(group.len())))
        .collect();
    store
        .record_error(&ErrorInput {
            surface: Surface::Tsc,
            message: tsc::summary_message(&errors),
            kind: Some("summary".into()),
            stack_raw: None,
            frames: None,
            values: None,
            http: None,
            extra: Some(json!({
                "total": errors.len(),
                "codes": codes,
                "label": label,
                "exitCode": exit_code,
            })),
            run_id: Some(run_id.to_owned()),
            ts: None,
        })
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// Failures listed in a test run's summary record, at most this many.
const SUMMARY_FAILURE_CAP: usize = 50;

/// The Ruby tool whose output was recognised, with what it reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RubyReport {
    Minitest(minitest::Report),
    Rspec(rspec::Report),
    Rubocop(rubocop::Report),
}

/// Sniff the captured output for a Ruby tool. A JSON formatter document
/// (RuboCop's, then RSpec's) is the most precise record and goes first;
/// then the text formats: RuboCop's offense grammar is the most specific,
/// and Minitest's `# Running:` banner beats RSpec's summary (a Rails app
/// printing both is a Minitest run). A truncated or malformed JSON
/// document is no document: the text parsers read the output instead.
pub fn detect_ruby(output: &str) -> Option<RubyReport> {
    if let Some(report) = rubocop::parse_json(output) {
        return Some(RubyReport::Rubocop(report));
    }
    if let Some(report) = rspec::parse_json(output) {
        return Some(RubyReport::Rspec(report));
    }
    if rubocop::detect(output) {
        return Some(RubyReport::Rubocop(rubocop::parse(output)));
    }
    if minitest::detect(output) {
        return Some(RubyReport::Minitest(minitest::parse(output)));
    }
    if rspec::detect(output) {
        return Some(RubyReport::Rspec(rspec::parse(output)));
    }
    None
}

/// The `test-pass` event payload for a green Ruby test run; `None` when the
/// output carried no summary line (a run that never reached its end) or the
/// summary itself reports failures.
pub fn ruby_pass_data(report: &RubyReport) -> Option<serde_json::Value> {
    match report {
        RubyReport::Minitest(report) => {
            let c = report.counters.filter(minitest::Counters::green)?;
            Some(json!({
                "runner": "minitest",
                "runs": c.runs,
                "assertions": c.assertions,
                "failures": c.failures,
                "errors": c.errors,
                "skips": c.skips,
                "passed": c.passed(),
            }))
        }
        RubyReport::Rspec(report) => {
            let c = report.counters.filter(rspec::Counters::green)?;
            Some(json!({
                "runner": "rspec",
                "examples": c.examples,
                "failures": c.failures,
                "pending": c.pending,
                "passed": c.passed(),
            }))
        }
        RubyReport::Rubocop(_) => None,
    }
}

/// True iff a test runner's report says tests failed: a failure record, or
/// a summary that is not green. A green process exit does not make such a
/// run a pass (`rspec || true`, a wrapper that drops the status).
pub fn ruby_tests_failed(report: &RubyReport) -> bool {
    match report {
        RubyReport::Minitest(r) => !r.failures.is_empty() || r.counters.is_some_and(|c| !c.green()),
        RubyReport::Rspec(r) => !r.failures.is_empty() || r.counters.is_some_and(|c| !c.green()),
        RubyReport::Rubocop(_) => false,
    }
}

/// `Class#test: first message line` — distinct per test so dedup keeps one
/// row per failing test, not one per identical assertion message.
pub fn failure_headline(failure: &TestFailure) -> String {
    let first = failure.message.lines().next().unwrap_or("");
    match &failure.test_class {
        Some(class) if !failure.test_name.starts_with(class.as_str()) => {
            format!("{class}#{}: {first}", failure.test_name)
        }
        _ => format!("{}: {first}", failure.test_name),
    }
}

/// The `ErrorInput` for one failing test.
pub fn failure_input(surface: Surface, failure: &TestFailure, run_id: &str) -> ErrorInput {
    let frames: Vec<Frame> = failure
        .backtrace
        .iter()
        .filter_map(|l| ruby::parse_frame(l))
        .collect();
    let mut extra = json!({
        "testClass": failure.test_class,
        "testName": failure.test_name,
        "file": failure.file,
        "line": failure.line,
        "message": failure.message,
        "rerun": failure.rerun,
    });
    if let Some(expected) = &failure.expected {
        extra["expected"] = json!(expected);
    }
    if let Some(actual) = &failure.actual {
        extra["actual"] = json!(actual);
    }
    let location_frame = match (&failure.file, failure.line) {
        (Some(file), Some(line)) if frames.is_empty() => Some(Frame {
            raw: format!("{file}:{line}"),
            file: Some(file.clone()),
            line: Some(line),
            ..Frame::default()
        }),
        _ => None,
    };
    let frames: Vec<Frame> = location_frame.into_iter().chain(frames).collect();
    ErrorInput {
        surface,
        message: failure_headline(failure),
        kind: Some(failure.kind.as_str().to_owned()),
        stack_raw: (!failure.backtrace.is_empty()).then(|| failure.backtrace.join("\n")),
        frames: (!frames.is_empty()).then_some(frames),
        values: None,
        http: None,
        extra: Some(extra),
        run_id: Some(run_id.to_owned()),
        ts: None,
    }
}

/// The `ErrorInput` for one RuboCop offense.
pub fn offense_input(offense: &rubocop::Offense, run_id: &str) -> ErrorInput {
    ErrorInput {
        surface: Surface::Rubocop,
        message: format!("{}: {}", offense.cop, offense.message),
        kind: Some("lint".into()),
        stack_raw: None,
        frames: Some(vec![Frame {
            raw: format!("{}:{}:{}", offense.file, offense.line, offense.column),
            file: Some(offense.file.clone()),
            line: Some(offense.line),
            column: Some(offense.column),
            ..Frame::default()
        }]),
        values: None,
        http: None,
        extra: Some(json!({
            "file": offense.file,
            "line": offense.line,
            "column": offense.column,
            "severity": offense.severity,
            "cop": offense.cop,
            "correctable": offense.correctable,
            "corrected": offense.corrected,
        })),
        run_id: Some(run_id.to_owned()),
        ts: None,
    }
}

/// The per-run summary record (`kind: summary`) that `sniper test` shows:
/// the runner's own summary line as message, counters and the failing
/// tests (capped) in `extra`.
pub fn summary_input(
    surface: Surface,
    summary_line: &str,
    counters: serde_json::Value,
    failures: &[TestFailure],
    label: &str,
    exit_code: i32,
    run_id: &str,
) -> ErrorInput {
    let listed: Vec<serde_json::Value> = failures
        .iter()
        .take(SUMMARY_FAILURE_CAP)
        .map(|f| {
            json!({
                "kind": f.kind.as_str(),
                "test": failure_headline(f),
                "file": f.file,
                "line": f.line,
                "rerun": f.rerun,
            })
        })
        .collect();
    let mut extra = json!({
        "counters": counters,
        "failures": listed,
        "label": label,
        "exitCode": exit_code,
    });
    if failures.len() > SUMMARY_FAILURE_CAP {
        extra["truncatedCount"] = json!(failures.len() - SUMMARY_FAILURE_CAP);
    }
    ErrorInput {
        surface,
        message: summary_line.to_owned(),
        kind: Some("summary".into()),
        stack_raw: None,
        frames: None,
        values: None,
        http: None,
        extra: Some(extra),
        run_id: Some(run_id.to_owned()),
        ts: None,
    }
}

/// `12 runs, 10 assertions, 1 failures, 1 errors, 0 skips` — the summary
/// message when the runner printed one, else a count of what was parsed.
pub fn minitest_summary_line(report: &minitest::Report) -> (String, serde_json::Value) {
    match report.counters {
        Some(c) => (
            format!(
                "{} runs, {} assertions, {} failures, {} errors, {} skips",
                c.runs, c.assertions, c.failures, c.errors, c.skips
            ),
            json!({
                "runs": c.runs,
                "assertions": c.assertions,
                "failures": c.failures,
                "errors": c.errors,
                "skips": c.skips,
            }),
        ),
        None => (
            format!("{} failing tests (no summary line)", report.failures.len()),
            serde_json::Value::Null,
        ),
    }
}

/// `7 examples, 2 failures, 1 pending` (pending shown only when non-zero,
/// as RSpec does).
pub fn rspec_summary_line(report: &rspec::Report) -> (String, serde_json::Value) {
    match report.counters {
        Some(c) => {
            let mut line = format!("{} examples, {} failures", c.examples, c.failures);
            if c.pending > 0 {
                line.push_str(&format!(", {} pending", c.pending));
            }
            (
                line,
                json!({
                    "examples": c.examples,
                    "failures": c.failures,
                    "pending": c.pending,
                }),
            )
        }
        None => (
            format!(
                "{} failing examples (no summary line)",
                report.failures.len()
            ),
            serde_json::Value::Null,
        ),
    }
}

/// Record a recognised Ruby tool's failures. Returns `false` when the tool
/// was recognised but reported nothing to record (a crash before the first
/// test, a RuboCop run that failed for another reason), so the caller falls
/// back to the generic tail record.
pub fn record_ruby_failure(
    store: &Store,
    run_id: &str,
    label: &str,
    report: &RubyReport,
    exit_code: i32,
) -> Result<bool, String> {
    let (surface, failures, summary) = match report {
        RubyReport::Rubocop(report) => {
            // An offense `--autocorrect` fixed in this run is not left in
            // the file.
            let remaining: Vec<&rubocop::Offense> =
                report.offenses.iter().filter(|o| !o.corrected).collect();
            if remaining.is_empty() {
                return Ok(false);
            }
            for offense in remaining {
                store
                    .record_error(&offense_input(offense, run_id))
                    .map_err(|e| e.to_string())?;
            }
            return Ok(true);
        }
        RubyReport::Minitest(report) => (
            Surface::Minitest,
            &report.failures,
            minitest_summary_line(report),
        ),
        RubyReport::Rspec(report) => (Surface::Rspec, &report.failures, rspec_summary_line(report)),
    };
    if failures.is_empty() {
        return Ok(false);
    }
    for failure in failures {
        store
            .record_error(&failure_input(surface, failure, run_id))
            .map_err(|e| e.to_string())?;
    }
    // Recorded last so it is the newest row of its surface: `sniper test`
    // shows the run, `sniper last` the individual failures above it.
    let (line, counters) = summary;
    store
        .record_error(&summary_input(
            surface, &line, counters, failures, label, exit_code, run_id,
        ))
        .map_err(|e| e.to_string())?;
    Ok(true)
}

fn record_generic_failure(
    store: &Store,
    run_id: &str,
    label: &str,
    output: &str,
    exit_code: i32,
) -> Result<(), String> {
    store
        .record_error(&ErrorInput {
            surface: Surface::RunWrapper,
            message: format!("{label} exited {exit_code}"),
            kind: Some(format!("exit-{exit_code}")),
            stack_raw: None,
            frames: None,
            values: None,
            http: None,
            extra: Some(json!({"tail": generic::tail_lines(output, TAIL_LINES)})),
            run_id: Some(run_id.to_owned()),
            ts: None,
        })
        .map_err(|e| e.to_string())?;
    store
        .record_raw_fallback(&format!("run:{label}"), output, None)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Spawn `argv`, tee output live, record the outcome. Returns the wrapped
/// command's exit code (which the CLI mirrors).
pub fn run_wrapped(store: &Store, label: Option<&str>, argv: &[String]) -> Result<i32, String> {
    let Some(program) = argv.first() else {
        return Err(
            "no command given (usage: sniper run [--label name] -- <cmd> [args...])".into(),
        );
    };
    let label = label.unwrap_or(program).to_owned();
    let class = classify(argv);
    let started = Instant::now();

    let mut child = Command::new(program)
        .args(&argv[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {program}: {e}"))?;

    let run_id = format!("run-{:x}-{}", now_ms(), child.id());
    let root = store.project_root().to_path_buf();
    store
        .record_run(&RunInput {
            run_id: run_id.clone(),
            pid: Some(i64::from(child.id())),
            port: None,
            git_head: git_head(&root),
            lockfile_hash: lockfile_hash(&root, is_ruby_command(argv)),
            vite_dep_hash: None,
            fingerprint: Some(json!({
                "kind": "sniper-run",
                "argv": argv,
                "label": label,
            })),
            changed_since_last_run: None,
            ts: None,
        })
        .map_err(|e| e.to_string())?;

    let out_buf = Arc::new(Mutex::new(Vec::new()));
    let err_buf = Arc::new(Mutex::new(Vec::new()));
    let out_thread = child
        .stdout
        .take()
        .map(|stdout| tee(stdout, std::io::stdout(), out_buf.clone()));
    let err_thread = child
        .stderr
        .take()
        .map(|stderr| tee(stderr, std::io::stderr(), err_buf.clone()));

    let status = child.wait().map_err(|e| format!("wait {program}: {e}"))?;
    for thread in [out_thread, err_thread].into_iter().flatten() {
        let _ = thread.join();
    }
    let duration_ms = started.elapsed().as_millis() as i64;
    let exit_code = status.code().unwrap_or(1);

    let stdout_text = String::from_utf8_lossy(&out_buf.lock().unwrap()).into_owned();
    let stderr_text = String::from_utf8_lossy(&err_buf.lock().unwrap()).into_owned();

    let combined = if stderr_text.is_empty() {
        stdout_text.clone()
    } else if stdout_text.is_empty() {
        stderr_text.clone()
    } else {
        format!("{stdout_text}\n{stderr_text}")
    };
    let ruby = match class {
        CommandClass::Tsc => None,
        CommandClass::Test | CommandClass::Build => detect_ruby(&combined),
    };

    // A green exit is a pass unless the test runner itself reported
    // failures; the exit code the wrapper returns is the child's either way.
    let failed_tests = ruby.as_ref().is_some_and(ruby_tests_failed);
    if exit_code == 0 && !failed_tests {
        let ruby_pass = ruby.as_ref().and_then(ruby_pass_data);
        let kind = match (class, &ruby_pass) {
            (_, Some(_)) | (CommandClass::Test, None) => EventKind::TestPass,
            (CommandClass::Tsc | CommandClass::Build, None) => EventKind::BuildOk,
        };
        let mut data = json!({
            "durationMs": duration_ms,
            "argv": argv,
            "label": label,
        });
        if let Some(counters) = ruby_pass
            && let (Some(into), Some(from)) = (data.as_object_mut(), counters.as_object())
        {
            for (key, value) in from {
                into.insert(key.clone(), value.clone());
            }
        }
        store
            .record_event(&EventInput {
                kind,
                data: Some(data),
                run_id: Some(run_id),
                ts: None,
            })
            .map_err(|e| e.to_string())?;
        return Ok(0);
    }

    let parsed = match class {
        // tsc prints diagnostics on stdout; fall back to combined output.
        CommandClass::Tsc => {
            record_tsc_failure(store, &run_id, &label, &stdout_text, exit_code)?
                || record_tsc_failure(store, &run_id, &label, &stderr_text, exit_code)?
        }
        // vitest reports through its reporter; Ruby tools are sniffed from
        // the output itself (`bundle exec rails test` looks like a build).
        CommandClass::Test | CommandClass::Build => match &ruby {
            Some(report) => record_ruby_failure(store, &run_id, &label, report, exit_code)?,
            None => false,
        },
    };
    if parsed && ruby.is_some() {
        // The structured records are a reading of the output; the output
        // itself stays recoverable (a late `rake aborted!`, a deprecation).
        store
            .record_raw_fallback(&format!("run:{label}"), &combined, None)
            .map_err(|e| e.to_string())?;
    } else if !parsed {
        record_generic_failure(store, &run_id, &label, &combined, exit_code)?;
    }
    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_head_is_the_full_oid_of_head_and_none_without_a_commit() {
        let root =
            std::env::temp_dir().join(format!("pixel-session-githead-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(git_head(&root), None, "not a repository");

        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        assert_eq!(git_head(&root), None, "no commit yet");
        std::fs::write(root.join("a.txt"), b"a\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "first"]);
        let head = git_head(&root).expect("HEAD after a commit");
        assert_eq!(head.len(), 40, "{head}");
        assert!(head.bytes().all(|b| b.is_ascii_hexdigit()), "{head}");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn classify_tsc() {
        assert_eq!(classify(&argv(&["tsc", "--noEmit"])), CommandClass::Tsc);
        assert_eq!(
            classify(&argv(&["bunx", "tsc", "--noEmit", "--pretty", "false"])),
            CommandClass::Tsc
        );
        assert_eq!(
            classify(&argv(&["node_modules/.bin/tsc"])),
            CommandClass::Tsc
        );
    }

    #[test]
    fn classify_test_runners() {
        assert_eq!(classify(&argv(&["vitest", "run"])), CommandClass::Test);
        assert_eq!(classify(&argv(&["bunx", "vitest"])), CommandClass::Test);
        assert_eq!(classify(&argv(&["cargo", "test"])), CommandClass::Test);
        assert_eq!(classify(&argv(&["bun", "test"])), CommandClass::Test);
        assert_eq!(classify(&argv(&["jest"])), CommandClass::Test);
    }

    fn failure(n: usize) -> TestFailure {
        TestFailure {
            kind: ruby::FailureKind::Failure,
            test_class: Some("T".into()),
            test_name: format!("test_{n}"),
            file: Some("t.rb".into()),
            line: Some(1),
            message: "boom".into(),
            expected: None,
            actual: None,
            backtrace: vec![],
            rerun: None,
        }
    }

    #[test]
    fn summary_lists_at_most_fifty_failures_and_counts_the_rest() {
        let fifty: Vec<TestFailure> = (0..50).map(failure).collect();
        let input = summary_input(Surface::Minitest, "s", json!({}), &fifty, "l", 1, "r");
        let extra = input.extra.unwrap();
        assert_eq!(extra["failures"].as_array().unwrap().len(), 50);
        assert!(extra.get("truncatedCount").is_none());

        let fifty_one: Vec<TestFailure> = (0..51).map(failure).collect();
        let input = summary_input(Surface::Minitest, "s", json!({}), &fifty_one, "l", 1, "r");
        let extra = input.extra.unwrap();
        assert_eq!(extra["failures"].as_array().unwrap().len(), 50);
        assert_eq!(extra["truncatedCount"], 1);
        assert_eq!(extra["failures"][0]["test"], "T#test_0: boom");
    }

    #[test]
    fn failure_without_backtrace_gets_a_location_frame() {
        let input = failure_input(Surface::Minitest, &failure(1), "r");
        let frames = input.frames.unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].raw, "t.rb:1");
        assert_eq!(frames[0].file.as_deref(), Some("t.rb"));
        assert_eq!(frames[0].line, Some(1));
        assert_eq!(input.stack_raw, None);
    }

    #[test]
    fn classify_build_fallback() {
        assert_eq!(classify(&argv(&["cargo", "build"])), CommandClass::Build);
        assert_eq!(
            classify(&argv(&["sh", "-c", "exit 3"])),
            CommandClass::Build
        );
        assert_eq!(
            classify(&argv(&["bun", "run", "build"])),
            CommandClass::Build
        );
    }
}

#[cfg(test)]
mod contract_tests;
