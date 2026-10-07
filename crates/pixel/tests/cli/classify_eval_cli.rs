// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel classify-eval` end to end: the exit-code contract and the JSON
//! envelope that marks the report as a synthetic evaluation with a modeled
//! fallback.
//!
//! The evaluation never calls a model, so its fallback numbers are estimates
//! from the `--model-error-rate` and `--model-latency-ms` assumptions, not
//! measurements. The epistemics/snapshot envelope is how a machine reader
//! tells that apart from runtime evidence; these tests pin the envelope and
//! the verdict/exit-code pairing so a caller never mistakes a modeled number
//! for a measured one.

use std::process::Output;

use crate::support::{Scratch, git, pixel_command};

fn run(args: &[&str]) -> Output {
    pixel_command().args(args).output().unwrap()
}

fn stdout_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).expect("stdout must be one JSON object")
}

/// The report is a frozen synthetic evaluation, not measured runtime
/// evidence: the epistemics and snapshot envelope must say so, and the
/// modeled fallback assumptions must be disclosed, not hidden.
#[test]
fn the_json_report_should_carry_the_synthetic_envelope() {
    let output = run(&["classify-eval", "--json"]);
    let value = stdout_json(&output);

    assert_eq!(
        value["epistemics"]["basis"], "synthetic frozen evaluation",
        "the basis must name the synthetic evaluation: {value}"
    );
    assert_eq!(
        value["epistemics"]["modeled_fallback"], true,
        "the modeled fallback must be flagged: {value}"
    );
    assert_eq!(value["epistemics"]["closed_world"], true);
    assert_eq!(value["snapshot"]["frozen"], true);
    assert_eq!(value["snapshot"]["deterministic"], true);
    assert!(
        value["snapshot"]["model_error_rate_assumption"].is_number(),
        "the assumed model error rate is disclosed, not implied: {value}"
    );
    assert!(
        value["snapshot"]["model_latency_ms_assumption"].is_number(),
        "the assumed model latency is disclosed, not implied: {value}"
    );
    // Measured tier latency is reported separately from modeled end-to-end
    // latency, so a reader cannot quote the modeled value as measured.
    assert!(value["system"]["p50_tier_latency_ms"].is_number());
    assert!(value["system"]["p95_tier_latency_ms"].is_number());
    assert!(value["system"]["p50_e2e_latency_ms"].is_number());
    assert!(value["system"]["p95_e2e_latency_ms"].is_number());
    assert!(value["system"]["error_rate_after_fallback"].is_number());
}

/// The verdict is the exit code: 0 only on go, 1 on no-go. The two must never
/// disagree, or a script reading one and a human the other split.
#[test]
fn the_exit_code_should_match_the_verdict() {
    let output = run(&["classify-eval", "--json"]);
    let value = stdout_json(&output);
    let verdict = value["verdict"].as_str().unwrap_or_default();
    let expected = if verdict == "go" { 0 } else { 1 };
    assert_eq!(
        output.status.code(),
        Some(expected),
        "verdict {verdict:?} must set the exit code: {output:?}"
    );
}

/// The modeled fallback assumptions are tunable, and the report reflects the
/// values it was given rather than a silent hard-coded constant.
#[test]
fn the_model_assumption_flags_should_surface_in_the_snapshot() {
    let output = run(&[
        "classify-eval",
        "--json",
        "--model-error-rate",
        "0.35",
        "--model-latency-ms",
        "900",
    ]);
    let value = stdout_json(&output);
    assert_eq!(value["snapshot"]["model_error_rate_assumption"], 0.35);
    assert_eq!(value["snapshot"]["model_latency_ms_assumption"], 900.0);
}

/// A label outside the task-intent vocabulary is refused at parse time
/// (exit 2), so a typo can never be written into the verified store and later
/// surface as a classification result.
#[test]
fn a_history_label_outside_the_vocabulary_should_be_a_usage_error() {
    let dir = Scratch::for_test("classify-eval-cli", "bad-label");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "baseline"]);

    let output = pixel_command()
        .args(["classify-history", "add", "some text", "bugfiz"])
        .current_dir(dir.as_ref())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("bugfiz") && stderr.contains("task-intent"),
        "the refusal names the bad label and the vocabulary: {stderr}"
    );
}
