// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the Cursor adapter: transcript discovery (subagents
//! included), session identity, the user-query wrapper, and appends.

use super::*;
use serde_json::json;
use std::path::Path;

fn adapter(root: &Path) -> Adapter {
    Adapter {
        projects_dir: root.to_path_buf(),
    }
}

fn user(content: Value) -> String {
    json!({"role": "user", "message": {"content": content}}).to_string()
}

fn assistant(content: Value) -> String {
    json!({"role": "assistant", "message": {"content": content}}).to_string()
}

fn write_lines(path: &Path, lines: &[String]) {
    let mut body = lines.join("\n");
    body.push('\n');
    fs::write(path, body).unwrap();
}

fn turns_of(path: PathBuf) -> Vec<(Role, Option<IntentSource>, String, bool)> {
    let unit = unit_for(path).unwrap();
    let out = Adapter::new().parse(&unit, Change::New, None).unwrap();
    out.sessions[0]
        .turns
        .iter()
        .map(|t| (t.role, t.intent_source, t.text.clone(), t.truncated))
        .collect()
}

// --- discover ---------------------------------------------------------------

#[test]
fn discover_should_find_conversation_and_subagent_transcripts_only() {
    let tmp = tempfile::tempdir().unwrap();
    let conv = tmp.path().join("proj-a/agent-transcripts/conv-1");
    fs::create_dir_all(conv.join("subagents")).unwrap();
    fs::create_dir_all(conv.join("other")).unwrap();
    fs::write(conv.join("conv-1.jsonl"), "").unwrap();
    fs::write(conv.join("notes.txt"), "").unwrap();
    fs::write(conv.join("subagents/sub-1.jsonl"), "").unwrap();
    fs::write(conv.join("subagents/sub-1.txt"), "").unwrap();
    fs::write(conv.join("other/hidden.jsonl"), "").unwrap();
    // A stray file where conversation directories live, and a project
    // without transcripts, are both skipped.
    fs::write(tmp.path().join("proj-a/agent-transcripts/stray.jsonl"), "").unwrap();
    fs::create_dir_all(tmp.path().join("proj-b")).unwrap();

    let mut found: Vec<String> = adapter(tmp.path())
        .discover()
        .unwrap()
        .into_iter()
        .map(|u| u.path.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    found.sort();
    assert_eq!(found, vec!["conv-1.jsonl", "sub-1.jsonl"]);
}

#[test]
fn discover_should_return_nothing_when_cursor_was_never_used() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(
        adapter(&tmp.path().join("missing"))
            .discover()
            .unwrap()
            .is_empty()
    );
}

// --- session identity -------------------------------------------------------

#[test]
fn parse_should_key_a_subagent_transcript_under_its_parent_conversation() {
    let tmp = tempfile::tempdir().unwrap();
    let subs = tmp.path().join("conv-9/subagents");
    fs::create_dir_all(&subs).unwrap();
    let path = subs.join("sub-3.jsonl");
    write_lines(&path, &[user(json!("look around"))]);
    let unit = unit_for(path).unwrap();
    let out = Adapter::new().parse(&unit, Change::New, None).unwrap();
    let s = &out.sessions[0].session;
    assert_eq!(s.source_session_id, "conv-9/sub-3");
    assert!(s.is_subagent);
    assert_eq!(s.parent_source_session_id.as_deref(), Some("conv-9"));
    assert_eq!(s.ts_source, TsSource::Mtime);
    assert_eq!(out.sessions[0].turns[0].ts, Some(unit.mtime_ms));
}

#[test]
fn parse_should_key_a_main_transcript_by_its_file_stem() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("conv-2.jsonl");
    write_lines(&path, &[user(json!("hi"))]);
    let unit = unit_for(path).unwrap();
    let out = Adapter::new().parse(&unit, Change::New, None).unwrap();
    let s = &out.sessions[0].session;
    assert_eq!(s.source_session_id, "conv-2");
    assert!(!s.is_subagent);
    assert_eq!(s.parent_source_session_id, None);
    assert_eq!(out.sessions[0].op, SessionOp::Replace);
}

// --- records ------------------------------------------------------------------

#[test]
fn parse_should_index_only_the_user_query_inside_the_cursor_wrapper() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("c.jsonl");
    write_lines(
        &path,
        &[
            user(
                json!([{"type": "text", "text": "<timestamp>t</timestamp>\n<user_query>\nwhy is it slow\n</user_query>"}]),
            ),
            user(json!("<user_query>unterminated query")),
            user(json!("no wrapper at all")),
            user(json!([
                {"type": "text", "text": "part one"},
                {"type": "image", "url": "x"},
                {"type": "text", "text": "part two"}
            ])),
            user(json!("<hooks_context>injected</hooks_context>")),
            user(json!("<user_query>\n\n</user_query>")),
            user(json!(42)),
        ],
    );
    assert_eq!(
        turns_of(path),
        vec![
            (
                Role::User,
                Some(IntentSource::Human),
                "why is it slow".to_string(),
                false
            ),
            (
                Role::User,
                Some(IntentSource::Human),
                "unterminated query".to_string(),
                false
            ),
            (
                Role::User,
                Some(IntentSource::Human),
                "no wrapper at all".to_string(),
                false
            ),
            (
                Role::User,
                Some(IntentSource::Human),
                "part one\npart two".to_string(),
                false
            ),
            (
                Role::User,
                Some(IntentSource::Orchestrator),
                "<hooks_context>injected</hooks_context>".to_string(),
                false
            ),
        ]
    );
}

#[test]
fn parse_should_fold_assistant_text_and_tool_calls_and_flag_a_capped_input() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("c.jsonl");
    let long = "i".repeat(TOOL_INPUT_CAP + 50);
    write_lines(
        &path,
        &[
            assistant(json!([
                {"type": "tool_use", "input": {"q": 1}},
                {"type": "text", "text": "then text"},
                {"type": "thinking", "text": "hidden"}
            ])),
            assistant(json!([{"type": "tool_use", "name": "grep", "input": long}])),
            assistant(json!([{"type": "tool_use", "name": "noop"}])),
            assistant(json!("a bare string is not read")),
            json!({"role": "tool", "message": {"content": "skipped"}}).to_string(),
        ],
    );
    let turns = turns_of(path);
    assert_eq!(
        turns[0],
        (
            Role::Assistant,
            None,
            "\u{22ee}tool unknown {\"q\":1}\nthen text".to_string(),
            false
        )
    );
    assert_eq!(turns[1].0, Role::Assistant);
    assert!(
        turns[1].3,
        "an input over TOOL_INPUT_CAP is flagged truncated"
    );
    assert!(turns[1].2.starts_with("\u{22ee}tool grep \"iii"));
    assert_eq!(
        turns[2],
        (
            Role::Assistant,
            None,
            "\u{22ee}tool noop ".to_string(),
            false
        )
    );
    assert_eq!(turns.len(), 3, "{turns:?}");
}

#[test]
fn parse_should_return_no_session_when_a_new_file_holds_no_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("c.jsonl");
    write_lines(&path, &[json!({"role": "system"}).to_string()]);
    let unit = unit_for(path).unwrap();
    let out = Adapter::new().parse(&unit, Change::New, None).unwrap();
    assert!(out.sessions.is_empty());
    assert_eq!(out.consumed_bytes, unit.size);
}

// --- appends --------------------------------------------------------------------

#[test]
fn parse_should_append_only_the_records_after_the_offset() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("c.jsonl");
    let old = user(json!("old question"));
    let new = user(json!("new question"));
    write_lines(&path, &[old.clone(), new]);
    let unit = unit_for(path).unwrap();
    let from = old.len() as u64 + 1;
    let out = Adapter::new()
        .parse(&unit, Change::Appended { from }, None)
        .unwrap();
    let s = &out.sessions[0];
    assert_eq!(s.op, SessionOp::Append);
    let texts: Vec<&str> = s.turns.iter().map(|t| t.text.as_str()).collect();
    assert_eq!(texts, vec!["new question"]);
    assert_eq!(s.turns[0].source_byte_start, Some(from));
}

#[test]
fn parse_should_keep_an_appended_session_when_the_tail_is_still_being_written() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("c.jsonl");
    let old = user(json!("old"));
    fs::write(&path, format!("{old}\n{{\"role\":\"us")).unwrap();
    let unit = unit_for(path).unwrap();
    let from = old.len() as u64 + 1;
    let out = Adapter::new()
        .parse(&unit, Change::Appended { from }, None)
        .unwrap();
    assert_eq!(out.sessions.len(), 1);
    assert!(out.sessions[0].turns.is_empty());
    assert_eq!(
        out.consumed_bytes, from,
        "the partial line is left for later"
    );
}

#[test]
fn append_valid_should_accept_an_untouched_prefix_and_reject_a_rewritten_one() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("c.jsonl");
    fs::write(&path, "line one\n").unwrap();
    let unit = unit_for(path.clone()).unwrap();
    let a = Adapter::new();
    let cursor = a.make_cursor(&unit, unit.size);
    let state = |cursor: Option<String>| IngestState {
        file_size: unit.size as i64,
        mtime_ms: unit.mtime_ms,
        bytes_ingested: unit.size as i64,
        cursor,
    };
    assert!(a.append_valid(&unit, &state(None)));
    fs::write(&path, "line one\nline two\n").unwrap();
    assert!(a.append_valid(&unit, &state(cursor.clone())));
    fs::write(&path, "LINE ONE\nline two\n").unwrap();
    assert!(!a.append_valid(&unit, &state(cursor)));
}

#[test]
fn agent_should_name_cursor() {
    assert_eq!(Adapter::new().agent(), "cursor");
}
