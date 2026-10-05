// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Gemini (Antigravity CLI) adapter: the single prompt-history file at
//! `~/.gemini/antigravity-cli/history.jsonl`. Each line is one user prompt:
//! `{display, timestamp: unix ms, workspace, conversationId}`. Lines group
//! by `conversationId` into sessions — an appended tail may extend existing
//! conversations, so appended parses emit `SessionOp::Append` (the store
//! inserts the session when missing).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

use serde_json::Value;

use crate::intent::classify_user_text;
use crate::model::{Role, TsSource, UnifiedSession, UnifiedTurn};
use crate::sources::{
    Change, IngestError, ParseOutput, ParsedSession, SessionOp, SourceAdapter, SourceUnit,
    file_tail_hash,
};
use crate::store::IngestState;

/// Group key for the rare lines missing a `conversationId`.
const NO_CONVERSATION: &str = "no-conversation-id";

pub struct Adapter {
    history_path: PathBuf,
}

impl Adapter {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            history_path: PathBuf::from(home).join(".gemini/antigravity-cli/history.jsonl"),
        }
    }
}

impl Default for Adapter {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceAdapter for Adapter {
    fn agent(&self) -> &'static str {
        "gemini"
    }

    fn discover(&self) -> Result<Vec<SourceUnit>, IngestError> {
        let Ok(meta) = fs::metadata(&self.history_path) else {
            return Ok(Vec::new()); // no Gemini store on this machine
        };
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as i64);
        Ok(vec![SourceUnit {
            unit_key: self.history_path.to_string_lossy().to_string(),
            path: self.history_path.clone(),
            size: meta.len(),
            mtime_ms,
        }])
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

        let op = match change {
            Change::Appended { .. } => SessionOp::Append,
            _ => SessionOp::Replace,
        };

        // conversationId → index into `sessions`, preserving first-seen order.
        let mut by_conv: HashMap<String, usize> = HashMap::new();
        let mut sessions: Vec<ParsedSession> = Vec::new();

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
            let Some(display) = record.get("display").and_then(Value::as_str) else {
                continue;
            };
            if display.trim().is_empty() {
                continue;
            }
            let conv_id = record
                .get("conversationId")
                .and_then(Value::as_str)
                .unwrap_or(NO_CONVERSATION)
                .to_string();
            let workspace = record
                .get("workspace")
                .and_then(Value::as_str)
                .map(str::to_string);
            let ts = record.get("timestamp").and_then(Value::as_i64);

            let idx = *by_conv.entry(conv_id.clone()).or_insert_with(|| {
                sessions.push(ParsedSession {
                    op,
                    session: UnifiedSession {
                        agent: "gemini",
                        source_session_id: conv_id,
                        source_path: unit.path.to_string_lossy().to_string(),
                        cwd: None,
                        git_branch: None,
                        title: None,
                        ts_source: TsSource::UnixMs,
                        is_subagent: false,
                        parent_source_session_id: None,
                    },
                    turns: Vec::new(),
                });
                sessions.len() - 1
            });
            let parsed = &mut sessions[idx];
            if parsed.session.cwd.is_none() {
                parsed.session.cwd = workspace;
            }
            let intent = classify_user_text(display);
            parsed.turns.push(UnifiedTurn {
                role: Role::User,
                intent_source: Some(intent),
                ts,
                text: display.to_string(),
                truncated: false,
                source_byte_start: Some(line_start),
                source_byte_len: Some(n as u64),
            });
        }

        Ok(ParseOutput {
            sessions,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_lists_the_history_file_when_it_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.jsonl");
        let adapter = Adapter {
            history_path: path.clone(),
        };
        assert!(adapter.discover().unwrap().is_empty());
        std::fs::write(&path, b"{\"a\":1}\n").unwrap();
        let units = adapter.discover().unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].path, path);
        assert_eq!(units[0].unit_key, path.to_string_lossy());
        assert_eq!(units[0].size, 8);
        assert!(units[0].mtime_ms > 0);
    }
}

#[cfg(test)]
mod contract_tests;
