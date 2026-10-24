// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel doctor` on the real binary against a throwaway home and repository.

use serde_json::Value;

use crate::support::{project, stderr, stdout};

fn checks(output: &std::process::Output) -> Vec<Value> {
    let report: Value = serde_json::from_str(&stdout(output)).unwrap();
    report["checks"].as_array().unwrap().clone()
}

fn status<'a>(checks: &'a [Value], id: &str) -> &'a str {
    checks
        .iter()
        .find(|check| check["id"] == id)
        .unwrap_or_else(|| panic!("no check {id}"))["status"]
        .as_str()
        .unwrap()
}

#[test]
fn the_catalogue_lists_every_check_with_its_repair() {
    let fixture = project();
    let output = fixture.run(&["doctor", "--list"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    for line in [
        "binary.path",
        "install.claude-hooks  pixel setup",
        "install.codex-hook-review  -",
        "graph.freshness",
    ] {
        assert!(
            text.lines().any(|row| row
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .starts_with(&line.split_whitespace().collect::<Vec<_>>().join(" "))),
            "{line}\n{text}"
        );
    }
}

#[test]
fn a_prepared_repository_on_a_machine_never_set_up_is_yellow_not_red() {
    let fixture = project();
    let output = fixture.run(&["doctor", "--json", "--skip", "binary.shell-path"]);
    assert!(
        output.status.success(),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let checks = checks(&output);
    assert_eq!(status(&checks, "install.skill"), "yellow");
    assert_eq!(status(&checks, "daemon.health"), "yellow");
    assert_eq!(status(&checks, "graph.freshness"), "green");
    assert_eq!(status(&checks, "index.freshness"), "green");
    assert_eq!(status(&checks, "repo.config"), "green");
    assert!(
        checks.iter().all(|check| check["status"] != "red"),
        "{checks:?}"
    );

    let strict = fixture.run(&[
        "doctor",
        "--skip",
        "binary.shell-path",
        "--fail-on",
        "yellow",
    ]);
    assert_eq!(strict.status.code(), Some(1));
    let text = stdout(&strict);
    assert!(text.starts_with("pixel doctor: ran "), "{text}");
    assert!(text.contains("fix: pixel setup"), "{text}");
}

#[test]
fn an_index_behind_head_is_yellow_with_the_repair_to_run() {
    let fixture = project();
    fixture.write("src/c.rs", "pub fn extra() {}\n");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "two"]);
    let output = fixture.run(&[
        "doctor",
        "--json",
        "--only",
        "index.freshness,graph.freshness",
    ]);
    let checks = checks(&output);
    assert_eq!(status(&checks, "index.freshness"), "yellow");
    assert_eq!(status(&checks, "graph.freshness"), "yellow");
    let graph = checks
        .iter()
        .find(|c| c["id"] == "graph.freshness")
        .unwrap();
    assert!(
        graph["summary"].as_str().unwrap().contains("but HEAD is"),
        "{graph}"
    );
    assert!(
        graph["fix"]
            .as_str()
            .unwrap()
            .starts_with("pixel prepare '"),
        "{graph}"
    );
}

#[test]
fn fix_runs_the_repair_once_and_judges_it_by_the_rerun() {
    let fixture = project();
    let output = fixture.run(&[
        "doctor",
        "--fix",
        "--only",
        "install.*",
        "--fail-on",
        "yellow",
        "--json",
    ]);
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "{}",
        stderr(&output)
    );
    let report: Value = serde_json::from_str(&stdout(&output)).unwrap();
    let repairs = report["repairs"].as_array().unwrap();
    assert_eq!(
        repairs.len(),
        1,
        "one command for every install finding: {repairs:?}"
    );
    assert_eq!(repairs[0]["command"], "pixel setup");
    let after = report["checks"].as_array().unwrap();
    assert_eq!(status(after, "install.skill"), "green");
    assert_eq!(status(after, "install.claude-hooks"), "green");
    assert!(
        fixture
            .dir
            .path()
            .join("home/.claude/skills/pixel/SKILL.md")
            .is_file(),
        "the repair wrote the skill"
    );
}

#[test]
fn an_unknown_check_id_is_refused_before_any_check_runs() {
    let fixture = project();
    let output = fixture.run(&["doctor", "--only", "nope.check"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("unknown doctor check `nope.check`"),
        "{}",
        stderr(&output)
    );
    let conflict = fixture.run(&["doctor", "--only", "binary.path", "--skip", "binary.path"]);
    assert_eq!(conflict.status.code(), Some(1));
    assert!(
        stderr(&conflict).contains("both selected"),
        "{}",
        stderr(&conflict)
    );
}
