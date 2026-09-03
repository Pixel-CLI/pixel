//! Integration matrix for `rewrite` (squash-to-one-commit with optional
//! leased push). Real git fixtures: a working clone plus a bare "remote"
//! that a second clone independently pushes into, so lease races are
//! genuine, not simulated (same conventions as `reconcile_matrix.rs` /
//! `crash_matrix.rs`).
//!
//! State isolation: every test passes its own tempdir state root through
//! `rewrite_with_state`, so journal/lock state never leaks into the real
//! user state dir (same seam `publish_with_state` tests use).

use std::path::Path;
use std::process::Command;

use serde_json::json;
use tempfile::TempDir;

use pixel_ops::rewrite::{rewrite_with_state, RewriteOptions};

// ---------------------------------------------------------------------------
// git fixture helpers
// ---------------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> String {
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
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit_file(dir: &Path, name: &str, content: &str, msg: &str) {
    std::fs::write(dir.join(name), content).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", msg]);
}

/// Working clone on branch `feat` (3 messy commits on top of a pushed
/// `main`), bare remote with `origin/HEAD -> main`, and `feat` pushed at
/// its base state when `push_feat_base` is set.
struct Fixture {
    work: TempDir,
    remote: TempDir,
    state: TempDir,
}

fn fixture(push_feat: bool) -> Fixture {
    let work = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();

    git(work.path(), &["init", "-q", "-b", "main", "."]);
    git(work.path(), &["config", "user.email", "t@t"]);
    git(work.path(), &["config", "user.name", "t"]);
    commit_file(work.path(), "base.txt", "base", "base");
    git(remote.path(), &["init", "--bare", "-q", "."]);
    git(work.path(), &["remote", "add", "origin", remote.path().to_str().unwrap()]);
    git(work.path(), &["push", "-qu", "origin", "main"]);
    // Make the default branch detectable via origin/HEAD.
    git(work.path(), &["remote", "set-head", "origin", "main"]);

    git(work.path(), &["checkout", "-qb", "feat"]);
    if push_feat {
        git(work.path(), &["push", "-qu", "origin", "feat"]);
    }
    commit_file(work.path(), "one.txt", "1", "wip one");
    commit_file(work.path(), "two.txt", "2", "wip two");
    commit_file(work.path(), "three.txt", "3", "wip three");

    Fixture { work, remote, state }
}

fn opts(onto: Option<&str>, push: bool) -> RewriteOptions {
    RewriteOptions {
        onto: onto.map(|s| s.to_string()),
        message: None,
        push,
        remote: "origin".to_string(),
        request_id: format!("rw-{}", uuid::Uuid::new_v4()),
        expected_head: None,
        allow_default_branch: false,
    }
}

// ---------------------------------------------------------------------------
// basic squash
// ---------------------------------------------------------------------------

#[test]
fn rewrite_squashes_three_commits_into_one() {
    let fx = fixture(true); // upstream = origin/feat at base
    let old_head = git(fx.work.path(), &["rev-parse", "HEAD"]);

    let result = rewrite_with_state(fx.work.path(), &opts(None, false), None, fx.state.path()).unwrap();

    assert_eq!(result["state"], json!("squashed"));
    assert_eq!(result["commits_squashed"], json!(3));
    assert_eq!(result["branch"], json!("feat"));
    assert_eq!(result["old_head"], json!(old_head));
    assert_eq!(result["pushed"], json!(false));

    // Exactly ONE commit on feat past the base now.
    let base = result["base_oid"].as_str().unwrap();
    let count = git(fx.work.path(), &["rev-list", "--count", &format!("{base}..HEAD")]);
    assert_eq!(count, "1");

    // Tree content of all three commits survives.
    for f in ["one.txt", "two.txt", "three.txt"] {
        assert!(fx.work.path().join(f).exists(), "{f} lost in squash");
    }

    // Auto-generated message lists the squashed subjects.
    let msg = git(fx.work.path(), &["log", "-1", "--format=%B"]);
    assert!(msg.starts_with("squash: 3 commits"), "unexpected message: {msg}");
    for s in ["wip one", "wip two", "wip three"] {
        assert!(msg.contains(s), "message missing subject {s:?}: {msg}");
    }
}

#[test]
fn rewrite_explicit_onto_base_is_used() {
    let fx = fixture(false); // no upstream for feat
    let main_oid = git(fx.work.path(), &["rev-parse", "main"]);

    let result =
        rewrite_with_state(fx.work.path(), &opts(Some("main"), false), None, fx.state.path()).unwrap();

    assert_eq!(result["state"], json!("squashed"));
    assert_eq!(result["base_oid"], json!(main_oid));
    assert_eq!(result["commits_squashed"], json!(3));
    let count = git(fx.work.path(), &["rev-list", "--count", &format!("{main_oid}..HEAD")]);
    assert_eq!(count, "1");
}

#[test]
fn rewrite_custom_message_is_used_verbatim() {
    let fx = fixture(true);
    let mut o = opts(None, false);
    o.message = Some("feat: the whole thing".to_string());
    rewrite_with_state(fx.work.path(), &o, None, fx.state.path()).unwrap();
    let msg = git(fx.work.path(), &["log", "-1", "--format=%s"]);
    assert_eq!(msg, "feat: the whole thing");
}

// ---------------------------------------------------------------------------
// backup ref
// ---------------------------------------------------------------------------

#[test]
fn rewrite_backup_ref_points_at_old_head() {
    let fx = fixture(true);
    let old_head = git(fx.work.path(), &["rev-parse", "HEAD"]);

    let result = rewrite_with_state(fx.work.path(), &opts(None, false), None, fx.state.path()).unwrap();

    assert_eq!(result["backup_ref"], json!("refs/pixel/rewrite-backup/feat"));
    let backup = git(fx.work.path(), &["rev-parse", "refs/pixel/rewrite-backup/feat"]);
    assert_eq!(backup, old_head, "backup ref must point at the pre-rewrite head");
}

// ---------------------------------------------------------------------------
// refusals
// ---------------------------------------------------------------------------

#[test]
fn rewrite_refuses_default_branch_without_onto() {
    let fx = fixture(false);
    git(fx.work.path(), &["checkout", "-q", "main"]);
    commit_file(fx.work.path(), "m.txt", "m", "unpushed on main");

    let err =
        rewrite_with_state(fx.work.path(), &opts(None, false), None, fx.state.path()).unwrap_err();
    assert!(err.contains("REFUSED"), "expected REFUSED, got: {err}");
    assert!(err.contains("default branch"), "expected default-branch reason, got: {err}");
}

#[test]
fn rewrite_refuses_when_base_not_ancestor() {
    let fx = fixture(false);
    // A branch diverged from main — NOT an ancestor of feat's HEAD.
    git(fx.work.path(), &["checkout", "-qb", "other", "main"]);
    commit_file(fx.work.path(), "other.txt", "o", "other");
    let other_oid = git(fx.work.path(), &["rev-parse", "HEAD"]);
    git(fx.work.path(), &["checkout", "-q", "feat"]);

    let err = rewrite_with_state(fx.work.path(), &opts(Some(&other_oid), false), None, fx.state.path())
        .unwrap_err();
    assert!(err.contains("not an ancestor"), "expected ancestor refusal, got: {err}");
}

#[test]
fn rewrite_refuses_squashing_commits_already_on_remote_default() {
    let fx = fixture(false);
    // Overlap case: advance main by one commit and push it, then create
    // feat2 FROM the new main tip, add a wip commit, and attempt to squash
    // --onto <old main> — the pushed main-tip commit lands inside the
    // squash range, which must be refused (published mainline).
    git(fx.work.path(), &["checkout", "-q", "main"]);
    commit_file(fx.work.path(), "m2.txt", "m2", "published main commit");
    git(fx.work.path(), &["push", "-q", "origin", "main"]);
    let old_main = git(fx.work.path(), &["rev-parse", "main~1"]);
    git(fx.work.path(), &["checkout", "-qb", "feat2"]);
    commit_file(fx.work.path(), "f2.txt", "f2", "feat2 wip");

    let err = rewrite_with_state(
        fx.work.path(),
        &opts(Some(&old_main), false),
        None,
        fx.state.path(),
    )
    .unwrap_err();
    assert!(
        err.contains("REFUSED") && err.contains("already contained"),
        "expected published-mainline refusal, got: {err}"
    );
}

#[test]
fn rewrite_allow_default_branch_overrides_default_branch_refusal() {
    let fx = fixture(false);
    git(fx.work.path(), &["checkout", "-q", "main"]);
    commit_file(fx.work.path(), "m.txt", "m", "unpushed on main");

    let mut o = opts(None, false);
    o.allow_default_branch = true;
    let res =
        rewrite_with_state(fx.work.path(), &o, None, fx.state.path());
    assert!(res.is_ok(), "allow_default_branch should override default-branch refusal, got: {res:?}");
}

#[test]
fn rewrite_allow_default_branch_overrides_published_mainline_refusal() {
    let fx = fixture(false);
    git(fx.work.path(), &["checkout", "-q", "main"]);
    commit_file(fx.work.path(), "m2.txt", "m2", "published main commit");
    git(fx.work.path(), &["push", "-q", "origin", "main"]);
    let old_main = git(fx.work.path(), &["rev-parse", "main~1"]);
    git(fx.work.path(), &["checkout", "-qb", "feat2"]);
    commit_file(fx.work.path(), "f2.txt", "f2", "feat2 wip");

    let mut o = opts(Some(&old_main), false);
    o.allow_default_branch = true;
    let res = rewrite_with_state(
        fx.work.path(),
        &o,
        None,
        fx.state.path(),
    );
    assert!(res.is_ok(), "allow_default_branch should override published-mainline refusal, got: {res:?}");
}

#[test]
fn rewrite_refuses_detached_head() {
    let fx = fixture(false);
    let head = git(fx.work.path(), &["rev-parse", "HEAD"]);
    git(fx.work.path(), &["checkout", "-q", &head]);

    let err =
        rewrite_with_state(fx.work.path(), &opts(Some("main"), false), None, fx.state.path())
            .unwrap_err();
    assert!(err.contains("detached HEAD"), "expected detached-HEAD refusal, got: {err}");
}

#[test]
fn rewrite_refuses_stale_expected_head() {
    let fx = fixture(true);
    let mut o = opts(None, false);
    o.expected_head = Some("0".repeat(40));
    let err = rewrite_with_state(fx.work.path(), &o, None, fx.state.path()).unwrap_err();
    assert!(err.contains("STALE_STATE"), "expected STALE_STATE, got: {err}");
}

#[test]
fn rewrite_errors_when_nothing_to_squash() {
    let fx = fixture(false);
    // Base == HEAD.
    let head = git(fx.work.path(), &["rev-parse", "HEAD"]);
    let err = rewrite_with_state(fx.work.path(), &opts(Some(&head), false), None, fx.state.path())
        .unwrap_err();
    assert!(err.contains("NOTHING_TO_SQUASH"), "expected NOTHING_TO_SQUASH, got: {err}");
}

// ---------------------------------------------------------------------------
// dirty worktree warning
// ---------------------------------------------------------------------------

#[test]
fn rewrite_dirty_worktree_tolerated_with_warning() {
    let fx = fixture(true);
    std::fs::write(fx.work.path().join("uncommitted.txt"), "dirty").unwrap();

    let result = rewrite_with_state(fx.work.path(), &opts(None, false), None, fx.state.path()).unwrap();
    assert_eq!(result["state"], json!("squashed"));
    let warnings = result["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap().contains("dirty")),
        "expected a dirty-worktree warning, got: {warnings:?}"
    );
    // The dirty file survives, untouched and uncommitted.
    assert_eq!(std::fs::read_to_string(fx.work.path().join("uncommitted.txt")).unwrap(), "dirty");
    let show = Command::new("git")
        .arg("-C")
        .arg(fx.work.path())
        .args(["cat-file", "-e", "HEAD:uncommitted.txt"])
        .output()
        .unwrap();
    assert!(!show.status.success(), "uncommitted file must not be swept into the squash commit");
}

// ---------------------------------------------------------------------------
// leased push
// ---------------------------------------------------------------------------

#[test]
fn rewrite_leased_push_succeeds_against_bare_remote() {
    let fx = fixture(true); // feat pushed at base; 3 local wip commits on top
    // Push the messy state so the remote holds the pre-rewrite tip.
    git(fx.work.path(), &["push", "-q", "origin", "feat"]);

    let result = rewrite_with_state(fx.work.path(), &opts(Some("main"), true), None, fx.state.path())
        .unwrap();

    assert_eq!(result["state"], json!("squashed"));
    assert_eq!(result["pushed"], json!(true), "push_error: {:?}", result["push_error"]);

    // The bare remote's feat now IS the squash commit.
    let remote_feat = git(fx.remote.path(), &["rev-parse", "refs/heads/feat"]);
    assert_eq!(json!(remote_feat), result["new_head"]);
    let base = result["base_oid"].as_str().unwrap();
    let count = git(fx.remote.path(), &["rev-list", "--count", &format!("{base}..refs/heads/feat")]);
    assert_eq!(count, "1");
}

#[test]
fn rewrite_leased_push_fails_when_remote_moved() {
    let fx = fixture(true);
    git(fx.work.path(), &["push", "-q", "origin", "feat"]);

    // A second clone advances feat on the remote AFTER our remote-tracking
    // ref was last updated — the classic lease-protection scenario.
    let intruder = TempDir::new().unwrap();
    git(intruder.path(), &["clone", "-q", fx.remote.path().to_str().unwrap(), "."]);
    git(intruder.path(), &["config", "user.email", "i@i"]);
    git(intruder.path(), &["config", "user.name", "i"]);
    git(intruder.path(), &["checkout", "-q", "feat"]);
    commit_file(intruder.path(), "intruder.txt", "x", "someone else pushed");
    git(intruder.path(), &["push", "-q", "origin", "feat"]);
    let intruder_tip = git(intruder.path(), &["rev-parse", "HEAD"]);

    let result = rewrite_with_state(fx.work.path(), &opts(Some("main"), true), None, fx.state.path())
        .unwrap();

    // The local squash succeeded, but the leased push was refused and
    // classified — never retried with plain --force.
    assert_eq!(result["state"], json!("squashed"));
    assert_eq!(result["pushed"], json!(false));
    let push_error = result["push_error"].as_str().unwrap();
    assert!(push_error.contains("STALE_REMOTE"), "expected STALE_REMOTE, got: {push_error}");

    // The intruder's commit is still the remote tip — nothing was clobbered.
    let remote_feat = git(fx.remote.path(), &["rev-parse", "refs/heads/feat"]);
    assert_eq!(remote_feat, intruder_tip, "lease must have protected the remote");
}

// ---------------------------------------------------------------------------
// crash recovery + idempotent replay
// ---------------------------------------------------------------------------

#[test]
fn rewrite_crash_in_reset_commit_window_restores_from_backup() {
    let fx = fixture(true);
    let old_head = git(fx.work.path(), &["rev-parse", "HEAD"]);
    let o = opts(None, false);

    // Crash right after the soft reset, before the squash commit.
    let probe: pixel_ops::rewrite::RewriteProbe = Box::new(|phase: &str| {
        if phase == "reset:done" {
            Err("simulated crash after reset".to_string())
        } else {
            Ok(())
        }
    });
    let err = rewrite_with_state(fx.work.path(), &o, Some(probe), fx.state.path()).unwrap_err();
    assert!(err.contains("simulated crash"), "unexpected: {err}");

    // Resume with the SAME request_id → restoration from the journaled
    // backup metadata, structured GIT_FAILED.
    let err2 = rewrite_with_state(fx.work.path(), &o, None, fx.state.path()).unwrap_err();
    assert!(err2.contains("GIT_FAILED"), "expected GIT_FAILED, got: {err2}");
    assert!(err2.contains(&old_head), "restore message names the restored head: {err2}");

    // The branch is back exactly where it started; backup ref intact.
    assert_eq!(git(fx.work.path(), &["rev-parse", "HEAD"]), old_head);
    assert_eq!(git(fx.work.path(), &["rev-parse", "refs/pixel/rewrite-backup/feat"]), old_head);
    // Worktree content intact.
    for f in ["one.txt", "two.txt", "three.txt"] {
        assert!(fx.work.path().join(f).exists(), "{f} lost across crash+restore");
    }
    // Nothing left staged relative to the restored HEAD.
    let staged = git(fx.work.path(), &["diff", "--cached", "--name-only"]);
    assert_eq!(staged, "", "index must match the restored head");
}

#[test]
fn rewrite_replay_is_idempotent() {
    let fx = fixture(true);
    let o = opts(None, false);
    let r1 = rewrite_with_state(fx.work.path(), &o, None, fx.state.path()).unwrap();
    let r2 = rewrite_with_state(fx.work.path(), &o, None, fx.state.path()).unwrap();
    assert_eq!(r1["new_head"], r2["new_head"]);
    // No second squash happened.
    let base = r1["base_oid"].as_str().unwrap();
    let count = git(fx.work.path(), &["rev-list", "--count", &format!("{base}..HEAD")]);
    assert_eq!(count, "1");
}
