//! Test doubles for the two seams: the decision engine and the browser.
//!
//! Both answer from a queue, so every arm of the loop is reachable without
//! a model, a network, a browser, or a page-load wait — and the calls each
//! double received can be read back afterwards.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use pixel_flow::Browser;

use crate::action::MAX_LABELS;
use crate::decide::{Decider, Decision, Distribution};

/// A decider that answers from a queue and records every question.
pub struct ScriptedDecider {
    answers: VecDeque<Result<Distribution, String>>,
    pub asked: Vec<Decision>,
    budget: usize,
}

impl Default for ScriptedDecider {
    fn default() -> Self {
        ScriptedDecider {
            answers: VecDeque::new(),
            asked: Vec::new(),
            budget: MAX_LABELS,
        }
    }
}

impl ScriptedDecider {
    pub fn new(answers: Vec<Result<Distribution, String>>) -> ScriptedDecider {
        ScriptedDecider {
            answers: answers.into_iter().collect(),
            asked: Vec::new(),
            budget: MAX_LABELS,
        }
    }

    /// An engine with a tighter option budget than the schema ceiling.
    pub fn with_budget(mut self, budget: usize) -> ScriptedDecider {
        self.budget = budget;
        self
    }

    /// Answer every question with `label` at full probability.
    pub fn always(label: &str) -> ScriptedDecider {
        ScriptedDecider::new(std::iter::repeat_n(Ok(distribution(&[(label, 1.0)])), 1000).collect())
    }

    /// Answer with `first`, then `second`, then `fallback` forever: the
    /// shape of a run that acts once and then reports the goal complete.
    pub fn then(first: &str, second: &str, fallback: &str) -> ScriptedDecider {
        ScriptedDecider::new(vec![
            Ok(distribution(&[(first, 1.0)])),
            Ok(distribution(&[(second, 1.0)])),
            Ok(distribution(&[(fallback, 1.0)])),
        ])
    }

    pub fn labels_of(&self, index: usize) -> &[String] {
        &self.asked[index].labels
    }

    pub fn asked(&self, index: usize) -> &Decision {
        &self.asked[index]
    }
}

impl Decider for ScriptedDecider {
    fn option_budget(&self) -> usize {
        self.budget
    }

    fn decide(&mut self, request: &Decision) -> Result<Distribution, String> {
        self.asked.push(request.clone());
        self.answers
            .pop_front()
            .unwrap_or_else(|| Err("scripted decider ran out of answers".to_string()))
    }
}

/// A bare probability map, for the action space's `choose`.
pub fn weights(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
    pairs
        .iter()
        .map(|(label, p)| ((*label).to_string(), *p))
        .collect()
}

/// A distribution holding the named labels at the named probabilities.
pub fn distribution(pairs: &[(&str, f64)]) -> Distribution {
    Distribution {
        probabilities: pairs
            .iter()
            .map(|(label, p)| ((*label).to_string(), *p))
            .collect::<BTreeMap<_, _>>(),
        model: "scripted".to_string(),
    }
}

/// A browser that answers from a queue and records every call.
///
/// The call shapes are the ones the loop really issues: an observation is
/// `get url` then `snapshot -i`, and a step that targets an element takes
/// its own snapshot first.
#[derive(Default)]
pub struct ScriptedBrowser {
    pub answers: VecDeque<Result<String, String>>,
    pub calls: Vec<Vec<String>>,
    pub paused: Vec<Duration>,
}

impl ScriptedBrowser {
    /// Queue the `open` a discovery run makes before its first look, with
    /// the URL probe the executor reads before it and the two poll answers
    /// after it (the two-empty stop ends the poll at once).
    pub fn start(&mut self) {
        self.ok("about:blank");
        self.ok("");
        self.ok("");
        self.ok("");
    }

    /// Queue the two calls of one observation.
    pub fn observe(&mut self, url: &str, snapshot: &str) {
        self.ok(url);
        self.ok(snapshot);
    }

    /// Queue one successful command.
    pub fn ok(&mut self, answer: &str) {
        self.answers.push_back(Ok(answer.to_string()));
    }

    /// Queue one failing command.
    pub fn fail(&mut self, error: &str) {
        self.answers.push_back(Err(error.to_string()));
    }

    /// Queue one click as the executor issues it: the pre-click snapshot
    /// that resolves the ref, the URL probe, the click itself, and two
    /// poll answers of "" — the two-empty stop ends the poll at once, so
    /// the next queued answer is the loop's own next call.
    pub fn click(&mut self, snapshot: &str) {
        self.ok(snapshot);
        self.ok("");
        self.ok("");
        self.ok("");
        self.ok("");
    }

    pub fn calls(&self) -> Vec<Vec<&str>> {
        self.calls
            .iter()
            .map(|call| call.iter().map(String::as_str).collect())
            .collect()
    }
}

impl Browser for ScriptedBrowser {
    fn run(&mut self, args: &[&str]) -> Result<String, String> {
        self.calls
            .push(args.iter().map(ToString::to_string).collect());
        self.answers
            .pop_front()
            .unwrap_or_else(|| Ok(String::new()))
    }

    fn pause(&mut self, duration: Duration) {
        self.paused.push(duration);
    }
}
