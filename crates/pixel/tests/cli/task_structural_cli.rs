// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! End-to-end structural check kinds (`diff-in-scope`, `graph-resolves`,
//! `tests-touched`) wired into `pixel task verify`.

use std::path::Path;
use std::process::Output;

use serde_json::{Value, json};

use super::support::{Scratch, git, pixel_command};

fn repo(tag: &str) -> Scratch {
    let root = Scratch::for_test("task-structural", tag);
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("source.txt"), "correct\n").unwrap();
    std::fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "initial"]);
    std::fs::create_dir(root.join(".pixel")).unwrap();
    std::fs::write(
        root.join(".pixel/config.json"),
        json!({"task":{"enforcement":"enforce"}}).to_string(),
    )
    .unwrap();
    root
}

fn command(root: &Path, args: &[&str]) -> Output {
    pixel_command()
        .current_dir(root)
        .env("PIXEL_METRICS", "0")
        .env("PIXEL_TASK_POLICY", "gates")
        .env_remove("PIXEL_TASK_CONTRACT")
        .env_remove("PIXEL_TASK_TELEMETRY_PATH")
        .env_remove("PIXEL_TASK_ID")
        .env_remove("PIXEL_TASK_SPAN_ID")
        .env_remove("PIXEL_TASK_PARENT_SPAN_ID")
        .env_remove("PIXEL_TASK_HOST_CALL_ID")
        .args(args)
        .output()
        .unwrap()
}

fn good(root: &Path, args: &[&str]) -> Value {
    let output = command(root, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{args:?}: {error}: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// Run a command that may exit non-zero (e.g. verify with failing checks) and
/// parse its JSON stdout anyway.
fn good_or_failure(root: &Path, args: &[&str]) -> Value {
    let output = command(root, args);
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{args:?}: {error}: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn begin_with(root: &Path, checks: Value) -> String {
    let contract = json!({
        "version": 1,
        "objective": "fix source",
        "checks": checks,
        "criteria": [{"id":"task-acceptance","description":"fix source","checks":[]}],
    });
    std::fs::write(root.join(".pixel/contract.json"), contract.to_string()).unwrap();
    good(
        root,
        &[
            "task",
            "begin",
            "fix source",
            "--contract",
            ".pixel/contract.json",
            "--request-id",
            "begin",
            "--json",
        ],
    )["task_id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn prepare(root: &Path, id: &str) {
    let prepared = good(
        root,
        &["task", "prepare", id, "--request-id", "prepare", "--json"],
    );
    assert_eq!(prepared["phase"], "prepared");
}

fn verify(root: &Path, id: &str) -> Value {
    good_or_failure(
        root,
        &["task", "verify", id, "--request-id", "verify", "--json"],
    )
}

fn receipt<'a>(verified: &'a Value, check_id: &str) -> &'a Value {
    verified["receipts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|receipt| receipt["check_id"] == check_id)
        .unwrap_or_else(|| panic!("no receipt for {check_id}: {verified}"))
}

fn write_manifest(root: &Path, paths: &[&str]) {
    let targets: Vec<Value> = paths
        .iter()
        .map(|path| json!({"path": path, "tier": "P0"}))
        .collect();
    let manifest = json!({
        "version": 2,
        "tasks": [{
            "id": "test-task",
            "task": "fix source",
            "created_unix": 9_999_999_999_u64,
            "head_oid": Value::Null,
            "limit": 20,
            "targets": targets,
        }],
    });
    std::fs::write(
        root.join(".pixel/targets.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

#[test]
fn diff_in_scope_reports_unavailable_without_a_targets_manifest() {
    let root = repo("diff-no-manifest");
    let id = begin_with(&root, json!([{"id":"diff-scope","kind":"diff-in-scope"}]));
    std::fs::write(root.join("source.txt"), "changed\n").unwrap();
    prepare(&root, &id);
    let verified = verify(&root, &id);
    let entry = receipt(&verified, "diff-scope");
    assert_eq!(entry["outcome"], "unavailable");
    assert!(
        entry["diagnostic"]
            .as_str()
            .unwrap()
            .contains("did not gather")
    );
}

#[test]
fn diff_in_scope_passes_when_the_manifest_covers_the_diff() {
    let root = repo("diff-covered");
    let id = begin_with(&root, json!([{"id":"diff-scope","kind":"diff-in-scope"}]));
    std::fs::write(root.join("source.txt"), "changed\n").unwrap();
    write_manifest(&root, &["source.txt"]);
    prepare(&root, &id);
    let verified = verify(&root, &id);
    let entry = receipt(&verified, "diff-scope");
    assert_eq!(entry["outcome"], "passed");
    let structural = &entry["structural"];
    assert_eq!(structural["passed"], true);
    assert_eq!(structural["complete"], true);
    assert_eq!(structural["witnesses"], json!([]));
}

#[test]
fn diff_in_scope_fails_with_each_out_of_scope_path_as_witness() {
    let root = repo("diff-uncovered");
    let id = begin_with(&root, json!([{"id":"diff-scope","kind":"diff-in-scope"}]));
    std::fs::write(root.join("source.txt"), "changed\n").unwrap();
    std::fs::write(root.join("extra.txt"), "out of scope\n").unwrap();
    write_manifest(&root, &["source.txt"]);
    prepare(&root, &id);
    let verified = verify(&root, &id);
    let entry = receipt(&verified, "diff-scope");
    assert_eq!(entry["outcome"], "failed");
    let structural = &entry["structural"];
    assert_eq!(structural["passed"], false);
    assert_eq!(structural["complete"], true);
    assert_eq!(structural["witnesses"], json!(["out of scope: extra.txt"]));
}

#[test]
fn tests_touched_finds_a_bare_source_change_and_passes_with_a_test_change() {
    let root = repo("tests-touched");
    let id = begin_with(&root, json!([{"id":"tests","kind":"tests-touched"}]));
    std::fs::write(root.join("source.txt"), "changed\n").unwrap();
    prepare(&root, &id);
    let verified = verify(&root, &id);
    let entry = receipt(&verified, "tests");
    assert_eq!(entry["outcome"], "failed");
    assert_eq!(
        entry["structural"]["witnesses"],
        json!(["non-test change without any test change: source.txt"])
    );

    // Touch a test file too: the same check now passes.
    let root = repo("tests-touched-ok");
    let id = begin_with(&root, json!([{"id":"tests","kind":"tests-touched"}]));
    std::fs::write(root.join("source.txt"), "changed\n").unwrap();
    std::fs::write(root.join("source_test.rs"), "#[test]\nfn t() {}\n").unwrap();
    prepare(&root, &id);
    let verified = verify(&root, &id);
    let entry = receipt(&verified, "tests");
    assert_eq!(entry["outcome"], "passed");
    assert_eq!(entry["structural"]["passed"], true);
    assert_eq!(entry["structural"]["witnesses"], json!([]));
}

#[test]
fn graph_resolves_reports_unavailable_without_a_graph() {
    let root = repo("graph-no-db");
    let id = begin_with(&root, json!([{"id":"graph","kind":"graph-resolves"}]));
    std::fs::write(root.join("source.txt"), "changed\n").unwrap();
    prepare(&root, &id);
    // Remove any graph db that prepare may have created.
    let _ = std::fs::remove_file(root.join(".pixel/graph.v2.db"));
    let verified = verify(&root, &id);
    let entry = receipt(&verified, "graph");
    assert_eq!(entry["outcome"], "unavailable");
    assert!(
        entry["diagnostic"]
            .as_str()
            .unwrap()
            .contains("did not gather")
    );
}

#[test]
fn graph_resolves_passes_when_reextraction_drops_no_edge() {
    let root = repo("graph-fresh");
    let id = begin_with(&root, json!([{"id":"graph","kind":"graph-resolves"}]));
    // Build the graph the check re-extracts against.
    let built = command(&root, &["rebuild-graph", "--json"]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let graph_db = root.join(".pixel").join("graph.v2.db");
    assert!(
        graph_db.exists(),
        "graph db not found at {graph_db:?} after rebuild-graph"
    );
    // A change that adds a new file drops no previously-resolved edge.
    std::fs::write(root.join("added.txt"), "new\n").unwrap();
    prepare(&root, &id);
    let verified = verify(&root, &id);
    let entry = receipt(&verified, "graph");
    assert_eq!(entry["outcome"], "passed");
    assert_eq!(entry["structural"]["passed"], true);
    assert_eq!(entry["structural"]["complete"], true);
}

#[test]
fn structural_checks_run_alongside_argv_checks_in_one_verify() {
    let root = repo("mixed");
    let id = begin_with(
        &root,
        json!([
            {"id":"content","argv":["/bin/sh","-c","test \"$(cat source.txt)\" = correct"],"timeout_ms":5000},
            {"id":"diff-scope","kind":"diff-in-scope"},
        ]),
    );
    std::fs::write(root.join("source.txt"), "changed\n").unwrap();
    write_manifest(&root, &["source.txt"]);
    prepare(&root, &id);
    let verified = verify(&root, &id);
    // The argv check fails (content changed); the structural check passes.
    assert_eq!(receipt(&verified, "content")["outcome"], "failed");
    assert_eq!(receipt(&verified, "diff-scope")["outcome"], "passed");
}

