//! Crash matrix tests for `publish` and `push`.
//!
//! Faithful port of usable-git's `mutation-crash-matrix.test.ts`: inject a
//! crash at each real production phase via the probe hook, retry, then
//! verify the exact reference invariants:
//!
//!   * The probe actually observed the target phase (proves the injection
//!     drives the real production code path, not a simulated stand-in).
//!   * Retry classification: crashing at `recovery:snapshotted`,
//!     `journal:index_staged`, or `recovery:commit_started` must FAIL the
//!     retry with `GIT_FAILED` and leave local state BYTE-IDENTICAL to the
//!     pre-crash state (raw `.git/index` bytes included, not just a status
//!     string). Crashing at `journal:started`, `journal:commit_observed`,
//!     or `journal:terminal` must make the retry SUCCEED.
//!   * Unrelated worktree/index state (staged/unstaged/loose) is preserved
//!     after every cell, and the commit scope (when one lands) is exactly
//!     the requested files.
//!   * `git fsck --strict` is clean.
//!   * No `publish-recovery/**/*.json` record is left behind afterward.
//!   * For push: non-target remote refs are untouched, a local untracked
//!     file survives, and a crash at `journal:push_started` surfaces
//!     `NETWORK_AMBIGUITY` rather than a silent retry.
//!
//! Each of the 10 cells is its own `#[test]` function (not a `for` loop
//! inside one test) so a failing cell is individually visible in the test
//! report instead of aborting the remaining cells silently.
//!
//! NOTE: `publish`/`push` resume from `JournalPhase::Started` by re-running
//! the operation body using the process-global state root helper as a
//! fallback path. To keep the journal + recovery stores consistent across
//! the crash and the retry, each test points `XDG_STATE_HOME` at a per-test
//! tempdir. A process-wide `Mutex` serializes tests so the env-var
//! manipulation is race-free.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tempfile::TempDir;

use pixel_ops::publish::{publish_with_state, PublishOptions, PublishProbe};
use pixel_ops::push::{push_with_state, PushOptions, PushProbe};

// ---------------------------------------------------------------------------
// Serialization + env-var guard
// ---------------------------------------------------------------------------

static ENV_GUARD: Mutex<()> = Mutex::new(());

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

fn lock_env(state_dir: &Path) -> (std::sync::MutexGuard<'static, ()>, XdgEnvGuard) {
    // SAFETY: `ENV_GUARD` serializes all callers, so the env-var write is not
    // racy with other tests in this binary.
    let guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::set_var("XDG_STATE_HOME", state_dir);
    }
    (guard, XdgEnvGuard)
}

// ---------------------------------------------------------------------------
// Probe factory — inject a crash at exactly one phase, and record every
// phase actually observed so we can prove the injection drove the real
// production code path (not merely that a phase name string exists
// somewhere in the test file).
// ---------------------------------------------------------------------------

type Observed = Arc<Mutex<Vec<String>>>;

fn crash_probe(target: &str) -> (Observed, Box<dyn FnMut(&str) -> Result<(), String>>) {
    let observed: Observed = Arc::new(Mutex::new(Vec::new()));
    let observed_for_probe = observed.clone();
    let t = target.to_string();
    let probe: Box<dyn FnMut(&str) -> Result<(), String>> = Box::new(move |phase: &str| {
        observed_for_probe.lock().unwrap().push(phase.to_string());
        if phase == t {
            Err(format!("CRASH@{t}"))
        } else {
            Ok(())
        }
    });
    (observed, probe)
}

fn assert_probe_reached_phase(observed: &Observed, phase: &str) {
    let seen = observed.lock().unwrap();
    assert!(
        seen.contains(&phase.to_string()),
        "phase {phase}: probe never observed the target phase (observed: {seen:?}) — \
         crash injection did not reach the real production code path",
    );
}

// ---------------------------------------------------------------------------
// git helpers
// ---------------------------------------------------------------------------

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

fn write_file(root: &Path, path: &str, content: &str) {
    std::fs::write(root.join(path), content).unwrap();
}

fn commit_file(root: &Path, path: &str, content: &str, message: &str) {
    write_file(root, path, content);
    git(root, &["add", "--", path]);
    git(root, &["commit", "-qm", message]);
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
    commit_file(root, "base.txt", "base\n", "base");
}

/// Faithful port of usable-git's `publishFixture()`: a committed baseline
/// plus, simultaneously, a staged MODIFICATION of a committed file, an
/// unstaged modification of a different committed file, an untracked
/// (loose) file, and the untracked target file to publish.
///
/// Critically, `staged.txt` is a modification of a pre-existing committed
/// file (not a brand-new staged file) — that distinction is what makes a
/// `git reset --quiet HEAD` on resume observably destructive: it would
/// unstage the pending edit and could even mask reverting it back to the
/// committed baseline content.
fn setup_publish_fixture(root: &Path) {
    commit_file(root, "staged.txt", "staged base\n", "staged base");
    commit_file(root, "unstaged.txt", "unstaged base\n", "unstaged base");
    write_file(root, "selected.txt", "selected\n");
    write_file(root, "staged.txt", "staged pending\n");
    git(root, &["add", "--", "staged.txt"]);
    write_file(root, "unstaged.txt", "unstaged pending\n");
    write_file(root, "loose.txt", "loose pending\n");
}

// ---------------------------------------------------------------------------
// Verification helpers
// ---------------------------------------------------------------------------

fn assert_fsck_ok(root: &Path) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["fsck", "--strict"])
        .output()
        .unwrap_or_else(|e| panic!("git fsck on {}: {e}", root.display()));
    assert!(
        output.status.success(),
        "git fsck --strict failed for {}: stdout={} stderr={}",
        root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// The reference asserts the remote's `fsck --strict` output is the exact
/// empty string, not merely "no error/broken/missing substrings".
fn assert_remote_fsck_empty(remote: &Path) {
    let output = Command::new("git")
        .arg("-C")
        .arg(remote)
        .args(["fsck", "--strict"])
        .output()
        .unwrap_or_else(|e| panic!("git fsck on {}: {e}", remote.display()));
    assert!(
        output.status.success(),
        "git fsck --strict failed for remote {}",
        remote.display(),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        combined.trim().is_empty(),
        "git fsck --strict expected empty output for remote {}, got: {combined}",
        remote.display(),
    );
}

/// Full local-state snapshot used for the byte-identical-after-failed-
/// recovery assertion. Includes the raw `.git/index` bytes (hex-encoded
/// for a readable diff on failure), not just a derived status string —
/// this is what actually catches the index being silently mutated.
#[derive(Debug, PartialEq, Eq)]
struct LocalStateSnapshot {
    head: String,
    tree: String,
    index_hex: String,
    status: String,
    staged: String,
    unstaged: String,
    loose: String,
}

fn snapshot_local_state(root: &Path) -> LocalStateSnapshot {
    let index_bytes = std::fs::read(root.join(".git").join("index")).unwrap_or_default();
    LocalStateSnapshot {
        head: git(root, &["rev-parse", "HEAD"]),
        tree: git(root, &["ls-tree", "-r", "HEAD"]),
        index_hex: hex::encode(index_bytes),
        status: git(root, &["status", "--porcelain=v2", "-z"]),
        staged: git(root, &["show", ":staged.txt"]),
        unstaged: std::fs::read_to_string(root.join("unstaged.txt")).unwrap(),
        loose: std::fs::read_to_string(root.join("loose.txt")).unwrap(),
    }
}

/// Faithful port of usable-git's `expectUnrelatedState()`. Notably
/// `staged.txt`'s INDEX content is checked via `git show :staged.txt` —
/// this is exactly the assertion that catches a resume path that runs
/// `git reset --quiet HEAD` and re-stages only the requested files,
/// silently discarding the user's own staged edit.
fn expect_unrelated_state(root: &Path) {
    assert_eq!(
        git(root, &["show", ":staged.txt"]),
        "staged pending\n",
        "staged.txt index content changed — the user's staged edit was discarded",
    );
    assert_eq!(
        std::fs::read_to_string(root.join("unstaged.txt")).unwrap(),
        "unstaged pending\n",
        "unstaged.txt worktree content changed",
    );
    assert_eq!(
        std::fs::read_to_string(root.join("loose.txt")).unwrap(),
        "loose pending\n",
        "loose.txt worktree content changed",
    );
    assert_eq!(
        git(root, &["diff", "--cached", "--name-only"]),
        "staged.txt\n",
        "staged index scope changed — an unrelated file was swept into or out of the index",
    );
    assert_fsck_ok(root);
}

fn assert_no_leftover_recovery(state_root: &Path) {
    let dir = state_root.join("publish-recovery");
    let mut leftover: Vec<PathBuf> = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().and_then(|s| s.to_str()) == Some("json") {
                    out.push(path);
                }
            }
        }
    }
    if dir.exists() {
        walk(&dir, &mut leftover);
    }
    assert!(
        leftover.is_empty(),
        "leftover publish-recovery records after operation completed: {leftover:?}",
    );
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
        let actual = after
            .get(name)
            .unwrap_or_else(|| panic!("phase {phase}: non-target ref {name} disappeared from remote"));
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
///
/// `recovery:snapshotted`, `journal:index_staged`, and
/// `recovery:commit_started` are the "ambiguous window": a recovery record
/// is on disk and it cannot be proven whether `git add`/`git commit` fully
/// applied, so retry must fail (`GIT_FAILED`) and restore byte-identical
/// pre-crash state. The other three phases resolve to a successful retry.
fn publish_crash_at_phase(phase: &str) {
    let repo_dir = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();
    let state_root = state_dir.path().join("pixel");
    std::fs::create_dir_all(&state_root).unwrap();

    let (_env_guard, _xdg) = lock_env(state_dir.path());

    let root = repo_dir.path();
    init_repo(root);
    setup_publish_fixture(root);

    let opts = PublishOptions {
        message: format!("publish crash @ {phase}"),
        files: vec!["selected.txt".to_string()],
        expected_head: None,
        expected_fingerprints: BTreeMap::new(),
        push: false,
        amend: false,
        request_id: format!("pub-{}-{}", phase.replace(':', "-"), uuid::Uuid::new_v4()),
    };

    let before = snapshot_local_state(root);

    // 1. Crash at the target phase, injected into the real production
    //    `publish_with_state` call.
    let (observed, probe) = crash_probe(phase);
    let probe: PublishProbe = probe;
    let crash_err = publish_with_state(root, &opts, Some(probe), &state_root)
        .err()
        .unwrap_or_else(|| panic!("phase {phase}: expected crash (Err), got Ok"));
    assert!(
        crash_err.contains("CRASH@"),
        "phase {phase}: crash error should contain CRASH@, got: {crash_err}",
    );
    assert_probe_reached_phase(&observed, phase);

    let ambiguous = matches!(
        phase,
        "recovery:snapshotted" | "journal:index_staged" | "recovery:commit_started"
    );

    // 2. Retry without probe.
    if ambiguous {
        let retry_err = publish_with_state(root, &opts, None, &state_root)
            .err()
            .unwrap_or_else(|| panic!("phase {phase}: expected retry to fail with GIT_FAILED, got Ok"));
        assert!(
            retry_err.contains("GIT_FAILED"),
            "phase {phase}: expected GIT_FAILED, got: {retry_err}",
        );
        let after = snapshot_local_state(root);
        assert_eq!(
            after, before,
            "phase {phase}: local state not byte-identical to pre-crash state after failed recovery",
        );
    } else {
        let result = publish_with_state(root, &opts, None, &state_root)
            .unwrap_or_else(|e| panic!("phase {phase}: retry failed: {e}"));
        assert_eq!(
            result["published"],
            json!(true),
            "phase {phase}: retry did not report published=true",
        );

        let selected_content = std::fs::read_to_string(root.join("selected.txt"))
            .unwrap_or_else(|e| panic!("phase {phase}: selected.txt lost: {e}"));
        assert_eq!(
            selected_content, "selected\n",
            "phase {phase}: selected.txt content changed during recovery",
        );

        let committed = commit_files_in_head(root);
        assert_eq!(
            committed,
            vec!["selected.txt".to_string()],
            "phase {phase}: commit scope mismatch — committed {committed:?}",
        );
    }

    // 3. Unrelated state preserved + fsck clean, in every case.
    expect_unrelated_state(root);

    // 4. No leftover recovery records.
    assert_no_leftover_recovery(&state_root);
}

#[test]
fn publish_crash_journal_started() {
    publish_crash_at_phase("journal:started");
}

#[test]
fn publish_crash_recovery_snapshotted() {
    publish_crash_at_phase("recovery:snapshotted");
}

#[test]
fn publish_crash_journal_index_staged() {
    publish_crash_at_phase("journal:index_staged");
}

#[test]
fn publish_crash_recovery_commit_started() {
    publish_crash_at_phase("recovery:commit_started");
}

#[test]
fn publish_crash_journal_commit_observed() {
    publish_crash_at_phase("journal:commit_observed");
}

#[test]
fn publish_crash_journal_terminal() {
    publish_crash_at_phase("journal:terminal");
}

// ===========================================================================
// Push crash matrix
// ===========================================================================

/// Faithful port of usable-git's `pushFixture()`: local + bare remote, an
/// UNRELATED branch on the remote pointing at the base commit, and an
/// untracked local file that must survive the push attempt untouched.
fn init_push_fixture(root: &Path, remote: &Path) {
    Command::new("git")
        .args(["init", "-q", "--initial-branch=main", root.to_str().unwrap()])
        .status()
        .unwrap();
    git(root, &["config", "user.email", "crash-matrix@pixel"]);
    git(root, &["config", "user.name", "Crash Matrix"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    commit_file(root, "tracked.txt", "base\n", "base");

    Command::new("git")
        .args(["init", "--bare", "-q", remote.to_str().unwrap()])
        .status()
        .unwrap();
    git(remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(root, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(root, &["push", "-q", "origin", "main"]);
}

/// Phases at which the push probe fires (in execution order):
///
///   journal:started
///     → journal:push_started
///     → remote:returned
///     → journal:terminal
fn push_crash_at_phase(phase: &str) {
    let repo_dir = TempDir::new().unwrap();
    let remote_dir = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();
    let state_root = state_dir.path().join("pixel");
    std::fs::create_dir_all(&state_root).unwrap();

    let (_env_guard, _xdg) = lock_env(state_dir.path());

    let root = repo_dir.path();
    let remote = remote_dir.path();
    init_push_fixture(root, remote);

    // Non-target ref: an unrelated BRANCH on the remote (not merely a tag)
    // pointing at the base commit.
    git(root, &["branch", "unrelated", "main"]);
    git(root, &["push", "-q", "origin", "unrelated"]);
    let refs_before = list_refs(remote);
    let base_oid = refs_before
        .get("refs/heads/main")
        .cloned()
        .unwrap_or_else(|| panic!("remote missing refs/heads/main after init"));

    // Make a new commit to push.
    write_file(root, "tracked.txt", "next\n");
    git(root, &["add", "--", "tracked.txt"]);
    git(root, &["commit", "-qm", "next"]);
    let source_oid = git(root, &["rev-parse", "HEAD"]).trim().to_string();

    // Untracked local file that must survive the push attempt untouched.
    write_file(root, "unrelated.txt", "local pending\n");

    let opts = PushOptions {
        remote: "origin".to_string(),
        refspec: "main".to_string(),
        request_id: format!("push-{}-{}", phase.replace(':', "-"), uuid::Uuid::new_v4()),
        force_with_lease: false,
    };

    // 1. Crash at the target phase, injected into the real production
    //    `push_with_state` call.
    let (observed, probe) = crash_probe(phase);
    let probe: PushProbe = probe;
    let crash_err = push_with_state(root, &opts, Some(probe), &state_root)
        .err()
        .unwrap_or_else(|| panic!("phase {phase}: expected crash (Err), got Ok"));
    assert!(
        crash_err.contains("CRASH@"),
        "phase {phase}: crash error should contain CRASH@, got: {crash_err}",
    );
    assert_probe_reached_phase(&observed, phase);

    // 2. Retry without probe.
    let retry = push_with_state(root, &opts, None, &state_root);
    let refs_after = list_refs(remote);

    if phase == "journal:push_started" {
        // Push may have started over the network — the safe behavior is to
        // refuse a blind retry and surface NETWORK_AMBIGUITY.
        let err = retry
            .err()
            .unwrap_or_else(|| panic!("phase {phase}: expected NETWORK_AMBIGUITY Err, got Ok"));
        assert!(
            err.contains("NETWORK_AMBIGUITY"),
            "phase {phase}: expected NETWORK_AMBIGUITY, got: {err}",
        );
        let remote_main = refs_after
            .get("refs/heads/main")
            .cloned()
            .unwrap_or_else(|| panic!("phase {phase}: remote main missing"));
        assert_eq!(
            remote_main, base_oid,
            "phase {phase}: remote main changed despite NETWORK_AMBIGUITY (push should not have run)",
        );
    } else {
        let result = retry.unwrap_or_else(|e| panic!("phase {phase}: retry failed: {e}"));
        assert_eq!(
            result["pushed"],
            json!(true),
            "phase {phase}: retry did not report pushed=true",
        );
        let remote_main = refs_after
            .get("refs/heads/main")
            .cloned()
            .unwrap_or_else(|| panic!("phase {phase}: remote main missing after push"));
        assert_eq!(
            remote_main, source_oid,
            "phase {phase}: remote main ({remote_main}) != source_oid ({source_oid}) after push",
        );
    }

    // 3. Remote non-target refs untouched (e.g. refs/heads/unrelated).
    verify_non_target_refs_untouched(&refs_before, &refs_after, "refs/heads/main", phase);

    // 4. Local untracked file survives untouched, and local status is
    //    exactly the one expected untracked entry.
    assert_eq!(
        std::fs::read_to_string(root.join("unrelated.txt")).unwrap(),
        "local pending\n",
        "phase {phase}: local untracked file lost or modified",
    );
    assert_eq!(
        git(root, &["status", "--porcelain=v1"]),
        "?? unrelated.txt\n",
        "phase {phase}: local status is not exactly the expected single untracked file",
    );

    // 5. git fsck clean on both local and remote (remote must be exactly
    //    empty output, matching the reference).
    assert_fsck_ok(root);
    assert_remote_fsck_empty(remote);
}

#[test]
fn push_crash_journal_started() {
    push_crash_at_phase("journal:started");
}

#[test]
fn push_crash_journal_push_started() {
    push_crash_at_phase("journal:push_started");
}

#[test]
fn push_crash_remote_returned() {
    push_crash_at_phase("remote:returned");
}

#[test]
fn push_crash_journal_terminal() {
    push_crash_at_phase("journal:terminal");
}
