// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The text handling of a probe script's `open(...)`, `.splitlines()` and
//! `.split()`, for the steps ported from one: Rust's `lines()` and
//! `split_whitespace()` cut at fewer characters, so a form feed or a lone
//! `\r` in a file would change what a step quotes.

/// `text` as Python's text-mode `open()` returns it: `\r\n` and a lone `\r`
/// are `\n`.
pub(super) fn universal_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// `str.splitlines()`: the line ends are `\n`, `\r`, `\r\n`, `\v`, `\f`,
/// `\x1c`-`\x1e`, `\x85`, ` ` and ` `; no empty last line.
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

/// `word` lowercased with a trailing `ing`, `es` or `s` cut (the first that
/// fits) when it is longer than four letters: the stem of `doc-sections` and
/// `name-files`.
pub(super) fn stem(word: &str) -> String {
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

/// Python's `re.escape`: a backslash before each of `()[]{}?*+-|^$\.&~#`,
/// a space and the ASCII whitespace controls.
pub(super) fn re_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if "()[]{}?*+-|^$\\.&~# \t\n\r\x0b\x0c".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_drop_pattern_escapes_the_directory_like_python() {
        assert_eq!(re_escape("a/b c.d-e_f"), "a/b\\ c\\.d\\-e_f");
        assert_eq!(
            re_escape("()[]{}?*+-|^$\\.&~# \t"),
            "\\(\\)\\[\\]\\{\\}\\?\\*\\+\\-\\|\\^\\$\\\\\\.\\&\\~\\#\\ \\\t"
        );
        assert_eq!(re_escape("é\u{b}\u{c}\n\r"), "é\\\u{b}\\\u{c}\\\n\\\r");
        assert_eq!(re_escape("a!\"%',:;<=>@`/_"), "a!\"%',:;<=>@`/_");
    }
}
