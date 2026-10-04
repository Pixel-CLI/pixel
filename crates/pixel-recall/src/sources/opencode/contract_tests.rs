// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the opencode-shaped SQLite adapter (also used by
//! zcode): change detection by cursor, which parts become turn text, and
//! what an incremental pass re-reads.

use super::*;
use rusqlite::params;

struct Db {
    _dir: tempfile::TempDir,
    path: PathBuf,
    conn: Connection,
}

fn db() -> Db {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("opencode.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT);
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
         CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, data TEXT);",
    )
    .unwrap();
    Db {
        _dir: dir,
        path,
        conn,
    }
}

impl Db {
    fn session(&self, id: &str, parent: Option<&str>, dir: Option<&str>, title: Option<&str>) {
        self.conn
            .execute(
                "INSERT INTO session VALUES (?1, ?2, ?3, ?4)",
                params![id, parent, dir, title],
            )
            .unwrap();
    }

    fn message(&self, id: &str, sid: &str, at: i64, role: &str, parts: &[&str]) {
        let data = serde_json::json!({ "role": role }).to_string();
        self.conn
            .execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, ?4)",
                params![id, sid, at, data],
            )
            .unwrap();
        for (i, p) in parts.iter().enumerate() {
            self.conn
                .execute(
                    "INSERT INTO part VALUES (?1, ?2, ?3)",
                    params![format!("{id}-p{i}"), id, p],
                )
                .unwrap();
        }
    }

    fn unit(&self) -> SourceUnit {
        db_unit(&self.path).unwrap()
    }
}

fn text(t: &str) -> String {
    serde_json::json!({"type": "text", "text": t}).to_string()
}

fn state(cursor: Option<&str>) -> IngestState {
    IngestState {
        file_size: 0,
        mtime_ms: 0,
        bytes_ingested: 0,
        cursor: cursor.map(str::to_string),
    }
}

/// The recorded cursor decides the change: never ingested → new, cursor
/// unreadable → rewritten, cursor at the newest message → unchanged,
/// anything else → appended.
#[test]
fn oc_classify_should_compare_the_cursor_with_the_newest_message_time() {
    let db = db();
    db.session("s", None, None, None);
    db.message("m1", "s", 1_000, "user", &[&text("a")]);
    db.message("m2", "s", 2_000, "user", &[&text("b")]);
    let unit = db.unit();
    assert_eq!(oc_classify(&unit, None), Change::New);
    assert_eq!(oc_classify(&unit, Some(&state(None))), Change::Rewritten);
    assert_eq!(
        oc_classify(&unit, Some(&state(Some("later")))),
        Change::Rewritten
    );
    assert_eq!(
        oc_classify(&unit, Some(&state(Some("2000")))),
        Change::Unchanged
    );
    assert_eq!(
        oc_classify(&unit, Some(&state(Some("1000")))),
        Change::Appended { from: 0 }
    );
}

/// A store that cannot be probed is re-read instead of assumed unchanged.
#[test]
fn oc_classify_should_report_appended_when_the_probe_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("opencode.db");
    std::fs::write(&path, b"definitely not sqlite, plain bytes here").unwrap();
    let unit = db_unit(&path).unwrap();
    assert_eq!(
        oc_classify(&unit, Some(&state(Some("1")))),
        Change::Appended { from: 0 }
    );
}

/// Text parts join with newlines; blank text, unparsable and unknown parts
/// add nothing; a tool part without a name is still shown as a tool call.
#[test]
fn oc_parse_should_build_turn_text_from_text_and_tool_parts_only() {
    let db = db();
    db.session("s", None, Some(""), Some("   "));
    let tool = serde_json::json!({"type": "tool", "state": {"input": {"q": 1}}}).to_string();
    db.message(
        "m1",
        "s",
        1,
        "assistant",
        &[
            &tool,
            &text("  "),
            "{bad json",
            &text("done"),
            r#"{"type":"step-start"}"#,
        ],
    );
    let out = oc_parse("opencode", &db.unit(), Change::New, None).unwrap();
    let s = &out.sessions[0];
    assert_eq!(s.turns.len(), 1);
    assert_eq!(s.turns[0].text, "\u{22ee}tool unknown {\"q\":1}\ndone");
    assert!(!s.turns[0].truncated);
    assert_eq!(s.session.cwd, None, "an empty directory is no cwd");
    assert_eq!(s.session.title, None, "a blank title is no title");
}

/// An oversized tool input is capped and the turn is marked truncated.
#[test]
fn oc_parse_should_cap_tool_input_and_mark_the_turn_truncated() {
    let db = db();
    db.session("s", None, None, None);
    let big = "x".repeat(TOOL_INPUT_CAP * 3);
    let tool =
        serde_json::json!({"type": "tool", "tool": "bash", "state": {"input": big}}).to_string();
    db.message("m1", "s", 1, "assistant", &[&tool]);
    let out = oc_parse("opencode", &db.unit(), Change::New, None).unwrap();
    let turn = &out.sessions[0].turns[0];
    assert!(turn.truncated);
    assert!(
        turn.text.starts_with("\u{22ee}tool bash \"xxx"),
        "{}",
        &turn.text[..40]
    );
    assert!(turn.text.len() < TOOL_INPUT_CAP * 2, "input was not capped");
}

/// Messages with no usable text are dropped; a role other than user or
/// assistant becomes a tool turn without intent.
#[test]
fn oc_parse_should_drop_empty_messages_and_map_other_roles_to_tool() {
    let db = db();
    db.session("s", None, None, None);
    db.message(
        "m1",
        "s",
        1,
        "user",
        &[r#"{"type":"reasoning","text":"hidden"}"#],
    );
    db.message("m2", "s", 2, "system", &[&text("compaction summary")]);
    let out = oc_parse("opencode", &db.unit(), Change::New, None).unwrap();
    let turns = &out.sessions[0].turns;
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].role, Role::Tool);
    assert_eq!(turns[0].intent_source, None);
    assert_eq!(turns[0].text, "compaction summary");
}

/// A child session is recorded as a subagent of its parent.
#[test]
fn oc_parse_should_mark_child_sessions_as_subagents() {
    let db = db();
    db.session("parent", None, None, None);
    db.session("child", Some("parent"), None, None);
    db.message("m1", "parent", 1, "user", &[&text("main task")]);
    db.message("m2", "child", 2, "user", &[&text("sub task")]);
    let out = oc_parse("zcode", &db.unit(), Change::New, None).unwrap();
    let flags: Vec<(String, bool, Option<String>, &str)> = {
        let mut v: Vec<_> = out
            .sessions
            .iter()
            .map(|s| {
                (
                    s.session.source_session_id.clone(),
                    s.session.is_subagent,
                    s.session.parent_source_session_id.clone(),
                    s.session.agent,
                )
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        flags,
        vec![
            (
                "child".to_string(),
                true,
                Some("parent".to_string()),
                "zcode"
            ),
            ("parent".to_string(), false, None, "zcode"),
        ]
    );
}

/// An incremental pass re-reads only sessions with a message newer than the
/// cursor, whole; sessions whose metadata row is gone or with no text are
/// skipped; the cursor moves to the newest message.
#[test]
fn oc_parse_should_rematerialize_only_sessions_newer_than_the_cursor() {
    let db = db();
    db.session("old", None, None, None);
    db.message("o1", "old", 100, "user", &[&text("old work")]);
    db.session("live", None, None, None);
    db.message("l1", "live", 150, "user", &[&text("start")]);
    db.message("l2", "live", 300, "assistant", &[&text("continued")]);
    db.message("g1", "ghost", 400, "user", &[&text("no session row")]);
    db.session("silent", None, None, None);
    db.message("q1", "silent", 500, "user", &[r#"{"type":"patch"}"#]);

    let out = oc_parse(
        "opencode",
        &db.unit(),
        Change::Appended { from: 0 },
        Some(&state(Some("200"))),
    )
    .unwrap();
    let got: Vec<(String, Vec<String>)> = out
        .sessions
        .iter()
        .map(|s| {
            (
                s.session.source_session_id.clone(),
                s.turns.iter().map(|t| t.text.clone()).collect(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![(
            "live".to_string(),
            vec!["start".to_string(), "continued".to_string()]
        )]
    );
    assert_eq!(out.sessions[0].op, SessionOp::Replace);
    assert_eq!(out.cursor.as_deref(), Some("500"));
}
