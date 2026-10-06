// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Agent-config rewrite helpers for `pixel install` / `pixel migrate`.
//!
//! Finds the Claude/agent config files (CLAUDE.md, AGENTS.md, settings.json),
//! applies the managed-marker wrapping (`<!-- pixel:managed:begin/end -->`),
//! deletes stale GitNexus / codebase-memory blocks, and scrubs settings.json
//! entries that point at the old guard hook.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

/// Managed-marker begin tag. Everything between this and [`MANAGED_END`] is
/// owned by pixel and rewritten on every `pixel install`.
pub const MANAGED_BEGIN: &str = "<!-- pixel:managed:begin -->";
/// Managed-marker end tag. See [`MANAGED_BEGIN`].
pub const MANAGED_END: &str = "<!-- pixel:managed:end -->";

/// Substrings that identify a stale GitNexus / codebase-memory block that
/// must be deleted from agent config during install.
const STALE_BLOCK_MARKERS: &[&str] =
    &["gitnexus", "GitNexus", "codebase-memory", "codebase memory"];

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

/// The Claude hooks directory (relative to home).
pub const CLAUDE_HOOKS_DIR: &str = ".claude/hooks";
/// The guard script name used before the `gitpixel` → `pixel` rename; still
/// recognised so an install or uninstall cleans machines that carry it.
pub const OLD_GUARD_HOOK: &str = "gitpixel-targets-guard";
/// The guard hook path.
pub const GUARD_HOOK: &str = "pixel-targets-guard";
/// The SessionStart hook path.
pub const SESSION_START_HOOK: &str = "pixel-session-start";
/// The UserPromptSubmit hook path (task boundary detector).
pub const PROMPT_SUBMIT_HOOK: &str = "pixel-prompt-submit";
/// The PostCompact hook path (targets manifest re-injection).
/// NB: the Claude Code event is `PostCompact` — `PostCompaction` is not a
/// valid event name and is silently ignored if written into settings.json.
pub const POST_COMPACTION_HOOK: &str = "pixel-post-compaction";

/// Timeout in seconds for pixel run-hook entries written into agent configs.
/// The pixel binary is ~48MB; a cold start can take a few seconds on first
/// invocation. Without an explicit timeout, harnesses like Codex apply a
/// short default and SIGKILL the hook (exit 137), breaking the guard.
/// 10s is generous enough for cold starts while still bounding hang risk.
pub const HOOK_TIMEOUT: u64 = 10;

/// The canonical pixel usage-rule file (relative to home). This is the real,
/// full rule text (the five mandatory scenarios, the doctrine, the git-op
/// table) that `pixel install` embeds into the managed CLAUDE.md/AGENTS.md
/// block — not the short 3-line summary. It lives outside the repo so the
/// rules can be edited without a rebuild.
pub const PIXEL_RULES_REL: &str = ".agent-config/rules/pixel.md";

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

/// The zcode config file (relative to home). zcode is a Claude Code variant
/// that uses the same hooks format as Claude (event `PreToolUse`,
/// `SessionStart`) under `~/.zcode/cli/config.json` → `hooks.events.<Event>`.
pub const ZCODE_CONFIG_FILE: &str = ".zcode/cli/config.json";

/// The Cursor hooks file (relative to home). Cursor uses a FLAT per-event
/// array (`hooks.<event>` is `[{command, matcher?, ...}, ...]` directly —
/// no nested `hooks` sub-array like Claude/Codex/Devin/Gemini/zcode) and
/// its own event name `preToolUse` (not `PreToolUse`). Verified against
/// the installed `cursor-agent` bundle: the `preToolUse` hook-script stdin
/// is `{conversation_id, generation_id, model, tool_name, tool_input,
/// tool_use_id, cwd}` — no `hook_event_name` field, which `pixel hook
/// guard` treats as an implicit PreToolUse (see `guard::is_guard_event`).
/// Exit code 2 blocks, same convention as Claude/Codex, per Cursor's own
/// `create-hook` skill docs — so the guard binary needs no Cursor-specific
/// output path, only the payload-shape and matcher additions above.
pub const CURSOR_HOOKS_FILE: &str = ".cursor/hooks.json";

/// The pi config directory (relative to home). The repository extension uses
/// pi's `tool_call` event to intercept native calls before execution.
pub const PI_CONFIG_DIR: &str = ".pi/agent";
/// The pi settings file (relative to home).
pub const PI_SETTINGS_FILE: &str = ".pi/agent/settings.json";

/// PreToolUse matcher covering Claude, Devin, Codex, Gemini, zcode, Cursor,
/// and Antigravity tool names.
/// Claude:       Bash, Read, Grep, Glob, Edit, MultiEdit, NotebookEdit, Write
/// Devin:        exec, read, grep, find_file_by_name, glob, edit, write, notebook_read, notebook_edit
/// Codex:        shell, unified_exec, local_shell, bash, read, write, edit, apply_patch, glob
///               (shell + unified_exec are the names Codex 0.146.1 actually emits;
///                omitting them left every Codex grep/find/cat unguarded)
/// Gemini:       bash, execute, run_shell_command, read, read_file, write, write_file, edit, grep, glob, search
/// zcode:        same tool names as Claude (Claude Code variant)
/// Cursor:       Shell, Read, Write (Read/Write already covered; Shell is Cursor-only),
///               edit_file, file_search (Cursor's composer-mode tools)
/// Antigravity:  run_command (bash), view_file (read), replace_file_content (edit),
///               write_to_file (write), grep_search (grep), find_by_name (find), list_dir (ls)
/// pi:           read, bash, edit, write, grep, find, ls (handled by its own
///               repository `tool_call` extension, not this hook matcher)
pub const GUARD_MATCHER: &str = "Bash|Read|Grep|Glob|Edit|MultiEdit|NotebookEdit|Write|\
     exec|read|grep|find_file_by_name|glob|edit|write|notebook_read|notebook_edit|\
     bash|apply_patch|read_file|write_file|execute|run_shell_command|search|\
     find|ls|Shell|\
     shell|unified_exec|local_shell|\
     run_command|view_file|replace_file_content|write_to_file|grep_search|find_by_name|list_dir|\
     file_search|edit_file";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid settings.json at {path}: {reason}")]
    InvalidSettings { path: PathBuf, reason: String },
}

pub type Result<T> = std::result::Result<T, ConfigError>;

/// Outcome of rewriting one agent-config file.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RewriteOutcome {
    pub path: PathBuf,
    /// True if the file existed and was rewritten. In dry-run mode this
    /// reflects what WOULD happen; no write is performed.
    pub rewritten: bool,
    /// True if the file already carried the managed markers (idempotent re-run).
    pub already_managed: bool,
    /// Number of stale GitNexus / codebase-memory blocks removed.
    pub stale_blocks_removed: usize,
    /// True if the resulting content actually differs from what is on disk
    /// today (i.e. a real write would change something). Always computed,
    /// even in dry-run mode.
    pub would_change: bool,
    /// Path to the timestamped backup written before the destructive
    /// rewrite, if the file existed and its content was about to change.
    /// Never set in dry-run mode (nothing is written).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<PathBuf>,
}

/// Outcome of scrubbing one settings.json.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScrubOutcome {
    pub path: PathBuf,
    pub existed: bool,
    /// Number of MCP-server entries removed (usable-git/gitpixel/sniper).
    pub mcp_servers_removed: usize,
    /// Path to the timestamped backup written before the destructive
    /// rewrite, if anything was actually removed. Never set in dry-run mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<PathBuf>,
}

/// The Markdown-style agent-config files `pixel install` manages with
/// managed-marker rewriting, in a stable order. Returns only paths that
/// exist on disk.
///
/// Deliberately excludes `.claude/settings.json`: that file is JSON, not
/// Markdown, and is handled separately by [`scrub_settings_json`]. It must
/// never be run through [`rewrite_agent_config`], which writes HTML-comment
/// managed markers into the file body — doing so would corrupt settings.json
/// into invalid JSON.
pub fn find_agent_configs(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for rel in [
        "CLAUDE.md",
        "AGENTS.md",
        ".claude/CLAUDE.md",
        ".claude/AGENTS.md",
    ] {
        let p = home.join(rel);
        if p.is_file() {
            out.push(p);
        }
    }
    out
}

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
    fs::write(&backup_path, &current)?;
    Ok(Some(backup_path))
}

/// Wrap `managed` between the managed markers, replacing any existing managed
/// block in `original` and deleting stale GitNexus / codebase-memory blocks.
///
/// Idempotent: if `original` already contains a managed block, only the block
/// body is replaced; the surrounding text is preserved.
pub fn apply_managed_markers(original: &str, managed: &str) -> String {
    let block = format!("{MANAGED_BEGIN}\n{managed}\n{MANAGED_END}\n");
    let (cleaned, _) = strip_stale_blocks(original);
    if let Some(start) = cleaned.find(MANAGED_BEGIN) {
        let head = &cleaned[..start];
        let tail = match cleaned[start..].find(MANAGED_END) {
            Some(end) => {
                let after = &cleaned[start..][end + MANAGED_END.len()..];
                // `block` already supplies exactly one trailing newline after
                // MANAGED_END. The single newline immediately following the
                // OLD end marker is that same canonical newline, not user
                // content — strip exactly one so it isn't duplicated on every
                // re-install (previously this grew the file by one byte per
                // `pixel install` run: 295, 296, 297, ... forever). Anything
                // beyond that first newline is genuine trailing content and
                // is preserved untouched.
                after.strip_prefix('\n').unwrap_or(after)
            }
            None => "",
        };
        let mut out = String::with_capacity(head.len() + block.len() + tail.len());
        out.push_str(head);
        out.push_str(&block);
        out.push_str(tail);
        out
    } else {
        let mut out = cleaned;
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&block);
        out
    }
}

/// Remove Markdown *sections* that are genuinely GitNexus/codebase-memory
/// generated blocks — bounded by a header line (`#`, `##`, ... followed by a
/// space) that itself contains a stale-block marker, through to (but
/// excluding) the next header at the same or shallower nesting depth (or
/// EOF). Returns the cleaned text and the number of blocks removed.
///
/// Deliberately does **not** delete a bare incidental mention of these words
/// inside otherwise-unrelated prose. This replaces a prior, confirmed
/// false-positive: naive whole-line substring deletion would have deleted a
/// real, hand-authored rule line in a real `~/.claude/CLAUDE.md` —
/// `"...override every other discovery protocol (codebase-memory, gitnexus,
/// generic exploration)."` — which merely *lists* those tools among others
/// it deprioritizes and is not itself a GitNexus block. Only a genuine
/// section header announcing a GitNexus-authored block triggers removal,
/// matching how a real generated block actually looks (e.g. this very
/// project's own `# GitNexus — Code Intelligence` section, a full H1 with
/// several subsections) — and removes the section as one clean unit instead
/// of leaving other, non-matching lines of that same section behind as
/// orphaned, mangled content.
pub fn strip_stale_blocks(text: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut removed = 0usize;
    // Depth of the stale section being dropped, until a header at the same
    // or a shallower depth closes it.
    let mut dropping: Option<usize> = None;
    for line in text.lines() {
        if let Some(depth) = dropping {
            if !header_depth(line).is_some_and(|d| d <= depth) {
                continue;
            }
            dropping = None;
        }
        if let Some(depth) = stale_block_header_depth(line) {
            removed += 1;
            dropping = Some(depth);
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    (out, removed)
}

/// If `line` is a Markdown header that also contains a stale-block marker,
/// return its header depth (1 for `#`, 2 for `##`, ...). Otherwise `None`.
fn stale_block_header_depth(line: &str) -> Option<usize> {
    let depth = header_depth(line)?;
    STALE_BLOCK_MARKERS
        .iter()
        .any(|m| line.contains(m))
        .then_some(depth)
}

/// If `line` is a Markdown header (one or more leading `#`, immediately
/// followed by a space or end-of-line), return its depth. Otherwise `None`.
fn header_depth(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    (rest.is_empty() || rest.starts_with(' ')).then_some(hashes)
}

/// Rewrite one agent-config file with the managed block. Creates the file if
/// it does not exist. Returns the outcome.
///
/// When `dry_run` is true, computes the exact same outcome (including
/// `stale_blocks_removed` and `would_change`) but performs no filesystem
/// writes and creates no directories or backups.
pub fn rewrite_agent_config(path: &Path, managed: &str, dry_run: bool) -> Result<RewriteOutcome> {
    let original = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let already_managed = original.contains(MANAGED_BEGIN);
    let (cleaned, stale_blocks_removed) = strip_stale_blocks(&original);
    let rewritten = apply_managed_markers(&cleaned, managed);
    let would_change = rewritten != original;

    if dry_run {
        return Ok(RewriteOutcome {
            path: path.to_path_buf(),
            rewritten: false,
            already_managed,
            stale_blocks_removed,
            would_change,
            backup_path: None,
        });
    }

    let backup_path = backup_if_changing(path, rewritten.as_bytes())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, rewritten)?;
    Ok(RewriteOutcome {
        path: path.to_path_buf(),
        rewritten: true,
        already_managed,
        stale_blocks_removed,
        would_change,
        backup_path,
    })
}

/// MCP server names that must be removed during install (the deprecated
/// tools). `pixel` itself is the only one kept.
pub const DEPRECATED_MCP_SERVERS: &[&str] = &["usable-git", "gitpixel", "sniper"];

/// Per-tool rule-file basenames belonging to tools pixel retired. Each of
/// these is a standing instruction to use `usable-git`, `gitpixel`,
/// `gitnexus`, or the hard `sniper` fence — every one of which pixel
/// replaced. Left on disk they compete with the pixel rule inside the same
/// agent's rule set: Devin, for instance, advertises every file under
/// `~/.devin/rules/` to the model as an available rule it may read, so a
/// retired rule keeps offering the model a retired tool.
pub const DEPRECATED_RULE_FILES: &[&str] =
    &["usable-git.md", "gitpixel.md", "gitnexus.md", "sniper.md"];

/// Directories (relative to home) that agent CLIs load per-tool Markdown
/// rule files from. Scrubbed for [`DEPRECATED_RULE_FILES`] on every
/// install. `.agent-config/**` is included because it is the source those
/// per-tool directories are regenerated from — scrubbing only the
/// destinations would let the next `build-agent-config` put them back.
pub const AGENT_RULES_DIRS: &[&str] = &[
    ".devin/rules",
    ".cline/rules",
    ".claude/rules",
    ".codex/rules",
    ".cursor/rules",
    ".gemini/rules",
    ".agent-config/rules",
    ".agent-config/.devin/rules",
];

/// Every [`DEPRECATED_RULE_FILES`] entry present under any
/// [`AGENT_RULES_DIRS`] directory, in scan order.
pub fn find_deprecated_rule_files(home: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in AGENT_RULES_DIRS {
        let dir = home.join(dir);
        if !dir.is_dir() {
            continue;
        }
        for name in DEPRECATED_RULE_FILES {
            let path = dir.join(name);
            if path.is_file() {
                found.push(path);
            }
        }
    }
    found
}

/// Remove every retired-tool rule file found by
/// [`find_deprecated_rule_files`], backing each one up first. Returns the
/// paths removed (or, under `dry_run`, the paths that would be removed).
pub fn scrub_deprecated_rule_files(home: &Path, dry_run: bool) -> Result<Vec<PathBuf>> {
    let found = find_deprecated_rule_files(home);
    if dry_run {
        return Ok(found);
    }
    for path in &found {
        // Back up unconditionally: the file is about to disappear, so
        // there is no "content already matches" case to skip.
        let current = fs::read(path)?;
        backup_if_changing(path, &sentinel_differing_from(&current))?;
        fs::remove_file(path)?;
    }
    Ok(found)
}

/// A byte string guaranteed to differ from `current`, so
/// [`backup_if_changing`] always writes the backup.
fn sentinel_differing_from(current: &[u8]) -> Vec<u8> {
    let mut sentinel = current.to_vec();
    sentinel.push(0);
    sentinel
}

/// Remove deprecated MCP-server entries from a Claude `settings.json`.
/// Returns the scrub outcome.
///
/// When `dry_run` is true, computes the same removal counts but performs no
/// write and no backup.
///
/// The deprecated usable-git/gitpixel/sniper MCP server entries are removed
/// unconditionally — pixel replaces them via Bash + the guard hook, not via
/// MCP.
pub fn scrub_settings_json(path: &Path, dry_run: bool) -> Result<ScrubOutcome> {
    let existed = path.is_file();
    if !existed {
        return Ok(ScrubOutcome {
            path: path.to_path_buf(),
            existed: false,
            mcp_servers_removed: 0,
            backup_path: None,
        });
    }
    let raw = fs::read_to_string(path)?;
    let mut value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| ConfigError::InvalidSettings {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;

    let mut mcp_servers_removed = 0usize;
    if let Some(servers) = value
        .get_mut("mcpServers")
        .and_then(serde_json::Value::as_object_mut)
    {
        for name in DEPRECATED_MCP_SERVERS {
            if servers.remove(*name).is_some() {
                mcp_servers_removed += 1;
            }
        }
    }

    let mut backup_path = None;
    if !dry_run && mcp_servers_removed > 0 {
        let serialized = format!("{}\n", serde_json::to_string_pretty(&value)?);
        backup_path = backup_if_changing(path, serialized.as_bytes())?;
        fs::write(path, serialized)?;
    }

    Ok(ScrubOutcome {
        path: path.to_path_buf(),
        existed: true,
        mcp_servers_removed,
        backup_path,
    })
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

/// Same idempotent append-and-dedupe as [`merge_hook_entry`], for Cursor's
/// FLAT hooks.json schema: each array entry carries `command` directly
/// (no nested `hooks` sub-array), so the marker match looks at
/// `entry.command` instead of `entry.hooks[].command`.
pub fn merge_flat_hook_entry(
    existing: Option<&serde_json::Value>,
    pixel_marker: &str,
    pixel_entry: serde_json::Value,
) -> serde_json::Value {
    let mut entries: Vec<serde_json::Value> = match existing {
        Some(serde_json::Value::Array(arr)) => arr.clone(),
        Some(other) => vec![other.clone()],
        None => Vec::new(),
    };
    entries.retain(|entry| {
        entry
            .get("command")
            .and_then(|c| c.as_str())
            .is_none_or(|c| !c.contains(pixel_marker))
    });
    entries.push(pixel_entry);
    serde_json::Value::Array(entries)
}

/// Remove the managed block (everything between and including
/// [`MANAGED_BEGIN`] and [`MANAGED_END`]) from `text`. If no managed block
/// is present, returns `text` unchanged. Also strips the single canonical
/// trailing newline after the end marker, matching
/// [`apply_managed_markers`]'s convention.
pub fn strip_managed_block(text: &str) -> String {
    let Some(start) = text.find(MANAGED_BEGIN) else {
        return text.to_string();
    };
    let managed_and_tail = &text[start..];
    let Some(end) = managed_and_tail.find(MANAGED_END) else {
        return text.to_string();
    };
    let end_pos = end + MANAGED_END.len();
    let after = managed_and_tail[end_pos..]
        .strip_prefix('\n')
        .unwrap_or(&managed_and_tail[end_pos..]);
    let mut result = String::with_capacity(start + after.len());
    result.push_str(&text[..start]);
    result.push_str(after);
    result
}

/// Remove every entry in a `hooks.<Event>` JSON array whose nested
/// `hooks[].command` contains `marker`. Returns the filtered array (or the
/// original value unchanged if it is not an array). The inverse of
/// [`merge_hook_entry`].
pub fn remove_hook_entries(existing: &serde_json::Value, marker: &str) -> serde_json::Value {
    match existing {
        serde_json::Value::Array(arr) => {
            let filtered: Vec<serde_json::Value> = arr
                .iter()
                .filter_map(|entry| {
                    let Some(hooks) = entry.get("hooks").and_then(serde_json::Value::as_array)
                    else {
                        return Some(entry.clone());
                    };

                    let has_match = hooks.iter().any(|hook| {
                        hook.get("command")
                            .and_then(|command| command.as_str())
                            .is_some_and(|command| command.contains(marker))
                    });
                    if !has_match {
                        return Some(entry.clone());
                    }

                    let mut cleaned = entry.clone();
                    let Some(cleaned_hooks) = cleaned
                        .get_mut("hooks")
                        .and_then(serde_json::Value::as_array_mut)
                    else {
                        return Some(entry.clone());
                    };
                    cleaned_hooks.retain(|hook| {
                        hook.get("command")
                            .and_then(|command| command.as_str())
                            .is_none_or(|command| !command.contains(marker))
                    });

                    // Preserve the outer matcher when it still contains any
                    // user hook. A Pixel guard must never make us discard a
                    // co-located, unrelated command.
                    (!cleaned_hooks.is_empty()).then_some(cleaned)
                })
                .collect();
            serde_json::Value::Array(filtered)
        }
        _ => existing.clone(),
    }
}

/// Same as [`remove_hook_entries`] but for Cursor's FLAT hooks.json schema
/// where each entry carries `command` directly (no nested `hooks` array).
/// The inverse of [`merge_flat_hook_entry`].
pub fn remove_flat_hook_entries(existing: &serde_json::Value, marker: &str) -> serde_json::Value {
    match existing {
        serde_json::Value::Array(arr) => {
            let filtered: Vec<serde_json::Value> = arr
                .iter()
                .filter(|e| {
                    e.get("command")
                        .and_then(|c| c.as_str())
                        .is_none_or(|c| !c.contains(marker))
                })
                .cloned()
                .collect();
            serde_json::Value::Array(filtered)
        }
        _ => existing.clone(),
    }
}

/// Remove Pixel's blocking guard from every nested hook event while keeping
/// all unrelated hook entries intact. Returns the number of event arrays that
/// changed.
pub fn remove_guard_hook_entries(hooks: &mut serde_json::Map<String, serde_json::Value>) -> usize {
    let mut changed = 0usize;
    let event_keys: Vec<String> = hooks.keys().cloned().collect();
    for event in event_keys {
        let Some(existing) = hooks.get(&event).cloned() else {
            continue;
        };
        let mut filtered = existing.clone();
        filtered = remove_hook_entries(&filtered, GUARD_HOOK);
        if filtered != existing {
            changed += 1;
            if filtered.as_array().is_some_and(Vec::is_empty) {
                hooks.remove(&event);
            } else {
                hooks.insert(event, filtered);
            }
        }
    }
    changed
}

/// Remove Pixel's blocking guard from a flat hook-event map such as Cursor's
/// `hooks.preToolUse`, preserving every unrelated command.
pub fn remove_flat_guard_hook_entries(
    hooks: &mut serde_json::Map<String, serde_json::Value>,
) -> usize {
    let mut changed = 0usize;
    let event_keys: Vec<String> = hooks.keys().cloned().collect();
    for event in event_keys {
        let Some(existing) = hooks.get(&event).cloned() else {
            continue;
        };
        let mut filtered = existing.clone();
        filtered = remove_flat_hook_entries(&filtered, GUARD_HOOK);
        if filtered != existing {
            changed += 1;
            if filtered.as_array().is_some_and(Vec::is_empty) {
                hooks.remove(&event);
            } else {
                hooks.insert(event, filtered);
            }
        }
    }
    changed
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
    fn removing_pixel_guard_preserves_co_located_user_hooks() {
        let mut hooks = serde_json::Map::new();
        hooks.insert(
            "PreToolUse".to_string(),
            serde_json::json!([{
                "matcher": "Bash",
                "hooks": [
                    { "type": "command", "command": "~/.claude/hooks/pixel-targets-guard" },
                    { "type": "command", "command": "~/.claude/hooks/user-hook" }
                ]
            }]),
        );

        assert_eq!(remove_guard_hook_entries(&mut hooks), 1);
        assert_eq!(
            hooks["PreToolUse"][0]["hooks"][0]["command"],
            "~/.claude/hooks/user-hook"
        );
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
    fn merge_flat_hook_entry_replaces_the_pixel_entry_and_keeps_the_rest() {
        let existing = serde_json::json!([flat("lint"), flat("pixel run-hook guard --old")]);
        let merged = merge_flat_hook_entry(
            Some(&existing),
            "pixel run-hook",
            flat("pixel run-hook guard"),
        );
        assert_eq!(
            merged,
            serde_json::json!([flat("lint"), flat("pixel run-hook guard")])
        );
        assert_eq!(
            merge_flat_hook_entry(None, "pixel run-hook", flat("pixel run-hook guard")),
            serde_json::json!([flat("pixel run-hook guard")])
        );
        // A scalar entry is wrapped, not dropped.
        assert_eq!(
            merge_flat_hook_entry(
                Some(&flat("lint")),
                "pixel run-hook",
                flat("pixel run-hook guard")
            ),
            serde_json::json!([flat("lint"), flat("pixel run-hook guard")])
        );
    }

    #[test]
    fn remove_flat_hook_entries_drops_only_the_marked_commands() {
        let existing = serde_json::json!([flat("lint"), flat("pixel run-hook guard"), {"note": 1}]);
        assert_eq!(
            remove_flat_hook_entries(&existing, "pixel run-hook"),
            serde_json::json!([flat("lint"), {"note": 1}])
        );
        let scalar = serde_json::json!("not an array");
        assert_eq!(remove_flat_hook_entries(&scalar, "pixel run-hook"), scalar);
    }

    #[test]
    fn remove_flat_guard_hook_entries_counts_changed_events_and_drops_emptied_ones() {
        let mut hooks = serde_json::Map::new();
        hooks.insert(
            "preToolUse".into(),
            serde_json::json!([flat("lint"), flat(&format!("sh {GUARD_HOOK}"))]),
        );
        hooks.insert(
            "postToolUse".into(),
            serde_json::json!([flat(&format!("sh {GUARD_HOOK}"))]),
        );
        hooks.insert("stop".into(), serde_json::json!([flat("say done")]));
        assert_eq!(remove_flat_guard_hook_entries(&mut hooks), 2);
        assert_eq!(
            hooks.get("preToolUse"),
            Some(&serde_json::json!([flat("lint")]))
        );
        assert!(
            !hooks.contains_key("postToolUse"),
            "emptied event is removed"
        );
        assert_eq!(
            hooks.get("stop"),
            Some(&serde_json::json!([flat("say done")]))
        );
        assert_eq!(remove_flat_guard_hook_entries(&mut hooks), 0, "idempotent");
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
}

#[cfg(test)]
mod contract_tests;
