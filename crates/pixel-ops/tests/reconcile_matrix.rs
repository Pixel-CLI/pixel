//! Integration matrix for `reconcile` (Engine 4: one-call deterministic
//! branch sync). Each of the four classification states gets a real git
//! fixture — a working clone plus a bare "remote" this test independently
//! pushes into via a second/third clone, so the states are genuine, not
//! simulated. See PLAN.md "Engine 4" (~L188-204) and the "One-call sync
//! (scenario 3)" acceptance criteria (~L241) for the contract this proves.
//!
//! Uses a per-test `XDG_STATE_HOME` (same pattern as `crash_matrix.rs`) so
//! the journal/lock state this op writes doesn't leak into the real user
//! state dir, and a process-wide mutex serializes the env-var manipulation.

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use tempfile::TempDir;

use pixel_ops::reconcile::{reconcile, reconcile_with_hooks, ReconcileOptions};

static ENV_GUARD: Mutex<()> = Mutex::new(());

struct XdgEnvGuard;
impl Drop for XdgEnvGuard {
    fn drop(&mut self) {
        // SAFETY: process-local env var, guarded by ENV_GUARD's mutex.
        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
        }
    }
}

fn with_isolated_state<T>(f: impl FnOnce() -> T) -> T {
    // Recover from poison: one test's assertion failure must not cascade
    // into every other test in this file failing with an unrelated
    // PoisonError, which would hide their real (possibly passing) results.
    let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let state_dir = TempDir::new().unwrap();
    // SAFETY: process-local env var, guarded by ENV_GUARD's mutex.
    unsafe {
        std::env::set_var("XDG_STATE_HOME", state_dir.path());
    }
    let _cleanup = XdgEnvGuard;
    f()
}

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

fn git_allow_fail(dir: &Path, args: &[&str]) -> (bool, String, String) {
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
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn write(dir: &Path, name: &str, content: &str) {
    std::fs::write(dir.join(name), content).unwrap();
}

fn commit_all(dir: &Path, msg: &str) -> String {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", msg]);
    git(dir, &["rev-parse", "HEAD"])
}

/// Bare remote + one clone ("local") with an initial commit already pushed.
fn new_remote_and_local() -> (TempDir, TempDir) {
    let remote = TempDir::new().unwrap();
    git(remote.path(), &["init", "-q", "--bare"]);

    let local = TempDir::new().unwrap();
    let out = Command::new("git")
        .args(["clone", "-q"])
        .arg(remote.path())
        .arg(local.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));
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
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    dir
}

fn opts(strategy: &str, push: &str) -> ReconcileOptions {
    ReconcileOptions {
        strategy: strategy.to_string(),
        push: push.to_string(),
        request_id: format!("rec-{}", uuid::Uuid::new_v4()),
    }
}

/// Every commit's parent count via `git log --format=%P`, to assert the
/// "never fabricate a merge commit" invariant end to end.
fn parent_counts(dir: &Path) -> Vec<usize> {
    let out = git(dir, &["log", "--format=%P"]);
    out.lines()
        .map(|l| l.split_whitespace().filter(|s| !s.is_empty()).count())
        .collect()
}

// ---------------------------------------------------------------------------
// (a) up_to_date
// ---------------------------------------------------------------------------

#[test]
fn state_up_to_date_is_a_real_noop() {
    with_isolated_state(|| {
        let (_remote, local) = new_remote_and_local();
        let head_before = git(local.path(), &["rev-parse", "HEAD"]);

        let result = reconcile(local.path(), &opts("report", "none")).unwrap();
        assert_eq!(result["state"], "up_to_date", "result={result}");

        let head_after = git(local.path(), &["rev-parse", "HEAD"]);
        assert_eq!(head_before, head_after, "up_to_date must not move HEAD");
    });
}

// ---------------------------------------------------------------------------
// (b) fast_forwarded
// ---------------------------------------------------------------------------

#[test]
fn state_fast_forward_actually_advances_head_to_match_origin() {
    with_isolated_state(|| {
        let (remote, local) = new_remote_and_local();

        // Push a new commit to the bare remote from a second clone.
        let other = clone_of(remote.path());
        write(other.path(), "remote_only.txt", "from other clone\n");
        let remote_head = commit_all(other.path(), "remote advances");
        git(other.path(), &["push", "-q"]);

        let result = reconcile(local.path(), &opts("report", "none")).unwrap();
        assert_eq!(result["state"], "fast_forwarded", "result={result}");

        let local_head_after = git(local.path(), &["rev-parse", "HEAD"]);
        assert_eq!(
            local_head_after, remote_head,
            "reconcile must actually fast-forward local HEAD to match origin, not just report it"
        );
        assert!(local.path().join("remote_only.txt").exists());

        // Never a merge commit.
        assert!(parent_counts(local.path()).iter().all(|&p| p <= 1));
    });
}

#[test]
fn state_fast_forward_refuses_when_incoming_touches_a_dirty_path() {
    with_isolated_state(|| {
        let (remote, local) = new_remote_and_local();

        // Remote-side commit that touches seed.txt.
        let other = clone_of(remote.path());
        write(other.path(), "seed.txt", "remote changed seed\n");
        commit_all(other.path(), "remote touches seed.txt");
        git(other.path(), &["push", "-q"]);

        // Dirty the SAME file locally (uncommitted).
        write(local.path(), "seed.txt", "locally dirtied seed\n");

        let head_before = git(local.path(), &["rev-parse", "HEAD"]);
        let err = reconcile(local.path(), &opts("report", "none")).unwrap_err();
        assert!(
            err.contains("UNSUPPORTED_STATE") && err.contains("seed.txt"),
            "expected dirty-intersect refusal naming seed.txt, got: {err}"
        );

        // Must not have mutated anything.
        let head_after = git(local.path(), &["rev-parse", "HEAD"]);
        assert_eq!(head_before, head_after);
        let dirty_content = std::fs::read_to_string(local.path().join("seed.txt")).unwrap();
        assert_eq!(dirty_content, "locally dirtied seed\n");
    });
}

// ---------------------------------------------------------------------------
// (c) ahead
// ---------------------------------------------------------------------------

#[test]
fn state_ahead_pushes_with_a_lease_against_the_freshly_fetched_remote_oid() {
    with_isolated_state(|| {
        let (remote, local) = new_remote_and_local();

        write(local.path(), "local_only.txt", "ahead commit\n");
        let local_head = commit_all(local.path(), "local advances, not yet pushed");

        let result = reconcile(local.path(), &opts("report", "auto")).unwrap();
        assert_eq!(result["state"], "pushed", "result={result}");

        // Verify against the REMOTE directly (not just local belief): clone
        // fresh and confirm the commit actually landed.
        let verify = clone_of(remote.path());
        let remote_head = git(verify.path(), &["rev-parse", "origin/main"]);
        assert_eq!(
            remote_head, local_head,
            "reconcile's auto-push must have actually landed the commit on the remote"
        );
    });
}

#[test]
fn state_ahead_lease_race_does_a_single_refetch_and_reclassify_never_a_second_blind_retry() {
    with_isolated_state(|| {
        let (remote, local) = new_remote_and_local();

        write(local.path(), "local_only.txt", "our ahead commit\n");
        commit_all(local.path(), "local advances, not yet pushed");

        // A third clone races a push into the remote in the window between
        // reconcile's own fetch and its push attempt, via the pre_push_hook
        // test seam.
        let racer = clone_of(remote.path());
        let racer_path = racer.path().to_path_buf();
        let hook: Box<dyn FnMut()> = Box::new(move || {
            write(&racer_path, "raced_in.txt", "raced commit\n");
            commit_all(&racer_path, "racer wins the push");
            git(&racer_path, &["push", "-q"]);
        });

        let result =
            reconcile_with_hooks(local.path(), &opts("report", "auto"), Some(hook)).unwrap();

        assert_eq!(result["state"], "push_raced", "result={result}");
        assert!(
            result["push_error"].as_str().is_some_and(|s| !s.is_empty()),
            "result={result}"
        );
        // Reclassified: our own commit still unpushed (ahead) AND the
        // racer's commit now present remotely (behind) => diverged.
        assert_eq!(result["reclassified"]["state"], "diverged", "result={result}");
        assert!(result["reclassified"]["ahead"].as_u64().unwrap() >= 1);
        assert!(result["reclassified"]["behind"].as_u64().unwrap() >= 1);

        // Prove there was no second blind push retry: the remote must NOT
        // have our commit.
        let verify = clone_of(remote.path());
        assert!(
            !verify.path().join("local_only.txt").exists(),
            "a second blind push retry would have landed our commit on the remote"
        );
        assert!(verify.path().join("raced_in.txt").exists());
    });
}

// ---------------------------------------------------------------------------
// (d) diverged — default "report" strategy, conflict report completeness
// ---------------------------------------------------------------------------

#[test]
fn state_diverged_report_never_filters_conflicts_and_separates_non_conflicting_paths() {
    with_isolated_state(|| {
        let (remote, local) = new_remote_and_local();

        // Local: change conflict.txt AND add a local-only file.
        write(local.path(), "conflict.txt", "local version\nsame base line\n");
        write(local.path(), "local_only.txt", "local only\n");
        commit_all(local.path(), "local diverges");

        // Remote: different content in the SAME lines of conflict.txt, from
        // a clone taken before local's commit, plus a remote-only file.
        let other = clone_of(remote.path());
        write(other.path(), "conflict.txt", "remote version\nsame base line\n");
        write(other.path(), "remote_only.txt", "remote only\n");
        commit_all(other.path(), "remote diverges");
        git(other.path(), &["push", "-q"]);

        let head_before = git(local.path(), &["rev-parse", "HEAD"]);
        let result = reconcile(local.path(), &opts("report", "none")).unwrap();

        assert_eq!(result["state"], "diverged", "result={result}");
        assert!(result["ahead"].as_u64().unwrap() >= 1);
        assert!(result["behind"].as_u64().unwrap() >= 1);
        assert_eq!(
            result["clean_rebase_possible"], false,
            "conflict.txt collides on the same line — must not predict a clean rebase"
        );

        // THE regression this op exists to fix: conflicting paths must be
        // PRESENT, not filtered out.
        let conflicts = result["conflicts"].as_array().expect("conflicts array");
        assert!(
            !conflicts.is_empty(),
            "conflicts must not be empty on a genuine conflict — result={result}"
        );
        let paths: Vec<&str> = conflicts.iter().map(|c| c["path"].as_str().unwrap()).collect();
        assert!(paths.contains(&"conflict.txt"), "paths={paths:?}");
        let entry = conflicts.iter().find(|c| c["path"] == "conflict.txt").unwrap();
        assert!(entry["ours"]["oid"].as_str().is_some_and(|s| !s.is_empty()), "entry={entry}");
        assert!(entry["theirs"]["oid"].as_str().is_some_and(|s| !s.is_empty()), "entry={entry}");
        assert!(
            entry["conflict_kind"].as_str().is_some_and(|s| !s.is_empty()),
            "entry={entry}"
        );

        // Non-conflicting paths correctly separated.
        let non_conflicting = &result["non_conflicting"];
        let ours_only: Vec<&str> = non_conflicting["ours_only_paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let theirs_only: Vec<&str> = non_conflicting["theirs_only_paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(ours_only.contains(&"local_only.txt"), "ours_only={ours_only:?}");
        assert!(theirs_only.contains(&"remote_only.txt"), "theirs_only={theirs_only:?}");
        assert!(!ours_only.contains(&"conflict.txt"));
        assert!(!theirs_only.contains(&"conflict.txt"));

        // Default strategy is report-only: no mutation whatsoever.
        let head_after = git(local.path(), &["rev-parse", "HEAD"]);
        assert_eq!(head_before, head_after, "report strategy must never mutate the repo");
        assert!(result["backup_ref"].is_null());
    });
}

// ---------------------------------------------------------------------------
// (e) rebase-if-clean — zero-conflict happy path
// ---------------------------------------------------------------------------

#[test]
fn rebase_if_clean_happy_path_rebases_linearly_backs_up_and_pushes() {
    with_isolated_state(|| {
        let (remote, local) = new_remote_and_local();
        let branch = git(local.path(), &["symbolic-ref", "--short", "HEAD"]);

        // Local and remote diverge on completely different files — no line
        // overlap, so merge-tree must predict a clean merge.
        write(local.path(), "local_change.txt", "local\n");
        commit_all(local.path(), "local diverges cleanly");
        // The backup ref must point at HEAD as it stood right before this
        // call — i.e. after our own divergent commit above, not the
        // fixture's initial commit.
        let pre_rebase_head = git(local.path(), &["rev-parse", "HEAD"]);

        let other = clone_of(remote.path());
        write(other.path(), "remote_change.txt", "remote\n");
        commit_all(other.path(), "remote diverges cleanly");
        git(other.path(), &["push", "-q"]);

        let result = reconcile(local.path(), &opts("rebase-if-clean", "auto")).unwrap();
        assert_eq!(result["state"], "rebased", "result={result}");
        assert_eq!(result["pushed"], true, "result={result}");

        let backup_ref = result["backup_ref"].as_str().expect("backup_ref present").to_string();
        assert_eq!(backup_ref, format!("refs/pixel/reconcile-backup/{branch}"));
        let backup_oid = git(local.path(), &["rev-parse", &backup_ref]);
        assert_eq!(
            backup_oid, pre_rebase_head,
            "backup ref must point at the pre-rebase HEAD"
        );

        // Linear history: every commit has exactly one parent (root commit
        // has zero) — no merge commit was fabricated.
        let parents = parent_counts(local.path());
        assert!(parents.iter().all(|&p| p <= 1), "parents={parents:?}");

        // The leased push actually landed on the remote.
        let verify = clone_of(remote.path());
        let remote_head = git(verify.path(), &["rev-parse", "origin/main"]);
        let local_head = git(local.path(), &["rev-parse", "HEAD"]);
        assert_eq!(remote_head, local_head);
        assert!(verify.path().join("local_change.txt").exists());
        assert!(verify.path().join("remote_change.txt").exists());
    });
}

// ---------------------------------------------------------------------------
// (f) rebase-if-clean — merge-tree correctly predicts a conflict and refuses
//     to attempt the rebase at all (falls back to a diverged report).
// ---------------------------------------------------------------------------

#[test]
fn rebase_if_clean_refuses_to_start_when_merge_tree_predicts_a_conflict() {
    with_isolated_state(|| {
        let (remote, local) = new_remote_and_local();

        // Genuine same-line divergence.
        write(local.path(), "conflict.txt", "local version\nsame base line\n");
        commit_all(local.path(), "local diverges with conflict");
        // Snapshot HEAD right before the call, not the fixture's initial
        // commit — reconcile must leave THIS HEAD untouched.
        let head_before = git(local.path(), &["rev-parse", "HEAD"]);

        let other = clone_of(remote.path());
        write(other.path(), "conflict.txt", "remote version\nsame base line\n");
        commit_all(other.path(), "remote diverges with conflict");
        git(other.path(), &["push", "-q"]);

        let result = reconcile(local.path(), &opts("rebase-if-clean", "auto")).unwrap();

        assert_eq!(result["state"], "diverged", "result={result}");
        assert_eq!(result["clean_rebase_possible"], false, "result={result}");
        let conflicts = result["conflicts"].as_array().expect("conflicts array");
        assert!(!conflicts.is_empty(), "result={result}");

        // Must NOT have started a rebase: HEAD unchanged, no sequencer state
        // left behind.
        let head_after = git(local.path(), &["rev-parse", "HEAD"]);
        assert_eq!(
            head_after, head_before,
            "merge-tree predicted a conflict — reconcile must refuse to attempt the rebase at all"
        );
        assert!(
            !local.path().join(".git/rebase-merge").exists()
                && !local.path().join(".git/rebase-apply").exists(),
            "no rebase sequencer state must be left behind when the rebase was never attempted"
        );

        // Backup ref is still written first (mechanics precede the clean
        // check), and reported.
        let backup_ref = result["backup_ref"].as_str().expect("backup_ref present");
        let (ok, _, _) = git_allow_fail(local.path(), &["rev-parse", "--verify", backup_ref]);
        assert!(ok, "backup ref {backup_ref} must exist");
    });
}

// ---------------------------------------------------------------------------
// request_id wiring regression (pixel-daemon dispatches with "")
// ---------------------------------------------------------------------------

#[test]
fn empty_request_id_from_the_daemon_wiring_does_not_crash_the_op() {
    with_isolated_state(|| {
        let (_remote, local) = new_remote_and_local();
        let result = reconcile(
            local.path(),
            &ReconcileOptions {
                strategy: "report".to_string(),
                push: "none".to_string(),
                request_id: String::new(),
            },
        )
        .unwrap();
        assert_eq!(result["state"], "up_to_date", "result={result}");
    });
}
