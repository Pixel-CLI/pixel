// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Cursor CLI adapter:
//! `~/.cursor/projects/<slug>/agent-transcripts/<uuid>/<uuid>.jsonl`.
//! Records carry no timestamps at all — every turn gets the file's mtime
//! and the session discloses `TsSource::Mtime`. User text arrives wrapped
//! in `<timestamp>…</timestamp>\n<user_query>…</user_query>`; only the
//! inner query is indexed.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

use serde_json::Value;

use crate::intent::classify_user_text;
use crate::model::{
    IntentSource, Role, TOOL_INPUT_CAP, TsSource, UnifiedSession, UnifiedTurn, cap_text,
};
use crate::sources::{
    Change, IngestError, ParseOutput, ParsedSession, SessionOp, SourceAdapter, SourceUnit,
    file_tail_hash,
};
use crate::store::IngestState;

/// Cursor-specific harness wrappers injected as "user" messages.
const CURSOR_ORCHESTRATOR_PREFIXES: &[&str] = &["<hooks_context", "<additional_data"];

pub struct Adapter {
    projects_dir: PathBuf,
}

impl Adapter {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            projects_dir: PathBuf::from(home).join(".cursor/projects"),
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

impl SourceAdapter for Adapter {
    fn agent(&self) -> &'static str {
        "cursor"
    }

    fn discover(&self) -> Result<Vec<SourceUnit>, IngestError> {
        let mut units = Vec::new();
        let projects = match fs::read_dir(&self.projects_dir) {
            Ok(rd) => rd,
            Err(_) => return Ok(units), // no Cursor store on this machine
        };
        for project in projects.flatten() {
            let transcripts = project.path().join("agent-transcripts");
            let Ok(convs) = fs::read_dir(&transcripts) else {
                continue;
            };
            for conv in convs.flatten() {
                let cpath = conv.path();
                if !cpath.is_dir() {
                    continue;
                }
                let Ok(files) = fs::read_dir(&cpath) else {
                    continue;
                };
                for f in files.flatten() {
                    let fpath = f.path();
                    if fpath.extension().is_some_and(|e| e == "jsonl") {
                        units.extend(unit_for(fpath));
                    } else if fpath.is_dir() && fpath.file_name().is_some_and(|n| n == "subagents")
                    {
                        // <conv-uuid>/subagents/<uuid>.jsonl
                        let Ok(subs) = fs::read_dir(&fpath) else {
                            continue;
                        };
                        for s in subs.flatten() {
                            let spath = s.path();
                            if spath.extension().is_some_and(|e| e == "jsonl") {
                                units.extend(unit_for(spath));
                            }
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

        let session = UnifiedSession {
            agent: "cursor",
            source_session_id,
            source_path: unit.path.to_string_lossy().to_string(),
            cwd: None, // the project slug is lossy; source_path carries it
            git_branch: None,
            title: None,
            ts_source: TsSource::Mtime,
            is_subagent,
            parent_source_session_id: parent,
        };

        let ts = Some(unit.mtime_ms);
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
            extract_record(&record, line_start, n as u64, ts, &mut turns);
        }

        let has_conversation = !turns.is_empty() || matches!(change, Change::Appended { .. });
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

/// `.../agent-transcripts/<parent-uuid>/subagents/x.jsonl` → parent uuid.
fn subagent_parent(path: &std::path::Path) -> (bool, Option<String>) {
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
    ts: Option<i64>,
    turns: &mut Vec<UnifiedTurn>,
) {
    let role = record.get("role").and_then(Value::as_str).unwrap_or("");
    if role != "user" && role != "assistant" {
        return;
    }
    let content = record.get("message").and_then(|m| m.get("content"));

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

    match role {
        "user" => {
            let mut raw = String::new();
            match content {
                Some(Value::String(s)) => raw.push_str(s),
                Some(Value::Array(parts)) => {
                    for part in parts {
                        if part.get("type").and_then(Value::as_str) == Some("text")
                            && let Some(t) = part.get("text").and_then(Value::as_str)
                        {
                            if !raw.is_empty() {
                                raw.push('\n');
                            }
                            raw.push_str(t);
                        }
                    }
                }
                _ => {}
            }
            let text = extract_user_query(&raw);
            if text.trim().is_empty() {
                return;
            }
            let intent = classify_cursor_user(text);
            push(Role::User, text.to_string(), false, Some(intent));
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
                        _ => {}
                    }
                }
            }
            push(Role::Assistant, text, any_truncated, None);
        }
        _ => {}
    }
}

/// Strip the `<timestamp>…</timestamp>\n<user_query>…</user_query>` wrapper,
/// returning the inner query; absent the wrapper, the raw text.
fn extract_user_query(raw: &str) -> &str {
    let Some(open) = raw.find("<user_query>") else {
        return raw;
    };
    let inner = &raw[open + "<user_query>".len()..];
    let inner = match inner.find("</user_query>") {
        Some(close) => &inner[..close],
        None => inner,
    };
    inner.trim_matches('\n')
}

fn classify_cursor_user(text: &str) -> IntentSource {
    let trimmed = text.trim_start();
    for p in CURSOR_ORCHESTRATOR_PREFIXES {
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

    #[test]
    fn extract_record_reads_the_user_query_and_folds_assistant_tool_calls() {
        let mut turns = Vec::new();
        let user = json!({
            "role": "user",
            "message": {"content": [
                {"type": "text", "text": "<user_query>\nrename the flag\n</user_query>"}
            ]}
        });
        extract_record(&user, 0, 50, Some(1_760_000_000_000), &mut turns);
        let assistant = json!({
            "role": "assistant",
            "message": {"content": [
                {"type": "text", "text": "renaming"},
                {"type": "tool_use", "name": "edit_file", "input": {"path": "a.rs"}}
            ]}
        });
        extract_record(&assistant, 50, 80, None, &mut turns);
        extract_record(
            &json!({"role": "system", "message": {"content": "x"}}),
            0,
            1,
            None,
            &mut turns,
        );

        let roles: Vec<Role> = turns.iter().map(|t| t.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant]);
        assert_eq!(turns[0].text, "rename the flag");
        assert_eq!(turns[0].intent_source, Some(IntentSource::Human));
        assert_eq!(turns[0].ts, Some(1_760_000_000_000));
        assert_eq!(
            (turns[0].source_byte_start, turns[0].source_byte_len),
            (Some(0), Some(50))
        );
        assert_eq!(
            turns[1].text,
            "renaming\n\u{22ee}tool edit_file {\"path\":\"a.rs\"}"
        );
        assert_eq!(turns[1].ts, None);
    }

    /// A complete line that is not valid JSON (a writer that crashed
    /// mid-flush) is consumed but counted: swallowing it would let a
    /// transcript whose records never parse look perfectly healthy.
    #[test]
    fn parse_counts_a_malformed_line_without_losing_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let conv = dir.path().join("conv-uuid");
        fs::create_dir_all(&conv).unwrap();
        let path = conv.join("0123abcd-0000-4000-8000-000000000001.jsonl");
        let record = json!({
            "role": "user",
            "message": {"content": [
                {"type": "text", "text": "<user_query>\nrename the flag\n</user_query>"}
            ]}
        })
        .to_string();
        let truncated = "{\"role\":\"user\",\"message\":{\"conte";
        fs::write(&path, format!("{record}\n{truncated}\n{record}\n")).unwrap();

        let unit = unit_for(path).unwrap();
        let out = Adapter::new()
            .parse(&unit, Change::New, None)
            .expect("parse");

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
