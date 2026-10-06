// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! SQLite store for the recall corpus (sessions + turns + ingest state).
//!
//! Same rusqlite/WAL/additive-migration discipline as pixel-graph's
//! store. Turn text is stored denormalized here — sources rotate and get
//! deleted, so the corpus must outlive them.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Result, params};

use crate::model::{TsSource, UnifiedSession, UnifiedTurn};

pub struct RecallStore {
    conn: Connection,
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct IngestState {
    pub file_size: i64,
    pub mtime_ms: i64,
    pub bytes_ingested: i64,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: i64,
    pub agent: String,
    pub source_session_id: String,
    pub source_path: String,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub title: Option<String>,
    pub first_user_prompt: Option<String>,
    pub ts_first: Option<i64>,
    pub ts_last: Option<i64>,
    pub ts_source: TsSource,
    pub turn_count: i64,
    pub is_subagent: bool,
    pub parent_session_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct TurnRow {
    pub id: i64,
    pub session_id: i64,
    pub seq: i64,
    pub role: String,
    pub intent_source: Option<String>,
    pub ts: Option<i64>,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AgentStats {
    pub agent: String,
    pub sessions: i64,
    pub turns: i64,
    pub last_ingest_at: Option<i64>,
}

impl RecallStore {
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Ingest, embed backfill, and the daemon may write concurrently
        // from separate processes; WAL serializes writers, so a generous
        // timeout beats spurious SQLITE_BUSY failures on big transactions.
        conn.pragma_update(None, "busy_timeout", 60_000)?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self {
            conn,
            path: db_path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    // --- ingest state ---

    pub fn ingest_state(&self, agent: &str, unit_key: &str) -> Result<Option<IngestState>> {
        self.conn
            .query_row(
                "SELECT file_size, mtime_ms, bytes_ingested, cursor
                 FROM ingest_state WHERE agent = ?1 AND unit_key = ?2",
                params![agent, unit_key],
                |r| {
                    Ok(IngestState {
                        file_size: r.get(0)?,
                        mtime_ms: r.get(1)?,
                        bytes_ingested: r.get(2)?,
                        cursor: r.get(3)?,
                    })
                },
            )
            .optional()
    }

    /// Newest ingest timestamp for an agent across all its units — the
    /// watermark an on-demand catch-up resumes from. `None` when the agent
    /// has never ingested (unlike `stats`, which only sees agents that
    /// already have sessions).
    pub fn agent_last_ingest_at(&self, agent: &str) -> Result<Option<i64>> {
        self.conn.query_row(
            "SELECT MAX(last_ingest_at) FROM ingest_state WHERE agent = ?1",
            params![agent],
            |r| r.get(0),
        )
    }

    // --- session ingest (each call is one transaction) ---

    /// Replace a session wholesale: delete any prior row + turns, insert the
    /// new set, and record ingest state — atomically.
    pub fn replace_session(
        &mut self,
        session: &UnifiedSession,
        turns: &[UnifiedTurn],
        unit_key: &str,
        state: &IngestState,
    ) -> Result<i64> {
        let tx = self.conn.transaction()?;
        if let Some(old_id) = existing_session_id(&tx, session.agent, &session.source_session_id)? {
            tx.execute("DELETE FROM turns WHERE session_id = ?1", params![old_id])?;
            tx.execute("DELETE FROM sessions WHERE id = ?1", params![old_id])?;
        }
        let session_id = insert_session(&tx, session)?;
        insert_turns(&tx, session_id, 0, turns)?;
        finalize_session(&tx, session_id, session, turns.iter())?;
        upsert_state(&tx, session.agent, unit_key, state)?;
        tx.commit()?;
        Ok(session_id)
    }

    /// Append turns to an existing session (append-only JSONL growth). Falls
    /// back to a full insert when the session is not present yet.
    pub fn append_session(
        &mut self,
        session: &UnifiedSession,
        new_turns: &[UnifiedTurn],
        unit_key: &str,
        state: &IngestState,
    ) -> Result<i64> {
        let tx = self.conn.transaction()?;
        let session_id = match existing_session_id(&tx, session.agent, &session.source_session_id)?
        {
            Some(id) => id,
            None => insert_session(&tx, session)?,
        };
        let next_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM turns WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        insert_turns(&tx, session_id, next_seq, new_turns)?;
        finalize_session(&tx, session_id, session, new_turns.iter())?;
        upsert_state(&tx, session.agent, unit_key, state)?;
        tx.commit()?;
        Ok(session_id)
    }

    /// Record that a unit was seen but produced no sessions (empty file).
    pub fn touch_state(&self, agent: &str, unit_key: &str, state: &IngestState) -> Result<()> {
        upsert_state(&self.conn, agent, unit_key, state)
    }

    /// Link subagent sessions to their parents once both sides exist.
    pub fn link_subagents(&self, agent: &str) -> Result<usize> {
        self.conn.execute(
            "UPDATE sessions SET parent_session_id = (
                 SELECT p.id FROM sessions p
                 WHERE p.agent = sessions.agent
                   AND p.source_session_id = sessions.parent_source_session_id
                   AND p.is_subagent = 0
             )
             WHERE agent = ?1 AND is_subagent = 1
               AND parent_source_session_id IS NOT NULL
               AND parent_session_id IS NULL",
            params![agent],
        )
    }

    // --- queries ---

    pub fn sessions(
        &self,
        agent: Option<&str>,
        repo_prefix: Option<&str>,
        since_ms: Option<i64>,
        until_ms: Option<i64>,
        include_subagents: bool,
        limit: usize,
    ) -> Result<Vec<SessionRow>> {
        let mut sql = String::from(
            "SELECT id, agent, source_session_id, source_path, cwd, git_branch, title,
                    first_user_prompt, ts_first, ts_last, ts_source, turn_count,
                    is_subagent, parent_session_id
             FROM sessions WHERE 1=1",
        );
        let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(a) = agent {
            sql.push_str(" AND agent = ?");
            args.push(Box::new(a.to_string()));
        }
        if let Some(r) = repo_prefix {
            sql.push_str(" AND cwd LIKE ? || '%'");
            args.push(Box::new(r.to_string()));
        }
        if let Some(s) = since_ms {
            sql.push_str(" AND ts_last >= ?");
            args.push(Box::new(s));
        }
        if let Some(u) = until_ms {
            sql.push_str(" AND ts_first <= ?");
            args.push(Box::new(u));
        }
        if !include_subagents {
            sql.push_str(" AND is_subagent = 0");
        }
        sql.push_str(" ORDER BY ts_last DESC NULLS LAST LIMIT ?");
        args.push(Box::new(limit as i64));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(args.iter().map(AsRef::as_ref)),
            row_to_session,
        )?;
        rows.collect()
    }

    pub fn session_by_id(&self, id: i64) -> Result<Option<SessionRow>> {
        self.conn
            .query_row(
                "SELECT id, agent, source_session_id, source_path, cwd, git_branch, title,
                        first_user_prompt, ts_first, ts_last, ts_source, turn_count,
                        is_subagent, parent_session_id
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()
    }

    /// Resolve a session by unique source-id prefix (optionally scoped to an
    /// agent). Ambiguity returns every candidate so the caller can list them.
    pub fn sessions_by_prefix(&self, agent: Option<&str>, prefix: &str) -> Result<Vec<SessionRow>> {
        let mut sql = String::from(
            "SELECT id, agent, source_session_id, source_path, cwd, git_branch, title,
                    first_user_prompt, ts_first, ts_last, ts_source, turn_count,
                    is_subagent, parent_session_id
             FROM sessions WHERE source_session_id LIKE ?1 || '%'",
        );
        if agent.is_some() {
            sql.push_str(" AND agent = ?2");
        }
        sql.push_str(" ORDER BY ts_last DESC LIMIT 20");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = match agent {
            Some(a) => stmt.query_map(params![prefix, a], row_to_session)?,
            None => stmt.query_map(params![prefix], row_to_session)?,
        };
        rows.collect()
    }

    pub fn turns_for_session(
        &self,
        session_id: i64,
        seq_range: Option<(i64, i64)>,
    ) -> Result<Vec<TurnRow>> {
        let mut sql = String::from(
            "SELECT id, session_id, seq, role, intent_source, ts, text, truncated
             FROM turns WHERE session_id = ?1",
        );
        if seq_range.is_some() {
            sql.push_str(" AND seq >= ?2 AND seq <= ?3");
        }
        sql.push_str(" ORDER BY seq");
        let mut stmt = self.conn.prepare(&sql)?;
        let map = |r: &rusqlite::Row<'_>| -> Result<TurnRow> {
            Ok(TurnRow {
                id: r.get(0)?,
                session_id: r.get(1)?,
                seq: r.get(2)?,
                role: r.get(3)?,
                intent_source: r.get(4)?,
                ts: r.get(5)?,
                text: r.get(6)?,
                truncated: r.get::<_, i64>(7)? != 0,
            })
        };
        let rows = match seq_range {
            Some((lo, hi)) => stmt.query_map(params![session_id, lo, hi], map)?,
            None => stmt.query_map(params![session_id], map)?,
        };
        rows.collect()
    }

    pub fn stats(&self) -> Result<Vec<AgentStats>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.agent, COUNT(*), COALESCE(SUM(s.turn_count), 0),
                    (SELECT MAX(last_ingest_at) FROM ingest_state i WHERE i.agent = s.agent)
             FROM sessions s GROUP BY s.agent ORDER BY s.agent",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(AgentStats {
                agent: r.get(0)?,
                sessions: r.get(1)?,
                turns: r.get(2)?,
                last_ingest_at: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    pub fn total_turns(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
    }

    /// Turns newer than `after_id`, oldest first, for segment building.
    pub fn turns_for_indexing(&self, after_id: i64, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, text FROM turns WHERE id > ?1 ORDER BY id LIMIT ?2")?;
        let rows = stmt.query_map(params![after_id, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        rows.collect()
    }

    /// Read-only access for the search path (joins, ordered scans).
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    // --- embedding pipeline ---
    // turns.embedded: 0 = pending, 1 = embedded, 2 = skipped by policy.

    /// Apply the embedding policy: tool output, harness-injected user text,
    /// and subagent user prompts stay lexical-only.
    pub fn mark_policy_skips(&self) -> Result<usize> {
        self.conn.execute(
            "UPDATE turns SET embedded = 2 WHERE embedded = 0 AND (
                 role = 'tool'
                 OR (role = 'user' AND (
                     intent_source = 'orchestrator'
                     OR session_id IN (SELECT id FROM sessions WHERE is_subagent = 1)
                 ))
             )",
            [],
        )
    }

    /// Next batch of turns awaiting embedding, strictly after `after_id`.
    /// Keyset pagination is load-bearing: turns are only MARKED embedded at
    /// segment flush, so a head query would return the same batch forever
    /// between flushes (and re-chunk it every iteration).
    pub fn pending_embed(&self, after_id: i64, limit: usize) -> Result<Vec<EmbedTurn>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT t.id, t.text, s.agent, s.cwd, t.role
             FROM turns t JOIN sessions s ON s.id = t.session_id
             WHERE t.embedded = 0 AND t.id > ?2 ORDER BY t.id LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64, after_id], |r| {
            Ok(EmbedTurn {
                turn_id: r.get(0)?,
                text: r.get(1)?,
                agent: r.get(2)?,
                cwd: r.get(3)?,
                role: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn embed_backlog(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM turns WHERE embedded = 0", [], |r| {
                r.get(0)
            })
    }

    /// Register chunk rows for a turn; returns their chunk ids.
    pub fn insert_chunks(&self, turn_id: i64, offsets: &[(usize, usize)]) -> Result<Vec<i64>> {
        let mut ids = Vec::with_capacity(offsets.len());
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO vector_chunks (turn_id, chunk_seq, chunk_start) VALUES (?1, ?2, ?3)",
        )?;
        for (seq, (start, _)) in offsets.iter().enumerate() {
            stmt.execute(params![turn_id, seq as i64, *start as i64])?;
            ids.push(self.conn.last_insert_rowid());
        }
        Ok(ids)
    }

    pub fn mark_embedded(&self, turn_ids: &[i64]) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("UPDATE turns SET embedded = 1 WHERE id = ?1")?;
        for id in turn_ids {
            stmt.execute(params![id])?;
        }
        Ok(())
    }

    /// Heal an interrupted embed run: drop chunk rows never persisted to a
    /// vector segment, so their turns re-chunk cleanly.
    pub fn drop_orphan_chunks(&self, last_persisted_chunk_id: i64) -> Result<usize> {
        self.conn.execute(
            "DELETE FROM vector_chunks WHERE chunk_id > ?1",
            params![last_persisted_chunk_id],
        )
    }

    /// Reset embedding state entirely (model change / --rebuild).
    pub fn reset_embeddings(&self) -> Result<()> {
        self.conn.execute("DELETE FROM vector_chunks", [])?;
        self.conn
            .execute("UPDATE turns SET embedded = 0 WHERE embedded != 0", [])?;
        Ok(())
    }

    /// Chunk ids allowed by the metadata filters (for KNN pre-filtering).
    pub fn allowed_chunk_ids(
        &self,
        filter_sql: &str,
        args: &[&dyn rusqlite::types::ToSql],
    ) -> Result<std::collections::HashSet<i64>> {
        let sql = format!(
            "SELECT c.chunk_id FROM vector_chunks c
             JOIN turns t ON t.id = c.turn_id
             JOIN sessions s ON s.id = t.session_id
             WHERE 1=1 {filter_sql}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter().copied()), |r| {
            r.get::<_, i64>(0)
        })?;
        rows.collect()
    }

    /// Map chunk ids back to (turn_id, chunk_start).
    pub fn chunk_turns(&self, chunk_ids: &[i64]) -> Result<Vec<(i64, i64, i64)>> {
        let mut out = Vec::with_capacity(chunk_ids.len());
        let mut stmt = self.conn.prepare_cached(
            "SELECT chunk_id, turn_id, COALESCE(chunk_start, 0) FROM vector_chunks WHERE chunk_id = ?1",
        )?;
        for id in chunk_ids {
            if let Some(row) = stmt
                .query_row(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?
            {
                out.push(row);
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub struct EmbedTurn {
    pub turn_id: i64,
    pub text: String,
    pub agent: String,
    pub cwd: Option<String>,
    pub role: String,
}

fn existing_session_id(
    conn: &Connection,
    agent: &str,
    source_session_id: &str,
) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM sessions WHERE agent = ?1 AND source_session_id = ?2",
        params![agent, source_session_id],
        |r| r.get(0),
    )
    .optional()
}

fn insert_session(conn: &Connection, s: &UnifiedSession) -> Result<i64> {
    conn.execute(
        "INSERT INTO sessions (agent, source_session_id, source_path, cwd, git_branch,
                               title, ts_source, is_subagent, parent_source_session_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            s.agent,
            s.source_session_id,
            s.source_path,
            s.cwd,
            s.git_branch,
            s.title,
            s.ts_source.as_str(),
            s.is_subagent as i64,
            s.parent_source_session_id,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn insert_turns(
    conn: &Connection,
    session_id: i64,
    start_seq: i64,
    turns: &[UnifiedTurn],
) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO turns (session_id, seq, role, intent_source, ts, text, text_len,
                            truncated, source_byte_start, source_byte_len)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    for (i, t) in turns.iter().enumerate() {
        stmt.execute(params![
            session_id,
            start_seq + i as i64,
            t.role.as_str(),
            t.intent_source.map(super::model::IntentSource::as_str),
            t.ts,
            t.text,
            t.text.len() as i64,
            t.truncated as i64,
            t.source_byte_start.map(|v| v as i64),
            t.source_byte_len.map(|v| v as i64),
        ])?;
    }
    Ok(())
}

/// Refresh derived session fields (ts range, turn count, title fallback,
/// first human prompt) from the turns now in the table.
fn finalize_session<'a>(
    conn: &Connection,
    session_id: i64,
    session: &UnifiedSession,
    _new_turns: impl Iterator<Item = &'a UnifiedTurn>,
) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET
           turn_count = (SELECT COUNT(*) FROM turns WHERE session_id = ?1),
           ts_first = (SELECT MIN(ts) FROM turns WHERE session_id = ?1),
           ts_last = (SELECT MAX(ts) FROM turns WHERE session_id = ?1),
           first_user_prompt = (SELECT text FROM turns
                                WHERE session_id = ?1 AND role = 'user'
                                  AND intent_source = 'human'
                                ORDER BY seq LIMIT 1)
         WHERE id = ?1",
        params![session_id],
    )?;
    if session.title.is_none() {
        conn.execute(
            "UPDATE sessions SET title = (
                 SELECT substr(replace(trim(text), char(10), ' '), 1, 120) FROM turns
                 WHERE session_id = ?1 AND role = 'user' AND intent_source = 'human'
                 ORDER BY seq LIMIT 1
             ) WHERE id = ?1 AND title IS NULL",
            params![session_id],
        )?;
    }
    Ok(())
}

fn upsert_state(conn: &Connection, agent: &str, unit_key: &str, st: &IngestState) -> Result<()> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    conn.execute(
        "INSERT INTO ingest_state (agent, unit_key, file_size, mtime_ms, bytes_ingested,
                                   cursor, last_ingest_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(agent, unit_key) DO UPDATE SET
           file_size = excluded.file_size,
           mtime_ms = excluded.mtime_ms,
           bytes_ingested = excluded.bytes_ingested,
           cursor = excluded.cursor,
           last_ingest_at = excluded.last_ingest_at",
        params![
            agent,
            unit_key,
            st.file_size,
            st.mtime_ms,
            st.bytes_ingested,
            st.cursor,
            now_ms
        ],
    )?;
    Ok(())
}

fn row_to_session(r: &rusqlite::Row<'_>) -> Result<SessionRow> {
    Ok(SessionRow {
        id: r.get(0)?,
        agent: r.get(1)?,
        source_session_id: r.get(2)?,
        source_path: r.get(3)?,
        cwd: r.get(4)?,
        git_branch: r.get(5)?,
        title: r.get(6)?,
        first_user_prompt: r.get(7)?,
        ts_first: r.get(8)?,
        ts_last: r.get(9)?,
        ts_source: TsSource::parse(&r.get::<_, String>(10)?),
        turn_count: r.get(11)?,
        is_subagent: r.get::<_, i64>(12)? != 0,
        parent_session_id: r.get(13)?,
    })
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS ingest_state (
  id INTEGER PRIMARY KEY,
  agent TEXT NOT NULL,
  unit_key TEXT NOT NULL,
  file_size INTEGER,
  mtime_ms INTEGER,
  bytes_ingested INTEGER NOT NULL DEFAULT 0,
  cursor TEXT,
  last_ingest_at INTEGER,
  UNIQUE(agent, unit_key)
);
CREATE TABLE IF NOT EXISTS sessions (
  id INTEGER PRIMARY KEY,
  agent TEXT NOT NULL,
  source_session_id TEXT NOT NULL,
  source_path TEXT NOT NULL,
  cwd TEXT,
  git_branch TEXT,
  title TEXT,
  first_user_prompt TEXT,
  ts_first INTEGER,
  ts_last INTEGER,
  ts_source TEXT NOT NULL DEFAULT 'absent',
  turn_count INTEGER NOT NULL DEFAULT 0,
  is_subagent INTEGER NOT NULL DEFAULT 0,
  parent_source_session_id TEXT,
  parent_session_id INTEGER,
  UNIQUE(agent, source_session_id)
);
CREATE INDEX IF NOT EXISTS idx_sessions_cwd ON sessions(cwd);
CREATE INDEX IF NOT EXISTS idx_sessions_agent_ts ON sessions(agent, ts_last);
CREATE TABLE IF NOT EXISTS turns (
  id INTEGER PRIMARY KEY,
  session_id INTEGER NOT NULL REFERENCES sessions(id),
  seq INTEGER NOT NULL,
  role TEXT NOT NULL,
  intent_source TEXT,
  ts INTEGER,
  text TEXT NOT NULL,
  text_len INTEGER NOT NULL DEFAULT 0,
  truncated INTEGER NOT NULL DEFAULT 0,
  source_byte_start INTEGER,
  source_byte_len INTEGER,
  embedded INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(session_id, seq);
CREATE INDEX IF NOT EXISTS idx_turns_ts ON turns(ts);
CREATE INDEX IF NOT EXISTS idx_turns_pending ON turns(id) WHERE embedded = 0;
CREATE TABLE IF NOT EXISTS vector_chunks (
  chunk_id INTEGER PRIMARY KEY,
  turn_id INTEGER NOT NULL,
  chunk_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_vector_chunks_turn ON vector_chunks(turn_id);
CREATE TABLE IF NOT EXISTS meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
";

/// Additive migrations, same introspection pattern as pixel-graph.
fn migrate(conn: &Connection) -> Result<()> {
    let has_column = |table: &str, col: &str| -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for row in rows {
            if row? == col {
                return Ok(true);
            }
        }
        Ok(false)
    };
    if !has_column("vector_chunks", "chunk_start")? {
        conn.execute(
            "ALTER TABLE vector_chunks ADD COLUMN chunk_start INTEGER",
            [],
        )?;
    }
    // The pending-embed queue is drained in id order; an index keyed on id
    // (not on the constant `embedded` value) makes each batch O(batch).
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_turns_pending ON turns(id) WHERE embedded = 0",
        [],
    )?;
    conn.execute("DROP INDEX IF EXISTS idx_turns_unembedded", [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IntentSource, Role};
    use crate::testutil::{add_session, add_session_with_intents};

    #[test]
    fn total_turns_and_turns_for_indexing_should_read_the_stored_turns() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = RecallStore::open(&tmp.path().join("recall.db")).unwrap();
        assert_eq!(store.total_turns().unwrap(), 0);
        add_session(
            &mut store,
            "claude",
            "aaaa1111",
            &[
                (Role::User, "one"),
                (Role::Assistant, "two"),
                (Role::User, "three"),
            ],
        );
        assert_eq!(store.total_turns().unwrap(), 3);
        assert_eq!(
            store.turns_for_indexing(1, 1).unwrap(),
            vec![(2, "two".to_string())]
        );
        assert_eq!(
            store.turns_for_indexing(0, 10).unwrap(),
            vec![
                (1, "one".to_string()),
                (2, "two".to_string()),
                (3, "three".to_string())
            ]
        );
        assert!(store.turns_for_indexing(3, 10).unwrap().is_empty());
    }

    #[test]
    fn mark_policy_skips_should_count_tool_and_injected_turns() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = RecallStore::open(&tmp.path().join("recall.db")).unwrap();
        add_session_with_intents(
            &mut store,
            "claude",
            "aaaa1111",
            &[
                (Role::User, Some(IntentSource::Human), "keep"),
                (Role::User, Some(IntentSource::Orchestrator), "injected"),
                (Role::Tool, None, "tool output"),
                (Role::Assistant, None, "answer"),
            ],
        );
        assert_eq!(store.mark_policy_skips().unwrap(), 2);
        assert_eq!(store.embed_backlog().unwrap(), 2);
        let embedded: Vec<(String, i64)> = store
            .connection()
            .prepare("SELECT text, embedded FROM turns ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            embedded,
            vec![
                ("keep".to_string(), 0),
                ("injected".to_string(), 2),
                ("tool output".to_string(), 2),
                ("answer".to_string(), 0),
            ]
        );
        assert_eq!(store.mark_policy_skips().unwrap(), 0);
    }

    #[test]
    fn drop_orphan_chunks_should_delete_chunks_past_the_persisted_id() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = RecallStore::open(&tmp.path().join("recall.db")).unwrap();
        add_session(&mut store, "claude", "aaaa1111", &[(Role::User, "one")]);
        let ids = store.insert_chunks(1, &[(0, 1), (1, 2), (2, 3)]).unwrap();
        assert_eq!(store.drop_orphan_chunks(ids[2]).unwrap(), 0);
        assert_eq!(store.drop_orphan_chunks(ids[0]).unwrap(), 2);
        assert_eq!(store.chunk_turns(&ids).unwrap(), vec![(ids[0], 1, 0)]);
    }

    /// The ingest state is what makes the next pass incremental: a write
    /// that does not land re-ingests every unit from scratch.
    #[test]
    fn touch_state_persists_and_overwrites_the_ingest_state() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RecallStore::open(&tmp.path().join("recall.db")).unwrap();
        assert!(store.ingest_state("claude", "/a.jsonl").unwrap().is_none());
        let first = IngestState {
            file_size: 10,
            mtime_ms: 20,
            bytes_ingested: 5,
            cursor: Some("c1".to_string()),
        };
        store.touch_state("claude", "/a.jsonl", &first).unwrap();
        let read = store.ingest_state("claude", "/a.jsonl").unwrap().unwrap();
        assert_eq!(
            (
                read.file_size,
                read.mtime_ms,
                read.bytes_ingested,
                read.cursor.as_deref()
            ),
            (10, 20, 5, Some("c1"))
        );
        let second = IngestState {
            file_size: 30,
            mtime_ms: 40,
            bytes_ingested: 30,
            cursor: None,
        };
        store.touch_state("claude", "/a.jsonl", &second).unwrap();
        let read = store.ingest_state("claude", "/a.jsonl").unwrap().unwrap();
        assert_eq!(
            (read.file_size, read.bytes_ingested, read.cursor),
            (30, 30, None)
        );
        assert!(store.ingest_state("codex", "/a.jsonl").unwrap().is_none());
    }

    /// The watermark a lazy catch-up resumes from: `None` before any
    /// ingest, a real timestamp after, and per-agent isolation.
    #[test]
    fn agent_last_ingest_at_is_none_until_the_agent_ingests() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RecallStore::open(&tmp.path().join("recall.db")).unwrap();
        assert_eq!(store.agent_last_ingest_at("claude").unwrap(), None);
        let st = IngestState {
            file_size: 1,
            mtime_ms: 1,
            bytes_ingested: 1,
            cursor: None,
        };
        store.touch_state("claude", "/a.jsonl", &st).unwrap();
        let at = store.agent_last_ingest_at("claude").unwrap().unwrap();
        assert!(at > 0, "touch_state stamps the wall clock");
        assert_eq!(store.agent_last_ingest_at("codex").unwrap(), None);
    }
}
