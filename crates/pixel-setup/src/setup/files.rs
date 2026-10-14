// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The managed block older `pixel setup` releases wrote, and the parser that
//! strips it back out. Setup no longer writes instruction files — the `pixel`
//! skill replaced the block — but files a previous release touched still carry
//! it, so install and uninstall parse it away here.
//!
//! Ported from GitButler's `crates/but/src/command/agent/files.rs` (read at
//! `5cbe33d`), because every case it handles is a way a managed block goes
//! wrong in a file the user also writes by hand:
//!
//! - **line-anchored markers** — a marker quoted in prose, or shown as an
//!   example inside a fenced code block, is documentation, not a delimiter;
//!   splicing on it would delete the surrounding text;
//! - **refusal on a partial block** — a start marker with no end means the user
//!   (or an editor) truncated the file; replacing it would destroy whatever
//!   was meant to follow;
//! - **convergence** — several blocks from an earlier buggy run collapse to
//!   one, so the file stops growing on every re-run;
//! - **CRLF** — a replaced block follows the file's existing endings instead of
//!   introducing mixed ones.

use crate::{InstallError, Result};

/// Opens a block `pixel setup` owns. Everything between this and
/// [`BLOCK_END`] is rewritten on every run; the text around it is the user's.
pub const BLOCK_START: &str = "<!-- pixel:setup:start -->";
/// Closes a block `pixel setup` owns. See [`BLOCK_START`].
pub const BLOCK_END: &str = "<!-- pixel:setup:end -->";

/// A block that cannot be read as a whole is left alone, with the fix in the
/// message: pixel removes what it wrote, never what it cannot parse.
const PARTIAL_BLOCK: &str = "found one pixel:setup marker with no matching other one; \
                             close or delete that block, then re-run";
#[cfg(test)]
const REVERSED_BLOCK: &str = "found the pixel:setup end marker before its start marker; \
                              close or delete that block, then re-run";

/// Byte offset of the next `needle` in `haystack` at or after `from` that sits
/// on a line of its own: at the start of the file or right after a newline, and
/// followed by a newline or the end of the file.
///
/// The scan is `match_indices`, not a hand-advanced cursor: a wrong bound on a
/// manual index is an infinite loop, and a scan that cannot spin is worth more
/// than the few bytes it saves.
pub fn find_line_anchored(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let bytes = haystack.as_bytes();
    haystack
        .match_indices(needle)
        .map(|(idx, _)| idx)
        .find(|&idx| {
            if idx < from || inside_fenced_block(haystack, idx) {
                return false;
            }
            let at_line_start = idx == 0 || bytes[idx - 1] == b'\n';
            let after = idx + needle.len();
            let at_line_end = after == haystack.len() || matches!(bytes[after], b'\n' | b'\r');
            at_line_start && at_line_end
        })
}

/// Whether the line starting at `idx` falls inside a fenced code block. An odd
/// number of fence delimiters before it means a fence is open, so a marker
/// documented inside one is left alone.
fn inside_fenced_block(haystack: &str, idx: usize) -> bool {
    let mut open = false;
    for line in haystack[..idx].lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            open = !open;
        }
    }
    open
}

/// The byte span of every well-formed block, from the first byte of its start
/// marker to just past its end marker.
pub fn block_spans(existing: &str) -> Result<Vec<std::ops::Range<usize>>> {
    let mut spans = Vec::new();
    let mut pos = 0;
    while let Some(start) = find_line_anchored(existing, BLOCK_START, pos) {
        let Some(end) = find_line_anchored(existing, BLOCK_END, start + BLOCK_START.len()) else {
            return Err(InstallError::Setup(PARTIAL_BLOCK.to_string()));
        };
        let span_end = end + BLOCK_END.len();
        spans.push(start..span_end);
        pos = span_end;
    }
    Ok(spans)
}

/// Render `block` with the file's line endings when the file already uses
/// CRLF, so a replaced block does not introduce mixed endings.
#[cfg(test)]
fn match_line_endings(existing: &str, block: &str) -> String {
    if existing.contains("\r\n") {
        block.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        block.to_string()
    }
}

/// Insert or replace the managed block in `existing`.
///
/// The surrounding text is preserved byte for byte. A block that cannot be
/// read as a whole is an error, not something to guess at.
#[cfg(test)]
pub fn upsert(existing: &str, block: &str) -> Result<String> {
    let start = find_line_anchored(existing, BLOCK_START, 0);
    let end = find_line_anchored(existing, BLOCK_END, 0);

    match (start, end) {
        (None, None) => return Ok(append_block(existing, block)),
        (Some(_), None) | (None, Some(_)) => {
            return Err(InstallError::Setup(PARTIAL_BLOCK.to_string()));
        }
        (Some(_), Some(_)) if end < start => {
            return Err(InstallError::Setup(REVERSED_BLOCK.to_string()));
        }
        (Some(_), Some(_)) => {}
    }

    let block = match_line_endings(existing, block);
    let mut updated = String::with_capacity(existing.len() + block.len());
    let mut copied = 0;
    for (index, span) in block_spans(existing)?.into_iter().enumerate() {
        let mut span_end = span.end;
        if existing[span_end..].starts_with("\r\n") {
            span_end += 2;
        } else if existing[span_end..].starts_with('\n') {
            span_end += 1;
        }
        updated.push_str(&existing[copied..span.start]);
        if index == 0 {
            updated.push_str(&block);
        }
        copied = span_end;
    }
    updated.push_str(&existing[copied..]);
    Ok(updated)
}

/// Append the block after `existing`, separated by exactly one blank line.
#[cfg(test)]
fn append_block(existing: &str, block: &str) -> String {
    if existing.is_empty() {
        return block.to_string();
    }
    let block = match_line_endings(existing, block);
    let crlf = existing.contains("\r\n");
    let mut updated = String::with_capacity(existing.len() + block.len() + 2);
    updated.push_str(existing);
    if existing.ends_with("\r\n\r\n") || existing.ends_with("\n\n") {
        // Already separated by a blank line.
    } else if crlf {
        updated.push_str(if existing.ends_with("\r\n") {
            "\r\n"
        } else {
            "\r\n\r\n"
        });
    } else if existing.ends_with('\n') {
        updated.push('\n');
    } else {
        updated.push_str("\n\n");
    }
    updated.push_str(&block);
    updated
}

/// Remove every pixel:setup block from `text`, keeping the user's own lines.
/// A block that cannot be read as a whole is left alone: uninstall removes what
/// pixel wrote and never what it cannot parse.
///
/// The blank line that separated a block goes with it, on both sides: leaving
/// one behind turns a removed block into a growing run of empty lines.
pub fn strip(text: &str) -> String {
    let Ok(spans) = block_spans(text) else {
        return text.to_string();
    };
    if spans.is_empty() {
        return text.to_string();
    }
    let crlf = text.contains("\r\n");
    let newline: &str = if crlf { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for span in spans {
        out.push_str(&text[copied..span.start]);
        if out.ends_with(&format!("{newline}{newline}")) {
            let keep = out.len() - newline.len();
            out.truncate(keep);
        }
        copied = span.end;
        if text[copied..].starts_with(newline) {
            copied += newline.len();
        }
    }
    out.push_str(&text[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "<!-- pixel:setup:start -->\nbody\n<!-- pixel:setup:end -->\n";

    fn managed(body: &str) -> String {
        format!("# Rules\n\n{BLOCK_START}\n{body}\n{BLOCK_END}\n")
    }

    fn upserted(body: &str) -> String {
        upsert("", &managed(body)).unwrap()
    }

    #[test]
    fn an_empty_file_gets_exactly_the_block_and_nothing_around_it() {
        assert_eq!(upsert("", BLOCK).unwrap(), BLOCK);
        assert_eq!(upserted("one"), managed("one"));
    }

    #[test]
    fn existing_text_is_preserved_byte_for_byte() {
        let original = "# My rules\n\nKeep this paragraph.\n";
        let updated = upsert(original, BLOCK).unwrap();
        assert!(
            updated.starts_with("# My rules\n\nKeep this paragraph.\n"),
            "got {updated:?}"
        );
        assert_eq!(updated.matches(BLOCK_START).count(), 1);
    }

    #[test]
    fn a_second_run_is_byte_identical() {
        let once = upsert("before\n", BLOCK).unwrap();
        let twice = upsert(&once, BLOCK).unwrap();
        assert_eq!(once, twice, "the block must not grow on every re-run");
    }

    #[test]
    fn a_replaced_block_takes_the_new_body_and_keeps_the_surroundings() {
        let first = upsert("before\n", BLOCK).unwrap();
        let replaced = upsert(&first, &managed("second")).unwrap();
        assert!(!replaced.contains("body"), "got {replaced:?}");
        assert!(replaced.contains("second"));
        assert!(replaced.starts_with("before\n"));
    }

    #[test]
    fn several_blocks_converge_to_one() {
        let doubled = format!("{BLOCK}{BLOCK}");
        let updated = upsert(&doubled, BLOCK).unwrap();
        assert_eq!(updated.matches(BLOCK_START).count(), 1);
        assert_eq!(updated.matches(BLOCK_END).count(), 1);
    }

    #[test]
    fn a_partial_block_is_refused_rather_than_guessed() {
        for original in [
            format!("{BLOCK_START}\nbody\n"),
            format!("body\n{BLOCK_END}\n"),
            format!("{BLOCK_END}\nbody\n{BLOCK_START}\n"),
        ] {
            assert!(
                upsert(&original, BLOCK).is_err(),
                "{original:?} must not be edited"
            );
        }
    }

    #[test]
    fn a_marker_quoted_in_prose_is_not_a_delimiter() {
        let original =
            format!("The setup writes {BLOCK_START} and {BLOCK_END} around its block.\n");
        let updated = upsert(&original, BLOCK).unwrap();
        assert!(
            updated.starts_with("The setup writes "),
            "the quoted marker must not splice the sentence away: {updated:?}"
        );
        assert_eq!(
            updated.matches(BLOCK_START).count(),
            2,
            "one quote, one block"
        );
    }

    #[test]
    fn a_marker_documented_in_a_fence_is_not_a_delimiter() {
        let original = format!("Example:\n\n```markdown\n{BLOCK_START}\nbody\n{BLOCK_END}\n```\n");
        let updated = upsert(&original, BLOCK).unwrap();
        assert!(
            updated.starts_with(&original),
            "the documented example survives untouched: {updated:?}"
        );
        assert_eq!(
            updated.matches(BLOCK_START).count(),
            2,
            "the quoted example and the real block: {updated:?}"
        );
        assert!(updated.ends_with(BLOCK), "the real block is appended");
    }

    #[test]
    fn a_marker_after_a_closed_fence_is_a_delimiter_again() {
        let original = format!("```\ncode\n```\n{BLOCK_START}\nold\n{BLOCK_END}\n");
        let updated = upsert(&original, &managed("new")).unwrap();
        assert_eq!(
            updated,
            format!("```\ncode\n```\n{}", managed("new")),
            "a closed fence must free the delimiter it documented"
        );
    }

    #[test]
    fn a_crlf_file_keeps_its_endings() {
        let original = "# Rules\r\n".to_string();
        let appended = upsert(&original, BLOCK).unwrap();
        assert!(
            appended.contains("\r\n") && !appended.replace("\r\n", "").contains('\n'),
            "got {appended:?}"
        );
        let replaced = upsert(&appended, BLOCK).unwrap();
        assert_eq!(
            replaced.replace("\r\n", "").matches('\n').count(),
            0,
            "a re-run must not introduce LF into a CRLF file"
        );
    }

    #[test]
    fn strip_removes_the_block_and_the_blank_line_it_owned() {
        let original = format!("keep\n\n{BLOCK_START}\nbody\n{BLOCK_END}\n\nkeep too\n");
        assert_eq!(strip(&original), "keep\n\nkeep too\n");
    }

    #[test]
    fn strip_leaves_a_file_it_cannot_parse() {
        let broken = format!("keep\n{BLOCK_START}\nbody\n");
        assert_eq!(strip(&broken), broken);
    }

    #[test]
    fn strip_leaves_a_file_with_no_block_untouched() {
        assert_eq!(strip("nothing here\n"), "nothing here\n");
    }
}
