// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Sparse n-gram extraction (convex-hull / monotonic-stack selection).
//!
//! Independent Rust implementation of the algorithm described by ClickHouse
//! (`sparseGramsImpl.h`) and Cursor ("Fast regex search"). See NOTICE.
//!
//! Selection predicate (context-free): slide a window of `min_len - 1` bytes
//! across the text and weigh each window. A substring spanning windows
//! `i..=j` (bytes `i .. j + w`) is a sparse gram iff the weights at both end
//! windows are strictly greater than every interior window weight, and its
//! byte length lies in `[min_len, max_len]`. Adjacent window pairs have an
//! empty interior, so every `min_len`-byte substring is always a gram —
//! which is what guarantees any literal of at least `min_len` bytes has
//! index-visible grams.
//!
//! Because the predicate only reads bytes inside the substring, every sparse
//! gram of a query literal is also a sparse gram of any document containing
//! that literal: candidate filtering can never produce false negatives.
//! (Property-tested against a brute-force oracle below.)

use std::collections::VecDeque;

use crate::weights::Weigher;

pub const DEFAULT_MIN_GRAM: usize = 3;
pub const DEFAULT_MAX_GRAM: usize = 100;

/// One extracted gram: content hash plus its span in the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GramHit {
    pub hash: u64,
    pub start: u32,
    pub len: u16,
}

/// The seam that keeps the sparse-gram bet reversible: `TrigramExtractor`
/// implements the same trait, and the Phase 1 benchmark gate picks the
/// default. Everything downstream (shards, planner) only sees this trait.
pub trait GramExtractor: Send + Sync {
    /// Extract all grams of `text` into `out` (appended; not cleared).
    fn grams(&self, text: &[u8], out: &mut Vec<GramHit>);

    /// Minimal covering gram hashes for a query literal. Sound with any
    /// subset of the literal's grams; empty means "cannot narrow" (literal
    /// shorter than the minimum gram length) and the caller must fall back
    /// to a broader plan.
    fn covering(&self, literal: &[u8]) -> Vec<u64>;

    /// Stable identifier persisted in shard headers.
    fn id(&self) -> String;
}

#[inline]
fn gram_hash(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(bytes)
}

pub struct SparseGramExtractor<W: Weigher> {
    weigher: W,
    min_len: usize,
    max_len: usize,
}

#[derive(Debug, Clone, Copy)]
struct HullEntry {
    left: usize,
    weight: u32,
}

impl<W: Weigher> SparseGramExtractor<W> {
    pub fn new(weigher: W) -> Self {
        Self::with_lengths(weigher, DEFAULT_MIN_GRAM, DEFAULT_MAX_GRAM)
    }

    pub fn with_lengths(weigher: W, min_len: usize, max_len: usize) -> Self {
        assert!(min_len >= 3, "min gram length must be >= 3");
        assert!(max_len >= min_len);
        assert!(u16::try_from(max_len).is_ok());
        Self {
            weigher,
            min_len,
            max_len,
        }
    }

    #[inline]
    fn window(&self) -> usize {
        self.min_len - 1
    }

    fn emit(&self, text: &[u8], left: usize, right: usize, out: &mut Vec<GramHit>) {
        let end = right + self.window();
        let len = end - left;
        debug_assert!(len >= self.min_len && len <= self.max_len);
        out.push(GramHit {
            hash: gram_hash(&text[left..end]),
            start: left as u32,
            len: len as u16,
        });
    }
}

impl<W: Weigher> GramExtractor for SparseGramExtractor<W> {
    fn grams(&self, text: &[u8], out: &mut Vec<GramHit>) {
        let w = self.window();
        if text.len() < self.min_len {
            return;
        }
        let nwin = text.len() - w + 1;

        // Monotonic strictly-decreasing (by weight, bottom -> top) hull of
        // candidate left ends. Front = oldest/heaviest, back = newest.
        let mut hull: VecDeque<HullEntry> = VecDeque::with_capacity(64);

        for right in 0..nwin {
            let weight = self.weigher.weight(&text[right..right + w]);

            // Left ends whose gram would exceed max_len can never emit again
            // (spans only grow); they form a prefix at the front of the hull.
            while let Some(front) = hull.front() {
                if right + w - front.left > self.max_len {
                    hull.pop_front();
                } else {
                    break;
                }
            }

            // Pop and emit every dominated left end. An equal weight also
            // emits (the predicate compares ends to the *interior* only) but
            // then blocks the surviving top: the equal window at `right`
            // would sit in that pair's interior without being strictly
            // dominated by it.
            let mut allow_survivor_pair = true;
            while let Some(top) = hull.back().copied() {
                if top.weight < weight {
                    self.emit(text, top.left, right, out);
                    hull.pop_back();
                } else if top.weight == weight {
                    self.emit(text, top.left, right, out);
                    hull.pop_back();
                    allow_survivor_pair = false;
                    break;
                } else {
                    break;
                }
            }

            // The nearest surviving left end strictly dominates everything
            // between it and `right` (all of that was just popped or is
            // lighter), so it pairs with `right` as well.
            if allow_survivor_pair && let Some(top) = hull.back() {
                self.emit(text, top.left, right, out);
            }

            hull.push_back(HullEntry {
                left: right,
                weight,
            });
        }
    }

    fn covering(&self, literal: &[u8]) -> Vec<u64> {
        let mut hits = Vec::new();
        self.grams(literal, &mut hits);
        if hits.is_empty() {
            return Vec::new();
        }
        // MVP covering (ClickHouse PR #93078 shape): keep longest grams,
        // drop any gram whose span is contained in an already-kept span —
        // if the index holds the longer gram it holds a superset of the
        // matches the shorter one would contribute to the AND.
        hits.sort_by(|a, b| b.len.cmp(&a.len).then(a.start.cmp(&b.start)));
        let mut kept: Vec<GramHit> = Vec::new();
        for h in hits {
            let contained = kept.iter().any(|k| {
                k.start <= h.start && u32::from(k.len) + k.start >= u32::from(h.len) + h.start
            });
            if !contained {
                kept.push(h);
            }
        }
        let mut hashes: Vec<u64> = kept.iter().map(|k| k.hash).collect();
        hashes.sort_unstable();
        hashes.dedup();
        hashes
    }

    fn id(&self) -> String {
        format!(
            "sparse-{}-{}-{}",
            self.weigher.id(),
            self.min_len,
            self.max_len
        )
    }
}

/// Dense fixed-length trigram extractor behind the same trait — the Phase 1
/// benchmark-gate fallback.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrigramExtractor;

impl GramExtractor for TrigramExtractor {
    fn grams(&self, text: &[u8], out: &mut Vec<GramHit>) {
        if text.len() < 3 {
            return;
        }
        for (i, win) in text.windows(3).enumerate() {
            out.push(GramHit {
                hash: gram_hash(win),
                start: i as u32,
                len: 3,
            });
        }
    }

    fn covering(&self, literal: &[u8]) -> Vec<u64> {
        if literal.len() < 3 {
            return Vec::new();
        }
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
    use crate::weights::Crc32Weigher;
    use proptest::prelude::*;
    use std::collections::HashSet;

    fn extractor() -> SparseGramExtractor<Crc32Weigher> {
        SparseGramExtractor::new(Crc32Weigher)
    }

    /// O(n^3) oracle implementing the predicate literally.
    fn brute_force(text: &[u8], min_len: usize, max_len: usize) -> HashSet<(u32, u16)> {
        let w = min_len - 1;
        let mut set = HashSet::new();
        if text.len() < min_len {
            return set;
        }
        let weights: Vec<u32> = text
            .windows(w)
            .map(|win| Crc32Weigher.weight(win))
            .collect();
        let nwin = weights.len();
        for i in 0..nwin {
            for j in (i + 1)..nwin {
                let len = j + w - i;
                if len > max_len {
                    break;
                }
                let interior = &weights[i + 1..j];
                let dominated = interior.iter().all(|&m| weights[i] > m && weights[j] > m);
                if dominated {
                    set.insert((i as u32, len as u16));
                }
            }
        }
        set
    }

    fn algo_spans(text: &[u8], min_len: usize, max_len: usize) -> HashSet<(u32, u16)> {
        let ex = SparseGramExtractor::with_lengths(Crc32Weigher, min_len, max_len);
        let mut hits = Vec::new();
        ex.grams(text, &mut hits);
        let spans: Vec<(u32, u16)> = hits.iter().map(|h| (h.start, h.len)).collect();
        let set: HashSet<(u32, u16)> = spans.iter().copied().collect();
        assert_eq!(set.len(), spans.len(), "duplicate span emitted");
        set
    }

    #[test]
    fn empty_and_tiny_inputs() {
        let ex = extractor();
        let mut out = Vec::new();
        ex.grams(b"", &mut out);
        ex.grams(b"ab", &mut out);
        assert!(out.is_empty());
        assert!(ex.covering(b"ab").is_empty());
    }

    #[test]
    fn three_bytes_is_one_gram() {
        let ex = extractor();
        let mut out = Vec::new();
        ex.grams(b"abc", &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].start, out[0].len), (0, 3));
    }

    #[test]
    fn matches_brute_force_on_fixed_cases() {
        for text in [
            &b"MAX_FILE_SIZE"[..],
            b"handleClick",
            b"aaaaaaaaaaaaaaaa",
            b"abababababab",
            b"fn main() { println!(\"hello\"); }",
            b"\x00\x01\x00\x01\xff\xfe",
        ] {
            assert_eq!(
                algo_spans(text, 3, 100),
                brute_force(text, 3, 100),
                "mismatch on {:?}",
                String::from_utf8_lossy(text)
            );
        }
    }

    /// The max-length cutoff must drop over-long left ends without
    /// disturbing shorter pairs (regression for the hull-eviction subtlety).
    #[test]
    fn max_len_cutoff_matches_brute_force() {
        let text: Vec<u8> = (0..200u8).map(|i| i.wrapping_mul(37)).collect();
        for max_len in [3usize, 4, 5, 8, 16, 33] {
            assert_eq!(
                algo_spans(&text, 3, max_len),
                brute_force(&text, 3, max_len),
                "mismatch at max_len={max_len}"
            );
        }
    }

    #[test]
    fn adjacent_pairs_always_emitted() {
        let text = b"some ordinary text with repeats repeats";
        let spans = algo_spans(text, 3, 100);
        for i in 0..text.len() - 2 {
            assert!(spans.contains(&(i as u32, 3)), "missing min-gram at {i}");
        }
    }

    proptest! {
        #[test]
        fn prop_matches_brute_force(text in proptest::collection::vec(any::<u8>(), 0..200)) {
            prop_assert_eq!(algo_spans(&text, 3, 100), brute_force(&text, 3, 100));
        }

        #[test]
        fn prop_matches_brute_force_small_alphabet(
            text in proptest::collection::vec(0u8..4, 0..200),
            max_len in 3usize..40,
        ) {
            // Small alphabet forces many equal-weight ties; varied max_len
            // exercises the front-eviction path.
            prop_assert_eq!(algo_spans(&text, 3, max_len), brute_force(&text, 3, max_len));
        }

        #[test]
        fn prop_no_false_negatives(
            text in proptest::collection::vec(any::<u8>(), 3..300),
            start in 0usize..250,
            len in 3usize..60,
        ) {
            let start = start.min(text.len() - 1);
            let len = len.min(text.len() - start);
            prop_assume!(len >= 3);
            let literal = &text[start..start + len];

            let ex = extractor();
            let mut doc_hits = Vec::new();
            ex.grams(&text, &mut doc_hits);
            let doc_hashes: HashSet<u64> = doc_hits.iter().map(|h| h.hash).collect();

            // Every covering gram of any substring must exist in the
            // document's gram set — the load-bearing soundness property.
            for h in ex.covering(literal) {
                prop_assert!(doc_hashes.contains(&h));
            }
        }

        #[test]
        fn prop_emission_rate_bounded(text in proptest::collection::vec(any::<u8>(), 0..2000)) {
            let ex = extractor();
            let mut out = Vec::new();
            ex.grams(&text, &mut out);
            // Each position pushes one hull entry (one pop-emission max) and
            // adds at most one survivor-pair emission: hard bound 2N.
            prop_assert!(out.len() <= 2 * text.len().max(1));
        }
    }

    #[test]
    fn covering_is_smaller_than_trigram_decomposition() {
        let ex = extractor();
        let sparse = ex.covering(b"handleClick");
        let tri = TrigramExtractor.covering(b"handleClick");
        assert!(!sparse.is_empty());
        assert!(
            sparse.len() < tri.len(),
            "sparse covering ({}) not smaller than trigrams ({})",
            sparse.len(),
            tri.len()
        );
    }
}
