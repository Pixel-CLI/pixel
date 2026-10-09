// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The prompt-submit brief's optional verdict: when the heuristic gate finds
//! a prompt only weakly about code, `pixel classify` on the local Ollaya
//! engine decides the intent instead of the stem table, but only when that
//! server is already warm. The call runs as a child process on
//! the brief's shared deadline: killed on timeout, absent, disabled or
//! unconfident all fall back to the heuristics — a verdict is an upgrade,
//! never a requirement.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::prompt_intent::{INTENT_CONTEXT, INTENTS};

/// The label the hook adds to the task-intent table: the prompt is not a
/// coding task at all, so the brief has nothing to answer.
const NONE_LABEL: &str = "none";
/// The label of the task-intent table that names repository operation.
const OPS_LABEL: &str = "ops";
const NONE_CRITERION: &str = "not a coding task: chat, a git or release request, prose, or anything the other labels do not cover";

/// Poll granularity while the classify child answers.
const POLL: Duration = Duration::from_millis(10);

/// The child's own cap, inside the shared brief deadline: classify gets at
/// most this much, so an on-time verdict always leaves room for the evidence
/// ops that follow.
const CLASSIFY_BUDGET: Duration = Duration::from_millis(400);

fn judge_args(typed: &str) -> Vec<String> {
    let mut args = vec![
        "classify".to_string(),
        typed.to_string(),
        "--json".to_string(),
        "--context".to_string(),
        INTENT_CONTEXT.to_string(),
        "--if-warm".to_string(),
        "--engine".to_string(),
        "ollaya".to_string(),
        "--ollaya-url".to_string(),
        crate::decide_ollaya::DEFAULT_BASE.to_string(),
    ];
    for kind in INTENTS
        .iter()
        .map(|kind| (kind.label, kind.criterion))
        .chain([(NONE_LABEL, NONE_CRITERION)])
    {
        args.push("--label".to_string());
        args.push(kind.0.to_string());
        args.push("--criterion".to_string());
        args.push(format!("{}={}", kind.0, kind.1));
    }
    args
}

fn stdout_reader(child: &mut Child) -> Option<JoinHandle<Result<String, String>>> {
    child.stdout.take().map(|mut stdout| {
        std::thread::spawn(move || {
            let mut text = String::new();
            stdout
                .read_to_string(&mut text)
                .map(|_| text)
                .map_err(|error| format!("read classify stdout: {error}"))
        })
    })
}

fn join_stdout(reader: Option<JoinHandle<Result<String, String>>>) -> Result<String, String> {
    reader
        .ok_or_else(|| "classify stdout was unavailable".to_string())?
        .join()
        .map_err(|_| "classify stdout reader panicked".to_string())?
}

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

    /// A plain-language prompt the evidence found on topic is still refused
    /// when the judge confidently says it is not a coding task (`none`) or
    /// that it operates the repository (`ops`): with no code identifier to
    /// vouch for it, the model's conviction outweighs a word overlap. A
    /// code-shaped prompt keeps the lighter [`Verdict::denies_brief`], where
    /// `ops` only steers.
    pub(crate) fn denies_prose(&self) -> bool {
        matches!(self.label.as_str(), NONE_LABEL | OPS_LABEL)
            && self.confidence >= MIN_DENY_PROBABILITY
    }

    /// The prompt asks about a change or about dependents: the chain runs
    /// `impact`, where the stem table would have said so too.
    pub(crate) fn change_intent(&self) -> bool {
        matches!(self.label.as_str(), "bugfix" | "refactor" | "review")
    }
}

/// The verdict of `pixel classify` run as a bounded child: the task-intent
/// labels plus `none`, decided only by an already-warm Ollaya server on the
/// loopback default. It never inherits the user's configured remote engine or
/// endpoint, and never starts the local server. `none` is the only deny; every
/// other label just steers the plan.
///
/// Any failure — classify disabled, engine unreachable, timeout, a killed
/// child, unparseable output — is `None`, and the caller's heuristics decide
/// as they always did.
#[cfg_attr(test, mutants::skip)] // Runtime adapter: subprocess + deadline + parse; the verdict policy is tested on `Verdict`.
pub(crate) fn judge(typed: &str, deadline: Instant) -> Option<Verdict> {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            if std::env::var_os("PIXEL_BRIEF_DEBUG").is_some() {
                eprintln!("pixel-brief intent: current_exe: {error}");
            }
            return None;
        }
    };
    let mut command = Command::new(exe);
    command.args(judge_args(typed));
    judge_with_command(command, deadline)
}

fn judge_with_command(mut command: Command, deadline: Instant) -> Option<Verdict> {
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
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => bail!(format!("spawn classify: {e}")),
    };
    let mut reader = stdout_reader(&mut child);
    let output = loop {
        match child.try_wait() {
            Ok(Some(_)) => match join_stdout(reader.take()) {
                Ok(output) => break output,
                Err(error) => bail!(error),
            },
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_stdout(reader.take());
                bail!("classify outlived the brief deadline");
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_stdout(reader.take());
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
    fn labels_should_build_exact_classifier_arguments_including_none() {
        let args = judge_args("typed prompt");
        assert_eq!(
            args,
            [
                "classify",
                "typed prompt",
                "--json",
                "--context",
                INTENT_CONTEXT,
                "--if-warm",
                "--engine",
                "ollaya",
                "--ollaya-url",
                crate::decide_ollaya::DEFAULT_BASE,
                "--label",
                "bugfix",
                "--criterion",
                "bugfix=something that used to work or should work is broken; the prompt asks to find and fix the defect",
                "--label",
                "feature",
                "--criterion",
                "feature=add new behaviour, a command, an option or an integration that does not exist yet",
                "--label",
                "refactor",
                "--criterion",
                "refactor=restructure, rename, move or clean up existing code without changing what it does",
                "--label",
                "investigate",
                "--criterion",
                "investigate=understand how something works or why it behaves as it does before any change; no edit asked yet",
                "--label",
                "question",
                "--criterion",
                "question=a direct question to answer in prose, about the code, a tool or a concept; no change asked",
                "--label",
                "review",
                "--criterion",
                "review=review, audit or critique existing changes, a diff or a pull request",
                "--label",
                "ops",
                "--criterion",
                "ops=operate the repository or its tooling: git state, branches, installs, CI, releases, environment",
                "--label",
                "none",
                "--criterion",
                "none=not a coding task: chat, a git or release request, prose, or anything the other labels do not cover",
            ]
        );
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

    #[test]
    fn denies_prose_should_refuse_a_confident_none_or_ops_and_nothing_else() {
        let verdict = |label: &str, confidence: f64| Verdict {
            label: label.to_string(),
            confidence,
        };
        for label in ["none", "ops"] {
            assert!(verdict(label, 0.9).denies_prose(), "{label}");
            assert!(
                verdict(label, MIN_DENY_PROBABILITY).denies_prose(),
                "{label} at the threshold"
            );
            assert!(
                !verdict(label, MIN_DENY_PROBABILITY - 0.01).denies_prose(),
                "{label} below the threshold"
            );
        }
        for label in [
            "bugfix",
            "feature",
            "refactor",
            "investigate",
            "question",
            "review",
        ] {
            assert!(!verdict(label, 0.99).denies_prose(), "{label}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn judge_should_parse_a_valid_subprocess_json_verdict() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "printf '%s' '{\"predicted\":\"bugfix\",\"probs\":{\"bugfix\":0.91}}'",
        ]);

        assert_eq!(
            judge_with_command(command, Instant::now() + Duration::from_secs(1)),
            Some(Verdict {
                label: "bugfix".to_string(),
                confidence: 0.91,
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn judge_should_fall_back_when_subprocess_stdout_is_empty_or_unreadable() {
        let mut empty = Command::new("sh");
        empty.args(["-c", "exit 0"]);
        assert_eq!(
            judge_with_command(empty, Instant::now() + Duration::from_secs(1)),
            None
        );

        let mut unreadable = Command::new("sh");
        unreadable.args(["-c", "printf '\\377'"]);
        assert_eq!(
            judge_with_command(unreadable, Instant::now() + Duration::from_secs(1)),
            None
        );
    }
}
