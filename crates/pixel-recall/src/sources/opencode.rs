// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! opencode adapter — SQLite store at `~/.local/share/opencode/opencode.db`.
//!
//! Also hosts the shared "opencode-like" schema logic (session / message /
//! part tables, JSON `data` columns) reused by the zcode adapter, whose
//! database at `~/.zcode/cli/db/db.sqlite` has the same shape.
//!
//! Safety: the database belongs to a running CLI and one table (`event`)
//! holds tens of GB of audit rows — every query here is read-only and hits
//! only `session` / `message` / `part` through their indexes:
//!   message(session_id, time_created, id)   part(message_id, id)
//! Verified with EXPLAIN QUERY PLAN: the GROUP BY cursor probe is a
//! COVERING INDEX scan; per-session and per-message fetches are index
//! SEARCHes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::intent::classify_user_text;
use crate::model::{Role, TOOL_INPUT_CAP, TsSource, UnifiedSession, UnifiedTurn, cap_text};
use crate::sources::{
    Change, IngestError, ParseOutput, ParsedSession, SessionOp, SourceAdapter, SourceUnit,
};
use crate::store::IngestState;

pub struct Adapter {
    db_path: PathBuf,
}

impl Adapter {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        let data_home = std::env::var("XDG_DATA_HOME")
            .map_or_else(|_| PathBuf::from(&home).join(".local/share"), PathBuf::from);
        Self {
            db_path: data_home.join("opencode/opencode.db"),
        }
    }

    pub fn with_db(db_path: PathBuf) -> Self {
        Self { db_path }
    }
}

impl Default for Adapter {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceAdapter for Adapter {
    fn agent(&self) -> &'static str {
        "opencode"
    }

    fn discover(&self) -> Result<Vec<SourceUnit>, IngestError> {
        Ok(db_unit(&self.db_path).into_iter().collect())
    }

    fn classify(&self, unit: &SourceUnit, state: Option<&IngestState>) -> Change {
        oc_classify(unit, state)
    }

    fn parse(
        &self,
        unit: &SourceUnit,
        change: Change,
        state: Option<&IngestState>,
    ) -> Result<ParseOutput, IngestError> {
        oc_parse("opencode", unit, change, state)
    }
}

// ---------------------------------------------------------------------------
// Shared helpers (used by both opencode and zcode adapters).
// ---------------------------------------------------------------------------

/// Stat a SQLite database file into a single `SourceUnit`. Missing file →
/// no unit (source not installed on this machine).
pub(crate) fn db_unit(path: &Path) -> Option<SourceUnit> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some(SourceUnit {
        unit_key: format!("db:{}", path.display()),
        path: path.to_path_buf(),
        size: meta.len(),
        mtime_ms,
    })
}

/// Read-only, no-mutex connection with a 5s busy timeout — the databases
/// belong to running CLIs and must never be written or blocked.
pub(crate) fn open_ro(path: &Path) -> Result<Connection, IngestError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    Ok(conn)
}

/// Max message.time_created across the whole store, via a covering-index
/// GROUP BY (never touches table rows, never touches other tables).
fn oc_max_cursor(conn: &Connection) -> Result<Option<i64>, IngestError> {
    let v: Option<i64> = conn.query_row(
        "SELECT MAX(m) FROM \
         (SELECT MAX(time_created) AS m FROM message GROUP BY session_id)",
        [],
        |r| r.get(0),
    )?;
    Ok(v)
}

/// SQLite classify: file size/mtime churn constantly, so compare the
/// monotonic cursor instead. New when never ingested; Unchanged when the
/// cheap max-cursor probe equals the recorded cursor; otherwise Appended
/// with the recorded cursor as the resume point (parse re-materializes
/// touched sessions wholesale).
pub(crate) fn oc_classify(unit: &SourceUnit, state: Option<&IngestState>) -> Change {
    let Some(st) = state else {
        return Change::New;
    };
    let recorded: Option<i64> = st.cursor.as_deref().and_then(|c| c.parse().ok());
    let Some(recorded) = recorded else {
        // No usable cursor from a prior pass — re-parse from scratch.
        return Change::Rewritten;
    };
    match open_ro(&unit.path).and_then(|c| oc_max_cursor(&c)) {
        Ok(Some(max)) if max == recorded => Change::Unchanged,
        Ok(_) => Change::Appended { from: 0 },
        // Probe failed (locked / transient) — let parse try and report.
        Err(_) => Change::Appended { from: 0 },
    }
}

pub(crate) fn oc_parse(
    agent: &'static str,
    unit: &SourceUnit,
    change: Change,
    state: Option<&IngestState>,
) -> Result<ParseOutput, IngestError> {
    let conn = open_ro(&unit.path)?;

    // Snapshot the cursor BEFORE selecting sessions: anything landing after
    // this point is re-picked next pass (Replace makes that idempotent).
    let new_cursor = oc_max_cursor(&conn)?;

    // Incremental only when the driver classified Appended; New/Rewritten
    // re-materialize everything.
    let since: i64 = match change {
        Change::Appended { .. } => state
            .and_then(|s| s.cursor.as_deref())
            .and_then(|c| c.parse().ok())
            .unwrap_or(i64::MIN),
        _ => i64::MIN,
    };

    // Touched sessions: covering-index GROUP BY over message.
    let touched: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT session_id FROM message \
             GROUP BY session_id HAVING MAX(time_created) > ?1",
        )?;
        let rows = stmt.query_map([since], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<_, _>>()?
    };

    let mut sess_stmt =
        conn.prepare("SELECT parent_id, directory, title FROM session WHERE id = ?1")?;
    let mut msg_stmt = conn.prepare(
        "SELECT id, time_created, data FROM message \
         WHERE session_id = ?1 ORDER BY time_created, id",
    )?;
    let mut part_stmt = conn.prepare("SELECT data FROM part WHERE message_id = ?1 ORDER BY id")?;

    let source_path = unit.path.to_string_lossy().to_string();
    let mut sessions: Vec<ParsedSession> = Vec::new();

    for sid in touched {
        // Session metadata (PK lookup); a message whose session row vanished
        // mid-scan is skipped — it gets re-picked next pass if it returns.
        let meta = sess_stmt.query_row([&sid], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        });
        let (parent_id, directory, title) = match meta {
            Ok(v) => v,
            Err(rusqlite::Error::QueryReturnedNoRows) => continue,
            Err(e) => return Err(e.into()),
        };

        let mut turns: Vec<UnifiedTurn> = Vec::new();
        let msg_rows = msg_stmt.query_map([&sid], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in msg_rows {
            let (msg_id, time_created, data) = row?;
            let role = serde_json::from_str::<Value>(&data)
                .ok()
                .and_then(|v| v.get("role").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default();

            // One turn per message: concatenate its text parts, fold tool
            // parts as "⋮tool <name> <input capped>", skip the rest
            // (reasoning, step-start/finish, patch, compaction…).
            let mut text = String::new();
            let mut truncated = false;
            let part_rows = part_stmt.query_map([&msg_id], |r| r.get::<_, String>(0))?;
            for pdata in part_rows {
                let pdata = pdata?;
                let Ok(pv) = serde_json::from_str::<Value>(&pdata) else {
                    continue;
                };
                match pv.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = pv.get("text").and_then(Value::as_str)
                            && !t.trim().is_empty()
                        {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                    }
                    Some("tool") => {
                        let name = pv.get("tool").and_then(Value::as_str).unwrap_or("unknown");
                        let input = pv
                            .pointer("/state/input")
                            .map(ToString::to_string)
                            .unwrap_or_default();
                        let (capped, cut) = cap_text(&input, TOOL_INPUT_CAP);
                        truncated |= cut;
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&format!("\u{22ee}tool {name} {capped}"));
                    }
                    _ => {}
                }
            }
            if text.trim().is_empty() {
                continue;
            }
            let (role, intent) = match role.as_str() {
                "user" => (Role::User, Some(classify_user_text(&text))),
                "assistant" => (Role::Assistant, None),
                _ => (Role::Tool, None),
            };
            turns.push(UnifiedTurn {
                role,
                intent_source: intent,
                ts: Some(time_created),
                text,
                truncated,
                source_byte_start: None,
                source_byte_len: None,
            });
        }
        if turns.is_empty() {
            continue;
        }

        let session = UnifiedSession {
            agent,
            source_session_id: sid,
            source_path: source_path.clone(),
            cwd: directory.filter(|s| !s.is_empty()),
            git_branch: None,
            title: title
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty()),
            ts_source: TsSource::UnixMs,
            is_subagent: parent_id.is_some(),
            parent_source_session_id: parent_id,
        };
        sessions.push(ParsedSession {
            op: SessionOp::Replace,
            session,
            turns,
        });
    }

    Ok(ParseOutput {
        sessions,
        skipped_records: 0,
        consumed_bytes: unit.size,
        cursor: new_cursor.map(|c| c.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, data TEXT);
             INSERT INTO session VALUES ('ses1', NULL, '/work/pixel', ' Fix build ');
             INSERT INTO message VALUES ('m1', 'ses1', 1000, '{\"role\":\"user\"}');
             INSERT INTO message VALUES ('m2', 'ses1', 2000, '{\"role\":\"assistant\"}');
             INSERT INTO part VALUES ('p1', 'm1', '{\"type\":\"text\",\"text\":\"fix the build\"}');
             INSERT INTO part VALUES ('p2', 'm2', '{\"type\":\"text\",\"text\":\"running\"}');
             INSERT INTO part VALUES ('p3', 'm2', '{\"type\":\"tool\",\"tool\":\"bash\",\"state\":{\"input\":{\"cmd\":\"make\"}}}');
             INSERT INTO part VALUES ('p4', 'm2', '{\"type\":\"reasoning\",\"text\":\"hidden\"}');",
        )
        .unwrap();
    }

    #[test]
    fn oc_parse_materializes_sessions_with_text_and_tool_parts() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        fixture(&db);
        let unit = SourceUnit {
            unit_key: "db:test".to_string(),
            path: db.clone(),
            size: std::fs::metadata(&db).unwrap().len(),
            mtime_ms: 1,
        };
        let out = oc_parse("opencode", &unit, Change::New, None).unwrap();
        assert_eq!(out.cursor.as_deref(), Some("2000"));
        assert_eq!(out.sessions.len(), 1);
        let s = &out.sessions[0];
        assert_eq!(s.session.source_session_id, "ses1");
        assert_eq!(s.session.title.as_deref(), Some("Fix build"));
        assert_eq!(s.session.cwd.as_deref(), Some("/work/pixel"));
        let roles: Vec<Role> = s.turns.iter().map(|t| t.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant]);
        assert_eq!(s.turns[0].text, "fix the build");
        assert_eq!(s.turns[0].ts, Some(1000));
        assert_eq!(
            s.turns[1].text,
            "running\n\u{22ee}tool bash {\"cmd\":\"make\"}"
        );
    }
}
