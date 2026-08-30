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
const STALE_BLOCK_MARKERS: &[&str] = &[
    "gitnexus",
    "GitNexus",
    "codebase-memory",
    "codebase memory",
];

/// The Claude hooks directory (relative to home).
pub const CLAUDE_HOOKS_DIR: &str = ".claude/hooks";
/// The old guard hook path that gets replaced.
pub const OLD_GUARD_HOOK: &str = "gitpixel-targets-guard";
/// The new guard hook path.
pub const GUARD_HOOK: &str = "pixel-targets-guard";
/// The SessionStart hook path.
pub const SESSION_START_HOOK: &str = "pixel-session-start";

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

/// The pi config directory (relative to home). pi uses an extension API with
/// lifecycle events only — no per-tool `PreToolUse` interception. pixel
/// installs rules into pi's memory but cannot wire guard hooks.
pub const PI_CONFIG_DIR: &str = ".pi/agent";
/// The pi settings file (relative to home).
pub const PI_SETTINGS_FILE: &str = ".pi/agent/settings.json";

/// PreToolUse matcher covering Claude, Devin, Codex, Gemini, zcode, and
/// Cursor tool names.
/// Claude:  Bash, Read, Grep, Glob, Edit, MultiEdit, NotebookEdit, Write
/// Devin:   exec, read, grep, find_file_by_name, glob, edit, write, notebook_read, notebook_edit
/// Codex:   bash, read, write, edit, apply_patch, glob
/// Gemini:  bash, execute, run_shell_command, read, read_file, write, write_file, edit, grep, glob, search
/// zcode:   same tool names as Claude (Claude Code variant)
/// Cursor:  Shell, Read, Write (Read/Write already covered above; Shell is
///          Cursor-only — verified against the installed `cursor-agent`
///          bundle's `preToolUse` tool-name mapping)
/// pi:      read, bash, edit, write, grep, find, ls (no PreToolUse hooks — rules only)
pub const GUARD_MATCHER: &str =
    "Bash|Read|Grep|Glob|Edit|MultiEdit|NotebookEdit|Write|\
     exec|read|grep|find_file_by_name|glob|edit|write|notebook_read|notebook_edit|\
     bash|apply_patch|read_file|write_file|execute|run_shell_command|search|\
     find|ls|Shell";

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
    /// Number of old-guard-hook references removed or repointed: either a
    /// (never actually observed in practice) top-level `hooks.<old-name>`
    /// key, or — the real case — a nested `hooks.<Event>[].hooks[].command`
    /// string rewritten from the old guard filename to the new one.
    pub guard_hooks_removed: usize,
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
    for rel in ["CLAUDE.md", "AGENTS.md", ".claude/CLAUDE.md", ".claude/AGENTS.md"] {
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
    let current = match fs::read(path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if current == new_content {
        return Ok(None);
    }
    static BACKUP_SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = BACKUP_SEQ.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
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
        let tail = match cleaned.find(MANAGED_END) {
            Some(end) => {
                let after = &cleaned[end + MANAGED_END.len()..];
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
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::with_capacity(text.len());
    let mut removed = 0usize;
    let mut i = 0usize;
    while i < lines.len() {
        if let Some(depth) = stale_block_header_depth(lines[i]) {
            removed += 1;
            i += 1;
            while i < lines.len() {
                if header_depth(lines[i]).is_some_and(|d| d <= depth) {
                    break;
                }
                i += 1;
            }
            continue;
        }
        out.push_str(lines[i]);
        out.push('\n');
        i += 1;
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

/// Hook names that point at the old guard and must be scrubbed.
pub const DEPRECATED_GUARD_HOOKS: &[&str] = &["gitpixel-targets-guard"];

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

/// Remove deprecated MCP-server entries and old-guard hook entries from a
/// Claude `settings.json`. Returns the scrub outcome.
///
/// When `dry_run` is true, computes the same removal counts but performs no
/// write and no backup.
///
/// The deprecated usable-git/gitpixel/sniper MCP server entries are removed
/// unconditionally — pixel replaces them via Bash + the guard hook, not via
/// MCP. The guard-hook command rewrite is unrelated to MCP registration and
/// always runs.
pub fn scrub_settings_json(path: &Path, dry_run: bool) -> Result<ScrubOutcome> {
    let existed = path.is_file();
    if !existed {
        return Ok(ScrubOutcome {
            path: path.to_path_buf(),
            existed: false,
            mcp_servers_removed: 0,
            guard_hooks_removed: 0,
            backup_path: None,
        });
    }
    let raw = fs::read_to_string(path)?;
    let mut value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| ConfigError::InvalidSettings {
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

    let mut guard_hooks_removed = 0usize;
    if let Some(hooks) = value
        .get_mut("hooks")
        .and_then(serde_json::Value::as_object_mut)
    {
        // Legacy defensive check: if `hooks` ever literally carried a
        // top-level key named after the old guard hook, remove it. In
        // practice real Claude settings never shape hooks this way (event
        // names like "PreToolUse"/"SessionStart" are the only top-level
        // keys), so this is expected to be a no-op — the real fix is the
        // nested command rewrite below.
        for name in DEPRECATED_GUARD_HOOKS {
            if hooks.remove(*name).is_some() {
                guard_hooks_removed += 1;
            }
        }
        // The real fix: the old guard hook is referenced as a `command`
        // string nested inside `hooks.<Event>[].hooks[]` (confirmed against
        // a real `~/.claude/settings.json`, e.g. under `PreToolUse`).
        // Repoint every such command at the new guard hook filename in
        // place, preserving the entry's matcher/timeout/everything else —
        // never delete the entry, since deleting would also drop unrelated
        // fields co-located on the same hook object.
        guard_hooks_removed += rewrite_guard_hook_commands(hooks);
    }

    let mut backup_path = None;
    if !dry_run && (mcp_servers_removed > 0 || guard_hooks_removed > 0) {
        let serialized = format!("{}\n", serde_json::to_string_pretty(&value)?);
        backup_path = backup_if_changing(path, serialized.as_bytes())?;
        fs::write(path, serialized)?;
    }

    Ok(ScrubOutcome {
        path: path.to_path_buf(),
        existed: true,
        mcp_servers_removed,
        guard_hooks_removed,
        backup_path,
    })
}

/// Rewrite every hook `command` string anywhere under `hooks.<Event>[]`
/// that references the old guard-hook filename.
///
/// Under the `PreToolUse` event the command is repointed at the new guard
/// hook filename **in place** — preserving the entry's matcher, timeout,
/// and every other field untouched. Under every *other* event (e.g. the
/// stray legacy `PostToolUse` registration that runs the guard binary per
/// Bash call for nothing) the guard-hook entry is **removed** instead of
/// repointed: the guard is a PreToolUse-only hook, so a registration under
/// any other event is dead weight. Returns the number of command strings
/// rewritten (PreToolUse) plus the number of guard-hook entries removed
/// (non-PreToolUse).
fn rewrite_guard_hook_commands(hooks: &mut serde_json::Map<String, serde_json::Value>) -> usize {
    let mut changed = 0usize;
    for (event, entries) in hooks.iter_mut() {
        let Some(entries) = entries.as_array_mut() else {
            continue;
        };
        if event == "PreToolUse" {
            // Repoint in place, preserving every other field.
            for entry in entries.iter_mut() {
                let Some(inner) = entry.get_mut("hooks").and_then(serde_json::Value::as_array_mut) else {
                    continue;
                };
                for hook in inner {
                    let Some(hook_obj) = hook.as_object_mut() else {
                        continue;
                    };
                    let Some(command) = hook_obj.get("command").and_then(|c| c.as_str()).map(str::to_string)
                    else {
                        continue;
                    };
                    if command.contains(OLD_GUARD_HOOK) {
                        let new_command = command.replace(OLD_GUARD_HOOK, GUARD_HOOK);
                        hook_obj.insert("command".to_string(), serde_json::Value::String(new_command));
                        changed += 1;
                    }
                }
            }
        } else {
            // Non-PreToolUse event: the guard is a PreToolUse-only hook, so
            // any registration under another event is dead weight — remove
            // the guard-hook entries instead of repointing them.
            let mut kept: Vec<serde_json::Value> = Vec::with_capacity(entries.len());
            for mut entry in std::mem::take(entries) {
                let Some(inner) = entry.get_mut("hooks").and_then(serde_json::Value::as_array_mut) else {
                    kept.push(entry);
                    continue;
                };
                inner.retain(|hook| {
                    let references_guard = hook
                        .get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains(OLD_GUARD_HOOK))
                        .unwrap_or(false);
                    if references_guard {
                        changed += 1;
                    }
                    !references_guard
                });
                if inner.is_empty() {
                    // The outer entry carried nothing but the removed guard
                    // hook; drop it too so we don't leave an empty shell.
                    continue;
                }
                kept.push(entry);
            }
            *entries = kept;
        }
    }
    changed
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
            .map(|c| !c.contains(pixel_marker))
            .unwrap_or(true)
    });
    entries.push(pixel_entry);
    serde_json::Value::Array(entries)
}

fn hook_entry_matches_marker(entry: &serde_json::Value, marker: &str) -> bool {
    entry
        .get("hooks")
        .and_then(serde_json::Value::as_array)
        .map(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(|c| c.as_str())
                    .map(|c| c.contains(marker))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook_entry(command: &str) -> serde_json::Value {
        serde_json::json!({ "matcher": "Bash", "hooks": [{ "type": "command", "command": command }] })
    }

    #[test]
    fn non_pretooluse_guard_hooks_are_removed_not_repointed() {
        let mut hooks = serde_json::Map::new();
        hooks.insert(
            "PreToolUse".to_string(),
            serde_json::Value::Array(vec![hook_entry("~/.claude/hooks/gitpixel-targets-guard")]),
        );
        hooks.insert(
            "PostToolUse".to_string(),
            serde_json::Value::Array(vec![hook_entry("~/.claude/hooks/gitpixel-targets-guard")]),
        );
        hooks.insert(
            "SessionStart".to_string(),
            serde_json::Value::Array(vec![hook_entry("~/.claude/hooks/pixel-session-start")]),
        );

        let changed = rewrite_guard_hook_commands(&mut hooks);

        // PreToolUse guard command repointed to the new filename.
        let pre = hooks["PreToolUse"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(pre, "~/.claude/hooks/pixel-targets-guard");
        // PostToolUse guard entry removed entirely (empty array left behind).
        assert_eq!(hooks["PostToolUse"].as_array().unwrap().len(), 0);
        // Unrelated SessionStart entry untouched.
        let session = hooks["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(session, "~/.claude/hooks/pixel-session-start");

        // One repoint (PreToolUse) + one removal (PostToolUse).
        assert_eq!(changed, 2);
    }

    #[test]
    fn non_pretooluse_guard_entry_with_other_hooks_keeps_others() {
        let mut hooks = serde_json::Map::new();
        hooks.insert(
            "PostToolUse".to_string(),
            serde_json::Value::Array(vec![serde_json::json!({
                "matcher": "Bash",
                "hooks": [
                    { "type": "command", "command": "~/.claude/hooks/gitpixel-targets-guard" },
                    { "type": "command", "command": "~/.claude/hooks/other-tool" }
                ]
            })]),
        );

        let changed = rewrite_guard_hook_commands(&mut hooks);

        // The outer entry survives, but only the non-guard hook remains.
        let remaining = hooks["PostToolUse"].as_array().unwrap();
        assert_eq!(remaining.len(), 1);
        let inner = remaining[0]["hooks"].as_array().unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0]["command"].as_str().unwrap(), "~/.claude/hooks/other-tool");
        assert_eq!(changed, 1);
    }
}
