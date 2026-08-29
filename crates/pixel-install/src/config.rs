//! Agent-config rewrite helpers for `pixel install` / `pixel migrate`.
//!
//! Finds the Claude/agent config files (CLAUDE.md, AGENTS.md, settings.json),
//! applies the managed-marker wrapping (`<!-- pixel:managed:begin/end -->`),
//! deletes stale GitNexus / codebase-memory blocks, and scrubs settings.json
//! entries that point at the old guard hook.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
    /// True if the file existed and was rewritten.
    pub rewritten: bool,
    /// True if the file already carried the managed markers (idempotent re-run).
    pub already_managed: bool,
    /// Number of stale GitNexus / codebase-memory blocks removed.
    pub stale_blocks_removed: usize,
}

/// Outcome of scrubbing one settings.json.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScrubOutcome {
    pub path: PathBuf,
    pub existed: bool,
    /// Number of MCP-server entries removed (usable-git/gitpixel/sniper).
    pub mcp_servers_removed: usize,
    /// Number of hook entries pointing at the old guard removed.
    pub guard_hooks_removed: usize,
}

/// The agent-config files `pixel install` manages, in a stable order.
/// Returns only paths that exist on disk.
pub fn find_agent_configs(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for rel in [
        "CLAUDE.md",
        "AGENTS.md",
        ".claude/CLAUDE.md",
        ".claude/AGENTS.md",
        ".claude/settings.json",
    ] {
        let p = home.join(rel);
        if p.is_file() {
            out.push(p);
        }
    }
    out
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
            Some(end) => &cleaned[end + MANAGED_END.len()..],
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

/// Remove lines/blocks that reference the stale GitNexus / codebase-memory
/// tooling. Returns the cleaned text and the number of blocks removed.
pub fn strip_stale_blocks(text: &str) -> (String, usize) {
    let mut removed = 0usize;
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if STALE_BLOCK_MARKERS.iter().any(|m| line.contains(m)) {
            removed += 1;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    (out, removed)
}

/// Rewrite one agent-config file with the managed block. Creates the file if
/// it does not exist. Returns the outcome.
pub fn rewrite_agent_config(path: &Path, managed: &str) -> Result<RewriteOutcome> {
    let original = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let already_managed = original.contains(MANAGED_BEGIN);
    let (cleaned, stale_blocks_removed) = strip_stale_blocks(&original);
    let rewritten = apply_managed_markers(&cleaned, managed);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, rewritten)?;
    Ok(RewriteOutcome {
        path: path.to_path_buf(),
        rewritten: true,
        already_managed,
        stale_blocks_removed,
    })
}

/// MCP server names that must be removed during install (the deprecated
/// tools). `pixel` itself is the only one kept.
pub const DEPRECATED_MCP_SERVERS: &[&str] = &["usable-git", "gitpixel", "sniper"];

/// Hook names that point at the old guard and must be scrubbed.
pub const DEPRECATED_GUARD_HOOKS: &[&str] = &["gitpixel-targets-guard"];

/// Remove deprecated MCP-server entries and old-guard hook entries from a
/// Claude `settings.json`. Returns the scrub outcome.
pub fn scrub_settings_json(path: &Path) -> Result<ScrubOutcome> {
    let existed = path.is_file();
    if !existed {
        return Ok(ScrubOutcome {
            path: path.to_path_buf(),
            existed: false,
            mcp_servers_removed: 0,
            guard_hooks_removed: 0,
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
        for name in DEPRECATED_GUARD_HOOKS {
            if hooks.remove(*name).is_some() {
                guard_hooks_removed += 1;
            }
        }
    }

    if mcp_servers_removed > 0 || guard_hooks_removed > 0 {
        let serialized = serde_json::to_string_pretty(&value)?;
        fs::write(path, format!("{serialized}\n"))?;
    }

    Ok(ScrubOutcome {
        path: path.to_path_buf(),
        existed: true,
        mcp_servers_removed,
        guard_hooks_removed,
    })
}
