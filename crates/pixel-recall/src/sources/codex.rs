// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Codex CLI adapter: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` plus
//! `~/.codex/archived_sessions/*.jsonl`. Envelope per line:
//! `{timestamp, type, payload}`. Only `session_meta` and `response_item`
//! payloads carry indexable content — `event_msg/*` duplicates
//! `response_item` and is skipped wholesale to avoid double-counting.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

use serde_json::Value;

use crate::intent::classify_user_text;
use crate::model::{
    IntentSource, Role, TOOL_INPUT_CAP, TOOL_RESULT_CAP, TsSource, UnifiedSession, UnifiedTurn,
    cap_text, parse_iso_ms,
};
use crate::sources::{
    Change, IngestError, ParseOutput, ParsedSession, SessionOp, SourceAdapter, SourceUnit,
    file_tail_hash,
};
use crate::store::IngestState;

/// Codex-specific harness wrappers injected as "user" messages.
const CODEX_ORCHESTRATOR_PREFIXES: &[&str] = &[
    "<user_instructions>",
    "<environment_context>",
    "<turn_aborted>",
    "<skills_instructions>",
    "<permissions_context>",
    "<recommended_plugins>",
    "<codex_internal_context",
    "# AGENTS.md instructions",
];

pub struct Adapter {
    sessions_dir: PathBuf,
    archived_dir: PathBuf,
}

impl Adapter {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            sessions_dir: PathBuf::from(&home).join(".codex/sessions"),
            archived_dir: PathBuf::from(&home).join(".codex/archived_sessions"),
        }
    }
}

impl Default for Adapter {
    fn default() -> Self {
        Self::new()
    }
}

fn unit_for(path: PathBuf) -> Option<SourceUnit> {
    let meta = fs::metadata(&path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some(SourceUnit {
        unit_key: path.to_string_lossy().to_string(),
        size: meta.len(),
        mtime_ms,
        path,
    })
}

/// Recursively collect `rollout-*.jsonl` under `dir` (YYYY/MM/DD layout,
/// but tolerant of any nesting).
fn collect_rollouts(dir: &PathBuf, units: &mut Vec<SourceUnit>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollouts(&path, units);
        } else if path.extension().is_some_and(|e| e == "jsonl")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-"))
        {
            units.extend(unit_for(path));
        }
    }
}

impl SourceAdapter for Adapter {
    fn agent(&self) -> &'static str {
        "codex"
    }

    fn discover(&self) -> Result<Vec<SourceUnit>, IngestError> {
        let mut units = Vec::new();
        collect_rollouts(&self.sessions_dir, &mut units);
        if let Ok(entries) = fs::read_dir(&self.archived_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
                    units.extend(unit_for(path));
                }
            }
        }
        Ok(units)
    }

    fn parse(
        &self,
        unit: &SourceUnit,
        change: Change,
        _state: Option<&IngestState>,
    ) -> Result<ParseOutput, IngestError> {
        let start = match change {
            Change::Appended { from } => from,
            _ => 0,
        };
        let mut file = File::open(&unit.path)?;
        if start > 0 {
            file.seek(SeekFrom::Start(start))?;
        }
        let mut reader = BufReader::new(file);

        let stem = unit
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();

        let mut session = UnifiedSession {
            agent: "codex",
            source_session_id: stem,
            source_path: unit.path.to_string_lossy().to_string(),
            cwd: None,
            git_branch: None,
            title: None,
            ts_source: TsSource::Iso,
            is_subagent: false,
            parent_source_session_id: None,
        };

        let mut turns: Vec<UnifiedTurn> = Vec::new();
        let mut meta_seen = false;
        // On an appended resume the session_meta line sits before the resume
        // offset — recover the session identity from the file head so the
        // appended turns land on the id-keyed session, not a stem-keyed twin.
        if start > 0 {
            apply_head_meta(&unit.path, &mut session, &mut meta_seen);
        }
        let mut offset = start;
        let mut skipped_records = 0usize;
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break;
            }
            // A line without a trailing newline is a record still being
            // written — leave it for the next ingest pass.
            if !line.ends_with('\n') {
                break;
            }
            let line_start = offset;
            offset += n as u64;
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                skipped_records += 1;
                continue;
            };
            extract_record(
                &record,
                line_start,
                n as u64,
                &mut session,
                &mut turns,
                &mut meta_seen,
            );
        }

        let has_conversation =
            !turns.is_empty() || session.cwd.is_some() || matches!(change, Change::Appended { .. });
        let op = match change {
            Change::Appended { .. } => SessionOp::Append,
            _ => SessionOp::Replace,
        };
        Ok(ParseOutput {
            sessions: if has_conversation {
                vec![ParsedSession { op, session, turns }]
            } else {
                Vec::new()
            },
            skipped_records,
            consumed_bytes: offset,
            cursor: None,
        })
    }

    fn make_cursor(&self, unit: &SourceUnit, consumed: u64) -> Option<String> {
        file_tail_hash(&unit.path, consumed)
    }

    fn append_valid(&self, unit: &SourceUnit, state: &IngestState) -> bool {
        let Some(expected) = state.cursor.as_deref() else {
            return true;
        };
        file_tail_hash(&unit.path, state.bytes_ingested as u64).as_deref() == Some(expected)
    }
}

/// Read the first line of `path` and, when it is a session_meta record,
/// apply it to `session` (id, cwd, subagent parentage).
fn apply_head_meta(path: &std::path::Path, session: &mut UnifiedSession, meta_seen: &mut bool) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let mut reader = BufReader::new(file);
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() {
        return;
    }
    let Ok(record) = serde_json::from_str::<Value>(&first) else {
        return;
    };
    let mut no_turns = Vec::new();
    extract_record(&record, 0, 0, session, &mut no_turns, meta_seen);
}

fn extract_record(
    record: &Value,
    byte_start: u64,
    byte_len: u64,
    session: &mut UnifiedSession,
    turns: &mut Vec<UnifiedTurn>,
    meta_seen: &mut bool,
) {
    let etype = record.get("type").and_then(Value::as_str).unwrap_or("");
    let payload = record.get("payload").unwrap_or(&Value::Null);
    let ts = record
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_iso_ms);

    let mut push = |role: Role, text: String, truncated: bool, intent: Option<IntentSource>| {
        if text.trim().is_empty() {
            return;
        }
        turns.push(UnifiedTurn {
            role,
            intent_source: intent,
            ts,
            text,
            truncated,
            source_byte_start: Some(byte_start),
            source_byte_len: Some(byte_len),
        });
    };

    match etype {
        "session_meta" => {
            // Resumed/forked sessions embed further session_meta records
            // (including the parent thread's own meta) — only the FIRST one
            // describes this file. Honoring later ones collides ids across
            // files and clobbers previously ingested sessions.
            if *meta_seen {
                return;
            }
            *meta_seen = true;
            if let Some(id) = payload.get("id").and_then(Value::as_str) {
                session.source_session_id = id.to_string();
            }
            if session.cwd.is_none() {
                session.cwd = payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            // Subagent threads announce themselves: thread_source "subagent"
            // plus the parent thread id.
            if payload.get("thread_source").and_then(Value::as_str) == Some("subagent")
                && let Some(parent) = payload
                    .get("parent_thread_id")
                    .or_else(|| payload.get("session_id"))
                    .and_then(Value::as_str)
                    .filter(|p| *p != session.source_session_id)
            {
                session.is_subagent = true;
                session.parent_source_session_id = Some(parent.to_string());
            }
        }
        "response_item" => {
            let ptype = payload.get("type").and_then(Value::as_str).unwrap_or("");
            match ptype {
                "message" => {
                    let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                    // developer messages are base instructions — huge, skip.
                    if role != "user" && role != "assistant" {
                        return;
                    }
                    let text = join_content_text(payload.get("content"));
                    if text.trim().is_empty() {
                        return;
                    }
                    if role == "user" {
                        let intent = classify_codex_user(&text);
                        push(Role::User, text, false, Some(intent));
                    } else {
                        push(Role::Assistant, text, false, None);
                    }
                }
                "function_call" | "custom_tool_call" => {
                    let name = payload
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    // function_call carries `arguments`, custom_tool_call `input`.
                    let args = payload
                        .get("arguments")
                        .or_else(|| payload.get("input"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let (capped, truncated) = cap_text(args, TOOL_INPUT_CAP);
                    push(
                        Role::Assistant,
                        format!("\u{22ee}tool {name} {capped}"),
                        truncated,
                        None,
                    );
                }
                "function_call_output" | "custom_tool_call_output" => {
                    let text = tool_output_text(payload.get("output"));
                    if text.trim().is_empty() {
                        return;
                    }
                    let (capped, truncated) = cap_text(&text, TOOL_RESULT_CAP);
                    push(Role::Tool, capped, truncated, None);
                }
                // reasoning: encrypted ciphertext — skip.
                _ => {}
            }
        }
        // event_msg/* duplicates response_item content; turn_context,
        // world_state, compacted, token_count carry no recall value.
        _ => {}
    }
}

/// Join text parts of a codex message content array (input_text/output_text).
fn join_content_text(content: Option<&Value>) -> String {
    let mut out = String::new();
    match content {
        Some(Value::String(s)) => out.push_str(s),
        Some(Value::Array(parts)) => {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text") | Some("output_text") | Some("text") => {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            if !out.is_empty() {
                                out.push('\n');
                            }
                            out.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

/// `output` is a plain string, or an object with an `output`/`content`
/// string field.
fn tool_output_text(output: Option<&Value>) -> String {
    match output {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(map)) => map
            .get("output")
            .or_else(|| map.get("content"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn classify_codex_user(text: &str) -> IntentSource {
    let trimmed = text.trim_start();
    for p in CODEX_ORCHESTRATOR_PREFIXES {
        if trimmed.starts_with(p) {
            return IntentSource::Orchestrator;
        }
    }
    classify_user_text(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A complete line that is not valid JSON (a writer that crashed
    /// mid-flush) is consumed but counted: swallowing it would let a
    /// transcript whose records never parse look perfectly healthy.
    #[test]
    fn parse_counts_a_malformed_line_without_losing_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-2026-08-08T10-00-00-sess-1.jsonl");
        let meta = json!({
            "timestamp": "2026-08-08T10:00:00.000Z",
            "type": "session_meta",
            "payload": {"id": "sess-1", "cwd": "/work/app"}
        })
        .to_string();
        let message = json!({
            "timestamp": "2026-08-08T10:00:01.000Z",
            "type": "response_item",
            "payload": {
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "fix the login"}]
            }
        })
        .to_string();
        let truncated = "{\"timestamp\":\"2026-08-08T10:00:02.000Z\",\"paylo";
        fs::write(
            &path,
            format!("{meta}\n{message}\n{truncated}\n{message}\n"),
        )
        .unwrap();

        let unit = unit_for(path).unwrap();
        let out = Adapter::new().parse(&unit, Change::New, None).unwrap();

        assert_eq!(out.skipped_records, 1);
        assert_eq!(
            out.consumed_bytes, unit.size,
            "the malformed line's bytes are consumed, never re-read"
        );
        assert_eq!(out.sessions.len(), 1);
        assert_eq!(out.sessions[0].session.source_session_id, "sess-1");
        assert_eq!(out.sessions[0].turns.len(), 2, "both readable records land");
    }
}
