// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Durable, bounded task packets for Claude Code hook delivery.
//!
//! This store is deliberately separate from `.pixel/targets.json`: targets is
//! an advisory cross-provider manifest, while this file records the active
//! Claude task for a particular hook session. Corrupt or unavailable state is
//! treated as absent so hook callers can always fail open.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

const STORE_VERSION: u8 = 1;
const MAX_SESSIONS: usize = 16;
const TTL_SECS: u64 = 24 * 60 * 60;
const MAX_SESSION_ID_BYTES: usize = 128;
const MAX_TASK_CHARS: usize = 1024;
const MAX_TARGETS: usize = 8;
const MAX_PATH_CHARS: usize = 512;
const MAX_EVIDENCE_CHARS: usize = 180;
const MIN_RENDER_BUDGET: usize = 256;
/// Marks a task the rendered packet had to cut to fit its byte budget.
const TASK_CUT_MARK: &str = "…";
/// What a packet appends after its targets when none fit the budget.
const NO_TARGETS_NOTE: &str = "No ranked targets were available; investigate from source.\n";
/// The packet's closing line.
const PACKET_CLOSING: &str =
    "Evidence is bounded; omitted files and unresolved dependencies may exist.";
/// Bytes the render keeps after the task line for one target row.
const TARGET_ROW_BYTES: usize = 96;
/// Bytes the render keeps after the task line: one target row, the
/// no-targets note and the closing line.
const RENDER_TAIL_RESERVE: usize = TARGET_ROW_BYTES + NO_TARGETS_NOTE.len() + PACKET_CLOSING.len();

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

impl Packet {
    /// Produce bounded, factual hook context. A packet is a discovery hint,
    /// never a closed-world edit boundary or instruction source.
    pub(crate) fn render_context(&self, budget: usize) -> Option<String> {
        if budget < MIN_RENDER_BUDGET {
            return None;
        }
        let intent = self
            .intent
            .as_ref()
            .and_then(crate::prompt_intent::render_line);
        let tail = format!("Impact: {}\nTargets:\n", self.evidence.impact);
        // The store bounds `task` in characters, the render measures bytes: a
        // long multibyte task plus the intent line would otherwise push the
        // header past the budget, return None here and cost the packet its
        // impact and targets. Reserve the rest of the render up front and cut
        // the task to what remains, marking the cut.
        let reserve = intent.as_ref().map_or(0, String::len) + tail.len() + RENDER_TAIL_RESERVE;
        let mut text = format!(
            "[PIXEL:TASK_RUNTIME v1] Factual local task packet, not an exhaustive task map or read/edit boundary. Expand investigation when evidence is insufficient.\nTask ID: {}\nGeneration: {} | revision: {}\nHEAD: {}\nTask: ",
            self.task_id, self.generation, self.revision, self.head_oid,
        );
        let task = truncate_bytes(&self.task, budget.saturating_sub(text.len() + reserve));
        text.push_str(task);
        if task.len() < self.task.len() {
            text.push_str(TASK_CUT_MARK);
        }
        text.push('\n');
        if let Some(line) = intent {
            text.push_str(&line);
            text.push('\n');
        }
        text.push_str(&tail);
        if text.len() >= budget {
            return None;
        }

        let mut emitted = 0;
        for target in &self.evidence.targets {
            let mut row = serde_json::json!({
                "path": target.path,
                "tier": target.tier,
            });
            if let Some(line) = target.line {
                row["line"] = Value::from(line);
            }
            if let Some(evidence) = &target.evidence {
                row["evidence"] = Value::from(evidence.clone());
            }
            let line = serde_json::to_string(&row).ok()?;
            if text.len() + line.len() + 1 > budget.saturating_sub(96) {
                break;
            }
            text.push_str(&line);
            text.push('\n');
            emitted += 1;
        }
        if emitted == 0 {
            text.push_str(NO_TARGETS_NOTE);
        }
        text.push_str(PACKET_CLOSING);
        Some(text)
    }
}

/// Create or refresh the active packet for a Claude hook session. A boundary
/// advances the generation; ordinary prompts refresh the current generation.
/// All errors become `None` so hook callers can continue without state.
pub(crate) fn upsert_claude_task(
    root: &Path,
    session_id: &str,
    prompt: &str,
    targets: Value,
    boundary: bool,
    intent: Option<Intent>,
) -> Option<Packet> {
    if !valid_session_id(session_id) {
        return None;
    }
    let now = now_unix();
    let head_oid = current_head(root);
    let state = PromptState {
        prompt,
        targets,
        boundary,
        intent,
    };
    upsert_at(root, session_id, state, &head_oid, now).ok()
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

/// What one prompt contributes to its session's packet.
struct PromptState<'a> {
    prompt: &'a str,
    targets: Value,
    boundary: bool,
    intent: Option<Intent>,
}

fn upsert_at(
    root: &Path,
    session_id: &str,
    state: PromptState<'_>,
    head_oid: &str,
    now: u64,
) -> Result<Packet, String> {
    let PromptState {
        prompt,
        targets,
        boundary,
        intent,
    } = state;
    let state_path = store_path(root);
    let mut store = load_store(&state_path);
    store
        .sessions
        .retain(|packet| !expired(packet.updated_unix, now));

    let prior = store
        .sessions
        .iter()
        .position(|packet| packet.session_id == session_id)
        .map(|index| store.sessions.remove(index));
    let generation = match &prior {
        Some(packet) if boundary => packet.generation.saturating_add(1),
        Some(packet) => packet.generation,
        None => 1,
    };
    let revision = prior
        .as_ref()
        .filter(|_| !boundary)
        .map_or(1, |packet| packet.revision.saturating_add(1));
    let packet = Packet {
        version: STORE_VERSION,
        task_id: format!("claude:{session_id}:{generation}"),
        session_id: session_id.to_string(),
        generation,
        revision,
        task: truncate(prompt.trim(), MAX_TASK_CHARS),
        head_oid: head_oid.to_string(),
        created_unix: prior
            .as_ref()
            .filter(|_| !boundary)
            .map_or(now, |packet| packet.created_unix),
        updated_unix: now,
        evidence: EvidenceSnapshot {
            revision,
            reason: if boundary {
                "task_boundary".to_string()
            } else if prior.is_some() {
                "prompt_refresh".to_string()
            } else {
                "initial_prompt".to_string()
            },
            created_unix: now,
            head_oid: head_oid.to_string(),
            targets: extract_targets(&targets),
            impact: "deferred_no_symbol".to_string(),
        },
        intent,
    };
    store.sessions.push(packet.clone());
    store.sessions.sort_by_key(|entry| entry.updated_unix);
    if store.sessions.len() > MAX_SESSIONS {
        let overflow = store.sessions.len() - MAX_SESSIONS;
        store.sessions.drain(0..overflow);
    }
    save_store(&state_path, &store)?;
    Ok(packet)
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

fn extract_targets(data: &Value) -> Vec<TaskTarget> {
    data.get("targets")
        .and_then(Value::as_array)
        .map(|targets| {
            targets
                .iter()
                .filter_map(|target| {
                    let path = target.get("path")?.as_str()?;
                    if path.is_empty() {
                        return None;
                    }
                    let tier = match target.get("tier").and_then(Value::as_str) {
                        Some("P0") => "P0",
                        Some("P2") => "P2",
                        _ => "P1",
                    };
                    let evidence = target
                        .get("evidence")
                        .and_then(Value::as_array)
                        .and_then(|items| items.first())
                        .and_then(|item| item.get("text"))
                        .and_then(Value::as_str)
                        .map(|text| truncate(text, MAX_EVIDENCE_CHARS));
                    let line = target
                        .get("evidence")
                        .and_then(Value::as_array)
                        .and_then(|items| items.first())
                        .and_then(|item| item.get("line"))
                        .and_then(Value::as_u64);
                    Some(TaskTarget {
                        path: truncate(path, MAX_PATH_CHARS),
                        tier: tier.to_string(),
                        line,
                        evidence,
                    })
                })
                .take(MAX_TARGETS)
                .collect()
        })
        .unwrap_or_default()
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

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

/// `value` cut to at most `max` bytes, never through a character.
fn truncate_bytes(value: &str, max: usize) -> &str {
    let mut end = 0;
    for (index, character) in value.char_indices() {
        if index + character.len_utf8() > max {
            break;
        }
        end = index + character.len_utf8();
    }
    &value[..end]
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

    fn upsert_plain(
        root: &Path,
        session_id: &str,
        prompt: &str,
        targets: Value,
        boundary: bool,
        head_oid: &str,
        now: u64,
    ) -> Result<Packet, String> {
        let state = PromptState {
            prompt,
            targets,
            boundary,
            intent: None,
        };
        upsert_at(root, session_id, state, head_oid, now)
    }

    fn targets(path: &str) -> Value {
        serde_json::json!({"targets":[{
            "path": path,
            "tier":"P0",
            "evidence":[{"line":7,"text":"relevant implementation evidence"}]
        }]})
    }

    #[test]
    fn starts_then_refreshes_same_generation_with_bounded_evidence() {
        let root = root("refresh");
        let first = upsert_plain(
            &root,
            "session-1",
            " first task ",
            targets("src/a.rs"),
            false,
            "abc",
            100,
        )
        .unwrap();
        let refreshed = upsert_plain(
            &root,
            "session-1",
            "second prompt",
            targets("src/b.rs"),
            false,
            "abc",
            101,
        )
        .unwrap();

        assert_eq!(first.task_id, "claude:session-1:1");
        assert_eq!(refreshed.generation, 1);
        assert_eq!(refreshed.revision, 2);
        assert_eq!(refreshed.created_unix, 100);
        assert_eq!(refreshed.evidence.reason, "prompt_refresh");
        assert_eq!(refreshed.evidence.targets[0].path, "src/b.rs");
        assert_eq!(
            read_claude_packet(&root, "session-1", "abc", 101),
            Some(refreshed)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn boundary_starts_new_generation_and_revision_one() {
        let root = root("boundary");
        upsert_plain(
            &root,
            "session-1",
            "first",
            targets("src/a.rs"),
            false,
            "abc",
            100,
        )
        .unwrap();
        let next = upsert_plain(
            &root,
            "session-1",
            "new task",
            targets("src/b.rs"),
            true,
            "abc",
            101,
        )
        .unwrap();

        assert_eq!(next.task_id, "claude:session-1:2");
        assert_eq!(next.generation, 2);
        assert_eq!(next.revision, 1);
        assert_eq!(next.created_unix, 101);
        assert_eq!(next.evidence.reason, "task_boundary");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mismatched_head_and_expired_packet_do_not_restore() {
        let root = root("freshness");
        upsert_plain(
            &root,
            "session-1",
            "task",
            targets("src/a.rs"),
            false,
            "abc",
            100,
        )
        .unwrap();

        assert!(read_claude_packet(&root, "session-1", "def", 101).is_none());
        assert!(read_claude_packet(&root, "session-1", "abc", 100 + TTL_SECS + 1).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cap_evicts_oldest_session_and_invalid_or_corrupt_state_fails_open() {
        let root = root("cap");
        for index in 0..=MAX_SESSIONS {
            upsert_plain(
                &root,
                &format!("session-{index}"),
                "task",
                targets("src/a.rs"),
                false,
                "abc",
                100 + index as u64,
            )
            .unwrap();
        }
        let store = load_store(&store_path(&root));
        assert_eq!(store.sessions.len(), MAX_SESSIONS);
        assert!(
            store
                .sessions
                .iter()
                .all(|packet| packet.session_id != "session-0")
        );

        std::fs::write(store_path(&root), "not json").unwrap();
        assert!(read_claude_packet(&root, "session-1", "abc", 200).is_none());
        assert!(
            upsert_claude_task(
                &root,
                "bad/session",
                "task",
                targets("src/a.rs"),
                false,
                None
            )
            .is_none()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rendered_packet_is_bounded_and_says_evidence_is_not_a_boundary() {
        let root = root("render");
        let packet = upsert_plain(
            &root,
            "session-1",
            "task",
            targets("src/a.rs"),
            false,
            "abc",
            100,
        )
        .unwrap();
        let rendered = packet.render_context(600).unwrap();

        assert!(rendered.len() <= 600);
        assert!(rendered.contains("[PIXEL:TASK_RUNTIME v1]"));
        assert!(rendered.contains("not an exhaustive task map"));
        assert!(packet.render_context(100).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn intent(label: &str, p: f64) -> Intent {
        Intent {
            label: label.to_string(),
            p,
            model: "winnow:e4b".to_string(),
        }
    }

    fn upsert_with_intent(root: &Path, prompt: &str, intent: Option<Intent>, now: u64) -> Packet {
        let state = PromptState {
            prompt,
            targets: targets("src/a.rs"),
            boundary: false,
            intent,
        };
        upsert_at(root, "session-1", state, "abc", now).unwrap()
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
        assert!(!loaded.render_context(4096).unwrap().contains("Intent"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn packet_should_persist_its_intent_and_render_it_after_a_restore() {
        let root = root("intent-roundtrip");
        let written = upsert_with_intent(&root, "fix auth", Some(intent("bugfix", 0.82)), 100);
        assert_eq!(written.intent, Some(intent("bugfix", 0.82)));
        let restored = read_claude_packet(&root, "session-1", "abc", 101).unwrap();
        assert_eq!(restored, written);
        let rendered = restored.render_context(4096).unwrap();
        let task = rendered.find("Task: fix auth\n").unwrap();
        let line = rendered
            .find("Intent (classifier verdict, not fact): bugfix p=0.82 (winnow:e4b) → start with: pixel plan-rollback")
            .unwrap();
        let impact = rendered.find("Impact: ").unwrap();
        assert!(task < line && line < impact, "{rendered}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn packet_should_drop_a_previous_intent_when_the_next_prompt_has_none() {
        let root = root("intent-refresh");
        upsert_with_intent(&root, "fix auth", Some(intent("bugfix", 0.82)), 100);
        let refreshed = upsert_with_intent(&root, "now explain it", None, 101);
        assert_eq!(refreshed.intent, None);
        assert_eq!(
            read_claude_packet(&root, "session-1", "abc", 102)
                .unwrap()
                .intent,
            None
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn packet_should_not_render_an_intent_below_one_half() {
        let root = root("intent-low");
        let packet = upsert_with_intent(&root, "fix auth", Some(intent("bugfix", 0.4)), 100);
        assert_eq!(
            packet.intent,
            Some(intent("bugfix", 0.4)),
            "kept as a claim"
        );
        assert!(!packet.render_context(4096).unwrap().contains("Intent"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_long_multibyte_task_should_keep_its_packet_context_and_intent() {
        let root = root("intent-multibyte");
        // The store's 1024-character bound in four-byte characters is the
        // render's whole 4096-byte budget: before the render cut the task to
        // size, the header pushed it over and the packet was dropped.
        let long = "😀".repeat(MAX_TASK_CHARS);
        let packet = upsert_with_intent(&root, &long, Some(intent("bugfix", 0.82)), 100);
        assert_eq!(packet.task.chars().count(), MAX_TASK_CHARS);

        let rendered = packet.render_context(4096).unwrap();
        assert!(rendered.len() <= 4096, "{}", rendered.len());
        assert!(rendered.contains("Task: 😀"), "the task survives, cut");
        assert!(rendered.contains(TASK_CUT_MARK), "a cut task is marked");
        assert!(
            rendered.contains("Intent (classifier verdict, not fact): bugfix p=0.82"),
            "the intent keeps its place"
        );
        assert!(rendered.contains("Impact: deferred_no_symbol"));
        assert!(
            rendered.contains("src/a.rs"),
            "the packet keeps its targets"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn truncate_bytes_should_cut_only_on_a_character_boundary() {
        assert_eq!(truncate_bytes("verbose", 7), "verbose");
        assert_eq!(truncate_bytes("verbose", 3), "ver");
        assert_eq!(truncate_bytes("é", 1), "");
        assert_eq!(truncate_bytes("é", 2), "é");
        assert_eq!(truncate_bytes("漢漢", 4), "漢");
        assert_eq!(truncate_bytes("a漢", 2), "a");
        assert_eq!(truncate_bytes("漢", 0), "");
    }

    #[test]
    fn packet_should_say_when_no_targets_were_available_and_not_when_some_were() {
        let root = root("no-targets");
        let bare = upsert_plain(
            &root,
            "session-1",
            "task",
            serde_json::json!({"targets": []}),
            false,
            "abc",
            100,
        )
        .unwrap();
        assert!(
            bare.render_context(4096).unwrap().contains(NO_TARGETS_NOTE),
            "an empty packet names the absence"
        );

        let ranked = upsert_plain(
            &root,
            "session-2",
            "task",
            targets("src/a.rs"),
            false,
            "abc",
            100,
        )
        .unwrap();
        let rendered = ranked.render_context(4096).unwrap();
        assert!(rendered.contains("src/a.rs"));
        assert!(
            !rendered.contains("No ranked targets"),
            "a packet with targets never claims there were none: {rendered}"
        );
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
