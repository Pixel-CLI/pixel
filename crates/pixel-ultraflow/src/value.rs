// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Field values: what to type, and where the answer came from.
//!
//! `pixel classify` is a decision engine, not a text generator, so a value
//! is never invented: it is *chosen* among options the caller supplied (a
//! declared `--var`) or options the goal itself contains (a date, an
//! address, a proper noun). When nothing fits, the honest answer is to
//! stop and name the field rather than to type a plausible string.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::action::MAX_LABELS;
use super::decide::{Decider, Decision, argmax_index};
use super::elements::Element;

/// The label standing for "no offered value belongs in this field".
pub const NONE_LABEL: &str = "NONE";
/// Most goal-derived candidates offered in one value decision. The label
/// ceiling leaves room for the declared vars beside them.
const MAX_CANDIDATES: usize = 200;
/// Longest goal-derived candidate. A longer string is prose, not a field
/// value, and a huge option would eat the model's option budget.
const MAX_CANDIDATE_CHARS: usize = 120;

/// A value the caller declared up front (`--var name=value`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Var {
    pub name: String,
    pub value: String,
    /// What the value is for — the option's meaning to the engine.
    pub description: String,
}

impl Var {
    /// A var whose description is its own name.
    pub fn new(name: &str, value: &str) -> Var {
        Var {
            name: name.to_string(),
            value: value.to_string(),
            description: name.to_string(),
        }
    }

    fn label(&self) -> String {
        format!("VAR {}", self.name)
    }

    fn criterion(&self) -> String {
        format!("the declared {} \"{}\"", self.description, self.value)
    }
}

/// Where a recorded value came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSource {
    /// A string taken from the goal: recorded as the step's `value`.
    Literal,
    /// A declared variable: recorded as the step's `value_var`, so the
    /// flow re-reads it from the caller on every replay.
    Var(String),
}

/// A value the engine named, and what it was chosen from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolved {
    pub value: String,
    pub source: ValueSource,
    pub label: String,
    pub probability: f64,
    pub model: String,
}

/// The outcome of asking what a field needs.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueChoice {
    Value(Resolved),
    /// Nothing could be chosen; `detail` says why, naming the field so the
    /// caller can supply it.
    Undetermined {
        detail: String,
    },
}

/// Ask which value the field needs, offering the declared variables — or,
/// when none was declared, the strings the goal itself contains.
///
/// A field with nothing to choose from is never sent to the engine: an
/// engine failure and an empty option set are different answers.
pub fn choose(
    decider: &mut dyn Decider,
    goal: &str,
    field: &Element,
    vars: &[Var],
    budget: usize,
) -> Result<ValueChoice, String> {
    // `NONE` needs one of the engine's options, and the declared variables
    // are the caller's own words: they are offered whole or refused, never
    // quietly halved.
    let room = budget.clamp(2, MAX_LABELS) - 1;
    if vars.len() > room {
        return Err(format!(
            "{} variables were declared but the engine accepts {budget} options per question",
            vars.len()
        ));
    }
    let mut labels = Vec::new();
    let mut criteria = BTreeMap::new();
    for var in vars {
        labels.push(var.label());
        criteria.insert(var.label(), var.criterion());
    }
    // A declared variable is the caller saying where values come from. The
    // goal's own words are offered only when nothing was declared: otherwise
    // every capitalized word in a sentence ("Search", "Find") competes with
    // the value the caller chose, and a small model picks the wrong one.
    let offered = if vars.is_empty() {
        candidates(goal)
    } else {
        Vec::new()
    };
    for candidate in offered {
        if labels.len() >= room {
            break;
        }
        let label = format!("TEXT {candidate}");
        criteria.insert(
            label.clone(),
            "a literal string written in the goal".to_string(),
        );
        labels.push(label);
    }
    if labels.is_empty() {
        return Ok(ValueChoice::Undetermined {
            detail: format!(
                "no value is available for the field \"{}\" — declare it with a variable",
                field.name
            ),
        });
    }
    let none = NONE_LABEL.to_string();
    criteria.insert(
        none.clone(),
        "none of the offered values belongs in this field".to_string(),
    );
    labels.push(none);

    let request = Decision {
        text: format!("FIELD: {}\nGOAL: {goal}", field.describe()),
        context: VALUE_RULES.to_string(),
        labels: labels.clone(),
        criteria,
    };
    let distribution = decider.decide(&request)?;
    let Some(index) = argmax_index(&distribution.probabilities, &labels) else {
        return Err(format!(
            "the decision engine gave no probability to any of the {} values offered for \"{}\"",
            labels.len(),
            field.name
        ));
    };
    let label = &labels[index];
    if label == NONE_LABEL {
        return Ok(ValueChoice::Undetermined {
            detail: format!(
                "no offered value fits the field \"{}\" — declare it with a variable",
                field.name
            ),
        });
    }
    let probability = distribution.probabilities[label];
    if let Some(name) = label.strip_prefix("VAR ") {
        let var = vars
            .iter()
            .find(|var| var.name == name)
            .ok_or_else(|| format!("the engine chose the undeclared variable {name:?}"))?;
        return Ok(ValueChoice::Value(Resolved {
            value: var.value.clone(),
            source: ValueSource::Var(var.name.clone()),
            label: label.clone(),
            probability,
            model: distribution.model,
        }));
    }
    let value = label
        .strip_prefix("TEXT ")
        .ok_or_else(|| format!("unrecognized value label {label:?}"))?;
    Ok(ValueChoice::Value(Resolved {
        value: value.to_string(),
        source: ValueSource::Literal,
        label: label.clone(),
        probability,
        model: distribution.model,
    }))
}

/// The shared framing of every value question.
const VALUE_RULES: &str = "Choose the value one form field needs to advance the goal.
Every option is a value that could be entered; the field and the goal are the evidence.
Prefer a declared variable when it names what the field asks for.
Choose NONE when none of the options belongs in this field.
Never invent a value that is not among the options: NONE is the honest answer.";

/// The strings a goal itself contains, in order of appearance: quoted
/// spans first, then dates, address-like, URL-like and capitalized tokens.
///
/// A quoted span is one candidate and is not scanned again, so
/// `from 'Zurich, CH'` offers the city once, not twice.
pub fn candidates(goal: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |candidate: &str| {
        if candidate.is_empty() || candidate.chars().count() > MAX_CANDIDATE_CHARS {
            return;
        }
        if out.iter().any(|seen| seen == candidate) {
            return;
        }
        out.push(candidate.to_string());
    };
    let quoted = quoted_spans(goal);
    for (_, content) in &quoted {
        push(content);
    }
    for token in mask(goal, &quoted).split_whitespace() {
        let token = token.trim_matches(is_edge_punctuation);
        if is_date(token)
            || token.contains('@')
            || token.starts_with("http")
            || is_capitalized(token)
        {
            push(token);
        }
    }
    out.truncate(MAX_CANDIDATES);
    out
}

/// Punctuation that can open or close a token without belonging to it.
///
/// `-` and `.` are deliberately not kept at the edges: `(London).` is a city
/// and a full stop, while `one-way` and `example.com` keep theirs because
/// only the ends of a token are trimmed.
fn is_edge_punctuation(c: char) -> bool {
    !(c.is_alphanumeric() || matches!(c, '@' | '_' | '/' | ':'))
}

/// The quoted literals of `text`: each one's byte range, quotes included,
/// with the text it holds. Ordered by position, so a goal that mixes quote
/// styles still reads in the order it was written.
fn quoted_spans(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let mut out = Vec::new();
    for quote in ['\'', '"'] {
        let mut rest = text;
        let mut consumed = 0usize;
        while let Some((head, after_open)) = rest.split_once(quote) {
            let Some((content, tail)) = after_open.split_once(quote) else {
                break;
            };
            // The quote sits at `head.len()` in `rest`, and both quotes this
            // scans for are one byte, so a span is its content plus the two
            // quotes around it.
            let start = consumed + head.len();
            let end = start + content.len() + 2;
            out.push((start..end, content.to_string()));
            consumed = start + content.len() + 2;
            rest = tail;
        }
    }
    out.sort_by_key(|(range, _)| range.start);
    out
}

/// `text` with every quoted span blanked out, the same length so token
/// scanning stays aligned with the original.
fn mask(text: &str, spans: &[(std::ops::Range<usize>, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for (range, _) in spans {
        // Overlapping spans keep the first: the second is inside it. A span
        // that *starts* where the last one ended is not inside it — its own
        // bytes still have to leave the token scan.
        if range.start < cursor {
            continue;
        }
        out.push_str(&text[cursor..range.start]);
        out.extend(std::iter::repeat_n(' ', range.len()));
        cursor = range.end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Whether `token` is a `YYYY-MM-DD` date.
fn is_date(token: &str) -> bool {
    let bytes = token.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    [0, 1, 2, 3, 5, 6, 8, 9]
        .iter()
        .all(|i| bytes[*i].is_ascii_digit())
}

/// Whether `token` reads as a proper noun: capitalized, not a lone letter,
/// and not an acronym in the middle of a word.
fn is_capitalized(token: &str) -> bool {
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_uppercase()
        && token.chars().count() > 1
        && chars.all(|c| c.is_lowercase() || c == '-' || c == '\'')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elements::parse_snapshot;
    use crate::testutil::ScriptedDecider;

    fn field(name: &str) -> Element {
        let (elements, _) = parse_snapshot(&format!("- textbox \"{name}\" [ref=e1]"));
        elements.into_iter().next().unwrap()
    }

    #[test]
    fn candidates_are_the_strings_a_goal_actually_contains() {
        let goal = "Find one-way flights from 'Zurich' to London on 2026-09-20 for \
                    bob@example.com, see https://example.com/x, not an ACRONYM or I";
        assert_eq!(
            candidates(goal),
            [
                "Zurich",
                "Find",
                "London",
                "2026-09-20",
                "bob@example.com",
                "https://example.com/x"
            ],
            "quoted, address, date and proper nouns, in order, deduplicated"
        );
        // A quoted span is one candidate: the words inside it are not
        // scanned again.
        assert_eq!(
            candidates("from 'Zurich, CH' and (London)."),
            ["Zurich, CH", "London"]
        );
    }

    #[test]
    fn a_candidate_is_neither_prose_nor_an_empty_or_duplicated_string() {
        let long = "Z".repeat(MAX_CANDIDATE_CHARS + 1);
        assert_eq!(
            candidates(&format!("go to {long} and to Zz then Zz")),
            ["Zz"]
        );
        assert!(candidates("lowercase only prose here").is_empty());
        assert!(candidates("").is_empty());
        // An all-uppercase word is not a proper noun, and a bare token does
        // not swallow a sentence: `I` is one letter.
        assert!(candidates("go to the ACRONYM and I").is_empty());
        // An empty quoted span contributes nothing, and a span at the exact
        // character cap is kept while one past it is not.
        assert_eq!(candidates("from '' to Zz"), ["Zz"]);
        let exact = "Z".repeat(MAX_CANDIDATE_CHARS);
        assert_eq!(
            candidates(&format!("'{exact}' and Zz")),
            [exact.as_str(), "Zz"]
        );
        let long = "Y".repeat(MAX_CANDIDATE_CHARS + 1);
        assert_eq!(candidates(&format!("'{long}' and Zz")), ["Zz"]);
    }

    #[test]
    fn a_declared_variable_is_offered_with_its_value_and_wins_when_it_fits() {
        let mut decider = ScriptedDecider::always("VAR where_from");
        let vars = [Var::new("where_from", "Zurich")];
        let choice = choose(
            &mut decider,
            "Fly to London",
            &field("Where from?"),
            &vars,
            MAX_LABELS,
        )
        .unwrap();
        let ValueChoice::Value(resolved) = choice else {
            panic!("{choice:?}");
        };
        assert_eq!(resolved.value, "Zurich");
        assert_eq!(resolved.source, ValueSource::Var("where_from".to_string()));
        assert_eq!(resolved.model, "scripted");
        // The question carries the field, the goal, the var and NONE.
        let asked = decider.asked(0);
        assert!(asked.text.contains("FIELD: textbox \"Where from?\""));
        assert!(asked.text.contains("GOAL: Fly to London"));
        assert_eq!(
            asked.criteria["VAR where_from"],
            "the declared where_from \"Zurich\""
        );
        assert_eq!(
            asked.criteria[NONE_LABEL],
            "none of the offered values belongs in this field"
        );
        assert!(asked.context.contains("Never invent a value"));
        // A declared variable is the whole option set: the goal's own words
        // are not offered beside it.
        assert_eq!(asked.labels, ["VAR where_from", NONE_LABEL]);
    }

    /// The mask's own contract, asserted directly: a span that starts where
    /// the last one ended is blanked too (adjacent, not overlapping), the
    /// prose survives, and the length never moves — which is what keeps the
    /// token scan aligned with the original text.
    #[test]
    fn the_mask_blanks_every_quoted_span_and_keeps_the_texts_length() {
        // Two spans with prose between them.
        let gapped = "'ab' x 'cd' and Zz";
        let masked = mask(gapped, &quoted_spans(gapped));
        assert_eq!(
            masked.len(),
            gapped.len(),
            "aligned with the original: {masked:?}"
        );
        assert!(masked.contains(" x "), "the prose survives: {masked:?}");
        assert!(
            !masked.contains('\''),
            "every quoted byte is blanked: {masked:?}"
        );
        // Two spans sharing an edge are both blanked too: adjacent is not
        // overlapping, and the prose's own spacing survives untouched.
        let adjacent = "'ab''cd' and Zz";
        let masked = mask(adjacent, &quoted_spans(adjacent));
        assert_eq!(masked.len(), adjacent.len(), "{masked:?}");
        assert!(!masked.contains('\''), "{masked:?}");
        assert!(masked.ends_with(" and Zz"), "{masked:?}");
        assert!(masked.starts_with("    "), "{masked:?}");
    }

    /// Two quoted spans that share an edge are two candidates, not one: the
    /// overlap guard must not swallow the span that starts where the last
    /// one ended, and the mask must keep its length aligned with the text
    /// (which is what keeps the token scan from re-reading a quoted span).
    #[test]
    fn adjacent_quoted_spans_are_two_candidates() {
        // `(0..4)` and `(4..8)` touch; neither contains the other.
        assert_eq!(candidates("'ab''cd'"), ["ab", "cd"]);
        // The same shape beside prose, so the masked scan still sees what is
        // outside the quotes.
        assert_eq!(candidates("'ab''cd' and Zz"), ["ab", "cd", "Zz"]);
    }

    /// A token of the right shape is a date only when every field is one:
    /// the separators are checked one by one, so each of them can reject.
    #[test]
    fn is_date_rejects_a_token_that_gets_any_separator_wrong() {
        assert!(is_date("2026-09-20"));
        assert!(!is_date("2026/09/20"), "wrong separator one");
        assert!(!is_date("2026-09x20"), "wrong separator two");
        assert!(!is_date("20260920x"), "wrong length");
    }

    /// A proper noun may carry an inner apostrophe; a word that is
    /// capitalized in any other way (an inner capital, an acronym) is not
    /// the shape a goal's own values take.
    #[test]
    fn is_capitalized_keeps_an_inner_apostrophe_but_not_an_inner_capital() {
        assert!(is_capitalized("Zurich"));
        assert!(is_capitalized("What's"), "What's");
        assert!(!is_capitalized("San-Francisco"), "an inner capital");
        assert!(!is_capitalized("ACRONYM"), "an acronym");
        assert!(!is_capitalized("lowercase"));
    }

    /// A declared variable replaces the goal's own words as the option set,
    /// the engine's budget bounds the rest, and a declaration the engine
    /// cannot fit is refused rather than halved.
    /// The option budget bounds the declared variables at the boundary too:
    /// exactly `room` variables are offered whole, one more is refused.
    /// The ranges the scanner reports, read back directly: quote position,
    /// the two quote bytes, and the order sorted by position — so a mask
    /// blanks exactly what the goal quoted, garbled input order included.
    #[test]
    fn quoted_spans_reports_the_exact_ranges_in_position_order() {
        let ranges: Vec<(std::ops::Range<usize>, String)> =
            quoted_spans("go \'a\' then \"b\" to Zz");
        assert_eq!(
            ranges,
            [(3usize..6, "a".to_string()), (12usize..15, "b".to_string()),]
        );
        // Mixed quote styles come back in the order the text wrote them.
        let ranges = quoted_spans("x \"m\" and \'n\' y");
        assert_eq!(ranges[0].0, 2..5);
        assert_eq!(ranges[1].0, 10..13);
        // Two spans of ONE quote style advance a running cursor between
        // them, so the second offset proves the cursor stepped exactly the
        // content plus its two quotes (a different length than two, where
        // doubling would land somewhere else).
        let ranges = quoted_spans("\'a\' and \'bcd\'");
        assert_eq!(
            ranges,
            [
                (0usize..3, "a".to_string()),
                (8usize..13, "bcd".to_string())
            ]
        );
    }

    #[test]
    fn the_variable_count_is_accepted_up_to_the_room() {
        let two = [Var::new("a", "1"), Var::new("b", "2")];
        let mut decider = ScriptedDecider::always("VAR a");
        // Budget 3 leaves room for two values beside NONE.
        choose(&mut decider, "goal", &field("Where to?"), &two, 3).unwrap();
        assert_eq!(decider.labels_of(0), ["VAR a", "VAR b", NONE_LABEL]);
        let mut decider = ScriptedDecider::always("VAR a");
        assert!(choose(&mut decider, "goal", &field("Where to?"), &two, 2).is_err());
    }

    #[test]
    fn a_declared_variable_is_the_whole_option_set() {
        let mut decider = ScriptedDecider::always("VAR query");
        let vars = [Var::new("query", "Zurich")];
        choose(
            &mut decider,
            "Fly to London",
            &field("Where to?"),
            &vars,
            MAX_LABELS,
        )
        .unwrap();
        assert_eq!(decider.labels_of(0), ["VAR query", NONE_LABEL]);

        // The engine's budget bounds the goal-derived candidates.
        let mut decider = ScriptedDecider::always("TEXT Fly");
        choose(&mut decider, "Fly to London", &field("Where to?"), &[], 2).unwrap();
        assert_eq!(decider.labels_of(0), ["TEXT Fly", NONE_LABEL]);

        // A caller's declaration is offered whole or refused, never halved.
        let mut decider = ScriptedDecider::always("VAR a");
        let two = [Var::new("a", "1"), Var::new("b", "2")];
        let err = choose(&mut decider, "goal", &field("Where to?"), &two, 2).unwrap_err();
        assert_eq!(
            err,
            "2 variables were declared but the engine accepts 2 options per question"
        );
        assert!(decider.asked.is_empty());
    }

    #[test]
    fn a_goal_string_is_recorded_as_a_literal_value() {
        let mut decider = ScriptedDecider::always("TEXT London");
        let choice = choose(
            &mut decider,
            "fly to London",
            &field("Where to?"),
            &[],
            MAX_LABELS,
        )
        .unwrap();
        let ValueChoice::Value(resolved) = choice else {
            panic!("{choice:?}");
        };
        assert_eq!(resolved.value, "London");
        assert_eq!(resolved.source, ValueSource::Literal);
        // Declaring nothing still offers the goal's own strings and NONE.
        let labels = decider.labels_of(0);
        assert_eq!(labels, ["TEXT London", NONE_LABEL]);
        // Every capitalized word is offered, so a goal of several still
        // gives the engine a choice rather than one forced answer.
        let mut decider = ScriptedDecider::always("TEXT London");
        choose(
            &mut decider,
            "Fly to London",
            &field("Where to?"),
            &[],
            MAX_LABELS,
        )
        .unwrap();
        assert_eq!(
            decider.labels_of(0),
            ["TEXT Fly", "TEXT London", NONE_LABEL]
        );
    }

    /// NONE and an empty option set are different answers, and both name
    /// the field: neither may be turned into a typed value.
    #[test]
    fn no_value_is_ever_invented() {
        let mut decider = ScriptedDecider::always(NONE_LABEL);
        let choice = choose(
            &mut decider,
            "Fly to London",
            &field("Where to?"),
            &[],
            MAX_LABELS,
        )
        .unwrap();
        let ValueChoice::Undetermined { detail } = choice else {
            panic!("{choice:?}");
        };
        assert!(detail.contains("\"Where to?\""), "{detail}");
        assert!(detail.contains("declare it with a variable"), "{detail}");

        // Nothing to offer: the engine is not even asked.
        let mut decider = ScriptedDecider::always("TEXT x");
        let choice = choose(
            &mut decider,
            "all lowercase prose",
            &field("Code"),
            &[],
            MAX_LABELS,
        )
        .unwrap();
        assert!(matches!(choice, ValueChoice::Undetermined { .. }));
        assert!(
            decider.asked.is_empty(),
            "an empty option set is not a question"
        );
    }

    #[test]
    fn an_engine_failure_and_an_unavailable_option_are_reported_apart() {
        let mut decider = ScriptedDecider::new(vec![]);
        let err = choose(
            &mut decider,
            "Fly to London",
            &field("Where to?"),
            &[],
            MAX_LABELS,
        )
        .unwrap_err();
        assert_eq!(err, "scripted decider ran out of answers");

        // An answer that names none of the offered options is an engine
        // contract violation, not a silent stop.
        let mut decider = ScriptedDecider::always("TELEPORT 7");
        let err = choose(
            &mut decider,
            "Fly to London",
            &field("Where to?"),
            &[],
            MAX_LABELS,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "the decision engine gave no probability to any of the 3 values offered for \"Where to?\""
        );
    }

    #[test]
    fn the_option_ceiling_bounds_how_many_candidates_are_offered() {
        let goal = (0..MAX_CANDIDATES + 20)
            .map(|i| format!("user{i}@example.com"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut decider = ScriptedDecider::always("TEXT user0@example.com");
        choose(&mut decider, &goal, &field("Code"), &[], MAX_LABELS).unwrap();
        // 200 candidates form the option set; NONE is offered beside them.
        assert_eq!(decider.labels_of(0).len(), MAX_CANDIDATES + 1);
        assert_eq!(candidates(&goal).len(), MAX_CANDIDATES);
    }
}
