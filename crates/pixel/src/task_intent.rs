// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The task-intent labels behind `pixel classify --task-intent`: the harness
//! classifies, the model never has to.
//!
//! The command asks the decision engine which kind of coding-agent task a
//! prompt is (bugfix, feature, …) and names the pixel commands that fit it,
//! from one [`INTENTS`] table. The verdict is a classifier claim, never a
//! repository fact; nothing in the default path calls it.

use std::time::Duration;

use crate::classify::Spec;

/// Whole-request cap of the warm task-intent call. A warm labeled call was
/// measured at 0.11 s and a cold one (model load) at 4.3 s: the cap keeps
/// the first and fails the second open.
pub(crate) const HOOK_CALL_TIMEOUT: Duration = Duration::from_millis(300);
/// The framing every intent label shares.
pub(crate) const INTENT_CONTEXT: &str = "What kind of coding-agent task is this prompt?";

/// One intent label: what it means to the model and the commands that fit it.
pub(crate) struct IntentKind {
    pub(crate) label: &'static str,
    pub(crate) criterion: &'static str,
    pub(crate) ops: &'static [&'static str],
}

/// Every intent label, its `--criterion` definition and the pixel commands to
/// start with, the one spelling `pixel classify --task-intent` uses.
pub(crate) const INTENTS: &[IntentKind] = &[
    IntentKind {
        label: "bugfix",
        criterion: "something that used to work or should work is broken; the prompt asks to find and fix the defect",
        ops: &[
            "pixel search-history \"<text>\"",
            "pixel impact \"<symbol>\"",
            "pixel call-path \"<from>\" \"<to>\"",
        ],
    },
    IntentKind {
        label: "feature",
        criterion: "add new behaviour, a command, an option or an integration that does not exist yet",
        ops: &[
            "pixel find-symbol \"<name>\"",
            "pixel impact \"<symbol>\"",
            "pixel who-calls \"<symbol>\"",
        ],
    },
    IntentKind {
        label: "refactor",
        criterion: "restructure, rename, move or clean up existing code without changing what it does",
        ops: &[
            "pixel find-symbol \"<name>\"",
            "pixel impact \"<symbol>\"",
            "pixel who-calls \"<symbol>\"",
        ],
    },
    IntentKind {
        label: "investigate",
        criterion: "understand how something works or why it behaves as it does before any change; no edit asked yet",
        ops: &[
            "pixel find-symbol \"<name>\"",
            "pixel search-content \"<pattern>\"",
            "pixel call-path \"<from>\" \"<to>\"",
        ],
    },
    IntentKind {
        label: "question",
        criterion: "a direct question to answer in prose, about the code, a tool or a concept; no change asked",
        ops: &[
            "pixel find-symbol \"<name>\"",
            "pixel search-content \"<pattern>\"",
            "pixel call-path \"<from>\" \"<to>\"",
        ],
    },
    IntentKind {
        label: "review",
        criterion: "review, audit or critique existing changes, a diff or a pull request",
        ops: &["pixel review", "pixel what-changed"],
    },
    IntentKind {
        label: "ops",
        criterion: "operate the repository or its tooling: git state, branches, installs, CI, releases, environment",
        ops: &["pixel status", "pixel doctor"],
    },
];

/// The commands that fit `label`, or `None` for a label outside [`INTENTS`].
pub(crate) fn ops_for(label: &str) -> Option<Vec<&'static str>> {
    INTENTS
        .iter()
        .find(|kind| kind.label == label)
        .map(|kind| kind.ops.to_vec())
}

/// The decision request that judges `prompt` against every intent label.
///
/// # Errors
///
/// Only when [`Spec::checked`] rejects the fixed table, which a test pins.
pub(crate) fn spec(prompt: &str) -> Result<Spec, String> {
    Spec::checked(
        prompt.to_string(),
        INTENT_CONTEXT.to_string(),
        labels(),
        INTENTS
            .iter()
            .map(|kind| (kind.label.to_string(), kind.criterion.to_string()))
            .collect(),
    )
}

/// The task-intent label vocabulary, exactly as [`spec`] ships it.
pub(crate) fn labels() -> Vec<String> {
    INTENTS.iter().map(|kind| kind.label.to_string()).collect()
}

#[cfg(test)]
mod tests;
