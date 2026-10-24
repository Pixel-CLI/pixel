// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The constants of the relevance gate's model.
//!
//! Copied by hand from Pixel's `gate-model.json` (the SHA-256 is in [`SOURCE`];
//! the file is not carried): "multi-repository compact gate, five features",
//! an L2 logistic regression fitted on the English dev rows of the brief-gate
//! set of Pixel (#885) and of the multi-repository set (#906), French rows
//! scored for information only.
//! Never read at run time: a change of model is a change of this file,
//! reviewed as one. The recorded blocks of `gate_parity.json` hold the Rust
//! score to the reference scorer's.

/// The json these constants were copied from, and its SHA-256; the journal
/// names it beside every decision.
pub(super) const SOURCE: &str = "gate-model.json@multirepo sha256:396b14dcc8f2942290eaeca7fa38efe5b866f8521faf58f7df7f3f3479651d7a";

/// The score before any feature (`intercept_raw`).
pub(super) const INTERCEPT: f64 = -5.103_519_618_265_883;

/// Coefficient of `struct_per_mille` (`raw_coefficient`).
pub(super) const STRUCT_PER_MILLE: f64 = 0.802_889_747_563_141_1;

/// Coefficient of `question`.
pub(super) const QUESTION: f64 = 1.278_180_008_182_126_6;

/// Coefficient of `ops_share`.
pub(super) const OPS_SHARE: f64 = -7.777_850_066_247_161;

/// Coefficient of `coverage`.
pub(super) const COVERAGE: f64 = 1.264_495_518_915_246_7;

/// Coefficient of `struct_ratio`.
pub(super) const STRUCT_RATIO: f64 = 1.354_003_905_490_292_8;

/// A score strictly above this is a high-tier brief (5% of the off-topic
/// English dev rows of both sets out of fold).
pub(super) const HIGH: f64 = 1.184_411_977_526_533_8;

/// A score strictly above this and not above [`HIGH`] is a low-tier brief
/// (10% out of fold).
pub(super) const LOW: f64 = 0.612_458_650_457_532_8;

/// Keywords that name a git, release or CI operation (`constants.ops_vocab`).
pub(super) const OPS_VOCAB: [&str; 37] = [
    "amend",
    "branch",
    "branches",
    "bump",
    "changelog",
    "checkout",
    "cherry",
    "ci",
    "clone",
    "commit",
    "commits",
    "conflict",
    "deploy",
    "fetch",
    "force",
    "git",
    "main",
    "master",
    "merge",
    "merged",
    "origin",
    "pipeline",
    "pr",
    "publish",
    "pull",
    "push",
    "rebase",
    "release",
    "remote",
    "revert",
    "squash",
    "stash",
    "tag",
    "tags",
    "upstream",
    "version",
    "workflow",
];

/// Words that open a question (`constants.question_words`).
pub(super) const QUESTION_WORDS: [&str; 15] = [
    "are", "can", "could", "do", "does", "how", "is", "should", "what", "when", "where", "which",
    "who", "why", "would",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_high_threshold_should_sit_above_the_low_one() {
        assert!(std::hint::black_box(HIGH) > LOW);
    }
}
