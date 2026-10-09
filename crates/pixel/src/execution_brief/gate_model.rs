// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The constants of the relevance gate's model.
//!
//! Copied by hand from `scripts/research-gate/results/gate-model.json`
//! (feature definitions, mean, std, coefficient, intercept and the two
//! thresholds), never read at run time: a change of model is a change of this
//! file, reviewed as one. PLACEHOLDER: the file has not landed yet, and the
//! numbers below are a hand-set stand-in that keeps the pipeline running.
//! Replace them, and the SHA-256 of the json this module was filled from,
//! when it does.

use super::relevance::{Feature, Term};

/// What these constants were copied from.
pub(super) const SOURCE: &str = "placeholder: gate-model.json has not landed";

/// The model's intercept, on the standardised scale.
pub(super) const INTERCEPT: f64 = 0.0;

/// A score at or above this is a high-tier brief (about 5% false positives on
/// the fitting set).
pub(super) const HIGH: f64 = 1.5;

/// A score at or above this, and below [`HIGH`], is a low-tier brief (about
/// 10% false positives on the fitting set).
pub(super) const LOW: f64 = 0.5;

/// The terms of the model, in the order the json lists them.
pub(super) const TERMS: [Term; 11] = [
    Term {
        feature: Feature::Shared,
        coef: 0.9,
        mean: 3.0,
        std: 1.5,
    },
    Term {
        feature: Feature::Coverage,
        coef: 0.7,
        mean: 0.8,
        std: 0.3,
    },
    Term {
        feature: Feature::StructuralCoverage,
        coef: 0.5,
        mean: 0.55,
        std: 0.4,
    },
    Term {
        feature: Feature::StructuralFiles,
        coef: 0.3,
        mean: 3.4,
        std: 1.6,
    },
    Term {
        feature: Feature::Informative,
        coef: 0.1,
        mean: 4.0,
        std: 2.0,
    },
    Term {
        feature: Feature::Keywords,
        coef: 0.1,
        mean: 5.7,
        std: 2.4,
    },
    Term {
        feature: Feature::Agreement,
        coef: 0.6,
        mean: 0.3,
        std: 0.3,
    },
    Term {
        feature: Feature::TopLead,
        coef: 0.2,
        mean: 0.03,
        std: 0.01,
    },
    Term {
        feature: Feature::OpsShare,
        coef: -0.7,
        mean: 0.07,
        std: 0.17,
    },
    Term {
        feature: Feature::Question,
        coef: 0.5,
        mean: 0.5,
        std: 0.5,
    },
    Term {
        feature: Feature::Length,
        coef: 0.4,
        mean: 2.6,
        std: 0.5,
    },
];
