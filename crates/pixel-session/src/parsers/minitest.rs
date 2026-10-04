// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Parser for Minitest (and `rails test`) output:
//!
//! ```text
//! # Running:
//! ..F.E
//!
//! Failure:
//! UserTest#test_name_is_required [test/models/user_test.rb:12]:
//! Expected false to be truthy.
//!
//! bin/rails test test/models/user_test.rb:10
//!
//! Error:
//! UserTest#test_boom:
//! NoMethodError: undefined method 'foo' for nil
//!     test/models/user_test.rb:20:in 'block in <class:UserTest>'
//!
//! 12 runs, 10 assertions, 1 failures, 1 errors, 0 skips
//! ```

use super::ruby::{
    FailureKind, TestFailure, blocks, cap_message, counter, counters, parse_frame, project_frame,
};

/// The counters of the `N runs, N assertions, …` summary line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    pub runs: u64,
    pub assertions: u64,
    pub failures: u64,
    pub errors: u64,
    pub skips: u64,
}

impl Counters {
    /// Tests that ran and neither failed, errored nor were skipped.
    pub fn passed(&self) -> u64 {
        self.runs
            .saturating_sub(self.failures)
            .saturating_sub(self.errors)
            .saturating_sub(self.skips)
    }

    /// The run is green when nothing failed or errored (skips are fine).
    pub fn green(&self) -> bool {
        self.failures == 0 && self.errors == 0
    }
}

/// A parsed Minitest run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub counters: Option<Counters>,
    pub failures: Vec<TestFailure>,
}

/// Parse the `N runs, N assertions, N failures, N errors, N skips` line.
pub fn parse_summary(line: &str) -> Option<Counters> {
    let text = line.trim();
    if !text.contains(" runs, ") && !text.contains(" run, ") {
        return None;
    }
    let c = counters(text);
    Some(Counters {
        runs: counter(&c, "run")?,
        assertions: counter(&c, "assertion")?,
        failures: counter(&c, "failure")?,
        errors: counter(&c, "error")?,
        skips: counter(&c, "skip")?,
    })
}

/// Whether the output came from Minitest: the `# Running:` banner or the
/// summary line is present.
pub fn detect(output: &str) -> bool {
    output
        .lines()
        .any(|l| l.trim() == "# Running:" || parse_summary(l).is_some())
}

/// A line the Rails/Minitest reporter prints to rerun one test.
pub fn is_rerun_line(line: &str) -> bool {
    let text = line.trim();
    [
        "bin/rails test ",
        "rails test ",
        "bundle exec rails test ",
        "ruby -I",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

/// The parts of a failure header: class, test name, file, line.
struct Header {
    class: Option<String>,
    name: String,
    file: Option<String>,
    line: Option<u32>,
}

/// `Class#test_name [file:line]:` or `Class#test_name:` → parts.
fn parse_header(line: &str) -> Option<Header> {
    let text = line.trim().strip_suffix(':')?;
    let (ident, location) = match text.rsplit_once(" [") {
        Some((ident, loc)) if loc.ends_with(']') => (ident, Some(&loc[..loc.len() - 1])),
        _ => (text, None),
    };
    let (class, name) = match ident.split_once('#') {
        Some((class, name)) => (Some(class.to_owned()), name.to_owned()),
        None => (None, ident.to_owned()),
    };
    if name.is_empty() {
        return None;
    }
    let (file, line_no) = match location.and_then(|l| l.rsplit_once(':')) {
        Some((file, n)) => (Some(file.to_owned()), n.parse().ok()),
        None => (None, None),
    };
    Some(Header {
        class,
        name,
        file,
        line: line_no,
    })
}

fn is_block_start(line: &str) -> Option<FailureKind> {
    match line.trim() {
        "Failure:" => Some(FailureKind::Failure),
        "Error:" => Some(FailureKind::Error),
        _ => None,
    }
}

/// Parse a full captured output into counters and one entry per failing
/// test. Lines outside `Failure:`/`Error:` blocks are ignored, so worker
/// noise between blocks does not matter.
pub fn parse(output: &str) -> Report {
    let lines: Vec<&str> = output.lines().collect();
    let counters = lines.iter().rev().find_map(|l| parse_summary(l));
    let mut failures = Vec::new();
    for block in blocks(&lines, |l| is_block_start(l).is_some()) {
        let Some(kind) = is_block_start(block[0]) else {
            continue;
        };
        // Header: first non-empty line after the marker.
        let body = &block[1..];
        let Some(header_at) = body.iter().position(|l| !l.trim().is_empty()) else {
            continue;
        };
        let Some(header) = parse_header(body[header_at]) else {
            continue;
        };
        let Header {
            class: test_class,
            name: test_name,
            mut file,
            line: mut line_no,
        } = header;
        let mut message_lines: Vec<String> = Vec::new();
        let mut expected = None;
        let mut actual = None;
        let mut backtrace = Vec::new();
        let mut rerun = None;
        for line in &body[header_at + 1..] {
            let trimmed = line.trim();
            if trimmed.starts_with("Finished in ") {
                break;
            }
            if is_rerun_line(line) {
                rerun = Some(trimmed.to_owned());
                break;
            }
            if let Some(frame) = project_frame(line) {
                backtrace.push(frame.raw);
            } else if parse_frame(line).is_some() {
                // A gem/stdlib frame: filtered out, not a message line.
            } else if let Some(value) = trimmed.strip_prefix("Expected: ") {
                expected = Some(value.to_owned());
                message_lines.push(trimmed.to_owned());
            } else if let Some(value) = trimmed.strip_prefix("Actual: ") {
                actual = Some(value.to_owned());
                message_lines.push(trimmed.to_owned());
            } else if !trimmed.is_empty() {
                message_lines.push(trimmed.to_owned());
            }
        }
        if file.is_none()
            && let Some(first) = backtrace.first().and_then(|l| project_frame(l))
        {
            file = first.file;
            line_no = first.line;
        }
        if rerun.is_none()
            && let Some(f) = &file
        {
            rerun = Some(format!("ruby -Itest {f} -n {test_name}"));
        }
        failures.push(TestFailure {
            kind,
            test_class,
            test_name,
            file,
            line: line_no,
            message: cap_message(&message_lines.join("\n")),
            expected,
            actual,
            backtrace,
            rerun,
        });
    }
    Report { counters, failures }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SINGLE: &str = include_str!("../../tests/fixtures/minitest-single-failure.txt");
    const MIXED: &str = include_str!("../../tests/fixtures/minitest-mixed.txt");
    const GREEN: &str = include_str!("../../tests/fixtures/minitest-green.txt");
    const PARALLEL: &str = include_str!("../../tests/fixtures/minitest-parallel.txt");

    #[test]
    fn detects_minitest_by_banner_or_summary() {
        assert!(detect(SINGLE));
        assert!(detect(GREEN));
        assert!(detect(
            "12 runs, 10 assertions, 1 failures, 1 errors, 0 skips"
        ));
        assert!(detect("# Running:\n"));
        assert!(!detect("10 examples, 1 failure"));
        assert!(!detect(""));
    }

    #[test]
    fn summary_needs_all_five_counters() {
        assert_eq!(
            parse_summary("12 runs, 10 assertions, 1 failures, 1 errors, 0 skips"),
            Some(Counters {
                runs: 12,
                assertions: 10,
                failures: 1,
                errors: 1,
                skips: 0,
            })
        );
        assert_eq!(
            parse_summary("1 run, 1 assertion, 0 failures, 0 errors, 0 skips").map(|c| c.runs),
            Some(1)
        );
        assert!(parse_summary("12 runs, 10 assertions, 1 failures").is_none());
        assert!(parse_summary("10 examples, 1 failure").is_none());
        assert!(parse_summary("Finished in 0.4s, 24.2 runs/s, 26.6 assertions/s.").is_none());
    }

    #[test]
    fn counters_derive_passed_and_green() {
        let c = parse_summary("8 runs, 12 assertions, 1 failures, 1 errors, 1 skips").unwrap();
        assert_eq!(c.passed(), 5);
        assert!(!c.green());
        let c = parse_summary("45 runs, 120 assertions, 0 failures, 0 errors, 2 skips").unwrap();
        assert_eq!(c.passed(), 43);
        assert!(c.green());
        assert!(
            !parse_summary("1 runs, 1 assertions, 0 failures, 1 errors, 0 skips")
                .unwrap()
                .green()
        );
        assert!(
            !parse_summary("1 runs, 1 assertions, 1 failures, 0 errors, 0 skips")
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
                runs: 10,
                assertions: 11,
                failures: 1,
                errors: 0,
                skips: 0
            })
        );
        assert_eq!(report.failures.len(), 1);
        assert_eq!(
            report.failures[0],
            TestFailure {
                kind: FailureKind::Failure,
                test_class: Some("UserTest".into()),
                test_name: "test_name_is_required".into(),
                file: Some("test/models/user_test.rb".into()),
                line: Some(12),
                message: "Expected false to be truthy.".into(),
                expected: None,
                actual: None,
                backtrace: vec![],
                rerun: Some("bin/rails test test/models/user_test.rb:10".into()),
            }
        );
    }

    #[test]
    fn mixed_run_separates_failure_and_error() {
        let report = parse(MIXED);
        assert_eq!(report.failures.len(), 2);
        let failure = &report.failures[0];
        assert_eq!(failure.kind, FailureKind::Failure);
        assert_eq!(failure.test_class.as_deref(), Some("Billing::InvoiceTest"));
        assert_eq!(failure.test_name, "test_total_includes_vat");
        assert_eq!(failure.line, Some(23));
        assert_eq!(failure.expected.as_deref(), Some("1200"));
        assert_eq!(failure.actual.as_deref(), Some("1000"));
        assert_eq!(failure.message, "Expected: 1200\nActual: 1000");
        assert_eq!(
            failure.rerun.as_deref(),
            Some("bin/rails test test/models/billing/invoice_test.rb:19")
        );

        let error = &report.failures[1];
        assert_eq!(error.kind, FailureKind::Error);
        assert_eq!(error.test_name, "test_pdf_generation");
        assert_eq!(
            error.message,
            "NoMethodError: undefined method 'render' for nil"
        );
        assert_eq!(error.expected, None);
        // Location comes from the first project frame when the header has none.
        assert_eq!(error.file.as_deref(), Some("app/models/billing/invoice.rb"));
        assert_eq!(error.line, Some(88));
        assert_eq!(
            error.backtrace,
            vec![
                "app/models/billing/invoice.rb:88:in 'Billing::Invoice#to_pdf'",
                "test/models/billing/invoice_test.rb:31:in 'block in <class:InvoiceTest>'",
            ]
        );
        assert_eq!(
            error.rerun.as_deref(),
            Some("bin/rails test test/models/billing/invoice_test.rb:29")
        );
        assert_eq!(report.counters.unwrap().errors, 1);
        assert_eq!(report.counters.unwrap().skips, 1);
    }

    #[test]
    fn green_run_has_counters_and_no_failures() {
        let report = parse(GREEN);
        assert!(report.failures.is_empty());
        let c = report.counters.unwrap();
        assert_eq!((c.runs, c.assertions, c.skips), (45, 120, 2));
        assert!(c.green());
    }

    #[test]
    fn parallel_worker_noise_does_not_leak_into_records() {
        let report = parse(PARALLEL);
        assert_eq!(report.failures.len(), 2);
        assert_eq!(report.counters.unwrap().runs, 24);
        let failure = &report.failures[0];
        assert_eq!(failure.test_class.as_deref(), Some("OrdersControllerTest"));
        assert_eq!(
            failure.message,
            "Expected response to be a <3XX: redirect>, but was a <422: Unprocessable Content>"
        );
        assert_eq!(
            failure.rerun.as_deref(),
            Some("bin/rails test test/controllers/orders_controller_test.rb:9")
        );
        let error = &report.failures[1];
        assert_eq!(error.kind, FailureKind::Error);
        assert_eq!(error.test_class.as_deref(), Some("TokenTest"));
        assert_eq!(error.file.as_deref(), Some("test/models/token_test.rb"));
        assert_eq!(error.line, Some(27));
        assert_eq!(error.backtrace.len(), 1);
        assert!(!error.message.contains("DEPRECATION"));
        assert!(!failure.message.contains("Worker"));
    }

    type Parts = (Option<String>, String, Option<String>, Option<u32>);

    fn header_parts(line: &str) -> Option<Parts> {
        parse_header(line).map(|h| (h.class, h.name, h.file, h.line))
    }

    #[test]
    fn header_shapes() {
        assert_eq!(
            header_parts("UserTest#test_x [test/models/user_test.rb:12]:"),
            Some((
                Some("UserTest".into()),
                "test_x".into(),
                Some("test/models/user_test.rb".into()),
                Some(12)
            ))
        );
        assert_eq!(
            header_parts("UserTest#test_x:"),
            Some((Some("UserTest".into()), "test_x".into(), None, None))
        );
        assert_eq!(
            header_parts("test_alone:"),
            Some((None, "test_alone".into(), None, None))
        );
        assert_eq!(
            header_parts("T#test_y [t.rb:x]:"),
            Some((Some("T".into()), "test_y".into(), Some("t.rb".into()), None))
        );
        // An unclosed bracket is not a location: the ident keeps it whole.
        assert_eq!(
            header_parts("T#test_y [t.rb:3:"),
            Some((Some("T".into()), "test_y [t.rb:3".into(), None, None))
        );
        assert!(header_parts("no trailing colon").is_none());
        assert!(header_parts("UserTest#:").is_none());
    }

    #[test]
    fn block_without_header_or_rerun_is_tolerated() {
        // No rerun line, no location in the header: rerun is synthesized
        // from the first project frame; the block ends at `Finished in`.
        let output = [
            "Error:",
            "FooTest#test_boom:",
            "RuntimeError: boom",
            "    test/foo_test.rb:5:in 'block in <class:FooTest>'",
            "",
            "Finished in 0.1s, 10 runs/s.",
            "1 runs, 0 assertions, 0 failures, 1 errors, 0 skips",
        ]
        .join("\n");
        let report = parse(&output);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(
            report.failures[0].rerun.as_deref(),
            Some("ruby -Itest test/foo_test.rb -n test_boom")
        );
        assert_eq!(report.failures[0].message, "RuntimeError: boom");

        // A marker followed by garbage is skipped, the next block still parses.
        let output = [
            "Failure:",
            "not a header",
            "Failure:",
            "BarTest#test_ok [test/bar_test.rb:3]:",
            "nope",
            "bin/rails test test/bar_test.rb:2",
        ]
        .join("\n");
        let report = parse(&output);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].test_name, "test_ok");

        // Trailing marker with nothing after it, or only blank lines.
        assert!(parse("Failure:").failures.is_empty());
        assert!(parse("Failure:\n\n\n").failures.is_empty());
        // Lines after the rerun line, or after `Finished in`, are not message.
        let report = parse("Failure:\nT#test_a [t.rb:1]:\nmsg\nbin/rails test t.rb:1\nextra\n");
        assert_eq!(report.failures[0].message, "msg");
        let report = parse("Failure:\nT#test_a [t.rb:1]:\nmsg\nFinished in 1s\nextra\n");
        assert_eq!(report.failures[0].message, "msg");
        // No location anywhere: rerun stays unknown.
        let report = parse("Error:\nBazTest#test_x:\nboom\n");
        assert_eq!(report.failures[0].rerun, None);
        assert_eq!(report.failures[0].file, None);
    }

    #[test]
    fn rerun_line_shapes() {
        assert!(is_rerun_line("bin/rails test test/x_test.rb:3"));
        assert!(is_rerun_line("  rails test test/x_test.rb:3"));
        assert!(is_rerun_line("bundle exec rails test test/x_test.rb:3"));
        assert!(is_rerun_line("ruby -Itest test/x_test.rb -n test_y"));
        assert!(!is_rerun_line("Expected: rails test"));
        assert!(!is_rerun_line(""));
    }

    #[test]
    fn message_is_capped() {
        let long = "x".repeat(5000);
        let output = format!("Failure:\nT#test_a [t.rb:1]:\n{long}\nbin/rails test t.rb:1\n");
        assert_eq!(parse(&output).failures[0].message.len(), 1024);
    }
}
