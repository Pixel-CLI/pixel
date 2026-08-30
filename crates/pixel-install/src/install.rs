//! Idempotent `pixel install` — removes the deprecated
//! usable-git/gitpixel/sniper MCP entries, installs the guard and
//! SessionStart hooks, and rewrites agent-config with managed markers.
//!
//! pixel is a CLI + hooks tool, not an MCP server. The four mandatory
//! scenarios (targets, resolve, rescue/excavate, reconcile) are enforced by
//! rule text plus the PreToolUse guard hook on Bash/Read/Grep/Glob/Edit/Write
//! — wiring pixel as an MCP server would give agents a transport that
//! bypasses that guard. See the pixel rule for the doctrine.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config;
use crate::InstallError;

pub type Result<T> = std::result::Result<T, InstallError>;

/// Per-check status for the install report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Green,
    Yellow,
    Red,
}

/// One install action and its outcome.
#[derive(Debug, Clone, Serialize)]
pub struct InstallStep {
    pub id: String,
    pub status: CheckStatus,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The full install report.
#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    pub version: String,
    pub ok: bool,
    pub executable_path: String,
    pub home: String,
    /// True if this report describes a dry run: every step below reflects
    /// what WOULD happen, but no filesystem write occurred.
    pub dry_run: bool,
    pub steps: Vec<InstallStep>,
    pub summary: InstallSummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallSummary {
    pub green: usize,
    pub yellow: usize,
    pub red: usize,
}

/// Options controlling an install run.
#[derive(Debug, Clone)]
pub struct InstallOptions {
    /// Path to the pixel binary to register. Defaults to the current exe.
    pub executable_path: Option<PathBuf>,
    /// Home directory. Defaults to `$HOME`.
    pub home: Option<PathBuf>,
    /// The capability block emitted by the SessionStart hook, derived from the
    /// binary's actual op registry. If None, a default block is used.
    pub capability_block: Option<String>,
    /// If true, compute and report every step's outcome exactly as a real
    /// run would, but perform no filesystem writes: no settings.json edits,
    /// no hook files, no agent-config rewrites, no backups, no directory
    /// creation. Safe to run against a real `$HOME` to preview an install.
    pub dry_run: bool,
}

impl Default for InstallOptions {
    fn default() -> Self {
        InstallOptions {
            executable_path: None,
            home: None,
            capability_block: None,
            dry_run: false,
        }
    }
}

/// Run `pixel install`. Idempotent: safe to re-run.
pub fn install(options: &InstallOptions) -> Result<InstallReport> {
    let home = options
        .home
        .clone()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .ok_or(InstallError::NoHome)?;
    let executable_path = match &options.executable_path {
        Some(p) => p.clone(),
        None => std::env::current_exe().map_err(InstallError::CurrentExe)?,
    };
    let exe = executable_path
        .canonicalize()
        .unwrap_or_else(|_| executable_path.clone());

    let dry_run = options.dry_run;
    let mut steps = Vec::new();

    // 1. Remove deprecated MCP servers + old guard hooks from Claude
    //    settings.json. pixel is a CLI + hooks tool, not an MCP server —
    //    the deprecated usable-git/gitpixel/sniper MCP entries are retired
    //    unconditionally (pixel replaces them via Bash, not MCP).
    steps.push(scrub_deprecated(&home, dry_run)?);

    // 2. Replace the old guard hook with `exec pixel hook guard "$@"` and
    //    wire the PreToolUse entry into Claude settings.json.
    steps.push(install_guard_hook(&home, &exe, dry_run)?);

    // 3. Install the SessionStart hook (Claude settings.json).
    steps.push(install_session_start_hook(
        &home,
        &exe,
        options.capability_block.as_deref(),
        dry_run,
    )?);

    // 4. Wire PreToolUse + SessionStart hooks into Devin, Codex, and Gemini.
    steps.push(install_devin_hooks(&home, &exe, dry_run)?);
    steps.push(install_codex_hooks(&home, &exe, dry_run)?);
    steps.push(install_gemini_hooks(&home, &exe, dry_run)?);

    // 5. Rewrite agent-config with managed markers.
    steps.push(rewrite_agent_configs(&home, &exe, dry_run)?);

    let green = steps.iter().filter(|s| s.status == CheckStatus::Green).count();
    let yellow = steps.iter().filter(|s| s.status == CheckStatus::Yellow).count();
    let red = steps.iter().filter(|s| s.status == CheckStatus::Red).count();
    let ok = red == 0;

    Ok(InstallReport {
        version: "v1".into(),
        ok,
        executable_path: exe.display().to_string(),
        home: home.display().to_string(),
        dry_run,
        steps,
        summary: InstallSummary { green, yellow, red },
    })
}

fn scrub_deprecated(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let settings = home.join(".claude").join("settings.json");
    // The deprecated usable-git/gitpixel/sniper MCP server entries are
    // retired unconditionally — pixel replaces them via Bash + the guard
    // hook, not via MCP. The guard-hook command rewrite is unrelated to MCP
    // registration and always proceeds.
    let outcome = config::scrub_settings_json(&settings, dry_run)?;
    let removed = outcome.mcp_servers_removed + outcome.guard_hooks_removed;
    let summary = format!(
        "removed {removed} deprecated MCP/hook entr{}",
        if removed == 1 { "y" } else { "ies" }
    );
    Ok(InstallStep {
        id: "mcp.deprecated".into(),
        status: CheckStatus::Green,
        summary: dry_run_summary(dry_run, &summary),
        detail: Some(with_backup_note(
            format!(
                "mcp_servers_removed={} guard_hooks_removed={}",
                outcome.mcp_servers_removed, outcome.guard_hooks_removed
            ),
            outcome.backup_path,
        )),
    })
}

fn install_guard_hook(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let hooks_dir = home.join(config::CLAUDE_HOOKS_DIR);
    let old = hooks_dir.join(config::OLD_GUARD_HOOK);
    let new = hooks_dir.join(config::GUARD_HOOK);
    let body = format!("#!/bin/sh\nexec {} hook guard \"$@\"\n", exe.display());
    let replaced_old = old.exists();

    // Also wire the PreToolUse entry into Claude settings.json, so the
    // guard actually fires on tool calls. The matcher covers both Claude
    // tool names (Bash, Read, Grep, Glob, Edit, MultiEdit, NotebookEdit,
    // Write) and Devin tool names (exec, read, grep, find_file_by_name,
    // glob, edit, write, notebook_read, notebook_edit) — Devin reads
    // ~/.claude/settings.json via its Claude compat layer.
    let settings = home.join(".claude").join("settings.json");
    let mut value = read_settings(&settings)?;
    let hooks_obj = value
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: settings.clone(),
            reason: "settings.json root is not an object".into(),
        }))?
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let hooks_map = hooks_obj
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: settings.clone(),
            reason: "hooks is not an object".into(),
        }))?;
    let guard_command = format!("~/.claude/hooks/{}", config::GUARD_HOOK);
    let existing_pretooluse = hooks_map.get("PreToolUse").cloned();
    let merged_pretooluse = config::merge_hook_entry(
        existing_pretooluse.as_ref(),
        &guard_command,
        serde_json::json!({
            "matcher": config::GUARD_MATCHER,
            "hooks": [{
                "type": "command",
                "command": guard_command,
            }],
        }),
    );
    hooks_map.insert("PreToolUse".to_string(), merged_pretooluse);

    if dry_run {
        return Ok(InstallStep {
            id: "hook.guard".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "guard hook installed"),
            detail: Some(format!(
                "would write {} (replaced_old={replaced_old}) + PreToolUse entry",
                new.display()
            )),
        });
    }

    fs::create_dir_all(&hooks_dir)?;
    let new_backup = config::backup_if_changing(&new, body.as_bytes())?;
    // Deleting the old guard script is itself a destructive write: back up
    // its current content before removing it, unconditionally (there is no
    // "content unchanged" case for a deletion).
    let old_backup = if replaced_old {
        config::backup_if_changing(&old, &[])?
    } else {
        None
    };
    if replaced_old {
        let _ = fs::remove_file(&old);
    }
    fs::write(&new, &body)?;
    set_executable(&new);
    let settings_backup = write_settings(&settings, &value, dry_run)?;
    let backup_path = new_backup.or(old_backup).or(settings_backup);
    Ok(InstallStep {
        id: "hook.guard".into(),
        status: CheckStatus::Green,
        summary: "guard hook installed".into(),
        detail: Some(with_backup_note(
            format!("wrote {} (replaced_old={replaced_old}) + PreToolUse entry", new.display()),
            backup_path,
        )),
    })
}

fn install_session_start_hook(
    home: &Path,
    exe: &Path,
    capability_block: Option<&str>,
    dry_run: bool,
) -> Result<InstallStep> {
    let hooks_dir = home.join(config::CLAUDE_HOOKS_DIR);
    let path = hooks_dir.join(config::SESSION_START_HOOK);
    let body = format!("#!/bin/sh\nexec {} hook session-start \"$@\"\n", exe.display());

    let settings = home.join(".claude").join("settings.json");
    let mut value = read_settings(&settings)?;
    let hooks = value
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: settings.clone(),
            reason: "settings.json root is not an object".into(),
        }))?;
    let hooks_obj = hooks
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let obj = hooks_obj
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: settings.clone(),
            reason: "hooks is not an object".into(),
        }))?;
    // Merge, never blind-overwrite: a real settings.json can already carry
    // multiple unrelated SessionStart entries registered by other tools
    // (e.g. separate matcher groups for "startup"/"resume"/"clear"). Replace
    // only a prior *pixel-authored* entry (identified by its own command
    // substring), so re-installs stay idempotent without destroying anyone
    // else's hooks.
    let existing_session_start = obj.get("SessionStart").cloned();
    let pixel_command = format!("{} hook session-start", exe.display());
    let merged = config::merge_hook_entry(existing_session_start.as_ref(), "hook session-start", serde_json::json!({
        "matcher": "SessionStart",
        "hooks": [{
            "type": "command",
            "command": pixel_command,
        }],
    }));
    obj.insert("SessionStart".to_string(), merged);

    let _ = capability_block; // emitted by the hook itself from the op registry

    if dry_run {
        return Ok(InstallStep {
            id: "hook.session-start".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "SessionStart hook installed"),
            detail: Some(format!("would write {}", path.display())),
        });
    }

    fs::create_dir_all(&hooks_dir)?;
    let hook_backup = config::backup_if_changing(&path, body.as_bytes())?;
    fs::write(&path, &body)?;
    set_executable(&path);

    let settings_backup = write_settings(&settings, &value, dry_run)?;
    let backup_path = hook_backup.or(settings_backup);

    Ok(InstallStep {
        id: "hook.session-start".into(),
        status: CheckStatus::Green,
        summary: "SessionStart hook installed".into(),
        detail: Some(with_backup_note(format!("wrote {}", path.display()), backup_path)),
    })
}

/// Load the canonical pixel usage-rule text from `~/.agent-config/rules/pixel.md`
/// and strip its YAML frontmatter so the body can be embedded directly into a
/// CLAUDE.md/AGENTS.md managed block. Returns `None` if the file is missing or
/// unreadable (the caller falls back to the short summary).
fn load_usage_rules(home: &Path) -> Option<String> {
    let path = home.join(config::PIXEL_RULES_REL);
    let text = fs::read_to_string(&path).ok()?;
    // Strip a leading `---\n...\n---\n` YAML frontmatter block if present.
    let body = if let Some(rest) = text.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---\n") {
            &rest[end + "\n---\n".len()..]
        } else {
            &text
        }
    } else {
        &text
    };
    // Remove the conflicting usable-git rule: usable-git is retired, so the
    // installed rules must not frame pixel's mutation ops as a "1:1
    // replacement" for it or cite its old benchmark as a live reference.
    // The retirement statements ("NEVER use gitpixel or usable-git") are
    // kept — only the live-comparison framing is stripped.
    let cleaned = body
        .replace("## Git operations — mutation ops replace usable-git 1:1", "## Git operations — mutation ops")
        .replace(
            "the same crash-safety discipline usable-git proved across a 960-trial benchmark (0 fsck failures, 0 lost unrelated work)",
            "the same crash-safety discipline that made the mutation surface trustworthy",
        );
    Some(cleaned.trim_end().to_string())
}

fn rewrite_agent_configs(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    // Ship the real usage rules (the four mandatory scenarios, the doctrine,
    // the git-op table) in the managed block, not just a 3-line summary. The
    // rules live in ~/.agent-config/rules/pixel.md; if that file is missing we
    // fall back to the short summary so install never hard-fails on it.
    let managed = match load_usage_rules(home) {
        Some(rules) => format!(
            "pixel is the unified retrieval + git engine. Use `pixel <verb>` for\n\
             search, resolve, targets, history, and safe git ops.\n\
             Binary: {}\n\n\
             {}\n",
            exe.display(),
            rules
        ),
        None => format!(
            "pixel is the unified retrieval + git engine. Use `pixel <verb>` for\n\
             search, resolve, targets, history, and safe git ops.\n\
             Binary: {}\n",
            exe.display()
        ),
    };
    let mut targets = config::find_agent_configs(home);
    if targets.is_empty() {
        // No CLAUDE.md/AGENTS.md exists anywhere pixel looks yet. Without
        // this fallback, `find_agent_configs` (which only returns files
        // that already exist) would return an empty list and this whole
        // step would silently no-op — a brand-new machine would get zero
        // pixel usage instructions written anywhere, forever. Ensure at
        // least the canonical CLAUDE.md carries the managed block.
        targets.push(home.join("CLAUDE.md"));
    }

    let mut rewritten = 0usize;
    let mut stale_removed = 0usize;
    let mut backups: Vec<String> = Vec::new();
    for path in targets {
        let outcome = config::rewrite_agent_config(&path, &managed, dry_run)?;
        if outcome.rewritten || (dry_run && outcome.would_change) {
            rewritten += 1;
        }
        stale_removed += outcome.stale_blocks_removed;
        if let Some(b) = outcome.backup_path {
            backups.push(b.display().to_string());
        }
    }
    let verb = if dry_run { "would rewrite" } else { "rewrote" };
    Ok(InstallStep {
        id: "agent-config".into(),
        status: CheckStatus::Green,
        summary: format!("{verb} {rewritten} agent-config file(s)"),
        detail: Some(format!(
            "stale_blocks_removed={stale_removed}{}",
            if backups.is_empty() {
                String::new()
            } else {
                format!(" backups={}", backups.join(","))
            }
        )),
    })
}

/// Wire PreToolUse + SessionStart hooks into Devin's `~/.config/devin/config.json`.
/// Devin reads `~/.claude/settings.json` via its Claude compat layer by
/// default, but writing directly to Devin's own config ensures the hooks
/// fire even if that compat layer is disabled.
fn install_devin_hooks(home: &Path, _exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_dir = home.join(config::DEVIN_CONFIG_DIR);
    let config_path = config_dir.join(config::DEVIN_CONFIG_FILE);
    let mut value = read_settings(&config_path)?;

    let hooks_obj = value
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: config_path.clone(),
            reason: "config.json root is not an object".into(),
        }))?
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let hooks_map = hooks_obj
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: config_path.clone(),
            reason: "hooks is not an object".into(),
        }))?;

    // PreToolUse — guard hook. Same matcher as Claude (covers both tool
    // name sets). The guard script is the same file under ~/.claude/hooks/.
    let guard_command = format!("~/.claude/hooks/{}", config::GUARD_HOOK);
    let existing_pretooluse = hooks_map.get("PreToolUse").cloned();
    let merged_pretooluse = config::merge_hook_entry(
        existing_pretooluse.as_ref(),
        &guard_command,
        serde_json::json!({
            "matcher": config::GUARD_MATCHER,
            "hooks": [{
                "type": "command",
                "command": guard_command,
            }],
        }),
    );
    hooks_map.insert("PreToolUse".to_string(), merged_pretooluse);

    // SessionStart — same hook script as Claude.
    let session_start_command = format!("~/.claude/hooks/{}", config::SESSION_START_HOOK);
    let existing_session_start = hooks_map.get("SessionStart").cloned();
    let merged_session_start = config::merge_hook_entry(
        existing_session_start.as_ref(),
        &session_start_command,
        serde_json::json!({
            "matcher": "SessionStart",
            "hooks": [{
                "type": "command",
                "command": session_start_command,
            }],
        }),
    );
    hooks_map.insert("SessionStart".to_string(), merged_session_start);

    if dry_run {
        return Ok(InstallStep {
            id: "hooks.devin".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "Devin hooks wired (PreToolUse + SessionStart)"),
            detail: Some(format!("would write {}", config_path.display())),
        });
    }

    let backup_path = write_settings(&config_path, &value, dry_run)?;
    Ok(InstallStep {
        id: "hooks.devin".into(),
        status: CheckStatus::Green,
        summary: "Devin hooks wired (PreToolUse + SessionStart)".into(),
        detail: Some(with_backup_note(
            format!("wrote {}", config_path.display()),
            backup_path,
        )),
    })
}

/// Wire PreToolUse + SessionStart hooks into Codex's `~/.codex/hooks.json`.
/// Codex uses the same hook format as Claude (event `PreToolUse`).
fn install_codex_hooks(home: &Path, _exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_path = home.join(config::CODEX_HOOKS_FILE);
    let mut value = read_settings(&config_path)?;

    let hooks_obj = value
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: config_path.clone(),
            reason: "hooks.json root is not an object".into(),
        }))?
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let hooks_map = hooks_obj
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: config_path.clone(),
            reason: "hooks is not an object".into(),
        }))?;

    let guard_command = format!("~/.claude/hooks/{}", config::GUARD_HOOK);
    let existing_pretooluse = hooks_map.get("PreToolUse").cloned();
    let merged_pretooluse = config::merge_hook_entry(
        existing_pretooluse.as_ref(),
        &guard_command,
        serde_json::json!({
            "matcher": config::GUARD_MATCHER,
            "hooks": [{
                "type": "command",
                "command": guard_command,
            }],
        }),
    );
    hooks_map.insert("PreToolUse".to_string(), merged_pretooluse);

    let session_start_command = format!("~/.claude/hooks/{}", config::SESSION_START_HOOK);
    let existing_session_start = hooks_map.get("SessionStart").cloned();
    let merged_session_start = config::merge_hook_entry(
        existing_session_start.as_ref(),
        &session_start_command,
        serde_json::json!({
            "matcher": "SessionStart",
            "hooks": [{
                "type": "command",
                "command": session_start_command,
            }],
        }),
    );
    hooks_map.insert("SessionStart".to_string(), merged_session_start);

    if dry_run {
        return Ok(InstallStep {
            id: "hooks.codex".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "Codex hooks wired (PreToolUse + SessionStart)"),
            detail: Some(format!("would write {}", config_path.display())),
        });
    }

    let backup_path = write_settings(&config_path, &value, dry_run)?;
    Ok(InstallStep {
        id: "hooks.codex".into(),
        status: CheckStatus::Green,
        summary: "Codex hooks wired (PreToolUse + SessionStart)".into(),
        detail: Some(with_backup_note(
            format!("wrote {}", config_path.display()),
            backup_path,
        )),
    })
}

/// Wire BeforeTool + SessionStart hooks into Gemini's `~/.gemini/settings.json`.
/// Gemini uses `BeforeTool` instead of `PreToolUse`, but the same hook format.
fn install_gemini_hooks(home: &Path, _exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_path = home.join(config::GEMINI_SETTINGS_FILE);
    let mut value = read_settings(&config_path)?;

    let hooks_obj = value
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: config_path.clone(),
            reason: "settings.json root is not an object".into(),
        }))?
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let hooks_map = hooks_obj
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: config_path.clone(),
            reason: "hooks is not an object".into(),
        }))?;

    // Gemini uses "BeforeTool" instead of "PreToolUse".
    let guard_command = format!("~/.claude/hooks/{}", config::GUARD_HOOK);
    let existing_beforetool = hooks_map.get("BeforeTool").cloned();
    let merged_beforetool = config::merge_hook_entry(
        existing_beforetool.as_ref(),
        &guard_command,
        serde_json::json!({
            "matcher": config::GUARD_MATCHER,
            "hooks": [{
                "type": "command",
                "command": guard_command,
            }],
        }),
    );
    hooks_map.insert("BeforeTool".to_string(), merged_beforetool);

    let session_start_command = format!("~/.claude/hooks/{}", config::SESSION_START_HOOK);
    let existing_session_start = hooks_map.get("SessionStart").cloned();
    let merged_session_start = config::merge_hook_entry(
        existing_session_start.as_ref(),
        &session_start_command,
        serde_json::json!({
            "matcher": "SessionStart",
            "hooks": [{
                "type": "command",
                "command": session_start_command,
            }],
        }),
    );
    hooks_map.insert("SessionStart".to_string(), merged_session_start);

    if dry_run {
        return Ok(InstallStep {
            id: "hooks.gemini".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "Gemini hooks wired (BeforeTool + SessionStart)"),
            detail: Some(format!("would write {}", config_path.display())),
        });
    }

    let backup_path = write_settings(&config_path, &value, dry_run)?;
    Ok(InstallStep {
        id: "hooks.gemini".into(),
        status: CheckStatus::Green,
        summary: "Gemini hooks wired (BeforeTool + SessionStart)".into(),
        detail: Some(with_backup_note(
            format!("wrote {}", config_path.display()),
            backup_path,
        )),
    })
}

fn read_settings(path: &Path) -> Result<serde_json::Value> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(serde_json::from_str(&s)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(e.into()),
    }
}

/// Serialize and write `value` to `path`, backing up any pre-existing,
/// content-differing file first. In dry-run mode, performs no write, no
/// backup, and no directory creation, and always returns `Ok(None)`.
fn write_settings(path: &Path, value: &serde_json::Value, dry_run: bool) -> Result<Option<PathBuf>> {
    let serialized = format!("{}\n", serde_json::to_string_pretty(value)?);
    if dry_run {
        return Ok(None);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let backup_path = config::backup_if_changing(path, serialized.as_bytes())?;
    fs::write(path, serialized)?;
    Ok(backup_path)
}

fn dry_run_summary(dry_run: bool, summary: &str) -> String {
    if dry_run {
        format!("[dry-run] would report: {summary}")
    } else {
        summary.to_string()
    }
}

fn with_backup_note(detail: String, backup_path: Option<PathBuf>) -> String {
    match backup_path {
        Some(p) => format!("{detail} (backup={})", p.display()),
        None => detail,
    }
}

fn set_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

// ---------------------------------------------------------------------------
// rollout / migration (clean cut, no shims)
// ---------------------------------------------------------------------------

/// Outcome of a `pixel migrate` run.
#[derive(Debug, Clone, Serialize)]
pub struct MigrateReport {
    pub version: String,
    pub ok: bool,
    pub repo_root: String,
    /// Number of gain-ledger events carried over from `.gitpixel/`.
    pub ledger_events_carried: usize,
    /// True if a `.gitpixel/` directory was found and deleted.
    pub old_state_removed: bool,
    /// True if `.pixel/` was rebuilt fresh.
    pub new_state_rebuilt: bool,
}

/// Migrate a repo from the old `.gitpixel/` state to a fresh `.pixel/` state.
///
/// Deletes `.gitpixel/` and rebuilds `.pixel/` fresh, carrying only the gain
/// ledger jsonl (appended with a source tag). No index migration — all
/// indexes are caches and are rebuilt.
pub fn migrate(repo_root: &Path) -> Result<MigrateReport> {
    let old_dir = repo_root.join(".gitpixel");
    let new_dir = repo_root.join(".pixel");

    // Carry the gain ledger jsonl from the old state, if present.
    let mut ledger_events_carried = 0usize;
    let old_ledger = old_dir.join(crate::gain::LEDGER_FILE);
    if old_ledger.is_file() {
        if let Ok(raw) = fs::read_to_string(&old_ledger) {
            let ledger = crate::gain::GainLedger::open()?;
            for line in raw.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(mut event) = serde_json::from_str::<crate::gain::GainEvent>(line) {
                    event.source = crate::gain::SOURCE_MIGRATED.to_string();
                    let input = crate::gain::GainEventInput {
                        operation: event.operation,
                        client: event.client,
                        transport: event.transport,
                        result_code: event.result_code,
                        envelope_bytes: event.envelope_bytes,
                        raw_equivalent_bytes: event.raw_equivalent_bytes,
                        agent_ops_raw: event.agent_ops_raw,
                        agent_ops_actual: event.agent_ops_actual,
                        git_subprocesses_raw: event.git_subprocesses_raw,
                        git_subprocesses_actual: event.git_subprocesses_actual,
                        duration_ms: event.duration_ms,
                        tokens_saved: event.tokens_saved,
                        source: crate::gain::SOURCE_MIGRATED.to_string(),
                    };
                    if ledger.append(&input).is_ok() {
                        ledger_events_carried += 1;
                    }
                }
            }
        }
    }

    // Delete the old state directory.
    let old_state_removed = if old_dir.exists() {
        fs::remove_dir_all(&old_dir)?;
        true
    } else {
        false
    };

    // Rebuild `.pixel/` fresh (the index/graph/facts are caches; the daemon
    // and CLI rebuild them on first use).
    fs::create_dir_all(&new_dir)?;
    let new_state_rebuilt = true;

    Ok(MigrateReport {
        version: "v1".into(),
        ok: true,
        repo_root: repo_root.display().to_string(),
        ledger_events_carried,
        old_state_removed,
        new_state_rebuilt,
    })
}
