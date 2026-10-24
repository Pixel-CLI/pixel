// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use super::*;

#[test]
fn a_window_should_read_seconds_minutes_hours_days_and_all() {
    assert_eq!(window_ms("90s"), Ok(Some(90_000)));
    assert_eq!(window_ms("30m"), Ok(Some(1_800_000)));
    assert_eq!(window_ms("24h"), Ok(Some(86_400_000)));
    assert_eq!(window_ms("7d"), Ok(Some(604_800_000)));
    assert_eq!(window_ms("all"), Ok(None));
    assert_eq!(window_ms(" 2h "), Ok(Some(7_200_000)));
}

#[test]
fn a_window_that_is_not_one_should_say_what_is_taken() {
    for bad in [
        "",
        "h",
        "24",
        "24x",
        "-1h",
        "1.5h",
        "everything",
        "99999999999999999999d",
    ] {
        let error = window_ms(bad).unwrap_err();
        assert!(error.contains("30m, 24h, 7d or all"), "{bad}: {error}");
    }
}

#[test]
fn counts_and_ratios_should_render_for_a_person() {
    assert_eq!(show(&json!(3)), "3");
    assert_eq!(show(&json!(0.5)), "0.50");
    assert_eq!(show(&Value::Null), "n/a");
    assert_eq!(map_line(&json!({"high": 2, "low": 1})), "high 2, low 1");
    assert_eq!(map_line(&json!({})), "none");
}

#[test]
fn the_text_should_label_what_was_counted_apart_from_what_was_estimated() {
    let report = json!({
        "briefs": {"delivered": 3, "declined": 1, "by_tier": {"high": 3}, "delivered_by_kind": {"lookup": 3}, "declined_by_reason": {"not_about_code": 1}},
        "turns": {"briefed": 2, "zero_call": 1, "zero_call_rate": 0.5, "mean_calls_briefed": 2.0, "unbriefed": 0, "mean_calls_unbriefed": null, "precision": 0.5, "recall": null},
        "edit_hooks": {"cards": 1, "stale_events": 1, "stale_sites": 2, "stop_checks": 1, "stop_blocks": 1, "stale_sites_fixed_before_stop": 1, "silent": {"edit:already_sent": 2}},
        "commands": {
            "unique_records": 4, "duplicate_records": 0, "legacy_records": 1,
            "versions": {"workflow-v2": {
                "complete": {"operations": 3, "measured": {"output_bytes": 1200}, "estimated": {"saved_tokens": 450.0}},
                "unavailable": {"operations": 1, "measured": {"output_bytes": 80}, "estimated": {"saved_tokens": null}},
            }},
            "time_estimates": [{"time_estimator_version": "sequential-v1", "coverage": "complete", "operations": 3, "round_trip_ms": 2000, "estimated": {"saved_ms": 5000.0}}],
        },
    });
    // Column widths are the renderer's business: compare words.
    let squeeze = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = squeeze(&render(&report, "since 24h"));
    for expected in [
        "Counted (journal)",
        "briefs delivered                3",
        "zero-call turns                 1 (rate 0.50)",
        "calls per turn                  n/a",
        "precision 0.50, recall n/a",
        "silent because                  edit:already_sent 2",
        "Derived",
        "stale sites fixed before Stop  1",
        "tokens and time are estimates, never measurements",
        "1200 B answered (measured), 450.00 tokens saved (estimate)",
        "unavailable: no comparable baseline",
        "5000.00 ms saved (sequential estimate at 2000 ms a round trip)",
    ] {
        assert!(text.contains(&squeeze(expected)), "{expected}\n{text}");
    }
}
