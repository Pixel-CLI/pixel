// SPDX-FileCopyrightText: The Pixel contributors
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

/// Timeout in seconds for pixel run-hook entries written into agent configs.
/// The pixel binary is ~48MB; a cold start can take a few seconds on first
/// invocation. Without an explicit timeout, harnesses like Codex apply a
/// short default and SIGKILL the hook (exit 137), breaking the guard.
/// 10s is generous enough for cold starts while still bounding hang risk.
pub const HOOK_TIMEOUT: u64 = 10;

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
    repo.into_iter()
        .chain([home])
        .find_map(|dir| {
            crate::doctor::load_pixel_config(dir)
                .ok()?
                .get(BRIEF_PARTS_KEY)?
                .as_u64()
        })
        .map_or(BRIEF_PARTS_DEFAULT, |count| {
            u8::try_from(count.clamp(1, u64::from(BRIEF_PARTS_MAX))).unwrap_or(BRIEF_PARTS_MAX)
        })
}

/// Settings key of the time one prompt brief may take.
pub const BRIEF_WINDOW_KEY: &str = "brief_window_ms";
/// Environment override of [`BRIEF_WINDOW_KEY`], in milliseconds; it wins over both config layers.
pub const BRIEF_WINDOW_ENV: &str = "PIXEL_BRIEF_WINDOW_MS";
/// Milliseconds a prompt-submit hook gives the brief: a loaded machine (a dozen agent sessions on one laptop)
/// slows every lookup at once, and a brief cut to nothing by a 750 ms window answered nothing at all. The hook
/// is killed by its host after [`HOOK_TIMEOUT`] seconds; this stays well under it.
pub const BRIEF_WINDOW_HOOK_DEFAULT_MS: u64 = 3_000;
/// Milliseconds the `pixel brief` command gives the brief: OpenCode's extension kills the command after 1.2 s.
pub const BRIEF_WINDOW_COMMAND_DEFAULT_MS: u64 = 750;
/// Longest window the `brief` command gets whatever the setting says: OpenCode kills the command after 1.2 s.
pub const BRIEF_WINDOW_COMMAND_MAX_MS: u64 = 1_000;
/// Shortest window a setting may ask for.
pub const BRIEF_WINDOW_MIN_MS: u64 = 250;
/// Longest window a setting may ask for: the hook decision is abandoned after six seconds
/// (`DECISION_TIMEOUT` in `task_hook.rs`), itself under the [`HOOK_TIMEOUT`] the host enforces, and the window
/// stays 1.5 s below that so the brief can still be printed.
pub const BRIEF_WINDOW_MAX_MS: u64 = 4_500;
const _: () = assert!(BRIEF_WINDOW_MAX_MS + 1_500 + 4_000 <= HOOK_TIMEOUT * 1_000);
const _: () = assert!(BRIEF_WINDOW_MIN_MS <= BRIEF_WINDOW_COMMAND_DEFAULT_MS);
const _: () = assert!(BRIEF_WINDOW_HOOK_DEFAULT_MS <= BRIEF_WINDOW_MAX_MS);

/// How many milliseconds the brief may take: `env` (the value of [`BRIEF_WINDOW_ENV`]) when it is a whole number,
/// else `brief_window_ms` in the repository's `.pixel/config.yaml` when `repo` is given and sets it, else the
/// global one, else `default_ms`. The result is clamped into [`BRIEF_WINDOW_MIN_MS`]..=[`BRIEF_WINDOW_MAX_MS`]
/// so no setting can outlast the hook that runs the brief; what is not a whole number counts as unset.
pub fn brief_window_ms(
    home: &Path,
    repo: Option<&Path>,
    env: Option<&str>,
    default_ms: u64,
) -> u64 {
    env.and_then(|text| text.trim().parse::<u64>().ok())
        .or_else(|| {
            repo.into_iter().chain([home]).find_map(|dir| {
                crate::doctor::load_pixel_config(dir)
                    .ok()?
                    .get(BRIEF_WINDOW_KEY)?
                    .as_u64()
            })
        })
        .unwrap_or(default_ms)
        .clamp(BRIEF_WINDOW_MIN_MS, BRIEF_WINDOW_MAX_MS)
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
/// before the helpers step runs).
pub const SKILL_ROOTS: &[&str] = &[
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
    let mut entries: Vec<serde_json::Value> = match existing {
        Some(serde_json::Value::Array(arr)) => arr.clone(),
        Some(other) => vec![other.clone()],
        None => Vec::new(),
    };
    entries.retain(|entry| !hook_entry_matches_marker(entry, pixel_marker));
    entries.push(pixel_entry);
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

    #[test]
    fn brief_window_ms_should_prefer_env_then_repo_then_global_then_the_default_and_clamp() {
        let home = tempfile::TempDir::new().unwrap();
        let repo = tempfile::TempDir::new().unwrap();
        let ask = |env: Option<&str>| brief_window_ms(home.path(), Some(repo.path()), env, 3_000);
        assert_eq!(ask(None), 3_000);
        write_config(home.path(), "brief_window_ms: 2000\n");
        assert_eq!(ask(None), 2_000);
        assert_eq!(brief_window_ms(home.path(), None, None, 3_000), 2_000);
        write_config(repo.path(), "brief_window_ms: 1500\n");
        assert_eq!(ask(None), 1_500);
        assert_eq!(
            brief_window_ms(home.path(), None, None, 3_000),
            2_000,
            "a repository layer is for a repository only"
        );
        assert_eq!(
            ask(Some("900")),
            900,
            "the environment wins over both layers"
        );
        assert_eq!(ask(Some(" 4000 ")), 4_000);
        assert_eq!(
            ask(Some("soon")),
            1_500,
            "a value that is not a number counts as unset"
        );
        assert_eq!(ask(Some("-5")), 1_500);
        write_config(repo.path(), "metrics: \"on\"\n");
        assert_eq!(
            ask(None),
            2_000,
            "a layer that does not set it defers to the next"
        );
    }

    #[test]
    fn brief_window_ms_should_never_outlast_the_hook_that_runs_the_brief() {
        let home = tempfile::TempDir::new().unwrap();
        for (env, expected) in [
            ("0", BRIEF_WINDOW_MIN_MS),
            ("100", BRIEF_WINDOW_MIN_MS),
            ("250", 250),
            ("4500", BRIEF_WINDOW_MAX_MS),
            ("4501", BRIEF_WINDOW_MAX_MS),
            ("600000", BRIEF_WINDOW_MAX_MS),
        ] {
            assert_eq!(
                brief_window_ms(home.path(), None, Some(env), 3_000),
                expected,
                "{env}"
            );
        }
        write_config(home.path(), "brief_window_ms: 99999\n");
        assert_eq!(
            brief_window_ms(home.path(), None, None, 3_000),
            BRIEF_WINDOW_MAX_MS
        );
        assert_eq!(
            brief_window_ms(home.path(), None, None, 90_000),
            BRIEF_WINDOW_MAX_MS.min(99_999)
        );
    }
}

#[cfg(test)]
mod contract_tests;
