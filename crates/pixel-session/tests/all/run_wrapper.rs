// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `sniper run` end to end: a shell command replays a fixture and exits
//! like the real tool; the store must hold the structured records.

use std::fs;
use std::path::{Path, PathBuf};

use pixel_session::query;
use pixel_session::run::run_wrapped;
use pixel_session::store::Store;
use pixel_session::types::{ErrorRecord, EventKind, Surface};
use serde_json::Value;

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> TempRoot {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pixel-session-run-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        TempRoot(dir)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Run `cat <fixture>; exit <code>` under the wrapper (stdout, like the
/// real runners) and return the store.
fn replay(state: &TempRoot, name: &str, exit: i32, label: Option<&str>) -> (Store, i32) {
    let project = state.0.join("project");
    fs::create_dir_all(&project).unwrap();
    let store = Store::open_at(&project, &state.0).unwrap();
    let script = format!("cat '{}'; exit {exit}", fixture(name).display());
    let argv = vec!["sh".to_owned(), "-c".to_owned(), script];
    let code = run_wrapped(&store, label, &argv).unwrap();
    (store, code)
}

fn all_errors(store: &Store) -> Vec<ErrorRecord> {
    // Oldest first, so indices follow the order the records were written.
    let mut errors = store.last_errors(100, None).unwrap();
    errors.reverse();
    errors
}

fn extra<'a>(record: &'a ErrorRecord, key: &str) -> &'a Value {
    record.extra.as_ref().unwrap().get(key).unwrap()
}

#[test]
fn minitest_single_failure_is_one_record_plus_summary() {
    let state = TempRoot::new();
    let (store, code) = replay(&state, "minitest-single-failure.txt", 1, Some("rails-test"));
    assert_eq!(code, 1);
    let errors = all_errors(&store);
    assert_eq!(errors.len(), 2, "{errors:#?}");

    let failure = &errors[0];
    assert_eq!(failure.surface, Surface::Minitest);
    assert_eq!(failure.kind.as_deref(), Some("failure"));
    assert_eq!(
        failure.message,
        "UserTest#test_name_is_required: Expected false to be truthy."
    );
    assert_eq!(extra(failure, "testClass"), "UserTest");
    assert_eq!(extra(failure, "testName"), "test_name_is_required");
    assert_eq!(extra(failure, "file"), "test/models/user_test.rb");
    assert_eq!(extra(failure, "line"), 12);
    assert_eq!(extra(failure, "message"), "Expected false to be truthy.");
    assert_eq!(
        extra(failure, "rerun"),
        "bin/rails test test/models/user_test.rb:10"
    );
    assert!(failure.extra.as_ref().unwrap().get("expected").is_none());
    // No backtrace: the header location becomes the single frame so
    // `sniper last` shows `@ file:line`.
    let frames = failure.frames.as_ref().unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].file.as_deref(), Some("test/models/user_test.rb"));
    assert_eq!(frames[0].line, Some(12));
    assert_eq!(failure.stack_raw, None);
    assert!(failure.run_id.as_deref().unwrap().starts_with("run-"));

    let summary = &errors[1];
    assert_eq!(summary.surface, Surface::Minitest);
    assert_eq!(summary.kind.as_deref(), Some("summary"));
    assert_eq!(
        summary.message,
        "10 runs, 11 assertions, 1 failures, 0 errors, 0 skips"
    );
    assert_eq!(extra(summary, "label"), "rails-test");
    assert_eq!(extra(summary, "exitCode"), 1);
    assert_eq!(extra(summary, "counters")["failures"], 1);
    assert_eq!(extra(summary, "counters")["runs"], 10);
    let listed = extra(summary, "failures").as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0]["test"],
        "UserTest#test_name_is_required: Expected false to be truthy."
    );
    assert_eq!(listed[0]["kind"], "failure");
    assert_eq!(
        listed[0]["rerun"],
        "bin/rails test test/models/user_test.rb:10"
    );
    assert!(
        summary
            .extra
            .as_ref()
            .unwrap()
            .get("truncatedCount")
            .is_none()
    );

    // No generic tail record, no raw fallback, no test-pass event.
    assert!(errors.iter().all(|e| e.surface != Surface::RunWrapper));
    assert!(
        store
            .latest_event_by_kind(EventKind::TestPass)
            .unwrap()
            .is_none()
    );

    // `sniper test` reports the run as failing with the summary record.
    let status = query::test_status(&store).unwrap();
    assert_eq!(status.passing, Some(false));
    assert_eq!(status.latest_failure.as_ref().unwrap().id, summary.id);
}

#[test]
fn minitest_mixed_run_records_failure_error_and_expected_actual() {
    let state = TempRoot::new();
    let (store, _) = replay(&state, "minitest-mixed.txt", 1, None);
    let errors = all_errors(&store);
    assert_eq!(errors.len(), 3, "{errors:#?}");

    let failure = &errors[0];
    assert_eq!(failure.kind.as_deref(), Some("failure"));
    assert_eq!(
        failure.message,
        "Billing::InvoiceTest#test_total_includes_vat: Expected: 1200"
    );
    assert_eq!(extra(failure, "expected"), "1200");
    assert_eq!(extra(failure, "actual"), "1000");
    assert_eq!(extra(failure, "message"), "Expected: 1200\nActual: 1000");

    let error = &errors[1];
    assert_eq!(error.kind.as_deref(), Some("error"));
    assert_eq!(
        error.message,
        "Billing::InvoiceTest#test_pdf_generation: NoMethodError: undefined method 'render' for nil"
    );
    assert_eq!(extra(error, "file"), "app/models/billing/invoice.rb");
    assert_eq!(extra(error, "line"), 88);
    assert_eq!(
        extra(error, "rerun"),
        "bin/rails test test/models/billing/invoice_test.rb:29"
    );
    // Backtrace filtered to project frames, parsed into frames.
    let stack = error.stack_raw.as_deref().unwrap();
    assert!(stack.contains("app/models/billing/invoice.rb:88"));
    assert!(stack.contains("test/models/billing/invoice_test.rb:31"));
    assert!(!stack.contains("/gems/"));
    assert!(!stack.contains("<internal:"));
    let frames = error.frames.as_ref().unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].func.as_deref(), Some("Billing::Invoice#to_pdf"));
    assert_eq!(frames[1].line, Some(31));

    let summary = &errors[2];
    assert_eq!(summary.kind.as_deref(), Some("summary"));
    assert_eq!(extra(summary, "counters")["errors"], 1);
    assert_eq!(extra(summary, "counters")["skips"], 1);
    assert_eq!(extra(summary, "failures").as_array().unwrap().len(), 2);
    // The generic wrapper's label defaults to the program name.
    assert_eq!(extra(summary, "label"), "sh");
}

#[test]
fn minitest_parallel_output_records_both_tests_without_worker_noise() {
    let state = TempRoot::new();
    let (store, _) = replay(&state, "minitest-parallel.txt", 1, None);
    let errors = all_errors(&store);
    assert_eq!(errors.len(), 3, "{errors:#?}");
    assert_eq!(extra(&errors[0], "testClass"), "OrdersControllerTest");
    assert_eq!(extra(&errors[1], "testClass"), "TokenTest");
    assert_eq!(errors[1].kind.as_deref(), Some("error"));
    assert_eq!(extra(&errors[1], "file"), "test/models/token_test.rb");
    assert_eq!(extra(&errors[1], "line"), 27);
    for record in &errors {
        let text = serde_json::to_string(record).unwrap();
        assert!(!text.contains("DEPRECATION"), "{text}");
        assert!(!text.contains("Worker"), "{text}");
    }
    assert_eq!(extra(&errors[2], "counters")["runs"], 24);
}

#[test]
fn minitest_green_run_records_test_pass_with_counters() {
    let state = TempRoot::new();
    let (store, code) = replay(&state, "minitest-green.txt", 0, Some("rails-test"));
    assert_eq!(code, 0);
    assert!(all_errors(&store).is_empty());
    assert!(
        store
            .latest_event_by_kind(EventKind::BuildOk)
            .unwrap()
            .is_none()
    );
    let pass = store
        .latest_event_by_kind(EventKind::TestPass)
        .unwrap()
        .unwrap();
    let data = pass.data.unwrap();
    assert_eq!(data["runner"], "minitest");
    assert_eq!(data["runs"], 45);
    assert_eq!(data["assertions"], 120);
    assert_eq!(data["failures"], 0);
    assert_eq!(data["errors"], 0);
    assert_eq!(data["skips"], 2);
    assert_eq!(data["passed"], 43);
    assert_eq!(data["label"], "rails-test");
    assert!(data["durationMs"].is_number());

    let status = query::test_status(&store).unwrap();
    assert_eq!(status.passing, Some(true));
    assert_eq!(status.latest_pass.unwrap().id, pass.id);
}

#[test]
fn rspec_single_failure_is_one_record_plus_summary() {
    let state = TempRoot::new();
    let (store, code) = replay(&state, "rspec-single-failure.txt", 1, None);
    assert_eq!(code, 1);
    let errors = all_errors(&store);
    assert_eq!(errors.len(), 2, "{errors:#?}");

    let failure = &errors[0];
    assert_eq!(failure.surface, Surface::Rspec);
    assert_eq!(failure.kind.as_deref(), Some("failure"));
    assert_eq!(
        failure.message,
        "User validation requires a name: Failure/Error: expect(user).to be_valid"
    );
    assert_eq!(extra(failure, "testClass"), "User");
    assert_eq!(
        extra(failure, "testName"),
        "User validation requires a name"
    );
    assert_eq!(extra(failure, "file"), "spec/models/user_spec.rb");
    assert_eq!(extra(failure, "line"), 12);
    assert_eq!(
        extra(failure, "rerun"),
        "rspec ./spec/models/user_spec.rb:10"
    );
    let frames = failure.frames.as_ref().unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].file.as_deref(), Some("spec/models/user_spec.rb"));
    assert_eq!(frames[0].line, Some(12));

    let summary = &errors[1];
    assert_eq!(summary.kind.as_deref(), Some("summary"));
    assert_eq!(summary.message, "10 examples, 1 failures");
    assert_eq!(extra(summary, "counters")["examples"], 10);
    assert_eq!(extra(summary, "counters")["pending"], 0);
    assert_eq!(query::test_status(&store).unwrap().passing, Some(false));
}

#[test]
fn rspec_mixed_run_records_expectation_and_exception() {
    let state = TempRoot::new();
    let (store, _) = replay(&state, "rspec-mixed.txt", 1, None);
    let errors = all_errors(&store);
    assert_eq!(errors.len(), 3, "{errors:#?}");

    let failure = &errors[0];
    assert_eq!(failure.kind.as_deref(), Some("failure"));
    // The description already starts with the class: not repeated.
    assert_eq!(
        failure.message,
        "Billing::Invoice#total includes VAT: Failure/Error: expect(invoice.total).to eq(1200)"
    );
    assert_eq!(extra(failure, "testClass"), "Billing::Invoice");
    assert_eq!(extra(failure, "expected"), "1200");
    assert_eq!(extra(failure, "actual"), "1000");
    assert_eq!(extra(failure, "line"), 23);
    let stack = failure.stack_raw.as_deref().unwrap();
    assert!(stack.contains("spec/support/with_invoice.rb:7"));
    assert!(!stack.contains("rspec-core"));
    assert_eq!(failure.frames.as_ref().unwrap().len(), 3);

    let error = &errors[1];
    assert_eq!(error.kind.as_deref(), Some("error"));
    assert_eq!(
        error.message,
        "Billing::Invoice#to_pdf renders the template: NoMethodError: undefined method 'render' for nil"
    );
    assert_eq!(extra(error, "file"), "spec/models/billing/invoice_spec.rb");
    assert_eq!(extra(error, "line"), 31);
    assert_eq!(
        extra(error, "rerun"),
        "rspec ./spec/models/billing/invoice_spec.rb:29"
    );

    let summary = &errors[2];
    assert_eq!(summary.message, "7 examples, 2 failures, 1 pending");
    assert_eq!(extra(summary, "failures").as_array().unwrap().len(), 2);
}

#[test]
fn rspec_parallel_output_uses_the_total_summary() {
    let state = TempRoot::new();
    let (store, _) = replay(&state, "rspec-parallel.txt", 1, None);
    let errors = all_errors(&store);
    assert_eq!(errors.len(), 2, "{errors:#?}");
    assert_eq!(extra(&errors[0], "testClass"), "OrdersController");
    assert_eq!(extra(&errors[0], "line"), 31);
    assert_eq!(errors[1].message, "28 examples, 1 failures");
    assert_eq!(extra(&errors[1], "counters")["examples"], 28);
}

#[test]
fn rspec_green_run_records_test_pass_with_counters() {
    let state = TempRoot::new();
    let (store, code) = replay(&state, "rspec-green.txt", 0, None);
    assert_eq!(code, 0);
    assert!(all_errors(&store).is_empty());
    let pass = store
        .latest_event_by_kind(EventKind::TestPass)
        .unwrap()
        .unwrap();
    let data = pass.data.unwrap();
    assert_eq!(data["runner"], "rspec");
    assert_eq!(data["examples"], 36);
    assert_eq!(data["failures"], 0);
    assert_eq!(data["pending"], 1);
    assert_eq!(data["passed"], 35);
    assert_eq!(query::test_status(&store).unwrap().passing, Some(true));
}

#[test]
fn rubocop_offenses_are_one_lint_record_each() {
    let state = TempRoot::new();
    let (store, code) = replay(&state, "rubocop-offenses.txt", 1, Some("rubocop"));
    assert_eq!(code, 1);
    let errors = all_errors(&store);
    assert_eq!(errors.len(), 3, "{errors:#?}");
    for record in &errors {
        assert_eq!(record.surface, Surface::Rubocop);
        assert_eq!(record.kind.as_deref(), Some("lint"));
    }
    let first = &errors[0];
    assert_eq!(
        first.message,
        "Style/StringLiterals: Prefer single-quoted strings when you don't need string interpolation or special symbols."
    );
    assert_eq!(extra(first, "file"), "app/models/user.rb");
    assert_eq!(extra(first, "line"), 3);
    assert_eq!(extra(first, "column"), 10);
    assert_eq!(extra(first, "severity"), "C");
    assert_eq!(extra(first, "cop"), "Style/StringLiterals");
    assert_eq!(extra(first, "correctable"), false);
    let frame = &first.frames.as_ref().unwrap()[0];
    assert_eq!(frame.file.as_deref(), Some("app/models/user.rb"));
    assert_eq!(frame.line, Some(3));
    assert_eq!(frame.column, Some(10));

    let second = &errors[1];
    assert_eq!(extra(second, "severity"), "W");
    assert_eq!(extra(second, "correctable"), true);
    assert_eq!(extra(second, "cop"), "Lint/UselessAssignment");
    assert_eq!(extra(&errors[2], "line"), 41);

    // Lint is not a test signal: `sniper test` stays silent.
    assert_eq!(query::test_status(&store).unwrap().passing, None);
    assert!(errors.iter().all(|e| e.kind.as_deref() != Some("summary")));
}

#[test]
fn rubocop_clean_run_is_a_build_ok_event() {
    let state = TempRoot::new();
    let (store, code) = replay(&state, "rubocop-clean.txt", 0, None);
    assert_eq!(code, 0);
    assert!(all_errors(&store).is_empty());
    assert!(
        store
            .latest_event_by_kind(EventKind::TestPass)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .latest_event_by_kind(EventKind::BuildOk)
            .unwrap()
            .is_some()
    );
}

#[test]
fn unrecognised_output_keeps_the_generic_tail_record() {
    let state = TempRoot::new();
    let project = state.0.join("project");
    fs::create_dir_all(&project).unwrap();
    let store = Store::open_at(&project, &state.0).unwrap();
    let argv = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        "echo something broke; exit 3".to_owned(),
    ];
    assert_eq!(run_wrapped(&store, Some("build"), &argv).unwrap(), 3);
    let errors = all_errors(&store);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].surface, Surface::RunWrapper);
    assert_eq!(errors[0].kind.as_deref(), Some("exit-3"));
    assert_eq!(
        extra(&errors[0], "tail"),
        &serde_json::json!(["something broke"])
    );
}

#[test]
fn recognised_runner_with_nothing_to_record_falls_back_to_tail() {
    // A Rails boot failure after the banner: Minitest is detected but no
    // block parsed, so the generic tail record still carries the output.
    let state = TempRoot::new();
    let project = state.0.join("project");
    fs::create_dir_all(&project).unwrap();
    let store = Store::open_at(&project, &state.0).unwrap();
    let argv = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        "printf '# Running:\\n\\nrails aborted!\\nPG::ConnectionBad: could not connect\\n'; exit 1"
            .to_owned(),
    ];
    assert_eq!(run_wrapped(&store, None, &argv).unwrap(), 1);
    let errors = all_errors(&store);
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].surface, Surface::RunWrapper);
    let tail = extra(&errors[0], "tail").as_array().unwrap();
    assert!(tail.iter().any(|l| l == "rails aborted!"));

    // A green exit with a banner but no summary line is a plain test-pass
    // without counters, not a crash.
    let argv = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        "printf '# Running:\\n'; exit 0".to_owned(),
    ];
    assert_eq!(run_wrapped(&store, None, &argv).unwrap(), 0);
    assert!(
        store
            .latest_event_by_kind(EventKind::BuildOk)
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .latest_event_by_kind(EventKind::TestPass)
            .unwrap()
            .is_none()
    );
}

#[test]
fn ruby_surfaces_round_trip_by_name() {
    for (surface, name) in [
        (Surface::Minitest, "minitest"),
        (Surface::Rspec, "rspec"),
        (Surface::Rubocop, "rubocop"),
    ] {
        assert_eq!(surface.as_str(), name);
        assert_eq!(Surface::parse(name), Some(surface));
        assert_eq!(
            serde_json::to_string(&surface).unwrap(),
            format!("\"{name}\"")
        );
    }
}

/// The test a headline names (`Class#test: first line` → `Class#test`):
/// the first line is the text formatter's quoted source line
/// (`Failure/Error: …`), which the JSON document does not carry.
fn identity(headline: &str) -> String {
    headline.split(": ").next().unwrap_or_default().to_owned()
}

/// What a failure record says, minus how its formatter spelled it: the
/// text formatter quotes the failing source line (`Failure/Error:`) and
/// marks backtrace lines with `# `, the JSON document does neither.
fn meaning(record: &ErrorRecord) -> Value {
    let extra = record.extra.clone().unwrap_or_default();
    let message: Vec<&str> = extra["message"]
        .as_str()
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with("Failure/Error:"))
        .collect();
    let frames: Vec<(Option<String>, Option<u32>, Option<String>)> = record
        .frames
        .iter()
        .flatten()
        .map(|f| (f.file.clone(), f.line, f.func.clone()))
        .collect();
    serde_json::json!({
        "surface": record.surface.as_str(),
        "kind": record.kind,
        "headline": identity(&record.message),
        "testClass": extra["testClass"],
        "testName": extra["testName"],
        "file": extra["file"],
        "line": extra["line"],
        "expected": extra.get("expected"),
        "actual": extra.get("actual"),
        "rerun": extra["rerun"],
        "message": message,
        "frames": frames,
        "counters": extra.get("counters"),
        "failures": extra.get("failures").and_then(Value::as_array).map(|listed| {
            listed
                .iter()
                .map(|f| {
                    let mut f = f.clone();
                    f["test"] = Value::String(identity(f["test"].as_str().unwrap_or_default()));
                    f
                })
                .collect::<Vec<_>>()
        }),
        "cop": extra.get("cop"),
        "column": extra.get("column"),
        "severity": extra.get("severity"),
        "correctable": extra.get("correctable"),
    })
}

fn meanings(store: &Store) -> Vec<Value> {
    all_errors(store).iter().map(meaning).collect()
}

#[test]
fn rspec_json_and_text_of_one_run_give_the_same_records() {
    let text = TempRoot::new();
    let (text_store, _) = replay(&text, "rspec-mixed.txt", 1, Some("rspec"));
    let json = TempRoot::new();
    let (json_store, code) = replay(&json, "rspec-mixed.json", 1, Some("rspec"));
    assert_eq!(code, 1);
    let want = meanings(&text_store);
    assert_eq!(want.len(), 3, "two failures and the summary: {want:#?}");
    assert_eq!(meanings(&json_store), want);
    let summary = &all_errors(&json_store)[2];
    assert_eq!(summary.message, "7 examples, 2 failures, 1 pending");
    assert_eq!(
        query::test_status(&json_store).unwrap().passing,
        Some(false)
    );
    // The raw document stays recoverable beside the records.
    let raw = json_store
        .latest_raw_fallback("run:rspec")
        .unwrap()
        .unwrap();
    assert!(raw.starts_with("warning: parser/current"), "{raw}");
    assert!(raw.contains("\"summary_line\": \"7 examples, 2 failures, 1 pending\""));
}

#[test]
fn rubocop_json_and_text_of_one_run_give_the_same_records() {
    let text = TempRoot::new();
    let (text_store, _) = replay(&text, "rubocop-offenses.txt", 1, Some("rubocop"));
    let json = TempRoot::new();
    let (json_store, code) = replay(&json, "rubocop-offenses.json", 1, Some("rubocop"));
    assert_eq!(code, 1);
    let want = meanings(&text_store);
    assert_eq!(want.len(), 3);
    assert_eq!(meanings(&json_store), want);
}

#[test]
fn rubocop_autocorrected_offenses_are_not_recorded_as_remaining() {
    let state = TempRoot::new();
    let (store, code) = replay(&state, "rubocop-autocorrect.json", 1, Some("rubocop"));
    assert_eq!(code, 1);
    let errors = all_errors(&store);
    assert_eq!(
        errors
            .iter()
            .map(|e| e.message.as_str())
            .collect::<Vec<_>>(),
        ["Lint/UselessAssignment: Useless assignment to variable - x."]
    );
    assert_eq!(extra(&errors[0], "corrected"), false);
}

#[test]
fn rspec_json_green_and_empty_runs_are_passes_with_their_counters() {
    let state = TempRoot::new();
    let (store, code) = replay(&state, "rspec-green.json", 0, None);
    assert_eq!(code, 0);
    let pass = store
        .latest_event_by_kind(EventKind::TestPass)
        .unwrap()
        .unwrap();
    let data = pass.data.unwrap();
    assert_eq!(
        (
            &data["runner"],
            &data["examples"],
            &data["failures"],
            &data["pending"],
            &data["passed"]
        ),
        (
            &serde_json::json!("rspec"),
            &serde_json::json!(36),
            &serde_json::json!(0),
            &serde_json::json!(1),
            &serde_json::json!(35)
        )
    );
    let empty = TempRoot::new();
    let (store, _) = replay(&empty, "rspec-empty.json", 0, None);
    let data = store
        .latest_event_by_kind(EventKind::TestPass)
        .unwrap()
        .unwrap()
        .data
        .unwrap();
    assert_eq!(
        (&data["examples"], &data["passed"]),
        (&serde_json::json!(0), &serde_json::json!(0))
    );
}

#[test]
fn rspec_json_boot_failure_is_an_error_not_a_pass() {
    let state = TempRoot::new();
    let (store, code) = replay(&state, "rspec-load-error.json", 1, Some("rspec"));
    assert_eq!(code, 1);
    let errors = all_errors(&store);
    assert_eq!(errors.len(), 2, "{errors:#?}");
    assert_eq!(errors[0].kind.as_deref(), Some("error"));
    assert_eq!(extra(&errors[0], "file"), "spec/models/order_spec.rb");
    assert!(
        extra(&errors[0], "message")
            .as_str()
            .unwrap()
            .contains("uninitialized constant Order::Customer")
    );
    assert_eq!(
        extra(&errors[0], "rerun"),
        "rspec ./spec/models/order_spec.rb"
    );
    assert_eq!(errors[1].kind.as_deref(), Some("summary"));
    assert_eq!(query::test_status(&store).unwrap().passing, Some(false));
    assert!(
        store
            .latest_event_by_kind(EventKind::TestPass)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_truncated_json_document_falls_back_to_the_raw_tail() {
    let state = TempRoot::new();
    let (store, code) = replay(&state, "rspec-truncated.json", 1, Some("rspec"));
    assert_eq!(code, 1);
    let errors = all_errors(&store);
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].surface, Surface::RunWrapper);
    assert_eq!(errors[0].kind.as_deref(), Some("exit-1"));
    assert!(
        store
            .latest_raw_fallback("run:rspec")
            .unwrap()
            .unwrap()
            .starts_with("{\"version\"")
    );
}

/// Run `script` under the wrapper and return the store and exit code.
fn run_script(state: &TempRoot, label: &str, script: &str) -> (Store, i32) {
    let project = state.0.join("project");
    fs::create_dir_all(&project).unwrap();
    let store = Store::open_at(&project, &state.0).unwrap();
    let argv = vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()];
    let code = run_wrapped(&store, Some(label), &argv).unwrap();
    (store, code)
}

#[test]
fn a_green_summary_with_a_failing_exit_is_never_a_pass() {
    // SimpleCov fails the run after RSpec printed a green summary.
    let state = TempRoot::new();
    let script = format!(
        "cat '{}'; echo 'Line coverage (79.30%) is below the expected minimum coverage (90.00%).'; echo 'SimpleCov failed with exit 2 due to a coverage related error'; exit 2",
        fixture("rspec-green.txt").display()
    );
    let (store, code) = run_script(&state, "rspec", &script);
    assert_eq!(code, 2, "the wrapped command's exit code is kept");
    assert!(
        store
            .latest_event_by_kind(EventKind::TestPass)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .latest_event_by_kind(EventKind::BuildOk)
            .unwrap()
            .is_none()
    );
    let errors = all_errors(&store);
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].kind.as_deref(), Some("exit-2"));
    let tail = extra(&errors[0], "tail").as_array().unwrap();
    assert_eq!(
        tail.last().unwrap(),
        "SimpleCov failed with exit 2 due to a coverage related error",
        "the late failure is the record's last line"
    );
}

#[test]
fn a_failing_summary_with_a_green_exit_records_its_failures() {
    // `rspec || true`: the status is lost, the report is not.
    let state = TempRoot::new();
    let script = format!("cat '{}'; exit 0", fixture("rspec-mixed.txt").display());
    let (store, code) = run_script(&state, "rspec", &script);
    assert_eq!(code, 0, "the exit code is the child's");
    assert!(
        store
            .latest_event_by_kind(EventKind::TestPass)
            .unwrap()
            .is_none()
    );
    let errors = all_errors(&store);
    assert_eq!(errors.len(), 3, "{errors:#?}");
    assert_eq!(errors[2].kind.as_deref(), Some("summary"));
    assert_eq!(extra(&errors[2], "exitCode"), 0);
    assert_eq!(query::test_status(&store).unwrap().passing, Some(false));
}

#[test]
fn unrecognised_rake_output_stays_visible_with_the_original_command() {
    let state = TempRoot::new();
    let var = format!("PIXEL_SESSION_RAKE_{}_{}", std::process::id(), line!());
    // SAFETY: the variable name is unique to this test and process; no
    // other test reads or writes it.
    unsafe { std::env::set_var(&var, "from-parent-env") };
    let script = format!(
        "echo '== 20261006120000 AddTotalToOrders: migrating ======'; echo '-- add_column(:orders, :total, :decimal)'; echo \"env=${var}\"; echo 'rake aborted!'; echo 'ActiveRecord::StatementInvalid: PG::DuplicateColumn'; exit 1"
    );
    let (store, code) = run_script(&state, "db:migrate", &script);
    // SAFETY: as above.
    unsafe { std::env::remove_var(&var) };
    assert_eq!(code, 1);
    let errors = all_errors(&store);
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].surface, Surface::RunWrapper);
    assert_eq!(errors[0].message, "db:migrate exited 1");
    assert_eq!(
        extra(&errors[0], "tail"),
        &serde_json::json!([
            "== 20261006120000 AddTotalToOrders: migrating ======",
            "-- add_column(:orders, :total, :decimal)",
            "env=from-parent-env",
            "rake aborted!",
            "ActiveRecord::StatementInvalid: PG::DuplicateColumn"
        ])
    );
    // The run keeps the command exactly as given.
    let run = &store.latest_runs(1).unwrap()[0];
    assert_eq!(
        run.fingerprint.as_ref().unwrap()["argv"],
        serde_json::json!(["sh", "-c", script])
    );
    // A green migration is a build, not a test pass.
    let ok = TempRoot::new();
    let (store, code) = run_script(
        &ok,
        "db:migrate",
        "echo '== AddTotalToOrders: migrated (0.01s) =='; exit 0",
    );
    assert_eq!(code, 0);
    assert!(
        store
            .latest_event_by_kind(EventKind::BuildOk)
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .latest_event_by_kind(EventKind::TestPass)
            .unwrap()
            .is_none()
    );
}
