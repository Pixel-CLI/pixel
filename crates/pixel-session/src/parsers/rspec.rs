// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Parser for RSpec's documentation/progress formatter output:
//!
//! ```text
//! Failures:
//!
//!   1) User validation requires a name
//!      Failure/Error: expect(user).to be_valid
//!        expected #<User …> to be valid
//!      # ./spec/models/user_spec.rb:12:in 'block (3 levels) in <top (required)>'
//!
//! 10 examples, 1 failure
//!
//! Failed examples:
//!
//! rspec ./spec/models/user_spec.rb:10 # User validation requires a name
//! ```
//!
//! and its JSON formatter (`--format json`, [`parse_json`]), whose document
//! gives the same records: one per failed example, the counters, and an
//! error outside of examples (a spec file that fails to load) as a failure.

use super::ruby::{
    FailureKind, TestFailure, blocks, cap_message, counter, counters, project_frame,
};

/// The counters of the `N examples, N failures[, N pending]` summary line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    pub examples: u64,
    pub failures: u64,
    pub pending: u64,
    /// `N errors occurred outside of examples`: a spec file that raised
    /// while loading, or a hook that failed around the suite.
    pub errors_outside: u64,
}

impl Counters {
    /// Examples that ran and neither failed nor were pending.
    pub fn passed(&self) -> u64 {
        self.examples
            .saturating_sub(self.failures)
            .saturating_sub(self.pending)
    }

    pub fn green(&self) -> bool {
        self.failures == 0 && self.errors_outside == 0
    }
}

/// A parsed RSpec run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub counters: Option<Counters>,
    pub failures: Vec<TestFailure>,
}

/// Parse `N examples, N failures[, N pending]`; under parallel_tests each
/// worker prints one and the last line is the total, so callers take the
/// last match.
pub fn parse_summary(line: &str) -> Option<Counters> {
    let text = line.trim();
    if !text.contains(" example, ") && !text.contains(" examples, ") {
        return None;
    }
    let c = counters(text);
    Some(Counters {
        examples: counter(&c, "example")?,
        failures: counter(&c, "failure")?,
        pending: counter(&c, "pending").unwrap_or(0),
        errors_outside: counter(&c, "error").unwrap_or(0),
    })
}

/// Whether the output came from RSpec: a summary line or the `Failures:`
/// section header.
pub fn detect(output: &str) -> bool {
    output
        .lines()
        .any(|l| l.trim() == "Failures:" || parse_summary(l).is_some())
}

/// `rspec ./spec/x_spec.rb:10 # description` → (rerun command, file, line).
pub fn parse_rerun_line(line: &str) -> Option<(String, String, Option<u32>)> {
    let text = line.trim();
    let rest = text.strip_prefix("rspec ")?;
    let target = rest.split_whitespace().next()?;
    let command = match rest.split_once(" # ") {
        Some((cmd, _)) => format!("rspec {}", cmd.trim()),
        None => format!("rspec {target}"),
    };
    let target = target.strip_prefix("./").unwrap_or(target);
    let (file, line_no) = match target.split_once('[') {
        Some((file, _)) => (file, None),
        None => match target.rsplit_once(':') {
            Some((file, n)) => (file, n.parse().ok()),
            None => (target, None),
        },
    };
    if file.is_empty() {
        return None;
    }
    Some((command, file.to_owned(), line_no))
}

/// `  1) description` → (index, description).
fn parse_entry_header(line: &str) -> Option<(usize, String)> {
    let text = line.trim();
    let (index, description) = text.split_once(") ")?;
    let index: usize = index.parse().ok()?;
    let description = description.trim();
    if description.is_empty() {
        return None;
    }
    Some((index, description.to_owned()))
}

/// The leading constant path of a description (`Billing::Invoice#total
/// includes VAT` → `Billing::Invoice`), when the description starts with one.
pub fn leading_constant(description: &str) -> Option<String> {
    let first = description.split_whitespace().next()?;
    let ident = first.split(['#', '.']).next()?;
    let mut segments = ident.split("::");
    let valid = |s: &str| {
        s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    let all_valid = segments.all(valid);
    (all_valid && !ident.is_empty()).then(|| ident.to_owned())
}

/// A line naming a raised exception class (`NoMethodError:`).
fn error_class_line(line: &str) -> Option<&str> {
    let text = line.trim().strip_suffix(':')?;
    (!text.is_empty()
        && !text.contains(char::is_whitespace)
        && text.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
    .then_some(text)
}

fn is_section_end(line: &str) -> bool {
    let text = line.trim();
    text.starts_with("Finished in ") || text == "Failed examples:"
}

/// Parse a full captured output into counters and one entry per failure.
pub fn parse(output: &str) -> Report {
    let lines: Vec<&str> = output.lines().collect();
    let counters = lines.iter().rev().find_map(|l| parse_summary(l));
    let reruns: Vec<(String, String, Option<u32>)> =
        lines.iter().filter_map(|l| parse_rerun_line(l)).collect();

    let mut failures = Vec::new();
    for block in blocks(&lines, |l| parse_entry_header(l).is_some()) {
        let Some((index, description)) = parse_entry_header(block[0]) else {
            continue;
        };
        let mut message_lines: Vec<String> = Vec::new();
        let mut error_class: Option<String> = None;
        let mut expected = None;
        let mut actual = None;
        let mut backtrace = Vec::new();
        let mut spec_location: Option<(String, u32)> = None;
        let mut first_project: Option<(String, u32)> = None;
        for line in &block[1..] {
            let trimmed = line.trim();
            if is_section_end(line) {
                break;
            }
            if trimmed.starts_with("# ") {
                if let Some(frame) = project_frame(line) {
                    if let (Some(file), Some(line_no)) = (&frame.file, frame.line) {
                        if spec_location.is_none() && file.contains("_spec.rb") {
                            spec_location = Some((file.clone(), line_no));
                        }
                        if first_project.is_none() {
                            first_project = Some((file.clone(), line_no));
                        }
                    }
                    backtrace.push(frame.raw);
                }
            } else if let Some(class) = error_class_line(line) {
                error_class = Some(class.to_owned());
            } else if let Some(value) = trimmed.strip_prefix("expected: ") {
                expected = Some(value.to_owned());
                message_lines.push(trimmed.to_owned());
            } else if let Some(value) = trimmed.strip_prefix("got: ") {
                actual = Some(value.to_owned());
                message_lines.push(trimmed.to_owned());
            } else if !trimmed.is_empty() {
                message_lines.push(trimmed.to_owned());
            }
        }
        let rerun = reruns.get(index.wrapping_sub(1));
        let (file, line_no) = match (spec_location.or(first_project), rerun) {
            (Some((file, line_no)), _) => (Some(file), Some(line_no)),
            (None, Some((_, file, line_no))) => (Some(file.clone()), *line_no),
            (None, None) => (None, None),
        };
        let rerun = match rerun {
            Some((command, _, _)) => Some(command.clone()),
            None => file.as_ref().map(|f| match line_no {
                Some(n) => format!("rspec {f}:{n}"),
                None => format!("rspec {f}"),
            }),
        };
        let message = match &error_class {
            Some(class) => {
                let rest: Vec<&str> = message_lines
                    .iter()
                    .filter(|l| !l.starts_with("Failure/Error:"))
                    .map(String::as_str)
                    .collect();
                format!("{class}: {}", rest.join("\n"))
            }
            None => message_lines.join("\n"),
        };
        failures.push(TestFailure {
            kind: if error_class.is_some() {
                FailureKind::Error
            } else {
                FailureKind::Failure
            },
            test_class: leading_constant(&description),
            test_name: description,
            file,
            line: line_no,
            message: cap_message(&message),
            expected,
            actual,
            backtrace,
            rerun,
        });
    }
    Report { counters, failures }
}

/// True iff `value` is an RSpec JSON formatter document.
fn is_rspec_document(value: &serde_json::Value) -> bool {
    value
        .get("examples")
        .is_some_and(serde_json::Value::is_array)
        && value
            .get("summary")
            .is_some_and(serde_json::Value::is_object)
}

/// Parse the document RSpec's JSON formatter printed somewhere in `output`,
/// or `None` when there is none (or it is incomplete): the caller then
/// reads the text formatter's output.
///
/// One failure per `failed` example: its location is the first project
/// frame in a `_spec.rb` file (else the first project frame, else the
/// example's own `file_path:line_number`), as the text parser takes it; the
/// rerun is `rspec <file_path>:<line_number>`, the line the text
/// formatter's `Failed examples:` prints. Each `messages` entry of a run
/// with errors outside of examples is one error record.
/// <https://rspec.info/documentation/3.13/rspec-core/RSpec/Core/Formatters/JsonFormatter.html>
pub fn parse_json(output: &str) -> Option<Report> {
    let doc = super::ruby::find_json_object(output, is_rspec_document)?;
    let summary = &doc["summary"];
    let count = |key: &str| summary.get(key).and_then(serde_json::Value::as_u64);
    let counters = Some(Counters {
        examples: count("example_count")?,
        failures: count("failure_count")?,
        pending: count("pending_count").unwrap_or(0),
        errors_outside: count("errors_outside_of_examples_count").unwrap_or(0),
    });
    let mut failures = Vec::new();
    for example in doc["examples"].as_array()? {
        if example.get("status").and_then(serde_json::Value::as_str) != Some("failed") {
            continue;
        }
        let text = |key: &str| example.get(key).and_then(serde_json::Value::as_str);
        let description = text("full_description")
            .or_else(|| text("description"))
            .unwrap_or_default()
            .to_owned();
        let spec_file = text("file_path").map(|f| f.strip_prefix("./").unwrap_or(f).to_owned());
        let spec_line = example
            .get("line_number")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok());
        let exception = example.get("exception");
        let class = exception
            .and_then(|e| e.get("class"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let raw_message = exception
            .and_then(|e| e.get("message"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let lines: Vec<&str> = raw_message
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let expected = lines
            .iter()
            .find_map(|l| l.strip_prefix("expected: "))
            .map(str::to_owned);
        let actual = lines
            .iter()
            .find_map(|l| l.strip_prefix("got: "))
            .map(str::to_owned);
        let expectation = class.is_empty() || class.starts_with("RSpec::Expectations::");
        let message = if expectation {
            lines.join("\n")
        } else {
            format!("{class}: {}", lines.join("\n"))
        };
        let mut backtrace = Vec::new();
        let mut spec_location = None;
        let mut first_project = None;
        for line in exception
            .and_then(|e| e.get("backtrace"))
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
        {
            let Some(frame) = project_frame(line) else {
                continue;
            };
            if let (Some(file), Some(line_no)) = (&frame.file, frame.line) {
                if spec_location.is_none() && file.contains("_spec.rb") {
                    spec_location = Some((file.clone(), line_no));
                }
                if first_project.is_none() {
                    first_project = Some((file.clone(), line_no));
                }
            }
            backtrace.push(frame.raw);
        }
        let (file, line) = match spec_location.or(first_project) {
            Some((file, line)) => (Some(file), Some(line)),
            None => (spec_file.clone(), spec_line),
        };
        let rerun = spec_file.as_ref().map(|f| match spec_line {
            Some(n) => format!("rspec ./{f}:{n}"),
            None => format!("rspec ./{f}"),
        });
        failures.push(TestFailure {
            kind: if expectation {
                FailureKind::Failure
            } else {
                FailureKind::Error
            },
            test_class: leading_constant(&description),
            test_name: description,
            file,
            line,
            message: cap_message(&message),
            expected,
            actual,
            backtrace,
            rerun,
        });
    }
    for message in doc
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|m| m.contains("error occurred") || m.contains("An error occurred"))
    {
        failures.push(outside_error(message));
    }
    Some(Report { counters, failures })
}

/// One error RSpec reported outside of any example: `An error occurred
/// while loading ./spec/x_spec.rb.` followed by the exception, located at
/// the file it names.
fn outside_error(message: &str) -> TestFailure {
    let file = message
        .split_whitespace()
        .find(|w| w.contains("_spec.rb"))
        .map(|w| {
            let w = w.trim_end_matches(['.', ',', ':']);
            w.strip_prefix("./").unwrap_or(w).to_owned()
        });
    let backtrace: Vec<String> = message
        .lines()
        .filter_map(project_frame)
        .map(|f| f.raw)
        .collect();
    TestFailure {
        kind: FailureKind::Error,
        test_class: None,
        test_name: message.lines().next().unwrap_or_default().trim().to_owned(),
        rerun: file.as_ref().map(|f| format!("rspec ./{f}")),
        file,
        line: None,
        message: cap_message(message.trim()),
        expected: None,
        actual: None,
        backtrace,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SINGLE: &str = include_str!("../../tests/fixtures/rspec-single-failure.txt");
    const MIXED: &str = include_str!("../../tests/fixtures/rspec-mixed.txt");
    const GREEN: &str = include_str!("../../tests/fixtures/rspec-green.txt");
    const PARALLEL: &str = include_str!("../../tests/fixtures/rspec-parallel.txt");

    #[test]
    fn detects_rspec_by_summary_or_failures_header() {
        assert!(detect(SINGLE));
        assert!(detect(GREEN));
        assert!(detect("Failures:\n"));
        assert!(detect("1 example, 0 failures"));
        assert!(!detect(
            "12 runs, 10 assertions, 1 failures, 1 errors, 0 skips"
        ));
        assert!(!detect(""));
    }

    #[test]
    fn summary_shapes() {
        assert_eq!(
            parse_summary("7 examples, 2 failures, 1 pending"),
            Some(Counters {
                examples: 7,
                failures: 2,
                pending: 1,
                errors_outside: 0
            })
        );
        assert_eq!(
            parse_summary("1 example, 1 failure"),
            Some(Counters {
                examples: 1,
                failures: 1,
                pending: 0,
                errors_outside: 0
            })
        );
        assert!(parse_summary("7 examples").is_none());
        assert!(parse_summary("12 runs, 10 assertions, 1 failures, 1 errors, 0 skips").is_none());
        let c = parse_summary("7 examples, 2 failures, 1 pending").unwrap();
        assert_eq!(c.passed(), 4);
        assert!(!c.green());
        assert!(
            parse_summary("36 examples, 0 failures, 1 pending")
                .unwrap()
                .green()
        );
    }

    #[test]
    fn single_failure_carries_every_field() {
        let report = parse(SINGLE);
        assert_eq!(
            report.counters,
            Some(Counters {
                examples: 10,
                failures: 1,
                pending: 0,
                errors_outside: 0
            })
        );
        assert_eq!(report.failures.len(), 1);
        assert_eq!(
            report.failures[0],
            TestFailure {
                kind: FailureKind::Failure,
                test_class: Some("User".into()),
                test_name: "User validation requires a name".into(),
                file: Some("spec/models/user_spec.rb".into()),
                line: Some(12),
                message: "Failure/Error: expect(user).to be_valid\nexpected #<User id: nil, name: nil> to be valid, but got errors: Name can't be blank".into(),
                expected: None,
                actual: None,
                backtrace: vec![
                    "# ./spec/models/user_spec.rb:12:in 'block (3 levels) in <top (required)>'".into()
                ],
                rerun: Some("rspec ./spec/models/user_spec.rb:10".into()),
            }
        );
    }

    #[test]
    fn mixed_run_separates_expectation_failure_and_exception() {
        let report = parse(MIXED);
        assert_eq!(report.failures.len(), 2);
        assert_eq!(report.counters.unwrap().pending, 1);

        let failure = &report.failures[0];
        assert_eq!(failure.kind, FailureKind::Failure);
        assert_eq!(failure.test_class.as_deref(), Some("Billing::Invoice"));
        assert_eq!(failure.test_name, "Billing::Invoice#total includes VAT");
        assert_eq!(failure.expected.as_deref(), Some("1200"));
        assert_eq!(failure.actual.as_deref(), Some("1000"));
        assert_eq!(
            failure.file.as_deref(),
            Some("spec/models/billing/invoice_spec.rb")
        );
        assert_eq!(failure.line, Some(23));
        assert_eq!(
            failure.rerun.as_deref(),
            Some("rspec ./spec/models/billing/invoice_spec.rb:21")
        );
        // The gem frame is dropped, the three project frames kept in order.
        assert_eq!(failure.backtrace.len(), 3);
        assert!(failure.backtrace[1].contains("spec/support/with_invoice.rb:7"));
        assert!(
            failure
                .message
                .starts_with("Failure/Error: expect(invoice.total).to eq(1200)")
        );
        assert!(failure.message.contains("(compared using ==)"));

        let error = &report.failures[1];
        assert_eq!(error.kind, FailureKind::Error);
        assert_eq!(
            error.message,
            "NoMethodError: undefined method 'render' for nil"
        );
        assert_eq!(
            error.file.as_deref(),
            Some("spec/models/billing/invoice_spec.rb")
        );
        assert_eq!(error.line, Some(31));
        assert_eq!(error.backtrace.len(), 2);
        assert!(error.backtrace[0].contains("app/models/billing/invoice.rb:88"));
        assert_eq!(
            error.rerun.as_deref(),
            Some("rspec ./spec/models/billing/invoice_spec.rb:29")
        );
    }

    #[test]
    fn green_run_has_counters_and_no_failures() {
        let report = parse(GREEN);
        assert!(report.failures.is_empty());
        let c = report.counters.unwrap();
        assert_eq!((c.examples, c.failures, c.pending), (36, 0, 1));
    }

    #[test]
    fn parallel_tests_total_line_wins_and_failure_is_kept() {
        let report = parse(PARALLEL);
        assert_eq!(
            report.counters,
            Some(Counters {
                examples: 28,
                failures: 1,
                pending: 0,
                errors_outside: 0
            })
        );
        assert_eq!(report.failures.len(), 1);
        let failure = &report.failures[0];
        assert_eq!(failure.test_class.as_deref(), Some("OrdersController"));
        assert_eq!(
            failure.file.as_deref(),
            Some("spec/requests/orders_spec.rb")
        );
        assert_eq!(failure.line, Some(31));
        assert_eq!(
            failure.rerun.as_deref(),
            Some("rspec ./spec/requests/orders_spec.rb:28")
        );
        assert!(!failure.message.contains("processes"));
    }

    #[test]
    fn rerun_line_shapes() {
        assert_eq!(
            parse_rerun_line(
                "rspec ./spec/models/user_spec.rb:10 # User validation requires a name"
            ),
            Some((
                "rspec ./spec/models/user_spec.rb:10".into(),
                "spec/models/user_spec.rb".into(),
                Some(10)
            ))
        );
        assert_eq!(
            parse_rerun_line("rspec ./spec/a_spec.rb[1:2:1] # nested"),
            Some((
                "rspec ./spec/a_spec.rb[1:2:1]".into(),
                "spec/a_spec.rb".into(),
                None
            ))
        );
        assert_eq!(
            parse_rerun_line("rspec spec/a_spec.rb"),
            Some(("rspec spec/a_spec.rb".into(), "spec/a_spec.rb".into(), None))
        );
        assert!(parse_rerun_line("bin/rails test test/x_test.rb:3").is_none());
        assert!(parse_rerun_line("rspec ").is_none());
        assert!(parse_rerun_line("rspec :3").is_none());
    }

    #[test]
    fn leading_constant_shapes() {
        assert_eq!(leading_constant("User validation"), Some("User".into()));
        assert_eq!(
            leading_constant("Billing::Invoice#total includes VAT"),
            Some("Billing::Invoice".into())
        );
        assert_eq!(leading_constant("Order.create works"), Some("Order".into()));
        assert_eq!(leading_constant("when the user is admin"), None);
        assert_eq!(leading_constant("POST /orders"), Some("POST".into()));
        assert_eq!(leading_constant("Foo::bar x"), None);
        assert_eq!(
            leading_constant("Some_Class does"),
            Some("Some_Class".into())
        );
        assert_eq!(leading_constant("Foo-bar does"), None);
        assert_eq!(
            leading_constant("Foo2::Bar_3#x y"),
            Some("Foo2::Bar_3".into())
        );
        assert_eq!(leading_constant("#total"), None);
        assert_eq!(leading_constant(""), None);
    }

    #[test]
    fn entry_without_rerun_or_spec_frame_synthesizes_rerun() {
        let output = [
            "Failures:",
            "",
            "  1) thing works",
            "     Failure/Error: expect(1).to eq(2)",
            "       expected: 2",
            "            got: 1",
            "     # ./lib/thing.rb:4:in 'run'",
            "",
            "1 example, 1 failure",
        ]
        .join("\n");
        let report = parse(&output);
        let failure = &report.failures[0];
        assert_eq!(failure.test_class, None);
        assert_eq!(failure.file.as_deref(), Some("lib/thing.rb"));
        assert_eq!(failure.line, Some(4));
        assert_eq!(failure.rerun.as_deref(), Some("rspec lib/thing.rb:4"));
        assert_eq!(failure.expected.as_deref(), Some("2"));
        assert_eq!(failure.actual.as_deref(), Some("1"));

        // Rerun known, no project frame at all: location from the rerun line.
        let output = [
            "  1) thing works",
            "     Failure/Error: boom",
            "     # /gems/x/lib/x.rb:1:in 'y'",
            "",
            "Failed examples:",
            "",
            "rspec ./spec/thing_spec.rb:9 # thing works",
        ]
        .join("\n");
        let failure = &parse(&output).failures[0];
        assert_eq!(failure.file.as_deref(), Some("spec/thing_spec.rb"));
        assert_eq!(failure.line, Some(9));
        assert!(failure.backtrace.is_empty());
        assert_eq!(
            failure.rerun.as_deref(),
            Some("rspec ./spec/thing_spec.rb:9")
        );

        // Lines after `Finished in` never reach the message.
        let failure = &parse("  1) alone\n     msg\nFinished in 1s\n     later\n").failures[0];
        assert_eq!(failure.message, "msg");
        // Nothing at all: no file, no rerun.
        let failure = &parse("  1) alone\n     Failure/Error: x\n").failures[0];
        assert_eq!(failure.file, None);
        assert_eq!(failure.rerun, None);
        assert_eq!(failure.message, "Failure/Error: x");
        // Message capped.
        let long = "y".repeat(5000);
        let failure = &parse(&format!("  1) alone\n     {long}\n")).failures[0];
        assert_eq!(failure.message.len(), 1024);
    }

    #[test]
    fn error_class_line_shapes() {
        assert_eq!(
            error_class_line("     NoMethodError:"),
            Some("NoMethodError")
        );
        assert_eq!(
            error_class_line("ActiveRecord::RecordNotFound:"),
            Some("ActiveRecord::RecordNotFound")
        );
        assert_eq!(error_class_line("Failures:"), Some("Failures"));
        assert_eq!(error_class_line("expected:"), None);
        assert_eq!(error_class_line("Failure/Error: x"), None);
        assert_eq!(error_class_line("Some words:"), None);
        assert_eq!(error_class_line(":"), None);
    }

    #[test]
    fn json_is_found_after_a_preamble_and_ignored_when_incomplete_or_foreign() {
        let mixed = include_str!("../../tests/fixtures/rspec-mixed.json");
        let report = parse_json(mixed).unwrap();
        assert_eq!(
            report.counters,
            Some(Counters {
                examples: 7,
                failures: 2,
                pending: 1,
                errors_outside: 0
            })
        );
        assert_eq!(
            report
                .failures
                .iter()
                .map(|f| (f.kind, f.test_name.as_str(), f.line))
                .collect::<Vec<_>>(),
            [
                (
                    FailureKind::Failure,
                    "Billing::Invoice#total includes VAT",
                    Some(23)
                ),
                (
                    FailureKind::Error,
                    "Billing::Invoice#to_pdf renders the template",
                    Some(31)
                ),
            ]
        );
        let truncated = include_str!("../../tests/fixtures/rspec-truncated.json");
        assert_eq!(parse_json(truncated), None);
        assert_eq!(parse_json("{\"files\": [], \"summary\": {}}"), None);
        assert_eq!(parse_json("no json here\n{ not json"), None);
        assert_eq!(parse_json(""), None);
    }

    #[test]
    fn errors_outside_of_examples_are_never_green() {
        let c =
            parse_summary("0 examples, 0 failures, 1 error occurred outside of examples").unwrap();
        assert_eq!(c.errors_outside, 1);
        assert!(!c.green());
        let load = include_str!("../../tests/fixtures/rspec-load-error.json");
        let report = parse_json(load).unwrap();
        assert!(!report.counters.unwrap().green());
        assert_eq!(
            report.failures.len(),
            1,
            "the `Run options` message is not an error"
        );
        assert_eq!(
            report.failures[0].file.as_deref(),
            Some("spec/models/order_spec.rb")
        );
        assert_eq!(
            report.failures[0].backtrace,
            [
                "# ./app/models/order.rb:3:in '<class:Order>'",
                "# ./spec/models/order_spec.rb:1:in '<top (required)>'"
            ]
        );
    }
}
