// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Property-based test over `publish`, in the spirit of usable-git's
//! `publish-property.test.ts`: instead of the crash matrix's 6 hand-picked
//! cells, vary the fixture (which candidate files are pre-staged, which
//! subset gets published) and check the same commit-scope invariant holds
//! broadly, not just for one hand-picked scenario.
//!
//! This does not attempt to port every property from the TS suite — it is
//! a modest, honest slice: for any non-empty subset of a small candidate
//! file set, `publish` must commit EXACTLY that subset (regardless of what
//! else happens to already be staged), and leave every non-selected
//! candidate's on-disk content untouched.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use proptest::prelude::*;
use serde_json::json;
use tempfile::TempDir;

use pixel_ops::publish::{PublishOptions, publish_with_state};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        output.status.success(),
        "git -C {} {:?} failed: {}",
        root.display(),
        args,
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn init_repo(root: &Path) {
    Command::new("git")
        .args(["init", "-q", root.to_str().unwrap()])
        .status()
        .unwrap();
    git(root, &["config", "user.email", "prop@pixel"]);
    git(root, &["config", "user.name", "Prop"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    std::fs::write(root.join("base.txt"), b"base").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
}

const CANDIDATES: &[&str] = &["alpha.txt", "beta.txt", "gamma.txt"];

#[test]
fn publish_without_an_explicit_file_list_stages_and_commits_all_changes() {
    let repo_dir = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();
    let root = repo_dir.path();
    init_repo(root);
    std::fs::write(root.join("default.txt"), b"default publish").unwrap();

    let opts = PublishOptions {
        message: "default publish".to_string(),
        files: Vec::new(),
        expected_head: None,
        expected_fingerprints: Default::default(),
        push: false,
        amend: false,
        request_id: format!("default-{}", uuid::Uuid::new_v4()),
    };

    publish_with_state(root, &opts, None, state_dir.path())
        .expect("the default publish scope commits working-tree changes");
    assert_eq!(
        git(
            root,
            &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
        )
        .trim(),
        "default.txt"
    );
}

fn opts(message: &str, files: &[&str]) -> PublishOptions {
    PublishOptions {
        message: message.to_string(),
        files: files.iter().map(ToString::to_string).collect(),
        expected_head: None,
        expected_fingerprints: Default::default(),
        push: false,
        amend: false,
        request_id: format!("{message}-{}", uuid::Uuid::new_v4()),
    }
}

fn head_changes(root: &Path) -> Vec<String> {
    git(
        root,
        &["diff-tree", "--no-commit-id", "--name-status", "-r", "HEAD"],
    )
    .lines()
    .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
    .filter(|l| !l.is_empty())
    .collect::<BTreeSet<_>>()
    .into_iter()
    .collect()
}

/// A deletion the caller already staged with `git rm` is gone from both the
/// worktree and the index, so `git add -- <path>` rejects it ("pathspec did
/// not match any files"). Publishing that path must still commit the
/// deletion instead of failing before the commit — otherwise a commit that
/// removes a file can only be made with native `git commit`.
#[test]
fn publish_commits_a_deletion_already_staged_with_git_rm() {
    let repo_dir = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();
    let root = repo_dir.path();
    init_repo(root);
    git(root, &["rm", "-q", "--", "base.txt"]);

    let result = publish_with_state(
        root,
        &opts("rm-staged", &["base.txt"]),
        None,
        state_dir.path(),
    )
    .expect("a staged deletion is a legitimate publish target");
    assert_eq!(result["published"], json!(true));
    assert_eq!(head_changes(root), vec!["D base.txt".to_string()]);
    assert_eq!(git(root, &["status", "--porcelain"]).trim(), "");
}

/// A file removed from the worktree only (no `git rm`) is staged by
/// `git add` itself; it must be committed together with the other selected
/// paths, and an unselected pre-staged file must still stay out of the commit.
#[test]
fn publish_commits_a_worktree_deletion_with_other_selected_files() {
    let repo_dir = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();
    let root = repo_dir.path();
    init_repo(root);
    std::fs::remove_file(root.join("base.txt")).unwrap();
    std::fs::write(root.join("new.txt"), b"new").unwrap();
    std::fs::write(root.join("unselected.txt"), b"staged but not selected").unwrap();
    git(root, &["add", "--", "unselected.txt"]);

    publish_with_state(
        root,
        &opts("rm-worktree", &["base.txt", "new.txt"]),
        None,
        state_dir.path(),
    )
    .expect("a worktree deletion is a legitimate publish target");
    assert_eq!(
        head_changes(root),
        vec!["A new.txt".to_string(), "D base.txt".to_string()]
    );
    assert_eq!(
        git(root, &["status", "--porcelain"]).trim(),
        "A  unselected.txt",
        "the unselected pre-staged file must survive, still staged and uncommitted"
    );
}

/// Skipping `git add` for absent paths must not turn a typo into a silent
/// empty commit: a path git has never known is still rejected.
#[test]
fn publish_rejects_a_path_git_has_never_known() {
    let repo_dir = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();
    let root = repo_dir.path();
    init_repo(root);
    let before = git(root, &["rev-parse", "HEAD"]);

    let err = publish_with_state(root, &opts("typo", &["nope.txt"]), None, state_dir.path())
        .expect_err("an unknown path must not be published");
    assert!(
        err.contains("nope.txt"),
        "error must name the offending path: {err}"
    );
    assert_eq!(
        git(root, &["rev-parse", "HEAD"]),
        before,
        "no commit may be created"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// For any non-empty subset of `CANDIDATES` selected for publishing,
    /// and any independent subset already staged beforehand, the resulting
    /// commit contains exactly the selected subset — never more (sweeping
    /// in a pre-staged-but-unselected file), never less.
    #[test]
    fn publish_commits_exactly_the_selected_files(
        selection_mask in 1u8..8u8,
        pre_stage_mask in 0u8..8u8,
    ) {
        let repo_dir = TempDir::new().unwrap();
        let state_dir = TempDir::new().unwrap();
        let root = repo_dir.path();
        init_repo(root);

        let mut selected: Vec<String> = Vec::new();
        for (i, name) in CANDIDATES.iter().enumerate() {
            std::fs::write(root.join(name), format!("content-{name}")).unwrap();
            if pre_stage_mask & (1 << i) != 0 {
                git(root, &["add", "--", name]);
            }
            if selection_mask & (1 << i) != 0 {
                selected.push(name.to_string());
            }
        }
        prop_assume!(!selected.is_empty());

        let opts = PublishOptions {
            message: "property publish".to_string(),
            files: selected.clone(),
            expected_head: None,
            expected_fingerprints: Default::default(),
            push: false,
            amend: false,
            request_id: format!("prop-{}", uuid::Uuid::new_v4()),
        };

        let result = publish_with_state(root, &opts, None, state_dir.path())
            .unwrap_or_else(|e| panic!("publish failed for selection {selected:?}: {e}"));
        prop_assert_eq!(result["published"].clone(), json!(true));

        let committed_raw = git(root, &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"]);
        let committed: BTreeSet<String> = committed_raw
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let expected: BTreeSet<String> = selected.iter().cloned().collect();
        prop_assert_eq!(
            committed.clone(),
            expected.clone(),
            "commit must contain exactly the selected files",
        );

        for (i, name) in CANDIDATES.iter().enumerate() {
            if selection_mask & (1 << i) == 0 {
                let content = std::fs::read_to_string(root.join(name)).unwrap();
                prop_assert_eq!(content, format!("content-{name}"));
                prop_assert!(!committed.contains(*name));
            }
        }
    }
}
