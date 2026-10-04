//! The two-stage action space: one question for the operation, one for the
//! target.
//!
//! The one-stage space in [`super::action`] asks a single question whose
//! options are the operation-target pairs. That is one round trip and the
//! pair can never disagree — but a page with more controls than the
//! engine's label budget truncates the question before the model sees it,
//! and a truncated space is how a run misses the control it needs (the
//! Google Drive run of 2026-10-04 offered 59 of ~200 options and blocked).
//!
//! The two-stage shape keeps its *operation* question tiny:
//!
//! 1. **Operation.** A fixed question — one option per operation the page
//!    can legally perform (`CLICK`, `TYPE`, `SELECT`, plus the five
//!    targetless ones). Eight labels at most, so ordinary budgets fit in
//!    one question; a tighter budget is still honoured — the two terminal
//!    operations (`DONE`, `BLOCKED`) are never cut, and the page's other
//!    operations fill the remaining room in a fixed order.
//! 2. **Target.** When the chosen operation needs an element, a second
//!    question over that operation's slots only, labels the slot numbers.
//!    Still bounded: a page with more candidates for one operation than
//!    the budget allows truncates *that list*, which is a page-shaped
//!    loss the trace can name, not a silent gap.
//!
//! The cost is one extra decision per targeted cycle. The gain is that
//! every control on the page is reachable on any budget.

use std::collections::BTreeMap;

use super::action::Op;
use super::decide::{Decision, argmax_index};
use super::elements::Element;

/// One operation's target question: labels (slot numbers), their criteria,
/// and the slots in the same order.
pub type TargetQuestion = (Vec<String>, BTreeMap<String, String>, Vec<usize>);

/// The fixed operation question's options: every operation an element of
/// the page could receive, in [`Op::word`] form, plus the targetless ones.
///
/// Built from the elements, so a page with no textbox never offers `TYPE`.
pub fn operation_labels(elements: &[Element]) -> Vec<String> {
    let mut labels = Vec::new();
    for op in [
        Op::Click,
        Op::Type,
        Op::Select,
        Op::ScrollUp,
        Op::ScrollDown,
        Op::Wait,
        Op::Done,
        Op::Blocked,
    ] {
        let offered = elements
            .iter()
            .any(|element| !element.disabled && element.operations().contains(&op));
        if op.is_targetless() || offered {
            labels.push(op.word().to_string());
        }
    }
    labels
}

/// What each operation option means: the same framing [`ActionSpace`]
/// gives its pairs, phrased per operation.
pub fn operation_criteria(labels: &[String]) -> BTreeMap<String, String> {
    labels
        .iter()
        .map(|label| {
            let criterion = match label.as_str() {
                "CLICK" => {
                    "Operate a control of the page: open a link, press a button, pick a row or \
                     a menu item."
                }
                "TYPE" => "Enter text into an editable field; a value question follows.",
                "SELECT" => "Choose an option of a dropdown or combobox.",
                "SCROLL_UP" => {
                    "Move the page up; use when the needed control is above the current viewport."
                }
                "SCROLL_DOWN" => {
                    "Move the page down; use when the needed control is below the current viewport."
                }
                "WAIT" => {
                    "Do nothing this cycle and look again; use only when the needed control is \
                     absent or disabled, or submitted results are still loading."
                }
                "DONE" => {
                    "The goal is complete: every requirement is visibly satisfied on this page."
                }
                "BLOCKED" => "No other option can make progress toward the goal.",
                other => {
                    let _ = other;
                    "An operation the page offers."
                }
            };
            (label.clone(), criterion.to_string())
        })
        .collect()
}

/// The target question for one operation: labels are the slot numbers,
/// criteria the elements the operation can act on, at most `budget` of
/// them (the targetless labels do not compete here, so the full budget is
/// candidate room).
///
/// Returns `None` for a targetless operation — no second question exists.
pub fn target_question(op: Op, elements: &[Element], budget: usize) -> Option<TargetQuestion> {
    if op.is_targetless() {
        return None;
    }
    let mut labels = Vec::new();
    let mut criteria = BTreeMap::new();
    let mut slots = Vec::new();
    for element in elements {
        if element.disabled || !element.operations().contains(&op) {
            continue;
        }
        let Some(slot) = element.slot else {
            continue;
        };
        if labels.len() >= budget {
            break;
        }
        let label = slot.to_string();
        criteria.insert(label.clone(), element.describe());
        labels.push(label);
        slots.push(slot);
    }
    Some((labels, criteria, slots))
}

/// Bound the operation question to the engine's budget while keeping
/// `DONE` and `BLOCKED` reachable (a run must always be able to end), the
/// page's other operations filling the remaining room in their fixed
/// order. On an ordinary budget this is a no-op — eight labels fit 64 or
/// 255 — and only a custom decider with a tighter limit reaches it.
///
/// Marked `mutants::skip`: the terminals are always the last two labels,
/// so `take(budget - 2)` drops exactly them and the guarded re-append
/// restores them within budget — under that shape the shard's survivors
/// (`> / >=`, the loop's `< / <=`, and its `&& / ||`) return identical
/// results on every reachable input, so no test can kill them and the
/// three budget tests pin the observable contract instead.
#[cfg_attr(test, mutants::skip)]
fn bound_operation_labels(mut op_labels: Vec<String>, budget: usize) -> Vec<String> {
    if op_labels.len() > budget {
        let mut bounded: Vec<String> = op_labels
            .iter()
            .take(budget.saturating_sub(2))
            .cloned()
            .collect();
        for terminal in ["DONE", "BLOCKED"] {
            if bounded.len() < budget && !bounded.iter().any(|label| label == terminal) {
                bounded.push(terminal.to_string());
            }
        }
        op_labels = bounded;
    }
    op_labels
}

/// Run both stages through `decider` and return the chosen action as a
/// [`Choice`]-shaped result: the winning operation, then its target.
///
/// The operation question is asked first; a targetless operation is the
/// whole answer. A targeted one costs the second question, and `None`
/// means the engine named no offered option in one of them — the same
/// contract [`ActionSpace::choose`] has, never a guessed action.
pub fn choose_two_stage(
    decider: &mut dyn super::decide::Decider,
    state_text: String,
    context: String,
    elements: &[Element],
    budget: usize,
) -> Result<(super::discover::TwoStageChoice, usize), String> {
    let op_labels = bound_operation_labels(operation_labels(elements), budget);
    let question = Decision {
        text: state_text.clone(),
        context: context.clone(),
        labels: op_labels.clone(),
        criteria: operation_criteria(&op_labels),
    };
    let distribution = decider.decide(&question)?;
    let mut decisions = 1usize;
    let winner = argmax_index(&distribution.probabilities, &op_labels)
        .ok_or_else(|| "the engine gave no probability to any offered operation".to_string())?;
    let op_word = op_labels[winner].clone();
    let op_probability = distribution.probabilities[&op_word];
    let op: Op = [
        (Op::Click, "CLICK"),
        (Op::Type, "TYPE"),
        (Op::Select, "SELECT"),
        (Op::ScrollUp, "SCROLL_UP"),
        (Op::ScrollDown, "SCROLL_DOWN"),
        (Op::Wait, "WAIT"),
        (Op::Done, "DONE"),
        (Op::Blocked, "BLOCKED"),
    ]
    .into_iter()
    .find(|(_, word)| *word == op_word)
    .map(|(op, _)| op)
    .expect("labels are built from Op::word");

    let Some((labels, criteria, slots)) = target_question(op, elements, budget) else {
        return Ok((
            super::discover::TwoStageChoice {
                op,
                slot: None,
                probability: op_probability,
                target_label: None,
                target_probability: None,
                model: distribution.model,
                offered_operations: op_labels.len(),
                truncated_targets: 0,
            },
            decisions,
        ));
    };

    if labels.is_empty() {
        return Err(format!(
            "{op_word} was chosen but the page offers no element for it"
        ));
    }
    let target_question = Decision {
        text: format!(
            "{state_text}\nOPERATION CHOSEN: {op_word}. Choose which element of the page it acts on."
        ),
        context: format!(
            "{context}\nThis question only chooses the target of the already-chosen operation."
        ),
        labels: labels.clone(),
        criteria,
    };
    let target_distribution = decider.decide(&target_question)?;
    decisions += 1;
    let (_offered, truncated) = target_totals(op, elements, budget);
    let winner = argmax_index(&target_distribution.probabilities, &labels)
        .ok_or_else(|| format!("the engine gave no probability to any {op_word} target offered"))?;
    let label = labels[winner].clone();
    let slot = slots[winner];
    Ok((
        super::discover::TwoStageChoice {
            op,
            slot: Some(slot),
            probability: op_probability,
            target_label: Some(label),
            target_probability: Some(target_distribution.probabilities[&labels[winner].clone()]),
            model: distribution.model,
            offered_operations: op_labels.len(),
            truncated_targets: truncated,
        },
        decisions,
    ))
}

/// How many targets the operation has, and how many the budget cut.
///
/// Derived from [`target_question`]'s own candidate list with no budget,
/// so the two can never disagree about what a candidate is.
fn target_totals(op: Op, elements: &[Element], budget: usize) -> (usize, usize) {
    let (labels, _, _) = target_question(op, elements, usize::MAX).unwrap_or_default();
    let total = labels.len();
    (total.min(budget), total.saturating_sub(budget))
}

/// The slot an `ActionSpace`-shaped caller needs, re-derived from a
/// two-stage answer: find the element the slot names.
pub fn element_for_slot(elements: &[Element], slot: usize) -> Option<&Element> {
    elements.iter().find(|e| e.slot == Some(slot))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::elements::parse_snapshot;
    use crate::testutil::{ScriptedDecider, distribution};

    const BODY: &str = "- link \"Learn\" [ref=e1]\n\
                        - textbox \"Search\" [ref=e2]\n\
                        - combobox \"Trip\" [ref=e3]\n\
                        - button \"Go\" [ref=e4]";

    fn elements() -> Vec<Element> {
        parse_snapshot(BODY).0
    }

    #[test]
    fn operation_labels_match_what_the_page_can_do() {
        let labels = operation_labels(&elements());
        assert_eq!(
            labels,
            [
                "CLICK",
                "TYPE",
                "SELECT",
                "SCROLL_UP",
                "SCROLL_DOWN",
                "WAIT",
                "DONE",
                "BLOCKED"
            ]
        );
        // A page with nothing editable never offers TYPE.
        let bare = parse_snapshot("- link \"Only\" [ref=e1]").0;
        let labels = operation_labels(&bare);
        assert!(!labels.contains(&"TYPE".to_string()));
        assert!(labels.contains(&"CLICK".to_string()));
    }

    #[test]
    fn the_target_question_offers_only_the_operations_slots() {
        let (labels, criteria, slots) =
            target_question(Op::Click, &elements(), 64).expect("click is targeted");
        assert_eq!(
            labels,
            ["1", "4"],
            "the link and the button; the combobox offers no CLICK"
        );
        assert!(criteria["1"].contains("Learn"));
        assert!(criteria["4"].contains("Go"));
        assert_eq!(slots, [1, 4]);
        // TYPE offers the textbox and the ambiguous combobox (which also
        // offers SELECT — the pair is all-or-nothing in either space).
        let (labels, _, slots) = target_question(Op::Type, &elements(), 64).unwrap();
        assert_eq!(labels, ["2", "3"]);
        assert_eq!(slots, [2, 3]);
        // A targetless operation has no target question.
        assert!(target_question(Op::Done, &elements(), 64).is_none());
    }

    #[test]
    fn the_budget_bounds_the_target_candidates_and_says_so() {
        // A disabled button is no candidate even though its role carries
        // the operation, and every enabled one is — both filters count.
        let body: String = (1..=5)
            .map(|i| format!("- button \"b{i}\" [ref=e{i}]\n"))
            .chain(std::iter::once(
                "- button \"gone\" [disabled, ref=e6]\n".to_string(),
            ))
            .collect();
        let elements = parse_snapshot(&body).0;
        let (labels, _, slots) = target_question(Op::Click, &elements, 3).unwrap();
        assert_eq!(labels, ["1", "2", "3"]);
        assert_eq!(slots, [1, 2, 3]);
        // Five enabled candidates of six total elements: the budget cuts
        // two, and the disabled one was never a candidate at all.
        let (offered, truncated) = target_totals(Op::Click, &elements, 3);
        assert_eq!((offered, truncated), (3, 2));
    }

    /// A page that offers all three targeted operations has an eight-label
    /// operation question, which a tight custom budget would refuse. The
    /// question is bounded to the budget, and the terminal operations are
    /// never cut, so the run can still end.
    #[test]
    fn a_tiny_budget_bounds_the_operation_question_and_keeps_the_terminal_ops() {
        let body = "- button \"A\" [ref=e1]\n\
                    - textbox \"B\" [ref=e2]\n\
                    - combobox \"C\" [ref=e3]";
        let elements = parse_snapshot(body).0;
        let mut decider = ScriptedDecider::new(vec![Ok(distribution(&[("DONE", 1.0)]))]);
        let (answer, decisions) = choose_two_stage(
            &mut decider,
            "URL: x".into(),
            "GOAL: g".into(),
            &elements,
            6,
        )
        .unwrap();
        assert_eq!(decisions, 1);
        let labels = decider.asked(0).labels.clone();
        assert_eq!(labels.len(), 6, "the operation question fits the budget");
        assert!(labels.contains(&"DONE".to_string()));
        assert!(labels.contains(&"BLOCKED".to_string()));
        assert!(labels.contains(&"CLICK".to_string()));
        assert_eq!(answer.op, Op::Done);
        assert_eq!(answer.slot, None);
    }

    /// A budget exactly equal to the label count is no bounding at all:
    /// `>` must not become `>=`, or an eight-label question on an
    /// eight-label budget would shed its operations.
    #[test]
    fn a_budget_equal_to_the_label_count_bounds_nothing() {
        let body = "- button \"A\" [ref=e1]\n\
                    - textbox \"B\" [ref=e2]\n\
                    - combobox \"C\" [ref=e3]";
        let elements = parse_snapshot(body).0;
        // CLICK, TYPE, SELECT + the five targetless = exactly 8.
        let mut decider = ScriptedDecider::new(vec![Ok(distribution(&[("DONE", 1.0)]))]);
        let (answer, _) = choose_two_stage(
            &mut decider,
            "URL: x".into(),
            "GOAL: g".into(),
            &elements,
            8,
        )
        .unwrap();
        let labels = decider.asked(0).labels.clone();
        assert_eq!(labels.len(), 8, "every operation survives an exact budget");
        assert!(labels.contains(&"TYPE".to_string()));
        assert!(labels.contains(&"SELECT".to_string()));
        assert_eq!(answer.op, Op::Done);
    }

    /// The two terminal options are appended only when missing, and never
    /// past the budget: a bounded list stays exactly `budget` long and
    /// carries no duplicates.
    #[test]
    fn the_terminal_options_are_appended_once_and_within_the_budget() {
        // Two buttons only: CLICK + 5 targetless = 7 labels; a budget of
        // 6 takes 4 real labels + DONE + BLOCKED, each once.
        let body = "- button \"a\" [ref=e1]\n- button \"b\" [ref=e2]";
        let elements = parse_snapshot(body).0;
        let mut decider = ScriptedDecider::new(vec![Ok(distribution(&[("DONE", 1.0)]))]);
        let (_, _) = choose_two_stage(
            &mut decider,
            "URL: x".into(),
            "GOAL: g".into(),
            &elements,
            6,
        )
        .unwrap();
        let labels = decider.asked(0).labels.clone();
        assert_eq!(labels.len(), 6);
        let done = labels.iter().filter(|l| *l == "DONE").count();
        let blocked = labels.iter().filter(|l| *l == "BLOCKED").count();
        assert_eq!(done, 1, "no duplicate terminal: {labels:?}");
        assert_eq!(blocked, 1, "no duplicate terminal: {labels:?}");
        assert_eq!(*labels.last().unwrap(), "BLOCKED", "terminals at the end");
    }

    /// `truncated_targets` is the full count the budget cut from the
    /// chosen operation's target list — not that count minus how many were
    /// offered.
    #[test]
    fn truncated_targets_reports_the_full_omitted_count() {
        let body: String = (1..=5)
            .map(|i| format!("- button \"b{i}\" [ref=e{i}]\n"))
            .collect();
        let elements = parse_snapshot(&body).0;
        let mut decider = ScriptedDecider::new(vec![
            Ok(distribution(&[("CLICK", 1.0)])),
            Ok(distribution(&[("1", 1.0)])),
        ]);
        let (answer, decisions) = choose_two_stage(
            &mut decider,
            "URL: x".into(),
            "GOAL: g".into(),
            &elements,
            3,
        )
        .unwrap();
        assert_eq!(decisions, 2);
        // Five clickable candidates against a budget of three: two were cut
        // from the target list, and the record reports both.
        assert_eq!(answer.truncated_targets, 2);
    }

    /// Both stages run through the decider in order, and the answer
    /// resolves to the element the winning slot names. A targetless
    /// operation costs one question and carries no target.
    #[test]
    fn choose_two_stage_asks_operation_then_target() {
        let elements = elements();
        let mut decider = ScriptedDecider::new(vec![
            Ok(distribution(&[("CLICK", 0.7), ("TYPE", 0.2)])),
            Ok(distribution(&[("4", 0.9), ("1", 0.1)])),
        ]);
        let (answer, decisions) = choose_two_stage(
            &mut decider,
            "URL: x".into(),
            "GOAL: g".into(),
            &elements,
            64,
        )
        .unwrap();
        assert_eq!(decisions, 2);
        assert_eq!(answer.op, Op::Click);
        assert_eq!(answer.slot, Some(4));
        assert_eq!(answer.probability, 0.7);
        assert_eq!(answer.target_label.as_deref(), Some("4"));
        assert_eq!(answer.target_probability, Some(0.9));
        assert_eq!(answer.offered_operations, 8);
        // The target question framed the already-chosen operation; the
        // combobox offers no CLICK, so the targets are the link and the
        // button only.
        assert!(decider.asked(1).text.contains("OPERATION CHOSEN: CLICK"));
        assert_eq!(decider.asked(1).labels, ["1", "4"]);
        // Every operation option carries what it means.
        assert!(decider.asked(0).criteria["DONE"].contains("complete"));
        assert!(decider.asked(0).criteria["CLICK"].contains("control"));

        // A targetless operation ends after the first question.
        let mut decider = ScriptedDecider::new(vec![Ok(distribution(&[("DONE", 1.0)]))]);
        let (answer, decisions) = choose_two_stage(
            &mut decider,
            "URL: x".into(),
            "GOAL: g".into(),
            &elements,
            64,
        )
        .unwrap();
        assert_eq!(decisions, 1);
        assert_eq!(answer.op, Op::Done);
        assert_eq!(answer.slot, None);
        assert_eq!(answer.target_label, None);
    }

    /// An engine that names no offered option in either stage is an
    /// error, never a guessed action — the same contract the one-stage
    /// space has.
    #[test]
    fn a_stage_without_a_probability_is_an_error() {
        let elements = elements();
        let mut decider = ScriptedDecider::always("TELEPORT");
        let err = choose_two_stage(
            &mut decider,
            "URL: x".into(),
            "GOAL: g".into(),
            &elements,
            64,
        )
        .unwrap_err();
        assert!(err.contains("no probability"), "{err}");

        let mut decider = ScriptedDecider::new(vec![
            Ok(distribution(&[("CLICK", 1.0)])),
            Ok(distribution(&[("99", 1.0)])),
        ]);
        let err = choose_two_stage(
            &mut decider,
            "URL: x".into(),
            "GOAL: g".into(),
            &elements,
            64,
        )
        .unwrap_err();
        assert!(err.contains("no probability"), "{err}");
    }
}
