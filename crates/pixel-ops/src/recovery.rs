// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Publish recovery store — long-lived recovery state for `publish` so
//! crashes can resume. Port of usable-git's `publish-recovery.ts`.
//!
//! The recovery store captures the pre-mutation state (HEAD, a byte-exact
//! `.git/index` snapshot, files being committed) so that a crashed publish
//! can be restored to its exact pre-operation state on retry. Any resume
//! that finds a recovery record on disk is, by construction, resuming into
//! a window where it is impossible to prove whether the underlying git
//! mutation (`git add`, `git commit`) fully applied, partially applied, or
//! never ran at all — so the safe behavior is always: restore the raw
//! index bytes (and HEAD, if it moved) back to the pre-operation snapshot,
//! delete the recovery record, and surface `GIT_FAILED` rather than guess.
//! A record that exists but cannot be read is that same window with less
//! information: [`PublishRecoveryStore::read`] returns `Err`, never `None`,
//! so the caller refuses the retry instead of re-running the mutation.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use pixel_git::GitRunner;

use crate::durable::{ensure_dir, sha256_hex, state_root, write_durably};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPhase {
    Snapshotted,
    IndexStaged,
    CommitStarted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishRecoveryState {
    pub schema_version: u32,
    pub request_id: String,
    pub repo_key: String,
    pub phase: RecoveryPhase,
    pub pre_head: Option<String>,
    pub files: Vec<String>,
    pub owned_index_checksum: Option<String>,
    pub mode: Option<String>, // "append" | "amend"
    pub resolved_message: Option<String>,
    /// Hex-encoded raw bytes of `.git/index` as it existed immediately
    /// before this operation touched it. `None` means the index file did
    /// not exist yet (e.g. a truly empty/unborn repo).
    pub pre_index_hex: Option<String>,
}

pub struct PublishRecoveryStore {
    state_root: PathBuf,
}

impl PublishRecoveryStore {
    pub fn new() -> Self {
        Self {
            state_root: state_root(),
        }
    }

    pub fn with_state_root(state_root: PathBuf) -> Self {
        Self { state_root }
    }

    fn recovery_dir(&self, repo_key: &str) -> PathBuf {
        self.state_root
            .join("publish-recovery")
            .join(sha256_hex(repo_key))
    }

    fn recovery_path(&self, repo_key: &str, request_id: &str) -> PathBuf {
        self.recovery_dir(repo_key)
            .join(format!("{}.json", sha256_hex(request_id)))
    }

    pub fn write(&self, state: &PublishRecoveryState) -> Result<(), String> {
        let dir = self.recovery_dir(&state.repo_key);
        ensure_dir(&dir).map_err(|e| e.to_string())?;
        let path = self.recovery_path(&state.repo_key, &state.request_id);
        let json = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
        write_durably(&path, &json).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Read the recovery record for `(repo_key, request_id)`.
    ///
    /// `Ok(None)` is reserved for a record that is genuinely absent — no
    /// crash window. A record that exists but cannot be read or parsed
    /// (truncated write, permissions, an unknown `schema_version`) is an
    /// `Err`: the caller must never read "I cannot tell what this record
    /// says" as "no crash happened" and re-run the mutation.
    pub fn read(
        &self,
        repo_key: &str,
        request_id: &str,
    ) -> Result<Option<PublishRecoveryState>, String> {
        let path = self.recovery_path(repo_key, request_id);
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(format!(
                    "GIT_FAILED: recovery record unreadable at {}: {e}",
                    path.display()
                ));
            }
        };
        let state: PublishRecoveryState = serde_json::from_slice(&data).map_err(|e| {
            format!(
                "GIT_FAILED: recovery record corrupt at {}: {e}",
                path.display()
            )
        })?;
        if state.schema_version != 1 {
            return Err(format!(
                "GIT_FAILED: recovery record has unknown schema_version {} at {}",
                state.schema_version,
                path.display()
            ));
        }
        Ok(Some(state))
    }

    pub fn remove(&self, repo_key: &str, request_id: &str) {
        let path = self.recovery_path(repo_key, request_id);
        let _ = std::fs::remove_file(&path);
    }

    /// Check if any recovery files exist for a repo (used to detect
    /// incomplete operations on startup).
    pub fn has_pending(&self, repo_key: &str) -> bool {
        let dir = self.recovery_dir(repo_key);
        if !dir.exists() {
            return false;
        }
        std::fs::read_dir(&dir).is_ok_and(|mut it| it.next().is_some())
    }
}

impl Default for PublishRecoveryStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Capture the git index checksum (sha256 of `.git/index`).
pub fn index_checksum(repo_root: &Path) -> Option<String> {
    let bytes = std::fs::read(index_path(repo_root)).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(hex::encode(hasher.finalize()))
}

fn index_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".git").join("index")
}

/// Capture a byte-exact, restorable snapshot of `.git/index` as it exists
/// right now. Returns `None` if the index file does not exist yet.
pub fn capture_index_snapshot(repo_root: &Path) -> Option<String> {
    std::fs::read(index_path(repo_root)).ok().map(hex::encode)
}

/// Restore `.git/index` to exactly the bytes captured by
/// `capture_index_snapshot`, and move `HEAD` back to `pre_head` if it has
/// since advanced. Never touches the worktree, so unrelated pending edits
/// (staged, unstaged, or untracked) in the worktree survive untouched —
/// only the index (staged intent) and HEAD (commit history) are restored.
pub fn restore_snapshot(repo_root: &Path, state: &PublishRecoveryState) -> Result<(), String> {
    let path = index_path(repo_root);
    match &state.pre_index_hex {
        Some(hex_bytes) => {
            let bytes = hex::decode(hex_bytes).map_err(|e| {
                format!("GIT_FAILED: corrupt recovery snapshot (bad index hex): {e}")
            })?;
            write_durably(&path, &bytes)
                .map_err(|e| format!("GIT_FAILED: failed to restore .git/index: {e}"))?;
        }
        None => {
            // No index existed before this operation started — remove
            // whatever partial index exists now.
            let _ = std::fs::remove_file(&path);
        }
    }

    if let Some(pre_head) = &state.pre_head {
        let runner = GitRunner::new(repo_root);
        let current_head = runner.rev_parse_head();
        if current_head.as_deref() != Some(pre_head.as_str()) {
            runner
                .run(&["update-ref", "HEAD", pre_head])
                .map_err(|e| format!("GIT_FAILED: failed to restore HEAD to {pre_head}: {e}"))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_state(phase: RecoveryPhase) -> PublishRecoveryState {
        PublishRecoveryState {
            schema_version: 1,
            request_id: "test-req".to_string(),
            repo_key: "/test/repo".to_string(),
            phase,
            pre_head: Some("abc123".to_string()),
            files: vec!["a.txt".to_string()],
            owned_index_checksum: Some("deadbeef".to_string()),
            mode: Some("append".to_string()),
            resolved_message: Some("test message".to_string()),
            pre_index_hex: Some(hex::encode(b"fake-index-bytes")),
        }
    }

    #[test]
    fn recovery_write_read_remove() {
        let dir = tempdir().unwrap();
        let store = PublishRecoveryStore::with_state_root(dir.path().to_path_buf());
        let state = make_state(RecoveryPhase::Snapshotted);
        store.write(&state).unwrap();
        let read = store
            .read("/test/repo", "test-req")
            .unwrap()
            .expect("record present");
        assert_eq!(read.phase, RecoveryPhase::Snapshotted);
        assert_eq!(read.pre_head, Some("abc123".to_string()));
        assert_eq!(read.pre_index_hex, state.pre_index_hex);
        store.remove("/test/repo", "test-req");
        assert!(store.read("/test/repo", "test-req").unwrap().is_none());
    }

    /// "Absent" and "I cannot tell what this record says" are different
    /// answers: only the first one licenses a retry to run the mutation.
    #[test]
    fn recovery_read_separates_absent_from_unknown_schema() {
        let dir = tempdir().unwrap();
        let store = PublishRecoveryStore::with_state_root(dir.path().to_path_buf());
        assert!(store.read("/test/repo", "missing").unwrap().is_none());

        let mut bumped = make_state(RecoveryPhase::Snapshotted);
        bumped.schema_version = 2;
        store.write(&bumped).unwrap();
        let err = store.read("/test/repo", "test-req").unwrap_err();
        assert!(err.contains("GIT_FAILED"), "{err}");
        assert!(err.contains("schema_version"), "{err}");
    }

    #[test]
    fn recovery_read_reports_a_truncated_record() {
        let dir = tempdir().unwrap();
        let store = PublishRecoveryStore::with_state_root(dir.path().to_path_buf());
        let state = make_state(RecoveryPhase::Snapshotted);
        store.write(&state).unwrap();
        let path = store.recovery_path("/test/repo", "test-req");
        let full = std::fs::read(&path).unwrap();
        std::fs::write(&path, &full[..full.len() / 2]).unwrap();
        let err = store.read("/test/repo", "test-req").unwrap_err();
        assert!(err.contains("GIT_FAILED"), "{err}");
        assert!(err.contains("corrupt"), "{err}");
    }

    /// A non-NotFound IO error (e.g., the record path is a directory) must
    /// fail closed, not be treated as an absent record.
    #[test]
    fn recovery_read_fails_on_non_notfound_io_error() {
        let dir = tempdir().unwrap();
        let store = PublishRecoveryStore::with_state_root(dir.path().to_path_buf());
        let path = store.recovery_path("/test/repo", "ioerr-req");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::create_dir(&path).unwrap();
        let err = store.read("/test/repo", "ioerr-req").unwrap_err();
        assert!(err.contains("GIT_FAILED"), "{err}");
        assert!(err.contains("unreadable"), "{err}");
    }

    #[test]
    fn recovery_has_pending() {
        let dir = tempdir().unwrap();
        let store = PublishRecoveryStore::with_state_root(dir.path().to_path_buf());
        assert!(!store.has_pending("/test/repo"));
        let state = make_state(RecoveryPhase::IndexStaged);
        store.write(&state).unwrap();
        assert!(store.has_pending("/test/repo"));
        store.remove("/test/repo", "test-req");
        assert!(!store.has_pending("/test/repo"));
    }

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
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn restore_snapshot_reverts_index_bytes_and_head() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        std::fs::write(root.join("base.txt"), b"base").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        let pre_head = git(root, &["rev-parse", "HEAD"]);
        let pre_index = capture_index_snapshot(root).expect("index should exist after commit");

        // Mutate: stage a new file and commit it (simulating a completed
        // mutation the recovery restore must fully undo).
        std::fs::write(root.join("new.txt"), b"new").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "mutate"]);
        let post_head = git(root, &["rev-parse", "HEAD"]);
        assert_ne!(pre_head, post_head);

        let state = PublishRecoveryState {
            schema_version: 1,
            request_id: "r1".to_string(),
            repo_key: "k".to_string(),
            phase: RecoveryPhase::CommitStarted,
            pre_head: Some(pre_head.clone()),
            files: vec!["new.txt".to_string()],
            owned_index_checksum: None,
            mode: None,
            resolved_message: None,
            pre_index_hex: Some(pre_index),
        };
        restore_snapshot(root, &state).unwrap();

        assert_eq!(
            git(root, &["rev-parse", "HEAD"]),
            pre_head,
            "HEAD must be restored"
        );
        let restored_index = std::fs::read(root.join(".git").join("index")).unwrap();
        let expected_index = hex::decode(state.pre_index_hex.unwrap()).unwrap();
        assert_eq!(
            restored_index, expected_index,
            "index bytes must be byte-identical"
        );
    }

    /// The index checksum is what tells a recovery pass whether the index it
    /// left behind is still the one it wrote.
    #[test]
    fn index_checksum_hashes_the_git_index_and_changes_when_it_does() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        assert_eq!(index_checksum(root), None, "no repository yet");
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("a.txt"), b"a\n").unwrap();
        git(&["add", "a.txt"]);
        let first = index_checksum(root).expect("index exists after add");
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "{first}");
        assert_eq!(
            index_checksum(root).as_deref(),
            Some(first.as_str()),
            "stable"
        );
        std::fs::write(root.join("b.txt"), b"b\n").unwrap();
        git(&["add", "b.txt"]);
        assert_ne!(index_checksum(root).unwrap(), first);
    }
}
