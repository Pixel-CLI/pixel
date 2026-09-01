//! Integration tests for the `branches` inventory op. Real git fixtures:
//! a bare "remote" plus working clones (same pattern as
//! `reconcile_matrix.rs`), so ahead/behind, merged, no-upstream, stale,
//! and fetch-freshness states are genuine, not simulated.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

use pixel_ops::branches::{branches, BranchesOptions};

// ---------------------------------------------------------------------------
// git fixture helpers
// ---------------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> String {
    git_env(dir, args, &[])
}

fn git_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(dir: &Path, name: &str, content: &str) {
    std::fs::write(dir.join(name), content).unwrap();
}

fn commit_all(dir: &Path, msg: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", msg]);
}

/// Bare remote + one clone ("local") with an initial commit pushed on main.
fn new_remote_and_local() -> (TempDir, TempDir) {
    let remote = TempDir::new().unwrap();
    git(remote.path(), &["init", "-q", "--bare"]);
    // Point the bare remote's HEAD at main: without init.defaultBranch on
    // the machine, HEAD dangles at refs/heads/master and `clone_of` gets a
    // repo with nothing checked out — commits in those clones then land on
    // `master` instead of `main`, silently defusing the fixture.
    git(remote.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let local = TempDir::new().unwrap();
    let out = Command::new("git")
        .args(["clone", "-q"])
        .arg(remote.path())
        .arg(local.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    git(local.path(), &["config", "user.email", "t@t"]);
    git(local.path(), &["config", "user.name", "t"]);
    git(local.path(), &["checkout", "-qb", "main"]);
    write(local.path(), "seed.txt", "seed\n");
    commit_all(local.path(), "init");
    git(local.path(), &["push", "-q", "-u", "origin", "main"]);
    (remote, local)
}

fn clone_of(remote: &Path) -> TempDir {
    let dir = TempDir::new().unwrap();
    let out = Command::new("git")
        .args(["clone", "-q"])
        .arg(remote)
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    dir
}

fn find_branch<'a>(result: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    result["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == name)
        .unwrap_or_else(|| panic!("branch '{name}' missing from inventory"))
}

fn names(list: &serde_json::Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[test]
fn inventory_classifies_ahead_merged_no_upstream_and_stale() {
    let (_remote, local) = new_remote_and_local();
    let root = local.path();

    // origin/HEAD symref so default-branch detection uses the remote path.
    git(root, &["remote", "set-head", "origin", "main"]);

    // feature-ahead: pushed with upstream, then one more local commit.
    git(root, &["checkout", "-qb", "feature-ahead"]);
    write(root, "ahead.txt", "a\n");
    commit_all(root, "ahead base");
    git(root, &["push", "-q", "-u", "origin", "feature-ahead"]);
    write(root, "ahead.txt", "a2\n");
    commit_all(root, "ahead extra");

    // feature-merged: points at main's head, so it is merged into main.
    git(root, &["checkout", "-q", "main"]);
    git(root, &["branch", "feature-merged", "main"]);

    // feature-no-upstream: local-only commit, never pushed.
    git(root, &["checkout", "-qb", "feature-no-upstream"]);
    write(root, "nu.txt", "n\n");
    commit_all(root, "no upstream");

    // feature-stale: committer date backdated 60 days.
    git(root, &["checkout", "-qb", "feature-stale", "main"]);
    write(root, "stale.txt", "s\n");
    git(root, &["add", "-A"]);
    git_env(
        root,
        &["commit", "-qm", "old work"],
        &[
            ("GIT_COMMITTER_DATE", "2026-07-01T12:00:00"),
            ("GIT_AUTHOR_DATE", "2026-07-01T12:00:00"),
        ],
    );

    git(root, &["checkout", "-q", "main"]);

    let result = branches(root, &BranchesOptions::default()).unwrap();

    assert_eq!(result["default_branch"], "main");
    assert_eq!(result["fetched"], false);
    assert!(
        !result["warnings"].as_array().unwrap().is_empty(),
        "fetch=false must carry a staleness warning"
    );

    let ahead = find_branch(&result, "feature-ahead");
    assert_eq!(ahead["upstream"], "origin/feature-ahead");
    assert_eq!(ahead["ahead"], 1);
    assert_eq!(ahead["behind"], 0);
    assert_eq!(ahead["merged_into_default"], false);
    assert_eq!(ahead["last_commit"]["subject"], "ahead extra");
    assert_eq!(ahead["last_commit"]["author"], "t");
    assert!(ahead["last_commit"]["date"]
        .as_str()
        .unwrap()
        .contains('T'));

    let merged = find_branch(&result, "feature-merged");
    assert_eq!(merged["merged_into_default"], true);
    assert_eq!(merged["is_current"], false);

    let no_up = find_branch(&result, "feature-no-upstream");
    assert!(no_up["upstream"].is_null());
    assert!(
        no_up["ahead"].is_null() && no_up["behind"].is_null(),
        "ahead/behind without upstream must be null, never 0"
    );

    let stale = find_branch(&result, "feature-stale");
    assert_eq!(stale["stale"], true);

    let main = find_branch(&result, "main");
    assert_eq!(main["is_current"], true);
    assert_eq!(main["stale"], false);

    let summary = &result["summary"];
    assert_eq!(summary["total"], 5);
    assert_eq!(summary["current"], "main");
    assert_eq!(names(&summary["unpushed"]), vec!["feature-ahead"]);
    let no_upstream = names(&summary["no_upstream"]);
    assert!(no_upstream.contains(&"feature-merged".to_string()));
    assert!(no_upstream.contains(&"feature-no-upstream".to_string()));
    assert!(no_upstream.contains(&"feature-stale".to_string()));
    assert!(!no_upstream.contains(&"main".to_string()));
    let candidates = names(&summary["merged_candidates"]);
    assert!(candidates.contains(&"feature-merged".to_string()));
    assert!(
        !candidates.contains(&"main".to_string()),
        "the default branch is never its own merge candidate"
    );
    assert_eq!(names(&summary["stale"]), vec!["feature-stale"]);
    assert_eq!(
        summary["fully_pushed"], false,
        "feature-ahead has ahead=1, so not fully pushed"
    );
}

#[test]
fn fully_pushed_true_then_false_on_unpushed_commit_and_dirty_tree() {
    let (_remote, local) = new_remote_and_local();
    let root = local.path();

    // Clean, everything with an upstream is at ahead=0.
    let result = branches(root, &BranchesOptions::default()).unwrap();
    assert_eq!(result["worktree_clean"], true);
    assert_eq!(result["summary"]["fully_pushed"], true);
    let main = find_branch(&result, "main");
    assert_eq!(main["ahead"], 0);
    assert_eq!(main["behind"], 0);

    // A dirty worktree alone breaks fully_pushed.
    write(root, "wip.txt", "wip\n");
    let result = branches(root, &BranchesOptions::default()).unwrap();
    assert_eq!(result["worktree_clean"], false);
    assert_eq!(result["summary"]["fully_pushed"], false);
    assert!(names(&result["summary"]["unpushed"]).is_empty());

    // An unpushed commit puts main in the unpushed list.
    commit_all(root, "local only");
    let result = branches(root, &BranchesOptions::default()).unwrap();
    assert_eq!(result["worktree_clean"], true);
    assert_eq!(result["summary"]["fully_pushed"], false);
    assert_eq!(names(&result["summary"]["unpushed"]), vec!["main"]);
    assert_eq!(find_branch(&result, "main")["ahead"], 1);
}

#[test]
fn fetch_true_sees_a_moved_remote_that_fetch_false_misses() {
    let (remote, local) = new_remote_and_local();

    // Someone else pushes a new commit to main on the remote.
    let other = clone_of(remote.path());
    write(other.path(), "seed.txt", "seed v2\n");
    commit_all(other.path(), "remote moved");
    git(other.path(), &["push", "-q", "origin", "main"]);

    // Without fetch: stale remote-tracking view, behind=0, warning present.
    let stale_view = branches(local.path(), &BranchesOptions::default()).unwrap();
    assert_eq!(stale_view["fetched"], false);
    assert_eq!(find_branch(&stale_view, "main")["behind"], 0);
    let warnings = stale_view["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("last fetch")),
        "fetch=false must warn about remote-tracking staleness, got {warnings:?}"
    );

    // With fetch: the moved remote is visible, behind=1, no staleness warning.
    let live_view = branches(
        local.path(),
        &BranchesOptions {
            fetch: true,
            ..BranchesOptions::default()
        },
    )
    .unwrap();
    assert_eq!(live_view["fetched"], true);
    assert_eq!(find_branch(&live_view, "main")["behind"], 1);
    assert_eq!(find_branch(&live_view, "main")["ahead"], 0);
    assert_eq!(live_view["summary"]["fully_pushed"], true);
    assert!(
        !live_view["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("last fetch")),
        "fetch=true must not carry the staleness warning"
    );
}

#[test]
fn fetch_prunes_a_gone_upstream() {
    let (remote, local) = new_remote_and_local();
    let root = local.path();

    // Push a feature branch with an upstream, then delete it on the remote
    // from another clone.
    git(root, &["checkout", "-qb", "feature-gone"]);
    write(root, "g.txt", "g\n");
    commit_all(root, "gone base");
    git(root, &["push", "-q", "-u", "origin", "feature-gone"]);
    git(root, &["checkout", "-q", "main"]);

    let other = clone_of(remote.path());
    git(other.path(), &["push", "-q", "origin", ":feature-gone"]);

    // fetch=true prunes; the branch's upstream is now gone → ahead/behind
    // null and it counts as no_upstream.
    let result = branches(
        root,
        &BranchesOptions {
            fetch: true,
            ..BranchesOptions::default()
        },
    )
    .unwrap();
    let gone = find_branch(&result, "feature-gone");
    assert_eq!(gone["upstream_gone"], true);
    assert!(gone["ahead"].is_null() && gone["behind"].is_null());
    assert!(names(&result["summary"]["no_upstream"]).contains(&"feature-gone".to_string()));
}
