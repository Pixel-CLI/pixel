// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Source adapters: one per LLM CLI transcript store.

pub mod claude;
pub mod codex;
pub mod cursor;
pub mod devin;
pub mod gemini;
pub mod opencode;
pub mod pi;
pub mod zcode;

use std::path::PathBuf;

use crate::model::{UnifiedSession, UnifiedTurn};

/// One ingestible unit — a transcript file (JSONL sources) or a database
/// cursor partition (SQLite sources).
#[derive(Debug, Clone)]
pub struct SourceUnit {
    /// Stable key into `ingest_state` (absolute path, or `db:<path>#<name>`).
    pub unit_key: String,
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ms: i64,
}

/// What happened to a unit since the last ingest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    New,
    /// Grew append-only; resume parsing from this byte offset.
    Appended {
        from: u64,
    },
    /// Shrank or changed in place; re-parse from scratch.
    Rewritten,
    Unchanged,
}

/// How the store should apply one parsed session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOp {
    /// Delete any prior copy and insert this one wholesale (full parses,
    /// and SQLite sources that re-materialize touched sessions).
    Replace,
    /// Append these turns to the existing session (append-only JSONL tail).
    Append,
}

pub struct ParsedSession {
    pub op: SessionOp,
    pub session: UnifiedSession,
    pub turns: Vec<UnifiedTurn>,
}

/// Everything a parse pass produced. A unit may hold several sessions
/// (SQLite databases, Gemini's single history file).
pub struct ParseOutput {
    pub sessions: Vec<ParsedSession>,
    /// Complete lines that were not valid JSON (a writer that crashed
    /// mid-flush). Their bytes are consumed and never re-read, so this
    /// count is the only trace they leave: an adapter must report it
    /// instead of swallowing the record.
    pub skipped_records: usize,
    /// Byte offset up to which complete lines were consumed — the resume
    /// point for the next `Appended` pass. A partial trailing line (record
    /// still being written) is never counted. SQLite sources report their
    /// file size.
    pub consumed_bytes: u64,
    /// Adapter-owned resume cursor (SQLite monotonic id). Takes precedence
    /// over the `make_cursor` hook when `Some`.
    pub cursor: Option<String>,
}

#[derive(Debug)]
pub enum IngestError {
    Io(std::io::Error),
    Store(rusqlite::Error),
    Other(String),
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestError::Io(e) => write!(f, "io: {e}"),
            IngestError::Store(e) => write!(f, "store: {e}"),
            IngestError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for IngestError {}

impl From<std::io::Error> for IngestError {
    fn from(e: std::io::Error) -> Self {
        IngestError::Io(e)
    }
}

impl From<rusqlite::Error> for IngestError {
    fn from(e: rusqlite::Error) -> Self {
        IngestError::Store(e)
    }
}

pub trait SourceAdapter {
    fn agent(&self) -> &'static str;

    /// Enumerate every ingestible unit currently on disk.
    fn discover(&self) -> Result<Vec<SourceUnit>, IngestError>;

    /// Compare a unit against its recorded ingest state.
    fn classify(&self, unit: &SourceUnit, state: Option<&crate::store::IngestState>) -> Change {
        match state {
            None => Change::New,
            Some(st) => {
                if st.file_size == unit.size as i64 && st.mtime_ms == unit.mtime_ms {
                    Change::Unchanged
                } else if (unit.size as i64) > st.file_size {
                    Change::Appended {
                        from: st.bytes_ingested as u64,
                    }
                } else {
                    Change::Rewritten
                }
            }
        }
    }

    /// Parse a unit into sessions + turns. For `Appended`, only new content
    /// since the recorded state is returned.
    fn parse(
        &self,
        unit: &SourceUnit,
        change: Change,
        state: Option<&crate::store::IngestState>,
    ) -> Result<ParseOutput, IngestError>;

    /// Opaque resume cursor recorded after a successful parse. JSONL
    /// adapters store a tail hash guarding the append assumption; SQLite
    /// adapters store their monotonic id.
    fn make_cursor(&self, _unit: &SourceUnit, _consumed: u64) -> Option<String> {
        None
    }

    /// Whether resuming from `state.bytes_ingested` is actually safe. A
    /// file that grew after being rewritten in place (session resume /
    /// compaction) fails this and is re-parsed from scratch.
    fn append_valid(&self, _unit: &SourceUnit, _state: &crate::store::IngestState) -> bool {
        true
    }
}

/// xxh3 hex of the last up-to-4KiB before `end` — the JSONL append guard.
pub fn file_tail_hash(path: &std::path::Path, end: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let start = end.saturating_sub(4096);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = vec![0u8; (end - start) as usize];
    f.read_exact(&mut buf).ok()?;
    Some(format!("{:016x}", xxhash_rust::xxh3::xxh3_64(&buf)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_error_should_display_its_cause() {
        let io = IngestError::Io(std::io::Error::other("disk"));
        assert_eq!(io.to_string(), "io: disk");
        assert_eq!(IngestError::Other("corrupt".into()).to_string(), "corrupt");
        let store = IngestError::Store(rusqlite::Error::InvalidQuery);
        assert_eq!(
            store.to_string(),
            format!("store: {}", rusqlite::Error::InvalidQuery)
        );
    }
}
