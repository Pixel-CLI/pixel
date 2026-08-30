//! `publish` — stage files, commit, and optionally push.
//!
//! Crash-safe: runs under repository lock + operation journal, AND under a
//! separate publish-recovery snapshot store. These are two distinct durable
//! state machines (mirroring usable-git's `operation-journal.ts` +
//! `publish-recovery.ts`):
//!
//!   * The journal tracks coarse phase (`started → index_staged →
//!     commit_observed → [push_started] → terminal`) for idempotent
//!     resume/replay keyed by `request_id`.
//!   * The recovery store holds a byte-exact snapshot of `.git/index` (plus
//!     `pre_head`) captured *before* anything is mutated. Its mere presence
//!     on disk means "we are in an ambiguous window where we cannot prove
//!     whether `git add`/`git commit` fully applied" — so any resume that
//!     finds a recovery record restores the exact pre-operation snapshot
//!     (never touching the worktree) and fails with `GIT_FAILED`, rather
//!     than guessing by re-running `git add`/`git commit` (which is exactly
//!     how a naive resume can silently discard a user's own staged work).
//!
//! Phase order for a fresh run (matches usable-git's crash matrix exactly):
//!   journal:started → recovery:snapshotted → journal:index_staged →
//!   recovery:commit_started → journal:commit_observed → journal:terminal
//!
//! The probe hook allows the crash matrix to inject failures at each phase.

use std::path::Path;

use serde_json::{json, Value};

use pixel_git::GitRunner;

use crate::durable::state_root;
use crate::fingerprint;
use crate::journal::{BeginOutcome, JournalOperation, JournalPhase, OperationJournal};
use crate::lock::RepositoryLock;
use crate::recovery::{self, PublishRecoveryState, PublishRecoveryStore, RecoveryPhase};

/// Options for a publish operation.
#[derive(Debug, Clone)]
pub struct PublishOptions {
    pub message: String,
    pub files: Vec<String>,
    pub expected_head: Option<String>,
    pub expected_fingerprints: std::collections::BTreeMap<String, String>,
    pub push: bool,
    pub amend: bool,
    pub request_id: String,
}

/// A probe hook called at each journal phase. Used by the crash matrix
/// to inject failures. Returns `Err` to simulate a crash.
pub type PublishProbe = Box<dyn FnMut(&str) -> Result<(), String>>;

/// Execute a publish operation with crash safety.
pub fn publish(
    root: &Path,
    opts: &PublishOptions,
    probe: Option<PublishProbe>,
) -> Result<Value, String> {
    let state_root = state_root();
    publish_with_state(root, opts, probe, &state_root)
}

pub fn publish_with_state(
    root: &Path,
    opts: &PublishOptions,
    probe: Option<PublishProbe>,
    state_root: &Path,
) -> Result<Value, String> {
    let runner = GitRunner::new(root);
    let repo_key = repo_key(root);
    let input_hash = publish_input_hash(opts);

    let journal = OperationJournal::with_state_root(state_root.to_path_buf());

    // Begin — check for existing journal record (crash recovery).
    let outcome = journal.begin(
        &opts.request_id,
        JournalOperation::Publish,
        &repo_key,
        &input_hash,
    )?;

    match outcome {
        BeginOutcome::Replay(result) => Ok(result),
        BeginOutcome::Resume { phase, .. } => {
            resume_publish(root, opts, &journal, phase, &runner, state_root)
        }
        BeginOutcome::Start => run_body(root, opts, &journal, &runner, state_root, probe),
    }
}

/// The full mutation body: acquire the lock, snapshot, stage, commit,
/// optionally push, and complete the journal. Used both for a fresh start
/// and for resuming from `JournalPhase::Started` with no pending recovery
/// record (i.e. the crash happened before any durable recovery state was
/// written, so there is no ambiguity to resolve).
fn run_body(
    root: &Path,
    opts: &PublishOptions,
    journal: &OperationJournal,
    runner: &GitRunner,
    state_root: &Path,
    mut probe: Option<PublishProbe>,
) -> Result<Value, String> {
    let repo_key = repo_key(root);

    let mut lock = RepositoryLock::acquire_with_state_root(&common_dir(root), state_root)
        .map_err(|_| "repository is busy".to_string())?;

    // Probe: journal:started
    if let Some(p) = probe.as_mut() {
        p("journal:started").map_err(|e| {
            let _ = lock.release();
            e
        })?;
    }

    // STALE_STATE: expected HEAD.
    let current_head = runner.rev_parse_head();
    if let Some(expected) = &opts.expected_head {
        if current_head.as_deref() != Some(expected.as_str()) {
            let _ = lock.release();
            return Err(format!(
                "STALE_STATE: expected head {}, got {:?}",
                expected, current_head
            ));
        }
    }

    // STALE_STATE: expected fingerprints for the files being published —
    // catches concurrent modification of exactly the files this request
    // believes it is publishing.
    for (path, expected_fp) in &opts.expected_fingerprints {
        let actual_fp = fingerprint::fingerprint_path(root, path);
        if &actual_fp != expected_fp {
            let _ = lock.release();
            return Err(format!(
                "STALE_STATE: fingerprint mismatch for {path}: expected {expected_fp}, got {actual_fp}"
            ));
        }
    }

    // Snapshot BEFORE mutating anything: byte-exact `.git/index` + pre-HEAD.
    // This is the sole basis for exact restoration if a crash lands
    // anywhere before the commit is durably observed.
    let recovery_store = PublishRecoveryStore::with_state_root(state_root.to_path_buf());
    let mut recovery_state = PublishRecoveryState {
        schema_version: 1,
        request_id: opts.request_id.clone(),
        repo_key: repo_key.clone(),
        phase: RecoveryPhase::Snapshotted,
        pre_head: current_head.clone(),
        files: opts.files.clone(),
        owned_index_checksum: recovery::index_checksum(root),
        mode: Some(if opts.amend { "amend".to_string() } else { "append".to_string() }),
        resolved_message: Some(opts.message.clone()),
        pre_index_hex: recovery::capture_index_snapshot(root),
    };
    recovery_store.write(&recovery_state).map_err(|e| {
        let _ = lock.release();
        e
    })?;

    // Probe: recovery:snapshotted
    if let Some(p) = probe.as_mut() {
        p("recovery:snapshotted").map_err(|e| {
            let _ = lock.release();
            e
        })?;
    }

    // Stage files.
    if !opts.files.is_empty() {
        let mut args: Vec<String> = vec!["add".into(), "--".into()];
        args.extend(opts.files.iter().cloned());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        runner.run(&arg_refs).map_err(|e| {
            let _ = lock.release();
            format!("git add: {e}")
        })?;
    }

    // Journal: index_staged
    journal.transition(&opts.request_id, &repo_key, JournalPhase::IndexStaged, None)?;

    // Probe: journal:index_staged
    if let Some(p) = probe.as_mut() {
        p("journal:index_staged").map_err(|e| {
            let _ = lock.release();
            e
        })?;
    }

    // Advance the recovery record's phase (the snapshot bytes themselves
    // never change — only the marker of how far we got).
    recovery_state.phase = RecoveryPhase::CommitStarted;
    recovery_store.write(&recovery_state).map_err(|e| {
        let _ = lock.release();
        e
    })?;

    // Probe: recovery:commit_started
    if let Some(p) = probe.as_mut() {
        p("recovery:commit_started").map_err(|e| {
            let _ = lock.release();
            e
        })?;
    }

    // Commit — scope to requested files with pathspec to avoid sweeping
    // unrelated staged files into the commit.
    let commit_mode = if opts.amend { "--amend" } else { "--no-edit" };
    let mut commit_args: Vec<String> =
        vec!["commit".into(), commit_mode.into(), "-m".into(), opts.message.clone()];
    if !opts.files.is_empty() {
        commit_args.push("--".into());
        commit_args.extend(opts.files.iter().cloned());
    }
    let arg_refs: Vec<&str> = commit_args.iter().map(String::as_str).collect();
    runner.run(&arg_refs).map_err(|e| {
        let _ = lock.release();
        format!("git commit: {e}")
    })?;

    // Observe the new HEAD.
    let new_head = runner.rev_parse_head();
    journal.transition(
        &opts.request_id,
        &repo_key,
        JournalPhase::CommitObserved,
        Some(json!({"head": new_head})),
    )?;

    // The commit is durably observed — the ambiguous window is over.
    // Drop the recovery record; any crash from here on resumes safely
    // without it (the commit already exists, nothing to roll back).
    recovery_store.remove(&repo_key, &opts.request_id);

    // Probe: journal:commit_observed
    if let Some(p) = probe.as_mut() {
        p("journal:commit_observed").map_err(|e| {
            let _ = lock.release();
            e
        })?;
    }

    // Push if requested.
    if opts.push {
        journal.transition(&opts.request_id, &repo_key, JournalPhase::PushStarted, None)?;
        if let Some(p) = probe.as_mut() {
            p("journal:push_started").map_err(|e| {
                let _ = lock.release();
                e
            })?;
        }
        runner.run(&["push"]).map_err(|e| {
            let _ = lock.release();
            format!("git push: {e}")
        })?;
    }

    // Complete.
    let result = json!({
        "head": new_head,
        "published": true,
        "pushed": opts.push,
    });
    journal.complete(&opts.request_id, &repo_key, result.clone())?;

    // Probe: journal:terminal
    if let Some(p) = probe.as_mut() {
        p("journal:terminal").map_err(|e| {
            let _ = lock.release();
            e
        })?;
    }

    let _ = lock.release();
    Ok(result)
}

/// Resume a publish from a given phase after a crash.
fn resume_publish(
    root: &Path,
    opts: &PublishOptions,
    journal: &OperationJournal,
    phase: JournalPhase,
    runner: &GitRunner,
    state_root: &Path,
) -> Result<Value, String> {
    let repo_key = repo_key(root);
    let recovery_store = PublishRecoveryStore::with_state_root(state_root.to_path_buf());

    match phase {
        JournalPhase::Started => {
            if let Some(state) = recovery_store.read(&repo_key, &opts.request_id) {
                // A recovery record exists: we cannot prove whether `git
                // add` (or, transitively, `git commit`) ran to completion
                // before the crash. Restore the exact pre-operation
                // snapshot and refuse to guess.
                recovery::restore_snapshot(root, &state)?;
                recovery_store.remove(&repo_key, &opts.request_id);
                return Err(format!(
                    "GIT_FAILED: crash detected mid-mutation at recovery phase {:?}; local state restored to the pre-operation snapshot",
                    state.phase
                ));
            }
            // No recovery record — the crash happened before anything
            // durable was written. Safe to run the operation fresh.
            run_body(root, opts, journal, runner, state_root, None)
        }
        JournalPhase::IndexStaged => {
            // The index was staged (and possibly committed) before the
            // crash; a recovery record must exist (it is always written
            // before staging begins). Restore from it rather than blindly
            // `git reset` + re-stage + re-commit — that is precisely the
            // pattern that can silently discard the user's own staged
            // work if the reset sweeps away state a recovery snapshot
            // would have preserved.
            if let Some(state) = recovery_store.read(&repo_key, &opts.request_id) {
                recovery::restore_snapshot(root, &state)?;
                recovery_store.remove(&repo_key, &opts.request_id);
            }
            Err("GIT_FAILED: crash detected at index_staged; cannot safely determine whether the commit ran to completion — local state has been restored to the pre-operation snapshot".to_string())
        }
        JournalPhase::CommitObserved => {
            // Commit already happened; nothing ambiguous remains.
            recovery_store.remove(&repo_key, &opts.request_id);
            let new_head = runner.rev_parse_head();
            let result = json!({
                "head": new_head,
                "published": true,
                "pushed": false,
            });
            journal.complete(&opts.request_id, &repo_key, result.clone())?;
            Ok(result)
        }
        JournalPhase::PushStarted => {
            // Push may or may not have happened. For safety, report as
            // NETWORK_AMBIGUITY rather than risk a double-push or a lost
            // push.
            Err("NETWORK_AMBIGUITY: push may have started, cannot safely retry".to_string())
        }
        JournalPhase::Terminal => {
            // Should have been caught by Replay in begin().
            let record = journal.read(&repo_key, &opts.request_id);
            if let Some(r) = record {
                Ok(r.result.unwrap_or(json!({})))
            } else {
                Err("journal record lost".to_string())
            }
        }
        JournalPhase::RefUpdateStarted => {
            Err("unexpected phase for publish".to_string())
        }
    }
}

fn repo_key(root: &Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .display()
        .to_string()
}

fn common_dir(root: &Path) -> String {
    root.join(".git").display().to_string()
}

fn publish_input_hash(opts: &PublishOptions) -> String {
    use crate::durable::sha256_hex;
    let mut input = format!("{}\u{0}{}\u{0}{}\u{0}{}",
        opts.message,
        opts.files.join(","),
        opts.expected_head.as_deref().unwrap_or(""),
        opts.push,
    );
    let mut fps: Vec<String> = opts.expected_fingerprints
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    fps.sort();
    input.push_str(&fps.join(","));
    sha256_hex(&input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_repo(root: &Path) {
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(root)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", "user.email", "t@t"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", "user.name", "t"])
            .status()
            .unwrap();
        std::fs::write(root.join("base.txt"), b"base").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["add", "."])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["commit", "-qm", "base"])
            .status()
            .unwrap();
    }

    fn make_opts(msg: &str, files: &[&str]) -> PublishOptions {
        PublishOptions {
            message: msg.to_string(),
            files: files.iter().map(|s| s.to_string()).collect(),
            expected_head: None,
            expected_fingerprints: std::collections::BTreeMap::new(),
            push: false,
            amend: false,
            request_id: format!("test-{}", uuid::Uuid::new_v4()),
        }
    }

    #[test]
    fn publish_creates_commit() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("new.txt"), b"new content").unwrap();

        let opts = make_opts("test commit", &["new.txt"]);
        let result = publish(dir.path(), &opts, None).unwrap();
        assert_eq!(result["published"], json!(true));
        assert!(result["head"].as_str().unwrap().len() >= 7);

        // Verify the commit exists.
        let head = GitRunner::new(dir.path()).rev_parse_head().unwrap();
        assert_eq!(result["head"], json!(head));
    }

    #[test]
    fn publish_idempotent_replay() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();

        let opts = make_opts("idempotent test", &["a.txt"]);
        let result1 = publish(dir.path(), &opts, None).unwrap();

        // Retry with same request_id — should replay.
        let result2 = publish(dir.path(), &opts, None).unwrap();
        assert_eq!(result1["head"], result2["head"]);
    }

    #[test]
    fn publish_stale_state_rejected() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();

        let opts = PublishOptions {
            message: "stale test".to_string(),
            files: vec!["a.txt".to_string()],
            expected_head: Some("0000000000000000000000000000000000000000".to_string()),
            expected_fingerprints: std::collections::BTreeMap::new(),
            push: false,
            amend: false,
            request_id: format!("stale-{}", uuid::Uuid::new_v4()),
        };

        let err = publish(dir.path(), &opts, None).unwrap_err();
        assert!(err.contains("STALE_STATE"));
    }

    #[test]
    fn publish_stale_fingerprint_rejected() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), b"original").unwrap();

        let mut expected_fingerprints = std::collections::BTreeMap::new();
        expected_fingerprints.insert("a.txt".to_string(), "0".repeat(64));

        let opts = PublishOptions {
            message: "stale fingerprint".to_string(),
            files: vec!["a.txt".to_string()],
            expected_head: None,
            expected_fingerprints,
            push: false,
            amend: false,
            request_id: format!("stale-fp-{}", uuid::Uuid::new_v4()),
        };

        let err = publish(dir.path(), &opts, None).unwrap_err();
        assert!(err.contains("STALE_STATE"), "expected STALE_STATE, got: {err}");
    }

    #[test]
    fn recovery_record_cleaned_up_after_success() {
        let dir = tempdir().unwrap();
        let state_dir = tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();

        let opts = make_opts("cleanup test", &["a.txt"]);
        publish_with_state(dir.path(), &opts, None, state_dir.path()).unwrap();

        let store = PublishRecoveryStore::with_state_root(state_dir.path().to_path_buf());
        assert!(
            !store.has_pending(&repo_key(dir.path())),
            "recovery record must not linger after a successful publish",
        );
    }
}
