// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Operation journal — durable, idempotent request journal for mutations.
//!
//! Port of usable-git's `operation-journal.ts`. Every mutation begins by
//! journaling `started`, transitions through phases, and completes with
//! `terminal`. On crash recovery, `begin` returns `Resume` or `Replay`
//! so the operation can restart or replay the terminal result.
//!
//! Durability: temp → fsync → rename → dir fsync on every write.
//! Idempotency: (repoKey, requestId, inputHash) must be stable.
//! Retention: 30 days / 1000 terminal records.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::durable::{ensure_dir, sha256_hex, state_root, write_durably, write_new_durably};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalOperation {
    Publish,
    Push,
    Branch,
    Update,
    Ship,
    Sync,
    Rewrite,
}

impl std::fmt::Display for JournalOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalOperation::Publish => write!(f, "publish"),
            JournalOperation::Push => write!(f, "push"),
            JournalOperation::Branch => write!(f, "branch"),
            JournalOperation::Update => write!(f, "update"),
            JournalOperation::Ship => write!(f, "ship"),
            JournalOperation::Sync => write!(f, "sync"),
            JournalOperation::Rewrite => write!(f, "rewrite"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalPhase {
    Started,
    IndexStaged,
    CommitObserved,
    PushStarted,
    RefUpdateStarted,
    Terminal,
}

impl std::fmt::Display for JournalPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalPhase::Started => write!(f, "started"),
            JournalPhase::IndexStaged => write!(f, "index_staged"),
            JournalPhase::CommitObserved => write!(f, "commit_observed"),
            JournalPhase::PushStarted => write!(f, "push_started"),
            JournalPhase::RefUpdateStarted => write!(f, "ref_update_started"),
            JournalPhase::Terminal => write!(f, "terminal"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalRecord {
    pub schema_version: u32,
    pub request_id: String,
    pub operation: JournalOperation,
    pub repo_key: String,
    pub input_hash: String,
    pub phase: JournalPhase,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

/// Outcome of `begin`: either start fresh, resume from a phase, or replay
/// a terminal result.
#[derive(Debug, Clone)]
pub enum BeginOutcome {
    /// No existing record — start a new operation.
    Start,
    /// Existing record at an intermediate phase — resume from here.
    Resume {
        phase: JournalPhase,
        result: Option<serde_json::Value>,
    },
    /// Terminal record with a result — replay it (idempotent retry).
    Replay(serde_json::Value),
}

const RETENTION_MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const RETENTION_MAX_COUNT: usize = 1000;

pub struct OperationJournal {
    state_root: PathBuf,
}

impl OperationJournal {
    pub fn new() -> Self {
        Self {
            state_root: state_root(),
        }
    }

    pub fn with_state_root(state_root: PathBuf) -> Self {
        Self { state_root }
    }

    fn journals_dir(&self, repo_key: &str) -> PathBuf {
        self.state_root.join("journals").join(sha256_hex(repo_key))
    }

    fn record_path(&self, repo_key: &str, request_id: &str) -> PathBuf {
        self.journals_dir(repo_key)
            .join(format!("{}.json", sha256_hex(request_id)))
    }

    /// Begin an operation. If a record already exists for this
    /// (repoKey, requestId), it must have the same operation + inputHash
    /// or an error is returned. If terminal, the result is replayed.
    pub fn begin(
        &self,
        request_id: &str,
        operation: JournalOperation,
        repo_key: &str,
        input_hash: &str,
    ) -> Result<BeginOutcome, String> {
        self.validate_request_id(request_id)?;
        let dir = self.journals_dir(repo_key);
        ensure_dir(&dir).map_err(|e| e.to_string())?;
        let path = self.record_path(repo_key, request_id);

        // An existing record is the truth for this request: it is read,
        // never overwritten, and a record that cannot be read is an error
        // — falling through to the creation below would run the mutation a
        // second time under the same request id.
        if let Some(outcome) =
            Self::read_existing(&path, request_id, operation, repo_key, input_hash)?
        {
            return Ok(outcome);
        }

        // No existing record — create one.
        let now = now_iso();
        let record = JournalRecord {
            schema_version: 1,
            request_id: request_id.to_string(),
            operation,
            repo_key: repo_key.to_string(),
            input_hash: input_hash.to_string(),
            phase: JournalPhase::Started,
            created_at: now.clone(),
            updated_at: now,
            result: None,
        };
        let json = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
        if write_new_durably(&path, &json).map_err(|e| e.to_string())? {
            return Ok(BeginOutcome::Start);
        }
        // Lost the race with a concurrent writer: `write_new_durably`
        // reports `Ok(false)` rather than clobbering the record the winner
        // just created. That record — replay or resume — is the answer;
        // `Start` here would run the mutation twice.
        Self::read_existing(&path, request_id, operation, repo_key, input_hash)?.ok_or_else(|| {
            format!(
                "GIT_FAILED: journal record for request {request_id} is unreadable after a concurrent creation attempt at {}",
                path.display()
            )
        })
    }

    /// Interpret the record already on disk for this request, if any:
    /// replay a terminal result, resume from an intermediate phase, or
    /// fail on an idempotency conflict.
    ///
    /// `Ok(None)` means the file is genuinely absent. A record that is
    /// there but cannot be read or parsed is an error, never `None`:
    /// starting the operation over a record it cannot read is how a retry
    /// becomes a second mutation.
    fn read_existing(
        path: &Path,
        request_id: &str,
        operation: JournalOperation,
        repo_key: &str,
        input_hash: &str,
    ) -> Result<Option<BeginOutcome>, String> {
        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(format!(
                    "GIT_FAILED: journal record unreadable at {}: {e}",
                    path.display()
                ));
            }
        };
        let existing: JournalRecord = serde_json::from_slice(&data).map_err(|e| {
            format!(
                "GIT_FAILED: corrupt journal record at {}: {e}",
                path.display()
            )
        })?;

        // Idempotency check.
        if existing.operation != operation
            || existing.repo_key != repo_key
            || existing.input_hash != input_hash
        {
            return Err(format!(
                "idempotency conflict: requestId {request_id} already used with different operation/input"
            ));
        }

        Ok(Some(match existing.phase {
            JournalPhase::Terminal => {
                BeginOutcome::Replay(existing.result.unwrap_or(serde_json::Value::Null))
            }
            phase => BeginOutcome::Resume {
                phase,
                result: existing.result,
            },
        }))
    }

    /// Transition to a new phase, optionally storing recovery metadata.
    pub fn transition(
        &self,
        request_id: &str,
        repo_key: &str,
        phase: JournalPhase,
        result: Option<serde_json::Value>,
    ) -> Result<(), String> {
        let path = self.record_path(repo_key, request_id);
        let data = std::fs::read(&path).map_err(|e| format!("read journal: {e}"))?;
        let mut record: JournalRecord =
            serde_json::from_slice(&data).map_err(|e| format!("parse journal: {e}"))?;
        record.phase = phase;
        record.updated_at = now_iso();
        if result.is_some() {
            record.result = result;
        }
        let json = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
        write_durably(&path, &json).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Complete the operation with a terminal result.
    pub fn complete(
        &self,
        request_id: &str,
        repo_key: &str,
        result: serde_json::Value,
    ) -> Result<(), String> {
        self.transition(request_id, repo_key, JournalPhase::Terminal, Some(result))?;
        self.prune(repo_key)?;
        Ok(())
    }

    /// Read the current record (if any) for a request.
    pub fn read(&self, repo_key: &str, request_id: &str) -> Option<JournalRecord> {
        let path = self.record_path(repo_key, request_id);
        let data = std::fs::read(&path).ok()?;
        serde_json::from_slice(&data).ok()
    }

    fn validate_request_id(&self, id: &str) -> Result<(), String> {
        if id.is_empty() || id.len() > 128 {
            return Err("requestId must be 1-128 chars".to_string());
        }
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err("requestId must be [A-Za-z0-9._-]".to_string());
        }
        Ok(())
    }

    /// Prune old terminal records.
    fn prune(&self, repo_key: &str) -> Result<(), String> {
        let dir = self.journals_dir(repo_key);
        if !dir.exists() {
            return Ok(());
        }
        let mut records: Vec<(PathBuf, u64)> = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let data = match std::fs::read(&path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let record: JournalRecord = match serde_json::from_slice(&data) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if record.phase != JournalPhase::Terminal {
                continue; // Never prune non-terminal records.
            }
            let ts = parse_iso_ms(&record.updated_at).unwrap_or(0);
            records.push((path, ts));
        }
        records.sort_by_key(|(_, ts)| std::cmp::Reverse(*ts));
        let now = current_unix_ms();
        let mut kept = 0;
        for (path, ts) in &records {
            let age = now.saturating_sub(*ts);
            if age > RETENTION_MAX_AGE_MS || kept >= RETENTION_MAX_COUNT {
                let _ = std::fs::remove_file(path);
            } else {
                kept += 1;
            }
        }
        Ok(())
    }
}

impl Default for OperationJournal {
    fn default() -> Self {
        Self::new()
    }
}

fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!("{secs}")
}

fn parse_iso_ms(s: &str) -> Option<u64> {
    s.parse::<u64>().ok()
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    fn make_journal(dir: &Path) -> OperationJournal {
        OperationJournal::with_state_root(dir.to_path_buf())
    }

    #[test]
    fn begin_creates_started_record() {
        let dir = tempdir().unwrap();
        let j = make_journal(dir.path());
        let outcome = j
            .begin("req-1", JournalOperation::Publish, "repo-key", "hash-1")
            .unwrap();
        assert!(matches!(outcome, BeginOutcome::Start));
        let record = j.read("repo-key", "req-1").unwrap();
        assert_eq!(record.phase, JournalPhase::Started);
    }

    #[test]
    fn begin_replays_terminal() {
        let dir = tempdir().unwrap();
        let j = make_journal(dir.path());
        j.begin("req-2", JournalOperation::Publish, "repo-key", "hash-2")
            .unwrap();
        j.transition("req-2", "repo-key", JournalPhase::IndexStaged, None)
            .unwrap();
        j.complete("req-2", "repo-key", serde_json::json!({"ok": true}))
            .unwrap();
        let outcome = j
            .begin("req-2", JournalOperation::Publish, "repo-key", "hash-2")
            .unwrap();
        match outcome {
            BeginOutcome::Replay(val) => assert_eq!(val, serde_json::json!({"ok": true})),
            _ => panic!("expected Replay"),
        }
    }

    #[test]
    fn begin_resumes_intermediate() {
        let dir = tempdir().unwrap();
        let j = make_journal(dir.path());
        j.begin("req-3", JournalOperation::Push, "repo-key", "hash-3")
            .unwrap();
        j.transition(
            "req-3",
            "repo-key",
            JournalPhase::PushStarted,
            Some(serde_json::json!({"meta": "recovery"})),
        )
        .unwrap();
        let outcome = j
            .begin("req-3", JournalOperation::Push, "repo-key", "hash-3")
            .unwrap();
        match outcome {
            BeginOutcome::Resume { phase, result } => {
                assert_eq!(phase, JournalPhase::PushStarted);
                assert!(result.is_some());
            }
            _ => panic!("expected Resume"),
        }
    }

    #[test]
    fn begin_rejects_idempotency_conflict() {
        let dir = tempdir().unwrap();
        let j = make_journal(dir.path());
        j.begin("req-4", JournalOperation::Publish, "repo-key", "hash-a")
            .unwrap();
        let err = j
            .begin("req-4", JournalOperation::Push, "repo-key", "hash-b")
            .unwrap_err();
        assert!(err.contains("idempotency"));
    }

    /// The NotFound guard in `read_existing` must not swallow other IO
    /// errors: a directory at the record path is not "absent".
    #[test]
    fn read_existing_fails_on_non_notfound_io_error() {
        let dir = tempdir().unwrap();
        let j = make_journal(dir.path());
        let path = j.record_path("repo-key", "req-ioerr");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::create_dir(&path).unwrap();
        let err = OperationJournal::read_existing(
            &path,
            "req-ioerr",
            JournalOperation::Publish,
            "repo-key",
            "hash",
        )
        .unwrap_err();
        assert!(err.contains("GIT_FAILED"), "{err}");
        assert!(err.contains("unreadable"), "{err}");
    }

    /// A mismatch in only the operation is enough to be a conflict.
    #[test]
    fn read_existing_rejects_operation_mismatch_alone() {
        let dir = tempdir().unwrap();
        let j = make_journal(dir.path());
        j.begin("req-op", JournalOperation::Publish, "repo-key", "hash")
            .unwrap();
        let path = j.record_path("repo-key", "req-op");
        let err = OperationJournal::read_existing(
            &path,
            "req-op",
            JournalOperation::Push,
            "repo-key",
            "hash",
        )
        .unwrap_err();
        assert!(err.contains("idempotency"), "{err}");
    }

    /// A mismatch in only the input_hash is enough to be a conflict.
    #[test]
    fn read_existing_rejects_input_hash_mismatch_alone() {
        let dir = tempdir().unwrap();
        let j = make_journal(dir.path());
        j.begin("req-hash", JournalOperation::Publish, "repo-key", "hash")
            .unwrap();
        let path = j.record_path("repo-key", "req-hash");
        let err = OperationJournal::read_existing(
            &path,
            "req-hash",
            JournalOperation::Publish,
            "repo-key",
            "other-hash",
        )
        .unwrap_err();
        assert!(err.contains("idempotency"), "{err}");
    }

    /// The creation race reports `Ok(false)`; when the path it lost to is
    /// not a readable record, the loser must fail rather than `Start`.
    #[test]
    fn a_lost_creation_race_without_a_readable_record_is_an_error() {
        let dir = tempdir().unwrap();
        let j = make_journal(dir.path());
        let path = j.record_path("repo-key", "req-dangling");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A dangling symlink: `read` sees `NotFound` (so `begin` tries to
        // create the record), while `write_new_durably`'s hard link sees
        // the path as taken (`AlreadyExists`) — the race, deterministically.
        std::os::unix::fs::symlink(dir.path().join("gone.json"), &path).unwrap();
        let err = j
            .begin(
                "req-dangling",
                JournalOperation::Publish,
                "repo-key",
                "hash",
            )
            .unwrap_err();
        assert!(err.contains("GIT_FAILED"), "{err}");
    }

    /// Two callers with the same request id: the one that loses the
    /// creation race resumes the winner's record — never `Start`, which
    /// would run the mutation a second time.
    #[test]
    fn race_loser_resumes_the_record_the_winner_created() {
        let dir = tempdir().unwrap();
        let j = make_journal(dir.path());
        j.begin(
            "req-race",
            JournalOperation::Publish,
            "repo-key",
            "hash-race",
        )
        .unwrap();
        j.transition("req-race", "repo-key", JournalPhase::IndexStaged, None)
            .unwrap();

        // Exactly what `begin` does once `write_new_durably` says the race
        // was lost.
        let path = j.record_path("repo-key", "req-race");
        let outcome = OperationJournal::read_existing(
            &path,
            "req-race",
            JournalOperation::Publish,
            "repo-key",
            "hash-race",
        )
        .unwrap()
        .expect("the winner's record is on disk");
        match outcome {
            BeginOutcome::Resume { phase, result } => {
                assert_eq!(phase, JournalPhase::IndexStaged);
                assert!(result.is_none());
            }
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    #[test]
    fn transition_durably_updates() {
        let dir = tempdir().unwrap();
        let j = make_journal(dir.path());
        j.begin("req-5", JournalOperation::Publish, "repo-key", "hash-5")
            .unwrap();
        j.transition("req-5", "repo-key", JournalPhase::IndexStaged, None)
            .unwrap();
        let record = j.read("repo-key", "req-5").unwrap();
        assert_eq!(record.phase, JournalPhase::IndexStaged);
    }

    #[test]
    fn validate_request_id_rejects_bad_chars() {
        let dir = tempdir().unwrap();
        let j = make_journal(dir.path());
        assert!(
            j.begin("req with spaces", JournalOperation::Publish, "k", "h")
                .is_err()
        );
        assert!(j.begin("", JournalOperation::Publish, "k", "h").is_err());
        assert!(
            j.begin(&"x".repeat(129), JournalOperation::Publish, "k", "h")
                .is_err()
        );
        assert!(
            j.begin("good-id.123", JournalOperation::Publish, "k", "h")
                .is_ok()
        );
    }

    /// Timestamps written here are compared with wall-clock time by their
    /// readers; a placeholder (0, 1) would date every entry to 1970.
    #[test]
    fn current_unix_ms_is_the_current_unix_epoch_in_milliseconds() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let now = current_unix_ms();
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(
            now >= before && now <= after,
            "{before} <= {now} <= {after}"
        );
        assert!(now > 1_577_836_800_000, "{now}"); // 2020-01-01T00:00:00Z
    }
}
