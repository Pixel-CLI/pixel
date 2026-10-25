// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The small string rules the ported probe scripts share, spelled the way
//! Python spells them, so a step prints the script's bytes.

use std::sync::LazyLock;

use regex::Regex;

use super::Ctx;

/// The source files the scripts read: `SRC` of the code-reading probes.
pub(super) static SRC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\.(rs|py|js|jsx|ts|tsx|go|java|kt|c|h|cc|cpp|hpp|rb|php|swift|cs|scala|sh|lua)$")
        .expect("the source pattern is a literal")
});

/// A path of a test file.
pub(super) static TESTPATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(^|/)(tests?|__tests__|spec|testdata)/|[._-](test|spec)s?\.\w+$|(^|/)test_\w+\.\w+$|_tests?\.\w+$",
    )
    .expect("the test path pattern is a literal")
});

/// A leading comment, attribute or decorator line of a declaration.
pub(super) static COMMENT_START: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(//|#\[|#\s|/\*|\*|@)").expect("the comment pattern is a literal")
});

/// What a brace count ignores: string and character literals and line comments.
pub(super) static BRACE_NOISE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])'|//.*$"#)
        .expect("the noise pattern is a literal")
});

/// A line that declares a function or a test: the name is group 1 for a
/// function, 2 for a macro-registered one, 3 for `it("...")`.
pub(super) static DECL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+|unsafe\s+|const\s+|extern\s+"[^"]*"\s+|static\s+|export\s+|default\s+)*(?:fn|def|function|func)\s+([A-Za-z_]\w*)"#,
        r#"|^\s*(?:[A-Za-z_]\w*!)\(\s*([A-Za-z_]\w*)\s*,\s*(?:\||move\s*\||\{|fn\b)"#,
        r#"|^\s*(?:test|it|describe)\(\s*['"`]([^'"`]+)"#,
    ))
    .expect("the declaration pattern is a literal")
});

/// `text[:count]` of Python: the first `count` characters.
pub(super) fn head(text: &str, count: usize) -> &str {
    text.char_indices()
        .nth(count)
        .map_or(text, |(end, _)| &text[..end])
}

/// `re.escape(text)` of Python 3.7+: a backslash before each character that
/// can mean something in a pattern, which the `drop:` line is read as.
pub(super) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if "()[]{}?*+-|^$\\.&~# \t\n\r\x0b\x0c".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// The lines of the tracked file `path`, empty when it cannot be read within
/// `limit` bytes (the scripts' `read(root, path)`).
pub(super) fn lines_of(ctx: &Ctx, path: &str, limit: u64) -> Vec<String> {
    ctx.read(path, limit)
        .map_or_else(Vec::new, |text| splitlines(&text))
}

/// `text.splitlines()` of Python: a line ends at `\n`, `\r\n`, `\r`, vertical
/// tab, form feed, the three separators `\x1c` to `\x1e`, NEL and the two
/// Unicode line and paragraph separators (`str::lines` knows only the first
/// two).
pub(super) fn splitlines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(std::mem::take(&mut line));
            }
            '\n' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}'
            | '\u{2029}' => out.push(std::mem::take(&mut line)),
            _ => line.push(ch),
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

/// A word without one plural or tense ending, lowercased: the first of
/// `ing`, `ed`, `es`, `s` the word ends with and keeps four letters after
/// losing.
pub(super) fn stem(word: &str) -> String {
    let word = word.to_lowercase();
    let length = word.chars().count();
    for suffix in ["ing", "ed", "es", "s"] {
        if length >= suffix.len() + 4
            && let Some(cut) = word.strip_suffix(suffix)
        {
            return cut.to_owned();
        }
    }
    word
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_counts_characters_not_bytes() {
        assert_eq!(head("héllo", 2), "hé");
        assert_eq!(head("héllo", 5), "héllo");
        assert_eq!(head("héllo", 9), "héllo");
        assert_eq!(head("héllo", 0), "");
        assert_eq!(head("", 3), "");
    }

    #[test]
    fn escape_is_pythons_re_escape() {
        assert_eq!(escape("a/b.rs:3-9"), "a/b\\.rs:3\\-9");
        assert_eq!(escape("x y\t#&~"), "x\\ y\\\t\\#\\&\\~");
        assert_eq!(
            escape("(a|b)[c]{d}?*+^$\\"),
            "\\(a\\|b\\)\\[c\\]\\{d\\}\\?\\*\\+\\^\\$\\\\"
        );
        assert_eq!(escape("é_1!%\"'/:;<=>@`,"), "é_1!%\"'/:;<=>@`,");
    }

    #[test]
    fn the_patterns_match_what_the_scripts_match() {
        assert!(
            SRC.is_match("a/b.rs")
                && SRC.is_match("x.tsx")
                && !SRC.is_match("x.md")
                && !SRC.is_match("rs")
        );
        for yes in [
            "tests/a.rs",
            "a/__tests__/b.js",
            "x.test.ts",
            "a/test_b.py",
            "b_test.go",
            "c/spec/d.rb",
        ] {
            assert!(TESTPATH.is_match(yes), "{yes}");
        }
        for no in ["src/latest.rs", "a/contest.py", "testing.rs"] {
            assert!(!TESTPATH.is_match(no), "{no}");
        }
        let name = |line: &str| {
            let caps = DECL.captures(line)?;
            (1..=3)
                .find_map(|k| caps.get(k))
                .map(|m| m.as_str().to_owned())
        };
        assert_eq!(
            name("  pub(crate) async fn run_it() {").as_deref(),
            Some("run_it")
        );
        assert_eq!(
            name("export default function go() {").as_deref(),
            Some("go")
        );
        assert_eq!(name("    def test_a(self):").as_deref(), Some("test_a"));
        assert_eq!(name("  case!(named, |x| x);").as_deref(), Some("named"));
        assert_eq!(
            name("  it('stops early', () => {").as_deref(),
            Some("stops early")
        );
        assert_eq!(name("let fn_name = 3;"), None);
    }

    #[test]
    fn splitlines_is_pythons() {
        assert_eq!(splitlines("a\nb\r\nc\rd"), ["a", "b", "c", "d"]);
        assert_eq!(splitlines("a\n\nb\n"), ["a", "", "b"]);
        assert_eq!(splitlines("x\x0cy\x0bz\u{2028}w"), ["x", "y", "z", "w"]);
        assert_eq!(splitlines(""), Vec::<String>::new());
        assert_eq!(splitlines("\n"), [""]);
        assert_eq!(splitlines("tab\tstays"), ["tab\tstays"]);
    }

    #[test]
    fn stem_cuts_one_ending_and_keeps_four_letters() {
        assert_eq!(stem("Stopping"), "stopp");
        assert_eq!(stem("walked"), "walk");
        assert_eq!(stem("boxes"), "boxe", "`es` would leave three letters");
        assert_eq!(stem("calls"), "call");
        assert_eq!(stem("runs"), "runs", "run is three letters long");
        assert_eq!(stem("callers"), "caller");
        assert_eq!(stem("string"), "string", "str would be three letters");
        assert_eq!(stem("used"), "used");
    }
}
