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

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::durable::{ensure_dir, sha256_hex, write_durably, write_new_durably, state_root};

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
    Resume { phase: JournalPhase, result: Option<serde_json::Value> },
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
        Self { state_root: state_root() }
    }

    pub fn with_state_root(state_root: PathBuf) -> Self {
        Self { state_root }
    }

    fn journals_dir(&self, repo_key: &str) -> PathBuf {
        self.state_root.join("journals").join(sha256_hex(repo_key))
    }

    fn record_path(&self, repo_key: &str, request_id: &str) -> PathBuf {
        self.journals_dir(repo_key).join(format!("{}.json", sha256_hex(request_id)))
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

        // Check for existing record.
        if let Ok(data) = std::fs::read(&path) {
            let existing: JournalRecord = serde_json::from_slice(&data)
                .map_err(|e| format!("corrupt journal record: {e}"))?;

            // Idempotency check.
            if existing.operation != operation
                || existing.repo_key != repo_key
                || existing.input_hash != input_hash
            {
                return Err(format!(
                    "idempotency conflict: requestId {request_id} already used with different operation/input"
                ));
            }

            return Ok(match existing.phase {
                JournalPhase::Terminal => {
                    BeginOutcome::Replay(existing.result.unwrap_or(serde_json::Value::Null))
                }
                phase => BeginOutcome::Resume {
                    phase,
                    result: existing.result,
                },
            });
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
        let json = serde_json::to_vec_pretty(&record)
            .map_err(|e| e.to_string())?;
        write_new_durably(&path, &json).map_err(|e| e.to_string())?;
        Ok(BeginOutcome::Start)
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
        let mut record: JournalRecord = serde_json::from_slice(&data)
            .map_err(|e| format!("parse journal: {e}"))?;
        record.phase = phase;
        record.updated_at = now_iso();
        if result.is_some() {
            record.result = result;
        }
        let json = serde_json::to_vec_pretty(&record)
            .map_err(|e| e.to_string())?;
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
        if !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
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
        records.sort_by(|a, b| b.1.cmp(&a.1));
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
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{secs}")
}

fn parse_iso_ms(s: &str) -> Option<u64> {
    s.parse::<u64>().ok()
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
        let outcome = j.begin("req-1", JournalOperation::Publish, "repo-key", "hash-1").unwrap();
        assert!(matches!(outcome, BeginOutcome::Start));
        let record = j.read("repo-key", "req-1").unwrap();
        assert_eq!(record.phase, JournalPhase::Started);
    }

    #[test]
    fn begin_replays_terminal() {
        let dir = tempdir().unwrap();
        let j = make_journal(dir.path());
        j.begin("req-2", JournalOperation::Publish, "repo-key", "hash-2").unwrap();
        j.transition("req-2", "repo-key", JournalPhase::IndexStaged, None).unwrap();
        j.complete("req-2", "repo-key", serde_json::json!({"ok": true})).unwrap();
        let outcome = j.begin("req-2", JournalOperation::Publish, "repo-key", "hash-2").unwrap();
        match outcome {
            BeginOutcome::Replay(val) => assert_eq!(val, serde_json::json!({"ok": true})),
            _ => panic!("expected Replay"),
        }
    }

    #[test]
    fn begin_resumes_intermediate() {
        let dir = tempdir().unwrap();
        let j = make_journal(dir.path());
        j.begin("req-3", JournalOperation::Push, "repo-key", "hash-3").unwrap();
        j.transition("req-3", "repo-key", JournalPhase::PushStarted, Some(serde_json::json!({"meta": "recovery"}))).unwrap();
        let outcome = j.begin("req-3", JournalOperation::Push, "repo-key", "hash-3").unwrap();
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
        j.begin("req-4", JournalOperation::Publish, "repo-key", "hash-a").unwrap();
        let err = j.begin("req-4", JournalOperation::Push, "repo-key", "hash-b").unwrap_err();
        assert!(err.contains("idempotency"));
    }

    #[test]
    fn transition_durably_updates() {
        let dir = tempdir().unwrap();
        let j = make_journal(dir.path());
        j.begin("req-5", JournalOperation::Publish, "repo-key", "hash-5").unwrap();
        j.transition("req-5", "repo-key", JournalPhase::IndexStaged, None).unwrap();
        let record = j.read("repo-key", "req-5").unwrap();
        assert_eq!(record.phase, JournalPhase::IndexStaged);
    }

    #[test]
    fn validate_request_id_rejects_bad_chars() {
        let dir = tempdir().unwrap();
        let j = make_journal(dir.path());
        assert!(j.begin("req with spaces", JournalOperation::Publish, "k", "h").is_err());
        assert!(j.begin("", JournalOperation::Publish, "k", "h").is_err());
        assert!(j.begin(&"x".repeat(129), JournalOperation::Publish, "k", "h").is_err());
        assert!(j.begin("good-id.123", JournalOperation::Publish, "k", "h").is_ok());
    }
}
