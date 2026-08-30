//! `gitpixel rescue` — plan correctness and apply safety invariants.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git(dir: &Path, args: &[&str]) {
    git_out(dir, args);
}

fn gitpixel(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pixel"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

const V1: &str = "pub fn calc_total(items: &[u32]) -> u32 {\n    items.iter().sum()\n}\n";
const V2: &str = "pub fn calc_total(items: &[u32]) -> u32 {\n    items.iter().product()\n}\n";

/// Two commits: v1 good, v2 breaks calc (subject names it).
fn fixture(tag: &str) -> (PathBuf, String, String) {
    let dir = std::env::temp_dir().join(format!("gpx-rescue-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    git(&dir, &["init", "-q"]);
    std::fs::write(dir.join("src/calc.rs"), V1).unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "good calc totals"]);
    let v1 = git_out(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    std::fs::write(dir.join("src/calc.rs"), V2).unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "rework calc aggregation"]);
    let v2 = git_out(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    (dir, v1, v2)
}

const D1: &str = "pub fn calc_total(items: &[u32]) -> u32 {\n    items.iter().sum()\n}\npub fn apply_discount(total: u32) -> u32 {\n    total - 1\n}\n";
const D2: &str = "// totals are computed eagerly\npub fn calc_total(items: &[u32]) -> u32 {\n    items.iter().sum()\n}\npub fn apply_discount(total: u32) -> u32 {\n    total - 1\n}\n";
const D3: &str = "// totals are computed eagerly\npub fn calc_total(items: &[u32]) -> u32 {\n    items.iter().sum()\n}\n";

/// Fixture where subject keywords and diff content DISAGREE:
///   A "add totals module"        — introduces apply_discount
///   B "tweak discount rounding"  — subject mentions "discount", but the
///                                  diff never touches the discount code
///   C "simplify aggregation module" — neutral subject, diff REMOVES
///                                  apply_discount entirely
/// The diff-content answer (suspect = C) must win over the subject-keyword
/// answer (which would have flagged B).
fn disagreement_fixture(tag: &str) -> (PathBuf, String, String, String) {
    let dir = std::env::temp_dir().join(format!("gpx-rescue-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    git(&dir, &["init", "-q"]);
    std::fs::write(dir.join("src/calc.rs"), D1).unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "add totals module"]);
    let a = git_out(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    std::fs::write(dir.join("src/calc.rs"), D2).unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "tweak discount rounding"]);
    let b = git_out(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    std::fs::write(dir.join("src/calc.rs"), D3).unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "simplify aggregation module"]);
    let c = git_out(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    (dir, a, b, c)
}

#[test]
fn plan_suspect_is_diff_content_based_and_beats_subject_keywords() {
    let (dir, _a, b, c) = disagreement_fixture("plan-disagree");
    let out = gitpixel(
        &dir,
        &["rescue", "discount broken", ".", "--file", "src/calc.rs", "--json"],
    );
    assert!(out.status.success(), "rescue plan failed: {out:?}");
    let plan: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    let target = plan["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["path"] == "src/calc.rs")
        .expect("calc.rs must be targeted");
    let versions = target["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 3);

    // Newest commit C: neutral subject, but its diff removed the discount
    // code → suspect, with an explicit diff-content basis.
    assert_eq!(versions[0]["oid"], c.as_str());
    assert_eq!(
        versions[0]["suspect"], true,
        "the commit whose CONTENT removed the phrase must be suspect: {versions:?}"
    );
    let basis = versions[0]["suspect_basis"].as_str().unwrap_or("");
    assert!(
        basis.starts_with("diff-content:"),
        "suspect basis must be labeled diff-content, got {basis:?}"
    );

    // Middle commit B: subject mentions "discount" but its diff did not
    // touch the discount code — the old subject-keyword heuristic would
    // have flagged it; diff content must win.
    assert_eq!(versions[1]["oid"], b.as_str());
    assert_eq!(
        versions[1]["suspect"], false,
        "a subject-only keyword match must NOT be suspect when the diff \
         content is readable and shows no removal: {versions:?}"
    );

    // Recommended = the version before the true (diff-content) suspect C,
    // i.e. B — NOT the version before the subject-keyword commit B.
    assert_eq!(target["recommended"]["oid"], b.as_str());
    let rec_basis = target["recommended"]["basis"].as_str().unwrap_or("");
    assert!(
        rec_basis.starts_with("diff-content:"),
        "recommendation must carry its basis, got {rec_basis:?}"
    );

    // No depth cap was hit for this 3-commit file under the default depth.
    assert_eq!(target["depth_cap_hit"], false);
    assert!(target["depth_note"].is_null());

    // Decision block carries both options.
    assert_eq!(plan["decision"]["options"][0]["id"], "revert");
    assert_eq!(plan["decision"]["options"][1]["id"], "fix_forward");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn plan_reports_depth_cap_honestly_when_no_suspect_found() {
    let (dir, _a, _b, _c) = disagreement_fixture("plan-depth");
    // Keywords match nothing; --depth 2 truncates the 3-commit history.
    let out = gitpixel(
        &dir,
        &[
            "rescue",
            "zebra glitter feature",
            ".",
            "--file",
            "src/calc.rs",
            "--depth",
            "2",
            "--json",
        ],
    );
    assert!(out.status.success(), "rescue plan failed: {out:?}");
    let plan: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let target = plan["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["path"] == "src/calc.rs")
        .expect("calc.rs must be targeted");

    assert_eq!(target["depth_cap_hit"], true);
    let note = target["depth_note"].as_str().unwrap_or("");
    assert!(
        note.contains("NOT examined"),
        "hitting the depth cap without a suspect must be said out loud, got {note:?}"
    );
    assert!(note.contains("--depth"), "note should point at the --depth remedy: {note:?}");
    // The note also rides in the decision caveats for JSON consumers.
    let caveats = plan["decision"]["caveats"].as_array().unwrap();
    assert!(
        caveats.iter().any(|c| c.as_str().unwrap_or("").contains("NOT examined")),
        "caveats must carry the bounded-answer note: {caveats:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn apply_restores_working_tree_only() {
    let (dir, v1, _) = fixture("apply");
    let out = gitpixel(
        &dir,
        &["rescue", "--apply", &v1, "--file", "src/calc.rs", "."],
    );
    assert!(out.status.success(), "apply failed: {out:?}");
    assert_eq!(
        std::fs::read_to_string(dir.join("src/calc.rs")).unwrap(),
        V1
    );
    // Index and HEAD untouched: restore shows as an ordinary unstaged diff.
    assert_eq!(
        git_out(&dir, &["diff", "--cached", "--name-only"]).trim(),
        ""
    );
    assert!(!git_out(&dir, &["diff", "--name-only"]).trim().is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn apply_refuses_dirty_without_strategy() {
    let (dir, v1, _) = fixture("dirty");
    let in_progress = format!("{V2}// in-progress work\n");
    std::fs::write(dir.join("src/calc.rs"), &in_progress).unwrap();

    let out = gitpixel(
        &dir,
        &["rescue", "--apply", &v1, "--file", "src/calc.rs", "."],
    );
    assert!(!out.status.success(), "must refuse dirty overwrite");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("uncommitted work"), "unexpected error: {err}");
    // Nothing was touched.
    assert_eq!(
        std::fs::read_to_string(dir.join("src/calc.rs")).unwrap(),
        in_progress
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn apply_merge_keeps_in_progress_edits() {
    let (dir, v1, _) = fixture("merge");
    // In-progress edit in a different region than the v1↔v2 change.
    let in_progress = format!("// header comment added in progress\n{V2}");
    std::fs::write(dir.join("src/calc.rs"), &in_progress).unwrap();

    let out = gitpixel(
        &dir,
        &[
            "rescue",
            "--apply",
            &v1,
            "--file",
            "src/calc.rs",
            ".",
            "--merge",
        ],
    );
    assert!(out.status.success(), "merge apply failed: {out:?}");
    let merged = std::fs::read_to_string(dir.join("src/calc.rs")).unwrap();
    assert!(
        merged.contains("header comment added in progress"),
        "lost in-progress edit: {merged}"
    );
    assert!(
        merged.contains("items.iter().sum()"),
        "old good version not restored: {merged}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn apply_rejects_bad_ref() {
    let (dir, _, _) = fixture("badref");
    let out = gitpixel(
        &dir,
        &[
            "rescue",
            "--apply",
            "deadbeef",
            "--file",
            "src/calc.rs",
            ".",
        ],
    );
    assert!(!out.status.success());
    std::fs::remove_dir_all(&dir).ok();
}
