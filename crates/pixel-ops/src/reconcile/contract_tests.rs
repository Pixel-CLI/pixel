// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for `sync-branch` (reconcile): what it refuses before
//! touching any state, how it unions an additive conflict, and how the
//! merge-tree output becomes the structured conflict report.

use super::*;
use tempfile::tempdir;

fn opts(push: &str) -> ReconcileOptions {
    ReconcileOptions {
        strategy: "report".to_string(),
        push: push.to_string(),
        request_id: String::new(),
        into_target: None,
    }
}

fn plant_conflict_state(root: &Path) -> std::path::PathBuf {
    let path = root.join(".pixel").join("reconcile-conflict.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{\"conflict_count\":1}").unwrap();
    path
}

/// Real git through the runner, with no global config, hooks or ambient
/// `GIT_*` environment, and an explicit identity for commits.
fn git(root: &Path, args: &[&str]) -> String {
    let mut full = vec!["-c", "user.name=t", "-c", "user.email=t@t"];
    full.extend_from_slice(args);
    let out = GitRunner::new(root)
        .run_isolated(&full)
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    String::from_utf8_lossy(&out).trim().to_string()
}

/// `main` with `a.txt` = "base"; `feature` and `main` then diverge. With
/// `conflicting`, both edit `a.txt`; `feature` always adds `only-ours.txt`
/// and `main` adds `only-theirs.txt`.
fn diverged(conflicting: bool) -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(root.join(".git/info")).unwrap();
    std::fs::write(root.join(".git/info/attributes"), "* merge=text\n").unwrap();
    std::fs::write(root.join("a.txt"), "base\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "base"]);
    git(root, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(root.join("only-ours.txt"), "ours\n").unwrap();
    if conflicting {
        std::fs::write(root.join("a.txt"), "feature\n").unwrap();
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "feature"]);
    git(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("only-theirs.txt"), "theirs\n").unwrap();
    if conflicting {
        std::fs::write(root.join("a.txt"), "main\n").unwrap();
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "main"]);
    git(root, &["checkout", "-q", "feature"]);
    dir
}

// --- refusals before any state is touched -----------------------------------

#[test]
fn reconcile_should_refuse_while_another_git_operation_is_in_progress() {
    let cases: [(&str, bool, &str); 5] = [
        ("rebase-merge", true, "a rebase"),
        ("rebase-apply", true, "a rebase"),
        ("MERGE_HEAD", false, "a merge"),
        ("CHERRY_PICK_HEAD", false, "a cherry-pick"),
        ("REVERT_HEAD", false, "a revert"),
    ];
    for (marker, is_dir, op) in cases {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let git_dir = root.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        if is_dir {
            std::fs::create_dir_all(git_dir.join(marker)).unwrap();
        } else {
            std::fs::write(git_dir.join(marker), "0000\n").unwrap();
        }
        let state = plant_conflict_state(root);
        let err = reconcile(root, &opts("none")).unwrap_err();
        assert!(
            err.starts_with(&format!("{op} is already in progress")),
            "{marker}: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&state).ok().as_deref(),
            Some("{\"conflict_count\":1}"),
            "{marker}: the refusal must come before any state is cleared or rewritten"
        );
    }
}

#[test]
fn reconcile_should_see_an_operation_in_progress_through_a_worktree_gitdir_file() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("wt");
    let real_git_dir = dir.path().join("main.git/worktrees/wt");
    std::fs::create_dir_all(real_git_dir.join("rebase-merge")).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join(".git"),
        format!("gitdir: {}\n", real_git_dir.display()),
    )
    .unwrap();
    let err = reconcile(&root, &opts("none")).unwrap_err();
    assert!(err.starts_with("a rebase is already in progress"), "{err}");
}

#[test]
fn resolve_git_dir_should_fall_back_to_dot_git_when_the_file_names_no_gitdir() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join(".git"), "not a gitdir line\n").unwrap();
    assert_eq!(resolve_git_dir(dir.path()), dir.path().join(".git"));
    assert_eq!(integration_in_progress(dir.path()), None);
}

#[test]
fn reconcile_should_reject_an_unknown_push_mode_after_clearing_stale_conflict_state() {
    let dir = tempdir().unwrap();
    let state = plant_conflict_state(dir.path());
    let err = reconcile(dir.path(), &opts("always")).unwrap_err();
    assert!(err.starts_with("invalid push value \"always\""), "{err}");
    assert!(
        err.contains("\"auto\"") && err.contains("\"none\""),
        "{err}"
    );
    assert!(
        !state.exists(),
        "a new attempt starts without the stale state"
    );
}

#[test]
fn validate_push_mode_should_normalize_never_to_none() {
    assert_eq!(validate_push_mode("auto"), Ok("auto"));
    assert_eq!(validate_push_mode("none"), Ok("none"));
    assert_eq!(validate_push_mode("never"), Ok("none"));
    assert!(validate_push_mode("").is_err());
}

// --- additive conflict auto-resolution ----------------------------------------

#[test]
fn auto_resolve_should_union_each_conflict_hunk_ours_first() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let file = [
        "head",
        "<<<<<<< HEAD",
        "ours 1",
        "=======",
        "theirs 1",
        ">>>>>>> feature",
        "middle",
        "<<<<<<< HEAD",
        "=======",
        "theirs 2",
        ">>>>>>> feature",
        "tail",
    ]
    .join("\n");
    std::fs::write(root.join("f.txt"), file).unwrap();
    assert!(auto_resolve_conflict_markers(
        &GitRunner::new(root),
        root,
        "f.txt"
    ));
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).unwrap(),
        "head\nours 1\ntheirs 1\nmiddle\ntheirs 2\ntail\n"
    );
}

#[test]
fn auto_resolve_should_leave_a_file_alone_when_it_has_no_marker() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "a\n=======\nb").unwrap();
    assert!(auto_resolve_conflict_markers(
        &GitRunner::new(root),
        root,
        "f.txt"
    ));
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).unwrap(),
        "a\n=======\nb",
        "a separator without an opening marker is content"
    );
}

#[test]
fn auto_resolve_should_refuse_an_unterminated_conflict_and_keep_the_file() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    for body in [
        "<<<<<<< HEAD\nours\n",
        "<<<<<<< HEAD\nours\n=======\ntheirs\n",
    ] {
        std::fs::write(root.join("f.txt"), body).unwrap();
        assert!(!auto_resolve_conflict_markers(
            &GitRunner::new(root),
            root,
            "f.txt"
        ));
        assert_eq!(std::fs::read_to_string(root.join("f.txt")).unwrap(), body);
    }
}

#[test]
fn auto_resolve_should_report_failure_when_the_file_cannot_be_read() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    assert!(!auto_resolve_conflict_markers(
        &GitRunner::new(root),
        root,
        "missing.txt"
    ));
}

// --- classification -------------------------------------------------------------

#[test]
fn classify_state_should_name_each_ahead_behind_shape() {
    assert_eq!(classify_state(0, 0), "up_to_date");
    assert_eq!(classify_state(0, 3), "fast_forward");
    assert_eq!(classify_state(2, 0), "ahead");
    assert_eq!(classify_state(2, 3), "diverged");
}

#[test]
fn ignorable_sidecar_entry_should_ignore_only_untracked_sidecar_files() {
    assert!(ignorable_sidecar_entry("??", ".pixel/index"));
    assert!(ignorable_sidecar_entry("??", ".gitpixel/old"));
    assert!(!ignorable_sidecar_entry(" M", ".pixel/targets.json"));
    assert!(!ignorable_sidecar_entry("UU", ".pixel/targets.json"));
    assert!(!ignorable_sidecar_entry("??", "src/new.rs"));
}

// --- merge-tree parsing ------------------------------------------------------------

#[test]
fn parse_stage_lines_should_group_stages_by_path_and_skip_malformed_lines() {
    let text = [
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        "100644 aaa 1\tsrc/a.rs",
        "100644 bbb 2\tsrc/a.rs",
        "100644 ccc 3\tsrc/a.rs",
        "",
        "100644 ddd 2\tonly-ours.rs",
        "100644 eee 2 no-tab-here",
        "100644\tmissing-oid.rs",
        "100644 fff\tmissing-stage.rs",
        "100644 ggg x\tbad-stage.rs",
    ]
    .join("\n");
    let map = parse_stage_lines(&text);
    let flat: Vec<(String, Vec<(u8, String)>)> = map
        .into_iter()
        .map(|(p, s)| (p, s.into_iter().collect()))
        .collect();
    assert_eq!(
        flat,
        vec![
            ("only-ours.rs".to_string(), vec![(2, "ddd".to_string())]),
            (
                "src/a.rs".to_string(),
                vec![
                    (1, "aaa".to_string()),
                    (2, "bbb".to_string()),
                    (3, "ccc".to_string())
                ]
            ),
        ]
    );
}

#[test]
fn parse_conflict_kinds_should_keep_only_well_formed_conflict_lines() {
    let text = [
        "Auto-merging a.txt",
        "  CONFLICT (content): Merge conflict in a.txt",
        "CONFLICT (modify/delete): b.txt deleted in theirs",
        "CONFLICT (broken without paren",
    ]
    .join("\n");
    assert_eq!(
        parse_conflict_kinds(&text),
        vec![
            ("content".to_string(), "Merge conflict in a.txt".to_string()),
            (
                "modify/delete".to_string(),
                "b.txt deleted in theirs".to_string()
            ),
        ]
    );
}

#[test]
fn capped_should_cut_at_the_cap_and_flag_the_cut() {
    assert_eq!(capped("abc".to_string(), 3), ("abc".to_string(), false));
    assert_eq!(capped("abcd".to_string(), 3), ("abc".to_string(), true));
}

#[test]
fn extract_base_span_should_read_the_base_side_of_the_first_hunk_header() {
    assert_eq!(extract_base_span(&None), None);
    assert_eq!(
        extract_base_span(&Some("diff --git a/x b/x\n+added".to_string())),
        None
    );
    assert_eq!(
        extract_base_span(&Some(
            "--- a/x\n+++ b/x\n@@ -12,3 +12,4 @@ fn x\n@@ -40 +41 @@".to_string()
        )),
        Some("12,3".to_string())
    );
}

// --- the report over a real repository ---------------------------------------------

#[test]
fn non_conflicting_paths_should_name_the_side_that_introduced_each_path() {
    let dir = diverged(true);
    let root = dir.path();
    let base = git(root, &["merge-base", "feature", "main"]);
    let report = non_conflicting_paths(root, &base, "feature", "main");
    assert_eq!(report["ours_only_paths"], json!(["only-ours.txt"]));
    assert_eq!(report["theirs_only_paths"], json!(["only-theirs.txt"]));
}

#[test]
fn non_conflicting_paths_should_be_empty_without_a_merge_base() {
    let dir = diverged(false);
    let report = non_conflicting_paths(dir.path(), "", "feature", "main");
    assert_eq!(
        report,
        json!({"ours_only_paths": [], "theirs_only_paths": []})
    );
}

#[test]
fn build_conflict_report_should_describe_every_conflicted_path_with_both_sides() {
    let dir = diverged(true);
    let root = dir.path();
    let base = git(root, &["merge-base", "feature", "main"]);
    let probe = probe_merge_tree(root, "feature", "main");
    assert!(!probe.clean);
    let report = build_conflict_report(&GitRunner::new(root), &base, "feature", "main", &probe);
    assert_eq!(report["conflict_count"], json!(1));
    assert_eq!(report["report_truncated"], json!(false));
    let entry = &report["conflicts"][0];
    assert_eq!(entry["path"], json!("a.txt"));
    assert_eq!(entry["conflict_kind"], json!("content"));
    assert_eq!(entry["base_span"], json!("1"));
    let base_blob = git(root, &["rev-parse", &format!("{base}:a.txt")]);
    assert_eq!(entry["base_oid"], json!(base_blob));
    assert_eq!(
        entry["ours"]["oid"],
        json!(git(root, &["rev-parse", "feature:a.txt"]))
    );
    assert_eq!(
        entry["theirs"]["oid"],
        json!(git(root, &["rev-parse", "main:a.txt"]))
    );
    assert!(
        entry["ours"]["hunk"]
            .as_str()
            .is_some_and(|h| h.contains("+feature")),
        "{entry}"
    );
    assert!(
        entry["theirs"]["hunk"]
            .as_str()
            .is_some_and(|h| h.contains("+main")),
        "{entry}"
    );
    assert_eq!(entry["ours"]["hunk_truncated"], json!(false));
}

#[test]
fn build_conflict_report_should_omit_hunks_without_a_merge_base() {
    let dir = diverged(true);
    let root = dir.path();
    let probe = probe_merge_tree(root, "feature", "main");
    let report = build_conflict_report(&GitRunner::new(root), "", "feature", "main", &probe);
    let entry = &report["conflicts"][0];
    assert_eq!(entry["ours"]["hunk"], Value::Null);
    assert_eq!(entry["theirs"]["hunk"], Value::Null);
    assert_eq!(entry["base_span"], Value::Null);
}
