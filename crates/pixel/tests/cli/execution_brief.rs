// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Focused CLI coverage for the bounded `execution-brief` projection.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::support::{Scratch, git, pixel_command};

fn fixture(tag: &str) -> Scratch {
    let dir = Scratch::for_test("gpx-execution-brief-cli", tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/login.rs"),
        "pub fn login_user(name: &str) -> bool {\n    !name.is_empty()\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/other.rs"),
        "pub fn unrelated() -> u32 {\n    7\n}\n",
    )
    .unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "fixture"]);
    dir
}

fn command(dir: &Scratch, args: &[&str]) -> std::process::Output {
    pixel_command()
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn paths_by_tier(report: &Value, tiers: &[&str]) -> BTreeSet<String> {
    report["targets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|target| {
            target["tier"]
                .as_str()
                .is_some_and(|tier| tiers.contains(&tier))
        })
        .filter_map(|target| target["path"].as_str().map(ToString::to_string))
        .collect()
}

#[test]
fn json_brief_preserves_p0_p1_scope_evidence_and_marks_unknown_dependencies() {
    let dir = fixture("json");
    let task = "fix `login_user` login flow";
    let scope = command(&dir, &["scope-task", task, ".", "--json", "--no-manifest"]);
    assert!(scope.status.success(), "scope-task failed: {scope:?}");
    let scope_json: Value = serde_json::from_slice(&scope.stdout).unwrap();

    let brief = command(
        &dir,
        &["execution-brief", task, ".", "--json", "--no-daemon"],
    );
    assert!(brief.status.success(), "execution-brief failed: {brief:?}");
    let json: Value = serde_json::from_slice(&brief.stdout).unwrap();

    assert_eq!(json["version"], 1);
    assert_eq!(json["task"], task);
    assert_eq!(json["uncertainty"]["closed_world"], false);
    assert_eq!(json["uncertainty"]["lower_bound"], true);
    assert!(
        json["uncertainty"]["unknown_dependencies"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );

    let expected = paths_by_tier(&scope_json, &["P0", "P1"]);
    let actual: BTreeSet<String> = json["workstreams"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|workstream| workstream["paths"].as_array().unwrap())
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect();
    assert_eq!(actual, expected);
    assert!(actual.contains("src/login.rs"));

    for workstream in json["workstreams"].as_array().unwrap() {
        assert!(matches!(workstream["tier"].as_str(), Some("P0" | "P1")));
        assert!(matches!(
            workstream["ownership"].as_str(),
            Some("read" | "write")
        ));
        assert_eq!(workstream["depends_on"], serde_json::json!([]));
        for target in workstream["targets"].as_array().unwrap() {
            assert!(target["path"].as_str().is_some());
            assert!(target["symbols"].is_array());
            assert!(target["evidence"].is_array());
        }
    }

    // The projection carries scope-task's own evidence for the target, not
    // just arrays of the right type.
    let scoped = scope_json["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|target| target["path"] == "src/login.rs")
        .expect("scope-task targets src/login.rs");
    let projected = json["workstreams"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|workstream| workstream["targets"].as_array().unwrap())
        .find(|target| target["path"] == "src/login.rs")
        .expect("the brief projects src/login.rs");
    let field = |target: &Value, list: &str, key: &str| -> Vec<String> {
        target[list].as_array().map_or_else(Vec::new, |items| {
            items.iter().map(|item| item[key].to_string()).collect()
        })
    };
    let scoped_symbols = field(scoped, "symbols", "name");
    assert!(
        scoped_symbols.contains(&"\"login_user\"".to_string()),
        "fixture: scope-task found the symbol: {scoped}"
    );
    assert_eq!(field(projected, "symbols", "name"), scoped_symbols);
    assert_eq!(
        field(projected, "symbols", "uid"),
        field(scoped, "symbols", "uid")
    );
    assert_eq!(
        field(projected, "symbols", "line"),
        field(scoped, "symbols", "line")
    );
    assert_eq!(
        field(projected, "evidence", "text"),
        field(scoped, "evidence", "text")
    );
    assert!(
        !scoped["reasons"].as_array().unwrap().is_empty(),
        "fixture: scope-task gave reasons: {scoped}"
    );
    assert_eq!(projected["reasons"], scoped["reasons"]);
}

#[test]
fn human_brief_exposes_workstreams_uncertainty_and_validation() {
    let dir = fixture("human");
    let out = command(
        &dir,
        &[
            "execution-brief",
            "fix `login_user` login flow",
            ".",
            "--no-daemon",
        ],
    );
    assert!(out.status.success(), "execution-brief failed: {out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("execution brief v1"),
        "missing header: {text}"
    );
    assert!(
        text.contains("workstream:src:P0"),
        "missing workstream: {text}"
    );
    assert!(
        text.contains("dependencies: unknown"),
        "missing uncertainty: {text}"
    );
    assert!(text.contains("validation:"), "missing validation: {text}");
}
