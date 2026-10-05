// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the Gemini (Antigravity CLI) history adapter: how
//! prompt lines group into sessions, where a resumed pass starts, and when
//! a resume is refused.

use super::*;

struct History {
    _dir: tempfile::TempDir,
    adapter: Adapter,
}

fn history(content: &str) -> History {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.jsonl");
    fs::write(&path, content).unwrap();
    History {
        _dir: dir,
        adapter: Adapter { history_path: path },
    }
}

impl History {
    fn unit(&self) -> SourceUnit {
        self.adapter.discover().unwrap().remove(0)
    }

    fn parse(&self, change: Change) -> ParseOutput {
        self.adapter.parse(&self.unit(), change, None).unwrap()
    }

    fn append(&self, text: &str) {
        let mut all = fs::read_to_string(&self.adapter.history_path).unwrap();
        all.push_str(text);
        fs::write(&self.adapter.history_path, all).unwrap();
    }
}

fn line(display: &str, conv: Option<&str>, workspace: Option<&str>, ts: i64) -> String {
    let mut v = serde_json::json!({"display": display, "timestamp": ts});
    if let Some(c) = conv {
        v["conversationId"] = c.into();
    }
    if let Some(w) = workspace {
        v["workspace"] = w.into();
    }
    format!("{v}\n")
}

fn summary(out: &ParseOutput) -> Vec<(String, Vec<String>)> {
    out.sessions
        .iter()
        .map(|s| {
            (
                s.session.source_session_id.clone(),
                s.turns.iter().map(|t| t.text.clone()).collect(),
            )
        })
        .collect()
}

/// Lines group by conversation in first-seen order, each a user turn with
/// its timestamp and the byte span it came from.
#[test]
fn parse_should_group_prompts_by_conversation_in_first_seen_order() {
    let a1 = line("first in a", Some("a"), Some("/repo/a"), 1_000);
    let b1 = line("first in b", Some("b"), None, 2_000);
    let a2 = line("second in a", Some("a"), Some("/elsewhere"), 3_000);
    let h = history(&format!("{a1}{b1}{a2}"));
    let out = h.parse(Change::New);
    assert_eq!(
        summary(&out),
        vec![
            (
                "a".to_string(),
                vec!["first in a".to_string(), "second in a".to_string()]
            ),
            ("b".to_string(), vec!["first in b".to_string()]),
        ]
    );
    let a = &out.sessions[0];
    assert_eq!(a.op, SessionOp::Replace);
    assert_eq!(a.session.agent, "gemini");
    assert_eq!(
        a.session.cwd.as_deref(),
        Some("/repo/a"),
        "first workspace wins"
    );
    assert_eq!(out.sessions[1].session.cwd, None);
    let spans: Vec<(Option<u64>, Option<u64>, Option<i64>)> = a
        .turns
        .iter()
        .map(|t| (t.source_byte_start, t.source_byte_len, t.ts))
        .collect();
    let a2_start = (a1.len() + b1.len()) as u64;
    assert_eq!(
        spans,
        vec![
            (Some(0), Some(a1.len() as u64), Some(1_000)),
            (Some(a2_start), Some(a2.len() as u64), Some(3_000)),
        ]
    );
    assert!(a.turns.iter().all(|t| t.role == Role::User));
    assert_eq!(out.consumed_bytes, (a1.len() + b1.len() + a2.len()) as u64);
    assert_eq!(out.cursor, None);
}

/// Lines without a conversation id still count, grouped together.
#[test]
fn parse_should_group_lines_without_a_conversation_id_together() {
    let h = history(&format!(
        "{}{}",
        line("orphan one", None, None, 1),
        line("orphan two", None, None, 2)
    ));
    let out = h.parse(Change::New);
    assert_eq!(
        summary(&out),
        vec![(
            NO_CONVERSATION.to_string(),
            vec!["orphan one".to_string(), "orphan two".to_string()]
        )]
    );
}

/// Blank prompts and lines without a `display` add no turn and no session.
#[test]
fn parse_should_skip_blank_and_display_less_lines() {
    let body = format!(
        "{}{}{}",
        line("   ", Some("blank"), None, 1),
        "{\"conversationId\":\"nodisplay\",\"timestamp\":2}\n",
        line("kept", Some("k"), None, 3)
    );
    let h = history(&body);
    let out = h.parse(Change::New);
    assert_eq!(
        summary(&out),
        vec![("k".to_string(), vec!["kept".to_string()])]
    );
    assert_eq!(
        out.consumed_bytes,
        body.len() as u64,
        "skipped lines are consumed"
    );
}

/// A last line without its newline is still being written: it is neither
/// parsed nor consumed, so the next pass reads it whole.
#[test]
fn parse_should_leave_a_partial_last_line_for_the_next_pass() {
    let done = line("done", Some("c"), None, 1);
    let h = history(&format!("{done}{{\"display\":\"half"));
    let out = h.parse(Change::New);
    assert_eq!(
        summary(&out),
        vec![("c".to_string(), vec!["done".to_string()])]
    );
    assert_eq!(out.consumed_bytes, done.len() as u64);
}

/// An appended pass starts at the recorded offset, emits `Append` sessions
/// and reports spans as absolute file offsets.
#[test]
fn parse_should_resume_at_the_offset_and_append_when_the_file_grew() {
    let first = line("before", Some("c"), None, 1);
    let h = history(&first);
    let full = h.parse(Change::New);
    let next = line("after", Some("c"), None, 2);
    h.append(&next);
    let out = h.parse(Change::Appended {
        from: full.consumed_bytes,
    });
    assert_eq!(
        summary(&out),
        vec![("c".to_string(), vec!["after".to_string()])]
    );
    assert_eq!(out.sessions[0].op, SessionOp::Append);
    assert_eq!(
        out.sessions[0].turns[0].source_byte_start,
        Some(first.len() as u64)
    );
    assert_eq!(out.consumed_bytes, (first.len() + next.len()) as u64);
}

fn ingested(consumed: u64, cursor: Option<String>) -> IngestState {
    IngestState {
        file_size: consumed as i64,
        mtime_ms: 0,
        bytes_ingested: consumed as i64,
        cursor,
    }
}

/// The cursor is the hash of the consumed tail: a file that only grew still
/// matches it, a file rewritten in place does not and is re-read.
#[test]
fn append_valid_should_accept_growth_and_refuse_an_in_place_rewrite() {
    let first = line("before", Some("c"), None, 1);
    let h = history(&first);
    let unit = h.unit();
    let consumed = first.len() as u64;
    let cursor = h.adapter.make_cursor(&unit, consumed);
    assert_eq!(cursor, file_tail_hash(&h.adapter.history_path, consumed));
    let state = ingested(consumed, cursor);

    h.append(&line("after", Some("c"), None, 2));
    assert!(h.adapter.append_valid(&h.unit(), &state));

    fs::write(
        &h.adapter.history_path,
        format!(
            "{}{}",
            line("BEFORE", Some("c"), None, 1),
            line("x", None, None, 3)
        ),
    )
    .unwrap();
    assert!(!h.adapter.append_valid(&h.unit(), &state));
}

/// Without a recorded cursor there is nothing to contradict the append.
#[test]
fn append_valid_should_accept_when_no_cursor_was_recorded() {
    let h = history(&line("x", Some("c"), None, 1));
    let state = ingested(0, None);
    assert!(h.adapter.append_valid(&h.unit(), &state));
    assert_eq!(h.adapter.agent(), "gemini");
}
