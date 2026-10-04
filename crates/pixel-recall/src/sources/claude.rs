// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Claude Code adapter: `~/.claude/projects/<slug>/<session>.jsonl` plus the
//! subagent transcripts at `<slug>/<session>/subagents/agent-*.jsonl` (the
//! majority of files — never skip them).

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

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

pub struct ClaudeAdapter {
    projects_dir: PathBuf,
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            projects_dir: PathBuf::from(home).join(".claude/projects"),
        }
    }

    pub fn with_root(projects_dir: PathBuf) -> Self {
        Self { projects_dir }
    }
}

impl Default for ClaudeAdapter {
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

impl SourceAdapter for ClaudeAdapter {
    fn agent(&self) -> &'static str {
        "claude"
    }

    fn discover(&self) -> Result<Vec<SourceUnit>, IngestError> {
        let mut units = Vec::new();
        let projects = match fs::read_dir(&self.projects_dir) {
            Ok(rd) => rd,
            Err(_) => return Ok(units), // no Claude store on this machine
        };
        for project in projects.flatten() {
            let ppath = project.path();
            if !ppath.is_dir() {
                continue;
            }
            let Ok(entries) = fs::read_dir(&ppath) else {
                continue;
            };
            for entry in entries.flatten() {
                let epath = entry.path();
                if epath.extension().is_some_and(|e| e == "jsonl") {
                    units.extend(unit_for(epath));
                } else if epath.is_dir() {
                    // <session-uuid>/subagents/agent-*.jsonl
                    let sub = epath.join("subagents");
                    let Ok(subs) = fs::read_dir(&sub) else {
                        continue;
                    };
                    for f in subs.flatten() {
                        let fpath = f.path();
                        if fpath.extension().is_some_and(|e| e == "jsonl") {
                            units.extend(unit_for(fpath));
                        }
                    }
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
        let (is_subagent, parent) = subagent_parent(&unit.path);
        let source_session_id = match &parent {
            Some(p) => format!("{p}/{stem}"),
            None => stem,
        };

        let mut session = UnifiedSession {
            agent: "claude",
            source_session_id,
            source_path: unit.path.to_string_lossy().to_string(),
            cwd: None,
            git_branch: None,
            title: None,
            ts_source: TsSource::Iso,
            is_subagent,
            parent_source_session_id: parent,
        };

        let mut turns: Vec<UnifiedTurn> = Vec::new();
        let mut skipped_records = 0usize;
        let mut offset = start;
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
            extract_record(&record, line_start, n as u64, &mut session, &mut turns);
        }

        // A file with no conversation and no session identity yields no
        // session row; the ingester records the consumed bytes regardless.
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
            // No guard recorded (pre-cursor state) — trust append-only.
            return true;
        };
        file_tail_hash(&unit.path, state.bytes_ingested as u64).as_deref() == Some(expected)
    }
}

/// `.../projects/<slug>/<parent-uuid>/subagents/agent-x.jsonl` → parent uuid.
fn subagent_parent(path: &Path) -> (bool, Option<String>) {
    let comps: Vec<&str> = path.iter().filter_map(|c| c.to_str()).collect();
    if comps.len() >= 3 && comps[comps.len() - 2] == "subagents" {
        return (true, Some(comps[comps.len() - 3].to_string()));
    }
    (false, None)
}

fn extract_record(
    record: &Value,
    byte_start: u64,
    byte_len: u64,
    session: &mut UnifiedSession,
    turns: &mut Vec<UnifiedTurn>,
) {
    let rtype = record.get("type").and_then(Value::as_str).unwrap_or("");
    if rtype != "user" && rtype != "assistant" {
        return;
    }
    if session.cwd.is_none() {
        session.cwd = record
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    if session.git_branch.is_none() {
        session.git_branch = record
            .get("gitBranch")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }
    let ts = record
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_iso_ms);
    let message = record.get("message").cloned().unwrap_or(Value::Null);
    let content = message.get("content");

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

    match rtype {
        "user" => {
            let mut user_text = String::new();
            let mut tool_results: Vec<String> = Vec::new();
            match content {
                Some(Value::String(s)) => user_text.push_str(s),
                Some(Value::Array(parts)) => {
                    for part in parts {
                        match part.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(t) = part.get("text").and_then(Value::as_str) {
                                    if !user_text.is_empty() {
                                        user_text.push('\n');
                                    }
                                    user_text.push_str(t);
                                }
                            }
                            Some("tool_result") => {
                                let text = tool_result_text(part);
                                if !text.trim().is_empty() {
                                    tool_results.push(text);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            if !user_text.trim().is_empty() {
                let intent = classify_user_text(&user_text);
                push(Role::User, user_text, false, Some(intent));
            }
            if !tool_results.is_empty() {
                let joined = tool_results.join("\n");
                let (capped, truncated) = cap_text(&joined, TOOL_RESULT_CAP);
                push(Role::Tool, capped, truncated, None);
            }
        }
        "assistant" => {
            let mut text = String::new();
            let mut any_truncated = false;
            if let Some(Value::Array(parts)) = content {
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = part.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(t);
                            }
                        }
                        Some("tool_use") => {
                            let name = part
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown");
                            let input = part
                                .get("input")
                                .map(ToString::to_string)
                                .unwrap_or_default();
                            let (capped, truncated) = cap_text(&input, TOOL_INPUT_CAP);
                            any_truncated |= truncated;
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(&format!("\u{22ee}tool {name} {capped}"));
                        }
                        // thinking: skipped in v1 (volume without recall value)
                        _ => {}
                    }
                }
            }
            push(Role::Assistant, text, any_truncated, None);
        }
        _ => {}
    }
}

/// tool_result content is either a plain string or a list of parts.
fn tool_result_text(part: &Value) -> String {
    match part.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => p.get("text").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session() -> UnifiedSession {
        UnifiedSession {
            agent: "claude",
            source_session_id: "s1".to_string(),
            source_path: "/fake/s1.jsonl".to_string(),
            cwd: None,
            git_branch: None,
            title: None,
            ts_source: TsSource::Iso,
            is_subagent: false,
            parent_source_session_id: None,
        }
    }

    #[test]
    fn extract_record_turns_user_and_assistant_records_into_turns() {
        let mut session = session();
        let mut turns = Vec::new();
        let user = json!({
            "type": "user", "cwd": "/work/pixel", "gitBranch": "develop",
            "timestamp": "2025-10-09T08:53:20.000Z",
            "message": {"content": [
                {"type": "text", "text": "please fix the engine"},
                {"type": "tool_result", "content": "exit 0"}
            ]}
        });
        extract_record(&user, 10, 200, &mut session, &mut turns);
        let assistant = json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "text", "text": "on it"},
                {"type": "tool_use", "name": "Bash", "input": {"command": "cargo test"}},
                {"type": "thinking", "thinking": "hidden"}
            ]}
        });
        extract_record(&assistant, 210, 300, &mut session, &mut turns);
        extract_record(
            &json!({"type": "summary", "summary": "x"}),
            0,
            1,
            &mut session,
            &mut turns,
        );

        assert_eq!(session.cwd.as_deref(), Some("/work/pixel"));
        assert_eq!(session.git_branch.as_deref(), Some("develop"));
        let roles: Vec<Role> = turns.iter().map(|t| t.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Tool, Role::Assistant]);
        assert_eq!(turns[0].text, "please fix the engine");
        assert_eq!(turns[0].intent_source, Some(IntentSource::Human));
        assert_eq!(turns[0].ts, Some(1_760_000_000_000));
        assert_eq!(
            (turns[0].source_byte_start, turns[0].source_byte_len),
            (Some(10), Some(200))
        );
        assert_eq!(turns[1].text, "exit 0");
        assert_eq!(
            turns[2].text,
            "on it\n\u{22ee}tool Bash {\"command\":\"cargo test\"}"
        );
        assert!(!turns[2].truncated);
    }

    /// A complete line that is not valid JSON (a writer that crashed
    /// mid-flush) is consumed but counted: swallowing it would let a
    /// transcript whose records never parse look perfectly healthy.
    #[test]
    fn parse_counts_a_malformed_line_without_losing_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.jsonl");
        let record = json!({
            "type": "user", "cwd": "/work/pixel",
            "message": {"content": [{"type": "text", "text": "please fix the engine"}]}
        })
        .to_string();
        let truncated = "{\"type\":\"user\",\"message\":{\"conte";
        fs::write(&path, format!("{record}\n{truncated}\n{record}\n")).unwrap();

        let unit = unit_for(path).unwrap();
        let out = ClaudeAdapter::with_root(dir.path().to_path_buf())
            .parse(&unit, Change::New, None)
            .unwrap();

        assert_eq!(out.skipped_records, 1);
        assert_eq!(
            out.consumed_bytes, unit.size,
            "the malformed line's bytes are consumed, never re-read"
        );
        assert_eq!(out.sessions.len(), 1);
        assert_eq!(out.sessions[0].turns.len(), 2, "both readable records land");
    }
}

#[cfg(test)]
mod contract_tests;
