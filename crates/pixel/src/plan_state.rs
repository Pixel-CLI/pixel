// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Tracked state for `pixel plan`: `.pixel/plan.json`.
//!
//! A plan run produces findings; the state file turns them into a checklist
//! that survives re-plans. Items are keyed by `file:line:label`, so a fresh
//! `pixel plan` preserves `done` marks for findings that are still present
//! and flags the ones that disappeared as `stale` instead of silently
//! dropping them — the re-validation step between planning and execution.

use std::path::{Path, PathBuf};

use pixel_graph::plan::PlanFinding;
use pixel_index::index::SHARD_DIR;
use serde::{Deserialize, Serialize};

pub const PLAN_STATE_FILE: &str = "plan.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanItem {
    /// `file:line:label` — the identity a re-plan matches on.
    pub key: String,
    pub label: String,
    pub file: String,
    pub line: u32,
    pub fan_in: u32,
    pub severity: String,
    #[serde(default)]
    pub done: bool,
    /// Present in a previous plan but absent from the latest one.
    #[serde(default)]
    pub stale: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PlanState {
    #[serde(default)]
    pub items: Vec<PlanItem>,
}

fn key_of(f: &PlanFinding) -> String {
    format!("{}:{}:{}", f.file, f.line, f.label)
}

pub fn state_path(root: &Path) -> PathBuf {
    root.join(SHARD_DIR).join(PLAN_STATE_FILE)
}

/// Load the tracked checklist; a missing file is an empty state, a corrupt
/// one is an error — the user decides whether to delete it.
pub fn load(root: &Path) -> Result<PlanState, String> {
    let path = state_path(root);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("plan state {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PlanState::default()),
        Err(e) => Err(format!("plan state {}: {e}", path.display())),
    }
}

/// Merge a fresh plan's findings into the tracked state: `done` survives by
/// key, vanished findings stay as `stale`, reappearing ones lose the flag.
pub fn merge(state: &mut PlanState, findings: &[PlanFinding]) {
    let fresh: std::collections::HashSet<String> = findings.iter().map(key_of).collect();
    for item in &mut state.items {
        item.stale = !fresh.contains(&item.key);
    }
    let known: std::collections::HashSet<String> =
        state.items.iter().map(|i| i.key.clone()).collect();
    for f in findings {
        if !known.contains(&key_of(f)) {
            state.items.push(PlanItem {
                key: key_of(f),
                label: f.label.clone(),
                file: f.file.clone(),
                line: f.line,
                fan_in: f.fan_in,
                severity: f.severity.as_str().to_string(),
                done: false,
                stale: false,
            });
        }
    }
}

/// Atomic write: temp file + rename so a concurrent reader never sees a
/// half-written checklist.
pub fn save(root: &Path, state: &PlanState) -> Result<(), String> {
    let path = state_path(root);
    if let Some(dir) = path.parent() {
        pixel_git::sidecar::private_dir(dir).map_err(|e| format!("plan state dir: {e}"))?;
    }
    let bytes = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    // A fresh temporary file renamed over the name: a link committed at
    // either name is replaced, never written through.
    pixel_git::nofollow::write_replace(&path, &bytes, pixel_git::nofollow::PRIVATE_MODE)
        .map_err(|e| format!("plan state write: {e}"))
}

/// Flip `done` on the item at 1-based position `n` (as `--status` prints
/// them). Out-of-range is a usage error naming the valid range.
pub fn set_done(state: &mut PlanState, n: usize, done: bool) -> Result<(), String> {
    let len = state.items.len();
    match n.checked_sub(1).and_then(|i| state.items.get_mut(i)) {
        Some(item) => {
            item.done = done;
            Ok(())
        }
        None => Err(format!(
            "plan: item {n} out of range (1..={len} tracked; 0 means none)"
        )),
    }
}

/// Drop items the latest plan no longer reports. Returns how many were
/// pruned so the caller can report it.
pub fn prune(state: &mut PlanState) -> usize {
    let before = state.items.len();
    state.items.retain(|i| !i.stale);
    before - state.items.len()
}

/// `[x]`/`[ ]` checklist with stale marks and a progress line.
pub fn render_status(state: &PlanState) -> String {
    if state.items.is_empty() {
        return "no tracked plan items — run `pixel plan` first\n".to_string();
    }
    let done = state.items.iter().filter(|i| i.done).count();
    let mut out = String::new();
    for (i, item) in state.items.iter().enumerate() {
        let mark = if item.done { "x" } else { " " };
        let stale = if item.stale { " [stale]" } else { "" };
        out.push_str(&format!(
            "{}. [{}] {} in {} (line {}) [{}]{}\n",
            i + 1,
            mark,
            item.label,
            item.file,
            item.line,
            item.severity,
            stale
        ));
    }
    out.push_str(&format!("{done}/{} done\n", state.items.len()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixel_graph::plan::Severity;

    fn finding(file: &str, line: u32, label: &str) -> PlanFinding {
        PlanFinding {
            file: file.to_string(),
            line,
            label: label.to_string(),
            fan_in: 1,
            severity: Severity::Low,
            kind: pixel_graph::plan::FindingKind::Site,
            blocking: false,
        }
    }

    #[test]
    fn merge_preserves_done_and_flags_vanished_items_stale() {
        let mut state = PlanState::default();
        merge(
            &mut state,
            &[finding("a.rs", 1, "one"), finding("b.rs", 2, "two")],
        );
        set_done(&mut state, 1, true).unwrap();
        // Re-plan: "one" still present, "two" gone, "three" new.
        merge(
            &mut state,
            &[finding("a.rs", 1, "one"), finding("c.rs", 3, "three")],
        );
        assert!(state.items[0].done);
        assert!(!state.items[0].stale);
        assert!(state.items[1].stale);
        assert_eq!(state.items.len(), 3);
        assert_eq!(state.items[2].label, "three");
        // A vanished item that reappears loses the stale flag.
        merge(&mut state, &[finding("b.rs", 2, "two")]);
        assert!(!state.items[1].stale);
        assert!(state.items[0].stale);
        assert!(state.items[2].stale);
    }

    #[test]
    fn set_done_bounds_are_exact() {
        let mut state = PlanState::default();
        merge(&mut state, &[finding("a.rs", 1, "one")]);
        assert!(set_done(&mut state, 0, true).is_err());
        assert!(set_done(&mut state, 2, true).is_err());
        set_done(&mut state, 1, true).unwrap();
        assert!(state.items[0].done);
        set_done(&mut state, 1, false).unwrap();
        assert!(!state.items[0].done);
    }

    #[test]
    fn prune_drops_only_stale_and_reports_the_count() {
        let mut state = PlanState::default();
        merge(
            &mut state,
            &[finding("a.rs", 1, "one"), finding("b.rs", 2, "two")],
        );
        merge(&mut state, &[finding("a.rs", 1, "one")]);
        assert_eq!(prune(&mut state), 1);
        assert_eq!(state.items.len(), 1);
        assert_eq!(prune(&mut state), 0);
    }

    #[test]
    fn save_then_load_round_trips_and_marks_progress() {
        let dir = std::env::temp_dir().join(format!("px-plan-{}", std::process::id()));
        let mut state = PlanState::default();
        merge(
            &mut state,
            &[finding("a.rs", 1, "one"), finding("b.rs", 2, "two")],
        );
        set_done(&mut state, 2, true).unwrap();
        save(&dir, &state).unwrap();
        let loaded = load(&dir).unwrap();
        assert_eq!(loaded.items.len(), 2);
        assert!(loaded.items[1].done);
        let text = render_status(&loaded);
        assert!(text.contains("1. [ ] one in a.rs (line 1) [LOW]"));
        assert!(text.contains("2. [x] two in b.rs (line 2) [LOW]"));
        assert!(text.contains("1/2 done"));
        // Missing file → empty state; the status render says so.
        let _ = std::fs::remove_file(state_path(&dir));
        let fresh = load(&dir).unwrap();
        assert!(render_status(&fresh).contains("no tracked plan items"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only `NotFound` means "no checklist yet" — any other read failure is
    /// an error, not an empty state that would silently drop progress.
    #[test]
    fn load_errors_when_the_state_file_is_not_a_file() {
        let dir = std::env::temp_dir().join(format!("px-plan-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(state_path(&dir)).unwrap();
        assert!(load(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
