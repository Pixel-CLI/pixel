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
}
