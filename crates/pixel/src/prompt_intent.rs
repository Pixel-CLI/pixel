// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Task-intent verdict for the prompt hook: the harness classifies, the model never has to.
//!
//! On each Claude prompt the hook asks the local decision daemon which kind
//! of coding-agent task the prompt is (bugfix, feature, …) and names the
//! pixel ops that fit it. The verdict is a classifier claim, never a
//! repository fact, and it is best-effort: classify disabled, a stored
//! `remote` engine, a daemon that is not already listening, a slow answer or
//! any error all leave the prompt without an intent line. The hook never
//! starts the daemon and never calls a remote engine.
//!
//! `pixel classify --task-intent` asks the same question from the command
//! line, from the same [`INTENTS`] table.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::classify::Spec;
use crate::task_runtime::Intent;

/// Connect cap of the warm probe: a daemon on this machine accepts in well
/// under a millisecond, so 100 ms only bounds a dead or filtered address.
pub(crate) const WARM_PROBE: Duration = Duration::from_millis(100);
/// Whole-request cap of the hook's classify call. A warm labeled call was
/// measured at 0.11 s and a cold one (model load) at 4.3 s: the cap keeps
/// the first and fails the second open, well inside the hook's 750 ms.
pub(crate) const HOOK_CALL_TIMEOUT: Duration = Duration::from_millis(300);
/// A verdict below this probability is not rendered: under one half, the
/// label is not even more likely than all the others together.
pub(crate) const MIN_RENDER_P: f64 = 0.5;
/// The framing every intent label shares.
pub(crate) const INTENT_CONTEXT: &str = "What kind of coding-agent task is this prompt?";

/// One intent label: what it means to the model and the ops that fit it.
pub(crate) struct IntentKind {
    pub(crate) label: &'static str,
    pub(crate) criterion: &'static str,
    pub(crate) ops: &'static [&'static str],
}

/// Every intent label, its `--criterion` definition and the pixel ops to
/// start with — the one spelling the hook, `pixel classify --task-intent`
/// and the rendered line share.
pub(crate) const INTENTS: &[IntentKind] = &[
    IntentKind {
        label: "bugfix",
        criterion: "something that used to work or should work is broken; the prompt asks to find and fix the defect",
        ops: &[
            "pixel plan-rollback \"<problem>\"",
            "pixel dig-history --phrase \"<text>\"",
            "pixel impact \"<symbol>\"",
        ],
    },
    IntentKind {
        label: "feature",
        criterion: "add new behaviour, a command, an option or an integration that does not exist yet",
        ops: &[
            "pixel scope-task \"<task>\"",
            "pixel impact \"<symbol>\"",
            "pixel who-calls \"<symbol>\"",
        ],
    },
    IntentKind {
        label: "refactor",
        criterion: "restructure, rename, move or clean up existing code without changing what it does",
        ops: &[
            "pixel scope-task \"<task>\"",
            "pixel impact \"<symbol>\"",
            "pixel who-calls \"<symbol>\"",
        ],
    },
    IntentKind {
        label: "investigate",
        criterion: "understand how something works or why it behaves as it does before any change; no edit asked yet",
        ops: &[
            "pixel find-code \"<concept>\"",
            "pixel search-meaning \"<query>\"",
            "pixel evaluate path --from \"A\" --to \"B\"",
        ],
    },
    IntentKind {
        label: "question",
        criterion: "a direct question to answer in prose, about the code, a tool or a concept; no change asked",
        ops: &[
            "pixel find-code \"<concept>\"",
            "pixel search-meaning \"<query>\"",
            "pixel evaluate path --from \"A\" --to \"B\"",
        ],
    },
    IntentKind {
        label: "review",
        criterion: "review, audit or critique existing changes, a diff or a pull request",
        ops: &["pixel review-changes", "pixel what-changed"],
    },
    IntentKind {
        label: "ops",
        criterion: "operate the repository or its tooling: git state, branches, installs, CI, releases, environment",
        ops: &["pixel repo-state", "pixel doctor"],
    },
];

/// The ops that fit `label`, or `None` for a label outside [`INTENTS`].
pub(crate) fn ops_for(label: &str) -> Option<&'static [&'static str]> {
    INTENTS
        .iter()
        .find(|kind| kind.label == label)
        .map(|kind| kind.ops)
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
        INTENTS.iter().map(|kind| kind.label.to_string()).collect(),
        INTENTS
            .iter()
            .map(|kind| (kind.label.to_string(), kind.criterion.to_string()))
            .collect(),
    )
}

/// What the hook knows before it may call the classifier.
pub(crate) struct Gate {
    /// `classify.enabled` from the global config.
    pub(crate) enabled: bool,
    /// The stored engine preference (`local`, `remote`, `auto`, none).
    pub(crate) stored_engine: Option<String>,
    /// The local daemon's base URL.
    pub(crate) base: String,
}

/// Classify `prompt` when the gate allows it and the local daemon is already
/// warm; `None` on every other path.
///
/// `warm` probes the base URL and `decide` answers the spec against it,
/// returning the probabilities and the model id: production passes the TCP
/// probe and the Ollaya adapter, tests pass stubs.
pub(crate) fn classify_prompt(
    prompt: &str,
    gate: &Gate,
    warm: impl FnOnce(&str) -> bool,
    decide: impl FnOnce(&str, &Spec) -> Result<(BTreeMap<String, f64>, String), String>,
) -> Option<Intent> {
    if !gate.enabled || !crate::classify_setup::local_permitted(gate.stored_engine.as_deref()) {
        return None;
    }
    if !warm(&gate.base) {
        return None;
    }
    let spec = spec(prompt).ok()?;
    let (probs, model) = decide(&gate.base, &spec).ok()?;
    let label = crate::classify::predicted(&probs, &spec.labels);
    let p = *probs.get(label)?;
    Some(Intent {
        label: label.to_string(),
        p,
        model,
    })
}

/// The hook's production path: the real config, the real probe, the local
/// Ollaya daemon under [`HOOK_CALL_TIMEOUT`].
#[cfg_attr(test, mutants::skip)] // wires the real config and daemon into `classify_prompt`, which is tested with stubs
pub(crate) fn hook_intent(prompt: &str) -> Option<Intent> {
    let gate = Gate {
        enabled: crate::config_cmd::classify_enabled().unwrap_or(false),
        stored_engine: crate::config_cmd::classify_engine(),
        base: crate::classify_setup::local_base(),
    };
    classify_prompt(
        prompt,
        &gate,
        |base| crate::classify_setup::server_reachable_within(base, WARM_PROBE),
        |base, spec| {
            let mut engine =
                crate::decide_ollaya::Ollaya::open(crate::decide_ollaya::OllayaConfig {
                    base: base.to_string(),
                    timeout: HOOK_CALL_TIMEOUT,
                    ..Default::default()
                });
            let probs = engine.decide(spec)?;
            Ok((probs, engine.model_id().to_string()))
        },
    )
}

/// The one line a verdict adds to the task packet, or `None` when it is
/// below [`MIN_RENDER_P`] or names a label outside [`INTENTS`].
pub(crate) fn render_line(intent: &Intent) -> Option<String> {
    if intent.p.is_nan() || intent.p < MIN_RENDER_P {
        return None;
    }
    let ops = ops_for(&intent.label)?;
    Some(format!(
        "Intent (classifier verdict, not fact): {} p={:.2} ({}) → start with: {}",
        intent.label,
        intent.p,
        intent.model,
        ops.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn gate(enabled: bool, stored: Option<&str>) -> Gate {
        Gate {
            enabled,
            stored_engine: stored.map(str::to_string),
            base: "http://127.0.0.1:11435".to_string(),
        }
    }

    fn verdict(label: &str, p: f64) -> BTreeMap<String, f64> {
        INTENTS
            .iter()
            .map(|kind| {
                let share = if kind.label == label {
                    p
                } else {
                    (1.0 - p) / (INTENTS.len() - 1) as f64
                };
                (kind.label.to_string(), share)
            })
            .collect()
    }

    fn intent(label: &str, p: f64) -> Intent {
        Intent {
            label: label.to_string(),
            p,
            model: "winnow:e4b".to_string(),
        }
    }

    #[test]
    fn classify_prompt_should_never_probe_or_decide_when_classify_is_disabled() {
        let probed = Cell::new(false);
        let decided = Cell::new(false);
        let result = classify_prompt(
            "fix the login bug",
            &gate(false, Some("local")),
            |_| {
                probed.set(true);
                true
            },
            |_, _| {
                decided.set(true);
                Ok((verdict("bugfix", 0.9), "m".to_string()))
            },
        );
        assert!(result.is_none());
        assert!(
            !probed.get(),
            "a disabled classify must not touch the daemon"
        );
        assert!(
            !decided.get(),
            "a disabled classify must never call the decider"
        );
    }

    #[test]
    fn classify_prompt_should_never_probe_or_decide_when_the_stored_engine_is_remote() {
        let probed = Cell::new(false);
        let decided = Cell::new(false);
        let result = classify_prompt(
            "fix the login bug",
            &gate(true, Some("remote")),
            |_| {
                probed.set(true);
                true
            },
            |_, _| {
                decided.set(true);
                Ok((verdict("bugfix", 0.9), "m".to_string()))
            },
        );
        assert!(result.is_none());
        assert!(!probed.get());
        assert!(!decided.get(), "the hook never calls a remote engine");
    }

    #[test]
    fn classify_prompt_should_not_decide_when_the_local_daemon_is_not_warm() {
        let probed = Cell::new(None);
        let decided = Cell::new(false);
        let result = classify_prompt(
            "fix the login bug",
            &gate(true, None),
            |base| {
                probed.set(Some(base.to_string()));
                false
            },
            |_, _| {
                decided.set(true);
                Ok((verdict("bugfix", 0.9), "m".to_string()))
            },
        );
        assert!(result.is_none());
        assert_eq!(
            probed.take().as_deref(),
            Some("http://127.0.0.1:11435"),
            "the probe targets the gate's base"
        );
        assert!(!decided.get(), "a cold daemon is never started or called");
    }

    #[test]
    fn classify_prompt_should_return_the_argmax_with_its_probability_and_model_when_warm() {
        let seen = Cell::new(None);
        let result = classify_prompt(
            "the build started failing after the merge",
            &gate(true, Some("local")),
            |_| true,
            |base, spec| {
                seen.set(Some((
                    base.to_string(),
                    spec.text.clone(),
                    spec.context.clone(),
                    spec.labels.clone(),
                )));
                Ok((verdict("bugfix", 0.82), "winnow:e4b".to_string()))
            },
        )
        .unwrap();
        assert_eq!(result, intent("bugfix", 0.82));
        let (base, text, context, labels) = seen.take().unwrap();
        assert_eq!(base, "http://127.0.0.1:11435");
        assert_eq!(text, "the build started failing after the merge");
        assert_eq!(context, INTENT_CONTEXT);
        assert_eq!(
            labels,
            [
                "bugfix",
                "feature",
                "refactor",
                "investigate",
                "question",
                "review",
                "ops"
            ]
        );
    }

    #[test]
    fn classify_prompt_should_fail_open_when_the_decider_errors() {
        let result = classify_prompt(
            "add a flag",
            &gate(true, Some("auto")),
            |_| true,
            |_, _| Err("timed out".to_string()),
        );
        assert!(result.is_none());
    }

    #[test]
    fn spec_should_carry_every_label_with_its_criterion() {
        let spec = spec("add a flag").unwrap();
        assert_eq!(spec.labels.len(), INTENTS.len());
        for kind in INTENTS {
            assert_eq!(spec.criteria[kind.label], kind.criterion, "{}", kind.label);
        }
    }

    #[test]
    fn ops_for_should_map_each_intent_to_its_documented_ops() {
        let first = |label| ops_for(label).unwrap()[0];
        assert_eq!(first("bugfix"), "pixel plan-rollback \"<problem>\"");
        assert!(ops_for("bugfix").unwrap()[1].starts_with("pixel dig-history --phrase"));
        assert_eq!(first("feature"), "pixel scope-task \"<task>\"");
        assert_eq!(first("refactor"), "pixel scope-task \"<task>\"");
        assert_eq!(first("investigate"), "pixel find-code \"<concept>\"");
        assert_eq!(first("question"), "pixel find-code \"<concept>\"");
        assert_eq!(
            ops_for("review").unwrap(),
            ["pixel review-changes", "pixel what-changed"]
        );
        assert_eq!(
            ops_for("ops").unwrap(),
            ["pixel repo-state", "pixel doctor"]
        );
        assert!(ops_for("chit-chat").is_none());
    }

    #[test]
    fn render_line_should_name_the_verdict_as_a_claim_and_list_its_ops() {
        assert_eq!(
            render_line(&intent("bugfix", 0.82)).unwrap(),
            "Intent (classifier verdict, not fact): bugfix p=0.82 (winnow:e4b) → start with: \
             pixel plan-rollback \"<problem>\", pixel dig-history --phrase \"<text>\", pixel impact \"<symbol>\""
        );
    }

    #[test]
    fn render_line_should_skip_a_verdict_below_one_half_or_outside_the_table() {
        assert!(render_line(&intent("bugfix", 0.49)).is_none());
        assert!(render_line(&intent("bugfix", f64::NAN)).is_none());
        assert!(
            render_line(&intent("review", 0.5)).is_some(),
            "exactly one half renders"
        );
        assert!(render_line(&intent("chit-chat", 0.9)).is_none());
    }
}
