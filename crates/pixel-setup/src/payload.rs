// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The fields PIXEL reads from a host's hook payload, under the names each host
//! gives them. Nothing here keeps a tool's output or a user's shell text: a
//! payload is reduced to what a decision and a journal record may carry.

use serde_json::Value;

use crate::hosts::Host;

/// Keys a host may carry its session id under, first match wins: Claude, Codex
/// and Devin say `session_id`, pi and OpenCode `sessionId`, Antigravity
/// `conversationId`.
const SESSION_KEYS: [&str; 3] = ["session_id", "sessionId", "conversationId"];

/// A tool name longer than this, or with a character outside
/// `[A-Za-z0-9_\-.:/]`, is not recorded.
const TOOL_LABEL_CHARS: usize = 256;

/// What a hook payload says, reduced.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Payload {
    /// The host's session id, as the host spells it.
    pub session: Option<String>,
    /// Directories the host says the work is in, best first: `cwd`, then the
    /// workspace paths of a host that reports several.
    pub dirs: Vec<String>,
    /// The typed prompt of a prompt-submit. Antigravity carries none: it
    /// names a transcript instead.
    pub prompt: Option<String>,
    /// The transcript file the host points at.
    pub transcript: Option<String>,
    /// `SessionStart`'s `source`: `startup`, `resume`, `clear` or `compact`.
    pub source: Option<String>,
    /// The tool of a tool event, when its name is a plain label.
    pub tool: Option<String>,
    /// The tool's input, `null` when the host sent none.
    pub tool_input: Value,
    /// The host says this stop is a continuation a stop hook caused.
    pub stop_active: bool,
    /// False for an Antigravity invocation that is not the turn's first: its
    /// prompt was briefed already.
    pub first_invocation: bool,
}

/// The text under the first of `keys` in `value` that holds a non-empty string.
fn string<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key)?.as_str())
        .filter(|text| !text.is_empty())
}

fn tool_label(name: &str) -> Option<&str> {
    (!name.is_empty()
        && name.len() <= TOOL_LABEL_CHARS
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:/".contains(&byte)))
    .then_some(name)
}

/// Reduces `payload`, as `host` sent it.
pub fn normalize(host: Host, payload: &Value) -> Payload {
    let (tool, tool_input) = tool_call(host, payload);
    let mut dirs: Vec<String> = string(payload, &["cwd"])
        .map(str::to_owned)
        .into_iter()
        .collect();
    if host == Host::Antigravity {
        dirs.extend(
            payload["workspacePaths"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|path| !path.is_empty())
                .map(str::to_owned),
        );
    }
    Payload {
        session: string(payload, &SESSION_KEYS).map(str::to_owned),
        dirs,
        prompt: string(payload, &["prompt"]).map(str::to_owned),
        transcript: string(payload, &["transcript_path", "transcriptPath"]).map(str::to_owned),
        source: string(payload, &["source"]).map(str::to_owned),
        tool,
        tool_input,
        stop_active: ["stop_hook_active", "stopHookActive"]
            .iter()
            .any(|key| payload.get(*key).and_then(Value::as_bool) == Some(true)),
        first_invocation: host != Host::Antigravity
            || payload.get("invocationNum").and_then(Value::as_u64) == Some(0),
    }
}

/// The tool of a tool event and its input, under the names `host` gives them.
/// Antigravity nests them in `toolCall` (`name`, `args`) and names the shell
/// command `CommandLine`; both are put in the shared shape here.
fn tool_call(host: Host, payload: &Value) -> (Option<String>, Value) {
    if host == Host::Antigravity
        && let Some(call) = payload.get("toolCall")
    {
        let mut input = call.get("args").cloned().unwrap_or(Value::Null);
        if input.get("command").is_none()
            && let Some(command) = input.get("CommandLine").cloned()
        {
            input["command"] = command;
        }
        let tool = string(call, &["name"])
            .and_then(tool_label)
            .map(str::to_owned);
        return (tool, input);
    }
    (
        string(payload, &["tool_name", "toolName"])
            .and_then(tool_label)
            .map(str::to_owned),
        payload
            .get("tool_input")
            .or_else(|| payload.get("input"))
            .cloned()
            .unwrap_or(Value::Null),
    )
}

/// The latest user turn in a transcript (JSONL). A turn is a user-role entry
/// or an explicit Antigravity `USER_INPUT`, with its text in `content`/`text`
/// or a text-parts array. Tolerant on purpose: a transcript shape this cannot
/// parse yields no brief, never a hook failure.
pub fn last_user_request(transcript: &str) -> Option<String> {
    let mut last = None;
    for line in transcript.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let role = entry["role"].as_str().or_else(|| entry["type"].as_str());
        let explicit = entry["type"].as_str() == Some("USER_INPUT")
            && entry["source"].as_str() == Some("USER_EXPLICIT");
        if !explicit && role != Some("user") && role != Some("human") {
            continue;
        }
        let text = entry["content"]
            .as_str()
            .or_else(|| entry["text"].as_str())
            .map(str::to_owned)
            .or_else(|| {
                entry["content"].as_array().map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
            });
        if let Some(text) = text
            .map(|text| user_request(&text).to_owned())
            .filter(|text| !text.trim().is_empty())
        {
            last = Some(text);
        }
    }
    last
}

/// What the user typed in an Antigravity turn. Antigravity CLI 1.4 stores a
/// turn as `<USER_REQUEST>…</USER_REQUEST>`, then its own
/// `<ADDITIONAL_METADATA>` (the local time) and, after a model switch, a
/// `<USER_SETTINGS_CHANGE>` block; briefing the whole text looked up
/// `USER_REQUEST`, `Gemini` and the time as symbols. Text without the wrapper
/// is returned as is.
fn user_request(text: &str) -> &str {
    text.trim_start()
        .strip_prefix("<USER_REQUEST>")
        .and_then(|rest| rest.split_once("</USER_REQUEST>"))
        .map_or(text, |(request, _)| request.trim())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_claude_prompt_payload_is_read_by_its_own_names() {
        let payload = json!({
            "hook_event_name": "UserPromptSubmit", "session_id": "s1",
            "cwd": "/repo", "prompt": "who calls run", "transcript_path": "/t.jsonl",
        });
        assert_eq!(
            normalize(Host::Claude, &payload),
            Payload {
                session: Some("s1".into()),
                dirs: vec!["/repo".into()],
                prompt: Some("who calls run".into()),
                transcript: Some("/t.jsonl".into()),
                source: None,
                tool: None,
                tool_input: Value::Null,
                stop_active: false,
                first_invocation: true,
            }
        );
    }

    #[test]
    fn the_session_is_found_under_the_key_each_host_uses() {
        for (key, host) in [
            ("session_id", Host::Codex),
            ("sessionId", Host::Pi),
            ("conversationId", Host::Antigravity),
        ] {
            let payload = json!({ key: "abc" });
            assert_eq!(normalize(host, &payload).session.as_deref(), Some("abc"));
        }
        assert_eq!(
            normalize(Host::Claude, &json!({"session_id": ""})).session,
            None
        );
    }

    #[test]
    fn antigravity_reports_several_workspaces_and_numbers_its_invocations() {
        let payload = json!({
            "cwd": "/a", "workspacePaths": ["/b", "", "/c"], "invocationNum": 0,
        });
        let first = normalize(Host::Antigravity, &payload);
        assert_eq!(first.dirs, ["/a", "/b", "/c"]);
        assert!(first.first_invocation);
        let later = normalize(Host::Antigravity, &json!({"invocationNum": 2}));
        assert!(!later.first_invocation);
        assert!(!normalize(Host::Antigravity, &json!({})).first_invocation);
        // Another host's workspacePaths are not read.
        assert_eq!(normalize(Host::Claude, &payload).dirs, ["/a"]);
        assert!(normalize(Host::Claude, &json!({"invocationNum": 3})).first_invocation);
    }

    #[test]
    fn a_tool_event_keeps_a_plain_tool_label_and_its_input() {
        let payload = json!({
            "tool_name": "Edit", "tool_input": {"file_path": "src/a.rs"},
        });
        let read = normalize(Host::Claude, &payload);
        assert_eq!(read.tool.as_deref(), Some("Edit"));
        assert_eq!(read.tool_input["file_path"], "src/a.rs");
        let pi = normalize(
            Host::Pi,
            &json!({"toolName": "bash", "input": {"command": "ls"}}),
        );
        assert_eq!(pi.tool.as_deref(), Some("bash"));
        assert_eq!(pi.tool_input["command"], "ls");
        for name in ["", "rm -rf /", "a\nb", &"x".repeat(257)] {
            assert_eq!(
                normalize(Host::Claude, &json!({"tool_name": name})).tool,
                None,
                "{name:?}"
            );
        }
        assert_eq!(
            normalize(Host::Claude, &json!({"tool_name": "mcp__pixel__impact"}))
                .tool
                .as_deref(),
            Some("mcp__pixel__impact")
        );
    }

    #[test]
    fn an_antigravity_tool_call_is_put_in_the_shared_shape() {
        let payload = json!({"toolCall": {"name": "run_command", "args": {"CommandLine": "pixel impact run", "Cwd": "/r"}}});
        let read = normalize(Host::Antigravity, &payload);
        assert_eq!(read.tool.as_deref(), Some("run_command"));
        assert_eq!(read.tool_input["command"], "pixel impact run");
        assert_eq!(read.tool_input["Cwd"], "/r");
        // Another host's `toolCall` key means nothing.
        assert_eq!(normalize(Host::Claude, &payload).tool, None);
    }

    #[test]
    fn a_stop_continuation_is_read_under_either_spelling() {
        assert!(normalize(Host::Claude, &json!({"stop_hook_active": true})).stop_active);
        assert!(normalize(Host::Gemini, &json!({"stopHookActive": true})).stop_active);
        assert!(!normalize(Host::Claude, &json!({"stop_hook_active": false})).stop_active);
        assert!(!normalize(Host::Claude, &json!({"stop_hook_active": "true"})).stop_active);
    }

    #[test]
    fn the_last_user_turn_of_a_transcript_is_the_request() {
        let transcript = [
            r#"{"role":"user","content":"first"}"#,
            r#"{"role":"assistant","content":"ok"}"#,
            "not json",
            r#"{"type":"USER_INPUT","source":"USER_EXPLICIT","content":"<USER_REQUEST>\nwho calls run?\n</USER_REQUEST>\n<ADDITIONAL_METADATA>time</ADDITIONAL_METADATA>"}"#,
            r#"{"type":"USER_INPUT","source":"SYSTEM","content":"ignored"}"#,
        ]
        .join("\n");
        assert_eq!(
            last_user_request(&transcript).as_deref(),
            Some("who calls run?")
        );
    }

    #[test]
    fn a_turn_in_a_parts_array_is_joined_and_a_blank_one_is_skipped() {
        let transcript = [
            r#"{"role":"user","content":[{"text":"a"},{"type":"image"},{"text":"b"}]}"#,
            r#"{"role":"user","content":"   "}"#,
        ]
        .join("\n");
        assert_eq!(last_user_request(&transcript).as_deref(), Some("a\nb"));
        assert_eq!(last_user_request(""), None);
        assert_eq!(
            last_user_request(r#"{"role":"assistant","content":"x"}"#),
            None
        );
    }
}
