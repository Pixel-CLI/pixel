// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Publish recovery store — long-lived recovery state for `publish` so
//! crashes can resume. Port of usable-git's `publish-recovery.ts`.
//!
//! Schema 2 records the actual worktree index path, branch, pre-HEAD and
//! exact pre-index bytes. The owned checksum advances after staging. Recovery
//! takes Git's index lock and restores only an unchanged pre-HEAD/branch/index
//! ownership tuple; later history or staged intent is never rolled back.
//! A refused request retains its snapshot so repeated retries remain refused.
//! Legacy or unreadable records fail closed; they are never treated as absent.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
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
    pub index_path: PathBuf,
    pub head_ref: Option<String>,
    pub files: Vec<String>,
    pub owned_index_checksum: Option<String>,
    pub mode: Option<String>, // "append" | "amend"
    pub resolved_message: Option<String>,
    /// Hex-encoded raw bytes of the worktree index as it existed immediately
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
        if state.schema_version != 2 {
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

/// Capture the worktree's index checksum, failing closed on unreadable state.
pub fn index_checksum(repo_root: &Path) -> Result<Option<String>, String> {
    Ok(read_index(&index_path(repo_root)?)?.map(|bytes| checksum(&bytes)))
}

fn checksum(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub(crate) fn index_path(repo_root: &Path) -> Result<PathBuf, String> {
    let out = GitRunner::new(repo_root)
        .run(&["rev-parse", "--path-format=absolute", "--git-path", "index"])
        .map_err(|e| format!("GIT_FAILED: resolve worktree index: {e}"))?;
    let path = PathBuf::from(String::from_utf8_lossy(&out).trim());
    let parent = path
        .parent()
        .ok_or("GIT_FAILED: index has no parent")?
        .canonicalize()
        .map_err(|e| format!("GIT_FAILED: resolve index directory: {e}"))?;
    Ok(parent.join(path.file_name().ok_or("GIT_FAILED: index has no name")?))
}

fn read_index(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("GIT_FAILED: read worktree index: {e}")),
    }
}

/// Capture exact index bytes; only NotFound means an unborn/absent index.
pub fn capture_index_snapshot(repo_root: &Path) -> Result<Option<String>, String> {
    Ok(read_index(&index_path(repo_root)?)?.map(hex::encode))
}

/// Restore only index state still attributable to this interrupted request.
/// HEAD drift is ambiguous: retain the evidence and never rewind later history.
pub fn restore_snapshot(repo_root: &Path, state: &PublishRecoveryState) -> Result<(), String> {
    struct IndexLock(Option<PathBuf>);
    impl Drop for IndexLock {
        fn drop(&mut self) {
            if let Some(path) = &self.0 {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    let conflict = || {
        "RECOVERY_CONFLICT: worktree, branch, HEAD or index changed since the interrupted commit; recovery retained".to_string()
    };
    let path = index_path(repo_root)?;
    let runner = GitRunner::new(repo_root);
    if path != state.index_path
        || runner.current_branch() != state.head_ref
        || runner.rev_parse_head() != state.pre_head
    {
        return Err(conflict());
    }
    // Respect native Git's index lock as well as Pixel's repository lock.
    let lock_path = path.with_file_name(format!(
        "{}.lock",
        path.file_name().unwrap().to_string_lossy()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| format!("RECOVERY_CONFLICT: lock worktree index: {e}"))?;
    let mut lock = IndexLock(Some(lock_path.clone()));
    let original = state
        .pre_index_hex
        .as_ref()
        .map(hex::decode)
        .transpose()
        .map_err(|e| format!("GIT_FAILED: corrupt recovery snapshot (bad index hex): {e}"))?;
    let current = read_index(&path)?;
    let current_checksum = current.as_deref().map(checksum);
    if current != original && current_checksum != state.owned_index_checksum {
        return Err(conflict());
    }
    // No ref mutation belongs to index recovery. Recheck after taking index.lock.
    if runner.current_branch() != state.head_ref || runner.rev_parse_head() != state.pre_head {
        return Err(conflict());
    }
    match original {
        Some(bytes) => {
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|e| format!("GIT_FAILED: write restored index: {e}"))?;
            std::fs::rename(&lock_path, &path)
                .map_err(|e| format!("GIT_FAILED: publish restored index: {e}"))?;
            lock.0 = None;
        }
        None => match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("GIT_FAILED: remove restored index: {e}")),
        },
    }
    std::fs::File::open(path.parent().unwrap())
        .and_then(|dir| dir.sync_all())
        .map_err(|e| format!("GIT_FAILED: sync restored index: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_state(phase: RecoveryPhase) -> PublishRecoveryState {
        PublishRecoveryState {
            schema_version: 2,
            request_id: "test-req".to_string(),
            repo_key: "/test/repo".to_string(),
            phase,
            pre_head: Some("abc123".to_string()),
            index_path: PathBuf::from("/test/repo/.git/index"),
            head_ref: Some("main".into()),
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
        bumped.schema_version = 1;
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
    fn restore_snapshot_should_refuse_to_rewind_a_later_commit() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        std::fs::write(root.join("base.txt"), b"base").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        let pre_head = git(root, &["rev-parse", "HEAD"]);
        let pre_index = capture_index_snapshot(root)
            .unwrap()
            .expect("index should exist after commit");

        // A later commit makes the original ownership ambiguous; recovery
        // must preserve this history and its index.
        std::fs::write(root.join("new.txt"), b"new").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "mutate"]);
        let post_head = git(root, &["rev-parse", "HEAD"]);
        assert_ne!(pre_head, post_head);

        let state = PublishRecoveryState {
            schema_version: 2,
            request_id: "r1".to_string(),
            repo_key: "k".to_string(),
            phase: RecoveryPhase::CommitStarted,
            pre_head: Some(pre_head.clone()),
            index_path: index_path(root).unwrap(),
            head_ref: GitRunner::new(root).current_branch(),
            files: vec!["new.txt".to_string()],
            owned_index_checksum: None,
            mode: None,
            resolved_message: None,
            pre_index_hex: Some(pre_index),
        };
        let before_index = std::fs::read(index_path(root).unwrap()).unwrap();
        let error = restore_snapshot(root, &state).unwrap_err();
        assert!(error.starts_with("RECOVERY_CONFLICT:"), "{error}");
        assert_eq!(git(root, &["rev-parse", "HEAD"]), post_head);
        assert_eq!(
            std::fs::read(index_path(root).unwrap()).unwrap(),
            before_index
        );
    }

    /// The index checksum is what tells a recovery pass whether the index it
    /// left behind is still the one it wrote.
    #[test]
    fn index_checksum_hashes_the_git_index_and_changes_when_it_does() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        assert!(
            index_checksum(root).is_err(),
            "cannot identify a non-repository index"
        );
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
        let first = index_checksum(root)
            .unwrap()
            .expect("index exists after add");
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "{first}");
        assert_eq!(
            index_checksum(root).unwrap().as_deref(),
            Some(first.as_str()),
            "stable"
        );
        std::fs::write(root.join("b.txt"), b"b\n").unwrap();
        git(&["add", "b.txt"]);
        assert_ne!(index_checksum(root).unwrap().unwrap(), first);
    }
}
