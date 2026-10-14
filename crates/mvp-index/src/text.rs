// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Helpers every rule file uses to turn source text into row fields.

/// Signatures are cut at this many characters (v1's `SIG_CAP`;
/// `design/map-format.md` caps each rendered field at 200, guessed).
pub const SIGNATURE_CAP: usize = 200;

/// The first line of `text`, trimmed and cut at [`SIGNATURE_CAP`]
/// characters: what a symbol's signature holds. It is always a prefix of
/// the trimmed source line it comes from, so C-10's check can find it.
pub fn signature(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    match first.char_indices().nth(SIGNATURE_CAP) {
        Some((cut, _)) => first[..cut].trim_end().to_owned(),
        None => first.to_owned(),
    }
}

/// The number of lines of a file as the indexer counts them: 1 for an
/// empty file, plus one per `\n`.
pub fn line_count(content: &[u8]) -> u32 {
    1 + content.iter().filter(|b| **b == b'\n').count() as u32
}

/// The text of a 1-based line, without its line break; `None` past the end.
pub fn line(content: &str, line: u32) -> Option<&str> {
    content
        .split('\n')
        .nth(line.checked_sub(1)? as usize)
        .map(|text| text.strip_suffix('\r').unwrap_or(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_is_the_trimmed_first_line_cut_on_a_character() {
        assert_eq!(
            signature("  def call(a, b)\n    run\n  end"),
            "def call(a, b)"
        );
        let long = format!("fn f({})", "é".repeat(300));
        let cut = signature(&long);
        assert_eq!(cut.chars().count(), SIGNATURE_CAP);
        assert!(long.starts_with(&cut));
    }

    #[test]
    fn lines_are_counted_and_read_one_based() {
        assert_eq!(line_count(b""), 1);
        assert_eq!(line_count(b"a\nb\n"), 3);
        assert_eq!(line("a\r\nb", 1), Some("a"));
        assert_eq!(line("a\nb", 2), Some("b"));
        assert_eq!(line("a\nb", 3), None);
        assert_eq!(line("a", 0), None);
    }
}
