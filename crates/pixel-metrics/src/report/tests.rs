// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use super::*;

fn brief(ts: u64, tier: &str, kind: &str, bytes: u64, reason: &str) -> Value {
    json!({"t": "brief", "ts": ts, "tier": tier, "kind": kind, "bytes": bytes, "reason": reason})
}

fn turn(
    ts: u64,
    briefed: bool,
    calls: u64,
    injected: &[&str],
    read: &[&str],
    edited: &[&str],
) -> Value {
    json!({"t": "turn", "ts": ts, "briefed": briefed, "calls": calls,
           "injected": injected, "read": read, "edited": edited})
}

fn check(kind: &str, ts: u64, silent: Option<&str>, count: u64, bytes: u64) -> Value {
    json!({"t": kind, "ts": ts, "silent": silent, "count": count, "bytes": bytes})
}

#[test]
fn briefs_should_be_counted_by_outcome_tier_kind_and_reason() {
    let lines = vec![
        brief(10, "high", "lookup", 400, ""),
        brief(11, "high", "flow", 900, ""),
        brief(12, "low", "lookup", 300, ""),
        brief(13, "off", "none", 0, "not_about_code"),
        json!({"t": "brief", "ts": 14, "bytes": 0}),
    ];
    let report = journal_report(&lines, 0);
    assert_eq!(report["briefs"]["delivered"], 3);
    assert_eq!(report["briefs"]["declined"], 2);
    assert_eq!(
        report["briefs"]["by_tier"],
        json!({"high": 2, "low": 1, "off": 1, "none": 1})
    );
    assert_eq!(
        report["briefs"]["delivered_by_kind"],
        json!({"lookup": 2, "flow": 1})
    );
    assert_eq!(
        report["briefs"]["declined_by_reason"],
        json!({"not_about_code": 1, "none": 1})
    );
}

#[test]
fn turns_should_report_zero_call_rate_calls_and_the_overlap_of_injected_and_touched_paths() {
    let lines = vec![
        // Both injected paths read; one more file read: precision 1, recall 2/3.
        turn(10, true, 0, &["a.rs", "b.rs"], &["a.rs", "c.rs"], &["b.rs"]),
        // Nothing read: precision 0.
        turn(11, true, 4, &["x.rs"], &["y.rs"], &[]),
        turn(12, false, 6, &[], &["a.rs"], &[]),
        turn(13, false, 2, &[], &[], &[]),
    ];
    let turns = &journal_report(&lines, 0)["turns"];
    assert_eq!(turns["briefed"], 2);
    assert_eq!(turns["zero_call"], 1);
    assert_eq!(turns["zero_call_rate"], 0.5);
    assert_eq!(turns["mean_calls_briefed"], 2.0);
    assert_eq!(turns["unbriefed"], 2);
    assert_eq!(turns["mean_calls_unbriefed"], 4.0);
    assert_eq!(turns["injected_paths"], 3);
    assert_eq!(turns["touched_paths"], 4);
    assert_eq!(turns["injected_and_touched"], 2);
    assert_eq!(turns["precision"], 2.0 / 3.0);
    assert_eq!(turns["recall"], 0.5);
}

#[test]
fn a_journal_with_no_turns_should_report_no_rates_rather_than_zero() {
    let report = journal_report(&[], 0);
    assert_eq!(report["lines"], 0);
    for key in [
        "zero_call_rate",
        "mean_calls_briefed",
        "mean_calls_unbriefed",
        "precision",
        "recall",
    ] {
        assert_eq!(report["turns"][key], Value::Null, "{key}");
    }
}

#[test]
fn the_edit_hooks_should_count_injections_apart_from_silences() {
    let lines = vec![
        check("edit", 1, None, 2, 300),
        check("edit", 2, Some("already_sent"), 0, 0),
        check("edit", 3, Some("no_shape_change"), 0, 0),
        check("edit", 4, Some("no_shape_change"), 0, 0),
        check("stale", 5, None, 3, 280),
        check("stale", 6, Some("nothing_new"), 0, 0),
        check("guard", 7, None, 1, 200),
        check("guard", 8, Some("already_blocked"), 0, 0),
    ];
    let edits = &journal_report(&lines, 0)["edit_hooks"];
    assert_eq!(edits["cards"], 1);
    assert_eq!(edits["stale_events"], 1);
    assert_eq!(edits["stale_sites"], 3);
    assert_eq!(edits["stop_checks"], 2);
    assert_eq!(edits["stop_blocks"], 1);
    assert_eq!(edits["blocked_sites"], 1);
    assert_eq!(edits["stale_sites_fixed_before_stop"], 2);
    assert_eq!(
        edits["silent"],
        json!({
            "edit:already_sent": 1, "edit:no_shape_change": 2,
            "stale:nothing_new": 1, "guard:already_blocked": 1,
        })
    );
}

#[test]
fn sites_still_stale_at_stop_should_never_make_the_fixed_count_negative() {
    let lines = vec![
        check("stale", 1, None, 1, 100),
        check("guard", 2, None, 5, 100),
    ];
    assert_eq!(
        journal_report(&lines, 0)["edit_hooks"]["stale_sites_fixed_before_stop"],
        0
    );
}

#[test]
fn the_window_should_keep_only_lines_at_or_after_the_cutoff_and_the_report_be_stable() {
    let lines = vec![
        brief(10, "high", "lookup", 400, ""),
        brief(20, "high", "flow", 400, ""),
        json!({"t": "brief", "bytes": 5}),
    ];
    let report = journal_report(&lines, 15);
    assert_eq!(report["lines"], 1);
    assert_eq!(report["briefs"]["delivered"], 1);
    assert_eq!(report["since_ms"], 15);
    assert_eq!(journal_report(&lines, 15), report);
    assert_eq!(journal_report(&lines, 0)["lines"], 3);
}
