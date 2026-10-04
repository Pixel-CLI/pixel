// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel_index::plan::plan_pattern` against the matcher that verifies it.
//!
//! A search pattern comes from whoever calls `pixel search-content` (an
//! agent, a hook rewriting a `grep`), and the documents from the
//! repository. The index narrows a search to the files whose grams satisfy
//! the plan, then `pixel_index::verify::Verifier` runs the real regex on
//! them; a file the plan wrongly drops is a match the search silently never
//! reports. `plan.rs` states the contract: "sound, never complete".
//!
//! Input: `<pattern>\n<document>`, e.g. `fn\s+main\nfn main() {}`. The
//! seeds under `fuzz/seeds/search_plan/` hold a few shapes the planner
//! treats differently (literals, alternations, classes, repetitions).
//!
//! Invariants, for both gram extractors (`TrigramExtractor`, the one the
//! daemon builds, and `SparseGramExtractor`):
//! - planning never panics, whatever the pattern;
//! - soundness: when the verifier's matcher (`grep_regex::RegexMatcher`)
//!   matches the document, the plan resolved over the document's grams
//!   keeps it as a candidate.
#![no_main]

use std::collections::HashSet;

use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use libfuzzer_sys::fuzz_target;
use pixel_index::plan::plan_pattern;
use pixel_index::posting::resolve_query;
use pixel_index::{Crc32Weigher, GramExtractor, SparseGramExtractor, TrigramExtractor};

/// Compiled-program cap for the oracle's matcher. Planning is unaffected;
/// a pattern over it only loses the soundness check, which keeps each run
/// fast instead of spending it compiling `\w{100}{100}`.
const MATCHER_SIZE_LIMIT: usize = 1 << 20;

fuzz_target!(|data: &[u8]| {
    let Some(newline) = data.iter().position(|&byte| byte == b'\n') else {
        return;
    };
    let Ok(pattern) = std::str::from_utf8(&data[..newline]) else {
        return;
    };
    let document = &data[newline + 1..];

    let sparse = SparseGramExtractor::new(Crc32Weigher);
    let extractors: [&dyn GramExtractor; 2] = [&TrigramExtractor, &sparse];
    let plans: Vec<_> = extractors
        .iter()
        .map(|extractor| plan_pattern(pattern, *extractor))
        .collect();

    let Ok(matcher) = RegexMatcherBuilder::new()
        .size_limit(MATCHER_SIZE_LIMIT)
        .build(pattern)
    else {
        return;
    };
    if !matches!(matcher.is_match(document), Ok(true)) {
        return;
    }
    for (extractor, plan) in extractors.iter().zip(&plans) {
        // A pattern the planner rejects is an error the search reports,
        // not a dropped file.
        let Ok(plan) = plan else {
            continue;
        };
        let mut hits = Vec::new();
        extractor.grams(document, &mut hits);
        let grams: HashSet<u64> = hits.iter().map(|hit| hit.hash).collect();
        let candidates = resolve_query(plan, 1, &|gram| {
            if grams.contains(&gram) {
                vec![0]
            } else {
                Vec::new()
            }
        });
        assert_eq!(
            candidates,
            [0],
            "{} plan {plan:?} drops a document {pattern:?} matches",
            extractor.id(),
        );
    }
});
