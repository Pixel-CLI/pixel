//! SQLite persistence for the sniper error sink — THE schema contract.
//!
//! One database per project under
//! `$XDG_STATE_HOME/gitpixel/sniper/<basename>-<sha256(root)[0:12]>/errors-v1.sqlite`
//! (`PIXEL_SNIPER_STATE_ROOT` overrides the state root for tests).
//! WAL mode, busy_timeout 2000 ms; retention runs on every open.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::dedup::dedup_hash;
use crate::types::{
    ErrorInput, ErrorRecord, EventInput, EventKind, EventRecord, RunInput, RunRecord, Surface,
};

const SCHEMA_VERSION: i64 = 1;
const ERROR_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const ERROR_MAX_ROWS: i64 = 5000;
const EVENT_RETENTION_MS: i64 = 3 * 24 * 60 * 60 * 1000;
const EVENT_MAX_ROWS: i64 = 20000;

#[derive(Debug)]
pub enum StoreError {
    Sql(rusqlite::Error),
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sql(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Json(e)
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sql(e) => write!(f, "sqlite: {e}"),
            StoreError::Io(e) => write!(f, "io: {e}"),
            StoreError::Json(e) => write!(f, "json: {e}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------------

/// State root: `PIXEL_SNIPER_STATE_ROOT` > `XDG_STATE_HOME` > `~/.local/state`.
pub fn resolve_state_root() -> PathBuf {
    if let Ok(root) = std::env::var("PIXEL_SNIPER_STATE_ROOT")
        && !root.is_empty()
    {
        return PathBuf::from(root);
    }
    if let Ok(root) = std::env::var("XDG_STATE_HOME")
        && !root.is_empty()
    {
        return PathBuf::from(root);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".local").join("state")
}

/// `<basename>-<sha256(root)[0:12]>` — stable per absolute project root.
pub fn project_key(project_root: &Path) -> String {
    let display = project_root.to_string_lossy();
    let mut hasher = Sha256::new();
    hasher.update(display.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(12);
    for byte in digest.iter().take(6) {
        hex.push_str(&format!("{byte:02x}"));
    }
    let base = project_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into());
    format!("{base}-{hex}")
}

pub fn store_directory(project_root: &Path, state_root: &Path) -> PathBuf {
    state_root
        .join("pixel")
        .join("sniper")
        .join(project_key(project_root))
}

pub fn store_path(project_root: &Path, state_root: &Path) -> PathBuf {
    store_directory(project_root, state_root).join("errors-v1.sqlite")
}

/// Resolve the project root: `git rev-parse --show-toplevel` from `start`,
/// falling back to the canonicalized start path outside a git repo.
pub fn resolve_project_root(start: &Path) -> Result<PathBuf> {
    let canonical = start.canonicalize()?;
    let out = Command::new("git")
        .arg("-C")
        .arg(&canonical)
        .args(["rev-parse", "--show-toplevel"])
        .output();
    if let Ok(out) = out
        && out.status.success()
    {
        let top = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !top.is_empty() {
            return Ok(PathBuf::from(top));
        }
    }
    Ok(canonical)
}

// ---------------------------------------------------------------------------
// store
// ---------------------------------------------------------------------------

const DDL: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS runs (
  run_id TEXT PRIMARY KEY,
  started_at INTEGER NOT NULL,
  pid INTEGER,
  port INTEGER,
  git_head TEXT,
  lockfile_hash TEXT,
  vite_dep_hash TEXT,
  fingerprint_json TEXT,
  changed_json TEXT
);
CREATE TABLE IF NOT EXISTS errors (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  first_ts INTEGER NOT NULL,
  last_ts INTEGER NOT NULL,
  count INTEGER NOT NULL DEFAULT 1,
  run_id TEXT,
  surface TEXT NOT NULL,
  kind TEXT,
  message TEXT NOT NULL,
  stack_raw TEXT,
  frames_json TEXT,
  values_json TEXT,
  http_json TEXT,
  extra_json TEXT,
  dedup_hash TEXT NOT NULL UNIQUE
);
CREATE INDEX IF NOT EXISTS errors_last_ts ON errors(last_ts);
CREATE INDEX IF NOT EXISTS errors_surface ON errors(surface, last_ts);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts INTEGER NOT NULL,
  run_id TEXT,
  kind TEXT NOT NULL,
  data_json TEXT
);
CREATE INDEX IF NOT EXISTS events_ts ON events(ts);
CREATE INDEX IF NOT EXISTS events_kind ON events(kind, ts);
CREATE TABLE IF NOT EXISTS raw_fallbacks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts INTEGER NOT NULL,
  source TEXT NOT NULL,
  raw TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS ingest_state (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

pub struct Store {
    conn: Connection,
    path: PathBuf,
    project_root: PathBuf,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct RecordedError {
    pub id: i64,
    pub count: i64,
    pub deduped: bool,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct GcOutcome {
    pub errors_deleted: i64,
    pub events_deleted: i64,
    pub vacuumed: bool,
}

impl Store {
    /// Open (creating if needed) the store for `project_root`, using the
    /// env-resolved state root.
    pub fn open(project_root: &Path) -> Result<Store> {
        Self::open_at(project_root, &resolve_state_root())
    }

    /// Open with an explicit state root (tests).
    pub fn open_at(project_root: &Path, state_root: &Path) -> Result<Store> {
        let dir = store_directory(project_root, state_root);
        fs::create_dir_all(&dir)?;
        set_mode(&dir, 0o700)?;
        let file = dir.join("errors-v1.sqlite");

        let project_json = dir.join("project.json");
        if !project_json.exists() {
            let body = serde_json::to_string_pretty(&serde_json::json!({
                "root": project_root.to_string_lossy(),
                "key": project_key(project_root),
                "created_at_ms": now_ms(),
            }))?;
            fs::write(&project_json, format!("{body}\n"))?;
            set_mode(&project_json, 0o600)?;
        }

        let conn = match Self::open_conn(&file) {
            Ok(conn) => conn,
            Err(_) => {
                // Self-heal: corrupt or mismatched database → delete + rebuild.
                for suffix in ["", "-wal", "-shm"] {
                    let mut victim = file.clone().into_os_string();
                    victim.push(suffix);
                    let _ = fs::remove_file(PathBuf::from(victim));
                }
                Self::open_conn(&file)?
            }
        };
        set_mode(&file, 0o600)?;
        let store = Store {
            conn,
            path: file,
            project_root: project_root.to_path_buf(),
        };
        store.retain(now_ms())?;
        Ok(store)
    }

    fn open_conn(file: &Path) -> Result<Connection> {
        let conn = Connection::open(file)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 2000)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != 0 && version != SCHEMA_VERSION {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        conn.execute_batch(DDL)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(conn)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    // -- writes -------------------------------------------------------------

    /// Insert one error occurrence; identical identities upsert
    /// `count = count + 1, last_ts = now`.
    pub fn record_error(&self, input: &ErrorInput) -> Result<RecordedError> {
        let ts = input.ts.unwrap_or_else(now_ms);
        let hash = dedup_hash(
            input.surface,
            input.kind.as_deref(),
            &input.message,
            input.frames.as_deref(),
        );
        let frames_json = match &input.frames {
            Some(frames) => Some(serde_json::to_string(frames)?),
            None => None,
        };
        let values_json = match &input.values {
            Some(v) => Some(serde_json::to_string(v)?),
            None => None,
        };
        let http_json = match &input.http {
            Some(v) => Some(serde_json::to_string(v)?),
            None => None,
        };
        let extra_json = match &input.extra {
            Some(v) => Some(serde_json::to_string(v)?),
            None => None,
        };
        let (id, count) = self.conn.query_row(
            "INSERT INTO errors (
               first_ts, last_ts, count, run_id, surface, kind, message,
               stack_raw, frames_json, values_json, http_json, extra_json, dedup_hash
             ) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(dedup_hash) DO UPDATE SET
               count = count + 1,
               last_ts = excluded.last_ts,
               run_id = excluded.run_id
             RETURNING id, count",
            params![
                ts,
                ts,
                input.run_id,
                input.surface.as_str(),
                input.kind,
                input.message,
                input.stack_raw,
                frames_json,
                values_json,
                http_json,
                extra_json,
                hash,
            ],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )?;
        Ok(RecordedError {
            id,
            count,
            deduped: count > 1,
        })
    }

    pub fn record_event(&self, input: &EventInput) -> Result<i64> {
        let data_json = match &input.data {
            Some(v) => Some(serde_json::to_string(v)?),
            None => None,
        };
        let id = self.conn.query_row(
            "INSERT INTO events (ts, run_id, kind, data_json)
             VALUES (?1, ?2, ?3, ?4) RETURNING id",
            params![
                input.ts.unwrap_or_else(now_ms),
                input.run_id,
                input.kind.as_str(),
                data_json,
            ],
            |r| r.get::<_, i64>(0),
        )?;
        Ok(id)
    }

    pub fn record_run(&self, input: &RunInput) -> Result<()> {
        let fingerprint_json = match &input.fingerprint {
            Some(v) => Some(serde_json::to_string(v)?),
            None => None,
        };
        let changed_json = match &input.changed_since_last_run {
            Some(v) => Some(serde_json::to_string(v)?),
            None => None,
        };
        self.conn.execute(
            "INSERT INTO runs (
               run_id, started_at, pid, port, git_head, lockfile_hash,
               vite_dep_hash, fingerprint_json, changed_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(run_id) DO UPDATE SET
               pid = excluded.pid,
               port = excluded.port,
               git_head = excluded.git_head,
               lockfile_hash = excluded.lockfile_hash,
               vite_dep_hash = excluded.vite_dep_hash,
               fingerprint_json = excluded.fingerprint_json,
               changed_json = excluded.changed_json",
            params![
                input.run_id,
                input.ts.unwrap_or_else(now_ms),
                input.pid,
                input.port,
                input.git_head,
                input.lockfile_hash,
                input.vite_dep_hash,
                fingerprint_json,
                changed_json,
            ],
        )?;
        Ok(())
    }

    /// Record a generic event with an arbitrary kind string (for the Journal
    /// op, which accepts any user-supplied kind rather than the fixed
    /// `EventKind` enum). The kind is stored verbatim in the `events.kind`
    /// column; optional `data` is JSON-serialized into `data_json`.
    pub fn record_event_raw(
        &self,
        kind: &str,
        data: Option<&serde_json::Value>,
        run_id: Option<&str>,
    ) -> Result<i64> {
        let data_json = match data {
            Some(v) => Some(serde_json::to_string(v)?),
            None => None,
        };
        let id = self.conn.query_row(
            "INSERT INTO events (ts, run_id, kind, data_json)
             VALUES (?1, ?2, ?3, ?4) RETURNING id",
            params![now_ms(), run_id, kind, data_json],
            |r| r.get::<_, i64>(0),
        )?;
        Ok(id)
    }

    pub fn record_raw_fallback(&self, source: &str, raw: &str, ts: Option<i64>) -> Result<i64> {
        let id = self.conn.query_row(
            "INSERT INTO raw_fallbacks (ts, source, raw) VALUES (?1, ?2, ?3) RETURNING id",
            params![ts.unwrap_or_else(now_ms), source, raw],
            |r| r.get::<_, i64>(0),
        )?;
        Ok(id)
    }

    // -- reads --------------------------------------------------------------

    pub fn last_errors(&self, n: i64, surface: Option<Surface>) -> Result<Vec<ErrorRecord>> {
        match surface {
            Some(surface) => self.collect_errors(
                "SELECT * FROM errors WHERE surface = ?1 ORDER BY id DESC LIMIT ?2",
                params![surface.as_str(), n],
            ),
            None => self.collect_errors(
                "SELECT * FROM errors ORDER BY id DESC LIMIT ?1",
                params![n],
            ),
        }
    }

    pub fn errors_since(&self, cursor: i64, limit: i64) -> Result<Vec<ErrorRecord>> {
        self.collect_errors(
            "SELECT * FROM errors WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
            params![cursor, limit],
        )
    }

    pub fn errors_since_ts(&self, ts: i64, limit: i64) -> Result<Vec<ErrorRecord>> {
        self.collect_errors(
            "SELECT * FROM errors WHERE last_ts >= ?1 ORDER BY id ASC LIMIT ?2",
            params![ts, limit],
        )
    }

    pub fn get_error(&self, id: i64) -> Result<Option<ErrorRecord>> {
        let mut rows = self.collect_errors("SELECT * FROM errors WHERE id = ?1", params![id])?;
        Ok(rows.pop())
    }

    /// Case-insensitive substring search over message, kind, stack, frames.
    pub fn search_errors(&self, text: &str, limit: i64) -> Result<Vec<ErrorRecord>> {
        let escaped = text
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        self.collect_errors(
            "SELECT * FROM errors
             WHERE message LIKE ?1 ESCAPE '\\'
                OR COALESCE(kind, '') LIKE ?1 ESCAPE '\\'
                OR COALESCE(stack_raw, '') LIKE ?1 ESCAPE '\\'
                OR COALESCE(frames_json, '') LIKE ?1 ESCAPE '\\'
             ORDER BY id DESC LIMIT ?2",
            params![pattern, limit],
        )
    }

    pub fn latest_error_by_surface(&self, surface: Surface) -> Result<Option<ErrorRecord>> {
        let mut rows = self.collect_errors(
            "SELECT * FROM errors WHERE surface = ?1 ORDER BY id DESC LIMIT 1",
            params![surface.as_str()],
        )?;
        Ok(rows.pop())
    }

    pub fn max_cursor(&self) -> Result<i64> {
        let max: Option<i64> = self
            .conn
            .query_row("SELECT MAX(id) FROM errors", [], |r| r.get(0))?;
        Ok(max.unwrap_or(0))
    }

    pub fn events_between(&self, from_ts: i64, to_ts: i64) -> Result<Vec<EventRecord>> {
        self.collect_events(
            "SELECT * FROM events WHERE ts >= ?1 AND ts <= ?2 ORDER BY ts ASC",
            params![from_ts, to_ts],
        )
    }

    pub fn latest_events(&self, kinds: &[EventKind], limit: i64) -> Result<Vec<EventRecord>> {
        if kinds.is_empty() {
            return self.collect_events(
                "SELECT * FROM events ORDER BY id DESC LIMIT ?1",
                params![limit],
            );
        }
        let placeholders: Vec<String> =
            (1..=kinds.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT * FROM events WHERE kind IN ({}) ORDER BY id DESC LIMIT ?{}",
            placeholders.join(", "),
            kinds.len() + 1
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut owned: Vec<Box<dyn rusqlite::types::ToSql>> = kinds
            .iter()
            .map(|k| Box::new(k.as_str().to_owned()) as Box<dyn rusqlite::types::ToSql>)
            .collect();
        owned.push(Box::new(limit));
        let refs: Vec<&dyn rusqlite::types::ToSql> = owned.iter().map(AsRef::as_ref).collect();
        let rows = stmt.query_map(refs.as_slice(), row_to_event)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn latest_event_by_kind(&self, kind: EventKind) -> Result<Option<EventRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM events WHERE kind = ?1 ORDER BY id DESC LIMIT 1")?;
        let event = stmt
            .query_row(params![kind.as_str()], row_to_event)
            .optional()?;
        Ok(event)
    }

    pub fn latest_runs(&self, n: i64) -> Result<Vec<RunRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM runs ORDER BY started_at DESC, run_id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![n], row_to_run)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // -- maintenance --------------------------------------------------------

    /// Retention pass: errors 7d / 5000 rows, events 3d / 20000 rows.
    pub fn retain(&self, now: i64) -> Result<(i64, i64)> {
        let mut errors_deleted = self.conn.execute(
            "DELETE FROM errors WHERE last_ts < ?1",
            params![now - ERROR_RETENTION_MS],
        )? as i64;
        errors_deleted += self.conn.execute(
            "DELETE FROM errors WHERE id NOT IN (SELECT id FROM errors ORDER BY id DESC LIMIT ?1)",
            params![ERROR_MAX_ROWS],
        )? as i64;
        let mut events_deleted = self.conn.execute(
            "DELETE FROM events WHERE ts < ?1",
            params![now - EVENT_RETENTION_MS],
        )? as i64;
        events_deleted += self.conn.execute(
            "DELETE FROM events WHERE id NOT IN (SELECT id FROM events ORDER BY id DESC LIMIT ?1)",
            params![EVENT_MAX_ROWS],
        )? as i64;
        Ok((errors_deleted, events_deleted))
    }

    pub fn gc(&self, vacuum: bool) -> Result<GcOutcome> {
        let (errors_deleted, events_deleted) = self.retain(now_ms())?;
        if vacuum {
            self.conn.execute_batch("VACUUM;")?;
        }
        Ok(GcOutcome {
            errors_deleted,
            events_deleted,
            vacuumed: vacuum,
        })
    }

    // -- row mapping ---------------------------------------------------------

    fn collect_errors(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<ErrorRecord>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, row_to_error)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn collect_events(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<EventRecord>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, row_to_event)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

fn parse_json_col(raw: Option<String>) -> Option<serde_json::Value> {
    raw.and_then(|s| serde_json::from_str(&s).ok())
}

fn row_to_error(r: &rusqlite::Row<'_>) -> rusqlite::Result<ErrorRecord> {
    let surface_raw: String = r.get("surface")?;
    let frames_raw: Option<String> = r.get("frames_json")?;
    Ok(ErrorRecord {
        id: r.get("id")?,
        first_ts: r.get("first_ts")?,
        last_ts: r.get("last_ts")?,
        count: r.get("count")?,
        run_id: r.get("run_id")?,
        surface: Surface::parse(&surface_raw).unwrap_or(Surface::Reported),
        kind: r.get("kind")?,
        message: r.get("message")?,
        stack_raw: r.get("stack_raw")?,
        frames: frames_raw.and_then(|s| serde_json::from_str(&s).ok()),
        values: parse_json_col(r.get("values_json")?),
        http: parse_json_col(r.get("http_json")?),
        extra: parse_json_col(r.get("extra_json")?),
        dedup_hash: r.get("dedup_hash")?,
    })
}

fn row_to_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    let kind_raw: String = r.get("kind")?;
    Ok(EventRecord {
        id: r.get("id")?,
        ts: r.get("ts")?,
        run_id: r.get("run_id")?,
        kind: EventKind::parse(&kind_raw).unwrap_or(EventKind::ServerStart),
        data: parse_json_col(r.get("data_json")?),
    })
}

fn row_to_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<RunRecord> {
    let changed_raw: Option<String> = r.get("changed_json")?;
    Ok(RunRecord {
        run_id: r.get("run_id")?,
        started_at: r.get("started_at")?,
        pid: r.get("pid")?,
        port: r.get("port")?,
        git_head: r.get("git_head")?,
        lockfile_hash: r.get("lockfile_hash")?,
        vite_dep_hash: r.get("vite_dep_hash")?,
        fingerprint: parse_json_col(r.get("fingerprint_json")?),
        changed_since_last_run: changed_raw.and_then(|s| serde_json::from_str(&s).ok()),
    })
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}
