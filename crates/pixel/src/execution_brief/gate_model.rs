// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The constants of the relevance gate's model.
//!
//! Copied by hand from `scripts/research-gate/results/gate-model.json` as of
//! commit `d03f0b84` of #885 (the SHA-256 is in [`SOURCE`]): "english compact
//! gate, four features", an L2 logistic regression fitted on the 78 English
//! dev rows of the brief-gate set, French rows scored for information only.
//! Never read at run time: a change of model is a change of this file,
//! reviewed as one. A test holds every constant here to the json, and
//! `scripts/research-gate/gate_reference.py` is the executable definition the
//! parity test in `evidence.rs` holds the Rust score to.

/// The json these constants were copied from, and its SHA-256; the decision
/// log names it beside every decision.
pub(super) const SOURCE: &str = "gate-model.json@d03f0b84 sha256:cf5b4b0d68eba577da005b3164e386ff4a9f09ae9b582ba8df614adadce08bdf";

/// The score before any feature (`intercept_raw`).
pub(super) const INTERCEPT: f64 = -3.989_180_734_547_075;

/// Coefficient of `struct_per_mille` (`raw_coefficient`).
pub(super) const STRUCT_PER_MILLE: f64 = 0.603_666_617_618_777_8;

/// Coefficient of `question`.
pub(super) const QUESTION: f64 = 1.640_480_295_995_913_5;

/// Coefficient of `ops_share`.
pub(super) const OPS_SHARE: f64 = -4.912_487_487_594_396_5;

/// Coefficient of `struct_ratio`.
pub(super) const STRUCT_RATIO: f64 = 1.668_483_373_357_045_4;

/// A score strictly above this is a high-tier brief (5% of the off-topic
/// English dev rows out of fold: 1 of 35).
pub(super) const HIGH: f64 = 1.297_755_973_577_406_5;

/// A score strictly above this and not above [`HIGH`] is a low-tier brief
/// (10% out of fold: 3 of 35).
pub(super) const LOW: f64 = 0.731_187_160_147_051_3;

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

    const JSON: &str = include_str!("../../../../scripts/research-gate/results/gate-model.json");

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
        assert_eq!(model["features"].as_array().unwrap().len(), 4);
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
