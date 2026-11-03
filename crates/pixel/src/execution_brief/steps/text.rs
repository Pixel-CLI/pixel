// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The small string rules the steps share, spelled the way Python spells
//! them, so a step prints the script's bytes: a probe script's `open(...)`,
//! `.splitlines()`, `re.escape` and slices. Rust's `lines()` and
//! `split_whitespace()` cut at fewer characters, so a form feed or a lone
//! `\r` in a file would change what a step quotes.

use std::sync::LazyLock;

use regex::Regex;

use super::Ctx;

/// The source files the scripts read: `SRC` of the code-reading probes.
pub(super) static SRC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\.(rs|py|js|jsx|ts|tsx|go|java|kt|c|h|cc|cpp|hpp|rb|php|swift|cs|scala|sh|lua)$")
        .expect("the source pattern is a literal")
});

/// A word of four letters or more of a prompt.
pub(super) static LETTERS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z]{4,}").expect("the letters pattern is a literal"));

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

/// `text` as a regex that matches only it, spelled like Python's `re.escape`
/// (3.7 and later): the characters that can mean something, and the space, `#`,
/// `&`, `-` and `~`, get a backslash. A probe script printed its directives with
/// that function; the same spelling keeps a step's bytes the script's.
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
        .map_or_else(Vec::new, |text| splitlines_owned(&text))
}

/// `text.splitlines()` of Python: a line ends at `\n`, `\r\n`, `\r`, vertical
/// tab, form feed, the three separators `\x1c` to `\x1e`, NEL and the two
/// Unicode line and paragraph separators (`str::lines` knows only the first
/// two); no empty last line.
pub(super) fn splitlines(text: &str) -> Vec<&str> {
    let ends = |ch: char| {
        matches!(
            ch,
            '\n' | '\r' | '\x0b' | '\x0c' | '\x1c'..='\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}'
        )
    };
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        if !ends(ch) {
            continue;
        }
        lines.push(&text[start..at]);
        start = at + ch.len_utf8();
        if ch == '\r' && chars.next_if(|(_, next)| *next == '\n').is_some() {
            start += 1;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// [`splitlines`], each line owned.
pub(super) fn splitlines_owned(text: &str) -> Vec<String> {
    splitlines(text).into_iter().map(str::to_owned).collect()
}

/// `text` as Python's text-mode `open()` returns it: `\r\n` and a lone `\r`
/// are `\n`.
pub(super) fn universal_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
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

/// `word` lowercased with a trailing `ing`, `es` or `s` cut (the first that
/// fits) when it is longer than four bytes: the stem of `doc-sections`,
/// `name-files` and `rank-blocks`. Unlike [`stem`], `ed` stays and the
/// length is the word's before the cut.
pub(super) fn stem_ing_es_s(word: &str) -> String {
    let word = word.to_lowercase();
    for suffix in ["ing", "es", "s"] {
        if word.len() > 4
            && let Some(cut) = word.strip_suffix(suffix)
        {
            return cut.to_owned();
        }
    }
    word
}

/// The words of code or prose, split at case changes, digits and punctuation,
/// lowercased: `HTTPServerError2` is `http`, `server`, `error`, `2`. The same
/// words as `re.findall(r"[A-Z]+(?![a-z])|[A-Z]?[a-z]+|\d+", text)` in a probe
/// script: a run of capitals stops before the capital that starts the next
/// word.
pub(super) fn subwords(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let run_end = |from: usize, keep: fn(char) -> bool| {
        from + chars[from..].iter().take_while(|ch| keep(**ch)).count()
    };
    let lower = |ch: char| ch.is_ascii_lowercase();
    let upper = |ch: char| ch.is_ascii_uppercase();
    let word = |from: usize, to: usize| chars[from..to].iter().collect::<String>().to_lowercase();
    let mut words = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        if upper(chars[at]) {
            let capitals = run_end(at, upper);
            let before_lower = chars.get(capitals).copied().is_some_and(lower);
            // `HTTPServer`: the capitals but the last, which starts `Server`.
            let end = if before_lower { capitals - 1 } else { capitals };
            if end > at {
                words.push(word(at, end));
                at = end;
                continue;
            }
            let end = run_end(at + 1, lower);
            words.push(word(at, end));
            at = end;
        } else if lower(chars[at]) {
            let end = run_end(at, lower);
            words.push(word(at, end));
            at = end;
        } else if chars[at].is_ascii_digit() {
            let end = run_end(at, |ch| ch.is_ascii_digit());
            words.push(word(at, end));
            at = end;
        } else {
            at += 1;
        }
    }
    words
}

/// `str.isupper()`: at least one cased character and no lowercase one.
pub(super) fn is_upper(text: &str) -> bool {
    text.chars().any(char::is_uppercase) && !text.chars().any(char::is_lowercase)
}

/// `str.islower()`: at least one cased character and no uppercase one.
pub(super) fn is_lower(text: &str) -> bool {
    text.chars().any(char::is_lowercase) && !text.chars().any(char::is_uppercase)
}

/// Whether `ch` can be part of a word: what `rg -w` and `\w` call one.
pub(super) fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// `len(line) - len(line.lstrip())`, in characters.
pub(super) fn indent(line: &str) -> usize {
    line.chars().count() - line.trim_start().chars().count()
}

/// `os.path.basename`: the last `/`-separated segment of `path`.
pub(super) fn last_segment(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `Path(path).suffix` of Python: the last `.ext` of the file name, if the
/// name does not start or end with the dot.
pub(super) fn suffix(path: &str) -> &str {
    let name = last_segment(path);
    match name.rfind('.') {
        Some(dot) if dot > 0 && dot + 1 < name.len() => &name[dot..],
        _ => "",
    }
}

/// A pattern of the source: a literal, so it always compiles.
pub(super) fn re(source: &str) -> Regex {
    Regex::new(source).expect("the pattern is a literal")
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
        assert_eq!(splitlines(""), Vec::<&str>::new());
        assert_eq!(splitlines("\n"), [""]);
        assert_eq!(splitlines_owned("a\r\nb"), ["a", "b"]);
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

    #[test]
    fn stem_ing_es_s_cuts_after_four_bytes_and_keeps_ed() {
        assert_eq!(stem_ing_es_s("Boxes"), "box");
        assert_eq!(stem_ing_es_s("calls"), "call");
        assert_eq!(stem_ing_es_s("runs"), "runs", "four bytes stay");
        assert_eq!(stem_ing_es_s("walked"), "walked");
        assert_eq!(stem_ing_es_s("String"), "str");
    }

    #[test]
    fn path_and_line_shapes_are_pythons() {
        assert_eq!(last_segment("a/b/c.rs"), "c.rs");
        assert_eq!(last_segment("c.rs"), "c.rs");
        assert_eq!(suffix("a/b.tar.gz"), ".gz");
        assert_eq!(suffix("a/.env"), "");
        assert_eq!(suffix("a/b."), "");
        assert_eq!(suffix("a.b/c"), "");
        assert_eq!(indent("  \té x"), 3);
        assert!(is_word('é') && is_word('_') && is_word('9') && !is_word('-'));
    }

    #[test]
    fn subwords_split_like_the_script_regex() {
        assert_eq!(
            subwords("HTTPServerError2"),
            ["http", "server", "error", "2"]
        );
        assert_eq!(
            subwords("followRedirect_v2"),
            ["follow", "redirect", "v", "2"]
        );
        assert_eq!(subwords("ABc"), ["a", "bc"]);
        assert_eq!(subwords("XML"), ["xml"]);
        assert_eq!(subwords("a-B c"), ["a", "b", "c"]);
        assert_eq!(subwords("Ab"), ["ab"]);
        assert_eq!(subwords("é"), Vec::<String>::new());
        assert_eq!(subwords(""), Vec::<String>::new());
    }

    #[test]
    fn escape_spells_like_python_re_escape() {
        assert_eq!(escape("a b-c.d"), "a\\ b\\-c\\.d");
        assert_eq!(
            escape("fn(x) -> [y]#&~"),
            "fn\\(x\\)\\ \\->\\ \\[y\\]\\#\\&\\~"
        );
        assert_eq!(escape("é_9Z"), "é_9Z");
        assert_eq!(escape("\\^$|?*+{}"), "\\\\\\^\\$\\|\\?\\*\\+\\{\\}");
        assert_eq!(escape("a\tb"), "a\\\tb");
        let held = regex::Regex::new(&format!("^{}$", escape("### a b-(c)+ [d]"))).unwrap();
        assert!(held.is_match("### a b-(c)+ [d]"));
    }

    #[test]
    fn splitlines_cuts_where_python_does_and_keeps_no_empty_last_line() {
        assert_eq!(splitlines("a\nb\n"), ["a", "b"]);
        assert_eq!(splitlines("a\r\nb\rc"), ["a", "b", "c"]);
        assert_eq!(
            splitlines("a\x0cb\u{2028}c\x1dd\u{85}e"),
            ["a", "b", "c", "d", "e"]
        );
        assert_eq!(splitlines("a\n\nb"), ["a", "", "b"]);
        assert_eq!(splitlines("\r\n"), [""]);
        assert_eq!(splitlines(""), Vec::<&str>::new());
        assert_eq!(splitlines("é\r\nü"), ["é", "ü"]);
    }

    #[test]
    fn universal_newlines_turns_every_line_end_into_a_newline() {
        assert_eq!(universal_newlines("a\r\nb\rc\n"), "a\nb\nc\n");
    }

    #[test]
    fn head_cuts_on_characters_and_case_tests_need_a_cased_letter() {
        assert_eq!(head("héllo", 2), "hé");
        assert_eq!(head("ab", 5), "ab");
        assert_eq!(head("ab", 0), "");
        assert!(is_upper("A1") && !is_upper("1") && !is_upper("Ab"));
        assert!(is_lower("a1") && !is_lower("1") && !is_lower("aB"));
    }

    #[test]
    fn the_drop_pattern_escapes_the_directory_like_python() {
        assert_eq!(escape("a/b c.d-e_f"), "a/b\\ c\\.d\\-e_f");
        assert_eq!(
            escape("()[]{}?*+-|^$\\.&~# \t"),
            "\\(\\)\\[\\]\\{\\}\\?\\*\\+\\-\\|\\^\\$\\\\\\.\\&\\~\\#\\ \\\t"
        );
        assert_eq!(escape("é\u{b}\u{c}\n\r"), "é\\\u{b}\\\u{c}\\\n\\\r");
        assert_eq!(escape("a!\"%',:;<=>@`/_"), "a!\"%',:;<=>@`/_");
    }
}
