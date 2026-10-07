// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The prompt-submit brief's optional verdict: when the heuristic gate finds
//! a prompt only weakly about code, `pixel classify --task-intent` — the
//! user's configured engine, Jev, local Ollaya or a remote preset — decides
//! the intent instead of the stem table. The call runs as a child process on
//! the brief's shared deadline: killed on timeout, absent, disabled or
//! unconfident all fall back to the heuristics — a verdict is an upgrade,
//! never a requirement.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::prompt_intent::{INTENT_CONTEXT, INTENTS};

/// The label the hook adds to the task-intent table: the prompt is not a
/// coding task at all, so the brief has nothing to answer.
const NONE_LABEL: &str = "none";
const NONE_CRITERION: &str = "not a coding task: chat, a git or release request, prose, or anything the other labels do not cover";

/// Poll granularity while the classify child answers.
const POLL: Duration = Duration::from_millis(10);

/// The child's own cap, inside the shared brief deadline: classify gets at
/// most this much, so an on-time verdict always leaves room for the evidence
/// ops that follow.
const CLASSIFY_BUDGET: Duration = Duration::from_millis(400);

/// A decided intent: the winning label and its probability, used as the
/// confidence a deny needs.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Verdict {
    pub(crate) label: String,
    pub(crate) confidence: f64,
}

/// Denying needs conviction: below this top-label probability a `none`
/// verdict falls back to the heuristic plan — proceeding never requires a
/// threshold because it is what today does anyway.
const MIN_DENY_PROBABILITY: f64 = 0.5;

impl Verdict {
    /// Only `none` silences a brief, and only confidently: a code-evidence
    /// block is noise for a non-coding prompt, but an `ops` answer is a real
    /// task the model can misjudge (a measured `bugfix → ops` confusion would
    /// otherwise silence a prompt that needed evidence).
    pub(crate) fn denies_brief(&self) -> bool {
        self.label == NONE_LABEL && self.confidence >= MIN_DENY_PROBABILITY
    }

    /// The prompt asks about a change or about dependents: the chain runs
    /// `impact`, where the stem table would have said so too.
    pub(crate) fn change_intent(&self) -> bool {
        matches!(self.label.as_str(), "bugfix" | "refactor" | "review")
    }
}

/// The verdict of `pixel classify` run as a bounded child: the task-intent
/// labels plus `none`, decided by the same engine the user configured
/// (local Ollaya, Jev, a remote preset), never more time than the brief's
/// deadline leaves. `none` is the only deny; every other label just steers
/// the plan.
///
/// Any failure — classify disabled, engine unreachable, timeout, a killed
/// child, unparseable output — is `None`, and the caller's heuristics decide
/// as they always did.
#[cfg_attr(test, mutants::skip)] // Runtime adapter: subprocess + deadline + parse; the verdict policy is tested on `Verdict`.
pub(crate) fn judge(typed: &str, deadline: Instant) -> Option<Verdict> {
    let debug = std::env::var_os("PIXEL_BRIEF_DEBUG").is_some();
    macro_rules! bail {
        ($why:expr) => {{
            if debug {
                eprintln!("pixel-brief intent: {}", $why);
            }
            return None;
        }};
    }
    let deadline = deadline.min(Instant::now() + CLASSIFY_BUDGET);
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => bail!(format!("current_exe: {e}")),
    };
    // `classify <text> --json` with the task-intent labels plus `none`,
    // criteria carried as `label=description` pairs — `--task-intent` itself
    // cannot express the extra deny label.
    let mut args = vec![
        "classify".to_string(),
        typed.to_string(),
        "--json".to_string(),
        "--context".to_string(),
        INTENT_CONTEXT.to_string(),
    ];
    for kind in INTENTS
        .iter()
        .map(|k| (k.label, k.criterion))
        .chain([(NONE_LABEL, NONE_CRITERION)])
    {
        args.push("--label".to_string());
        args.push(kind.0.to_string());
        args.push("--criterion".to_string());
        args.push(format!("{}={}", kind.0, kind.1));
    }
    let mut child = match Command::new(exe)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => bail!(format!("spawn classify: {e}")),
    };
    let output = loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let mut text = String::new();
                let _ = child
                    .stdout
                    .take()
                    .map(|mut out| out.read_to_string(&mut text));
                break text;
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("classify outlived the brief deadline");
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(format!("wait: {e}"));
            }
        }
    };
    let doc: Value = match serde_json::from_str(&output) {
        Ok(doc) => doc,
        Err(e) => bail!(format!("classify json: {e}")),
    };
    let label = match doc["predicted"].as_str() {
        Some(label) => label.to_string(),
        None => bail!("classify answered without a prediction"),
    };
    let confidence = doc["probs"][&label].as_f64().unwrap_or(0.0);
    if debug {
        eprintln!("pixel-brief intent: verdict {label} ({confidence:.2})");
    }
    Some(Verdict { label, confidence })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hook_labels_are_the_intent_table_plus_none() {
        let mut labels: Vec<&str> = INTENTS.iter().map(|kind| kind.label).collect();
        labels.push(NONE_LABEL);
        assert_eq!(labels.len(), 8);
        assert!(labels.contains(&"none"));
    }

    #[test]
    fn a_verdict_should_deny_ops_and_none_and_run_impact_on_change_intents() {
        let verdict = |label: &str| Verdict {
            label: label.to_string(),
            confidence: 0.9,
        };
        assert!(verdict("none").denies_brief());
        assert!(!verdict("ops").denies_brief(), "ops steers, never silences");
        let low = Verdict {
            label: "none".to_string(),
            confidence: MIN_DENY_PROBABILITY - 0.01,
        };
        assert!(
            !low.denies_brief(),
            "an unconfident none never silences a brief"
        );
        for label in ["bugfix", "refactor", "review"] {
            let v = verdict(label);
            assert!(!v.denies_brief(), "{label}");
            assert!(v.change_intent(), "{label}");
        }
        for label in ["feature", "investigate", "question"] {
            let v = verdict(label);
            assert!(!v.denies_brief(), "{label}");
            assert!(!v.change_intent(), "{label}");
        }
    }
}
