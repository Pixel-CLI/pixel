// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel metrics` over a journal and an action log the real binary wrote.

use serde_json::{Value, json};

use crate::support::{Fixture, project, stderr, stdout};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn journal(fixture: &Fixture, lines: &[Value]) {
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(fixture.root().join(".pixel/journal.jsonl"), text).unwrap();
}

fn report(fixture: &Fixture, args: &[&str]) -> Value {
    let mut all = vec!["metrics", "--json"];
    all.extend_from_slice(args);
    let output = fixture.run(&all);
    assert!(output.status.success(), "{}", stderr(&output));
    serde_json::from_str(&stdout(&output)).unwrap()
}

#[test]
fn the_report_should_count_the_journal_and_summarise_the_action_log() {
    let fixture = project();
    assert!(fixture.run(&["status"]).status.success());
    assert!(fixture.run(&["find-symbol", "run"]).status.success());
    let now = now_ms();
    journal(
        &fixture,
        &[
            json!({"t": "brief", "ts": now, "tier": "high", "kind": "lookup", "bytes": 500}),
            json!({"t": "brief", "ts": now, "tier": "off", "bytes": 0, "reason": "not_about_code"}),
            json!({"t": "turn", "ts": now, "briefed": true, "calls": 0,
                   "injected": ["src/a.rs"], "read": [], "edited": ["src/a.rs"]}),
            json!({"t": "edit", "ts": now, "silent": null, "count": 1, "bytes": 300}),
            json!({"t": "brief", "ts": 1_000, "tier": "high", "kind": "flow", "bytes": 700}),
        ],
    );

    let all = report(&fixture, &["--since", "all"]);
    assert_eq!(all["briefs"]["delivered"], 2);
    let day = report(&fixture, &[]);
    assert_eq!(day["briefs"]["delivered"], 1);
    assert_eq!(
        day["briefs"]["declined_by_reason"],
        json!({"not_about_code": 1})
    );
    assert_eq!(day["turns"]["zero_call_rate"], 1.0);
    assert_eq!(day["turns"]["precision"], 1.0);
    assert_eq!(day["edit_hooks"]["cards"], 1);
    assert_eq!(day["window"], "24h");
    // The two commands above left action-log records; this one is not in it yet.
    assert!(
        day["commands"]["record_count"].as_u64().unwrap() >= 2,
        "{day}"
    );
    assert_eq!(
        day["commands"]["token_approximation"],
        "UTF-8 bytes / 4 (including reporting overhead)"
    );
}

#[test]
fn the_text_should_label_counted_derived_and_estimated_numbers() {
    let fixture = project();
    assert!(fixture.run(&["status"]).status.success());
    let output = fixture.run(&["metrics", "--metrics", "off"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    for heading in [
        "Counted (journal)",
        "Derived",
        "tokens and time are estimates, never measurements",
    ] {
        assert!(text.contains(heading), "{heading}\n{text}");
    }
}

#[test]
fn a_repository_with_no_journal_should_report_zeroes_and_no_rates() {
    let fixture = project();
    let _ = std::fs::remove_file(fixture.root().join(".pixel/journal.jsonl"));
    let day = report(&fixture, &[]);
    assert_eq!(day["briefs"]["delivered"], 0);
    assert_eq!(day["turns"]["zero_call_rate"], Value::Null);
}

#[test]
fn a_window_that_is_not_one_should_fail_with_what_is_taken() {
    let fixture = project();
    let output = fixture.run(&["metrics", "--since", "soon"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("30m, 24h, 7d or all"),
        "{}",
        stderr(&output)
    );
}
