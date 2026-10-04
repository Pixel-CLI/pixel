// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the compact CLI rendering: what an agent reading
//! `pixel errors`-style output can rely on for every optional field, and
//! which lines must stay absent when the data is missing.

use super::*;
use crate::types::{EventKind, FramePackage, Surface};
use serde_json::json;

fn record(message: &str) -> ErrorRecord {
    ErrorRecord {
        id: 7,
        first_ts: 0,
        last_ts: 0,
        count: 1,
        run_id: None,
        surface: Surface::ServerConsole,
        kind: None,
        message: message.into(),
        stack_raw: None,
        frames: None,
        values: None,
        http: None,
        extra: None,
        dedup_hash: "h".into(),
    }
}

fn frame(file: &str) -> Frame {
    Frame {
        raw: format!("at {file}"),
        file: Some(file.into()),
        line: Some(3),
        column: Some(9),
        ..Frame::default()
    }
}

fn event(ts: i64, kind: EventKind, data: Option<serde_json::Value>) -> EventRecord {
    EventRecord {
        id: 1,
        ts,
        run_id: None,
        kind,
        data,
    }
}

fn run(run_id: &str) -> RunRecord {
    RunRecord {
        run_id: run_id.into(),
        started_at: 0,
        pid: None,
        port: None,
        git_head: None,
        lockfile_hash: None,
        vite_dep_hash: None,
        fingerprint: None,
        changed_since_last_run: None,
    }
}

// --- age ------------------------------------------------------------------

#[test]
fn age_should_clamp_to_zero_seconds_when_the_timestamp_is_in_the_future() {
    assert_eq!(age(1_000, 9_000), "0s ago");
}

#[test]
fn age_should_switch_unit_exactly_at_each_boundary() {
    assert_eq!(age(59_000, 0), "59s ago");
    assert_eq!(age(60_000, 0), "1m ago");
    assert_eq!(age(3_599_000, 0), "59m ago");
    assert_eq!(age(3_600_000, 0), "1h ago");
    assert_eq!(age(86_399_000, 0), "23h ago");
    assert_eq!(age(86_400_000, 0), "1d ago");
}

// --- error_lines ----------------------------------------------------------

#[test]
fn error_lines_should_render_one_line_with_count_one_when_no_frame_or_kind() {
    let rendered = error_lines(&record("boom\nsecond line"), 5_000);
    assert_eq!(
        rendered,
        "#7  5s ago   \u{d7}1   [server-console] boom second line"
    );
}

#[test]
fn error_lines_should_prefix_the_kind_when_the_message_lacks_it() {
    let mut r = record("bad thing");
    r.kind = Some("RangeError".into());
    assert!(
        error_lines(&r, 0).ends_with("] RangeError: bad thing"),
        "{}",
        error_lines(&r, 0)
    );
}

#[test]
fn error_lines_should_point_at_the_first_app_frame_when_vendor_frames_come_first() {
    let mut r = record("x");
    r.frames = Some(vec![
        frame("/app/node_modules/react/index.js"),
        frame("node:internal/process"),
        frame("src/app.ts"),
    ]);
    let rendered = error_lines(&r, 0);
    assert_eq!(rendered.lines().nth(1), Some("      @ src/app.ts:3:9"));
}

#[test]
fn error_lines_should_fall_back_to_a_vendor_frame_when_no_app_frame_exists() {
    let mut r = record("x");
    r.frames = Some(vec![
        Frame {
            raw: "native".into(),
            ..Frame::default()
        },
        frame("/app/node_modules/lib/a.js"),
    ]);
    let rendered = error_lines(&r, 0);
    assert_eq!(
        rendered.lines().nth(1),
        Some("      @ /app/node_modules/lib/a.js:3:9")
    );
}

#[test]
fn error_lines_should_omit_the_location_line_when_no_frame_has_a_location() {
    let mut r = record("x");
    r.frames = Some(vec![Frame {
        raw: "native code".into(),
        ..Frame::default()
    }]);
    assert_eq!(error_lines(&r, 0).lines().count(), 1);
}

#[test]
fn error_lines_should_prefer_the_source_mapped_location_and_omit_a_missing_column() {
    let mut r = record("x");
    r.frames = Some(vec![Frame {
        raw: "at bundle".into(),
        file: Some("dist/bundle.js".into()),
        line: Some(1),
        column: Some(4000),
        mapped_file: Some("src/view.tsx".into()),
        mapped_line: Some(42),
        mapped_column: None,
        ..Frame::default()
    }]);
    assert_eq!(
        error_lines(&r, 0).lines().nth(1),
        Some("      @ src/view.tsx:42")
    );
}

#[test]
fn error_lines_should_name_the_package_without_version_or_duplicate_warning_when_single() {
    let mut f = frame("src/a.ts");
    f.pkg = Some(FramePackage {
        name: "left-pad".into(),
        version: None,
        path: None,
        dup_paths: vec!["only".into()],
    });
    let mut r = record("x");
    r.frames = Some(vec![f]);
    assert_eq!(
        error_lines(&r, 0).lines().nth(1),
        Some("      @ src/a.ts:3:9  \u{2190} via left-pad")
    );
}

// --- render_error_list ----------------------------------------------------

#[test]
fn render_error_list_should_say_no_errors_and_still_print_the_cursor_when_empty() {
    let list = ErrorList {
        errors: vec![],
        cursor: 0,
    };
    assert_eq!(render_error_list(&list, 0), "no errors\ncursor: 0\n");
}

#[test]
fn render_error_list_should_print_one_block_per_error_before_the_cursor() {
    let list = ErrorList {
        errors: vec![record("a"), record("b")],
        cursor: 9,
    };
    let rendered = render_error_list(&list, 0);
    assert_eq!(
        rendered,
        "#7  0s ago   \u{d7}1   [server-console] a\n#7  0s ago   \u{d7}1   [server-console] b\ncursor: 9\n"
    );
}

// --- render_show / render_run ---------------------------------------------

#[test]
fn render_show_should_print_only_the_headline_and_first_seen_when_nothing_else_is_known() {
    let result = ShowResult {
        error: record("x"),
        correlated_events: vec![],
        run: None,
    };
    assert_eq!(
        render_show(&result, 0),
        "#7  0s ago   \u{d7}1   [server-console] x\nfirst seen 0s ago, seen 1 time(s)\n"
    );
}

#[test]
fn render_show_should_print_every_optional_section_in_order_when_present() {
    let mut error = record("x");
    error.stack_raw = Some("line a\nline b".into());
    error.values = Some(json!({"v": 1}));
    error.http = Some(json!({"status": 502}));
    error.extra = Some(json!("note"));
    let mut r = run("run-1");
    r.pid = Some(42);
    r.port = Some(5173);
    let result = ShowResult {
        error,
        correlated_events: vec![event(0, EventKind::HmrUpdate, None)],
        run: Some(r),
    };
    let rendered = render_show(&result, 0);
    let expected_tail = "first seen 0s ago, seen 1 time(s)\n\
stack:\n  line a\n  line b\n\
values: {\"v\":1}\n\
http: {\"status\":502}\n\
extra: \"note\"\n\
run run-1  pid 42  port 5173\n\
events within \u{b1}30s:\n  0s ago    [hmr-update]\n";
    assert!(rendered.ends_with(expected_tail), "{rendered}");
}

#[test]
fn render_show_should_keep_only_the_first_fifteen_stack_lines() {
    let mut error = record("x");
    error.stack_raw = Some(
        (1..=20)
            .map(|i| format!("f{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let rendered = render_show(
        &ShowResult {
            error,
            correlated_events: vec![],
            run: None,
        },
        0,
    );
    assert!(rendered.contains("  f15\n"));
    assert!(!rendered.contains("f16"), "{rendered}");
}

#[test]
fn render_run_should_truncate_hashes_to_twelve_chars_and_list_changed_inputs() {
    let mut r = run("run-2");
    r.git_head = Some("0123456789abcdef0123".into());
    r.lockfile_hash = Some("short".into());
    r.vite_dep_hash = Some("fedcba9876543210".into());
    r.changed_since_last_run = Some(vec!["lockfile".into(), "git_head".into()]);
    let env = EnvFingerprint {
        run: Some(r),
        previous: None,
        changed: None,
    };
    assert_eq!(
        render_env(&env),
        "run run-2  head 0123456789ab  lockfile short  vite-deps fedcba987654  changed-since-last-run: lockfile, git_head\n"
    );
}

#[test]
fn render_run_should_omit_the_changed_list_when_it_is_empty() {
    let mut r = run("run-3");
    r.changed_since_last_run = Some(vec![]);
    let env = EnvFingerprint {
        run: Some(r),
        previous: None,
        changed: None,
    };
    assert_eq!(render_env(&env), "run run-3\n");
}

// --- render_hmr -----------------------------------------------------------

#[test]
fn render_hmr_should_say_no_updates_when_none_was_recorded() {
    let status = HmrStatus {
        last_update: None,
        events: vec![],
    };
    assert_eq!(render_hmr(&status, 0), "no hmr updates recorded\n");
}

#[test]
fn render_hmr_should_print_the_last_update_with_its_data_then_each_event() {
    let status = HmrStatus {
        last_update: Some(event(0, EventKind::FullReload, Some(json!("app.tsx")))),
        events: vec![event(0, EventKind::HmrUpdate, None)],
    };
    assert_eq!(
        render_hmr(&status, 3_000),
        "last update: 3s ago    [full-reload]  \"app.tsx\"\n  3s ago    [hmr-update]\n"
    );
}

// --- render_env -----------------------------------------------------------

#[test]
fn render_env_should_say_no_runs_when_none_was_recorded() {
    let env = EnvFingerprint {
        run: None,
        previous: None,
        changed: None,
    };
    assert_eq!(render_env(&env), "no runs recorded\n");
}

#[test]
fn render_env_should_say_nothing_changed_when_the_diff_is_empty() {
    let env = EnvFingerprint {
        run: Some(run("now")),
        previous: Some(run("before")),
        changed: Some(vec![]),
    };
    assert_eq!(
        render_env(&env),
        "run now\nprevious: run before\nchanged: nothing (or no previous run)\n"
    );
}

#[test]
fn render_env_should_list_each_changed_input_when_the_diff_is_not_empty() {
    let env = EnvFingerprint {
        run: Some(run("now")),
        previous: None,
        changed: Some(vec!["lockfile".into(), "port".into()]),
    };
    assert_eq!(render_env(&env), "run now\nchanged:\n  lockfile\n  port\n");
}

// --- render_test ----------------------------------------------------------

#[test]
fn render_test_should_report_each_passing_state() {
    let status = |passing| TestStatus {
        latest_failure: None,
        latest_pass: None,
        passing,
    };
    assert_eq!(render_test(&status(Some(true)), 0), "passing\n");
    assert_eq!(render_test(&status(Some(false)), 0), "failing\n");
    assert_eq!(render_test(&status(None), 0), "no test runs recorded\n");
}

#[test]
fn render_test_should_show_the_failure_its_details_and_the_last_green_run() {
    let mut failure = record("expected 1 got 2");
    failure.extra = Some(json!(["a.test.ts"]));
    let status = TestStatus {
        latest_failure: Some(failure),
        latest_pass: Some(event(0, EventKind::TestPass, None)),
        passing: Some(false),
    };
    assert_eq!(
        render_test(&status, 0),
        "failing\n#7  0s ago   \u{d7}1   [server-console] expected 1 got 2\nfailures: [\"a.test.ts\"]\nlast green: 0s ago    [test-pass]\n"
    );
}

#[test]
fn render_test_should_omit_the_failures_line_when_the_failure_has_no_details() {
    let status = TestStatus {
        latest_failure: Some(record("boom")),
        latest_pass: None,
        passing: Some(false),
    };
    assert!(!render_test(&status, 0).contains("failures:"));
}

// --- render_cursor / render_gc --------------------------------------------

#[test]
fn render_cursor_should_print_the_cursor_line() {
    assert_eq!(render_cursor(&CursorResult { cursor: 31 }), "cursor: 31\n");
}

#[test]
fn render_gc_should_mention_vacuum_only_when_it_ran() {
    let outcome = |vacuumed| GcOutcome {
        errors_deleted: 3,
        events_deleted: 4,
        vacuumed,
    };
    assert_eq!(
        render_gc(&outcome(true)),
        "deleted 3 error(s), 4 event(s), vacuumed\n"
    );
    assert_eq!(
        render_gc(&outcome(false)),
        "deleted 3 error(s), 4 event(s)\n"
    );
}
