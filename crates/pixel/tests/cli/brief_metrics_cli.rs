// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel metrics` through the binary: it counts the lines of
//! `.pixel/brief-decisions.jsonl` and `.pixel/brief-feedback.jsonl` inside a
//! window, writes nothing, and states what the counts stand on.

use serde_json::{Value, json};

use crate::support::{Scratch, git, pixel_command};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn fixture(tag: &str) -> Scratch {
    let repo = Scratch::for_test("pixel-brief-metrics-cli", tag);
    std::fs::write(repo.join("a.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(repo.join(".gitignore"), ".pixel/\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "fixture"]);
    std::fs::create_dir_all(repo.join(".pixel")).unwrap();
    repo
}

fn write_log(repo: &Scratch, name: &str, lines: &[Value]) {
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(repo.join(".pixel").join(name), text).unwrap();
}

fn metrics(repo: &Scratch, args: &[&str]) -> std::process::Output {
    pixel_command()
        .current_dir(&**repo)
        .arg("metrics")
        .args(args)
        .arg("--metrics=off")
        .output()
        .unwrap()
}

fn report(repo: &Scratch, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    let out = metrics(repo, &all);
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn the_report_should_count_delivered_and_declined_briefs_zero_call_turns_and_files_read() {
    let repo = fixture("counts");
    let now = now_ms();
    write_log(
        &repo,
        "brief-decisions.jsonl",
        &[
            json!({"ts": now, "gate": "open", "bytes": 900}),
            json!({"ts": now, "gate": "open", "bytes": 100}),
            json!({"ts": now, "gate": "closed", "bytes": 0}),
            json!({"ts": now, "gate": "declined", "bytes": 0}),
            json!({"ts": 1_000, "gate": "open", "bytes": 700}),
        ],
    );
    write_log(
        &repo,
        "brief-feedback.jsonl",
        &[
            json!({"t": "prompt", "ts": now, "bytes": 900}),
            json!({"t": "turn", "ts": now, "briefed": true, "calls": 0,
                   "injected": ["a.rs", "b.rs"], "read": ["a.rs"], "edited": []}),
            json!({"t": "turn", "ts": now, "briefed": true, "calls": 4,
                   "injected": ["c.rs"], "read": ["x.rs"], "edited": ["c.rs"]}),
            json!({"t": "turn", "ts": now, "briefed": false, "calls": 6,
                   "injected": [], "read": ["a.rs"], "edited": []}),
        ],
    );

    let day = report(&repo, &[]);
    assert_eq!(day["window"], "24h");
    assert_eq!(day["briefs"]["delivered"], 2);
    assert_eq!(day["briefs"]["declined"], 2);
    assert_eq!(
        day["briefs"]["declined_by_gate"],
        json!({"closed": 1, "declined": 1})
    );
    assert_eq!(day["turns"]["briefed"], 2);
    assert_eq!(day["turns"]["zero_call"], 1);
    assert_eq!(day["turns"]["zero_call_rate"], 0.5);
    assert_eq!(day["turns"]["unbriefed"], 1);
    assert_eq!(day["turns"]["unbriefed_zero_call"], 0);
    // injected a, b, c; read or edited a, x, c; overlap a, c.
    assert_eq!(day["files"]["injected"], 3);
    assert_eq!(day["files"]["read_or_edited"], 3);
    assert_eq!(day["files"]["injected_and_read"], 2);

    let all = report(&repo, &["--since", "all"]);
    assert_eq!(all["briefs"]["delivered"], 3, "the 1970 line is in `all`");
    assert_eq!(all["window"], "all");
}

#[test]
fn the_json_should_carry_the_epistemics_and_snapshot_envelope() {
    let repo = fixture("envelope");
    write_log(
        &repo,
        "brief-decisions.jsonl",
        &[json!({"ts": now_ms(), "gate": "open", "bytes": 10})],
    );
    let value = report(&repo, &[]);
    assert_eq!(value["epistemics"]["closed_world"], false);
    assert_eq!(value["epistemics"]["lower_bound"], false);
    assert_eq!(value["epistemics"]["confidence"], "complete");
    assert!(
        value["epistemics"]["basis"]
            .as_str()
            .unwrap()
            .contains("nothing is estimated"),
        "{value}"
    );
    assert_eq!(value["snapshot"]["decision_log"], 1);
    assert_eq!(
        value["snapshot"]["feedback_log"],
        Value::Null,
        "no such log"
    );
    assert_eq!(value["snapshot"]["decision_lines_in_window"], 1);
}

#[test]
fn a_repository_with_no_logs_should_report_zeroes_and_no_rates_and_write_nothing() {
    let repo = fixture("empty");
    let value = report(&repo, &[]);
    assert_eq!(value["briefs"]["delivered"], 0);
    assert_eq!(value["turns"]["zero_call_rate"], Value::Null);
    assert_eq!(value["files"]["precision"], Value::Null);
    let text = metrics(&repo, &[]);
    assert!(text.status.success(), "{text:?}");
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("no brief logs under .pixel/"), "{text}");
    assert!(
        !repo.join(".pixel/brief-decisions.jsonl").exists()
            && !repo.join(".pixel/brief-feedback.jsonl").exists(),
        "metrics reads the logs, it never creates them"
    );
}

#[test]
fn the_text_should_name_each_count() {
    let repo = fixture("text");
    let now = now_ms();
    write_log(
        &repo,
        "brief-decisions.jsonl",
        &[
            json!({"ts": now, "gate": "open", "bytes": 10}),
            json!({"ts": now, "gate": "closed", "bytes": 0}),
        ],
    );
    write_log(
        &repo,
        "brief-feedback.jsonl",
        &[json!({"t": "turn", "ts": now, "briefed": true, "calls": 0,
                 "injected": ["a.rs"], "read": ["a.rs"], "edited": []})],
    );
    let out = metrics(&repo, &["--since", "7d"]);
    assert!(out.status.success(), "{out:?}");
    // Column padding is not the contract: compare each line with its blanks squeezed.
    let text: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    let text = text.join("\n");
    for expected in [
        "pixel metrics: since 7d",
        "briefs delivered 1",
        "briefs declined 1 (closed 1)",
        "zero-call turns 1 (100%)",
        "injected, then read 1 (precision 100%, recall 100%)",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
}

#[test]
fn a_window_that_is_not_one_should_fail_with_what_is_taken() {
    let repo = fixture("window");
    let out = metrics(&repo, &["--since", "soon"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("30m, 24h, 7d or all"), "{stderr}");
    assert!(out.stdout.is_empty());
}
