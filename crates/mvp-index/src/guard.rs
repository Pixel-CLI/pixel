// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! What keeps a file that is not source, or that a grammar chokes on, from
//! costing a run its time or a stack: shared by every rule file (v1
//! `extract.rs:219-305`).

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use tree_sitter::{Language, ParseOptions, ParseState, Parser, Tree};

/// Wall-clock cap on one parse. Error recovery on a few hundred malformed
/// bytes ran for minutes in v1 (#800); a parse past the cap yields no tree,
/// as a grammar failure does. A source file parses in milliseconds, so only
/// a pathological file ever meets it.
pub const PARSE_BUDGET: Duration = Duration::from_secs(3);

/// Size floor of the generated-blob guard: below it, even a one-line file
/// parses in microseconds, so the guard would only risk refusing source.
pub const GENERATED_MIN_BYTES: usize = 65_536;

/// Mean bytes per line from which a file of at least
/// [`GENERATED_MIN_BYTES`] is a generated or minified blob, not source.
/// v1 calibrated it on real trees (`extract.rs:226-248`): the worst
/// hand-written file of a 400k-line Rails monolith averages 127 bytes per
/// line, its 400 KB `schema.rb` 47, while a one-line bundle averages its
/// whole length. A per-line cap would refuse real source (a spec file with
/// a 16 044-byte line); only the whole-file mean separates the two.
pub const GENERATED_MAX_BYTES_PER_LINE: usize = 512;

/// A generated or minified blob a grammar should not be pointed at: at
/// least [`GENERATED_MIN_BYTES`] and a mean of at least
/// [`GENERATED_MAX_BYTES_PER_LINE`] bytes per line, an unterminated last
/// line counting as a line. A pure function of the bytes, so the same bytes
/// always yield the same rows.
pub fn is_generated_blob(content: &[u8]) -> bool {
    if content.len() < GENERATED_MIN_BYTES {
        return false;
    }
    let newlines = content.iter().filter(|b| **b == b'\n').count();
    let lines = if content.ends_with(b"\n") {
        newlines
    } else {
        newlines + 1
    };
    content.len() / lines.max(1) >= GENERATED_MAX_BYTES_PER_LINE
}

/// True once a parse has run longer than its budget; a parse that took
/// exactly the budget is still within it.
pub fn over_budget(elapsed: Duration, budget: Duration) -> bool {
    elapsed > budget
}

/// The tree of `content` in `language`, or `None` when the grammar fails to
/// load or the parse runs past `budget`.
pub fn parse(language: &Language, content: &[u8], budget: Duration) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(language).ok()?;
    let start = Instant::now();
    let mut stop_late = |_: &ParseState| {
        if over_budget(start.elapsed(), budget) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    parser.parse_with_options(
        &mut |offset, _| content.get(offset..).unwrap_or_default(),
        None,
        Some(ParseOptions::new().progress_callback(&mut stop_late)),
    )
}
