// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Pi agent adapter: `<agent dir>/sessions/<encoded-cwd>/*.jsonl`, where the
//! agent dir is `$PI_CODING_AGENT_DIR` or `~/.pi/agent`.
//! Records are flat `{type, timestamp, ...}` lines — `session` carries the
//! meta (id, cwd, and `parentSession` on a fork); `message` wraps
//! `{role, content}` where content is a string or `text` | `thinking` |
//! `toolCall` parts, and a `bashExecution` message is a `!command` the user
//! ran; `custom_message` carries extension injections (fanout digests) worth
//! indexing; `custom`/`model_change`/`thinking_level_change` are markers
//! with no recall value.
//!
//! Two layouts share the store:
//! - a fork is a new top-level file whose header names
//!   the file it branched from and which starts with a copy of that file's
//!   entries, all dated before its own header;
//! - a subagent run lives at `<cwd>/<parent stem>/<hash>/run-<n>/session.jsonl`.

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

pub struct Adapter {
    sessions_dir: PathBuf,
}

impl Adapter {
    pub fn new() -> Self {
        Self::with_root(machine_sessions_dir())
    }

    pub fn with_root(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }
}

/// Where pi writes sessions: `<agent_dir>/sessions` when pi's
/// `PI_CODING_AGENT_DIR` is set (a leading `~` is `home`, as pi expands it),
/// else `<home>/.pi/agent/sessions`.
pub fn sessions_dir(agent_dir: Option<&str>, home: &Path) -> PathBuf {
    let agent_dir = match agent_dir.filter(|dir| !dir.is_empty()) {
        None => home.join(".pi/agent"),
        Some("~") => home.to_path_buf(),
        Some(dir) => dir
            .strip_prefix("~/")
            .map_or_else(|| PathBuf::from(dir), |rest| home.join(rest)),
    };
    agent_dir.join("sessions")
}

/// [`sessions_dir`] for this process's environment.
#[cfg_attr(test, mutants::skip)] // one-line adapter over the environment; `sessions_dir` is tested
pub fn machine_sessions_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let agent_dir = std::env::var("PI_CODING_AGENT_DIR").ok();
    sessions_dir(agent_dir.as_deref(), Path::new(&home))
}

/// The session id a pi file stem carries: `<timestamp>_<id>` → `<id>`.
fn session_id_of_stem(stem: &str) -> &str {
    stem.split_once('_').map_or(stem, |(_, id)| id)
}

/// The parent session id of a subagent run, which pi stores at
/// `<cwd>/<parent stem>/<hash>/run-<n>/session.jsonl` under the sessions
/// directory; `None` for a top-level session file.
fn subagent_parent(sessions_dir: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(sessions_dir).ok()?;
    let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
    match parts.as_slice() {
        [_cwd, parent, _hash, run, "session.jsonl"] if run.starts_with("run-") => {
            Some(session_id_of_stem(parent).to_string())
        }
        _ => None,
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

fn collect(dir: &PathBuf, units: &mut Vec<SourceUnit>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, units);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            units.extend(unit_for(path));
        }
    }
}

impl SourceAdapter for Adapter {
    fn agent(&self) -> &'static str {
        "pi"
    }

    fn discover(&self) -> Result<Vec<SourceUnit>, IngestError> {
        let mut units = Vec::new();
        collect(&self.sessions_dir, &mut units);
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
        file.seek(SeekFrom::Start(start))?;
        let mut reader = BufReader::new(file);

        let stem = unit
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();

        let parent = subagent_parent(&self.sessions_dir, &unit.path);
        let mut session = UnifiedSession {
            agent: "pi",
            source_session_id: session_id_of_stem(&stem).to_string(),
            source_path: unit.path.to_string_lossy().to_string(),
            cwd: None,
            git_branch: None,
            title: None,
            ts_source: TsSource::Iso,
            is_subagent: parent.is_some(),
            parent_source_session_id: parent,
        };

        let mut turns: Vec<UnifiedTurn> = Vec::new();
        let mut head = Head::default();
        // On an appended resume the `session` record sits before the resume
        // offset — recover identity and the fork cutoff from the file head.
        if matches!(change, Change::Appended { .. }) {
            apply_head_meta(&unit.path, &mut session, &mut head);
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
                &mut head,
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

/// What the `session` header record decided for the records after it.
#[derive(Debug, Default)]
struct Head {
    /// The header was read; a second `session` record changes nothing.
    seen: bool,
    /// On a fork, the header's timestamp: the entries before it are the
    /// copy of the parent session, already indexed there.
    fork_cutoff_ms: Option<i64>,
}

fn apply_head_meta(path: &Path, session: &mut UnifiedSession, head: &mut Head) {
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
    extract_record(&record, 0, 0, session, &mut no_turns, head);
}

fn extract_record(
    record: &Value,
    byte_start: u64,
    byte_len: u64,
    session: &mut UnifiedSession,
    turns: &mut Vec<UnifiedTurn>,
    head: &mut Head,
) {
    let rtype = record.get("type").and_then(Value::as_str).unwrap_or("");
    let ts = record
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_iso_ms);
    if rtype != "session"
        && let (Some(cutoff), Some(ts)) = (head.fork_cutoff_ms, ts)
        && ts < cutoff
    {
        return;
    }

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
        "session" => {
            if head.seen {
                return;
            }
            head.seen = true;
            if let Some(id) = record.get("id").and_then(Value::as_str) {
                session.source_session_id = id.to_string();
            }
            if session.cwd.is_none() {
                session.cwd = record
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            if let Some(parent) = record.get("parentSession").and_then(Value::as_str) {
                if let Some(stem) = Path::new(parent).file_stem().and_then(|s| s.to_str()) {
                    session.parent_source_session_id = Some(session_id_of_stem(stem).to_string());
                }
                head.fork_cutoff_ms = ts;
            }
        }
        "message" => {
            let msg = record.get("message").unwrap_or(&Value::Null);
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
            match role {
                "user" => {
                    let text = content_text(msg.get("content"));
                    if text.trim().is_empty() {
                        return;
                    }
                    let intent = classify_user_text(&text);
                    push(Role::User, text, false, Some(intent));
                }
                "assistant" => {
                    let text = content_text(msg.get("content"));
                    push(Role::Assistant, text, false, None);
                    // thinking parts carry ANSI styling, no recall value.
                    if let Some(parts) = msg.get("content").and_then(Value::as_array) {
                        for part in parts {
                            if part.get("type").and_then(Value::as_str) == Some("toolCall") {
                                let name = part
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown");
                                let args = part
                                    .get("arguments")
                                    .map(|a| {
                                        if a.is_string() {
                                            a.as_str().unwrap_or_default().to_string()
                                        } else {
                                            a.to_string()
                                        }
                                    })
                                    .unwrap_or_default();
                                let (capped, truncated) = cap_text(&args, TOOL_INPUT_CAP);
                                push(
                                    Role::Assistant,
                                    format!("\u{22ee}tool {name} {capped}"),
                                    truncated,
                                    None,
                                );
                            }
                        }
                    }
                }
                "toolResult" => {
                    let text = content_text(msg.get("content"));
                    let (capped, truncated) = cap_text(&text, TOOL_RESULT_CAP);
                    push(Role::Tool, capped, truncated, None);
                }
                // A `!command` the user ran in the pi prompt: the command is
                // the user's, its output a tool result.
                "bashExecution" => {
                    let command = msg.get("command").and_then(Value::as_str).unwrap_or("");
                    if command.trim().is_empty() {
                        return;
                    }
                    push(
                        Role::User,
                        format!("!{command}"),
                        false,
                        Some(IntentSource::Human),
                    );
                    let output = msg.get("output").and_then(Value::as_str).unwrap_or("");
                    let exit = msg
                        .get("exitCode")
                        .and_then(Value::as_i64)
                        .map_or_else(|| "?".to_string(), |code| code.to_string());
                    let (capped, truncated) =
                        cap_text(&format!("exit {exit}\n{output}"), TOOL_RESULT_CAP);
                    push(Role::Tool, capped, truncated, None);
                }
                _ => {}
            }
        }
        // Extension-injected messages (fanout digests, wake events) — harness
        // text, classified as orchestrator so recall can filter it.
        "custom_message" => {
            let text = content_text(record.get("content"));
            let (capped, truncated) = cap_text(&text, TOOL_RESULT_CAP);
            push(
                Role::User,
                capped,
                truncated,
                Some(IntentSource::Orchestrator),
            );
        }
        // custom (steering markers), model_change, thinking_level_change,
        // compactions — no recall value.
        _ => {}
    }
}

/// The text of a pi `content` field: a plain string, or its `text` parts
/// joined by newlines (images, thinking and tool calls skipped).
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: &str = "2026-08-08T10:00:00.000Z";
    const T1: &str = "2026-08-08T10:05:00.000Z";
    const T2: &str = "2026-08-08T10:10:00.000Z";

    fn header(id: &str, ts: &str, parent: Option<&str>) -> String {
        let mut v = serde_json::json!({
            "type": "session", "version": 3, "id": id, "timestamp": ts, "cwd": "/work/app"
        });
        if let Some(p) = parent {
            v["parentSession"] = serde_json::json!(p);
        }
        v.to_string()
    }

    fn message(ts: &str, message: serde_json::Value) -> String {
        serde_json::json!({"type": "message", "id": "e1", "timestamp": ts, "message": message})
            .to_string()
    }

    fn write(dir: &Path, rel: &str, lines: &[String], partial: &str) -> SourceUnit {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        text.push_str(partial);
        fs::write(&path, text).unwrap();
        unit_for(path).unwrap()
    }

    fn texts(out: &ParseOutput) -> Vec<(Role, String)> {
        out.sessions[0]
            .turns
            .iter()
            .map(|t| (t.role, t.text.clone()))
            .collect()
    }

    /// Every record kind pi writes lands as the turn a reader expects:
    /// thinking stays out, a `!command` is the user's, extension messages are
    /// orchestrator text, and a line still being written is not consumed.
    #[test]
    fn parse_maps_every_record_kind_and_stops_before_a_partial_line() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![
            header("sess-1", T0, None),
            message(T0, serde_json::json!({"role": "user", "content": [{"type": "text", "text": "fix the login"}, {"type": "image", "data": "x"}]})),
            message(T0, serde_json::json!({"role": "user", "content": "plain string prompt"})),
            message(T1, serde_json::json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hidden"},
                {"type": "text", "text": "on it"},
                {"type": "toolCall", "name": "bash", "arguments": {"command": "cargo test"}}
            ]})),
            message(T1, serde_json::json!({"role": "toolResult", "content": [{"type": "text", "text": "ok 3 passed"}]})),
            message(T1, serde_json::json!({"role": "bashExecution", "command": "git status", "output": "clean", "exitCode": 0})),
            serde_json::json!({"type": "custom_message", "timestamp": T1, "content": "fanout digest"}).to_string(),
            serde_json::json!({"type": "custom_message", "timestamp": T1, "content": [{"type": "text", "text": "wake event"}]}).to_string(),
            serde_json::json!({"type": "model_change", "timestamp": T1, "model": "x"}).to_string(),
            "{not json".to_string(),
            header("sess-2", T2, None).replace("/work/app", "/elsewhere"),
        ];
        let unit = write(
            dir.path(),
            "--work-app--/2026-08-08T10-00-00-000Z_sess-1.jsonl",
            &lines,
            "{\"type\":\"message\"",
        );
        let adapter = Adapter::with_root(dir.path().to_path_buf());
        let out = adapter.parse(&unit, Change::New, None).unwrap();

        assert_eq!(out.sessions.len(), 1);
        let parsed = &out.sessions[0];
        assert_eq!(parsed.op, SessionOp::Replace);
        assert_eq!(
            parsed.session.source_session_id, "sess-1",
            "a second header changes nothing"
        );
        assert_eq!(parsed.session.cwd.as_deref(), Some("/work/app"));
        assert!(!parsed.session.is_subagent);
        assert_eq!(parsed.session.parent_source_session_id, None);
        assert_eq!(
            texts(&out),
            vec![
                (Role::User, "fix the login".to_string()),
                (Role::User, "plain string prompt".to_string()),
                (Role::Assistant, "on it".to_string()),
                (
                    Role::Assistant,
                    "\u{22ee}tool bash {\"command\":\"cargo test\"}".to_string()
                ),
                (Role::Tool, "ok 3 passed".to_string()),
                (Role::User, "!git status".to_string()),
                (Role::Tool, "exit 0\nclean".to_string()),
                (Role::User, "fanout digest".to_string()),
                (Role::User, "wake event".to_string()),
            ]
        );
        let intents: Vec<Option<IntentSource>> =
            parsed.turns.iter().map(|t| t.intent_source).collect();
        assert_eq!(intents[0], Some(IntentSource::Human));
        assert_eq!(intents[5], Some(IntentSource::Human));
        assert_eq!(intents[7], Some(IntentSource::Orchestrator));
        assert_eq!(intents[8], Some(IntentSource::Orchestrator));
        assert_eq!(parsed.turns[2].ts, parse_iso_ms(T1));
        let complete: u64 = lines.iter().map(|l| l.len() as u64 + 1).sum();
        assert_eq!(
            out.consumed_bytes, complete,
            "the partial last line is left for the next pass"
        );
        assert_eq!(
            out.skipped_records, 1,
            "the `{{not json` line is consumed and counted, not swallowed"
        );
        assert_eq!(
            parsed.turns[1].source_byte_start,
            Some(lines[0].len() as u64 + lines[1].len() as u64 + 2)
        );
    }

    /// A fork starts with a copy of its parent's entries (older than its own
    /// header). They are indexed once, in the parent; the fork keeps only
    /// what happened after it branched, and names its parent.
    #[test]
    fn a_fork_skips_the_copied_parent_entries_and_names_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let parent_path = dir
            .path()
            .join("--work-app--/2026-08-08T09-00-00-000Z_parent-id.jsonl");
        let lines = vec![
            header("fork-id", T1, Some(parent_path.to_str().unwrap())),
            message(
                T0,
                serde_json::json!({"role": "user", "content": "copied from the parent"}),
            ),
            message(
                T1,
                serde_json::json!({"role": "user", "content": "asked at the fork"}),
            ),
            message(
                T2,
                serde_json::json!({"role": "assistant", "content": "answered in the fork"}),
            ),
        ];
        let unit = write(
            dir.path(),
            "--work-app--/2026-08-08T10-05-00-000Z_fork-id.jsonl",
            &lines,
            "",
        );
        let adapter = Adapter::with_root(dir.path().to_path_buf());
        let out = adapter.parse(&unit, Change::New, None).unwrap();
        let session = &out.sessions[0].session;
        assert_eq!(session.source_session_id, "fork-id");
        assert_eq!(
            session.parent_source_session_id.as_deref(),
            Some("parent-id")
        );
        assert!(!session.is_subagent, "a fork is a session of its own");
        assert_eq!(
            texts(&out),
            vec![
                (Role::User, "asked at the fork".to_string()),
                (Role::Assistant, "answered in the fork".to_string()),
            ]
        );

        // Resuming after the header recovers the cutoff from the file head.
        let head_len = lines[0].len() as u64 + 1;
        let resumed = adapter
            .parse(&unit, Change::Appended { from: head_len }, None)
            .unwrap();
        assert_eq!(resumed.sessions[0].op, SessionOp::Append);
        assert_eq!(texts(&resumed).len(), 2, "{:?}", texts(&resumed));
        assert_eq!(
            resumed.sessions[0]
                .session
                .parent_source_session_id
                .as_deref(),
            Some("parent-id")
        );
    }

    /// Every subagent run is a file named `session.jsonl`: its identity and
    /// its parent come from the header and the path, never from the stem.
    #[test]
    fn a_subagent_run_is_marked_and_linked_even_on_an_appended_pass() {
        let dir = tempfile::tempdir().unwrap();
        let lines = vec![
            header("child-id", T0, None),
            message(
                T0,
                serde_json::json!({"role": "user", "content": "Task: audit the parser"}),
            ),
            message(
                T1,
                serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]}),
            ),
        ];
        let rel = "--work-app--/2026-08-08T09-00-00-000Z_parent-id/abc123/run-0/session.jsonl";
        let unit = write(dir.path(), rel, &lines, "");
        let adapter = Adapter::with_root(dir.path().to_path_buf());
        let from = lines[0].len() as u64 + lines[1].len() as u64 + 2;
        let out = adapter
            .parse(&unit, Change::Appended { from }, None)
            .unwrap();
        let session = &out.sessions[0].session;
        assert_eq!(session.source_session_id, "child-id");
        assert!(session.is_subagent);
        assert_eq!(
            session.parent_source_session_id.as_deref(),
            Some("parent-id")
        );
        assert_eq!(session.cwd.as_deref(), Some("/work/app"));
        assert_eq!(texts(&out), vec![(Role::Assistant, "done".to_string())]);
    }

    #[test]
    fn subagent_parent_reads_only_the_run_layout_under_the_sessions_dir() {
        let root = Path::new("/s");
        let parent = |p: &str| subagent_parent(root, Path::new(p));
        assert_eq!(
            parent("/s/cwd/2026_pid/h/run-3/session.jsonl").as_deref(),
            Some("pid")
        );
        assert_eq!(parent("/s/cwd/2026_pid.jsonl"), None, "top-level session");
        assert_eq!(parent("/s/cwd/2026_pid/h/tmp-3/session.jsonl"), None);
        assert_eq!(parent("/s/cwd/2026_pid/h/run-3/other.jsonl"), None);
        assert_eq!(parent("/other/cwd/2026_pid/h/run-3/session.jsonl"), None);
        assert_eq!(
            session_id_of_stem("2026-08-08T10-00-00-000Z_0198-ab_c"),
            "0198-ab_c"
        );
        assert_eq!(session_id_of_stem("session"), "session");
    }

    #[test]
    fn sessions_dir_follows_pi_coding_agent_dir() {
        let home = Path::new("/home/u");
        assert_eq!(
            sessions_dir(None, home),
            Path::new("/home/u/.pi/agent/sessions")
        );
        assert_eq!(
            sessions_dir(Some(""), home),
            Path::new("/home/u/.pi/agent/sessions")
        );
        assert_eq!(
            sessions_dir(Some("/opt/pi"), home),
            Path::new("/opt/pi/sessions")
        );
        assert_eq!(
            sessions_dir(Some("~/alt/pi"), home),
            Path::new("/home/u/alt/pi/sessions")
        );
        assert_eq!(sessions_dir(Some("~"), home), Path::new("/home/u/sessions"));
    }

    #[test]
    fn discover_walks_nested_jsonl_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let line = vec![header("a", T0, None)];
        write(dir.path(), "cwd/a.jsonl", &line, "");
        write(dir.path(), "cwd/p_a/h/run-0/session.jsonl", &line, "");
        write(dir.path(), "cwd/notes.txt", &line, "");
        let adapter = Adapter::with_root(dir.path().to_path_buf());
        assert_eq!(adapter.agent(), "pi");
        let mut found: Vec<String> = adapter
            .discover()
            .unwrap()
            .into_iter()
            .map(|u| {
                u.path
                    .strip_prefix(dir.path())
                    .unwrap()
                    .display()
                    .to_string()
            })
            .collect();
        found.sort();
        assert_eq!(found, ["cwd/a.jsonl", "cwd/p_a/h/run-0/session.jsonl"]);
        let missing = Adapter::with_root(dir.path().join("absent"));
        assert!(missing.discover().unwrap().is_empty());
    }

    /// A file with neither a header nor turns is not a session; an appended
    /// pass always reports its session so the store can extend it.
    #[test]
    fn only_files_with_a_conversation_become_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = Adapter::with_root(dir.path().to_path_buf());
        let marker = vec![serde_json::json!({"type": "model_change", "timestamp": T0}).to_string()];
        let empty = write(dir.path(), "cwd/x_empty.jsonl", &marker, "");
        assert!(
            adapter
                .parse(&empty, Change::New, None)
                .unwrap()
                .sessions
                .is_empty()
        );
        let appended = adapter
            .parse(&empty, Change::Appended { from: 1 }, None)
            .unwrap();
        assert_eq!(appended.sessions.len(), 1);

        let header_only = write(dir.path(), "cwd/x_head.jsonl", &[header("h", T0, None)], "");
        assert_eq!(
            adapter
                .parse(&header_only, Change::New, None)
                .unwrap()
                .sessions
                .len(),
            1
        );

        let no_cwd = vec![message(
            T0,
            serde_json::json!({"role": "user", "content": "hi"}),
        )];
        let turns_only = write(dir.path(), "cwd/x_turns.jsonl", &no_cwd, "");
        assert_eq!(
            adapter
                .parse(&turns_only, Change::New, None)
                .unwrap()
                .sessions
                .len(),
            1
        );
    }

    /// pi rewrites a session file in place on a migration or a branch: the
    /// tail hash must then refuse an append resume.
    #[test]
    fn append_valid_refuses_a_rewritten_file() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = Adapter::with_root(dir.path().to_path_buf());
        let lines = vec![
            header("a", T0, None),
            message(T0, serde_json::json!({"role": "user", "content": "one"})),
        ];
        let unit = write(dir.path(), "cwd/x_a.jsonl", &lines, "");
        let consumed = fs::metadata(&unit.path).unwrap().len();
        let state = |cursor: Option<String>| IngestState {
            file_size: consumed as i64,
            mtime_ms: 0,
            bytes_ingested: consumed as i64,
            cursor,
        };
        let cursor = adapter.make_cursor(&unit, consumed);
        assert!(cursor.is_some());
        assert!(adapter.append_valid(&unit, &state(cursor.clone())));
        assert!(
            adapter.append_valid(&unit, &state(None)),
            "no cursor recorded yet"
        );
        let rewritten = vec![
            header("a", T0, None),
            message(T0, serde_json::json!({"role": "user", "content": "two"})),
        ];
        let unit = write(dir.path(), "cwd/x_a.jsonl", &rewritten, "");
        assert!(!adapter.append_valid(&unit, &state(cursor)));
    }

    #[test]
    fn content_text_reads_strings_and_text_parts() {
        use serde_json::json;
        assert_eq!(content_text(Some(&json!("plain"))), "plain");
        assert_eq!(
            content_text(Some(
                &json!([{"type": "text", "text": "a"}, {"type": "image"}, {"type": "text", "text": "b"}, {"type": "thinking", "text": "c"}])
            )),
            "a\nb"
        );
        assert_eq!(content_text(Some(&json!(42))), "");
        assert_eq!(content_text(None), "");
    }
}
