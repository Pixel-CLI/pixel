// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The decision seam: one bounded question, one answer.
//!
//! Production answers through `pixel classify` — a remote chat completion
//! or the local Ollaya typed-choice readout. Tests script it, so the loop
//! is exercised without a model, a network, or a browser.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One question: the state to judge, the framing every option shares, and
/// what each option means.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    /// What varies cycle to cycle — the page.
    pub text: String,
    /// Framing identical for every option — the goal and the rules.
    pub context: String,
    pub labels: Vec<String>,
    pub criteria: BTreeMap<String, String>,
}

/// What the engine answered: a distribution over the labels it was given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Distribution {
    pub probabilities: BTreeMap<String, f64>,
    /// The model that answered, disclosed in the trace.
    pub model: String,
}

/// The decision engine behind the loop.
pub trait Decider {
    /// How many options one question may offer this engine.
    ///
    /// A model's own budget is below the schema ceiling, and an over-budget
    /// question is refused by the engine rather than truncated by it, so a
    /// caller that builds a question (`ActionSpace`) has to ask first.
    fn option_budget(&self) -> usize;

    fn decide(&mut self, request: &Decision) -> Result<Distribution, String>;
}

/// The index of the winning label: the highest probability among `labels`,
/// earliest on a tie. `None` when the engine gave no offered label a
/// probability at all.
///
/// One tie rule for every question this crate asks — a page decision and a
/// value decision must agree on what "the same answer" means, and a run
/// has to be reproducible from the same distribution.
pub fn argmax_index(probabilities: &BTreeMap<String, f64>, labels: &[String]) -> Option<usize> {
    let mut winner: Option<(usize, f64)> = None;
    for (index, label) in labels.iter().enumerate() {
        let Some(probability) = probabilities.get(label) else {
            continue;
        };
        if winner.is_none_or(|(_, best)| *probability > best) {
            winner = Some((index, *probability));
        }
    }
    winner.map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    fn probs(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs
            .iter()
            .map(|(label, p)| ((*label).to_string(), *p))
            .collect()
    }

    #[test]
    fn the_winner_is_the_highest_probability_among_the_offered_labels() {
        let offered = labels(&["a", "b", "c"]);
        assert_eq!(
            argmax_index(&probs(&[("a", 0.2), ("b", 0.7), ("c", 0.1)]), &offered),
            Some(1)
        );
        assert_eq!(
            argmax_index(&probs(&[("c", 0.9), ("a", 0.05)]), &offered),
            Some(2)
        );
        // A label the engine invented is not a candidate, however high.
        assert_eq!(argmax_index(&probs(&[("zzz", 1.0)]), &offered), None);
        assert_eq!(argmax_index(&BTreeMap::new(), &offered), None);
    }

    #[test]
    fn a_tie_resolves_to_the_earliest_offered_label() {
        let offered = labels(&["a", "b"]);
        // The map's own order must not decide: `b` sorts before `a` and
        // would win a naive iteration.
        assert_eq!(
            argmax_index(&probs(&[("b", 0.5), ("a", 0.5)]), &offered),
            Some(0)
        );
        // A hair more on the later label wins outright.
        assert_eq!(
            argmax_index(&probs(&[("b", 0.500_000_1), ("a", 0.5)]), &offered),
            Some(1)
        );
        assert_eq!(
            argmax_index(&probs(&[("b", 0.6), ("a", 0.5)]), &offered),
            Some(1)
        );
    }

    #[test]
    fn no_label_is_ever_a_candidate() {
        assert_eq!(argmax_index(&probs(&[("a", 1.0)]), &[]), None);
    }
}
