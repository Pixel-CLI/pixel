//! `store.rs` — the `.pixel/history.db` SQLite store plus the shared fact
//! types every module in this crate returns.
//!
//! Schema (per PLAN.md Engine 2): `refs`, `commits`, `file_changes`, `hunks`,
//! `poison_paths`, `ingest_jobs`, `messages_fts` (FTS5, unicode61, NO prefix
//! index), and two trigram posting tables (`diff_grams`, `path_grams`) that
//! realize the "recall rowid-in-path trick": a gram's posting carries the
//! rowid of the `hunks` / `file_changes` row it came from, so a hit's path is
//! resolved at fetch time and stale rowids simply die on join.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use pixel_git::GitRunner;

/// The on-disk history database file name, relative to the repo root.
pub const HISTORY_DB_FILE: &str = "history.db";

/// `diff_state` bit/values on `commits`:
///   0 = metadata present, diff not yet ingested
///   1 = diff ingested (hunks present)
///   2 = diff skipped with a recorded `skip_note` (never silent)
///   3 = diff evicted by budget (metadata retained, findable + live-probe)
pub const DIFF_STATE_PENDING: i64 = 0;
pub const DIFF_STATE_INDEXED: i64 = 1;
pub const DIFF_STATE_SKIPPED: i64 = 2;
pub const DIFF_STATE_EVICTED: i64 = 3;

/// `commits.reach` bitmask — "code that exists nowhere reachable" is a
/// filter flag, not a special case.
pub const REACH_BRANCH: i64 = 1;
pub const REACH_REMOTE: i64 = 2;
pub const REACH_TAG: i64 = 4;
pub const REACH_STASH: i64 = 8;
pub const REACH_REFLOG_ONLY: i64 = 16;

/// Budget the default eviction budget (bytes of diff residue kept).
pub const DEFAULT_DIFF_BUDGET_BYTES: u64 = 150 * 1024 * 1024;

/// The on-disk schema version, stamped via `PRAGMA user_version`. Bump this
/// whenever the DDL changes. On open, a mismatch (or a pre-versioned DB that
/// already has rows) routes through the corrupt-rebuild path so every poisoned
/// DB self-heals on next open — no manual `rm` required.
pub const FACTS_SCHEMA_VERSION: i64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum FactsError {
    #[error("rusqlite: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("git: {0}")]
    Git(#[from] pixel_git::GitError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Msg(String),
}

pub type Result<T> = std::result::Result<T, FactsError>;

impl From<&str> for FactsError {
    fn from(s: &str) -> Self {
        FactsError::Msg(s.to_string())
    }
}

impl From<String> for FactsError {
    fn from(s: String) -> Self {
        FactsError::Msg(s)
    }
}

/// Every response carries this so callers know how much of history is covered
/// and whether the ingest thread has caught up to the current refs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexState {
    /// One of `"phase_a" | "phase_b" | "phase_c" | "fresh"`.
    pub phase: String,
    pub commits_indexed: u64,
    pub total_commits: u64,
    /// Fraction (0.0..=1.0) of commits whose diff text has been ingested.
    pub diff_indexed_pct: f64,
    /// True when ingest is caught up to the current refs (no pending work).
    pub fresh: bool,
    /// The on-disk schema version (PRAGMA user_version) this store was opened
    /// with — lets visibility report it.
    pub schema_version: i64,
}

impl IndexState {
    pub fn empty() -> Self {
        IndexState {
            phase: "phase_a".into(),
            commits_indexed: 0,
            total_commits: 0,
            diff_indexed_pct: 0.0,
            fresh: false,
            schema_version: FACTS_SCHEMA_VERSION,
        }
    }
}

/// A single commit reference (shortened oid + subject + timestamp) used by
/// lifecycle and search responses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommitRef {
    pub oid: String,
    pub at: String,
    pub subject: String,
}

/// The store: a SQLite connection to `.pixel/history.db` plus the git runner
/// used by the live-probe / present-at-HEAD paths that cannot be answered from
/// the index alone.
pub struct FactsStore {
    pub(crate) conn: Connection,
    root: PathBuf,
    runner: GitRunner,
    path: PathBuf,
}

impl FactsStore {
    /// Open (creating schema if needed) the history db at `root/.pixel/history.db`.
    /// Creates `.pixel` if absent. Self-healing: on corruption the db is
    /// removed and rebuilt from scratch (it is derived data, never
    /// load-bearing for correctness).
    pub fn open(root: &Path) -> Result<Self> {
        let root = root.to_path_buf();
        let pixel_dir = root.join(".pixel");
        std::fs::create_dir_all(&pixel_dir)?;
        let path = pixel_dir.join(HISTORY_DB_FILE);
        // Self-healing: rebuild on structural corruption OR a schema-version
        // mismatch OR a pre-versioned DB that already has rows. The db is
        // derived data, never load-bearing for correctness, so wiping it is
        // always safe — and this auto-heals every poisoned DB on next open
        // with no manual `rm` required.
        let rebuild = match Self::needs_rebuild(&path) {
            Ok(b) => b,
            Err(_) => true, // can't even read the version → corrupt → rebuild
        };
        let conn = if rebuild {
            Self::remove_db(&path);
            Self::open_conn(&path)?
        } else {
            match Self::open_conn(&path) {
                Ok(c) => c,
                Err(_) => {
                    Self::remove_db(&path);
                    Self::open_conn(&path)?
                }
            }
        };
        Ok(FactsStore {
            conn,
            runner: GitRunner::new(&root),
            path,
            root,
        })
    }

    /// True when the on-disk db at `path` must be rebuilt: the schema version
    /// (PRAGMA user_version) is missing or mismatched, or the db is
    /// pre-versioned (user_version 0) but already holds rows (a poisoned DB
    /// written by an older build). An empty pre-versioned db is fine — it just
    /// gets stamped on open.
    fn needs_rebuild(path: &Path) -> Result<bool> {
        if !path.exists() {
            return Ok(false);
        }
        // READ_WRITE (not READ_ONLY) so a WAL-mode db with a live -wal file is
        // readable; the file exists so CREATE is unnecessary.
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if version == FACTS_SCHEMA_VERSION {
            return Ok(false);
        }
        if version == 0 {
            // Pre-versioned. Rebuild only if it already has rows; an empty one
            // is stamped in place on open.
            let has_rows: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='commits'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if has_rows == 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn remove_db(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    fn open_conn(path: &Path) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(DDL)?;
        conn.pragma_update(None, "user_version", FACTS_SCHEMA_VERSION)?;
        Ok(conn)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub(crate) fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn runner(&self) -> &GitRunner {
        &self.runner
    }

    /// Snapshot of how much of history is indexed. Cheap: a few counts.
    pub fn index_state(&self) -> IndexState {
        let (total, indexed, _skipped, pending_diff) = self.diff_counts();
        let phase = self.current_phase();
        // diff_indexed_pct = fraction of non-structurally-skipped commits that
        // have their diff text ingested (phase C progress).
        let diff_total = indexed + pending_diff;
        let diff_indexed_pct = if diff_total == 0 {
            0.0
        } else {
            (indexed as f64) / (diff_total as f64)
        };
        IndexState {
            phase: phase.to_string(),
            commits_indexed: total as u64,
            total_commits: total as u64,
            diff_indexed_pct,
            fresh: pending_diff == 0 && self.phase_a_fresh() && self.phase_b_done(),
            schema_version: FACTS_SCHEMA_VERSION,
        }
    }

    fn current_phase(&self) -> &'static str {
        if !self.phase_a_fresh() {
            return "phase_a";
        }
        if !self.phase_b_done() {
            return "phase_b";
        }
        let pending_diff: i64 = self
            .conn
            .query_row(
                "SELECT count(*) FROM commits WHERE diff_state = ?1",
                [DIFF_STATE_PENDING],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if pending_diff > 0 {
            "phase_c"
        } else {
            "fresh"
        }
    }

    fn phase_a_done(&self) -> bool {
        let status: Option<String> = self
            .conn
            .query_row(
                "SELECT status FROM ingest_jobs WHERE phase = 'A'",
                [],
                |r| r.get(0),
            )
            .ok();
        matches!(status.as_deref(), Some("done"))
    }

    /// Phase A is 'done' AND its recorded refs hash still matches the current
    /// refs. A ref move since the last phase-A run makes us stale (phase_a
    /// again) even though the ingest_jobs row says 'done' — this is what fixes
    /// the frozen-at-commit-11 class permanently.
    fn phase_a_fresh(&self) -> bool {
        if !self.phase_a_done() {
            return false;
        }
        match self.current_refs_hash() {
            Ok(current) => {
                let stored: Option<String> = self
                    .conn
                    .query_row(
                        "SELECT ref_hash FROM ingest_jobs WHERE phase = 'A'",
                        [],
                        |r| r.get(0),
                    )
                    .ok();
                stored.as_deref() == Some(current.as_str())
            }
            Err(_) => false,
        }
    }

    /// xxh3 hash of the current refs state (`for-each-ref` + HEAD). Stored at
    /// phase-A completion; a differing hash means refs moved and phase A must
    /// re-run.
    pub(crate) fn current_refs_hash(&self) -> Result<String> {
        let mut buf = Vec::new();
        let refs = self
            .runner
            .run(&["for-each-ref", "--format=%(refname)%00%(objectname)"])?;
        buf.extend_from_slice(&refs);
        let head = self.runner.run(&["rev-parse", "HEAD"])?;
        buf.extend_from_slice(&head);
        let h = xxhash_rust::xxh3::xxh3_64(&buf);
        Ok(format!("{:016x}", h))
    }

    fn phase_b_done(&self) -> bool {
        let status: Option<String> = self
            .conn
            .query_row(
                "SELECT status FROM ingest_jobs WHERE phase = 'B'",
                [],
                |r| r.get(0),
            )
            .ok();
        matches!(status.as_deref(), Some("done"))
    }

    fn diff_counts(&self) -> (i64, i64, i64, i64) {
        let total = self
            .conn
            .query_row("SELECT count(*) FROM commits", [], |r| r.get(0))
            .unwrap_or(0);
        let indexed = self
            .conn
            .query_row(
                "SELECT count(*) FROM commits WHERE diff_state = ?1",
                [DIFF_STATE_INDEXED],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let skipped = self
            .conn
            .query_row(
                "SELECT count(*) FROM commits WHERE diff_state = ?1",
                [DIFF_STATE_SKIPPED],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let pending_diff = self
            .conn
            .query_row(
                "SELECT count(*) FROM commits WHERE diff_state = ?1",
                [DIFF_STATE_PENDING],
                |r| r.get(0),
            )
            .unwrap_or(0);
        (total, indexed, skipped, pending_diff)
    }

    /// Close the connection (flushes WAL).
    pub fn close(self) {
        let _ = self.conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        drop(self.conn);
    }
}

/// The schema. FTS5 is used ONLY for commit messages (unicode61, no prefix
/// index). Diff/path text uses trigram posting tables.
const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS refs (
  ref    TEXT PRIMARY KEY,
  oid    TEXT NOT NULL,
  kind   TEXT NOT NULL,
  indexed_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS commits (
  id INTEGER PRIMARY KEY,
  oid TEXT NOT NULL UNIQUE,
  parents TEXT NOT NULL DEFAULT '',
  author TEXT NOT NULL DEFAULT '',
  committed_at TEXT NOT NULL DEFAULT '',
  message TEXT NOT NULL DEFAULT '',
  reach INTEGER NOT NULL DEFAULT 0,
  diff_state INTEGER NOT NULL DEFAULT 0,
  skip_note TEXT
);
CREATE INDEX IF NOT EXISTS commits_oid ON commits (oid);
CREATE INDEX IF NOT EXISTS commits_diff_state ON commits (diff_state);
CREATE INDEX IF NOT EXISTS commits_committed_at ON commits (committed_at);

CREATE TABLE IF NOT EXISTS file_changes (
  id INTEGER PRIMARY KEY,
  commit_id INTEGER NOT NULL REFERENCES commits (id),
  path TEXT NOT NULL,
  status TEXT NOT NULL,
  old_path TEXT,
  UNIQUE (commit_id, path)
);
CREATE INDEX IF NOT EXISTS file_changes_path ON file_changes (path);
CREATE INDEX IF NOT EXISTS file_changes_commit ON file_changes (commit_id);
CREATE INDEX IF NOT EXISTS file_changes_status ON file_changes (status);

CREATE TABLE IF NOT EXISTS hunks (
  id INTEGER PRIMARY KEY,
  commit_id INTEGER NOT NULL REFERENCES commits (id),
  path TEXT NOT NULL,
  added TEXT NOT NULL DEFAULT '',
  removed TEXT NOT NULL DEFAULT '',
  truncated INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS hunks_commit ON hunks (commit_id);

CREATE TABLE IF NOT EXISTS poison_paths (
  path TEXT PRIMARY KEY,
  reason TEXT NOT NULL,
  learned_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS ingest_jobs (
  id INTEGER PRIMARY KEY,
  phase TEXT NOT NULL UNIQUE,
  cursor TEXT,
  status TEXT NOT NULL DEFAULT 'pending',
  ref_hash TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

-- Transient: reach bitmask accumulation keyed by oid during enumeration.
CREATE TABLE IF NOT EXISTS reach_map (
  oid TEXT PRIMARY KEY,
  bits INTEGER NOT NULL DEFAULT 0
);

-- FTS5 for commit messages only (unicode61, NO prefix index per PLAN.md).
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
  message,
  content='commits',
  content_rowid='id',
  tokenize="unicode61 tokenchars '_'"
);

-- Trigram segments: the "recall rowid-in-path trick". `diff_grams.hash` is a
-- gram over hunks.added/removed text, `diff_grams.hunk_id` is the hunks rowid
-- that carried it. `path_grams` is the same for file_changes.path.
CREATE TABLE IF NOT EXISTS diff_grams (
  hash INTEGER NOT NULL,
  hunk_id INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS diff_grams_hash ON diff_grams (hash);
CREATE INDEX IF NOT EXISTS diff_grams_hunk ON diff_grams (hunk_id);

CREATE TABLE IF NOT EXISTS path_grams (
  hash INTEGER NOT NULL,
  change_id INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS path_grams_hash ON path_grams (hash);
CREATE INDEX IF NOT EXISTS path_grams_change ON path_grams (change_id);
"#;

/// Shorten an oid to the conventional 12-char display form.
pub fn short_oid(oid: &str) -> String {
    let trimmed = oid.trim();
    if trimmed.len() > 12 {
        trimmed[..12].to_string()
    } else {
        trimmed.to_string()
    }
}

/// Extract the subject line (first line) of a commit message.
pub fn subject_of(message: &str) -> &str {
    message.lines().next().unwrap_or("").trim()
}
