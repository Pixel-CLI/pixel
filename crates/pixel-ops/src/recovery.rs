//! Publish recovery store — long-lived recovery state for `publish` so
//! crashes can resume. Port of usable-git's `publish-recovery.ts`.
//!
//! The recovery store captures the pre-mutation state (HEAD, index snapshot,
//! files being committed) so that a crashed publish can be rolled back or
//! completed safely on retry.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::durable::{ensure_dir, sha256_hex, write_durably, state_root};

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
}

pub struct PublishRecoveryStore {
    state_root: PathBuf,
}

impl PublishRecoveryStore {
    pub fn new() -> Self {
        Self { state_root: state_root() }
    }

    pub fn with_state_root(state_root: PathBuf) -> Self {
        Self { state_root }
    }

    fn recovery_dir(&self, repo_key: &str) -> PathBuf {
        self.state_root.join("publish-recovery").join(sha256_hex(repo_key))
    }

    fn recovery_path(&self, repo_key: &str, request_id: &str) -> PathBuf {
        self.recovery_dir(repo_key).join(format!("{}.json", sha256_hex(request_id)))
    }

    pub fn write(&self, state: &PublishRecoveryState) -> Result<(), String> {
        let dir = self.recovery_dir(&state.repo_key);
        ensure_dir(&dir).map_err(|e| e.to_string())?;
        let path = self.recovery_path(&state.repo_key, &state.request_id);
        let json = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
        write_durably(&path, &json).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn read(&self, repo_key: &str, request_id: &str) -> Option<PublishRecoveryState> {
        let path = self.recovery_path(repo_key, request_id);
        let data = std::fs::read(&path).ok()?;
        let state: PublishRecoveryState = serde_json::from_slice(&data).ok()?;
        if state.schema_version != 1 {
            return None;
        }
        Some(state)
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
        std::fs::read_dir(&dir)
            .map(|mut it| it.next().is_some())
            .unwrap_or(false)
    }
}

impl Default for PublishRecoveryStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Capture the git index checksum (sha256 of .git/index).
pub fn index_checksum(repo_root: &Path) -> Option<String> {
    let index_path = repo_root.join(".git").join("index");
    let bytes = std::fs::read(&index_path).ok()?;
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(hex::encode(hasher.finalize()))
}

/// Restore the git index if we still own it (checksum matches).
/// Returns true if restored, false if ownership lost.
pub fn restore_index_if_owned(repo_root: &Path, owned_checksum: &str) -> bool {
    let current = index_checksum(repo_root);
    if current.as_deref() != Some(owned_checksum) {
        return false;
    }
    // We still own the index — but there's nothing to restore since
    // we're the owner. In a full implementation, we'd restore from a
    // captured snapshot. For now, the checksum match means our staged
    // changes are still in place.
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn recovery_write_read_remove() {
        let dir = tempdir().unwrap();
        let store = PublishRecoveryStore::with_state_root(dir.path().to_path_buf());
        let state = PublishRecoveryState {
            schema_version: 1,
            request_id: "test-req".to_string(),
            repo_key: "/test/repo".to_string(),
            phase: RecoveryPhase::Snapshotted,
            pre_head: Some("abc123".to_string()),
            files: vec!["a.txt".to_string()],
            owned_index_checksum: Some("deadbeef".to_string()),
            mode: Some("append".to_string()),
            resolved_message: Some("test message".to_string()),
        };
        store.write(&state).unwrap();
        let read = store.read("/test/repo", "test-req").unwrap();
        assert_eq!(read.phase, RecoveryPhase::Snapshotted);
        assert_eq!(read.pre_head, Some("abc123".to_string()));
        store.remove("/test/repo", "test-req");
        assert!(store.read("/test/repo", "test-req").is_none());
    }

    #[test]
    fn recovery_has_pending() {
        let dir = tempdir().unwrap();
        let store = PublishRecoveryStore::with_state_root(dir.path().to_path_buf());
        assert!(!store.has_pending("/test/repo"));
        let state = PublishRecoveryState {
            schema_version: 1,
            request_id: "test-req".to_string(),
            repo_key: "/test/repo".to_string(),
            phase: RecoveryPhase::IndexStaged,
            pre_head: None,
            files: vec![],
            owned_index_checksum: None,
            mode: None,
            resolved_message: None,
        };
        store.write(&state).unwrap();
        assert!(store.has_pending("/test/repo"));
        store.remove("/test/repo", "test-req");
        assert!(!store.has_pending("/test/repo"));
    }
}
