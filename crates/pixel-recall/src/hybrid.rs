// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Reciprocal-rank fusion of the lexical and semantic channels.
//!
//! RRF needs no score calibration (trigram counts and cosine sims have
//! incomparable distributions) and degrades gracefully when one channel
//! returns nothing. Lexical carries a small weight bonus: exact
//! identifiers should beat paraphrase.

use std::collections::HashMap;

const RRF_K: f32 = 60.0;
const LEXICAL_WEIGHT: f32 = 2.0;
const SEMANTIC_WEIGHT: f32 = 1.0;

/// Fuse two ranked turn-id lists into one, best first.
pub fn fuse(lexical: &[i64], semantic: &[i64]) -> Vec<(i64, f32)> {
    let mut scores: HashMap<i64, f32> = HashMap::new();
    for (rank, id) in lexical.iter().enumerate() {
        *scores.entry(*id).or_default() += LEXICAL_WEIGHT / (RRF_K + rank as f32 + 1.0);
    }
    for (rank, id) in semantic.iter().enumerate() {
        *scores.entry(*id).or_default() += SEMANTIC_WEIGHT / (RRF_K + rank as f32 + 1.0);
    }
    let mut out: Vec<(i64, f32)> = scores.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuses_and_ranks() {
        // id 5 appears high in both lists → must win.
        let fused = fuse(&[5, 1, 2], &[9, 5, 3]);
        assert_eq!(fused[0].0, 5);
        // One empty channel degrades to the other's order.
        let fused = fuse(&[], &[7, 8]);
        assert_eq!(fused[0].0, 7);
        assert_eq!(fused[1].0, 8);
    }
}
