//! Idempotent `pixel install` — removes the deprecated
//! usable-git/gitpixel/sniper MCP entries, installs the guard and
//! SessionStart hooks, and rewrites agent-config with managed markers.
//!
//! pixel is a CLI + hooks tool, not an MCP server. The five mandatory
//! scenarios (targets — mandatory first call with an advisory fence,
//! resolve, rescue/excavate, reconcile, and impact/changes blast radius)
//! are enforced by rule text plus the PreToolUse guard hook on
//! Bash/Read/Grep/Glob/Edit/Write — wiring pixel as an MCP server would
//! give agents a transport that bypasses that guard. See the pixel rule
//! for the doctrine.

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
    /// Path to the pixel binary the installed hooks point at. Defaults to
    /// the current exe.
    pub executable_path: Option<PathBuf>,
    /// Home directory. Defaults to `$HOME`.
    pub home: Option<PathBuf>,
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

    // 3. Install the SessionStart hook (Claude settings.json). The
    //    capability block itself is emitted at session time by the hook,
    //    straight from the binary's live op registry — never a snapshot
    //    baked in at install time that could drift from the binary.
    steps.push(install_session_start_hook(&home, &exe, dry_run)?);

    // 3b. Install the UserPromptSubmit hook (task boundary detector).
    //     Fires on every user prompt, embeds prompt + recent context, and
    //     emits a [PIXEL:TASK_BOUNDARY] advisory when a task boundary is
    //     detected.
    steps.push(install_prompt_submit_hook(&home, &exe, dry_run)?);

    // 3c. Install the PostCompaction hook (targets manifest re-injection).
    //     Fires after context compaction, re-injects the active targets
    //     manifest so the agent resumes with its retrieval state intact.
    steps.push(install_post_compaction_hook(&home, &exe, dry_run)?);

    // 4. Wire PreToolUse + SessionStart hooks into Devin, Codex, Gemini,
    //    zcode, and Cursor. pi gets rules only (no per-tool hooks).
    steps.push(install_devin_hooks(&home, &exe, dry_run)?);
    steps.push(install_codex_hooks(&home, &exe, dry_run)?);
    steps.push(install_gemini_hooks(&home, &exe, dry_run)?);
    steps.push(install_zcode_hooks(&home, &exe, dry_run)?);
    steps.push(install_cursor_hooks(&home, &exe, dry_run)?);
    steps.push(install_pi_rules(&home, &exe, dry_run)?);

    // 4b. Heal any project-level `.codex/hooks.json` that shadows the
    //     global one just installed above (see `patch_project_codex_hooks`
    //     for the empirically-verified shadowing bug this closes).
    steps.push(patch_project_codex_hooks(&home, dry_run)?);

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
    // Claude Code keeps GLOBAL MCP registrations in ~/.claude.json (top-level
    // `mcpServers`), not in ~/.claude/settings.json — a retired server
    // registered there survived every install scrub and kept failing to
    // connect at session start. Same file shape, so the same scrubber runs
    // on both.
    let global_config = home.join(".claude.json");
    let global_outcome = config::scrub_settings_json(&global_config, dry_run)?;
    // Retired-tool RULE files are scrubbed alongside the MCP entries. An
    // MCP registration and a Markdown rule are two different ways to keep
    // offering a retired tool; removing only the first leaves Devin,
    // Cline, and Cursor still advertising `usable-git`/`gitpixel` to the
    // model as rules it may load.
    let rule_files = config::scrub_deprecated_rule_files(home, dry_run)?;
    let removed = outcome.mcp_servers_removed
        + global_outcome.mcp_servers_removed
        + outcome.guard_hooks_removed
        + rule_files.len();
    let summary = format!(
        "removed {removed} deprecated MCP/hook/rule entr{}",
        if removed == 1 { "y" } else { "ies" }
    );
    let mut detail = format!(
        "mcp_servers_removed={} global_mcp_servers_removed={} guard_hooks_removed={} rule_files_removed={}",
        outcome.mcp_servers_removed,
        global_outcome.mcp_servers_removed,
        outcome.guard_hooks_removed,
        rule_files.len()
    );
    if !rule_files.is_empty() {
        detail.push_str(&format!(
            " rule_files=[{}]",
            rule_files
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    Ok(InstallStep {
        id: "mcp.deprecated".into(),
        status: CheckStatus::Green,
        summary: dry_run_summary(dry_run, &summary),
        detail: Some(with_backup_note(detail, outcome.backup_path)),
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

fn install_session_start_hook(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
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

fn install_prompt_submit_hook(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let hooks_dir = home.join(config::CLAUDE_HOOKS_DIR);
    let path = hooks_dir.join(config::PROMPT_SUBMIT_HOOK);
    let body = format!("#!/bin/sh\nexec {} hook prompt-submit \"$@\"\n", exe.display());

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
    // Merge, never blind-overwrite — same idempotent pattern as guard and
    // session-start. Replace only a prior pixel-authored entry.
    let existing = obj.get("UserPromptSubmit").cloned();
    let pixel_command = format!("{} hook prompt-submit", exe.display());
    let merged = config::merge_hook_entry(existing.as_ref(), "hook prompt-submit", serde_json::json!({
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": pixel_command,
        }],
    }));
    obj.insert("UserPromptSubmit".to_string(), merged);

    if dry_run {
        return Ok(InstallStep {
            id: "hook.prompt-submit".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "UserPromptSubmit hook installed"),
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
        id: "hook.prompt-submit".into(),
        status: CheckStatus::Green,
        summary: "UserPromptSubmit hook installed".into(),
        detail: Some(with_backup_note(format!("wrote {}", path.display()), backup_path)),
    })
}

fn install_post_compaction_hook(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let hooks_dir = home.join(config::CLAUDE_HOOKS_DIR);
    let path = hooks_dir.join(config::POST_COMPACTION_HOOK);
    let body = format!("#!/bin/sh\nexec {} hook post-compaction \"$@\"\n", exe.display());

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
    // Merge, never blind-overwrite — same idempotent pattern as the other
    // hooks. Replace only a prior pixel-authored entry.
    let existing = obj.get("PostCompaction").cloned();
    let pixel_command = format!("{} hook post-compaction", exe.display());
    let merged = config::merge_hook_entry(existing.as_ref(), "hook post-compaction", serde_json::json!({
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": pixel_command,
        }],
    }));
    obj.insert("PostCompaction".to_string(), merged);

    if dry_run {
        return Ok(InstallStep {
            id: "hook.post-compaction".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "PostCompaction hook installed"),
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
        id: "hook.post-compaction".into(),
        status: CheckStatus::Green,
        summary: "PostCompaction hook installed".into(),
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
    // Ship the real usage rules (the five mandatory scenarios, the doctrine,
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
        // If the file already contains the pixel rule text (deployed by
        // build-agent-config's aggregate), skip writing a managed block —
        // it would only create a duplicate. Still strip any stale managed
        // blocks from a previous install that ran before build-agent-config
        // included pixel.md in the aggregate.
        let existing = fs::read_to_string(&path).unwrap_or_default();
        let has_pixel_rule = existing.contains("# pixel — Deterministic");
        let has_managed = existing.contains(config::MANAGED_BEGIN);

        if has_pixel_rule && !has_managed {
            // Pixel rule already present via aggregate, no stale managed
            // block to clean — nothing to do.
            rewritten += 1;
            continue;
        }

        if has_pixel_rule && has_managed {
            // Pixel rule present via aggregate AND a stale managed block
            // exists — strip the managed block only, don't write a new one.
            let stripped = config::strip_stale_blocks(&existing).0;
            if dry_run {
                rewritten += 1;
                continue;
            }
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let bk = config::backup_if_changing(&path, stripped.as_bytes())?;
            fs::write(&path, &stripped)?;
            if bk.is_some() {
                backups.push(path.display().to_string());
            }
            stale_removed += 1;
            rewritten += 1;
            continue;
        }

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

    // UserPromptSubmit — task boundary detector hook.
    let prompt_submit_command = format!("~/.claude/hooks/{}", config::PROMPT_SUBMIT_HOOK);
    let existing_prompt_submit = hooks_map.get("UserPromptSubmit").cloned();
    let merged_prompt_submit = config::merge_hook_entry(
        existing_prompt_submit.as_ref(),
        &prompt_submit_command,
        serde_json::json!({
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": prompt_submit_command,
            }],
        }),
    );
    hooks_map.insert("UserPromptSubmit".to_string(), merged_prompt_submit);

    // PostCompaction — re-inject targets manifest after context compaction.
    let post_compaction_command = format!("~/.claude/hooks/{}", config::POST_COMPACTION_HOOK);
    let existing_post_compaction = hooks_map.get("PostCompaction").cloned();
    let merged_post_compaction = config::merge_hook_entry(
        existing_post_compaction.as_ref(),
        &post_compaction_command,
        serde_json::json!({
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": post_compaction_command,
            }],
        }),
    );
    hooks_map.insert("PostCompaction".to_string(), merged_post_compaction);

    if dry_run {
        return Ok(InstallStep {
            id: "hooks.devin".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "Devin hooks wired (PreToolUse + SessionStart + UserPromptSubmit + PostCompaction)"),
            detail: Some(format!("would write {}", config_path.display())),
        });
    }

    let backup_path = write_settings(&config_path, &value, dry_run)?;
    Ok(InstallStep {
        id: "hooks.devin".into(),
        status: CheckStatus::Green,
        summary: "Devin hooks wired (PreToolUse + SessionStart + UserPromptSubmit + PostCompaction)".into(),
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

    // UserPromptSubmit — task boundary detector hook.
    let prompt_submit_command = format!("~/.claude/hooks/{}", config::PROMPT_SUBMIT_HOOK);
    let existing_prompt_submit = hooks_map.get("UserPromptSubmit").cloned();
    let merged_prompt_submit = config::merge_hook_entry(
        existing_prompt_submit.as_ref(),
        &prompt_submit_command,
        serde_json::json!({
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": prompt_submit_command,
            }],
        }),
    );
    hooks_map.insert("UserPromptSubmit".to_string(), merged_prompt_submit);

    // PostCompaction — re-inject targets manifest after context compaction.
    let post_compaction_command = format!("~/.claude/hooks/{}", config::POST_COMPACTION_HOOK);
    let existing_post_compaction = hooks_map.get("PostCompaction").cloned();
    let merged_post_compaction = config::merge_hook_entry(
        existing_post_compaction.as_ref(),
        &post_compaction_command,
        serde_json::json!({
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": post_compaction_command,
            }],
        }),
    );
    hooks_map.insert("PostCompaction".to_string(), merged_post_compaction);

    if dry_run {
        return Ok(InstallStep {
            id: "hooks.codex".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "Codex hooks wired (PreToolUse + SessionStart + UserPromptSubmit + PostCompaction)"),
            detail: Some(format!("would write {}", config_path.display())),
        });
    }

    let backup_path = write_settings(&config_path, &value, dry_run)?;
    Ok(InstallStep {
        id: "hooks.codex".into(),
        status: CheckStatus::Green,
        summary: "Codex hooks wired (PreToolUse + SessionStart + UserPromptSubmit + PostCompaction)".into(),
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

    // BeforeAgent — task boundary detector hook in Gemini CLI.
    let prompt_submit_command = format!("~/.claude/hooks/{}", config::PROMPT_SUBMIT_HOOK);
    hooks_map.remove("UserPromptSubmit"); // Scrub stale key if previously registered
    let existing_prompt_submit = hooks_map.get("BeforeAgent").cloned();
    let merged_prompt_submit = config::merge_hook_entry(
        existing_prompt_submit.as_ref(),
        &prompt_submit_command,
        serde_json::json!({
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": prompt_submit_command,
            }],
        }),
    );
    hooks_map.insert("BeforeAgent".to_string(), merged_prompt_submit);

    // PostCompaction — re-inject targets manifest after context compaction.
    let post_compaction_command = format!("~/.claude/hooks/{}", config::POST_COMPACTION_HOOK);
    let existing_post_compaction = hooks_map.get("PostCompaction").cloned();
    let merged_post_compaction = config::merge_hook_entry(
        existing_post_compaction.as_ref(),
        &post_compaction_command,
        serde_json::json!({
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": post_compaction_command,
            }],
        }),
    );
    hooks_map.insert("PostCompaction".to_string(), merged_post_compaction);

    if dry_run {
        return Ok(InstallStep {
            id: "hooks.gemini".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "Gemini hooks wired (BeforeTool + SessionStart + BeforeAgent + PostCompaction)"),
            detail: Some(format!("would write {}", config_path.display())),
        });
    }

    let backup_path = write_settings(&config_path, &value, dry_run)?;
    Ok(InstallStep {
        id: "hooks.gemini".into(),
        status: CheckStatus::Green,
        summary: "Gemini hooks wired (BeforeTool + SessionStart + BeforeAgent + PostCompaction)".into(),
        detail: Some(with_backup_note(
            format!("wrote {}", config_path.display()),
            backup_path,
        )),
    })
}

/// Wire a `preToolUse` hook into Cursor's `~/.cursor/hooks.json`. Unlike
/// every other tool wired above, Cursor's hooks.json is a FLAT per-event
/// array (`{command, matcher?}` objects directly, no nested `hooks`
/// sub-array) and its own event casing `preToolUse` — see
/// `config::CURSOR_HOOKS_FILE` and `config::merge_flat_hook_entry` for the
/// verified schema. No SessionStart hook is wired here: only `preToolUse`'s
/// payload shape has been verified against the installed `cursor-agent`
/// bundle.
fn install_cursor_hooks(home: &Path, _exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_path = home.join(config::CURSOR_HOOKS_FILE);
    let mut value = read_settings(&config_path)?;

    let root = value
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: config_path.clone(),
            reason: "hooks.json root is not an object".into(),
        }))?;
    root.entry("version".to_string()).or_insert(serde_json::json!(1));
    let hooks_obj = root
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let hooks_map = hooks_obj
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: config_path.clone(),
            reason: "hooks is not an object".into(),
        }))?;

    let guard_command = format!("~/.claude/hooks/{}", config::GUARD_HOOK);
    let existing_pretooluse = hooks_map.get("preToolUse").cloned();
    let merged_pretooluse = config::merge_flat_hook_entry(
        existing_pretooluse.as_ref(),
        &guard_command,
        serde_json::json!({
            "command": guard_command,
            "matcher": config::GUARD_MATCHER,
        }),
    );
    hooks_map.insert("preToolUse".to_string(), merged_pretooluse);

    if dry_run {
        return Ok(InstallStep {
            id: "hooks.cursor".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "Cursor hooks wired (preToolUse)"),
            detail: Some(format!("would write {}", config_path.display())),
        });
    }

    let backup_path = write_settings(&config_path, &value, dry_run)?;
    Ok(InstallStep {
        id: "hooks.cursor".into(),
        status: CheckStatus::Green,
        summary: "Cursor hooks wired (preToolUse)".into(),
        detail: Some(with_backup_note(
            format!("wrote {}", config_path.display()),
            backup_path,
        )),
    })
}

/// Directories commonly holding project checkouts, searched one level deep
/// for a project-local `.codex/hooks.json` that could SHADOW the global one
/// installed above. Empirically verified (2026-08-30, `codex exec` against
/// a real ship-fast checkout carrying only a `worktree-path-guard.sh`
/// PreToolUse entry): Codex does NOT merge global and project-level
/// `hooks.json` — a project's own file completely replaces the global
/// PreToolUse array for every Codex session in that project. A destructive
/// `git branch -D` ran to completion, unblocked, proving the global guard
/// never fired. Any project with a pre-existing `.codex/hooks.json`
/// (installed by cmux/orca's worktree-path-guard, observed here in
/// ship-fast, omni, liza, execution-engine, and others) silently drops all
/// of pixel's Codex-side enforcement.
fn project_hook_search_roots(home: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for parent in ["Documents", "Desktop"] {
        let Ok(entries) = fs::read_dir(home.join(parent)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                roots.push(path);
            }
        }
    }
    roots
}

/// Merge pixel's guard into the PreToolUse array of every project-level
/// `.codex/hooks.json` found under `home`'s common project directories that
/// doesn't already carry it. One `pixel install` run heals every shadowed
/// project on the machine, not just the repo it happens to run from.
fn patch_project_codex_hooks(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let mut patched = Vec::new();
    let mut already_ok = 0usize;
    for root in project_hook_search_roots(home) {
        let config_path = root.join(".codex").join("hooks.json");
        if !config_path.is_file() {
            continue;
        }
        let carries_hooks = fs::read_to_string(&config_path)
            .map(|s| s.contains(config::GUARD_HOOK) && s.contains(config::PROMPT_SUBMIT_HOOK))
            .unwrap_or(true); // unreadable => don't touch it, don't count it broken
        if carries_hooks {
            already_ok += 1;
            continue;
        }
        if dry_run {
            patched.push(config_path.display().to_string());
            continue;
        }
        let mut value = read_settings(&config_path)?;
        let Some(root_obj) = value.as_object_mut() else {
            continue;
        };
        let hooks_obj = root_obj
            .entry("hooks".to_string())
            .or_insert_with(|| serde_json::json!({}));
        let Some(hooks_map) = hooks_obj.as_object_mut() else {
            continue;
        };
        let guard_command = format!("~/.claude/hooks/{}", config::GUARD_HOOK);
        let existing = hooks_map.get("PreToolUse").cloned();
        let merged = config::merge_hook_entry(
            existing.as_ref(),
            &guard_command,
            serde_json::json!({
                "matcher": config::GUARD_MATCHER,
                "hooks": [{"type": "command", "command": guard_command}],
            }),
        );
        hooks_map.insert("PreToolUse".to_string(), merged);

        let prompt_submit_command = format!("~/.claude/hooks/{}", config::PROMPT_SUBMIT_HOOK);
        let existing_prompt = hooks_map.get("UserPromptSubmit").cloned();
        let merged_prompt = config::merge_hook_entry(
            existing_prompt.as_ref(),
            &prompt_submit_command,
            serde_json::json!({
                "matcher": "*",
                "hooks": [{"type": "command", "command": prompt_submit_command}],
            }),
        );
        hooks_map.insert("UserPromptSubmit".to_string(), merged_prompt);

        write_settings(&config_path, &value, dry_run)?;
        patched.push(config_path.display().to_string());
    }

    let status = CheckStatus::Green;
    let summary = if patched.is_empty() {
        format!("no shadowed project-level .codex/hooks.json found ({already_ok} already carry hooks)")
    } else {
        format!(
            "{} shadowed project-level .codex/hooks.json patched ({already_ok} already fine)",
            patched.len()
        )
    };
    Ok(InstallStep {
        id: "hooks.codex_project_shadow".into(),
        status,
        summary: dry_run_summary(dry_run, &summary),
        detail: if patched.is_empty() {
            None
        } else {
            Some(patched.join(", "))
        },
    })
}

/// Wire PreToolUse + SessionStart hooks into zcode's
/// `~/.zcode/cli/config.json` and deploy the pixel rules to
/// `~/.zcode/AGENTS.md`. zcode is a Claude Code variant that uses the same
/// hooks format as Claude — hooks under `hooks.events.<Event>`, event
/// `PreToolUse` with a `matcher` field. zcode reads user-level instructions
/// from `~/.zcode/AGENTS.md` (loaded into model context every session).
fn install_zcode_hooks(home: &Path, _exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_path = home.join(config::ZCODE_CONFIG_FILE);
    if !config_path.is_file() {
        return Ok(InstallStep {
            id: "hooks.zcode".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "no zcode config.json — skipping"),
            detail: None,
        });
    }
    let mut value = read_settings(&config_path)?;

    // zcode nests hooks under `hooks.events.<Event>` (one level deeper than
    // Claude's `hooks.<Event>`). Configuration-file hooks are DISABLED by
    // default — `hooks.enabled: true` MUST be set or none of the events fire.
    let hooks_root = value
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: config_path.clone(),
            reason: "config.json root is not an object".into(),
        }))?
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: config_path.clone(),
            reason: "hooks is not an object".into(),
        }))?;
    // Enable config-file hooks (disabled by default in zcode).
    hooks_root.insert("enabled".to_string(), serde_json::Value::Bool(true));

    let hooks_obj = hooks_root
        .entry("events".to_string())
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: config_path.clone(),
            reason: "hooks.events is not an object".into(),
        }))?;

    let guard_command = format!("~/.claude/hooks/{}", config::GUARD_HOOK);
    let existing_pretooluse = hooks_obj.get("PreToolUse").cloned();
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
    hooks_obj.insert("PreToolUse".to_string(), merged_pretooluse);

    let session_start_command = format!("~/.claude/hooks/{}", config::SESSION_START_HOOK);
    let existing_session_start = hooks_obj.get("SessionStart").cloned();
    let merged_session_start = config::merge_hook_entry(
        existing_session_start.as_ref(),
        &session_start_command,
        serde_json::json!({
            "matcher": ".*",
            "hooks": [{
                "type": "command",
                "command": session_start_command,
            }],
        }),
    );
    hooks_obj.insert("SessionStart".to_string(), merged_session_start);

    // UserPromptSubmit — task boundary detector hook.
    let prompt_submit_command = format!("~/.claude/hooks/{}", config::PROMPT_SUBMIT_HOOK);
    let existing_prompt_submit = hooks_obj.get("UserPromptSubmit").cloned();
    let merged_prompt_submit = config::merge_hook_entry(
        existing_prompt_submit.as_ref(),
        &prompt_submit_command,
        serde_json::json!({
            "matcher": ".*",
            "hooks": [{
                "type": "command",
                "command": prompt_submit_command,
            }],
        }),
    );
    hooks_obj.insert("UserPromptSubmit".to_string(), merged_prompt_submit);

    // PostCompaction — re-inject targets manifest after context compaction.
    let post_compaction_command = format!("~/.claude/hooks/{}", config::POST_COMPACTION_HOOK);
    let existing_post_compaction = hooks_obj.get("PostCompaction").cloned();
    let merged_post_compaction = config::merge_hook_entry(
        existing_post_compaction.as_ref(),
        &post_compaction_command,
        serde_json::json!({
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": post_compaction_command,
            }],
        }),
    );
    hooks_obj.insert("PostCompaction".to_string(), merged_post_compaction);

    // Deploy pixel rules to ~/.zcode/AGENTS.md (zcode's user-level
    // instruction file, loaded into model context every session). Uses
    // managed markers so re-installs replace only pixel's block.
    let agents_md = home.join(".zcode").join("AGENTS.md");
    let rules_path = home.join(config::PIXEL_RULES_REL);
    let rules_content = fs::read_to_string(&rules_path).unwrap_or_default();
    // Strip frontmatter — AGENTS.md is pure markdown, no YAML. Pass only
    // the body to apply_managed_markers (it adds the markers itself).
    let managed_body = if rules_content.is_empty() {
        String::new()
    } else if rules_content.starts_with("---") {
        // Frontmatter is `---\n...\n---\n<content>`. `splitn(3, "---")`
        // gives ["", "\n...yaml...\n", "\n<content>"].
        rules_content
            .splitn(3, "---")
            .nth(2)
            .unwrap_or("")
            .trim_start()
            .to_string()
    } else {
        rules_content.clone()
    };

    if dry_run {
        return Ok(InstallStep {
            id: "hooks.zcode".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "zcode hooks + AGENTS.md rules wired"),
            detail: Some(format!(
                "would write {} + {}",
                config_path.display(),
                agents_md.display()
            )),
        });
    }

    let config_backup = write_settings(&config_path, &value, dry_run)?;

    // Write AGENTS.md with managed markers (idempotent replace of pixel's block).
    let agents_backup = if !managed_body.is_empty() {
        let existing = fs::read_to_string(&agents_md).unwrap_or_default();
        let rewritten = config::apply_managed_markers(&existing, &managed_body);
        if let Some(parent) = agents_md.parent() {
            fs::create_dir_all(parent)?;
        }
        let bk = config::backup_if_changing(&agents_md, rewritten.as_bytes())?;
        fs::write(&agents_md, &rewritten)?;
        bk
    } else {
        None
    };

    let backup_path = config_backup.or(agents_backup);
    Ok(InstallStep {
        id: "hooks.zcode".into(),
        status: CheckStatus::Green,
        summary: "zcode hooks + AGENTS.md rules wired".into(),
        detail: Some(with_backup_note(
            format!("wrote {} + {}", config_path.display(), agents_md.display()),
            backup_path,
        )),
    })
}

/// Install a pixel guard extension into pi's extensions directory AND deploy
/// the pixel rules to `~/.pi/agent/AGENTS.md` (pi's global instruction file,
/// loaded into model context at startup). pi uses a TypeScript extension API
/// with a `tool_call` event that CAN block or rewrite tool calls by mutating
/// `event.input` in place. The extension shells out to `pixel hook guard`
/// with the same JSON payload the Bash/PreToolUse hooks use, and:
///   - blocks the tool call when the guard exits non-zero (exit 2);
///   - rewrites the tool input by mutating `event.input` in place when the
///     guard emits `updatedInput` JSON (pi docs: "Mutations to event.input
///     affect the actual tool execution");
///   - allows otherwise.
///
/// pi auto-discovers extensions from `~/.pi/agent/extensions/*.ts` (global
/// scope). The extension is wrapped in managed markers so re-installs
/// replace only pixel's own content, preserving any other extension files.
/// The AGENTS.md rules use the same managed-marker approach so pi knows
/// WHEN to use pixel proactively (the extension only enforces/rewrites).
fn install_pi_rules(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_dir = home.join(config::PI_CONFIG_DIR);
    if !config_dir.is_dir() {
        return Ok(InstallStep {
            id: "hooks.pi".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "no pi config dir — skipping"),
            detail: None,
        });
    }

    let extensions_dir = config_dir.join("extensions");
    let ext_file = extensions_dir.join("pixel-guard.ts");
    let agents_md = config_dir.join("AGENTS.md");

    // The guard script path — pi extensions run in Node, so use the
    // absolute path to the pixel binary's guard subcommand.
    let exe_path = exe.display().to_string();

    let extension_body = format!(
        r#"// pixel-guard extension — managed by `pixel install`
// {begin}
// {end}
import {{ spawnSync }} from "child_process";

const PIXEL_BIN = {exe_literal};
const GUARD_TOOLS = new Set(["bash", "edit", "write", "read", "grep", "find", "ls"]);

export default function activate(pi) {{
  pi.on("tool_call", async (event, ctx) => {{
    const toolName = event.toolName;
    if (!GUARD_TOOLS.has(toolName)) return;

    // Build the PreToolUse-compatible payload that `pixel hook guard`
    // expects on stdin.
    const cwd = ctx?.cwd ?? process.cwd();
    const payload = {{
      hook_event_name: "PreToolUse",
      tool_name: toolName,
      tool_input: event.input ?? {{}},
      cwd,
    }};

    try {{
      const result = spawnSync(PIXEL_BIN, ["hook", "guard"], {{
        input: JSON.stringify(payload),
        timeout: 5000,
        encoding: "utf-8",
      }});

      // exit 2 = block the tool call
      if (result.status === 2) {{
        const reason = (result.stderr || "").trim() || "blocked by pixel guard";
        return {{ block: true, reason }};
      }}

      // exit 0 with stdout = possibly a rewrite (hookSpecificOutput.updatedInput).
      // pi docs: "Mutations to event.input affect the actual tool execution"
      // — mutate in place rather than returning a separate object.
      if (result.status === 0 && result.stdout) {{
        try {{
          const parsed = JSON.parse(result.stdout);
          const updated = parsed?.hookSpecificOutput?.updatedInput;
          if (updated && typeof updated === "object") {{
            Object.assign(event.input, updated);
            return;
          }}
        }} catch {{
          // stdout wasn't JSON — that's fine, the guard just allowed the call
        }}
      }}

      // Any other exit (including crash/timeout) = allow, don't block the
      // agent on a guard failure.
      return;
    }} catch {{
      // spawn failure — allow, don't block the agent.
      return;
    }}
  }});
}}
"#,
        begin = config::MANAGED_BEGIN,
        end = config::MANAGED_END,
        exe_literal = format!("{:?}", exe_path),
    );

    // Load the pixel rules for AGENTS.md (strip frontmatter, same as zcode).
    let rules_path = home.join(config::PIXEL_RULES_REL);
    let rules_content = fs::read_to_string(&rules_path).unwrap_or_default();
    let managed_body = if rules_content.is_empty() {
        String::new()
    } else if rules_content.starts_with("---") {
        rules_content
            .splitn(3, "---")
            .nth(2)
            .unwrap_or("")
            .trim_start()
            .to_string()
    } else {
        rules_content.clone()
    };

    if dry_run {
        return Ok(InstallStep {
            id: "hooks.pi".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "pi guard extension + AGENTS.md rules installed"),
            detail: Some(format!("would write {} + {}", ext_file.display(), agents_md.display())),
        });
    }

    // Write the extension file.
    fs::create_dir_all(&extensions_dir)?;
    let ext_backup = config::backup_if_changing(&ext_file, extension_body.as_bytes())?;
    fs::write(&ext_file, &extension_body)?;

    // Write AGENTS.md with managed markers (idempotent replace of pixel's block).
    let agents_backup = if !managed_body.is_empty() {
        let existing = fs::read_to_string(&agents_md).unwrap_or_default();
        let rewritten = config::apply_managed_markers(&existing, &managed_body);
        if let Some(parent) = agents_md.parent() {
            fs::create_dir_all(parent)?;
        }
        let bk = config::backup_if_changing(&agents_md, rewritten.as_bytes())?;
        fs::write(&agents_md, &rewritten)?;
        bk
    } else {
        None
    };

    let backup_path = ext_backup.or(agents_backup);
    Ok(InstallStep {
        id: "hooks.pi".into(),
        status: CheckStatus::Green,
        summary: "pi guard extension + AGENTS.md rules installed".into(),
        detail: Some(with_backup_note(
            format!("wrote {} + {}", ext_file.display(), agents_md.display()),
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
    /// True if a `.gitpixel/` directory was found and deleted.
    pub old_state_removed: bool,
    /// True if `.pixel/` was rebuilt fresh.
    pub new_state_rebuilt: bool,
}

/// Migrate a repo from the old `.gitpixel/` state to a fresh `.pixel/` state.
///
/// Deletes `.gitpixel/` and rebuilds `.pixel/` fresh. No state migration —
/// every index is a cache and is rebuilt on first use. (The old gain-ledger
/// carry-over was removed together with the gain module: an unmeasured
/// token-savings ledger was exactly the kind of claim-without-measurement
/// the doctrine now forbids.)
pub fn migrate(repo_root: &Path) -> Result<MigrateReport> {
    let old_dir = repo_root.join(".gitpixel");
    let new_dir = repo_root.join(".pixel");

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
        old_state_removed,
        new_state_rebuilt,
    })
}
