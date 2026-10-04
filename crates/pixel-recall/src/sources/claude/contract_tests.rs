// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the Claude Code transcript adapter: which files are
//! sessions (subagent transcripts included), how a resumed pass reads only
//! the new tail, and which content becomes turn text.

use super::*;
use serde_json::json;

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn rec(v: &Value) -> String {
    format!("{v}\n")
}

fn user(text: &str) -> String {
    rec(&json!({"type": "user", "cwd": "/repo", "message": {"content": text}}))
}

fn parse(path: &Path, change: Change) -> ParseOutput {
    let unit = unit_for(path.to_path_buf()).unwrap();
    ClaudeAdapter::with_root(PathBuf::from("/unused"))
        .parse(&unit, change, None)
        .unwrap()
}

/// Every session transcript and every subagent transcript is a unit; files
/// that are not `.jsonl`, stray files at the projects root and session
/// directories without `subagents/` are not.
#[test]
fn discover_should_list_session_and_subagent_transcripts_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let proj = root.join("-repo");
    write(&proj.join("s1.jsonl"), "{}\n");
    write(&proj.join("notes.txt"), "x");
    write(&proj.join("s1/subagents/agent-a.jsonl"), "{}\n");
    write(&proj.join("s1/subagents/readme.md"), "x");
    fs::create_dir_all(proj.join("s2")).unwrap();
    write(&root.join("stray.jsonl"), "{}\n");

    let mut found: Vec<PathBuf> = ClaudeAdapter::with_root(root.clone())
        .discover()
        .unwrap()
        .into_iter()
        .map(|u| u.path)
        .collect();
    found.sort();
    assert_eq!(
        found,
        vec![
            proj.join("s1/subagents/agent-a.jsonl"),
            proj.join("s1.jsonl")
        ]
    );
}

#[test]
fn discover_should_find_nothing_when_the_projects_directory_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = ClaudeAdapter::with_root(dir.path().join("none"));
    assert!(adapter.discover().unwrap().is_empty());
    assert_eq!(adapter.agent(), "claude");
}

/// A subagent transcript is its own session, keyed under its parent and
/// marked as a subagent of it.
#[test]
fn parse_should_key_a_subagent_transcript_under_its_parent_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("-repo/abc-123/subagents/agent-x.jsonl");
    write(&path, &user("explore the parser"));
    let out = parse(&path, Change::New);
    let s = &out.sessions[0].session;
    assert_eq!(s.source_session_id, "abc-123/agent-x");
    assert!(s.is_subagent);
    assert_eq!(s.parent_source_session_id.as_deref(), Some("abc-123"));

    let top = dir.path().join("-repo/abc-123.jsonl");
    write(&top, &user("main"));
    let s = &parse(&top, Change::New).sessions[0].session;
    assert_eq!(s.source_session_id, "abc-123");
    assert!(!s.is_subagent);
    assert_eq!(s.parent_source_session_id, None);
}

/// A resumed pass reads from the recorded offset, appends, and leaves a
/// half-written last line for the next pass.
#[test]
fn parse_should_resume_at_the_offset_and_leave_a_partial_line_unread() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p/s.jsonl");
    let first = user("first");
    let second = user("second");
    write(&path, &format!("{first}{second}{{\"type\":\"user\",\"mess"));
    let out = parse(
        &path,
        Change::Appended {
            from: first.len() as u64,
        },
    );
    assert_eq!(out.sessions[0].op, SessionOp::Append);
    let texts: Vec<&str> = out.sessions[0]
        .turns
        .iter()
        .map(|t| t.text.as_str())
        .collect();
    assert_eq!(texts, vec!["second"]);
    assert_eq!(
        out.sessions[0].turns[0].source_byte_start,
        Some(first.len() as u64)
    );
    assert_eq!(out.consumed_bytes, (first.len() + second.len()) as u64);
}

/// A file with no turn and no cwd yields no session on a full pass; an
/// appended pass always reports its session so the store can extend it.
#[test]
fn parse_should_emit_a_session_only_with_content_or_when_appending() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p/s.jsonl");
    write(&path, &rec(&json!({"type": "summary", "summary": "x"})));
    assert!(parse(&path, Change::New).sessions.is_empty());
    let appended = parse(&path, Change::Appended { from: 0 });
    assert_eq!(appended.sessions.len(), 1);
    assert!(appended.sessions[0].turns.is_empty());

    let with_cwd = dir.path().join("p/c.jsonl");
    write(
        &with_cwd,
        &rec(&json!({"type": "assistant", "cwd": "/repo", "message": {"content": []}})),
    );
    let out = parse(&with_cwd, Change::New);
    assert_eq!(out.sessions.len(), 1, "a known cwd is a session identity");
    assert!(out.sessions[0].turns.is_empty());
}

/// Tool results in list form join their text parts; several results of one
/// record form one tool turn, capped and marked truncated when too long.
#[test]
fn extract_record_should_join_and_cap_tool_results() {
    let mut session = UnifiedSession {
        agent: "claude",
        source_session_id: "s".into(),
        source_path: String::new(),
        cwd: None,
        git_branch: None,
        title: None,
        ts_source: TsSource::Iso,
        is_subagent: false,
        parent_source_session_id: None,
    };
    let mut turns = Vec::new();
    let record = json!({"type": "user", "gitBranch": "", "message": {"content": [
        {"type": "tool_result", "content": [
            {"type": "text", "text": "line one"},
            {"type": "image", "source": "x"},
            {"type": "text", "text": "line two"}
        ]},
        {"type": "tool_result", "content": "   "},
        {"type": "tool_result", "content": 5},
        {"type": "tool_result", "content": "second result"}
    ]}});
    extract_record(&record, 0, 1, &mut session, &mut turns);
    assert_eq!(session.git_branch, None, "an empty branch is no branch");
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].role, Role::Tool);
    assert_eq!(turns[0].text, "line one\nline two\nsecond result");
    assert!(!turns[0].truncated);
    assert_eq!(turns[0].intent_source, None);

    let mut turns = Vec::new();
    let huge = "y".repeat(TOOL_RESULT_CAP + 10);
    let record = json!({"type": "user", "message": {"content": [
        {"type": "tool_result", "content": huge}
    ]}});
    extract_record(&record, 0, 1, &mut session, &mut turns);
    assert!(turns[0].truncated);
    assert_eq!(turns[0].text.len(), TOOL_RESULT_CAP);
}

/// Several text parts of one user record are one turn; an assistant tool
/// call with an oversized input is capped and marked truncated, and one
/// without a name is still shown.
#[test]
fn extract_record_should_join_user_text_parts_and_cap_tool_inputs() {
    let mut session = UnifiedSession {
        agent: "claude",
        source_session_id: "s".into(),
        source_path: String::new(),
        cwd: None,
        git_branch: None,
        title: None,
        ts_source: TsSource::Iso,
        is_subagent: false,
        parent_source_session_id: None,
    };
    let mut turns = Vec::new();
    let user_rec = json!({"type": "user", "message": {"content": [
        {"type": "text", "text": "part one"},
        {"type": "text", "text": "part two"}
    ]}});
    extract_record(&user_rec, 0, 1, &mut session, &mut turns);
    let big = "z".repeat(TOOL_INPUT_CAP * 2);
    let assistant = json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "input": big},
        {"type": "text", "text": "after"}
    ]}});
    extract_record(&assistant, 0, 1, &mut session, &mut turns);
    let empty = json!({"type": "assistant", "message": {"content": "plain string"}});
    extract_record(&empty, 0, 1, &mut session, &mut turns);

    assert_eq!(turns.len(), 2, "an assistant string content yields no turn");
    assert_eq!(turns[0].text, "part one\npart two");
    assert_eq!(turns[0].role, Role::User);
    assert!(turns[1].truncated);
    assert!(turns[1].text.starts_with("\u{22ee}tool unknown \"zzz"));
    assert!(turns[1].text.ends_with("\nafter"));
}

/// The resume guard hashes the consumed tail: growth keeps it, an in-place
/// rewrite breaks it, and no recorded cursor trusts the append.
#[test]
fn append_valid_should_accept_growth_and_refuse_an_in_place_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p/s.jsonl");
    let first = user("first");
    write(&path, &first);
    let adapter = ClaudeAdapter::with_root(dir.path().to_path_buf());
    let unit = unit_for(path.clone()).unwrap();
    let consumed = first.len() as u64;
    let cursor = adapter.make_cursor(&unit, consumed);
    assert_eq!(cursor, file_tail_hash(&path, consumed));
    let state = IngestState {
        file_size: consumed as i64,
        mtime_ms: 0,
        bytes_ingested: consumed as i64,
        cursor,
    };
    write(&path, &format!("{first}{}", user("more")));
    assert!(adapter.append_valid(&unit_for(path.clone()).unwrap(), &state));
    write(&path, &format!("{}{}", user("FIRST"), user("more")));
    assert!(!adapter.append_valid(&unit_for(path.clone()).unwrap(), &state));
    let no_cursor = IngestState {
        cursor: None,
        ..state
    };
    assert!(adapter.append_valid(&unit_for(path).unwrap(), &no_cursor));
}
