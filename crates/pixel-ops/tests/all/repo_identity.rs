// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! AR-03: one canonical identity for the repository lock and the journal.
//!
//! `publish`, `push` and `rewrite` keyed the lock on a raw `<root>/.git`
//! while the journal key was `root.canonicalize()`: `/repo`, `/./repo`, a
//! symlink to `/repo`, a `/tmp` vs `/private/tmp` path and a linked
//! worktree each took their own lock, so two mutations of one repository
//! could run at the same time instead of serializing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use pixel_ops::publish::{PublishOptions, publish_with_state};
use pixel_ops::{RepositoryLock, repo_identity};

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git -C {} {args:?} failed: {}",
        root.display(),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn init_repo(root: &Path) {
    let out = Command::new("git")
        .args(["init", "-q"])
        .arg(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "git init {root:?}");
    git(root, &["config", "user.email", "repo-identity@example.com"]);
    git(root, &["config", "user.name", "Repo Identity"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("base.txt"), "base\n").unwrap();
    git(root, &["add", "--", "base.txt"]);
    git(root, &["commit", "-qm", "base"]);
}

/// `<path>/.` — the same directory spelled the long way, as a shell or an
/// editor launches a command.
fn dotted(path: &Path) -> PathBuf {
    path.join(".")
}

fn publish_options(request_id: &str) -> PublishOptions {
    PublishOptions {
        message: "lock contention".to_string(),
        files: vec!["base.txt".to_string()],
        expected_head: None,
        expected_fingerprints: BTreeMap::new(),
        push: false,
        amend: false,
        request_id: request_id.to_string(),
    }
}

/// The three extra spellings of one repository the fixture can produce:
/// `/./`, through a symlink, and a linked worktree (whose `.git` is a file
/// pointing at the main repo's).
fn spellings(dir: &Path, root: &Path) -> Vec<(&'static str, PathBuf)> {
    let link = dir.join("link");
    std::os::unix::fs::symlink(root, &link).unwrap();
    let worktree = dir.join("wt");
    git(
        root,
        &[
            "worktree",
            "add",
            "-q",
            worktree.to_str().unwrap(),
            "-b",
            "wt-branch",
        ],
    );
    vec![
        ("`/./`", dotted(root)),
        ("symlink", link),
        ("linked worktree", worktree),
    ]
}

#[test]
fn one_repository_has_one_identity_however_it_is_spelled() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("repo");
    init_repo(&root);

    let expected = root
        .canonicalize()
        .unwrap()
        .join(".git")
        .display()
        .to_string();
    assert_eq!(
        repo_identity(&root),
        expected,
        "the identity is the canonical git common directory",
    );

    for (name, spelling) in spellings(dir.path(), &root) {
        assert_eq!(
            repo_identity(&spelling),
            expected,
            "{name}: another spelling of one repository must share its identity",
        );
    }

    // The worktree case only resolves because git is asked for the real
    // common directory: `<worktree>/.git` is a file, not the repo.
    assert!(
        dir.path().join("wt").join(".git").is_file(),
        "fixture: a linked worktree's .git is a file",
    );
}

/// The lock is the whole point of the identity: a mutation driven through
/// any spelling of a repository whose lock is held must find it busy,
/// rather than take its own lock and mutate concurrently.
#[test]
fn every_spelling_of_one_repository_contends_for_the_same_lock() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("repo");
    init_repo(&root);
    let spellings = spellings(dir.path(), &root);

    let state = TempDir::new().unwrap();
    let mut held = RepositoryLock::acquire_with_state_root(&repo_identity(&root), state.path())
        .expect("the canonical spelling takes the lock");

    for (index, (name, spelling)) in spellings.iter().enumerate() {
        let opts = publish_options(&format!("lock-contention-{index}"));
        let err = publish_with_state(spelling, &opts, None, state.path())
            .expect_err(&format!("{name}: publish must find the repository busy"));
        assert_eq!(
            err, "repository is busy",
            "{name}: took its own lock for a repository already locked",
        );
    }

    // Releasing hands the one lock back to every spelling.
    held.release();
    std::fs::write(root.join("base.txt"), "base changed\n").unwrap();
    let opts = publish_options("lock-after-release");
    let result = publish_with_state(&dotted(&root), &opts, None, state.path())
        .expect("publish succeeds once the lock is released");
    assert_eq!(result["published"], serde_json::json!(true));
}
