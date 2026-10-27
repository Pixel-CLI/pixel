// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel metrics` — what the prompt-submit briefs did, read back from the
//! two logs the hooks already write.
//!
//! `.pixel/brief-decisions.jsonl` says which prompts got a brief and which
//! were declined (`bytes` is 0); `.pixel/brief-feedback.jsonl` closes each
//! prompt with a `turn` line: the tool calls before the first answer and the
//! repository files the brief injected against the files the agent then read
//! or edited. This command only counts those lines inside a window. It writes
//! nothing, creates no file, and estimates no tokens or time: a rate is a
//! count divided by a count, `null` when there is nothing to divide by.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use pixel_index::index::SHARD_DIR;
use serde_json::{Value, json};

use crate::execution_brief::{decision_log, feedback};

/// Window used when `--since` is not given.
pub const DEFAULT_SINCE: &str = "24h";

/// A log is read up to this many bytes (both hold a few hundred lines).
const MAX_READ_BYTES: u64 = 16_777_216; // 16 MiB

#[derive(Debug, Clone)]
pub struct MetricsOptions {
    pub path: PathBuf,
    pub since: String,
    pub json: bool,
}

/// The milliseconds a window such as `30m`, `24h` or `7d` spans; `None` for
/// `all`.
fn window_ms(since: &str) -> Result<Option<u64>, String> {
    let text = since.trim();
    if text == "all" {
        return Ok(None);
    }
    let bad = || format!("metrics: --since takes 30m, 24h, 7d or all, not {since:?}");
    let unit_ms: u64 = match text.chars().last() {
        Some('m') => 60_000,
        Some('h') => 3_600_000,
        Some('d') => 86_400_000,
        _ => return Err(bad()),
    };
    text[..text.len() - 1]
        .parse::<u64>()
        .ok()
        .filter(|count| *count > 0)
        .and_then(|count| count.checked_mul(unit_ms))
        .map(Some)
        .ok_or_else(bad)
}

/// The JSON lines of `path`; a missing file, or a line that is not JSON, adds
/// none. `None` when there is no file.
fn read_lines(path: &Path) -> Option<Vec<Value>> {
    let file = pixel_git::nofollow::open_read(path).ok()?;
    let mut text = String::new();
    let _ = file.take(MAX_READ_BYTES).read_to_string(&mut text);
    Some(
        text.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect(),
    )
}

fn kind(line: &Value) -> &str {
    line.get("t").and_then(Value::as_str).unwrap_or("")
}

fn number(line: &Value, key: &str) -> u64 {
    line.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn paths(line: &Value, key: &str) -> BTreeSet<String> {
    line.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// `numerator / denominator`, `None` when there is nothing to divide by.
fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator > 0).then(|| numerator as f64 / denominator as f64)
}

/// The briefs the decision log judged: delivered when they carried bytes,
/// declined otherwise, the declined ones grouped by the gate that said no.
fn briefs(decisions: &[&Value]) -> Value {
    let (mut delivered, mut bytes) = (0_u64, 0_u64);
    let mut declined: BTreeMap<String, u64> = BTreeMap::new();
    for line in decisions {
        match number(line, "bytes") {
            0 => {
                let gate = line.get("gate").and_then(Value::as_str).unwrap_or("none");
                *declined.entry(gate.to_owned()).or_default() += 1;
            }
            size => {
                delivered += 1;
                bytes += size;
            }
        }
    }
    json!({
        "delivered": delivered,
        "delivered_bytes": bytes,
        "declined": declined.values().sum::<u64>(),
        "declined_by_gate": declined,
    })
}

/// The turns the feedback log closed, and the files each brief named against
/// the files its agent touched: a read or an edit counts as touching.
fn turns(feedback: &[&Value]) -> (Value, Value) {
    let (mut briefed, mut zero_call, mut briefed_calls) = (0_u64, 0_u64, 0_u64);
    let (mut plain, mut plain_zero, mut plain_calls) = (0_u64, 0_u64, 0_u64);
    let (mut injected, mut touched, mut overlap) = (0_u64, 0_u64, 0_u64);
    for line in feedback.iter().filter(|line| kind(line) == "turn") {
        let calls = number(line, "calls");
        if line.get("briefed").and_then(Value::as_bool) == Some(true) {
            briefed += 1;
            briefed_calls += calls;
            zero_call += u64::from(calls == 0);
            let named = paths(line, "injected");
            let used: BTreeSet<String> = paths(line, "read")
                .union(&paths(line, "edited"))
                .cloned()
                .collect();
            injected += named.len() as u64;
            touched += used.len() as u64;
            overlap += named.intersection(&used).count() as u64;
        } else {
            plain += 1;
            plain_calls += calls;
            plain_zero += u64::from(calls == 0);
        }
    }
    let turns = json!({
        "briefed": briefed,
        "zero_call": zero_call,
        "zero_call_rate": ratio(zero_call, briefed),
        "mean_calls": ratio(briefed_calls, briefed),
        "unbriefed": plain,
        "unbriefed_zero_call": plain_zero,
        "unbriefed_zero_call_rate": ratio(plain_zero, plain),
        "unbriefed_mean_calls": ratio(plain_calls, plain),
    });
    let files = json!({
        "injected": injected,
        "read_or_edited": touched,
        "injected_and_read": overlap,
        "precision": ratio(overlap, injected),
        "recall": ratio(overlap, touched),
    });
    (turns, files)
}

/// Whether a log that keeps its last `cap` lines may have dropped lines of
/// the window: it holds exactly the cap.
fn at_cap(lines: Option<&Vec<Value>>, cap: usize) -> bool {
    lines.is_some_and(|held| held.len() >= cap)
}

/// What every number stands on, as the JSON envelope states it.
fn basis(capped: bool) -> String {
    let mut basis = format!(
        "counts of the retained lines of .pixel/{} (last {}) and .pixel/{} (last {}) inside the window; nothing is estimated",
        decision_log::LOG_FILE,
        decision_log::MAX_LINES,
        feedback::LOG_FILE,
        feedback::MAX_LINES,
    );
    if capped {
        basis.push_str("; a log at its line cap may have dropped lines of the window");
    }
    basis
}

/// The lines of `held` stamped at or after `since_ms`.
fn inside(held: Option<&Vec<Value>>, since_ms: u64) -> Vec<&Value> {
    held.into_iter()
        .flatten()
        .filter(|line| number(line, "ts") >= since_ms)
        .collect()
}

/// The report over the lines at or after `since_ms`. Pure: logs in, JSON out.
fn report(
    decisions: Option<&Vec<Value>>,
    feedback_lines: Option<&Vec<Value>>,
    window: &str,
    since_ms: u64,
) -> Value {
    let (judged, closed) = (
        inside(decisions, since_ms),
        inside(feedback_lines, since_ms),
    );
    let (turns, files) = turns(&closed);
    let capped =
        at_cap(decisions, decision_log::MAX_LINES) || at_cap(feedback_lines, feedback::MAX_LINES);
    json!({
        "window": window,
        "epistemics": {
            "closed_world": false,
            "lower_bound": capped,
            "basis": basis(capped),
            "confidence": if capped { "capped" } else { "complete" },
        },
        "snapshot": {
            "since_ms": since_ms,
            "decision_log": decisions.map(Vec::len),
            "feedback_log": feedback_lines.map(Vec::len),
            "decision_lines_in_window": judged.len(),
            "feedback_lines_in_window": closed.len(),
        },
        "briefs": briefs(&judged),
        "turns": turns,
        "files": files,
    })
}

fn percent(value: &Value) -> String {
    value
        .as_f64()
        .map_or_else(|| "n/a".to_owned(), |rate| format!("{:.0}%", rate * 100.0))
}

fn mean(value: &Value) -> String {
    value
        .as_f64()
        .map_or_else(|| "n/a".to_owned(), |calls| format!("{calls:.1}"))
}

fn by_gate(map: &Value) -> String {
    let gates: Vec<String> = map
        .as_object()
        .into_iter()
        .flatten()
        .map(|(gate, count)| format!("{gate} {count}"))
        .collect();
    if gates.is_empty() {
        String::new()
    } else {
        format!(" ({})", gates.join(", "))
    }
}

fn render(report: &Value) -> String {
    let (briefs, turns, files) = (&report["briefs"], &report["turns"], &report["files"]);
    let snapshot = &report["snapshot"];
    let mut out = format!(
        "pixel metrics: since {}\n",
        report["window"].as_str().unwrap_or("?")
    );
    if snapshot["decision_log"].is_null() && snapshot["feedback_log"].is_null() {
        out.push_str(&format!(
            "no brief logs under .pixel/ ({} and {}); PIXEL_BRIEF_LOG=0 switches them off\n",
            decision_log::LOG_FILE,
            feedback::LOG_FILE,
        ));
    }
    let rows = [
        ("briefs delivered", briefs["delivered"].to_string()),
        (
            "briefs declined",
            format!(
                "{}{}",
                briefs["declined"],
                by_gate(&briefs["declined_by_gate"])
            ),
        ),
        ("briefed turns", turns["briefed"].to_string()),
        (
            "  zero-call turns",
            format!(
                "{} ({})",
                turns["zero_call"],
                percent(&turns["zero_call_rate"])
            ),
        ),
        ("  calls per turn", mean(&turns["mean_calls"])),
        ("turns without a brief", turns["unbriefed"].to_string()),
        (
            "  zero-call turns",
            format!(
                "{} ({})",
                turns["unbriefed_zero_call"],
                percent(&turns["unbriefed_zero_call_rate"])
            ),
        ),
        ("  calls per turn", mean(&turns["unbriefed_mean_calls"])),
        ("files injected", files["injected"].to_string()),
        ("files read or edited", files["read_or_edited"].to_string()),
        (
            "  injected, then read",
            format!(
                "{} (precision {}, recall {})",
                files["injected_and_read"],
                percent(&files["precision"]),
                percent(&files["recall"])
            ),
        ),
    ];
    for (label, value) in rows {
        out.push_str(&format!("  {label:<24} {value}\n"));
    }
    if report["epistemics"]["lower_bound"] == json!(true) {
        out.push_str("a log is at its line cap: the window may hold more than it shows\n");
    }
    out
}

pub fn run(opts: MetricsOptions) -> Result<(), String> {
    let window = window_ms(&opts.since)?;
    let root = crate::discover_root(&opts.path).map_err(|e| format!("metrics: {e}"))?;
    let since_ms = window.map_or(0, |span| {
        pixel_actionlog::now_ms()
            .unsigned_abs()
            .saturating_sub(span)
    });
    let dir = root.join(SHARD_DIR);
    let decisions = read_lines(&dir.join(decision_log::LOG_FILE));
    let closed = read_lines(&dir.join(feedback::LOG_FILE));
    let report = report(decisions.as_ref(), closed.as_ref(), &opts.since, since_ms);
    if opts.json {
        let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
        println!("{text}");
    } else {
        print!("{}", render(&report));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision(ts: u64, gate: &str, bytes: u64) -> Value {
        json!({"ts": ts, "gate": gate, "bytes": bytes})
    }

    fn turn(ts: u64, briefed: bool, calls: u64, injected: &[&str], read: &[&str]) -> Value {
        json!({"t": "turn", "ts": ts, "briefed": briefed, "calls": calls,
               "injected": injected, "read": read, "edited": []})
    }

    #[test]
    fn a_window_is_minutes_hours_days_or_all() {
        assert_eq!(window_ms("30m"), Ok(Some(1_800_000)));
        assert_eq!(window_ms("24h"), Ok(Some(86_400_000)));
        assert_eq!(window_ms("7d"), Ok(Some(604_800_000)));
        assert_eq!(window_ms(" all "), Ok(None));
        for bad in [
            "",
            "d",
            "0h",
            "-1h",
            "5",
            "5s",
            "soon",
            "5é",
            "99999999999999999d",
        ] {
            assert!(window_ms(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_brief_with_bytes_is_delivered_and_one_without_is_declined_by_its_gate() {
        let lines = [
            decision(10, "open", 500),
            decision(10, "open", 300),
            decision(10, "closed", 0),
            decision(10, "declined", 0),
            decision(10, "closed", 0),
        ];
        let all: Vec<&Value> = lines.iter().collect();
        assert_eq!(
            briefs(&all),
            json!({"delivered": 2, "delivered_bytes": 800, "declined": 3,
                   "declined_by_gate": {"closed": 2, "declined": 1}})
        );
    }

    #[test]
    fn a_zero_call_turn_counts_only_among_briefed_turns() {
        let lines = [
            turn(10, true, 0, &["a.rs"], &[]),
            turn(10, true, 3, &["a.rs"], &["a.rs"]),
            turn(10, false, 0, &[], &[]),
            turn(10, false, 5, &[], &["z.rs"]),
            json!({"t": "tool", "ts": 10}),
        ];
        let all: Vec<&Value> = lines.iter().collect();
        let (turns, _) = super::turns(&all);
        assert_eq!(turns["briefed"], 2);
        assert_eq!(turns["zero_call"], 1);
        assert_eq!(turns["zero_call_rate"], 0.5);
        assert_eq!(turns["mean_calls"], 1.5);
        assert_eq!(turns["unbriefed"], 2);
        assert_eq!(turns["unbriefed_zero_call"], 1);
        assert_eq!(turns["unbriefed_mean_calls"], 2.5);
    }

    #[test]
    fn files_injected_are_weighed_against_the_files_read_or_edited() {
        let edited = json!({"t": "turn", "ts": 10, "briefed": true, "calls": 2,
            "injected": ["a.rs", "b.rs", "c.rs"], "read": ["a.rs", "x.rs"], "edited": ["b.rs", "x.rs"]});
        let unbriefed = turn(10, false, 1, &["q.rs"], &["q.rs"]);
        let lines = [edited, unbriefed];
        let all: Vec<&Value> = lines.iter().collect();
        let (_, files) = turns(&all);
        // read or edited: a, b, x. Overlap with the three injected: a and b.
        assert_eq!(
            files,
            json!({"injected": 3, "read_or_edited": 3, "injected_and_read": 2,
                   "precision": 2.0 / 3.0, "recall": 2.0 / 3.0})
        );
    }

    #[test]
    fn a_rate_with_nothing_to_divide_by_is_null_not_zero() {
        let value = report(None, None, "24h", 0);
        assert_eq!(value["turns"]["zero_call_rate"], Value::Null);
        assert_eq!(value["files"]["precision"], Value::Null);
        assert_eq!(value["briefs"]["delivered"], 0);
        assert_eq!(value["snapshot"]["decision_log"], Value::Null);
    }

    #[test]
    fn only_lines_at_or_after_the_window_start_count() {
        let lines = vec![
            decision(99, "open", 5),
            decision(100, "open", 5),
            decision(101, "open", 5),
        ];
        let value = report(Some(&lines), None, "7d", 100);
        assert_eq!(value["briefs"]["delivered"], 2);
        assert_eq!(value["snapshot"]["decision_lines_in_window"], 2);
        assert_eq!(value["snapshot"]["decision_log"], 3);
    }

    #[test]
    fn a_log_exactly_at_its_cap_makes_the_count_a_lower_bound() {
        let full = vec![decision(1, "open", 5); decision_log::MAX_LINES];
        let under = vec![decision(1, "open", 5); decision_log::MAX_LINES - 1];
        let capped = report(Some(&full), None, "all", 0);
        assert_eq!(capped["epistemics"]["lower_bound"], true);
        assert_eq!(capped["epistemics"]["confidence"], "capped");
        assert!(
            capped["epistemics"]["basis"]
                .as_str()
                .unwrap()
                .contains("line cap")
        );
        let complete = report(Some(&under), None, "all", 0);
        assert_eq!(complete["epistemics"]["lower_bound"], false);
        assert_eq!(complete["epistemics"]["confidence"], "complete");
    }

    #[test]
    fn the_text_names_what_was_declined_and_the_rates() {
        let decisions = vec![decision(5, "open", 10), decision(5, "closed", 0)];
        let closed = vec![turn(5, true, 0, &["a.rs"], &["a.rs"])];
        let text = render(&report(Some(&decisions), Some(&closed), "24h", 0));
        let squeezed: Vec<String> = text
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        for expected in [
            "briefs declined 1 (closed 1)",
            "zero-call turns 1 (100%)",
            "injected, then read 1 (precision 100%, recall 100%)",
        ] {
            assert!(
                squeezed.iter().any(|line| line == expected),
                "{expected}\n{text}"
            );
        }
    }

    #[test]
    fn a_ratio_divides_and_has_no_value_without_a_denominator() {
        assert_eq!(ratio(1, 4), Some(0.25));
        assert_eq!(ratio(0, 3), Some(0.0));
        assert_eq!(ratio(0, 0), None);
        assert_eq!(ratio(5, 0), None);
    }

    #[test]
    fn the_read_limit_is_16_mebibytes() {
        assert_eq!(MAX_READ_BYTES, 16 * 1024 * 1024);
    }

    #[test]
    fn a_mean_has_one_decimal_and_a_missing_one_is_not_available() {
        assert_eq!(mean(&json!(1.5)), "1.5");
        assert_eq!(mean(&json!(2.0)), "2.0");
        assert_eq!(mean(&Value::Null), "n/a");
    }

    #[test]
    fn the_missing_logs_note_needs_both_logs_to_be_absent() {
        let lines = vec![decision(1, "open", 5)];
        let note = "no brief logs under .pixel/";
        assert!(render(&report(None, None, "all", 0)).contains(note));
        assert!(!render(&report(Some(&lines), None, "all", 0)).contains(note));
        assert!(!render(&report(None, Some(&lines), "all", 0)).contains(note));
        assert!(!render(&report(Some(&lines), Some(&lines), "all", 0)).contains(note));
    }

    #[test]
    fn the_line_cap_note_is_printed_only_for_a_capped_log() {
        let note = "a log is at its line cap";
        let full = vec![decision(1, "open", 5); decision_log::MAX_LINES];
        let under = vec![decision(1, "open", 5); decision_log::MAX_LINES - 1];
        assert!(render(&report(Some(&full), None, "all", 0)).contains(note));
        assert!(!render(&report(Some(&under), None, "all", 0)).contains(note));
    }
}
