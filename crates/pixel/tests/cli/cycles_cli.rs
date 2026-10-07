// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel cycles` end to end: the exit-code contract, the JSON shape, and
//! the coverage honesty.
//!
//! The exit codes carry a distinction prose cannot: `0` means the enumeration
//! was *performed*, whatever it concluded, so a script must not read a non-zero
//! exit as "the answer is no cycles". `2` is a usage error and `3` a technical
//! failure — the two cases where there is no answer at all.

use std::path::Path;
use std::process::Output;

use crate::support::{Scratch, git, pixel_command};

/// A repository with a known cycle: `a` calls `b`, `b` calls `a`.
fn fixture_with_cycle(tag: &str) -> Scratch {
    let dir = Scratch::for_test("cycles-cli", tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/a.ts"),
        "import { b } from \"./b\";\nexport function a(): number { return b() }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/b.ts"),
        "import { a } from \"./a\";\nexport function b(): number { return a() }\n",
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), ".pixel/\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "baseline"]);

    rebuild_graph(&dir);
    dir
}

/// A repository with no cycles: `a` calls `b`, `b` calls nothing.
fn fixture_acyclic(tag: &str) -> Scratch {
    let dir = Scratch::for_test("cycles-cli", tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/a.ts"),
        "import { b } from \"./b\";\nexport function a(): number { return b() }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/b.ts"),
        "export function b(): number { return 0 }\n",
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), ".pixel/\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "baseline"]);

    rebuild_graph(&dir);
    dir
}

/// A repository with exactly one unresolved call: `a` calls `missing`,
/// which is not defined anywhere and not imported, so the resolver cannot
/// attach it to an edge.
fn fixture_with_unresolved_call(tag: &str) -> Scratch {
    let dir = Scratch::for_test("cycles-cli", tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/a.ts"),
        "export function a(): number { return missing() }\n",
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), ".pixel/\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "baseline"]);

    rebuild_graph(&dir);
    dir
}

/// Build or rebuild the graph the way any other command does.
fn rebuild_graph(dir: &Path) {
    let built = pixel_command()
        .args(["rebuild-graph"])
        .arg(dir)
        .output()
        .unwrap();
    assert!(built.status.success(), "graph build failed: {built:?}");
}

fn cycles(dir: &Path, args: &[&str]) -> Output {
    pixel_command()
        .arg("cycles")
        .args(args)
        .arg(dir)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// A cycle is found: exit 0, the component is reported with its witness.
#[test]
fn a_cycle_should_exit_zero_and_report_the_component() {
    let dir = fixture_with_cycle("cycle");
    let output = cycles(&dir, &["--json"]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let components = json.get("components").unwrap().as_array().unwrap();
    assert_eq!(components.len(), 1, "{text}");
    let comp = &components[0];
    assert!(comp.get("id").is_some(), "{text}");
    assert!(
        comp.get("members").unwrap().as_array().unwrap().len() == 2,
        "{text}"
    );
    let witness = comp.get("witness").unwrap();
    assert!(
        witness.get("edges").unwrap().as_array().unwrap().len() == 2,
        "{text}"
    );
}

/// An acyclic graph: exit 0, no components, coverage says exhaustive.
#[test]
fn an_acyclic_graph_should_report_no_cycles() {
    let dir = fixture_acyclic("acyclic");
    let output = cycles(&dir, &["--json"]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let components = json.get("components").unwrap().as_array().unwrap();
    assert_eq!(components.len(), 0, "{text}");
    let coverage = json.get("coverage").unwrap();
    assert_eq!(
        coverage.get("enumeration_exhausted").unwrap().as_bool(),
        Some(true),
        "{text}"
    );
}

/// The human form leads with coverage honesty.
#[test]
fn the_human_form_should_lead_with_coverage() {
    let dir = fixture_with_cycle("human");
    let output = cycles(&dir, &[]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    let first = text.lines().next().unwrap_or_default();
    assert!(first.contains("Enumeration exhaustive"), "{first}");
    assert!(text.contains("cycle(s) found"), "{text}");
}

/// A usage error exits 2.
#[test]
fn an_unknown_tiers_value_should_exit_two() {
    let dir = fixture_with_cycle("bad-tiers");
    let output = cycles(&dir, &["--tiers", "probable"]);

    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

/// A technical failure (non-existent path) exits 3.
#[test]
fn a_nonexistent_path_should_exit_three() {
    let dir = fixture_with_cycle("nonexistent-path");
    let nonexistent = dir.join("does-not-exist");
    let output = cycles(&nonexistent, &["--json"]);

    assert_eq!(output.status.code(), Some(3), "{output:?}");
}

/// The coverage reports unresolved call sites.
#[test]
fn coverage_reports_unresolved_sites() {
    let dir = fixture_with_unresolved_call("unresolved");
    let output = cycles(&dir, &["--json"]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let coverage = json.get("coverage").unwrap();
    assert_eq!(
        coverage.get("unresolved_same_name_sites").unwrap().as_u64(),
        Some(1),
        "{text}"
    );
}

/// The coverage reports which budget stopped the enumeration.
#[test]
fn a_budget_cap_should_be_reported_in_coverage() {
    let dir = fixture_with_cycle("budget");
    let output = cycles(&dir, &["--json", "--max-nodes", "1"]);

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let coverage = json.get("coverage").unwrap();
    assert_eq!(
        coverage.get("enumeration_exhausted").unwrap().as_bool(),
        Some(false),
        "{text}"
    );
    assert!(
        coverage
            .get("stopped_by")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("max_nodes"),
        "{text}"
    );
}
