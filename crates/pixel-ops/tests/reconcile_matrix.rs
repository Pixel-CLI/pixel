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
    // Point the bare remote's HEAD at main: without init.defaultBranch on
    // the machine, HEAD dangles at refs/heads/master and `clone_of` gets a
    // repo with nothing checked out — commits in those clones then land on
    // `master` instead of `main`, silently defusing every fixture.
    git(remote.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);

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
        into_target: None,
    }
}

fn opts_into(target: &str, push: &str) -> ReconcileOptions {
    ReconcileOptions {
        strategy: "report".to_string(),
        push: push.to_string(),
        request_id: format!("rec-{}", uuid::Uuid::new_v4()),
        into_target: Some(target.to_string()),
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
// (f) rebase-if-clean — merge-tree predicts a conflict, but reconcile now
//     attempts the rebase and auto-resolves additive conflicts via union
//     merge. For genuine same-line conflicts, the union merge produces both
//     lines (structurally valid, semantically union).
// ---------------------------------------------------------------------------

#[test]
fn rebase_if_clean_auto_resolves_when_merge_tree_predicts_a_conflict() {
    with_isolated_state(|| {
        let (remote, local) = new_remote_and_local();

        // Genuine same-line divergence.
        write(local.path(), "conflict.txt", "local version\nsame base line\n");
        commit_all(local.path(), "local diverges with conflict");
        let head_before = git(local.path(), &["rev-parse", "HEAD"]);

        let other = clone_of(remote.path());
        write(other.path(), "conflict.txt", "remote version\nsame base line\n");
        commit_all(other.path(), "remote diverges with conflict");
        git(other.path(), &["push", "-q"]);

        let result = reconcile(local.path(), &opts("rebase-if-clean", "auto")).unwrap();

        // Auto-resolve should produce a rebased state (union merge of both
        // sides' changes). The conflict is additive (both changed line 1
        // differently), so union merge keeps both versions.
        let state = result["state"].as_str().expect("state");
        assert!(
            state == "rebased" || state == "diverged",
            "expected rebased or diverged, got {state}: result={result}"
        );

        if state == "rebased" {
            // Auto-resolve succeeded — HEAD should have changed.
            let head_after = git(local.path(), &["rev-parse", "HEAD"]);
            assert_ne!(
                head_after, head_before,
                "rebase should have moved HEAD after auto-resolve"
            );
            // No sequencer state left behind.
            assert!(
                !local.path().join(".git/rebase-merge").exists()
                    && !local.path().join(".git/rebase-apply").exists(),
                "no rebase sequencer state must remain after successful rebase"
            );
            // The file should contain both versions (union merge).
            let content = std::fs::read_to_string(local.path().join("conflict.txt"))
                .unwrap_or_default();
            assert!(
                content.contains("local version") && content.contains("remote version"),
                "union merge should contain both sides: {content}"
            );
        } else {
            // Auto-resolve failed — fall back to diverged report.
            assert_eq!(result["clean_rebase_possible"], false, "result={result}");
            let conflicts = result["conflicts"].as_array().expect("conflicts array");
            assert!(!conflicts.is_empty(), "result={result}");
        }

        // Backup ref is always written first.
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
                into_target: None,
            },
        )
        .unwrap();
        assert_eq!(result["state"], "up_to_date", "result={result}");
    });
}

// ---------------------------------------------------------------------------
// --push value validation (typos must be structured errors, never silent
// don't-push; "never" is an explicit alias of "none")
// ---------------------------------------------------------------------------

#[test]
fn reconcile_rejects_an_unknown_push_value_with_a_structured_error() {
    with_isolated_state(|| {
        let (_remote, local) = new_remote_and_local();
        // Previously any unrecognized value (e.g. the typo "always") silently
        // behaved as don't-push. It must now fail fast, naming every
        // accepted value.
        let err = reconcile(local.path(), &opts("report", "always"))
            .expect_err("an unknown --push value must be rejected, not silently mean don't-push");
        assert!(
            err.contains("invalid push value"),
            "error must identify the invalid value: {err}"
        );
        assert!(
            err.contains("\"auto\"") && err.contains("\"none\"") && err.contains("\"never\""),
            "error must name the accepted values (auto, none, and the never alias): {err}"
        );
    });
}

#[test]
fn reconcile_accepts_never_as_an_explicit_alias_of_none() {
    with_isolated_state(|| {
        let (_remote, local) = new_remote_and_local();
        // Old rule text documented `--push never`; it must behave exactly
        // like "none" instead of silently falling into the catch-all.
        let result = reconcile(local.path(), &opts("report", "never"))
            .expect("--push never must be accepted as an alias of none");
        assert_eq!(result["state"], "up_to_date", "result={result}");
    });
}

// ---------------------------------------------------------------------------
// (g) --into <target> integration mode: rebase current branch onto
//     origin/<target>, then fast-forward the LOCAL <target> ref to the
//     rebased head. Never force, never a merge commit.
// ---------------------------------------------------------------------------

#[test]
fn into_clean_integration_rebases_feature_fast_forwards_target_and_pushes_both() {
    with_isolated_state(|| {
        let (remote, local) = new_remote_and_local();

        // develop branches off main and is pushed.
        git(local.path(), &["checkout", "-qb", "develop"]);
        write(local.path(), "develop_seed.txt", "develop\n");
        commit_all(local.path(), "develop seed");
        git(local.path(), &["push", "-q", "-u", "origin", "develop"]);

        // feature branches off develop with its own commit — never pushed,
        // so the leased feature push must create the remote branch.
        git(local.path(), &["checkout", "-qb", "feature/x"]);
        write(local.path(), "feature.txt", "feature\n");
        commit_all(local.path(), "feature work");

        // origin/develop advances from another clone (disjoint file — clean).
        let other = clone_of(remote.path());
        git(other.path(), &["checkout", "-q", "develop"]);
        write(other.path(), "remote_dev.txt", "remote develop\n");
        let remote_dev_head = commit_all(other.path(), "remote develop advances");
        git(other.path(), &["push", "-q"]);

        let result = reconcile(local.path(), &opts_into("develop", "auto")).unwrap();
        assert_eq!(result["state"], "integrated", "result={result}");
        assert_eq!(result["pushed"], true, "result={result}");
        assert_eq!(result["into"]["target"], "develop", "result={result}");
        assert_eq!(result["into"]["target_pushed"], true, "result={result}");

        let new_head = git(local.path(), &["rev-parse", "HEAD"]);
        assert_eq!(
            result["into"]["target_new_oid"].as_str().unwrap(),
            new_head,
            "result={result}"
        );

        // Local develop was fast-forwarded to the rebased feature head.
        let dev_oid = git(local.path(), &["rev-parse", "refs/heads/develop"]);
        assert_eq!(dev_oid, new_head, "local develop must be ff'd to the rebased head");

        // The rebased head contains the remote develop advance (a true
        // rebase onto origin/develop, not a stale-base replay).
        let (is_anc, _, _) = git_allow_fail(
            local.path(),
            &["merge-base", "--is-ancestor", &remote_dev_head, &new_head],
        );
        assert!(is_anc, "rebased head must descend from the fetched origin/develop tip");

        // Still on the feature branch; linear history — no merge commit.
        assert_eq!(git(local.path(), &["symbolic-ref", "--short", "HEAD"]), "feature/x");
        assert!(parent_counts(local.path()).iter().all(|&p| p <= 1));

        // Both pushes actually landed on the remote — verified via a fresh
        // clone, not local belief.
        let verify = clone_of(remote.path());
        assert_eq!(git(verify.path(), &["rev-parse", "origin/develop"]), new_head);
        assert_eq!(git(verify.path(), &["rev-parse", "origin/feature/x"]), new_head);
        git(verify.path(), &["checkout", "-q", "develop"]);
        assert!(verify.path().join("feature.txt").exists());
        assert!(verify.path().join("remote_dev.txt").exists());
    });
}

#[test]
fn into_integration_ignores_untracked_sidecar_dirt() {
    // Regression guard for the `--into` clean-worktree gate: untracked
    // `.pixel/` (current) and `.gitpixel/` (legacy) sidecar artifacts are
    // daemon-generated, never rebased, and must not refuse the integration.
    // Only the rebase-if-clean path had coverage; this pins the `--into`
    // path against a refactor of `dirty_excluding_sidecar`.
    with_isolated_state(|| {
        let (remote, local) = new_remote_and_local();

        git(local.path(), &["checkout", "-qb", "develop"]);
        write(local.path(), "develop_seed.txt", "develop\n");
        commit_all(local.path(), "develop seed");
        git(local.path(), &["push", "-q", "-u", "origin", "develop"]);

        git(local.path(), &["checkout", "-qb", "feature/x"]);
        write(local.path(), "feature.txt", "feature\n");
        commit_all(local.path(), "feature work");

        let other = clone_of(remote.path());
        git(other.path(), &["checkout", "-q", "develop"]);
        write(other.path(), "remote_dev.txt", "remote develop\n");
        commit_all(other.path(), "remote develop advances");
        git(other.path(), &["push", "-q"]);

        // Simulate daemon sidecar writes — both generations.
        std::fs::create_dir_all(local.path().join(".pixel")).unwrap();
        write(local.path(), ".pixel/actions.jsonl", "{}\n");
        std::fs::create_dir_all(local.path().join(".gitpixel")).unwrap();
        write(local.path(), ".gitpixel/index", "legacy\n");

        let result = reconcile(local.path(), &opts_into("develop", "none")).unwrap();
        assert_eq!(
            result["state"], "integrated",
            "untracked sidecar dirt must not refuse --into: {result}"
        );
    });
}

#[test]
fn into_auto_resolves_conflict_and_rebases() {
    with_isolated_state(|| {
        let (remote, local) = new_remote_and_local();

        git(local.path(), &["checkout", "-qb", "develop"]);
        git(local.path(), &["push", "-q", "-u", "origin", "develop"]);

        git(local.path(), &["checkout", "-qb", "feature/x"]);
        write(local.path(), "conflict.txt", "feature version\nsame base line\n");
        commit_all(local.path(), "feature conflicting");
        let head_before = git(local.path(), &["rev-parse", "HEAD"]);
        let dev_before = git(local.path(), &["rev-parse", "refs/heads/develop"]);

        let other = clone_of(remote.path());
        git(other.path(), &["checkout", "-q", "develop"]);
        write(other.path(), "conflict.txt", "remote version\nsame base line\n");
        commit_all(other.path(), "remote develop conflicting");
        git(other.path(), &["push", "-q"]);

        let result = reconcile(local.path(), &opts_into("develop", "auto")).unwrap();
        let state = result["state"].as_str().expect("state");
        assert_eq!(result["into_target"], "develop", "report must name the target: {result}");

        if state == "rebased" {
            // Auto-resolve succeeded — HEAD moved, no sequencer state.
            assert_ne!(
                git(local.path(), &["rev-parse", "HEAD"]), head_before,
                "rebase should have moved HEAD"
            );
            assert!(
                !local.path().join(".git/rebase-merge").exists()
                    && !local.path().join(".git/rebase-apply").exists(),
                "no rebase sequencer state must remain"
            );
        } else {
            // Auto-resolve failed — diverged report with conflict detail.
            assert_eq!(state, "diverged", "result={result}");
            assert_eq!(result["clean_rebase_possible"], false, "result={result}");
            let conflicts = result["conflicts"].as_array().expect("conflicts array");
            assert!(!conflicts.is_empty(), "conflicts must be reported: {result}");
            let paths: Vec<&str> = conflicts.iter().map(|c| c["path"].as_str().unwrap()).collect();
            assert!(paths.contains(&"conflict.txt"), "paths={paths:?}");
            // No mutation: feature HEAD unmoved, local develop unmoved.
            assert_eq!(git(local.path(), &["rev-parse", "HEAD"]), head_before);
            assert_eq!(git(local.path(), &["rev-parse", "refs/heads/develop"]), dev_before);
        }
    });
}

#[test]
fn into_refuses_when_local_target_is_not_an_ancestor_of_remote_target() {
    with_isolated_state(|| {
        let (remote, local) = new_remote_and_local();

        git(local.path(), &["checkout", "-qb", "develop"]);
        git(local.path(), &["push", "-q", "-u", "origin", "develop"]);
        // Local-only commit on develop → develop diverges once the remote
        // also advances. The ff of a diverged target would be a forced move,
        // which --into must refuse outright.
        write(local.path(), "local_dev_only.txt", "local develop\n");
        commit_all(local.path(), "local develop diverges");
        let dev_before = git(local.path(), &["rev-parse", "refs/heads/develop"]);

        git(local.path(), &["checkout", "-qb", "feature/x"]);
        write(local.path(), "feature.txt", "feature\n");
        commit_all(local.path(), "feature work");
        let head_before = git(local.path(), &["rev-parse", "HEAD"]);

        let other = clone_of(remote.path());
        git(other.path(), &["checkout", "-q", "develop"]);
        write(other.path(), "remote_dev.txt", "remote develop\n");
        commit_all(other.path(), "remote develop advances");
        git(other.path(), &["push", "-q"]);

        let err = reconcile(local.path(), &opts_into("develop", "auto")).unwrap_err();
        assert!(
            err.contains("NON_FAST_FORWARD") && err.contains("develop"),
            "expected structured non-ff refusal naming the target: {err}"
        );

        // The refusal precedes ALL mutation: nothing moved, no rebase ran.
        assert_eq!(git(local.path(), &["rev-parse", "HEAD"]), head_before);
        assert_eq!(git(local.path(), &["rev-parse", "refs/heads/develop"]), dev_before);
    });
}

#[test]
fn into_refuses_when_target_is_the_current_branch() {
    with_isolated_state(|| {
        let (_remote, local) = new_remote_and_local();
        git(local.path(), &["checkout", "-qb", "develop"]);
        let err = reconcile(local.path(), &opts_into("develop", "none")).unwrap_err();
        assert!(
            err.contains("UNSUPPORTED_STATE") && err.contains("develop"),
            "expected target==current refusal: {err}"
        );
    });
}
