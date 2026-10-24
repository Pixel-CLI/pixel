// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Shared configuration constants and file helpers for `pixel setup`:
//! the agents' config locations, the managed marker, the hook timeout, the
//! prompt-brief settings and backup-before-write.

use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::settings::{Setting, Settings};

/// Managed-marker begin tag. Everything between this and [`MANAGED_END`] is
/// owned by pixel and rewritten on every `pixel setup`.
pub const MANAGED_BEGIN: &str = "<!-- pixel:managed:begin -->";
/// Managed-marker end tag. See [`MANAGED_BEGIN`].
pub const MANAGED_END: &str = "<!-- pixel:managed:end -->";

/// The file name every release installs pixel under.
pub const PIXEL_EXECUTABLE: &str = "pixel";
/// The file name `pixel self-update --dev` installs a side build under, so it
/// runs beside the managed `pixel` without shadowing it.
pub const PIXEL_DEV_EXECUTABLE: &str = "pixel-dev";
/// Every file name pixel installs itself under. A command run through either
/// is pixel's, whichever of the two binaries reads it: a release install
/// replaces the hooks a dev build wrote, and the guard recognises a
/// `pixel-dev` invocation as it does a `pixel` one.
pub const PIXEL_EXECUTABLES: [&str; 2] = [PIXEL_EXECUTABLE, PIXEL_DEV_EXECUTABLE];

/// Whether `exe` is the side build `self-update --dev` installs, whatever
/// directory it was copied to: the one binary on a machine that leaves the
/// home install (hooks, deployed prompts) to the managed `pixel`.
#[must_use]
pub fn is_side_build(exe: &Path) -> bool {
    exe.file_name() == Some(std::ffi::OsStr::new(PIXEL_DEV_EXECUTABLE))
}

/// Timeout in seconds for the prompt-submit entry written into agent configs.
/// A cold start can take a few seconds on first invocation, and the brief
/// itself may use up to its window (`Host::deadline`, 8 s, bounds the work).
/// Without an explicit timeout, harnesses like Codex apply a short default and
/// SIGKILL the hook (exit 137). 10 s is generous enough while still bounding
/// hang risk.
pub const HOOK_TIMEOUT: u64 = 10;
/// Timeout in seconds for the entries that never wait on the daemon
/// (session start, every tool call, stop).
pub const QUICK_HOOK_TIMEOUT: u64 = 5;

/// Settings key of how many `UserPromptSubmit` hooks carry the prompt brief.
pub const BRIEF_PARTS_KEY: &str = "brief_parts";
/// Parts the brief is split over unless `brief_parts` says otherwise. Claude
/// Code and Codex each cap what one hook injects (about 10,000 characters),
/// per hook, so a brief larger than that needs several hooks.
pub const BRIEF_PARTS_DEFAULT: u8 = 3;
/// Most parts a brief is split over; a value above it reads as this one.
pub const BRIEF_PARTS_MAX: u8 = 8;

/// How many hooks carry the prompt brief: `brief_parts` in the repository's
/// `.pixel/config.yaml` when `repo` is given and sets it, else in the global
/// `~/.pixel/config.yaml`, else [`BRIEF_PARTS_DEFAULT`]. A value outside
/// `1..=`[`BRIEF_PARTS_MAX`] is clamped into it and a value that is not a
/// whole number counts as unset; `1` is the single hook of earlier releases.
pub fn brief_parts(home: &Path, repo: Option<&Path>) -> u8 {
    Settings::load(home, repo, &|_| None).count(Setting::BriefParts)
}

/// `<dir>/.pixel/config.yaml` parsed (YAML into JSON values), or an empty
/// document when the file does not exist. A malformed file is an error, not a
/// silent default: setup and doctor must not hide a config they cannot read.
///
/// # Errors
///
/// An unreadable file, or one that is not a YAML mapping.
pub fn load_config(dir: &Path) -> std::result::Result<serde_json::Value, String> {
    let yaml = dir.join(".pixel/config.yaml");
    if !yaml.is_file() {
        return Ok(serde_json::json!({}));
    }
    let text =
        fs::read_to_string(&yaml).map_err(|error| format!("read {}: {error}", yaml.display()))?;
    let value: serde_json::Value = serde_saphyr::from_str(&text).map_err(|_| {
        format!(
            "invalid configuration {}; expected a YAML mapping",
            yaml.display()
        )
    })?;
    match value {
        serde_json::Value::Null => Ok(serde_json::json!({})),
        serde_json::Value::Object(_) => Ok(value),
        _ => Err(format!(
            "configuration {} must be a mapping",
            yaml.display()
        )),
    }
}

/// The Devin config directory (relative to home).
pub const DEVIN_CONFIG_DIR: &str = ".config/devin";
/// The Devin config file (hooks live under the `"hooks"` key here).
pub const DEVIN_CONFIG_FILE: &str = "config.json";

/// The Codex hooks file (relative to home). Codex uses the same hook
/// format as Claude — hooks under the `"hooks"` key, event `PreToolUse`.
pub const CODEX_HOOKS_FILE: &str = ".codex/hooks.json";

/// The Gemini config file (relative to home). Gemini uses hooks under
/// the `"hooks"` key but the tool-event is `BeforeTool` (not `PreToolUse`).
pub const GEMINI_SETTINGS_FILE: &str = ".gemini/settings.json";

/// The pi config directory (relative to home). The repository extension uses
/// pi's `tool_call` event to intercept native calls before execution.
pub const PI_CONFIG_DIR: &str = ".pi/agent";

/// Harness config roots whose `skills/` dir a `pixel-classify` copy deploys
/// into — the skill is harness-agnostic, so every configured harness gets it
/// (`.claude` is a target unconditionally: install writes Claude's hooks
/// before the helpers step runs). `.agents` is the cross-harness personal
/// skills directory: GitHub Copilot CLI and Amp read `~/.agents/skills`, and
/// it is where the generic `AGENTS.md` target's skill goes.
pub const SKILL_ROOTS: &[&str] = &[
    ".agents",
    ".codex",
    ".cursor",
    ".devin",
    ".gemini",
    ".pi/agent",
    ".config/opencode",
];

/// Back up `path` to a timestamped sibling file before a destructive
/// rewrite — but only if the file exists AND its current content differs
/// from `new_content`. This skips no-op backups on idempotent re-installs
/// where nothing would actually change.
///
/// Returns the backup path if one was written, or `None` if the file did
/// not exist yet or its content is already identical to `new_content`.
pub fn backup_if_changing(path: &Path, new_content: &[u8]) -> io::Result<Option<PathBuf>> {
    static BACKUP_SEQ: AtomicU64 = AtomicU64::new(0);
    let current = match fs::read(path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if current == new_content {
        return Ok(None);
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seq = BACKUP_SEQ.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let backup_name = format!("{file_name}.pixel-bak.{nanos}-{seq}");
    let backup_path = match path.parent() {
        Some(parent) => parent.join(backup_name),
        None => PathBuf::from(backup_name),
    };
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut backup = options.open(&backup_path)?;
    backup.write_all(&current)?;
    backup.set_permissions(fs::metadata(path)?.permissions())?;
    Ok(Some(backup_path))
}

/// Merge a pixel-authored hook entry into an existing `hooks.<Event>` JSON
/// value, replacing only a **prior pixel-authored entry** for that same
/// event (identified by `pixel_marker`, a substring unique to pixel's own
/// command — e.g. `"hook session-start"`) and preserving every other entry
/// verbatim, however many other tools have registered there.
///
/// This is the fix for a real, confirmed bug: naively doing
/// `obj.insert("SessionStart", [pixel_entry])` would silently destroy every
/// pre-existing `SessionStart` entry other tools configured (observed
/// directly against a real `~/.claude/settings.json` carrying three
/// unrelated `SessionStart` matcher groups). Merging by marker keeps
/// re-installs idempotent (pixel's own entry is replaced, not duplicated)
/// without touching anyone else's configuration.
///
/// `existing` is the current `hooks.<Event>` value if any. Real Claude
/// settings always shape this as an array; a non-array value is preserved
/// by wrapping it rather than discarded, since that's still "someone's
/// configuration" even if malformed.
pub fn merge_hook_entry(
    existing: Option<&serde_json::Value>,
    pixel_marker: &str,
    pixel_entry: serde_json::Value,
) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = match existing {
        Some(serde_json::Value::Array(arr)) => arr.clone(),
        Some(other) => vec![other.clone()],
        None => Vec::new(),
    };
    // Codex keys a hook's trust by its group index: replace in place.
    let mut placed = false;
    let mut merged = Vec::with_capacity(entries.len() + 1);
    for entry in entries {
        if !hook_entry_matches_marker(&entry, pixel_marker) {
            merged.push(entry);
        } else if !placed {
            placed = true;
            merged.push(pixel_entry.clone());
        }
    }
    if !placed {
        merged.push(pixel_entry);
    }
    let entries = merged;
    serde_json::Value::Array(entries)
}

pub fn hook_entry_matches_marker(entry: &serde_json::Value, marker: &str) -> bool {
    entry
        .get("hooks")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.contains(marker))
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook_entry(command: &str) -> serde_json::Value {
        serde_json::json!({ "matcher": "Bash", "hooks": [{ "type": "command", "command": command }] })
    }

    /// The side build is known by its installed name wherever it sits; a
    /// checkout build run by hand, or a tool whose name merely starts with
    /// `pixel-dev`, is not one and keeps the managed binary's rules.
    #[test]
    fn is_side_build_should_recognise_the_pixel_dev_name_only() {
        assert!(is_side_build(Path::new("/home/u/.local/bin/pixel-dev")));
        assert!(is_side_build(Path::new("/tmp/copy/pixel-dev")));
        assert!(!is_side_build(Path::new("/home/u/.local/bin/pixel")));
        assert!(!is_side_build(Path::new(
            "/home/u/code/pixel/target/dev-release/pixel"
        )));
        assert!(!is_side_build(Path::new("/opt/pixel-dev-helper")));
    }

    #[test]
    fn backup_if_changing_writes_a_copy_only_when_the_content_differs() {
        let dir = std::env::temp_dir().join(format!("pixel-backup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("settings.json");
        assert_eq!(
            backup_if_changing(&file, b"new").unwrap(),
            None,
            "missing file"
        );
        fs::write(&file, b"old").unwrap();
        assert_eq!(
            backup_if_changing(&file, b"old").unwrap(),
            None,
            "identical content"
        );
        let backup = backup_if_changing(&file, b"new")
            .unwrap()
            .expect("backup written");
        assert_eq!(fs::read(&backup).unwrap(), b"old");
        let name = backup.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("settings.json.pixel-bak."), "{name}");
        assert_eq!(backup.parent(), Some(dir.as_path()));
        let second = backup_if_changing(&file, b"new").unwrap().unwrap();
        assert_ne!(second, backup, "every backup gets a fresh name");
        let _ = fs::remove_dir_all(&dir);
    }

    fn flat(command: &str) -> serde_json::Value {
        serde_json::json!({ "command": command })
    }

    #[test]
    fn hook_entry_matches_marker_looks_inside_the_nested_hooks_commands() {
        assert!(hook_entry_matches_marker(
            &hook_entry("pixel run-hook guard"),
            "pixel run-hook"
        ));
        assert!(!hook_entry_matches_marker(
            &hook_entry("lint"),
            "pixel run-hook"
        ));
        assert!(
            !hook_entry_matches_marker(&flat("pixel run-hook guard"), "pixel run-hook"),
            "flat entries have no nested hooks"
        );
        assert!(!hook_entry_matches_marker(
            &serde_json::json!({"hooks": "x"}),
            "pixel run-hook"
        ));
    }

    fn write_config(dir: &Path, text: &str) {
        fs::create_dir_all(dir.join(".pixel")).unwrap();
        fs::write(dir.join(".pixel/config.yaml"), text).unwrap();
    }

    #[test]
    fn brief_parts_should_read_the_repo_layer_then_the_global_one_then_default() {
        let home = tempfile::TempDir::new().unwrap();
        let repo = tempfile::TempDir::new().unwrap();
        assert_eq!(brief_parts(home.path(), None), BRIEF_PARTS_DEFAULT);
        assert_eq!(BRIEF_PARTS_DEFAULT, 3);
        assert_eq!(
            brief_parts(home.path(), Some(repo.path())),
            BRIEF_PARTS_DEFAULT
        );
        write_config(home.path(), "brief_parts: 5\n");
        assert_eq!(brief_parts(home.path(), None), 5);
        assert_eq!(brief_parts(home.path(), Some(repo.path())), 5);
        write_config(repo.path(), "brief_parts: 2\n");
        assert_eq!(brief_parts(home.path(), Some(repo.path())), 2);
        assert_eq!(
            brief_parts(home.path(), None),
            5,
            "a repository layer is for a repository install only"
        );
        write_config(repo.path(), "metrics: \"on\"\n");
        assert_eq!(
            brief_parts(home.path(), Some(repo.path())),
            5,
            "a repository layer that does not set it defers to the global one"
        );
    }

    #[test]
    fn load_config_should_read_a_mapping_and_refuse_what_is_not_one() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(load_config(dir.path()).unwrap(), serde_json::json!({}));
        write_config(dir.path(), "");
        assert_eq!(load_config(dir.path()).unwrap(), serde_json::json!({}));
        write_config(dir.path(), "metrics: \"off\"\nbrief_parts: 2\n");
        assert_eq!(
            load_config(dir.path()).unwrap(),
            serde_json::json!({"metrics": "off", "brief_parts": 2})
        );
        write_config(dir.path(), "- a\n- b\n");
        assert!(
            load_config(dir.path())
                .unwrap_err()
                .contains("must be a mapping")
        );
        write_config(dir.path(), "a: [\n");
        assert!(
            load_config(dir.path())
                .unwrap_err()
                .contains("expected a YAML mapping")
        );
    }

    #[test]
    fn brief_parts_should_clamp_into_range_and_ignore_what_is_not_a_count() {
        let home = tempfile::TempDir::new().unwrap();
        for (text, expected) in [
            ("brief_parts: 1\n", 1),
            ("brief_parts: 0\n", 1),
            ("brief_parts: 8\n", 8),
            ("brief_parts: 9\n", BRIEF_PARTS_MAX),
            ("brief_parts: 4000\n", BRIEF_PARTS_MAX),
            ("brief_parts: -2\n", BRIEF_PARTS_DEFAULT),
            ("brief_parts: 2.5\n", BRIEF_PARTS_DEFAULT),
            ("brief_parts: many\n", BRIEF_PARTS_DEFAULT),
            ("brief_parts: [1]\n", BRIEF_PARTS_DEFAULT),
            ("brief_parts: [\n", BRIEF_PARTS_DEFAULT),
            ("", BRIEF_PARTS_DEFAULT),
        ] {
            write_config(home.path(), text);
            assert_eq!(brief_parts(home.path(), None), expected, "{text:?}");
        }
    }
}

#[cfg(test)]
mod contract_tests;
