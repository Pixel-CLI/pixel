// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The values of `pixel classify`: a harness's
//! question with its labels and their rules, and the answer. The adapter
//! reads and writes them as JSON; the rule path in `pixel-minimap` decides.

use std::fmt;

/// Why the area rules are refused.
const AREAS: &str = "areas come from the map's clustering, which is not built yet \
                     (design/map-format.md, \"Global map\")";

/// The label of an answer the rules did not decide; no label may take it.
pub const NONE: &str = "none";

/// One request: a bounded question, its closed list of labels, the prompt.
#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    /// What is asked; context for a decision model, read by no rule.
    pub question: String,
    pub labels: Vec<Label>,
    pub prompt: String,
}

/// One label of a question.
#[derive(Clone, Debug, PartialEq)]
pub struct Label {
    pub id: String,
    pub rules: Vec<Rule>,
    /// What the label means, for a decision model; read by no rule.
    pub text: Option<String>,
}

/// A deterministic predicate over the prompt and the graph: the closed
/// list of [`Rule::KINDS`]. None reads a file's contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    /// A regular expression found in the typed text.
    Pattern(String),
    /// The typed text has at most this many words.
    MaxWords(u64),
    /// The typed text has at least this many words.
    MinWords(u64),
    /// The minimap matched at least this many of the prompt's names
    /// (step 2: symbols and files named).
    MinimapNamedAtLeast(u64),
    MinimapNamedAtMost(u64),
    /// The minimap picks at least this many files changed in the working
    /// tree (step 3), counted before its cut.
    MinimapChangedAtLeast(u64),
    MinimapChangedAtMost(u64),
}

/// The value a request gives a rule.
#[derive(Clone, Copy, Debug)]
pub enum RuleValue<'a> {
    Text(&'a str),
    Count(u64),
    /// Anything else: a negative or fractional number, a list, an object.
    Other,
}

impl Rule {
    /// Every rule kind, as a request names it.
    pub const KINDS: [&str; 7] = [
        "pattern",
        "max_words",
        "min_words",
        "minimap_named_at_least",
        "minimap_named_at_most",
        "minimap_changed_at_least",
        "minimap_changed_at_most",
    ];

    /// Kinds the design names that cannot be read yet, and why.
    pub const REFUSED: [(&str, &str); 2] = [
        ("minimap_areas_at_least", AREAS),
        ("minimap_areas_at_most", AREAS),
    ];

    /// The rule a request names `kind` with `value`; an error naming the
    /// kinds when `kind` is not one, or the value it takes when `value`
    /// is not that.
    pub fn new(kind: &str, value: RuleValue<'_>) -> Result<Rule, ClassifyError> {
        let count = |make: fn(u64) -> Rule| match value {
            RuleValue::Count(n) => Ok(make(n)),
            _ => Err(ClassifyError(format!(
                "`{kind}` takes a whole number of 0 or more"
            ))),
        };
        match kind {
            "pattern" => match value {
                RuleValue::Text(pattern) => Ok(Rule::Pattern(pattern.to_owned())),
                _ => Err(ClassifyError(
                    "`pattern` takes a regular expression, as a string".to_owned(),
                )),
            },
            "max_words" => count(Rule::MaxWords),
            "min_words" => count(Rule::MinWords),
            "minimap_named_at_least" => count(Rule::MinimapNamedAtLeast),
            "minimap_named_at_most" => count(Rule::MinimapNamedAtMost),
            "minimap_changed_at_least" => count(Rule::MinimapChangedAtLeast),
            "minimap_changed_at_most" => count(Rule::MinimapChangedAtMost),
            _ => match Rule::REFUSED.iter().find(|(refused, _)| *refused == kind) {
                Some((_, why)) => Err(ClassifyError(format!("`{kind}` is not read yet: {why}"))),
                None => Err(ClassifyError(format!(
                    "unknown rule kind `{kind}`; the kinds are {}",
                    Rule::KINDS.join(", ")
                ))),
            },
        }
    }

    /// Whether the rule reads the minimap, hence the graph.
    pub fn reads_graph(&self) -> bool {
        !matches!(
            self,
            Rule::Pattern(_) | Rule::MaxWords(_) | Rule::MinWords(_)
        )
    }
}

/// Which path answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answered {
    Rules,
    /// The decision model (`pixel-clef`, M4); never in this build.
    Model,
}

impl Answered {
    pub fn as_str(self) -> &'static str {
        match self {
            Answered::Rules => "rules",
            Answered::Model => "model",
        }
    }
}

/// The answer to a question.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    /// One of the labels, or [`NONE`].
    pub label: String,
    /// A probability per label, in the request's order, then [`NONE`];
    /// they sum to 1.
    pub probabilities: Vec<(String, f64)>,
    /// In [0, 1].
    pub confidence: f64,
    pub path: Answered,
}

/// Why a request cannot be answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassifyError(pub String);

impl fmt::Display for ClassifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ClassifyError {}
