//! `pixel hook post-compaction` — re-inject targets manifest after context compaction.
//!
//! Fires on every `PostCompaction` hook event. When Devin/Claude/Codex compacts
//! the conversation context, the agent loses its `pixel targets` manifest — the
//! P0/P1/P2 file list that constrains which files it may read/edit. Without
//! re-injection, the agent may start reading files outside the list, breaking
//! the retrieval contract.
//!
//! This hook reads the PostCompaction payload from stdin, checks whether a
//! targets manifest is active for the current repo (`.pixel/targets.json`),
//! and if so, emits the manifest as `additionalContext` so the agent resumes
//! with its retrieval state intact.
//!
//! Never blocks: a hard 200ms deadline means any slow path is abandoned and
//! the hook exits 0 with no output.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

/// Hard deadline for the entire hook — never block after compaction.
const HOOK_DEADLINE: Duration = Duration::from_millis(200);
/// Manifest TTL — matches the guard's `MANIFEST_MAX_AGE_SECS`.
const MANIFEST_MAX_AGE_SECS: u64 = 24 * 60 * 60;

/// The PostCompaction hook payload (Claude Code / Devin shape).
#[derive(Deserialize)]
struct PostCompactionPayload {
    #[serde(default)]
    cwd: Option<String>,
}

/// Entry point for `pixel hook post-compaction`. Reads the PostCompaction
/// payload from stdin. Never returns an `Err` as exit 1 — every failure
/// path is a silent exit 0 (compaction proceeds normally).
pub fn run() -> ! {
    // Allow opt-out via env var.
    if let Ok(kill) = std::env::var("PIXEL_POST_COMPACTION") {
        if matches!(kill.as_str(), "0" | "false" | "off") {
            std::process::exit(0);
        }
    }

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() || input.trim().is_empty() {
        // No payload — try current dir.
        let cwd = std::env::current_dir().unwrap_or_default();
        try_emit_manifest(&cwd);
    }

    // Try to parse the payload — if it fails, fall back to current dir.
    let payload: PostCompactionPayload = match serde_json::from_str(&input) {
        Ok(p) => p,
        Err(_) => {
            let cwd = std::env::current_dir().unwrap_or_default();
            try_emit_manifest(&cwd);
        }
    };

    let cwd = payload
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    try_emit_manifest(&cwd);
}

/// Try to read the active targets manifest and emit it as additional context.
/// If no manifest is active, or reading fails, exit 0 silently.
fn try_emit_manifest(cwd: &PathBuf) -> ! {
    let deadline = Instant::now() + HOOK_DEADLINE;

    let (tx, rx) = std::sync::mpsc::channel();
    let cwd_clone = cwd.clone();
    std::thread::spawn(move || {
        let result = read_manifest(&cwd_clone);
        let _ = tx.send(result);
    });

    match rx.recv_timeout(HOOK_DEADLINE) {
        Ok(Ok(Some(manifest_text))) if Instant::now() <= deadline => {
            emit_manifest(&manifest_text);
        }
        _ => std::process::exit(0),
    }
}

/// Read the active targets manifest from `{repo}/.pixel/targets.json`.
/// Supports both v2 (multi-task) and legacy v1 formats.
/// Returns `Some(text)` if a manifest with fresh tasks is active, `None` otherwise.
fn read_manifest(cwd: &Path) -> Result<Option<String>, String> {
    // Walk up from cwd to find a .pixel/targets.json (the repo root).
    let manifest_path = find_manifest(cwd)?;
    let manifest_path = match manifest_path {
        Some(p) => p,
        None => return Ok(None),
    };

    let data = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("read manifest: {e}"))?;

    let m: Value = serde_json::from_str(&data)
        .map_err(|e| format!("parse manifest: {e}"))?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Collect (task, [(path, tier)]) pairs from the manifest, filtering expired.
    let mut all_tasks: Vec<(String, Vec<(String, String)>)> = Vec::new();

    if m.get("version").and_then(Value::as_u64) == Some(2) {
        // v2 multi-task format.
        if let Some(tasks) = m.get("tasks").and_then(Value::as_array) {
            for t in tasks {
                let created = t.get("created_unix").and_then(Value::as_u64).unwrap_or(0);
                if now.saturating_sub(created) > MANIFEST_MAX_AGE_SECS {
                    continue; // expired
                }
                let task = t.get("task").and_then(Value::as_str).unwrap_or("?").to_string();
                let files = parse_targets(t.get("targets").and_then(Value::as_array));
                if !files.is_empty() {
                    all_tasks.push((task, files));
                }
            }
        }
    } else {
        // legacy v1 format.
        let created = m.get("created_unix").and_then(Value::as_u64).unwrap_or(0);
        if now.saturating_sub(created) <= MANIFEST_MAX_AGE_SECS {
            let task = m.get("task").and_then(Value::as_str).unwrap_or("?").to_string();
            let files = parse_targets(m.get("files").and_then(Value::as_array));
            if !files.is_empty() {
                all_tasks.push((task, files));
            }
        }
    }

    if all_tasks.is_empty() {
        return Ok(None);
    }

    // Build a compact manifest summary for re-injection.
    let mut lines = Vec::new();
    lines.push(
        "[PIXEL:POST_COMPACTION] Context was compacted. Your active `pixel targets` manifest has been re-injected below — do NOT read or edit files outside this list. Re-run `pixel targets` if the task has changed.\n"
            .to_string(),
    );

    for (task, files) in &all_tasks {
        lines.push(format!("Task: {task}"));

        let mut p0: Vec<&str> = Vec::new();
        let mut p1: Vec<&str> = Vec::new();
        let mut p2: Vec<&str> = Vec::new();
        for (path, tier) in files {
            match tier.as_str() {
                "P0" => p0.push(path),
                "P1" => p1.push(path),
                "P2" => p2.push(path),
                _ => p0.push(path),
            }
        }

        if !p0.is_empty() {
            lines.push("P0 (work first):".to_string());
            for p in &p0 {
                lines.push(format!("  {p}"));
            }
        }
        if !p1.is_empty() {
            lines.push("P1 (supporting):".to_string());
            for p in &p1 {
                lines.push(format!("  {p}"));
            }
        }
        if !p2.is_empty() {
            lines.push("P2 (may be dropped):".to_string());
            for p in &p2 {
                lines.push(format!("  {p}"));
            }
        }
        lines.push(String::new());
    }

    Ok(Some(lines.join("\n")))
}

/// Walk up from `start` to find a `.pixel/targets.json` file.
fn find_manifest(start: &Path) -> Result<Option<PathBuf>, String> {
    let mut current = start;
    loop {
        let candidate = current.join(".pixel").join("targets.json");
        if candidate.exists() {
            return Ok(Some(candidate));
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return Ok(None),
        }
    }
}

/// Parse targets array into (path, tier) pairs.
fn parse_targets(arr: Option<&Vec<Value>>) -> Vec<(String, String)> {
    let arr = match arr {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .filter_map(|t| {
            // v2 format: {path: "...", tier: "P0"}
            if let Some(path) = t.get("path").and_then(Value::as_str) {
                let tier = t.get("tier").and_then(Value::as_str).unwrap_or("P0");
                return Some((path.to_string(), tier.to_string()));
            }
            // legacy format: plain string or {path: "..."}
            if let Some(path) = t.as_str() {
                return Some((path.to_string(), "P0".to_string()));
            }
            None
        })
        .collect()
}

/// Emit the manifest as additionalContext via the hook output format.
fn emit_manifest(text: &str) -> ! {
    let output = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PostCompaction",
            "additionalContext": text
        }
    });

    let json = serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_string());
    print!("{json}");
    std::process::exit(0);
}
