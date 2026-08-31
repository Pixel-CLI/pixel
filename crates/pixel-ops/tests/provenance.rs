//! Integration tests for the `provenance` op: real git repos built with
//! real git commands (same convention as the other pixel-ops tests).

use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::tempdir;

use pixel_ops::provenance::{provenance, ProvenanceOptions};

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
    assert!(status.success(), "git {args:?} exited non-zero");
}

fn init_repo(root: &Path) {
    let status = Command::new("git")
        .arg("init")
        .arg("-q")
        .arg(root)
        .status()
        .unwrap();
    assert!(status.success());
    git(root, &["config", "user.email", "alice@example.com"]);
    git(root, &["config", "user.name", "Alice"]);
}

fn set_author(root: &Path, name: &str, email: &str) {
    git(root, &["config", "user.name", name]);
    git(root, &["config", "user.email", email]);
}

fn commit_all(root: &Path, msg: &str) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", msg]);
}

fn opts(file: &str) -> ProvenanceOptions {
    ProvenanceOptions {
        file: file.to_string(),
        ..Default::default()
    }
}

/// Alice commits 3 lines, Bob appends 2 — regions and the histogram must
/// attribute each block to its real author.
#[test]
fn multi_author_repo_attribution() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_repo(root);

    std::fs::write(root.join("app.txt"), "a1\na2\na3\n").unwrap();
    commit_all(root, "alice adds three lines");

    set_author(root, "Bob", "bob@example.com");
    std::fs::write(root.join("app.txt"), "a1\na2\na3\nb4\nb5\n").unwrap();
    commit_all(root, "bob appends two lines");

    let result = provenance(root, &opts("app.txt")).unwrap();

    let regions = result["regions"].as_array().unwrap();
    assert_eq!(regions.len(), 2, "expected two contiguous regions: {result}");
    assert_eq!(regions[0]["author"], "Alice");
    assert_eq!(regions[0]["start_line"], 1);
    assert_eq!(regions[0]["end_line"], 3);
    assert_eq!(regions[1]["author"], "Bob");
    assert_eq!(regions[1]["start_line"], 4);
    assert_eq!(regions[1]["end_line"], 5);
    // Region metadata is populated.
    assert!(regions[0]["oid"].as_str().unwrap().len() == 40);
    assert_eq!(regions[1]["author_mail"], "bob@example.com");
    assert_eq!(regions[1]["summary"], "bob appends two lines");
    assert!(regions[0]["author_time"]
        .as_str()
        .unwrap()
        .contains('T'));

    // Histogram over all regions.
    assert_eq!(result["authors"]["Alice"], 3);
    assert_eq!(result["authors"]["Bob"], 2);

    // File-level facts.
    assert_eq!(result["introduced_by"]["author"], "Alice");
    assert_eq!(result["last_modified"]["author"], "Bob");
    assert_eq!(result["rename_follow"], "log --follow only");
    assert_eq!(result["lower_bound"], false);
}

/// `-L a,b` restricts attribution to the requested lines only.
#[test]
fn line_range_restricts_regions() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_repo(root);

    std::fs::write(root.join("f.txt"), "a1\na2\na3\n").unwrap();
    commit_all(root, "alice");
    set_author(root, "Bob", "bob@example.com");
    std::fs::write(root.join("f.txt"), "a1\na2\na3\nb4\nb5\n").unwrap();
    commit_all(root, "bob");

    let mut o = opts("f.txt");
    o.lines = Some((4, 5));
    let result = provenance(root, &o).unwrap();

    let regions = result["regions"].as_array().unwrap();
    assert_eq!(regions.len(), 1, "range should cover only Bob's block: {result}");
    assert_eq!(regions[0]["author"], "Bob");
    assert_eq!(regions[0]["start_line"], 4);
    assert_eq!(regions[0]["end_line"], 5);
    assert_eq!(result["authors"].get("Alice"), None);
    assert_eq!(result["line_range"], serde_json::json!([4, 5]));
}

/// Verdict: an author who owns lines and introduced the file (positive
/// match, by name and by email substring) and one who owns nothing
/// (negative match).
#[test]
fn author_verdict_positive_and_negative() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_repo(root);

    std::fs::write(root.join("v.txt"), "a1\na2\n").unwrap();
    commit_all(root, "alice creates");
    set_author(root, "Bob", "bob@example.com");
    std::fs::write(root.join("v.txt"), "a1\na2\nb3\n").unwrap();
    commit_all(root, "bob adds");

    // Positive — matched via email substring, case-insensitive.
    let mut o = opts("v.txt");
    o.author = Some("ALICE@EXAMPLE".to_string());
    let result = provenance(root, &o).unwrap();
    let verdict = &result["verdict"];
    assert_eq!(verdict["author_query"], "ALICE@EXAMPLE");
    assert_eq!(verdict["lines_owned"], 2);
    assert_eq!(verdict["regions_owned"], 1);
    assert_eq!(verdict["introduced_file"], true);
    assert!(verdict["last_touch"].as_str().unwrap().contains('T'));

    // Negative — no lines, did not introduce, never touched.
    let mut o = opts("v.txt");
    o.author = Some("mallory".to_string());
    let result = provenance(root, &o).unwrap();
    let verdict = &result["verdict"];
    assert_eq!(verdict["lines_owned"], 0);
    assert_eq!(verdict["regions_owned"], 0);
    assert_eq!(verdict["introduced_file"], false);
    assert_eq!(verdict["last_touch"], Value::Null);

    // No author query -> no verdict object.
    let result = provenance(root, &opts("v.txt")).unwrap();
    assert_eq!(result["verdict"], Value::Null);
}

/// `introduced_by` must survive a rename: Alice creates `old.txt`, Bob
/// renames it to `new.txt` — the introducing commit is still Alice's
/// (found via `log --follow --diff-filter=A`).
#[test]
fn introduced_by_follows_rename() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_repo(root);

    std::fs::write(root.join("old.txt"), "line1\nline2\n").unwrap();
    commit_all(root, "alice creates old.txt");

    set_author(root, "Bob", "bob@example.com");
    git(root, &["mv", "old.txt", "new.txt"]);
    commit_all(root, "bob renames to new.txt");

    let result = provenance(root, &opts("new.txt")).unwrap();
    assert_eq!(
        result["introduced_by"]["author"], "Alice",
        "introduced_by must cross the rename: {result}"
    );
    assert_eq!(
        result["introduced_by"]["summary"],
        "alice creates old.txt"
    );
    assert_eq!(result["last_modified"]["author"], "Bob");
    // Blame content is unchanged by the rename, still Alice's.
    let regions = result["regions"].as_array().unwrap();
    assert_eq!(regions.len(), 1);
    assert_eq!(regions[0]["author"], "Alice");
}

/// Uncommitted working-tree lines blame to the all-zeros oid — they must
/// surface as regions with `oid: null` and author "uncommitted".
#[test]
fn uncommitted_lines_region() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_repo(root);

    std::fs::write(root.join("w.txt"), "a1\na2\n").unwrap();
    commit_all(root, "alice");
    // Append lines without committing.
    std::fs::write(root.join("w.txt"), "a1\na2\nnew-uncommitted\n").unwrap();

    let result = provenance(root, &opts("w.txt")).unwrap();
    let regions = result["regions"].as_array().unwrap();
    assert_eq!(regions.len(), 2, "{result}");
    assert_eq!(regions[1]["oid"], Value::Null);
    assert_eq!(regions[1]["author"], "uncommitted");
    assert_eq!(regions[1]["start_line"], 3);
    assert_eq!(regions[1]["end_line"], 3);
    assert_eq!(result["authors"]["uncommitted"], 1);
}

/// An untracked file gets a structured error naming the path.
#[test]
fn untracked_file_is_a_structured_error() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_repo(root);

    std::fs::write(root.join("tracked.txt"), "x\n").unwrap();
    commit_all(root, "init");
    std::fs::write(root.join("loose.txt"), "not added\n").unwrap();

    let err = provenance(root, &opts("loose.txt")).unwrap_err();
    assert!(err.starts_with("FILE_NOT_TRACKED"), "got: {err}");
    assert!(err.contains("loose.txt"), "error must name the path: {err}");

    let err = provenance(root, &opts("does-not-exist.txt")).unwrap_err();
    assert!(err.starts_with("FILE_NOT_TRACKED"), "got: {err}");
}

/// T2 honesty: more regions than `limit_regions` truncates the list, sets
/// `lower_bound: true` and a warning — while histogram/verdict still cover
/// everything.
#[test]
fn truncation_sets_lower_bound_and_warning() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_repo(root);

    // Alternate authors line-by-line to force many single-line regions.
    std::fs::write(root.join("t.txt"), "l1\nl2\nl3\nl4\n").unwrap();
    commit_all(root, "alice all");
    set_author(root, "Bob", "bob@example.com");
    std::fs::write(root.join("t.txt"), "l1\nB2\nl3\nB4\n").unwrap();
    commit_all(root, "bob every other line");

    let mut o = opts("t.txt");
    o.limit_regions = 2;
    o.author = Some("bob".to_string());
    let result = provenance(root, &o).unwrap();

    assert_eq!(result["regions"].as_array().unwrap().len(), 2);
    assert_eq!(result["region_count_total"], 4);
    assert_eq!(result["lower_bound"], true);
    let warnings = result["warnings"].as_array().unwrap();
    assert!(!warnings.is_empty());
    assert!(warnings[0].as_str().unwrap().contains("limit_regions"));
    // Verdict still counts truncated-away regions.
    assert_eq!(result["verdict"]["lines_owned"], 2);
    assert_eq!(result["verdict"]["regions_owned"], 2);
    // Histogram too.
    assert_eq!(result["authors"]["Bob"], 2);
    assert_eq!(result["authors"]["Alice"], 2);
}
