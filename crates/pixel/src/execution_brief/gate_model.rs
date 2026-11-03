// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The constants of the relevance gate's model.
//!
//! Copied by hand from `scripts/bench/research-gate/results/gate-model.json` (the
//! SHA-256 is in [`SOURCE`]): "multi-repository compact gate, five features",
//! an L2 logistic regression fitted on the English dev rows of the brief-gate
//! set of Pixel (#885) and of the multi-repository set (#906), French rows
//! scored for information only. The first, Pixel-only model is kept beside it
//! as `gate-model-pixel-only.json`.
//! Never read at run time: a change of model is a change of this file,
//! reviewed as one. A test holds every constant here to the json, and
//! `scripts/bench/research-gate/gate_reference.py` is the executable definition the
//! parity test in `evidence.rs` holds the Rust score to.

/// The json these constants were copied from, and its SHA-256; the decision
/// log names it beside every decision.
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
    use serde_json::Value;
    use sha2::{Digest, Sha256};

    use super::*;

    const JSON: &str =
        include_str!("../../../../scripts/bench/research-gate/results/gate-model.json");

    fn model() -> Value {
        serde_json::from_str(JSON).unwrap()
    }

    fn coefficient(model: &Value, name: &str) -> f64 {
        model["features"]
            .as_array()
            .unwrap()
            .iter()
            .find(|feature| feature["name"] == name)
            .unwrap_or_else(|| panic!("no feature {name}"))["raw_coefficient"]
            .as_f64()
            .unwrap()
    }

    fn strings(value: &Value) -> Vec<&str> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|word| word.as_str().unwrap())
            .collect()
    }

    #[test]
    fn the_constants_should_be_the_ones_in_the_json_they_cite() {
        let model = model();
        let digest: String = Sha256::digest(JSON.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert!(
            SOURCE.ends_with(&format!("sha256:{digest}")),
            "{SOURCE}: {digest}"
        );
        assert_eq!(INTERCEPT, model["intercept_raw"].as_f64().unwrap());
        assert_eq!(STRUCT_PER_MILLE, coefficient(&model, "struct_per_mille"));
        assert_eq!(QUESTION, coefficient(&model, "question"));
        assert_eq!(OPS_SHARE, coefficient(&model, "ops_share"));
        assert_eq!(STRUCT_RATIO, coefficient(&model, "struct_ratio"));
        assert_eq!(COVERAGE, coefficient(&model, "coverage"));
        assert_eq!(model["features"].as_array().unwrap().len(), 5);
        assert_eq!(HIGH, model["thresholds"]["high"]["score"].as_f64().unwrap());
        assert_eq!(LOW, model["thresholds"]["low"]["score"].as_f64().unwrap());
        assert_eq!(
            OPS_VOCAB.to_vec(),
            strings(&model["constants"]["ops_vocab"])
        );
        assert_eq!(
            QUESTION_WORDS.to_vec(),
            strings(&model["constants"]["question_words"])
        );
    }

    #[test]
    fn the_high_threshold_should_sit_above_the_low_one() {
        assert!(std::hint::black_box(HIGH) > LOW);
    }
}
