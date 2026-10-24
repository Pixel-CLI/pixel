// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Trigram extraction (`specs/index-history.md`).
//!
//! Every window of three bytes is a gram, identified by its xxh3 hash. A
//! query literal of at least three bytes has index-visible grams, and each of
//! them is a gram of any document that contains the literal, so candidate
//! filtering can never produce a false negative.

/// One extracted gram: content hash plus its span in the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GramHit {
    pub hash: u64,
    pub start: u32,
    pub len: u16,
}

/// What shards and the planner see of gram selection.
pub trait GramExtractor: Send + Sync {
    /// Extract all grams of `text` into `out` (appended; not cleared).
    fn grams(&self, text: &[u8], out: &mut Vec<GramHit>);

    /// The distinct gram hashes of a query literal, sorted. Sound with any
    /// subset of the literal's grams; empty means "cannot narrow" (literal
    /// shorter than a gram) and the caller must fall back to a broader plan.
    fn covering(&self, literal: &[u8]) -> Vec<u64>;

    /// Stable identifier persisted in shard headers.
    fn id(&self) -> String;
}

#[inline]
fn gram_hash(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(bytes)
}

/// Dense fixed-length trigrams.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrigramExtractor;

impl GramExtractor for TrigramExtractor {
    fn grams(&self, text: &[u8], out: &mut Vec<GramHit>) {
        for (i, win) in text.windows(3).enumerate() {
            out.push(GramHit {
                hash: gram_hash(win),
                start: i as u32,
                len: 3,
            });
        }
    }

    fn covering(&self, literal: &[u8]) -> Vec<u64> {
        let mut hashes: Vec<u64> = literal.windows(3).map(gram_hash).collect();
        hashes.sort_unstable();
        hashes.dedup();
        hashes
    }

    fn id(&self) -> String {
        "trigram".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashSet;

    fn grams_of(text: &[u8]) -> Vec<GramHit> {
        let mut out = Vec::new();
        TrigramExtractor.grams(text, &mut out);
        out
    }

    #[test]
    fn text_shorter_than_a_gram_has_none_and_a_literal_that_short_cannot_narrow() {
        for text in [&b""[..], b"a", b"ab"] {
            assert!(grams_of(text).is_empty());
            assert!(TrigramExtractor.covering(text).is_empty());
        }
    }

    #[test]
    fn three_bytes_are_one_gram_and_each_window_after_is_another() {
        let one = grams_of(b"abc");
        assert_eq!(
            one,
            [GramHit {
                hash: gram_hash(b"abc"),
                start: 0,
                len: 3
            }]
        );
        let spans: Vec<(u32, u16)> = grams_of(b"abcd").iter().map(|g| (g.start, g.len)).collect();
        assert_eq!(spans, [(0, 3), (1, 3)]);
    }

    #[test]
    fn covering_is_sorted_and_without_repeats() {
        let covering = TrigramExtractor.covering(b"aaaaaa");
        assert_eq!(covering, [gram_hash(b"aaa")]);
        let mixed = TrigramExtractor.covering(b"abcabc");
        assert!(mixed.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(mixed.len(), 3, "abc, bca, cab");
    }

    #[test]
    fn the_id_names_the_scheme() {
        assert_eq!(TrigramExtractor.id(), "trigram");
    }

    proptest! {
        /// A literal inside a text: every covering gram is a gram of the text.
        #[test]
        fn prop_no_false_negatives(
            prefix in proptest::collection::vec(any::<u8>(), 0..40),
            literal in proptest::collection::vec(any::<u8>(), 3..20),
            suffix in proptest::collection::vec(any::<u8>(), 0..40),
        ) {
            let mut text = prefix;
            text.extend_from_slice(&literal);
            text.extend_from_slice(&suffix);
            let in_text: HashSet<u64> = grams_of(&text).iter().map(|g| g.hash).collect();
            for hash in TrigramExtractor.covering(&literal) {
                prop_assert!(in_text.contains(&hash));
            }
        }

        #[test]
        fn prop_one_gram_per_window(text in proptest::collection::vec(any::<u8>(), 0..300)) {
            prop_assert_eq!(grams_of(&text).len(), text.len().saturating_sub(2));
        }
    }
}
