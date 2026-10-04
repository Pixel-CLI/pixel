// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The dynamic action space: one option per legal operation-target pair.
//!
//! This is the shape the whole loop is built on. A cycle is not asked
//! "which operation?" and then "which target?" — it is asked one question
//! whose options *are* the operation-target pairs the page currently
//! offers, so one answer is one executable action and the two can never
//! disagree. An option that cannot run is never offered, and the label and
//! the action are stored side by side, so there is no label to parse back.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::elements::Element;

/// The operations the action space offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Op {
    Click,
    Type,
    Select,
    ScrollUp,
    ScrollDown,
    Wait,
    Done,
    Blocked,
}

impl Op {
    /// The word a label carries for this operation.
    pub fn word(self) -> &'static str {
        match self {
            Op::Click => "CLICK",
            Op::Type => "TYPE",
            Op::Select => "SELECT",
            Op::ScrollUp => "SCROLL_UP",
            Op::ScrollDown => "SCROLL_DOWN",
            Op::Wait => "WAIT",
            Op::Done => "DONE",
            Op::Blocked => "BLOCKED",
        }
    }

    /// Whether the operation reaches the page without naming an element.
    pub fn is_targetless(self) -> bool {
        matches!(
            self,
            Op::ScrollUp | Op::ScrollDown | Op::Wait | Op::Done | Op::Blocked
        )
    }
}

/// One executable action: what to do, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    pub op: Op,
    /// The slot the operation targets, absent for a targetless operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<usize>,
}

impl Action {
    /// The label this action is offered under.
    pub fn label(self) -> String {
        match (self.op.is_targetless(), self.slot) {
            (false, Some(slot)) => format!("{} {slot}", self.op.word()),
            _ => self.op.word().to_string(),
        }
    }
}

/// Most labels one question accepts — Ollaya's `choice` ceiling and
/// TypeSafe's shared schema limit. A model's own budget is lower
/// (`Decider::option_budget`), and that is the number that matters at run
/// time; this is the ceiling the space is clamped to.
pub const MAX_LABELS: usize = 255;
/// Labels reserved for the targetless operations, so a long page can never
/// crowd out `DONE`/`BLOCKED`.
const TARGETLESS_LABELS: usize = 5;

/// A decided cycle: the winning option, and the element it acts on.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub label: String,
    pub probability: f64,
    pub action: Action,
    /// The element a targeted action acts on. `None` exactly when the
    /// action is targetless: the space is built from the observation the
    /// question is asked about, so a targeted option always carries its
    /// element and no lookup can fail.
    pub element: Option<Element>,
}

/// The offered options for one observation, labels, actions and elements
/// aligned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionSpace {
    labels: Vec<String>,
    actions: Vec<Action>,
    elements: Vec<Option<Element>>,
    criteria: BTreeMap<String, String>,
    /// Slots a full space could not offer, in snapshot order.
    truncated: Vec<usize>,
}

impl ActionSpace {
    /// Build the space over one observation's actionable elements, offering
    /// at most `budget` options.
    ///
    /// `budget` is the engine's own limit (`Decider::option_budget`), and
    /// what does not fit is reported in [`ActionSpace::truncated`] rather
    /// than dropped quietly: a run whose page overflowed its options is
    /// never passed off as one that saw everything.
    pub fn of(elements: &[Element], budget: usize) -> ActionSpace {
        let room = budget.clamp(TARGETLESS_LABELS, MAX_LABELS) - TARGETLESS_LABELS;
        let mut space = ActionSpace {
            labels: Vec::new(),
            actions: Vec::new(),
            elements: Vec::new(),
            criteria: BTreeMap::new(),
            truncated: Vec::new(),
        };
        for element in elements {
            let Some(slot) = element.slot else {
                continue;
            };
            let ops = element.operations();
            if ops.is_empty() {
                continue;
            }
            // A slot's options are all-or-nothing: a half-offered combobox
            // would offer `TYPE` with no `SELECT`.
            if space.labels.len() + ops.len() > room {
                space.truncated.push(slot);
                continue;
            }
            for op in ops {
                let action = Action {
                    op,
                    slot: Some(slot),
                };
                space.criteria.insert(action.label(), element.describe());
                space.labels.push(action.label());
                space.actions.push(action);
                space.elements.push(Some(element.clone()));
            }
        }
        for op in [
            Op::ScrollUp,
            Op::ScrollDown,
            Op::Wait,
            Op::Done,
            Op::Blocked,
        ] {
            let action = Action { op, slot: None };
            space
                .criteria
                .insert(action.label(), targetless_criterion(op).to_string());
            space.labels.push(action.label());
            space.actions.push(action);
            space.elements.push(None);
        }
        space
    }

    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    pub fn criteria(&self) -> &BTreeMap<String, String> {
        &self.criteria
    }

    pub fn truncated(&self) -> &[usize] {
        &self.truncated
    }

    /// The winning option for a distribution, or `None` when the engine
    /// gave no probability to any offered label.
    pub fn choose(&self, probabilities: &BTreeMap<String, f64>) -> Option<Choice> {
        let index = crate::decide::argmax_index(probabilities, &self.labels)?;
        Some(Choice {
            label: self.labels[index].clone(),
            probability: probabilities[&self.labels[index]],
            action: self.actions[index],
            element: self.elements[index].clone(),
        })
    }
}

/// What a targetless operation means, and when it is the right answer.
fn targetless_criterion(op: Op) -> &'static str {
    match op {
        Op::ScrollUp => {
            "Move the page up; use when the needed control is above the current viewport."
        }
        Op::ScrollDown => {
            "Move the page down; use when the needed control is below the current viewport."
        }
        Op::Wait => {
            "Do nothing this cycle and look again; use only when the needed control is absent or \
             disabled, or submitted results are still loading."
        }
        Op::Done => "The goal is complete: every requirement is visibly satisfied on this page.",
        Op::Blocked => "No other option can make progress toward the goal.",
        Op::Click | Op::Type | Op::Select => "A targeted operation; never offered without a slot.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elements::parse_snapshot;

    fn space(body: &str) -> ActionSpace {
        space_within(body, MAX_LABELS)
    }

    fn space_within(body: &str, budget: usize) -> ActionSpace {
        let (elements, _) = parse_snapshot(body);
        ActionSpace::of(&elements, budget)
    }

    fn probs(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs
            .iter()
            .map(|(label, p)| ((*label).to_string(), *p))
            .collect()
    }

    #[test]
    fn every_option_is_an_executable_operation_target_pair() {
        let space = space(
            "- button \"Search\" [ref=e1]\n\
             - textbox \"Where from?\" [ref=e2]\n\
             - combobox \"Trip type\" [ref=e3]\n\
             - heading \"Title\" [ref=e4]\n\
             - button \"Off\" [disabled, ref=e5]",
        );
        assert_eq!(
            space.labels(),
            [
                "CLICK 1",
                "TYPE 2",
                // The ambiguous combobox offers both, in a fixed order.
                "TYPE 3",
                "SELECT 3",
                "SCROLL_UP",
                "SCROLL_DOWN",
                "WAIT",
                "DONE",
                "BLOCKED",
            ]
        );
        // Each option carries what the element is.
        assert_eq!(space.criteria()["CLICK 1"], "button \"Search\"");
        assert_eq!(space.criteria()["TYPE 2"], "textbox \"Where from?\"");
        assert_eq!(space.criteria()["SELECT 3"], "combobox \"Trip type\"");
        assert!(space.criteria()["DONE"].contains("complete"));
        assert!(space.criteria()["BLOCKED"].contains("progress"));
        assert!(space.truncated().is_empty());
        assert_eq!(space.labels().len(), 9);
    }

    #[test]
    fn a_label_names_the_slot_of_the_action_it_was_offered_for() {
        let space = space("- button \"Only\" [ref=e7]");
        let choice = space.choose(&probs(&[("CLICK 1", 0.9)])).unwrap();
        assert_eq!(
            choice.action,
            Action {
                op: Op::Click,
                slot: Some(1)
            }
        );
        assert_eq!(choice.label, "CLICK 1");
        assert_eq!(choice.probability, 0.9);
        assert_eq!(choice.action.label(), "CLICK 1");
        // A targetless action labels itself without a slot.
        assert_eq!(
            Action {
                op: Op::Done,
                slot: None
            }
            .label(),
            "DONE"
        );
        assert_eq!(Op::ScrollUp.word(), "SCROLL_UP");
        assert!(Op::Done.is_targetless());
        assert!(!Op::Click.is_targetless());
    }

    #[test]
    fn the_winning_option_is_the_argmax_over_what_was_offered() {
        let space = space("- button \"A\" [ref=e1]\n- button \"B\" [ref=e2]");
        // Labels the engine invented carry no probability for an offered
        // option and are simply not candidates.
        let choice = space
            .choose(&probs(&[
                ("CLICK 2", 0.8),
                ("CLICK 1", 0.1),
                ("TELEPORT", 0.9),
            ]))
            .unwrap();
        assert_eq!(choice.label, "CLICK 2");
        assert_eq!(choice.probability, 0.8);
        // Nothing offered got a probability: no action, never a guess.
        assert_eq!(space.choose(&probs(&[("TELEPORT", 1.0)])), None);
        assert_eq!(space.choose(&BTreeMap::new()), None);
    }

    /// The tie rule is what makes a run reproducible: the earliest offered
    /// option wins, not the map's order.
    #[test]
    fn a_tie_resolves_to_the_earliest_offered_option() {
        let space = space("- button \"A\" [ref=e1]\n- button \"B\" [ref=e2]");
        let tie = space
            .choose(&probs(&[("CLICK 2", 0.5), ("CLICK 1", 0.5)]))
            .unwrap();
        assert_eq!(tie.label, "CLICK 1");
        assert_eq!(tie.probability, 0.5);
        // A hair more on the later option still loses.
        let late = space
            .choose(&probs(&[("CLICK 2", 0.5), ("CLICK 1", 0.500_000_1)]))
            .unwrap();
        assert_eq!(late.label, "CLICK 1");
    }

    /// The budget is the *engine's*: what does not fit is reported, never
    /// dropped quietly, and the five targetless options always survive.
    #[test]
    fn the_engine_budget_bounds_the_options_and_the_rest_is_reported() {
        let body = "- button \"a\" [ref=e1]\n- button \"b\" [ref=e2]\n- button \"c\" [ref=e3]";
        let space = space_within(body, 7);
        assert_eq!(
            space.labels(),
            [
                "CLICK 1",
                "CLICK 2",
                "SCROLL_UP",
                "SCROLL_DOWN",
                "WAIT",
                "DONE",
                "BLOCKED"
            ],
            "two element options fit in a budget of seven"
        );
        assert_eq!(space.truncated(), [3]);
        assert_eq!(space.labels().len(), 7);

        // A budget below the five targetless options cannot shrink them: a
        // space always says how to stop.
        let tiny = space_within(body, 2);
        assert_eq!(tiny.labels().len(), 5);
        assert_eq!(tiny.truncated(), [1, 2, 3]);
        // And a budget past the schema ceiling is clamped to it.
        let wide: String = (1..=MAX_LABELS + 2)
            .map(|i| format!("- button \"b{i}\" [ref=e{i}]\n"))
            .collect();
        let huge = space_within(&wide, 10_000);
        assert_eq!(huge.labels().len(), MAX_LABELS, "the schema ceiling holds");
        assert_eq!(huge.truncated().first(), Some(&(MAX_LABELS - 4)));
        assert_eq!(huge.truncated().len(), 7);
    }

    /// A page that offers both operations on one element must not lose the
    /// element half-way: the pair is all-or-nothing.
    #[test]
    fn a_two_operation_element_is_offered_atomically() {
        let body = "- button \"a\" [ref=e1]\n- button \"b\" [ref=e2]\n- combobox \"c\" [ref=e3]";
        // Room for three element options: the combobox needs two, and it
        // must not take one and leave the other out.
        let space = space_within(body, 8);
        assert_eq!(
            space.labels(),
            [
                "CLICK 1",
                "CLICK 2",
                "SCROLL_UP",
                "SCROLL_DOWN",
                "WAIT",
                "DONE",
                "BLOCKED"
            ]
        );
        assert_eq!(space.truncated(), [3]);
    }
}
