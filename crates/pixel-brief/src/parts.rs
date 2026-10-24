// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The prompt brief split over several hooks (`specs/brief.md`, "Deliver").
//!
//! Claude Code caps what one `UserPromptSubmit` hook injects as additional
//! context at 10,000 characters (10,001 arrives as a preview of about 2,000),
//! and Codex spills a hook's context past 2,500 estimated tokens (four bytes
//! each) to a file behind a head and tail preview. Both cap *per hook* and run
//! the hooks of an event concurrently, so a brief larger than one hook may
//! inject is delivered by several: `pixel hook prompt-submit --part K/N`. A slice
//! is cut at line boundaries, is at most [`PART_CHARS`] characters (and, for a
//! host that counts bytes, as many bytes), and the slices in order concatenate
//! to the brief.

use std::str::FromStr;

/// Parts a brief may be split into at most.
pub const PARTS_MAX: u8 = 8;
/// Parts a hook delivers when the settings do not say.
pub const PARTS_DEFAULT: u8 = 3;

/// Characters one part carries at most: under the 10,000 a Claude Code hook
/// may inject, with room for the host's own framing.
pub const PART_CHARS: usize = 9_800;
/// Bytes one part carries at most where the host measures bytes (Codex: four
/// bytes per estimated token, 2,500 tokens): the same figure as [`PART_CHARS`],
/// since 9,800 bytes stay under 10,000.
pub const PART_BYTES: usize = 9_800;

/// One slice of a split brief: `index` of `count`, both from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BriefPart {
    pub index: u8,
    pub count: u8,
}

impl FromStr for BriefPart {
    type Err = String;

    /// `K/N` with `1 <= K <= N <= BRIEF_PARTS_MAX`.
    fn from_str(text: &str) -> Result<Self, String> {
        let bad = || format!("expected K/N with 1 <= K <= N <= {PARTS_MAX}, got `{text}`");
        let (index, count) = text.split_once('/').ok_or_else(bad)?;
        let index: u8 = index.parse().map_err(|_| bad())?;
        let count: u8 = count.parse().map_err(|_| bad())?;
        ((1..=PARTS_MAX).contains(&count) && (1..=count).contains(&index))
            .then_some(Self { index, count })
            .ok_or_else(bad)
    }
}

/// What one part may carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub chars: usize,
    pub bytes: usize,
}

impl Limits {
    /// Characters only: a host that counts characters.
    pub const CHARS: Self = Self {
        chars: PART_CHARS,
        bytes: usize::MAX,
    };
    /// Characters and bytes: a host that estimates tokens from bytes.
    pub const BYTES: Self = Self {
        chars: PART_CHARS,
        bytes: PART_BYTES,
    };
}

/// `text` cut into pieces of at most `limits`, at line boundaries: a piece
/// takes whole lines while they fit, and a line larger than a piece is cut at
/// a character boundary. The pieces concatenate to `text`, and the same text
/// always gives the same pieces.
pub fn split(text: &str, limits: Limits) -> Vec<&str> {
    let (max_chars, max_bytes) = (limits.chars.max(1), limits.bytes.max(4));
    let mut pieces = Vec::new();
    // The open piece starts at `start`; its last, unfinished line at `line`.
    let (mut start, mut line) = (0, 0);
    let (mut chars, mut bytes) = (0, 0);
    let (mut line_chars, mut line_bytes) = (0, 0);
    for (offset, c) in text.char_indices() {
        // At most two cuts per character: one back to the last whole line,
        // one inside a line that alone fills a piece.
        for _ in 0..2 {
            if chars < max_chars && bytes + c.len_utf8() <= max_bytes {
                break;
            }
            if line > start {
                // Whole lines precede the unfinished one: they are the piece.
                pieces.push(&text[start..line]);
                start = line;
                chars = line_chars;
                bytes = line_bytes;
            } else {
                // One line fills the piece: cut inside it.
                pieces.push(&text[start..offset]);
                (start, line) = (offset, offset);
                (chars, bytes) = (0, 0);
                (line_chars, line_bytes) = (0, 0);
            }
        }
        chars += 1;
        bytes += c.len_utf8();
        line_chars += 1;
        line_bytes += c.len_utf8();
        if c == '\n' {
            line = offset + 1;
            (line_chars, line_bytes) = (0, 0);
        }
    }
    if start < text.len() {
        pieces.push(&text[start..]);
    }
    pieces
}

/// The slice `part` serves of `text`: the empty string when the brief has
/// fewer pieces than parts. Pieces past the last part are not served.
pub fn slice(text: &str, part: BriefPart, limits: Limits) -> &str {
    split(text, limits)
        .get(usize::from(part.index) - 1)
        .copied()
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(chars: usize, bytes: usize) -> Limits {
        Limits { chars, bytes }
    }

    /// Lines of `width` characters (newline included), `count` of them.
    fn lines(width: usize, count: usize) -> String {
        let line = format!("{}\n", "x".repeat(width - 1));
        line.repeat(count)
    }

    fn part(index: u8, count: u8) -> BriefPart {
        BriefPart { index, count }
    }

    fn lengths(pieces: &[&str]) -> Vec<usize> {
        pieces.iter().map(|piece| piece.chars().count()).collect()
    }

    #[test]
    fn a_piece_holds_exactly_the_limit_and_the_next_character_starts_another() {
        assert_eq!(PART_CHARS, 9_800);
        assert_eq!(split("", Limits::CHARS), Vec::<&str>::new());
        let exact = "a".repeat(9_800);
        assert_eq!(split(&exact, Limits::CHARS), vec![exact.as_str()]);
        let over = "a".repeat(9_801);
        assert_eq!(lengths(&split(&over, Limits::CHARS)), vec![9_800, 1]);
        let under = "a".repeat(9_799);
        assert_eq!(lengths(&split(&under, Limits::CHARS)), vec![9_799]);
        let twice = "a".repeat(19_600);
        assert_eq!(lengths(&split(&twice, Limits::CHARS)), vec![9_800, 9_800]);
        let thrice = "a".repeat(19_601);
        assert_eq!(
            lengths(&split(&thrice, Limits::CHARS)),
            vec![9_800, 9_800, 1]
        );
    }

    #[test]
    fn whole_lines_fill_a_piece_and_the_line_that_does_not_fit_opens_the_next() {
        // 98 lines of 100 characters are exactly one piece.
        let full = lines(100, 98);
        assert_eq!(full.chars().count(), 9_800);
        assert_eq!(split(&full, Limits::CHARS), vec![full.as_str()]);
        // The 99th line opens the second piece whole.
        let more = lines(100, 99);
        let pieces = split(&more, Limits::CHARS);
        assert_eq!(lengths(&pieces), vec![9_800, 100]);
        assert_eq!(pieces[1], lines(100, 1));
        // A line that would cross the boundary moves to the next piece: 9,750
        // characters of lines, then one of 100, then a short one.
        let text = format!("{}{}{}", lines(150, 65), lines(100, 1), "tail\n");
        assert_eq!(text.chars().count(), 9_750 + 100 + 5);
        let pieces = split(&text, Limits::CHARS);
        assert_eq!(lengths(&pieces), vec![9_750, 105]);
        assert!(pieces[1].starts_with(&lines(100, 1)));
        assert!(pieces.iter().all(|piece| piece.ends_with('\n')));
    }

    #[test]
    fn a_line_longer_than_a_piece_is_cut_inside_it_and_the_rest_continues() {
        let text = format!("head\n{}\ntail\n", "y".repeat(25_000));
        let pieces = split(&text, Limits::CHARS);
        assert_eq!(pieces.concat(), text);
        assert_eq!(
            pieces[0], "head\n",
            "the lines before it stay a piece of their own"
        );
        assert!(pieces.iter().all(|piece| piece.chars().count() <= 9_800));
        assert_eq!(lengths(&pieces), vec![5, 9_800, 9_800, 5_406]);
        assert_eq!(pieces[3], format!("{}\ntail\n", "y".repeat(5_400)));
    }

    #[test]
    fn the_character_limit_counts_characters_not_bytes() {
        // 9,800 two-byte characters are 19,600 bytes and still one piece for a
        // host that counts characters; one more character starts another.
        let text = "é".repeat(9_800);
        assert_eq!(split(&text, Limits::CHARS).len(), 1);
        assert_eq!(
            lengths(&split(&"é".repeat(9_801), Limits::CHARS)),
            vec![9_800, 1]
        );
        let emoji = "🦀".repeat(9_801);
        let pieces = split(&emoji, Limits::CHARS);
        assert_eq!(lengths(&pieces), vec![9_800, 1]);
        assert_eq!(pieces.concat(), emoji);
    }

    #[test]
    fn the_byte_limit_cuts_on_character_boundaries_for_a_host_that_counts_bytes() {
        // Two-byte characters: 4,900 fit 9,800 bytes.
        let text = "é".repeat(9_800);
        let pieces = split(&text, Limits::BYTES);
        assert_eq!(lengths(&pieces), vec![4_900, 4_900]);
        assert!(pieces.iter().all(|piece| piece.len() <= 9_800));
        // Four-byte characters: 2,450 fit.
        let crabs = "🦀".repeat(2_451);
        let pieces = split(&crabs, Limits::BYTES);
        assert_eq!(lengths(&pieces), vec![2_450, 1]);
        // ASCII is the same under both.
        let ascii = "a".repeat(9_801);
        assert_eq!(split(&ascii, Limits::BYTES), split(&ascii, Limits::CHARS));
        // A multibyte character that would cross the byte boundary moves whole.
        let edge = format!("{}é", "a".repeat(9_799));
        let pieces = split(&edge, Limits::BYTES);
        assert_eq!(pieces, vec!["a".repeat(9_799).as_str(), "é"]);
    }

    #[test]
    fn a_limit_smaller_than_one_character_still_makes_progress() {
        let pieces = split("🦀🦀", limits(0, 0));
        assert_eq!(pieces, vec!["🦀", "🦀"]);
        assert_eq!(
            split("ab\ncd", limits(1, 1)),
            vec!["a", "b", "\n", "c", "d"]
        );
    }

    #[test]
    fn pieces_always_concatenate_to_the_text_and_never_exceed_the_limits() {
        // A deterministic mix of line lengths, multibyte characters and blank lines.
        let mut seed = 0x2545_f491_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut text = String::new();
        for _ in 0..400 {
            let width = usize::try_from(next() % 900).unwrap();
            for _ in 0..width {
                text.push(match next() % 7 {
                    0 => 'é',
                    1 => '字',
                    2 => '🦀',
                    _ => 'a',
                });
            }
            text.push('\n');
        }
        for limit in [
            Limits::CHARS,
            Limits::BYTES,
            limits(300, usize::MAX),
            limits(64, 100),
        ] {
            let pieces = split(&text, limit);
            assert_eq!(pieces.concat(), text, "{limit:?}");
            assert!(
                pieces
                    .iter()
                    .all(|piece| piece.chars().count() <= limit.chars && piece.len() <= limit.bytes),
                "{limit:?}"
            );
            assert!(pieces.iter().all(|piece| !piece.is_empty()), "{limit:?}");
            assert_eq!(pieces, split(&text, limit), "deterministic");
        }
    }

    #[test]
    fn a_slice_is_the_indexed_piece_and_empty_past_the_end() {
        let text = lines(100, 99);
        assert_eq!(slice(&text, part(1, 3), Limits::CHARS), lines(100, 98));
        assert_eq!(slice(&text, part(2, 3), Limits::CHARS), lines(100, 1));
        assert_eq!(slice(&text, part(3, 3), Limits::CHARS), "");
        assert_eq!(slice("", part(1, 3), Limits::CHARS), "");
        // Parts together are the brief.
        let big = lines(100, 250);
        let joined: String = (1..=3)
            .map(|k| slice(&big, part(k, 3), Limits::CHARS))
            .collect();
        assert_eq!(joined, big, "250 lines are exactly three pieces");
        // A brief longer than the parts is cut after the last one, whole lines.
        let huge = lines(100, 400);
        let joined: String = (1..=3)
            .map(|k| slice(&huge, part(k, 3), Limits::CHARS))
            .collect();
        assert_eq!(joined, lines(100, 98 * 3));
    }

    #[test]
    fn a_part_is_k_over_n_inside_the_bounds() {
        assert_eq!("1/3".parse::<BriefPart>(), Ok(part(1, 3)));
        assert_eq!("3/3".parse::<BriefPart>(), Ok(part(3, 3)));
        assert_eq!("1/1".parse::<BriefPart>(), Ok(part(1, 1)));
        let most = PARTS_MAX;
        assert_eq!(
            format!("{most}/{most}").parse::<BriefPart>(),
            Ok(part(most, most))
        );
        for bad in [
            "0/3", "4/3", "1/9", "2", "/3", "1/", "a/b", "1/3/3", "", "-1/3", "1/0", " 1/3",
        ] {
            let error = bad.parse::<BriefPart>().unwrap_err();
            assert!(error.contains("K/N"), "{bad}: {error}");
        }
    }
}
