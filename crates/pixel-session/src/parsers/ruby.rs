// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Shapes shared by the Ruby test-runner parsers (Minitest, RSpec): one
//! failure record, the backtrace-line grammar, and the project-path filter.

use crate::types::Frame;

/// Whether the runner reported an assertion failure or a raised exception.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Failure,
    Error,
}

impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FailureKind::Failure => "failure",
            FailureKind::Error => "error",
        }
    }
}

/// One failing test as reported by a Ruby runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestFailure {
    pub kind: FailureKind,
    /// `UserTest`; for RSpec the leading constant of the description when
    /// there is one (`Billing::Invoice#total …` → `Billing::Invoice`).
    pub test_class: Option<String>,
    /// `test_name_is_required`, or the RSpec full description.
    pub test_name: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    /// Assertion message or `ErrorClass: message`, newlines preserved.
    pub message: String,
    pub expected: Option<String>,
    pub actual: Option<String>,
    /// Backtrace lines that point into the project (gems, the Ruby
    /// standard library and `<internal:…>` frames dropped).
    pub backtrace: Vec<String>,
    /// The runner's rerun line (`bin/rails test path:line`,
    /// `rspec ./path:line`), synthesized when the runner printed none.
    pub rerun: Option<String>,
}

/// Longest text kept for one failure message.
pub const MESSAGE_CAP: usize = 1024;

/// Cut `text` to [`MESSAGE_CAP`] bytes on a char boundary.
pub fn cap_message(text: &str) -> String {
    if text.len() <= MESSAGE_CAP {
        return text.to_owned();
    }
    // Highest boundary at or below the cap; 0 is always a boundary.
    let end = (0..=MESSAGE_CAP)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    text[..end].to_owned()
}

/// `path:line:in 'func'` / `path:line:in `func'` / `path:line` → parts.
/// Leading whitespace and RSpec's `# ` marker are tolerated; `./` is
/// stripped from the path.
pub fn parse_frame(line: &str) -> Option<Frame> {
    let raw = line.trim();
    let text = raw.strip_prefix("# ").unwrap_or(raw);
    let text = text.strip_prefix("./").unwrap_or(text);
    let (location, func) = match text.split_once(":in ") {
        Some((location, func)) => (location, Some(func)),
        None => (text, None),
    };
    let (file, line_str) = location.rsplit_once(':')?;
    // A path has an extension; the interpreter's own frames read `<internal:…>`.
    let looks_like_path = file.contains('.') || file.starts_with("<internal:");
    if file.is_empty() || !looks_like_path || file.contains(char::is_whitespace) {
        return None;
    }
    let line_no: u32 = line_str.parse().ok()?;
    let func = func.map(|f| {
        f.trim_matches(|c| c == '\'' || c == '`' || c == '"')
            .to_owned()
    });
    Some(Frame {
        raw: raw.to_owned(),
        func,
        file: Some(file.to_owned()),
        line: Some(line_no),
        ..Frame::default()
    })
}

/// A frame belongs to the project unless it points into an installed gem,
/// the Ruby standard library, bundler, or the interpreter's internals.
pub fn is_project_frame(frame: &Frame) -> bool {
    let Some(file) = frame.file.as_deref() else {
        return false;
    };
    !(file.contains("/gems/")
        || file.contains("/ruby/")
        || file.contains("/bundler/")
        || file.starts_with("<internal:"))
}

/// Parse `line` as a backtrace frame and keep it only when it is a project
/// frame.
pub fn project_frame(line: &str) -> Option<Frame> {
    parse_frame(line).filter(is_project_frame)
}

/// Parse `"12 runs"` / `"1 failure"` style counters out of a summary line:
/// every `<digits> <word>` pair, keyed by the word with any trailing `s`
/// and `,` removed (`runs` → `run`, `failures,` → `failure`).
pub fn counters(summary: &str) -> Vec<(String, u64)> {
    let words: Vec<&str> = summary.split_whitespace().collect();
    let mut out = Vec::new();
    for pair in words.windows(2) {
        let Ok(value) = pair[0].parse::<u64>() else {
            continue;
        };
        let key = pair[1].trim_end_matches(',').trim_end_matches('s');
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphabetic()) {
            continue;
        }
        out.push((key.to_owned(), value));
    }
    out
}

/// Split `lines` into blocks: each block starts at a line `is_start`
/// accepts (kept as its first line) and runs up to the next such line.
/// Lines before the first start are dropped.
pub fn blocks<'a>(lines: &'a [&'a str], is_start: impl Fn(&str) -> bool) -> Vec<&'a [&'a str]> {
    let starts: Vec<usize> = (0..lines.len()).filter(|&k| is_start(lines[k])).collect();
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let end = starts
                .get(n.wrapping_add(1))
                .copied()
                .unwrap_or(lines.len());
            &lines[start..end]
        })
        .collect()
}

/// Look one counter up by its singular key.
pub fn counter(counters: &[(String, u64)], key: &str) -> Option<u64> {
    counters.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ruby34_and_legacy_frames() {
        let modern =
            parse_frame("    test/models/user_test.rb:20:in 'block in <class:UserTest>'").unwrap();
        assert_eq!(modern.file.as_deref(), Some("test/models/user_test.rb"));
        assert_eq!(modern.line, Some(20));
        assert_eq!(modern.func.as_deref(), Some("block in <class:UserTest>"));
        assert_eq!(
            modern.raw,
            "test/models/user_test.rb:20:in 'block in <class:UserTest>'"
        );

        let legacy = parse_frame("app/models/order.rb:10:in `total'").unwrap();
        assert_eq!(legacy.func.as_deref(), Some("total"));
        assert_eq!(legacy.line, Some(10));

        let rspec = parse_frame(
            "     # ./spec/models/user_spec.rb:12:in 'block (3 levels) in <top (required)>'",
        )
        .unwrap();
        assert_eq!(rspec.file.as_deref(), Some("spec/models/user_spec.rb"));
        assert_eq!(rspec.line, Some(12));

        let internal = parse_frame("<internal:kernel>:90:in 'Kernel#tap'").unwrap();
        assert_eq!(internal.file.as_deref(), Some("<internal:kernel>"));
        assert!(!is_project_frame(&internal));

        let bare = parse_frame("lib/x.rb:7").unwrap();
        assert_eq!(bare.line, Some(7));
        assert_eq!(bare.func, None);
    }

    #[test]
    fn rejects_non_frames() {
        assert!(parse_frame("Expected false to be truthy.").is_none());
        assert!(parse_frame("bin/rails test test/models/user_test.rb:10").is_none());
        assert!(parse_frame("Expected: 1200").is_none());
        assert!(parse_frame(":12:in 'x'").is_none());
        assert!(parse_frame("noext:12").is_none());
        assert!(parse_frame("x.rb:abc:in 'y'").is_none());
        assert!(parse_frame("Expected 1.5: 3").is_none());
        assert!(parse_frame("").is_none());
    }

    #[test]
    fn project_filter_drops_gems_stdlib_and_internals() {
        let keep = |s: &str| project_frame(s).is_some();
        assert!(keep("test/models/user_test.rb:20:in 'x'"));
        assert!(keep("/app/test/models/user_test.rb:20:in 'x'"));
        assert!(!keep(
            "/usr/local/bundle/gems/minitest-5.25.4/lib/minitest/test.rb:94:in 'x'"
        ));
        assert!(!keep(
            "/opt/ruby/3.4.1/lib/ruby/3.4.0/forwardable.rb:240:in 'x'"
        ));
        assert!(!keep("/usr/lib/ruby/bundler/cli.rb:1:in 'x'"));
        assert!(!keep("<internal:kernel>:90:in 'Kernel#tap'"));
        assert!(!is_project_frame(&Frame::default()));
    }

    #[test]
    fn counters_read_every_number_word_pair() {
        let c = counters("8 runs, 12 assertions, 1 failures, 1 errors, 1 skips");
        assert_eq!(counter(&c, "run"), Some(8));
        assert_eq!(counter(&c, "assertion"), Some(12));
        assert_eq!(counter(&c, "failure"), Some(1));
        assert_eq!(counter(&c, "error"), Some(1));
        assert_eq!(counter(&c, "skip"), Some(1));
        assert_eq!(counter(&c, "pending"), None);

        let c = counters("7 examples, 2 failures, 1 pending");
        assert_eq!(counter(&c, "example"), Some(7));
        assert_eq!(counter(&c, "failure"), Some(2));
        assert_eq!(counter(&c, "pending"), Some(1));

        let c = counters("10 examples, 1 failure");
        assert_eq!(counter(&c, "failure"), Some(1));
        assert!(
            counters("Finished in 0.5 seconds (files took 1.2 seconds to load)")
                .iter()
                .all(|(k, _)| k == "second")
        );
        assert_eq!(counters("12 34 x"), vec![("x".to_owned(), 34)]);
        assert!(counters("3 a1").is_empty());
        assert!(counters("").is_empty());
    }

    #[test]
    fn blocks_split_at_every_start_and_drop_the_preamble() {
        let lines = ["noise", "A", "1", "2", "A", "A", "3"];
        let got = blocks(&lines, |l| l == "A");
        assert_eq!(got, vec![&["A", "1", "2"][..], &["A"][..], &["A", "3"][..]]);
        assert!(blocks(&lines, |l| l == "Z").is_empty());
        assert!(blocks(&[], |_| true).is_empty());
        assert_eq!(blocks(&["A"], |l| l == "A"), vec![&["A"][..]]);
    }

    #[test]
    fn cap_message_cuts_on_char_boundary() {
        assert_eq!(cap_message("short"), "short");
        // 3-byte chars: 1024 is not a boundary, the cut must step back to 1023.
        let long = "€".repeat(MESSAGE_CAP);
        let capped = cap_message(&long);
        assert_eq!(capped.len(), 1023);
        assert!(capped.chars().all(|c| c == '€'));
        let exact = "a".repeat(MESSAGE_CAP);
        assert_eq!(cap_message(&exact).len(), MESSAGE_CAP);
    }
}
