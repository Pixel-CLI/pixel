// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The prompt-submit brief's optional verdict: when the classifier is switched
//! on (`pixel config classify on`, off by default), the configured
//! `pixel classify` engine, local or remote, decides the task intent of a
//! prompt that is not code-shaped, and a confident `none` ends its brief. The
//! call runs as a child process on the brief's shared deadline: killed on
//! timeout, and switched off, absent, failed or unconfident all fall back to
//! the heuristics — a verdict is an upgrade, never a requirement, and no
//! failure of it ever closes a brief.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::prompt_intent::{INTENT_CONTEXT, INTENTS};

/// The label the hook adds to the task-intent table: the prompt is not a
/// coding task at all, so the brief has nothing to answer.
const NONE_LABEL: &str = "none";
const NONE_CRITERION: &str = "not a coding task: chat, a git or release request, prose, or anything the other labels do not cover";

/// Poll granularity while the classify child answers.
const POLL: Duration = Duration::from_millis(10);

/// The most the child gets, inside the shared brief deadline. A remote engine
/// needs more than a warm local one.
const CLASSIFY_BUDGET: Duration = Duration::from_millis(1500);

/// When the child is killed: after [`CLASSIFY_BUDGET`] or half of what is left
/// of the brief's window, whichever is shorter, so a classifier that hangs
/// still leaves the evidence ops half the window.
fn classify_deadline(now: Instant, deadline: Instant) -> Instant {
    now + (deadline.saturating_duration_since(now) / 2).min(CLASSIFY_BUDGET)
}

/// The decision `pixel classify --jsonl` serves: the typed prompt, the
/// task-intent labels with their criteria, and `none`.
fn request(typed: &str) -> Value {
    let mut labels: Vec<&str> = INTENTS.iter().map(|kind| kind.label).collect();
    labels.push(NONE_LABEL);
    let mut criteria = serde_json::Map::new();
    for kind in INTENTS {
        criteria.insert(kind.label.to_string(), json!(kind.criterion));
    }
    criteria.insert(NONE_LABEL.to_string(), json!(NONE_CRITERION));
    json!({
        "text": typed,
        "context": INTENT_CONTEXT,
        "labels": labels,
        "criteria": criteria,
    })
}

type Output = Result<String, String>;

fn stdout_reader(child: &mut Child) -> Option<Receiver<Output>> {
    let mut stdout = child.stdout.take()?;
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = send.send(
            stdout
                .read_to_string(&mut text)
                .map(|_| text)
                .map_err(|error| format!("read classify stdout: {error}")),
        );
    });
    Some(receive)
}

/// The child's output, waited for until `deadline` at most: a process the
/// child started and left holding the pipe must not hold the brief with it.
fn read_stdout(reader: Option<Receiver<Output>>, deadline: Instant) -> Output {
    reader
        .ok_or_else(|| "classify stdout was unavailable".to_string())?
        .recv_timeout(deadline.saturating_duration_since(Instant::now()).max(POLL))
        .map_err(|_| "classify stdout did not close in time".to_string())?
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

    /// The prompt asks about a change or about dependents: the chain runs
    /// `impact`, where the stem table would have said so too.
    pub(crate) fn change_intent(&self) -> bool {
        matches!(self.label.as_str(), "bugfix" | "refactor" | "review")
    }
}

/// The verdict of `pixel classify --jsonl` run as a bounded child: the
/// task-intent labels plus `none`, decided by the engine the user configured,
/// and only when `enabled` says the classifier is switched on. It never starts
/// without that switch, and `none` is the only deny; every other label just
/// steers the plan.
///
/// Any failure — switched off, unreadable setting, no executable, engine
/// unreachable, timeout, a killed child, unparseable output — is `None`, and
/// the caller's heuristics decide as they always did.
pub(crate) fn judge_when(
    enabled: Result<bool, String>,
    typed: &str,
    deadline: Instant,
    command: impl FnOnce() -> Option<Command>,
) -> Option<Verdict> {
    if !enabled.is_ok_and(|on| on) {
        return None;
    }
    judge_with_command(command()?, &request(typed), deadline)
}

#[cfg_attr(test, mutants::skip)] // Runtime adapter: the global switch and this executable; the policy is tested on `judge_when`.
pub(crate) fn judge(typed: &str, deadline: Instant) -> Option<Verdict> {
    judge_when(
        crate::config_cmd::classify_enabled(),
        typed,
        deadline,
        || {
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
            command.args(["--metrics", "off", "classify", "--jsonl"]);
            Some(command)
        },
    )
}

/// Run `command` with `input` as its one stdin line and read its verdict.
fn judge_with_command(mut command: Command, input: &Value, deadline: Instant) -> Option<Verdict> {
    let debug = std::env::var_os("PIXEL_BRIEF_DEBUG").is_some();
    macro_rules! bail {
        ($why:expr) => {{
            if debug {
                eprintln!("pixel-brief intent: {}", $why);
            }
            return None;
        }};
    }
    let deadline = classify_deadline(Instant::now(), deadline);
    let mut child = match command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => bail!(format!("spawn classify: {e}")),
    };
    // A thread writes the line, so a child that never reads cannot hold the
    // brief past its deadline; a killed child breaks the pipe and ends it.
    if let Some(mut stdin) = child.stdin.take() {
        let line = format!("{input}\n");
        std::thread::spawn(move || {
            let _ = stdin.write_all(line.as_bytes());
        });
    }
    let mut reader = stdout_reader(&mut child);
    let output = loop {
        match child.try_wait() {
            Ok(Some(_)) => match read_stdout(reader.take(), deadline) {
                Ok(output) => break output,
                Err(error) => bail!(error),
            },
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
        None => bail!(format!("classify answered without a prediction: {output}")),
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
    fn request_should_carry_the_prompt_the_intent_labels_their_criteria_and_none() {
        let value = request("typed prompt");
        assert_eq!(value["text"], "typed prompt");
        assert_eq!(value["context"], INTENT_CONTEXT);
        let labels: Vec<&str> = value["labels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|label| label.as_str().unwrap())
            .collect();
        let expected: Vec<&str> = INTENTS
            .iter()
            .map(|kind| kind.label)
            .chain([NONE_LABEL])
            .collect();
        assert_eq!(labels, expected);
        for kind in INTENTS {
            assert_eq!(value["criteria"][kind.label], kind.criterion);
        }
        assert_eq!(value["criteria"][NONE_LABEL], NONE_CRITERION);
        assert_eq!(
            value["criteria"].as_object().unwrap().len(),
            INTENTS.len() + 1
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

    /// A stand-in for `pixel classify --jsonl`: reads the request line, then
    /// runs `script`.
    #[cfg(unix)]
    fn engine(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", &format!("cat >/dev/null; {script}")]);
        command
    }

    #[cfg(unix)]
    const SECOND: Duration = Duration::from_secs(2);

    #[cfg(unix)]
    #[test]
    fn judge_should_parse_a_valid_subprocess_json_verdict() {
        let command = engine(
            "printf '%s\\n' '{\"ok\":true,\"predicted\":\"bugfix\",\"probs\":{\"bugfix\":0.91}}'",
        );

        assert_eq!(
            judge_with_command(command, &request("x"), Instant::now() + SECOND),
            Some(Verdict {
                label: "bugfix".to_string(),
                confidence: 0.91,
            })
        );
    }

    #[test]
    fn classify_should_get_half_of_what_is_left_of_the_window_and_never_more_than_its_budget() {
        let now = Instant::now();
        let after = |ms: u64| now + Duration::from_millis(ms);
        assert_eq!(classify_deadline(now, after(750)), after(375));
        assert_eq!(classify_deadline(now, after(3_000)), now + CLASSIFY_BUDGET);
        assert_eq!(classify_deadline(now, after(10_000)), now + CLASSIFY_BUDGET);
        assert_eq!(classify_deadline(after(500), now), after(500));
    }

    #[cfg(unix)]
    #[test]
    fn judge_should_send_the_request_as_one_line_on_stdin() {
        let dir = std::env::temp_dir().join(format!("pixel-intent-stdin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sink = dir.join("request.jsonl");
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!(
                "cat > '{}'; printf '%s\\n' '{{\"ok\":true,\"predicted\":\"none\",\"probs\":{{\"none\":0.8}}}}'",
                sink.display()
            ),
        ]);
        let verdict = judge_with_command(command, &request("hello there"), Instant::now() + SECOND);
        let sent = std::fs::read_to_string(&sink).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            verdict.map(|verdict| verdict.label),
            Some("none".to_string())
        );
        assert_eq!(sent.lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(&sent).unwrap(),
            request("hello there")
        );
    }

    #[cfg(unix)]
    #[test]
    fn judge_should_fall_back_when_the_engine_fails_answers_badly_or_is_too_slow() {
        for (why, script) in [
            ("empty", "exit 0"),
            ("unreadable", "printf '\\377'"),
            (
                "failed",
                "printf '%s\\n' '{\"ok\":false,\"error\":\"no engine\"}'; exit 1",
            ),
            (
                "no prediction",
                "printf '%s\\n' '{\"ok\":false,\"error\":\"no engine\"}'",
            ),
        ] {
            assert_eq!(
                judge_with_command(engine(script), &request("x"), Instant::now() + SECOND),
                None,
                "{why}"
            );
        }
        // Past the deadline the child is killed: no verdict, and on time.
        let started = Instant::now();
        assert_eq!(
            judge_with_command(
                engine("sleep 5"),
                &request("x"),
                Instant::now() + Duration::from_millis(200)
            ),
            None
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn judge_when_should_never_start_the_engine_unless_the_classifier_is_on() {
        let answer =
            "printf '%s\\n' '{\"ok\":true,\"predicted\":\"none\",\"probs\":{\"none\":0.97}}'";
        let deadline = || Instant::now() + SECOND;
        for (setting, why) in [
            (Ok(false), "switched off"),
            (
                Err("classify.enabled must be true or false".to_string()),
                "unreadable",
            ),
        ] {
            let mut started = false;
            let verdict = judge_when(setting, "x", deadline(), || {
                started = true;
                Some(engine(answer))
            });
            assert_eq!((verdict, started), (None, false), "{why}");
        }
        let verdict = judge_when(Ok(true), "x", deadline(), || Some(engine(answer)));
        assert_eq!(
            verdict,
            Some(Verdict {
                label: "none".to_string(),
                confidence: 0.97,
            })
        );
        assert!(verdict.unwrap().denies_brief());
        // On, but no executable to run: the brief goes ahead.
        assert_eq!(judge_when(Ok(true), "x", deadline(), || None), None);
    }
}
