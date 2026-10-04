// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the Devin CLI adapter: which turns of a session store
//! become recall turns, in which order, and what an incremental pass
//! re-reads.

use super::*;
use rusqlite::params;

struct Db {
    _dir: tempfile::TempDir,
    path: PathBuf,
    conn: Connection,
}

fn db() -> Db {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT, title TEXT);
         CREATE TABLE message_nodes (
             row_id INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id TEXT, node_id INTEGER, parent_node_id INTEGER,
             chat_message TEXT, created_at INTEGER);",
    )
    .unwrap();
    Db {
        _dir: dir,
        path,
        conn,
    }
}

impl Db {
    fn session(&self, id: &str, cwd: Option<&str>, title: Option<&str>) {
        self.conn
            .execute(
                "INSERT INTO sessions VALUES (?1, ?2, ?3)",
                params![id, cwd, title],
            )
            .unwrap();
    }

    fn node(&self, sid: &str, node: i64, parent: Option<i64>, msg: &str, at: i64) {
        self.conn
            .execute(
                "INSERT INTO message_nodes (session_id, node_id, parent_node_id, chat_message, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![sid, node, parent, msg, at],
            )
            .unwrap();
    }

    fn unit(&self) -> SourceUnit {
        db_unit(&self.path).unwrap()
    }
}

fn msg(role: &str, content: &str) -> String {
    serde_json::json!({"role": role, "content": content}).to_string()
}

fn texts(s: &ParsedSession) -> Vec<(Role, String)> {
    s.turns.iter().map(|t| (t.role, t.text.clone())).collect()
}

fn full_parse(db: &Db) -> ParseOutput {
    Adapter::with_db(db.path.clone())
        .parse(&db.unit(), Change::New, None)
        .unwrap()
}

/// Turns come out parent before child, siblings by time, whatever order the
/// rows were written in; system and tool rows never reach recall.
#[test]
fn parse_should_linearize_the_tree_and_keep_only_user_and_assistant_turns() {
    let db = db();
    db.session("s1", Some("/repo"), Some("  Fix login  "));
    db.node(
        "s1",
        3,
        Some(1),
        &msg("assistant", "second reply"),
        1_700_000_030,
    );
    db.node("s1", 1, None, &msg("user", "fix the login"), 1_700_000_000);
    db.node(
        "s1",
        2,
        Some(1),
        &msg("assistant", "first reply"),
        1_700_000_010,
    );
    db.node(
        "s1",
        4,
        Some(2),
        &msg("tool", "raw tool output"),
        1_700_000_011,
    );
    db.node(
        "s1",
        5,
        Some(2),
        &msg("system", "rules boilerplate"),
        1_700_000_012,
    );
    db.node("s1", 6, Some(2), &msg("user", "thanks"), 1_700_000_013);

    let out = full_parse(&db);
    assert_eq!(out.sessions.len(), 1);
    let s = &out.sessions[0];
    assert_eq!(s.op, SessionOp::Replace);
    assert_eq!(
        texts(s),
        vec![
            (Role::User, "fix the login".to_string()),
            (Role::Assistant, "first reply".to_string()),
            (Role::User, "thanks".to_string()),
            (Role::Assistant, "second reply".to_string()),
        ]
    );
    assert_eq!(s.session.agent, "devin");
    assert_eq!(s.session.source_session_id, "s1");
    assert_eq!(s.session.cwd.as_deref(), Some("/repo"));
    assert_eq!(s.session.title.as_deref(), Some("Fix login"));
    assert_eq!(s.session.ts_source, TsSource::UnixMs);
    assert_eq!(out.cursor.as_deref(), Some("6"), "cursor is the max row id");
    assert_eq!(out.skipped_records, 0);
}

/// Devin writes seconds; a millisecond value (a future format) is kept as-is.
#[test]
fn parse_should_convert_second_timestamps_to_milliseconds() {
    let db = db();
    db.session("s1", None, None);
    db.node("s1", 1, None, &msg("user", "a"), 1_700_000_000);
    db.node("s1", 2, Some(1), &msg("assistant", "b"), 1_700_000_000_123);
    let out = full_parse(&db);
    let ts: Vec<Option<i64>> = out.sessions[0].turns.iter().map(|t| t.ts).collect();
    assert_eq!(ts, vec![Some(1_700_000_000_000), Some(1_700_000_000_123)]);
}

/// Only user turns carry an intent; assistant turns never do.
#[test]
fn parse_should_classify_intent_for_user_turns_only() {
    let db = db();
    db.session("s1", None, None);
    db.node("s1", 1, None, &msg("user", "why does login fail?"), 1);
    db.node("s1", 2, Some(1), &msg("assistant", "because"), 2);
    let out = full_parse(&db);
    let turns = &out.sessions[0].turns;
    assert_eq!(
        turns[0].intent_source,
        Some(classify_user_text("why does login fail?"))
    );
    assert_eq!(turns[1].intent_source, None);
}

/// An array content joins its text items; malformed rows, empty text and
/// unknown roles are dropped without dropping the session.
#[test]
fn parse_should_join_array_content_and_skip_unusable_rows() {
    let db = db();
    db.session("s1", Some(""), Some("   "));
    let array = serde_json::json!({
        "role": "user",
        "content": ["first", {"text": "second"}, {"image": "x"}, 7]
    })
    .to_string();
    db.node("s1", 1, None, &array, 1);
    db.node("s1", 2, Some(1), "{not json", 2);
    db.node("s1", 3, Some(1), &msg("assistant", "   "), 3);
    db.node(
        "s1",
        4,
        Some(1),
        &serde_json::json!({"role": "assistant"}).to_string(),
        4,
    );
    db.node(
        "s1",
        5,
        Some(1),
        &serde_json::json!({"content": "no role"}).to_string(),
        5,
    );
    db.node(
        "s1",
        6,
        Some(1),
        &serde_json::json!({"role": "assistant", "content": 42}).to_string(),
        6,
    );
    let out = full_parse(&db);
    let s = &out.sessions[0];
    assert_eq!(texts(s), vec![(Role::User, "first\nsecond".to_string())]);
    assert_eq!(s.session.cwd, None, "an empty cwd is no cwd");
    assert_eq!(s.session.title, None, "a blank title is no title");
}

/// A session with no user or assistant text is not a session to recall.
#[test]
fn parse_should_omit_sessions_without_turns() {
    let db = db();
    db.session("empty", None, None);
    db.session("tools", None, None);
    db.node("tools", 1, None, &msg("tool", "x"), 1);
    db.session("real", None, None);
    db.node("real", 1, None, &msg("user", "hello"), 1);
    let out = full_parse(&db);
    let ids: Vec<&str> = out
        .sessions
        .iter()
        .map(|s| s.session.source_session_id.as_str())
        .collect();
    assert_eq!(ids, vec!["real"]);
}

/// An orphan (parent never stored) is a root, and a parent cycle neither
/// loops nor loses its turns.
#[test]
fn parse_should_keep_orphans_and_cycles_in_time_order() {
    let db = db();
    db.session("s1", None, None);
    // The orphan is the earliest root: its subtree comes before the later
    // root's, as a root's would.
    db.node("s1", 1, Some(99), &msg("user", "orphan root"), 1);
    db.node("s1", 2, Some(1), &msg("assistant", "orphan child"), 100);
    db.node("s1", 3, None, &msg("user", "real root"), 5);
    db.node("s1", 4, Some(3), &msg("assistant", "real child"), 6);
    db.node("s1", 5, Some(6), &msg("user", "cycle a"), 200);
    db.node("s1", 6, Some(5), &msg("assistant", "cycle b"), 300);
    let out = full_parse(&db);
    assert_eq!(
        texts(&out.sessions[0]),
        vec![
            (Role::User, "orphan root".to_string()),
            (Role::Assistant, "orphan child".to_string()),
            (Role::User, "real root".to_string()),
            (Role::Assistant, "real child".to_string()),
            (Role::User, "cycle a".to_string()),
            (Role::Assistant, "cycle b".to_string()),
        ]
    );
}

/// An incremental pass re-materializes only the sessions written after the
/// cursor, each one whole, and moves the cursor to the newest row.
#[test]
fn parse_should_rematerialize_only_sessions_touched_after_the_cursor() {
    let db = db();
    db.session("old", None, None);
    db.node("old", 1, None, &msg("user", "untouched"), 1);
    db.session("live", None, None);
    db.node("live", 1, None, &msg("user", "before"), 2);
    let state = IngestState {
        file_size: 0,
        mtime_ms: 0,
        bytes_ingested: 0,
        cursor: Some("2".into()),
    };
    db.node("live", 2, Some(1), &msg("assistant", "after"), 3);
    db.node("live", 3, Some(2), &msg("user", "again"), 4);

    let out = Adapter::with_db(db.path.clone())
        .parse(&db.unit(), Change::Appended { from: 0 }, Some(&state))
        .unwrap();
    assert_eq!(out.sessions.len(), 1);
    let s = &out.sessions[0];
    assert_eq!(s.session.source_session_id, "live");
    assert_eq!(s.op, SessionOp::Replace);
    assert_eq!(
        texts(s),
        vec![
            (Role::User, "before".to_string()),
            (Role::Assistant, "after".to_string()),
            (Role::User, "again".to_string()),
        ]
    );
    assert_eq!(out.cursor.as_deref(), Some("4"));
}

/// Rows of a session whose `sessions` row is gone are skipped, not fatal.
#[test]
fn parse_should_skip_touched_sessions_missing_from_the_sessions_table() {
    let db = db();
    db.node("ghost", 1, None, &msg("user", "no metadata"), 1);
    let state = IngestState {
        file_size: 0,
        mtime_ms: 0,
        bytes_ingested: 0,
        cursor: Some("0".into()),
    };
    let out = Adapter::with_db(db.path.clone())
        .parse(&db.unit(), Change::Appended { from: 0 }, Some(&state))
        .unwrap();
    assert!(out.sessions.is_empty());
    assert_eq!(out.cursor.as_deref(), Some("1"));
}

fn state(cursor: Option<&str>) -> IngestState {
    IngestState {
        file_size: 0,
        mtime_ms: 0,
        bytes_ingested: 0,
        cursor: cursor.map(str::to_string),
    }
}

/// The cursor decides the change: none → new, unreadable → rewritten, the
/// current max row → unchanged, anything else → appended.
#[test]
fn classify_should_compare_the_recorded_cursor_with_the_max_row_id() {
    let db = db();
    db.session("s", None, None);
    db.node("s", 1, None, &msg("user", "a"), 1);
    db.node("s", 2, Some(1), &msg("user", "b"), 2);
    let adapter = Adapter::with_db(db.path.clone());
    let unit = db.unit();
    assert_eq!(adapter.classify(&unit, None), Change::New);
    assert_eq!(
        adapter.classify(&unit, Some(&state(None))),
        Change::Rewritten
    );
    assert_eq!(
        adapter.classify(&unit, Some(&state(Some("x")))),
        Change::Rewritten
    );
    assert_eq!(
        adapter.classify(&unit, Some(&state(Some("2")))),
        Change::Unchanged
    );
    assert_eq!(
        adapter.classify(&unit, Some(&state(Some("1")))),
        Change::Appended { from: 0 }
    );
}

/// A store that cannot be opened is re-read rather than reported unchanged.
#[test]
fn classify_should_report_appended_when_the_store_cannot_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.db");
    std::fs::write(&path, b"not a sqlite file at all, just bytes").unwrap();
    let adapter = Adapter::with_db(path.clone());
    let unit = db_unit(&path).unwrap();
    assert_eq!(
        adapter.classify(&unit, Some(&state(Some("5")))),
        Change::Appended { from: 0 }
    );
}

/// No store on disk means nothing to discover; a store is one unit keyed by
/// its path.
#[test]
fn discover_should_find_the_store_only_when_it_exists() {
    let dir = tempfile::tempdir().unwrap();
    let missing = Adapter::with_db(dir.path().join("none.db"));
    assert!(missing.discover().unwrap().is_empty());
    let db = db();
    let units = Adapter::with_db(db.path.clone()).discover().unwrap();
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].unit_key, format!("db:{}", db.path.display()));
    assert_eq!(Adapter::with_db(db.path.clone()).agent(), "devin");
}
