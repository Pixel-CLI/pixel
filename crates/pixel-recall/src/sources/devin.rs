// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Devin CLI adapter — SQLite store at `~/.local/share/devin/cli/sessions.db`.
//!
//! `message_nodes` is a per-session message FOREST (node_id/parent_node_id)
//! with ~300k rows; each row's `chat_message` is a JSON string
//! `{message_id, role, content, ...}` and `created_at` is unix SECONDS.
//! Nodes are linearized topologically (parent before child, siblings by
//! created_at then row_id). Only user/assistant turns are kept — "system"
//! rows are workspace/rules boilerplate and "tool" rows raw tool traffic;
//! both are skipped.
//!
//! Incremental cursor: `MAX(row_id)` (INTEGER PRIMARY KEY AUTOINCREMENT —
//! monotonic, never reused). Touched sessions come from a PK range scan
//! `WHERE row_id > cursor` (EXPLAIN QUERY PLAN: SEARCH ... USING INTEGER
//! PRIMARY KEY), then each is re-materialized wholesale via
//! `idx_message_nodes_session`. The first full ingest walks the sessions
//! table (~1k rows) with one indexed per-session fetch each.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::Value;

use crate::intent::classify_user_text;
use crate::model::{Role, TsSource, UnifiedSession, UnifiedTurn};
use crate::sources::opencode::{db_unit, open_ro};
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
            db_path: data_home.join("devin/cli/sessions.db"),
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

/// O(1) probe on the integer primary key.
fn max_row_id(conn: &Connection) -> Result<Option<i64>, IngestError> {
    let v: Option<i64> =
        conn.query_row("SELECT MAX(row_id) FROM message_nodes", [], |r| r.get(0))?;
    Ok(v)
}

/// Devin timestamps are unix seconds; tolerate a future switch to ms.
fn to_ms(v: i64) -> i64 {
    if v > 1_000_000_000_000 { v } else { v * 1000 }
}

struct Node {
    node_id: i64,
    parent: Option<i64>,
    created_at: i64,
    row_id: i64,
    chat_message: String,
}

/// Parent-before-child linearization; siblings by (created_at, row_id).
/// Orphans (parent id never seen — shouldn't happen) are treated as roots;
/// a visited guard makes cycles harmless.
fn linearize(mut nodes: Vec<Node>) -> Vec<Node> {
    nodes.sort_by_key(|n| (n.created_at, n.row_id));
    let known: HashSet<i64> = nodes.iter().map(|n| n.node_id).collect();
    let mut children: HashMap<Option<i64>, Vec<Node>> = HashMap::new();
    for n in nodes {
        let key = match n.parent {
            Some(p) if known.contains(&p) => Some(p),
            _ => None, // root or orphan
        };
        children.entry(key).or_default().push(n);
    }
    let mut out: Vec<Node> = Vec::new();
    let mut stack: Vec<Node> = children.remove(&None).unwrap_or_default();
    stack.reverse(); // pop() yields earliest sibling first
    let mut visited: HashSet<i64> = HashSet::new();
    while let Some(n) = stack.pop() {
        if !visited.insert(n.node_id) {
            continue;
        }
        if let Some(mut kids) = children.remove(&Some(n.node_id)) {
            kids.reverse();
            stack.extend(kids);
        }
        out.push(n);
    }
    // Anything unreachable through the visited/cycle guard still gets kept,
    // appended in timestamp order.
    let mut rest: Vec<Node> = children.into_values().flatten().collect();
    rest.sort_by_key(|n| (n.created_at, n.row_id));
    out.extend(rest);
    out
}

/// content is a plain string in practice; tolerate an array of text items.
fn content_text(v: &Value) -> String {
    match v.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|it| match it {
                Value::String(s) => Some(s.clone()),
                Value::Object(_) => it.get("text").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

impl SourceAdapter for Adapter {
    fn agent(&self) -> &'static str {
        "devin"
    }

    fn discover(&self) -> Result<Vec<SourceUnit>, IngestError> {
        Ok(db_unit(&self.db_path).into_iter().collect())
    }

    fn classify(&self, unit: &SourceUnit, state: Option<&IngestState>) -> Change {
        let Some(st) = state else {
            return Change::New;
        };
        let recorded: Option<i64> = st.cursor.as_deref().and_then(|c| c.parse().ok());
        let Some(recorded) = recorded else {
            return Change::Rewritten;
        };
        match open_ro(&unit.path).and_then(|c| max_row_id(&c)) {
            Ok(Some(max)) if max == recorded => Change::Unchanged,
            Ok(_) => Change::Appended { from: 0 },
            Err(_) => Change::Appended { from: 0 },
        }
    }

    fn parse(
        &self,
        unit: &SourceUnit,
        change: Change,
        state: Option<&IngestState>,
    ) -> Result<ParseOutput, IngestError> {
        let conn = open_ro(&unit.path)?;

        // Snapshot before selecting: rows landing after this are re-picked
        // next pass (Replace is idempotent).
        let new_cursor = max_row_id(&conn)?;

        let since: Option<i64> = match change {
            Change::Appended { .. } => state
                .and_then(|s| s.cursor.as_deref())
                .and_then(|c| c.parse().ok()),
            _ => None,
        };

        // Touched sessions. Incremental: PK range scan bounded by the
        // cursor. Full: walk the small sessions table.
        let touched: Vec<String> = match since {
            Some(cursor) => {
                let mut stmt =
                    conn.prepare("SELECT session_id FROM message_nodes WHERE row_id > ?1")?;
                let rows = stmt.query_map([cursor], |r| r.get::<_, String>(0))?;
                let mut seen = HashSet::new();
                let mut out = Vec::new();
                for r in rows {
                    let sid = r?;
                    if seen.insert(sid.clone()) {
                        out.push(sid);
                    }
                }
                out
            }
            None => {
                let mut stmt = conn.prepare("SELECT id FROM sessions")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                rows.collect::<Result<_, _>>()?
            }
        };

        let mut sess_stmt =
            conn.prepare("SELECT working_directory, title FROM sessions WHERE id = ?1")?;
        let mut node_stmt = conn.prepare(
            "SELECT node_id, parent_node_id, chat_message, created_at, row_id \
             FROM message_nodes WHERE session_id = ?1",
        )?;

        let source_path = unit.path.to_string_lossy().to_string();
        let mut sessions: Vec<ParsedSession> = Vec::new();

        for sid in touched {
            let meta = sess_stmt.query_row([&sid], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                ))
            });
            let (cwd, title) = match meta {
                Ok(v) => v,
                Err(rusqlite::Error::QueryReturnedNoRows) => continue,
                Err(e) => return Err(e.into()),
            };

            let nodes: Vec<Node> = node_stmt
                .query_map([&sid], |r| {
                    Ok(Node {
                        node_id: r.get(0)?,
                        parent: r.get(1)?,
                        chat_message: r.get(2)?,
                        created_at: r.get(3)?,
                        row_id: r.get(4)?,
                    })
                })?
                .collect::<Result<_, _>>()?;

            let mut turns: Vec<UnifiedTurn> = Vec::new();
            for node in linearize(nodes) {
                let Ok(msg) = serde_json::from_str::<Value>(&node.chat_message) else {
                    continue;
                };
                let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
                // "system" is workspace/rules boilerplate and "tool" raw
                // tool traffic — both skipped (see module docs).
                let role = match role {
                    "user" => Role::User,
                    "assistant" => Role::Assistant,
                    _ => continue,
                };
                let text = content_text(&msg);
                if text.trim().is_empty() {
                    continue;
                }
                let intent = match role {
                    Role::User => Some(classify_user_text(&text)),
                    _ => None,
                };
                turns.push(UnifiedTurn {
                    role,
                    intent_source: intent,
                    ts: Some(to_ms(node.created_at)),
                    text,
                    truncated: false,
                    source_byte_start: None,
                    source_byte_len: None,
                });
            }
            if turns.is_empty() {
                continue;
            }

            let session = UnifiedSession {
                agent: "devin",
                source_session_id: sid,
                source_path: source_path.clone(),
                cwd: cwd.filter(|s| !s.is_empty()),
                git_branch: None,
                title: title
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty()),
                ts_source: TsSource::UnixMs,
                is_subagent: false,
                parent_source_session_id: None,
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
}
