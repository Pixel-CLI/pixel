// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the Codex adapter: which files it discovers, which
//! records become turns (and with which role and intent), and how an
//! appended resume keeps the session identity.

use super::*;
use serde_json::json;
use std::path::Path;

fn adapter(root: &Path) -> Adapter {
    Adapter {
        sessions_dir: root.join("sessions"),
        archived_dir: root.join("archived"),
    }
}

fn line(etype: &str, payload: Value) -> String {
    json!({"timestamp": "2026-08-08T10:00:00.000Z", "type": etype, "payload": payload}).to_string()
}

fn message(role: &str, content: Value) -> String {
    line(
        "response_item",
        json!({"type": "message", "role": role, "content": content}),
    )
}

fn write_lines(path: &Path, lines: &[String]) {
    let mut body = lines.join("\n");
    body.push('\n');
    fs::write(path, body).unwrap();
}

fn parse_new(path: PathBuf) -> ParseOutput {
    let unit = unit_for(path).unwrap();
    Adapter::new().parse(&unit, Change::New, None).unwrap()
}

fn texts(out: &ParseOutput) -> Vec<(Role, Option<IntentSource>, String)> {
    out.sessions[0]
        .turns
        .iter()
        .map(|t| (t.role, t.intent_source, t.text.clone()))
        .collect()
}

// --- discover -------------------------------------------------------------

#[test]
fn discover_should_find_nested_rollouts_and_every_archived_jsonl_only() {
    let tmp = tempfile::tempdir().unwrap();
    let day = tmp.path().join("sessions/2026/08/08");
    fs::create_dir_all(&day).unwrap();
    fs::write(day.join("rollout-a.jsonl"), "").unwrap();
    fs::write(day.join("notes.jsonl"), "").unwrap();
    fs::write(day.join("rollout-b.json"), "").unwrap();
    let archived = tmp.path().join("archived");
    fs::create_dir_all(archived.join("sub")).unwrap();
    fs::write(archived.join("old.jsonl"), "").unwrap();
    fs::write(archived.join("old.txt"), "").unwrap();

    let mut found: Vec<String> = adapter(tmp.path())
        .discover()
        .unwrap()
        .into_iter()
        .map(|u| u.path.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    found.sort();
    assert_eq!(found, vec!["old.jsonl", "rollout-a.jsonl"]);
}

#[test]
fn discover_should_return_nothing_when_codex_was_never_used() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(adapter(tmp.path()).discover().unwrap().is_empty());
}

// --- parse: records -------------------------------------------------------

#[test]
fn parse_should_turn_each_indexable_record_into_one_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("rollout-x.jsonl");
    write_lines(
        &path,
        &[
            line("session_meta", json!({"id": "sess-9", "cwd": "/w/app"})),
            message("developer", json!("base instructions")),
            message("user", json!("plain string question")),
            message(
                "user",
                json!([
                    {"type": "input_text", "text": "first part"},
                    {"type": "input_image", "image_url": "x"},
                    {"type": "text", "text": "second part"}
                ]),
            ),
            message(
                "user",
                json!([{"type": "input_text", "text": "<environment_context>cwd</environment_context>"}]),
            ),
            message(
                "assistant",
                json!([{"type": "output_text", "text": "the answer"}]),
            ),
            message("assistant", json!([{"type": "output_text", "text": "   "}])),
            line(
                "response_item",
                json!({"type": "function_call", "name": "shell", "arguments": "{\"cmd\":\"ls\"}"}),
            ),
            line(
                "response_item",
                json!({"type": "custom_tool_call", "input": "patch body"}),
            ),
            line(
                "response_item",
                json!({"type": "function_call_output", "output": "file list"}),
            ),
            line(
                "response_item",
                json!({"type": "custom_tool_call_output", "output": {"content": "applied"}}),
            ),
            line(
                "response_item",
                json!({"type": "function_call_output", "output": {"output": "exit 0"}}),
            ),
            line(
                "response_item",
                json!({"type": "function_call_output", "output": 42}),
            ),
            line(
                "response_item",
                json!({"type": "reasoning", "content": "secret"}),
            ),
            line(
                "event_msg",
                json!({"type": "user_message", "message": "dup"}),
            ),
        ],
    );
    let out = parse_new(path);
    assert_eq!(out.sessions.len(), 1);
    let s = &out.sessions[0];
    assert_eq!(s.op, SessionOp::Replace);
    assert_eq!(s.session.source_session_id, "sess-9");
    assert_eq!(s.session.cwd.as_deref(), Some("/w/app"));
    assert!(!s.session.is_subagent);
    assert_eq!(
        texts(&out),
        vec![
            (
                Role::User,
                Some(IntentSource::Human),
                "plain string question".to_string()
            ),
            (
                Role::User,
                Some(IntentSource::Human),
                "first part\nsecond part".to_string()
            ),
            (
                Role::User,
                Some(IntentSource::Orchestrator),
                "<environment_context>cwd</environment_context>".to_string()
            ),
            (Role::Assistant, None, "the answer".to_string()),
            (
                Role::Assistant,
                None,
                "\u{22ee}tool shell {\"cmd\":\"ls\"}".to_string()
            ),
            (
                Role::Assistant,
                None,
                "\u{22ee}tool unknown patch body".to_string()
            ),
            (Role::Tool, None, "file list".to_string()),
            (Role::Tool, None, "applied".to_string()),
            (Role::Tool, None, "exit 0".to_string()),
        ]
    );
    let first = &s.turns[0];
    assert_eq!(first.ts, parse_iso_ms("2026-08-08T10:00:00.000Z"));
    assert!(
        first.source_byte_start.is_some_and(|b| b > 0),
        "after the meta line"
    );
}

#[test]
fn parse_should_honour_only_the_first_session_meta_of_a_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("rollout-fork.jsonl");
    write_lines(
        &path,
        &[
            line("session_meta", json!({"id": "child", "cwd": "/w/child"})),
            line("session_meta", json!({"id": "parent", "cwd": "/w/parent"})),
            message("user", json!("hello")),
        ],
    );
    let out = parse_new(path);
    let session = &out.sessions[0].session;
    assert_eq!(session.source_session_id, "child");
    assert_eq!(session.cwd.as_deref(), Some("/w/child"));
}

#[test]
fn parse_should_link_a_subagent_thread_to_its_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let with_parent = tmp.path().join("rollout-sub.jsonl");
    write_lines(
        &with_parent,
        &[line(
            "session_meta",
            json!({"id": "sub-1", "thread_source": "subagent", "parent_thread_id": "main-1", "cwd": "/w"}),
        )],
    );
    let session = &parse_new(with_parent).sessions[0].session;
    assert!(session.is_subagent);
    assert_eq!(session.parent_source_session_id.as_deref(), Some("main-1"));

    let via_session_id = tmp.path().join("rollout-sub2.jsonl");
    write_lines(
        &via_session_id,
        &[line(
            "session_meta",
            json!({"id": "sub-2", "thread_source": "subagent", "session_id": "main-2", "cwd": "/w"}),
        )],
    );
    let session = &parse_new(via_session_id).sessions[0].session;
    assert_eq!(session.parent_source_session_id.as_deref(), Some("main-2"));
}

#[test]
fn parse_should_not_make_a_thread_its_own_parent_or_trust_other_thread_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let selfref = tmp.path().join("rollout-self.jsonl");
    write_lines(
        &selfref,
        &[line(
            "session_meta",
            json!({"id": "same", "thread_source": "subagent", "parent_thread_id": "same", "cwd": "/w"}),
        )],
    );
    let session = &parse_new(selfref).sessions[0].session;
    assert!(!session.is_subagent);
    assert_eq!(session.parent_source_session_id, None);

    let user_thread = tmp.path().join("rollout-user.jsonl");
    write_lines(
        &user_thread,
        &[line(
            "session_meta",
            json!({"id": "u", "thread_source": "user", "parent_thread_id": "p", "cwd": "/w"}),
        )],
    );
    assert!(!parse_new(user_thread).sessions[0].session.is_subagent);
}

#[test]
fn parse_should_flag_a_tool_output_cut_at_the_result_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("rollout-big.jsonl");
    let big = "o".repeat(TOOL_RESULT_CAP + 10);
    write_lines(
        &path,
        &[line(
            "response_item",
            json!({"type": "function_call_output", "output": big}),
        )],
    );
    let out = parse_new(path);
    let turn = &out.sessions[0].turns[0];
    assert!(turn.truncated);
    assert!(
        turn.text.len() <= TOOL_RESULT_CAP + 3,
        "{}",
        turn.text.len()
    );
}

#[test]
fn parse_should_return_no_session_when_a_file_holds_no_conversation() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("rollout-empty.jsonl");
    write_lines(&path, &[line("event_msg", json!({"type": "token_count"}))]);
    let out = parse_new(path);
    assert!(out.sessions.is_empty());
    assert_eq!(out.skipped_records, 0);
}

#[test]
fn parse_should_leave_an_unterminated_last_line_for_the_next_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("rollout-partial.jsonl");
    let meta = line("session_meta", json!({"id": "s", "cwd": "/w"}));
    let partial = message("user", json!("half written"));
    fs::write(&path, format!("{meta}\n{partial}")).unwrap();
    let out = parse_new(path);
    assert_eq!(out.consumed_bytes, meta.len() as u64 + 1);
    assert!(out.sessions[0].turns.is_empty());
}

// --- parse: appended resume -------------------------------------------------

#[test]
fn parse_should_append_from_the_offset_under_the_session_id_from_the_file_head() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("rollout-stem-name.jsonl");
    let meta = line("session_meta", json!({"id": "real-id", "cwd": "/w/app"}));
    let old = message("user", json!("already ingested"));
    let new = message("assistant", json!("fresh reply"));
    write_lines(&path, &[meta.clone(), old.clone(), new]);
    let from = (meta.len() + old.len() + 2) as u64;

    let unit = unit_for(path).unwrap();
    let out = Adapter::new()
        .parse(&unit, Change::Appended { from }, None)
        .unwrap();
    let s = &out.sessions[0];
    assert_eq!(s.op, SessionOp::Append);
    assert_eq!(s.session.source_session_id, "real-id");
    assert_eq!(s.session.cwd.as_deref(), Some("/w/app"));
    assert_eq!(
        texts(&out),
        vec![(Role::Assistant, None, "fresh reply".to_string())]
    );
    assert_eq!(s.turns[0].source_byte_start, Some(from));
    assert_eq!(out.consumed_bytes, unit.size);
}

#[test]
fn parse_should_keep_an_appended_session_even_when_the_tail_adds_no_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("rollout-quiet.jsonl");
    let meta = line("session_meta", json!({"id": "q"}));
    write_lines(&path, &[meta.clone(), line("event_msg", json!({}))]);
    let unit = unit_for(path).unwrap();
    let out = Adapter::new()
        .parse(
            &unit,
            Change::Appended {
                from: meta.len() as u64 + 1,
            },
            None,
        )
        .unwrap();
    assert_eq!(
        out.sessions.len(),
        1,
        "an appended unit always reports its session"
    );
    assert_eq!(out.sessions[0].session.source_session_id, "q");
}

// --- cursor -----------------------------------------------------------------

#[test]
fn append_valid_should_accept_an_untouched_prefix_and_reject_a_rewritten_one() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("rollout-c.jsonl");
    fs::write(&path, "first line\n").unwrap();
    let unit = unit_for(path.clone()).unwrap();
    let a = Adapter::new();
    let cursor = a.make_cursor(&unit, unit.size);
    assert!(cursor.is_some());
    let state = |cursor: Option<String>| IngestState {
        file_size: unit.size as i64,
        mtime_ms: unit.mtime_ms,
        bytes_ingested: unit.size as i64,
        cursor,
    };
    assert!(
        a.append_valid(&unit, &state(None)),
        "no cursor: nothing to check"
    );

    fs::write(&path, "first line\nsecond line\n").unwrap();
    assert!(
        a.append_valid(&unit, &state(cursor.clone())),
        "appended only"
    );

    fs::write(&path, "FIRST LINE\nsecond line\n").unwrap();
    assert!(
        !a.append_valid(&unit, &state(cursor)),
        "the ingested prefix changed"
    );
}

#[test]
fn agent_should_name_codex() {
    assert_eq!(Adapter::new().agent(), "codex");
}
