// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Comment and string masking, and the braced items of Rust and JavaScript
//! sources, the way the script finds them: offsets are kept (a masked
//! character becomes spaces of its own byte width), so a match in the masked
//! text indexes the original.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::{Captures, Regex};

use super::evidence::SECRETISH;
use super::super::text::{splitlines, head};

/// Lines a quoted source range may span.
pub(super) const SCOPED_BLOCK_MAX_LINES: usize = 72;
/// Characters of the narrow lanes' output.
pub(super) const SCOPED_EVIDENCE_MAX: usize = 3600;

/// `text` with the characters at `masked` replaced by spaces of their width.
fn apply(text: &[char], masked: &[bool]) -> String {
    let mut out = String::with_capacity(text.len());
    for (ch, hide) in text.iter().zip(masked) {
        if *hide {
            out.extend(std::iter::repeat_n(' ', ch.len_utf8()));
        } else {
            out.push(*ch);
        }
    }
    out
}

fn starts(text: &[char], at: usize, needle: &str) -> bool {
    needle
        .chars()
        .enumerate()
        .all(|(offset, ch)| text.get(at + offset) == Some(&ch))
}

#[derive(Clone, Copy, PartialEq)]
enum Rust {
    Code,
    Line,
    Block,
    Raw,
    Str,
    Char,
}

/// `source` with Rust comments and quoted literals blanked.
pub(super) fn rust_code_mask(source: &str) -> String {
    let text: Vec<char> = source.chars().collect();
    let mut hide = vec![false; text.len()];
    let (mut at, mut depth, mut state) = (0, 0usize, Rust::Code);
    let mut raw_end: Vec<char> = Vec::new();
    while at < text.len() {
        let ch = text[at];
        match state {
            Rust::Code => {
                if starts(&text, at, "//") {
                    state = Rust::Line;
                    hide[at] = true;
                    hide[at + 1] = true;
                    at += 2;
                    continue;
                }
                if starts(&text, at, "/*") {
                    (state, depth) = (Rust::Block, 1);
                    hide[at] = true;
                    hide[at + 1] = true;
                    at += 2;
                    continue;
                }
                // Raw strings may hold braces in examples or diagnostics.
                if ch == 'r' {
                    let hashes = text[at + 1..].iter().take(8).take_while(|c| **c == '#').count();
                    if text.get(at + 1 + hashes) == Some(&'"') {
                        state = Rust::Raw;
                        raw_end = std::iter::once('"').chain(std::iter::repeat_n('#', hashes)).collect();
                        let width = 2 + hashes;
                        for flag in &mut hide[at..at + width] {
                            *flag = true;
                        }
                        at += width;
                        continue;
                    }
                }
                if ch == '"' {
                    state = Rust::Str;
                    hide[at] = true;
                    at += 1;
                    continue;
                }
                if ch == '\''
                    && at + 2 < text.len()
                    && (text[at + 2] == '\'' || text[at + 1] == '\\')
                {
                    state = Rust::Char;
                    hide[at] = true;
                    at += 1;
                    continue;
                }
            }
            Rust::Line => {
                if ch == '\n' {
                    state = Rust::Code;
                } else {
                    hide[at] = true;
                }
            }
            Rust::Block => {
                if starts(&text, at, "/*") {
                    depth += 1;
                    hide[at] = true;
                    hide[at + 1] = true;
                    at += 2;
                    continue;
                }
                if starts(&text, at, "*/") {
                    depth -= 1;
                    hide[at] = true;
                    hide[at + 1] = true;
                    at += 2;
                    if depth == 0 {
                        state = Rust::Code;
                    }
                    continue;
                }
                if ch != '\n' {
                    hide[at] = true;
                }
            }
            Rust::Raw => {
                let ending: String = raw_end.iter().collect();
                if starts(&text, at, &ending) {
                    for flag in &mut hide[at..at + raw_end.len()] {
                        *flag = true;
                    }
                    at += raw_end.len();
                    state = Rust::Code;
                    continue;
                }
                if ch != '\n' {
                    hide[at] = true;
                }
            }
            Rust::Str | Rust::Char => {
                let closing = if state == Rust::Str { '"' } else { '\'' };
                if ch == '\\' {
                    hide[at] = true;
                    if text.get(at + 1).is_some_and(|next| *next != '\n') {
                        hide[at + 1] = true;
                    }
                    at += 2;
                    continue;
                }
                if ch == closing {
                    state = Rust::Code;
                }
                if ch != '\n' {
                    hide[at] = true;
                }
            }
        }
        at += 1;
    }
    apply(&text, &hide)
}

#[derive(Clone, Copy, PartialEq)]
enum Js {
    Code,
    Line,
    Block,
    Quote(char),
}

/// `source` with JavaScript comments and every quoted string blanked
/// (newlines kept, a string running over lines until its quote).
pub(super) fn js_code_mask(source: &str) -> String {
    let text: Vec<char> = source.chars().collect();
    let mut hide = vec![false; text.len()];
    let (mut at, mut state, mut escaped) = (0, Js::Code, false);
    while at < text.len() {
        let ch = text[at];
        match state {
            Js::Code => {
                if starts(&text, at, "//") || starts(&text, at, "/*") {
                    state = if text[at + 1] == '/' { Js::Line } else { Js::Block };
                    hide[at] = true;
                    hide[at + 1] = true;
                    at += 2;
                    continue;
                }
                if matches!(ch, '\'' | '"' | '`') {
                    hide[at] = true;
                    state = Js::Quote(ch);
                    escaped = false;
                }
            }
            Js::Line => {
                if ch == '\n' {
                    state = Js::Code;
                } else {
                    hide[at] = true;
                }
            }
            Js::Block => {
                if starts(&text, at, "*/") {
                    hide[at] = true;
                    hide[at + 1] = true;
                    at += 2;
                    state = Js::Code;
                    continue;
                }
                if ch != '\n' {
                    hide[at] = true;
                }
            }
            Js::Quote(quote) => {
                if ch == '\n' {
                    at += 1;
                    continue;
                }
                if escaped {
                    hide[at] = true;
                    escaped = false;
                    at += 1;
                    continue;
                }
                if ch == '\\' {
                    hide[at] = true;
                    escaped = true;
                    at += 1;
                    continue;
                }
                if ch == quote {
                    state = Js::Code;
                }
                hide[at] = true;
            }
        }
        at += 1;
    }
    apply(&text, &hide)
}

/// Offset of every `{` that a later `}` closes, by the opening brace.
fn matching_braces(masked: &str) -> HashMap<usize, usize> {
    let (mut stack, mut matching) = (Vec::new(), HashMap::new());
    for (at, byte) in masked.bytes().enumerate() {
        match byte {
            b'{' => stack.push(at),
            b'}' => {
                if let Some(open) = stack.pop() {
                    matching.insert(open, at);
                }
            }
            _ => {}
        }
    }
    matching
}

/// `(start, end, body start)` of each braced item `pattern` (multi-line) opens
/// in the code of a Rust-like source: the item runs from the match to the
/// brace that closes the first `{` at or after the match's end.
pub(super) fn rust_scopes(source: &str, pattern: &Regex) -> Vec<(usize, usize, usize)> {
    scopes_of(&rust_code_mask(source), pattern)
}

fn scopes_of(masked: &str, pattern: &Regex) -> Vec<(usize, usize, usize)> {
    let matching = matching_braces(masked);
    let mut out = Vec::new();
    for found in pattern.find_iter(masked) {
        let end = found.end();
        let opening = if end > 0 && masked.as_bytes()[end - 1] == b'{' {
            Some(end - 1)
        } else {
            masked[end..].find('{').map(|at| end + at)
        };
        if let Some(closing) = opening.and_then(|open| matching.get(&open)) {
            out.push((found.start(), closing + 1, opening.unwrap_or(0) + 1));
        }
    }
    out
}

static FUNCTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bfunction(?:\s+[A-Za-z_$][\w$]*)?\s*\(").expect("the function pattern is a literal")
});

/// `(start, end, body start)` of each braced JavaScript `function`.
pub(super) fn js_function_ranges(source: &str) -> Vec<(usize, usize, usize)> {
    scopes_of(&js_code_mask(source), &FUNCTION)
}

/// The first match of `pattern` in `source` whose masked text satisfies
/// `expected` (a comment or a string is not code) and `accepts`.
pub(super) fn active_match<'s>(
    source: &'s str,
    pattern: &Regex,
    expected: impl Fn(&str) -> bool,
    accepts: impl Fn(&Captures) -> bool,
) -> Option<Captures<'s>> {
    let masked = js_code_mask(source);
    pattern.captures_iter(source).find(|caps| {
        let whole = caps.get(0).expect("a match has a whole");
        expected(&masked[whole.start()..whole.end()]) && accepts(caps)
    })
}

/// The byte offset `count` characters before `at` in `text`, at least 0.
pub(super) fn back_chars(text: &str, at: usize, count: usize) -> usize {
    text[..at].char_indices().rev().take(count).last().map_or(at, |(offset, _)| offset)
}

/// The byte offset `count` characters after `at` in `text`, at most its end.
pub(super) fn forward_chars(text: &str, at: usize, count: usize) -> usize {
    text[at..].char_indices().nth(count).map_or(text.len(), |(offset, _)| at + offset)
}

/// `### path:lo-hi — note` and the numbered lines of the range of `source`
/// from byte `start` to byte `end`, at most [`SCOPED_BLOCK_MAX_LINES`].
pub(super) fn source_range(path: &str, source: &str, start: usize, end: usize, note: &str) -> Vec<String> {
    let first = source[..start].matches('\n').count() + 1;
    let last = source[..end].matches('\n').count() + 1;
    let lines = splitlines(source);
    let lo = first.max(1);
    let mut hi = lines.len().min(last);
    if hi + 1 > lo + SCOPED_BLOCK_MAX_LINES {
        hi = lo + SCOPED_BLOCK_MAX_LINES - 1;
    }
    let mut out = vec![format!("### {path}:{lo}-{hi} — {note}")];
    out.extend((lo..=hi).map(|at| format!("{at}| {}", head(lines[at - 1], 150))));
    out
}

/// A range with the coordinates kept and every secret-named subject or line masked.
pub(super) fn redact_source_range(lines: Vec<String>, subject: &str) -> Vec<String> {
    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 || !(SECRETISH.is_match(subject) || SECRETISH.is_match(&line)) {
                line
            } else {
                format!("{}| <masked>", line.split('|').next().unwrap_or(""))
            }
        })
        .collect()
}
