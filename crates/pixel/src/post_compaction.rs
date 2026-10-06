// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Restore bounded, advisory task hints after compaction.
//!
//! Claude/Codex use SessionStart(source=compact), which supports context output;
//! PostCompact does not. Devin retains its PostCompaction lifecycle event.
//! Only current-HEAD, recent manifests are reused. This does not prove task
//! relevance or working-tree freshness, and never establishes a read/edit fence.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

/// Hard deadline for the entire hook — never block after compaction.
const HOOK_DEADLINE: Duration = Duration::from_millis(200);
/// Manifest TTL — matches the guard's `MANIFEST_MAX_AGE_SECS`.
const MANIFEST_MAX_AGE_SECS: u64 = 86_400; // 24 h
const MANIFEST_MAX_BYTES: u64 = 65_536; // 64 KiB
const CONTEXT_MAX_BYTES: usize = 4096;

#[derive(Deserialize)]
struct PostCompactionPayload {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, alias = "hookEventName")]
    hook_event_name: Option<String>,
    #[serde(default)]
    source: Option<String>,
    /// Claude Code's session identifier. This is required for the
    /// provider-qualified runtime restore path.
    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,
}

/// The whole payload when it reads as UTF-8 within `cap` bytes; `None` for
/// an unreadable stream or one past the cap, which the hook ignores.
fn read_payload(reader: impl Read, cap: u64) -> Option<String> {
    let mut input = String::new();
    reader.take(cap + 1).read_to_string(&mut input).ok()?;
    (input.len() as u64 <= cap).then_some(input)
}

/// Entry point for `pixel hook post-compaction`. Reads the PostCompaction
/// payload from stdin. Never returns an `Err` as exit 1 — every failure
/// path is a silent exit 0 (compaction proceeds normally).
pub fn run(provider: Option<crate::guard::Provider>) -> ! {
    // An imported Claude entry re-injects the Claude session packet keyed by
    // the payload's session id — under an importing host that id belongs to
    // the other harness, so the entry either injects nothing or, on an id
    // collision, another session's evidence. Devin's own entry (no provider
    // argument) reads the repo manifest instead.
    if crate::prompt_submit::imported_claude_entry(provider) {
        std::process::exit(0);
    }
    if crate::env_flag_off("PIXEL_POST_COMPACTION") {
        std::process::exit(0);
    }

    let Some(input) = read_payload(std::io::stdin(), MANIFEST_MAX_BYTES) else {
        std::process::exit(0);
    };
    let Ok(payload) = serde_json::from_str::<PostCompactionPayload>(&input) else {
        std::process::exit(0);
    };
    let Some(event_name) = context_event(&payload) else {
        std::process::exit(0);
    };

    let cwd = payload.cwd.as_deref().map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    );

    let is_claude_runtime = matches!(provider, Some(crate::guard::Provider::Claude));
    let session_id = payload.session_id.filter(|id| !id.is_empty());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let context = if is_claude_runtime {
            session_id
                .as_deref()
                .and_then(|id| read_claude_runtime(&cwd, id))
        } else {
            read_manifest(&cwd)
        };
        let _ = tx.send(context);
    });
    if let Ok(Some(text)) = rx.recv_timeout(HOOK_DEADLINE) {
        crate::prompt_submit::emit_context(&text, event_name);
    }
    std::process::exit(0);
}

/// Restore only the exact Claude session packet that matches the current HEAD.
/// Missing, stale, malformed, or cross-session state is deliberately silent.
fn read_claude_runtime(cwd: &Path, session_id: &str) -> Option<String> {
    let root = crate::discover_root(cwd).ok()?;
    let head = pixel_index::gitsync::rev_parse_head(&root)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    crate::task_runtime::read_claude_packet(&root, session_id, &head, now)?
        .render_context(CONTEXT_MAX_BYTES)
}

fn context_event(payload: &PostCompactionPayload) -> Option<&'static str> {
    match payload.hook_event_name.as_deref() {
        Some("SessionStart") if payload.source.as_deref() == Some("compact") => {
            Some("SessionStart")
        }
        Some("PostCompaction") => Some("PostCompaction"),
        // Never emit context for an event that cannot consume it.
        _ => None,
    }
}

/// Read the active targets manifest from `{repo}/.pixel/targets.json`.
/// Supports both v2 (multi-task) and legacy v1 formats.
/// Returns `Some(text)` if a manifest with fresh tasks is active, `None` otherwise.
fn read_manifest(cwd: &Path) -> Option<String> {
    let root = crate::discover_root(cwd).ok()?;
    let path = root.join(".pixel/targets.json");
    let data = pixel_index::index::read_regular_bounded(&path, MANIFEST_MAX_BYTES).ok()?;
    let manifest: Value = serde_json::from_slice(&data).ok()?;
    let head = pixel_index::gitsync::rev_parse_head(&root)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    manifest_context(&manifest, &root, &head, now)
}

fn manifest_context(m: &Value, root: &Path, head: &str, now: u64) -> Option<String> {
    let mut tasks: Vec<&Value> = match m.get("version").and_then(Value::as_u64) {
        Some(2) => m.get("tasks")?.as_array()?.iter().collect(),
        Some(1) | None => vec![m],
        _ => return None,
    };
    tasks.retain(|task| {
        task.get("head_oid").and_then(Value::as_str) == Some(head)
            && task
                .get("created_unix")
                .and_then(Value::as_u64)
                .is_some_and(|created| created <= now && now - created <= MANIFEST_MAX_AGE_SECS)
    });
    tasks.sort_by_key(|task| std::cmp::Reverse(task["created_unix"].as_u64()));
    let mut text = String::from(
        "[PIXEL:POST_COMPACTION] Saved task hints from this HEAD, not proof of working-tree freshness or current task relevance. Refresh targets if the task or files changed.\n",
    );
    let mut emitted = false;
    for task in tasks {
        let Some(files) = task
            .get("targets")
            .or_else(|| task.get("files"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        let targets: Vec<_> = files
            .iter()
            .map(|file| match file.as_str() {
                Some(path) => serde_json::json!({"path":path,"tier":"P0"}),
                None => file.clone(),
            })
            .collect();
        let title: String = task
            .get("task")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .chars()
            .take(180)
            .collect();
        let heading = format!("Task: {}\n", serde_json::to_string(&title).ok()?);
        let remaining = CONTEXT_MAX_BYTES.saturating_sub(text.len() + heading.len() + 1);
        if let Some(hints) = crate::prompt_submit::render_task_context(
            &serde_json::json!({"root":root,"targets":targets}),
            remaining,
        ) {
            text.push_str(&heading);
            text.push_str(&hints);
            text.push('\n');
            emitted = true;
        }
    }
    emitted.then_some(text)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_payload_is_read_whole_up_to_its_cap_and_dropped_past_it() {
        assert_eq!(
            super::read_payload(&b"abcd"[..], 4).as_deref(),
            Some("abcd")
        );
        assert_eq!(super::read_payload(&b"abc"[..], 4).as_deref(), Some("abc"));
        assert_eq!(super::read_payload(&b"abcde"[..], 4), None);
        assert_eq!(super::read_payload(&b"abcdef"[..], 4), None);
        assert_eq!(super::read_payload(&[0xff_u8, 0xfe][..], 4), None);
    }

    use super::*;

    fn task(name: &str, head: &str, created: u64) -> Value {
        serde_json::json!({"task":name,"head_oid":head,"created_unix":created,
            "targets":[{"path":format!("src/{name}.rs"),"tier":"P0"}]})
    }

    #[test]
    fn supported_compaction_events_only() {
        for (input, expected) in [
            (
                serde_json::json!({"hook_event_name":"SessionStart","source":"compact"}),
                Some("SessionStart"),
            ),
            (
                serde_json::json!({"hook_event_name":"SessionStart","source":"startup"}),
                None,
            ),
            (
                serde_json::json!({"hook_event_name":"PostCompact","trigger":"manual"}),
                None,
            ),
            (
                serde_json::json!({"hookEventName":"PostCompaction"}),
                Some("PostCompaction"),
            ),
            (serde_json::json!({}), None),
        ] {
            let payload: PostCompactionPayload = serde_json::from_value(input).unwrap();
            assert_eq!(context_event(&payload), expected);
        }
    }

    #[test]
    fn claude_payload_accepts_both_session_id_spellings() {
        for input in [
            serde_json::json!({
                "hook_event_name": "SessionStart",
                "source": "compact",
                "session_id": "session-a"
            }),
            serde_json::json!({
                "hookEventName": "SessionStart",
                "source": "compact",
                "sessionId": "session-b"
            }),
        ] {
            let payload: PostCompactionPayload = serde_json::from_value(input).unwrap();
            assert!(payload.session_id.is_some());
        }
    }

    #[test]
    fn restores_only_recent_same_head_tasks_newest_first() {
        let now = 200_000;
        let m = serde_json::json!({"version":2,"tasks":[
            task("older", "current", now - 30), task("newer", "current", now),
            task("stale_branch", "old", now), task("expired", "current", 1),
            task("future", "current", now + 1),
            {"task":"missing_head","created_unix":now,"targets":["src/invalid.rs"]}
        ]});
        let text = manifest_context(&m, Path::new("/repo"), "current", now).unwrap();
        assert!(text.find("newer").unwrap() < text.find("older").unwrap());
        for absent in ["stale_branch", "expired", "future", "missing_head"] {
            assert!(!text.contains(absent));
        }
        assert!(text.contains("not an exhaustive task map or a read/edit boundary"));
        assert!(text.contains("not proof of working-tree freshness"));
        assert!(!text.contains("do NOT read"));
    }

    #[test]
    fn legacy_manifest_requires_same_provenance() {
        let m = serde_json::json!({"created_unix":200_000,"head_oid":"current",
            "task":"login","files":["src/login.rs",{"path":"tests/login.rs","tier":"P1"}]});
        let text = manifest_context(&m, Path::new("/repo"), "current", 200_000).unwrap();
        assert!(text.contains("src/login.rs") && text.contains("tests/login.rs"));
        assert!(manifest_context(&m, Path::new("/repo"), "new_head", 200_000).is_none());
    }

    #[test]
    fn absent_malformed_or_expired_tasks_do_not_emit() {
        for m in [
            serde_json::json!({}),
            serde_json::json!({"version":99}),
            serde_json::json!({"version":2,"tasks":[]}),
            serde_json::json!({"version":2,"tasks":[task("old","current",1)]}),
        ] {
            assert!(manifest_context(&m, Path::new("/repo"), "current", 200_000).is_none());
        }
    }

    #[test]
    fn large_manifest_keeps_complete_json_lines_within_budget() {
        let tasks: Vec<_> = (0..40)
            .map(|i| task(&format!("{i}_{}", "🦀".repeat(80)), "current", 200_000))
            .collect();
        let m = serde_json::json!({"version":2,"tasks":tasks});
        let text = manifest_context(&m, Path::new("/repo"), "current", 200_000).unwrap();
        assert!(text.len() <= CONTEXT_MAX_BYTES);
        for line in text.lines().filter(|line| line.starts_with('{')) {
            assert!(serde_json::from_str::<Value>(line).is_ok());
        }
    }
}
