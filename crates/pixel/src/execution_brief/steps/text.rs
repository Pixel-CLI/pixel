// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The small string rules the ported probe scripts share, spelled the way
//! Python spells them, so a step prints the script's bytes.

use super::Ctx;

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
    ctx.read(path, limit).map_or_else(Vec::new, |text| {
        text.lines().map(ToString::to_string).collect()
    })
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
