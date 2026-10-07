// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Read and reset access to the Claude task packets in
//! `.pixel/task-runtime.json`.
//!
//! The prompt hook that wrote this store is retired, so nothing creates or
//! refreshes a packet any more; `pixel task-state` still shows and clears the
//! packets an older release left. Corrupt or unavailable state is treated as
//! absent.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

const STORE_VERSION: u8 = 1;
const TTL_SECS: u64 = 86_400; // 24 h
const MAX_SESSION_ID_BYTES: usize = 128;
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct TaskTarget {
    pub(crate) path: String,
    pub(crate) tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) evidence: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct EvidenceSnapshot {
    pub(crate) revision: u64,
    pub(crate) reason: String,
    pub(crate) created_unix: u64,
    pub(crate) head_oid: String,
    pub(crate) targets: Vec<TaskTarget>,
    pub(crate) impact: String,
}

/// A task-intent verdict from the local classifier: a model claim about the
/// prompt, kept apart from the packet's repository facts and rendered as such.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(crate) struct Intent {
    pub(crate) label: String,
    pub(crate) p: f64,
    pub(crate) model: String,
}

/// The factual packet injected into a Claude Code session.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(crate) struct Packet {
    pub(crate) version: u8,
    pub(crate) task_id: String,
    pub(crate) session_id: String,
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    pub(crate) task: String,
    pub(crate) head_oid: String,
    pub(crate) created_unix: u64,
    pub(crate) updated_unix: u64,
    pub(crate) evidence: EvidenceSnapshot,
    /// The classifier's verdict on this prompt, when the local daemon was
    /// warm. Absent in stores written before it existed, which still load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) intent: Option<Intent>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Store {
    version: u8,
    sessions: Vec<Packet>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            sessions: Vec::new(),
        }
    }
}

/// Read a session's active packet only when it still refers to the supplied
/// HEAD and has not aged out. This is intentionally read-only for hooks.
pub(crate) fn read_claude_packet(
    root: &Path,
    session_id: &str,
    head: &str,
    now: u64,
) -> Option<Packet> {
    if !valid_session_id(session_id) {
        return None;
    }
    let store = load_store(&store_path(root));
    store.sessions.into_iter().find(|packet| {
        packet.session_id == session_id
            && packet.head_oid == head
            && !expired(packet.updated_unix, now)
    })
}

/// JSON inspection entry point for the CLI. Invalid/corrupt stores read as
/// empty; filesystem errors while locating a root remain actionable to CLI.
pub(crate) fn show(path: &Path, session: &str) -> Result<Option<Value>, String> {
    let root = crate::discover_root(path)?;
    let head = current_head(&root);
    let packet = read_claude_packet(&root, session, &head, now_unix());
    packet
        .map(|entry| serde_json::to_value(entry).map_err(|e| e.to_string()))
        .transpose()
}

/// Remove exactly one Claude session packet. Unlike hook operations, CLI
/// callers receive an error when a requested state mutation cannot publish.
pub(crate) fn reset(path: &Path, session: &str) -> Result<bool, String> {
    if !valid_session_id(session) {
        return Err("invalid Claude session id".to_string());
    }
    let root = crate::discover_root(path)?;
    let state_path = store_path(&root);
    let mut store = load_store(&state_path);
    let before = store.sessions.len();
    store.sessions.retain(|packet| packet.session_id != session);
    if store.sessions.len() == before {
        return Ok(false);
    }
    save_store(&state_path, &store)?;
    Ok(true)
}

fn store_path(root: &Path) -> PathBuf {
    root.join(".pixel").join("task-runtime.json")
}

fn load_store(path: &Path) -> Store {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Store>(&raw).ok())
        .filter(|store| store.version == STORE_VERSION)
        .unwrap_or_default()
}

fn save_store(path: &Path, store: &Store) -> Result<(), String> {
    save_json_atomic(path, store)
}

fn save_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("task runtime path has no parent: {}", path.display()))?;
    pixel_git::sidecar::private_dir(parent)
        .map_err(|e| format!("create {}: {e}", parent.display()))?;
    let body = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    // A fresh temporary file renamed over the name: a link committed at
    // either name is replaced, never written through.
    pixel_git::nofollow::write_replace(path, &body, pixel_git::nofollow::PRIVATE_MODE)
        .map_err(|e| format!("publish {}: {e}", path.display()))
}

fn current_head(root: &Path) -> String {
    pixel_git::GitRunner::new(root)
        .rev_parse_head()
        .unwrap_or_else(|| "unavailable".to_string())
}

fn valid_session_id(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= MAX_SESSION_ID_BYTES
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn expired(updated_unix: u64, now: u64) -> bool {
    now.saturating_sub(updated_unix) > TTL_SECS
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "pixel-task-runtime-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join(".pixel")).unwrap();
        root
    }

    fn packet(session: &str, head: &str, updated_unix: u64) -> Packet {
        Packet {
            version: STORE_VERSION,
            task_id: format!("claude:{session}:1"),
            session_id: session.to_string(),
            generation: 1,
            revision: 1,
            task: "fix auth".to_string(),
            head_oid: head.to_string(),
            created_unix: updated_unix,
            updated_unix,
            evidence: EvidenceSnapshot {
                revision: 1,
                reason: "initial_prompt".to_string(),
                created_unix: updated_unix,
                head_oid: head.to_string(),
                targets: vec![TaskTarget {
                    path: "src/a.rs".to_string(),
                    tier: "P0".to_string(),
                    line: Some(7),
                    evidence: None,
                }],
                impact: "deferred_no_symbol".to_string(),
            },
            intent: None,
        }
    }

    fn write_store(root: &Path, sessions: Vec<Packet>) {
        let store = Store {
            version: STORE_VERSION,
            sessions,
        };
        save_store(&store_path(root), &store).unwrap();
    }

    #[test]
    fn read_returns_the_session_packet_only_for_its_head_and_while_fresh() {
        let root = root("freshness");
        let written = packet("session-1", "abc", 100);
        write_store(
            &root,
            vec![written.clone(), packet("session-2", "abc", 100)],
        );

        assert_eq!(
            read_claude_packet(&root, "session-1", "abc", 101),
            Some(written)
        );
        assert!(read_claude_packet(&root, "session-1", "def", 101).is_none());
        assert!(read_claude_packet(&root, "session-1", "abc", 100 + TTL_SECS + 1).is_none());
        assert!(read_claude_packet(&root, "bad/session", "abc", 101).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_state_reads_as_absent() {
        let root = root("corrupt");
        std::fs::write(store_path(&root), "not json").unwrap();
        assert!(read_claude_packet(&root, "session-1", "abc", 200).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn store_should_load_a_packet_written_before_the_intent_field_existed() {
        let root = root("pre-intent");
        let packet = [
            r#"{"version":1,"task_id":"claude:session-1:1","session_id":"session-1","#,
            r#""generation":1,"revision":1,"task":"fix auth","head_oid":"abc","#,
            r#""created_unix":100,"updated_unix":100,"evidence":{"revision":1,"#,
            r#""reason":"initial_prompt","created_unix":100,"head_oid":"abc","#,
            r#""targets":[{"path":"src/a.rs","tier":"P0"}],"impact":"deferred_no_symbol"}}"#,
        ]
        .join("");
        std::fs::write(
            store_path(&root),
            format!(r#"{{"version":1,"sessions":[{packet}]}}"#),
        )
        .unwrap();
        let loaded = read_claude_packet(&root, "session-1", "abc", 101).unwrap();
        assert_eq!(loaded.task, "fix auth");
        assert_eq!(loaded.intent, None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reset_removes_exactly_the_named_session() {
        let root = root("reset");
        write_store(
            &root,
            vec![
                packet("session-1", "abc", 100),
                packet("session-2", "abc", 100),
            ],
        );
        assert_eq!(reset(&root, "session-1"), Ok(true));
        assert_eq!(reset(&root, "session-1"), Ok(false));
        let left = load_store(&store_path(&root));
        assert_eq!(
            left.sessions
                .iter()
                .map(|p| p.session_id.as_str())
                .collect::<Vec<_>>(),
            ["session-2"]
        );
        assert!(reset(&root, "bad/session").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Packet and schedule timestamps are compared with wall-clock time by
    /// their readers; a placeholder (0, 1) would date everything to 1970.
    #[test]
    fn now_unix_is_the_current_unix_epoch_in_seconds() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let now = now_unix();
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(
            now >= before && now <= after,
            "{before} <= {now} <= {after}"
        );
        assert!(now > 1_577_836_800, "{now}"); // 2020-01-01T00:00:00Z
    }
}
