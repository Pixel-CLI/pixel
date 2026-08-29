//! Crash matrix tests for `publish` and `push`.
//!
//! Ports usable-git's crash matrix to Rust: inject a crash at each journal
//! phase via the probe hook, retry the operation, then verify the safety
//! invariants:
//!
//!   * Unrelated worktree state (staged / unstaged / loose) is preserved.
//!   * `git fsck --full` is clean on every repo involved.
//!   * No work is lost (all file contents survive the crash + recovery).
//!   * The resulting commit contains *exactly* the requested files.
//!   * For push: non-target remote refs are untouched, and a crash at
//!     `journal:push_started` surfaces `NETWORK_AMBIGUITY` rather than a
//!     silent retry.
//!
//! The probe hook (`Option<PublishProbe>` / `Option<PushProbe>`) is called at
//! each phase; returning `Err` simulates a process crash. The retry call
//! passes `None` so the operation journal's `begin` → Resume/Replay path
//! drives recovery.
//!
//! NOTE: `publish`/`push` resume from `JournalPhase::Started` by re-invoking
//! the top-level `publish()`/`push()` which uses the process-global state
//! root. To keep the journal store consistent across the crash and the retry,
//! each test points `XDG_STATE_HOME` at a per-test tempdir. A process-wide
//! `Mutex` serializes tests so the env-var manipulation is race-free.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use serde_json::json;
use tempfile::TempDir;

use pixel_ops::publish::{publish_with_state, PublishOptions, PublishProbe};
use pixel_ops::push::{push_with_state, PushOptions, PushProbe};

// ---------------------------------------------------------------------------
// Serialization + env-var guard
// ---------------------------------------------------------------------------

/// Serializes tests that mutate the process-global `XDG_STATE_HOME` env var.
/// Without this, parallel `#[test]` threads would race on the env var and
/// corrupt each other's state root / journal store.
static ENV_GUARD: Mutex<()> = Mutex::new(());

/// Removes `XDG_STATE_HOME` on drop so the env var never leaks across tests,
/// even on panic.
struct XdgEnvGuard;
impl Drop for XdgEnvGuard {
    fn drop(&mut self) {
        // SAFETY: `XDG_STATE_HOME` is a process-local env var; removing it is
        // not memory-unsafe. The `ENV_GUARD` mutex serializes access.
        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
        }
    }
}

/// Lock the env guard + set `XDG_STATE_HOME` to `state_dir`. Returns an
/// `(MutexGuard, XdgEnvGuard)` pair that must be held for the test lifetime.
fn lock_env(state_dir: &Path) -> (std::sync::MutexGuard<'static, ()>, XdgEnvGuard) {
    // SAFETY: `ENV_GUARD` serializes all callers, so the env-var write is not
    // racy with other tests in this binary.
    let guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::set_var("XDG_STATE_HOME", state_dir);
    }
    // Transmute the lifetime to 'static: the guard is dropped at fn end, same
    // as the XdgEnvGuard, so the borrow is valid for the entire test body.
    (guard, XdgEnvGuard)
}

// ---------------------------------------------------------------------------
// Probe factory — inject a crash at exactly one phase
// ---------------------------------------------------------------------------

/// Build a probe that returns `Err("CRASH@<target>")` the first (and every)
/// time `<target>` is reached, and `Ok(())` for all other phases.
///
/// The retry call passes `None`, so the probe only fires during the crash run.
fn crash_probe(target: &str) -> Box<dyn FnMut(&str) -> Result<(), String>> {
    let t = target.to_string();
    Box::new(move |phase: &str| {
        if phase == t {
            Err(format!("CRASH@{t}"))
        } else {
            Ok(())
        }
    })
}

// ---------------------------------------------------------------------------
// git helpers
// ---------------------------------------------------------------------------

/// Run `git -C <root> <args...>` and return stdout. Panics on non-zero exit.
fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {:?}: {e}", args));
    if !output.status.success() {
        panic!(
            "git -C {} {:?} failed (exit {:?})\nstdout: {}\nstderr: {}",
            root.display(),
            args,
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Initialize a repo with a base commit on `main`.
fn init_repo(root: &Path) {
    Command::new("git")
        .args(["init", "-q", root.to_str().unwrap()])
        .status()
        .unwrap();
    git(root, &["config", "user.email", "crash-matrix@pixel"]);
    git(root, &["config", "user.name", "Crash Matrix"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    std::fs::write(root.join("base.txt"), b"base").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
}

/// Initialize a repo + a bare remote named `origin`, and push the base commit
/// so the remote has `refs/heads/main`.
fn init_repo_with_remote(root: &Path, remote: &Path) {
    init_repo(root);
    Command::new("git")
        .args(["init", "--bare", "-q", remote.to_str().unwrap()])
        .status()
        .unwrap();
    git(remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(root, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(root, &["push", "-q", "origin", "main"]);
}

// ---------------------------------------------------------------------------
// Verification helpers
// ---------------------------------------------------------------------------

/// Assert `git fsck --full` exits clean with no corruption indicators.
///
/// `dangling` notices are tolerated (normal after reset/commit); only
/// `error`, `broken`, and `missing` indicate real corruption.
fn assert_fsck_clean(root: &Path) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["fsck", "--full"])
        .output()
        .unwrap_or_else(|e| panic!("git fsck on {}: {e}", root.display()));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.status.success(),
        "git fsck failed for {}: {combined}",
        root.display(),
    );
    assert!(
        !combined.contains("error")
            && !combined.contains("broken")
            && !combined.contains("missing"),
        "git fsck reported corruption for {}: {combined}",
        root.display(),
    );
}

/// Unrelated worktree state set up before the operation under test.
struct UnrelatedState {
    /// `(path, expected_content)` for files that must survive the crash.
    files: Vec<(&'static str, &'static [u8])>,
}

/// Set up three kinds of unrelated changes that must survive a crash:
///
///   * `staged.txt`  — new file, staged via `git add` (not committed).
///   * `base.txt`    — committed in base, then modified (unstaged change).
///   * `loose.txt`   — untracked (loose) file.
fn setup_unrelated_changes(root: &Path) -> UnrelatedState {
    // staged: new file added to the index but not committed.
    std::fs::write(root.join("staged.txt"), b"staged-content").unwrap();
    git(root, &["add", "staged.txt"]);

    // unstaged: modify a tracked, committed file without staging.
    std::fs::write(root.join("base.txt"), b"base-modified").unwrap();

    // loose: untracked file.
    std::fs::write(root.join("loose.txt"), b"loose-content").unwrap();

    UnrelatedState {
        files: vec![
            ("staged.txt", b"staged-content" as &[u8]),
            ("base.txt", b"base-modified" as &[u8]),
            ("loose.txt", b"loose-content" as &[u8]),
        ],
    }
}

/// Verify every unrelated file still exists with its original content.
///
/// This checks *worktree* state (file contents), which is the safety-critical
/// property: no user work is destroyed. The index/staged status may legitimately
/// change during recovery (e.g. `git reset` on resume from `IndexStaged`), but
/// file contents must never be lost.
fn verify_unrelated_preserved(root: &Path, state: &UnrelatedState) {
    for (path, expected) in &state.files {
        let actual = std::fs::read(root.join(path))
            .unwrap_or_else(|e| panic!("unrelated file {path} lost: {e}"));
        assert_eq!(
            actual,
            *expected,
            "unrelated file {path} content changed — work lost during crash recovery",
        );
    }
}

/// Return the sorted list of file paths changed in the HEAD commit.
fn commit_files_in_head(root: &Path) -> Vec<String> {
    let out = git(root, &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"]);
    let mut files: Vec<String> = out
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    files.sort();
    files
}

/// Return `BTreeMap<refname, objectname>` for all refs in a repo (or bare remote).
fn list_refs(repo: &Path) -> BTreeMap<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["for-each-ref", "--format=%(refname) %(objectname)"])
        .output()
        .unwrap_or_else(|e| panic!("git for-each-ref on {}: {e}", repo.display()));
    let s = String::from_utf8_lossy(&out.stdout).to_string();
    s.lines()
        .filter_map(|l| {
            let mut parts = l.splitn(2, ' ');
            let name = parts.next()?.trim().to_string();
            let oid = parts.next()?.trim().to_string();
            if name.is_empty() || oid.is_empty() {
                return None;
            }
            Some((name, oid))
        })
        .collect()
}

/// Verify that every non-target ref in `before` is unchanged in `after`.
/// `target_ref` (e.g. `refs/heads/main`) is excluded — it is the ref the push
/// operation is allowed to update.
fn verify_non_target_refs_untouched(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    target_ref: &str,
    phase: &str,
) {
    for (name, oid) in before {
        if name == target_ref {
            continue;
        }
        let actual = after.get(name).unwrap_or_else(|| {
            panic!("phase {phase}: non-target ref {name} disappeared from remote")
        });
        assert_eq!(
            actual, oid,
            "phase {phase}: non-target ref {name} changed ({oid} -> {actual})",
        );
    }
}

// ===========================================================================
// Publish crash matrix
// ===========================================================================

/// Phases at which the publish probe fires (in execution order):
///
///   journal:started
///     → recovery:snapshotted
///     → journal:index_staged
///     → recovery:commit_started
///     → journal:commit_observed
///     → journal:terminal
const PUBLISH_PHASES: &[&str] = &[
    "journal:started",
    "recovery:snapshotted",
    "journal:index_staged",
    "recovery:commit_started",
    "journal:commit_observed",
    "journal:terminal",
];

#[test]
fn publish_crash_matrix() {
    for &phase in PUBLISH_PHASES {
        eprintln!("publish crash matrix: phase = {phase}");
        publish_crash_at_phase(phase);
    }
}

fn publish_crash_at_phase(phase: &str) {
    let repo_dir = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();
    let state_root = state_dir.path().join("pixel");
    std::fs::create_dir_all(&state_root).unwrap();

    let (_env_guard, _xdg) = lock_env(state_dir.path());

    let root = repo_dir.path();
    init_repo(root);
    let unrelated = setup_unrelated_changes(root);

    // Target file to publish.
    std::fs::write(root.join("new.txt"), b"new content").unwrap();

    let opts = PublishOptions {
        message: format!("publish crash @ {phase}"),
        files: vec!["new.txt".to_string()],
        expected_head: None,
        expected_fingerprints: BTreeMap::new(),
        push: false,
        amend: false,
        request_id: format!("pub-{}-{}", phase.replace(':', "-"), uuid::Uuid::new_v4()),
    };

    // 1. Crash at the target phase.
    let probe: PublishProbe = crash_probe(phase);
    let crash_err = publish_with_state(root, &opts, Some(probe), &state_root)
        .err()
        .unwrap_or_else(|| panic!("phase {phase}: expected crash (Err), got Ok"));
    assert!(
        crash_err.contains("CRASH@"),
        "phase {phase}: crash error should contain CRASH@, got: {crash_err}",
    );

    // 2. Retry without probe — the journal's begin → Resume/Replay drives
    //    recovery to completion.
    let result = publish_with_state(root, &opts, None, &state_root)
        .unwrap_or_else(|e| panic!("phase {phase}: retry failed: {e}"));
    assert_eq!(
        result["published"],
        json!(true),
        "phase {phase}: retry did not report published=true",
    );

    // 3. Unrelated worktree state preserved (no lost work).
    verify_unrelated_preserved(root, &unrelated);

    // 4. Target file content preserved.
    let new_content = std::fs::read_to_string(root.join("new.txt"))
        .unwrap_or_else(|e| panic!("phase {phase}: new.txt lost: {e}"));
    assert_eq!(
        new_content, "new content",
        "phase {phase}: new.txt content changed during recovery",
    );

    // 5. git fsck clean.
    assert_fsck_clean(root);

    // 6. Commit scope is exactly the requested files — nothing more, nothing
    //    less. A pre-existing staged unrelated file must NOT be swept into the
    //    commit.
    let committed = commit_files_in_head(root);
    let mut expected = opts.files.clone();
    expected.sort();
    assert_eq!(
        committed, expected,
        "phase {phase}: commit scope mismatch — committed {committed:?}, expected {expected:?}",
    );
}

// ===========================================================================
// Push crash matrix
// ===========================================================================

/// Phases at which the push probe fires (in execution order):
///
///   journal:started
///     → journal:push_started
///     → remote:returned
///     → journal:terminal
const PUSH_PHASES: &[&str] = &[
    "journal:started",
    "journal:push_started",
    "remote:returned",
    "journal:terminal",
];

#[test]
fn push_crash_matrix() {
    for &phase in PUSH_PHASES {
        eprintln!("push crash matrix: phase = {phase}");
        push_crash_at_phase(phase);
    }
}

fn push_crash_at_phase(phase: &str) {
    let repo_dir = TempDir::new().unwrap();
    let remote_dir = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();
    let state_root = state_dir.path().join("pixel");
    std::fs::create_dir_all(&state_root).unwrap();

    let (_env_guard, _xdg) = lock_env(state_dir.path());

    let root = repo_dir.path();
    let remote = remote_dir.path();
    init_repo_with_remote(root, remote);

    // Create a non-target ref on the remote (tag v0 → base commit) so we can
    // verify it survives the push operation untouched.
    git(root, &["tag", "v0"]);
    git(root, &["push", "-q", "origin", "v0"]);
    let refs_before = list_refs(remote);
    let base_oid = refs_before
        .get("refs/heads/main")
        .cloned()
        .unwrap_or_else(|| panic!("remote missing refs/heads/main after init"));

    // Make a new commit to push.
    std::fs::write(root.join("new.txt"), b"push me").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "new commit for push crash matrix"]);
    let source_oid = git(root, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    let opts = PushOptions {
        remote: "origin".to_string(),
        refspec: "main".to_string(),
        request_id: format!("push-{}-{}", phase.replace(':', "-"), uuid::Uuid::new_v4()),
        force_with_lease: false,
    };

    // 1. Crash at the target phase.
    let probe: PushProbe = crash_probe(phase);
    let crash_err = push_with_state(root, &opts, Some(probe), &state_root)
        .err()
        .unwrap_or_else(|| panic!("phase {phase}: expected crash (Err), got Ok"));
    assert!(
        crash_err.contains("CRASH@"),
        "phase {phase}: crash error should contain CRASH@, got: {crash_err}",
    );

    // 2. Retry without probe.
    let retry = push_with_state(root, &opts, None, &state_root);

    let refs_after = list_refs(remote);

    if phase == "journal:push_started" {
        // Push may have started over the network — the safe behavior is to
        // refuse a blind retry and surface NETWORK_AMBIGUITY.
        let err = retry.err().unwrap_or_else(|| {
            panic!("phase {phase}: expected NETWORK_AMBIGUITY Err, got Ok")
        });
        assert!(
            err.contains("NETWORK_AMBIGUITY"),
            "phase {phase}: expected NETWORK_AMBIGUITY, got: {err}",
        );
        // The push never actually ran, so remote main must still be the base.
        let remote_main = refs_after
            .get("refs/heads/main")
            .cloned()
            .unwrap_or_else(|| panic!("phase {phase}: remote main missing"));
        assert_eq!(
            remote_main, base_oid,
            "phase {phase}: remote main changed despite NETWORK_AMBIGUITY (push should not have run)",
        );
    } else {
        // All other phases recover to a successful push.
        let result = retry.unwrap_or_else(|e| panic!("phase {phase}: retry failed: {e}"));
        assert_eq!(
            result["pushed"],
            json!(true),
            "phase {phase}: retry did not report pushed=true",
        );
        // Remote main must now point at the pushed commit.
        let remote_main = refs_after
            .get("refs/heads/main")
            .cloned()
            .unwrap_or_else(|| panic!("phase {phase}: remote main missing after push"));
        assert_eq!(
            remote_main, source_oid,
            "phase {phase}: remote main ({remote_main}) != source_oid ({source_oid}) after push",
        );
    }

    // 3. Remote non-target refs untouched (e.g. refs/tags/v0).
    verify_non_target_refs_untouched(
        &refs_before,
        &refs_after,
        "refs/heads/main",
        phase,
    );

    // 4. git fsck clean on both local and remote.
    assert_fsck_clean(root);
    assert_fsck_clean(remote);
}
