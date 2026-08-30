//! Idempotent `pixel install` — registers ONE MCP server `pixel`, removes the
//! deprecated usable-git/gitpixel/sniper MCP entries, installs the guard and
//! SessionStart hooks, and rewrites agent-config with managed markers.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config;
use crate::InstallError;

/// The single MCP server pixel registers.
pub const MCP_SERVER_NAME: &str = "pixel";

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
    // Preflight: does this binary actually implement `<exe> mcp` as a real
    // subcommand? Confirmed live against a real release build that it does
    // NOT (`error: unrecognized subcommand 'mcp'`) — registering
    // `{"command": exe, "args": ["mcp"]}` when that's true would install an
    // MCP server entry that can never start. Gate both registering the new
    // entry AND removing the old (working) usable-git/gitpixel/sniper
    // entries on this check, so a binary without `mcp` wired up never
    // leaves the user with zero working MCP retrieval tools.
    let mcp_ready = binary_supports_mcp_subcommand(&exe);
    let mut steps = Vec::new();

    // 1. Register the pixel MCP server in settings.json.
    steps.push(register_mcp_server(&home, &exe, dry_run, mcp_ready)?);

    // 2. Remove deprecated MCP servers + old guard hooks from settings.json.
    steps.push(scrub_deprecated(&home, dry_run, mcp_ready)?);

    // 3. Replace the old guard hook with `exec pixel hook guard "$@"`.
    steps.push(install_guard_hook(&home, &exe, dry_run)?);

    // 4. Install the SessionStart hook.
    steps.push(install_session_start_hook(
        &home,
        &exe,
        options.capability_block.as_deref(),
        dry_run,
    )?);

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

/// Probe whether the installed binary actually implements `pixel mcp` as a
/// real subcommand, WITHOUT ever letting the probe block indefinitely: an
/// MCP server is a long-lived stdio process, so if `mcp` really existed and
/// somehow ignored `--help`, waiting on it forever is a real risk. This
/// defends against that with a null stdin/stdout/stderr (so a read on stdin
/// sees immediate EOF rather than blocking) plus an explicit wall-clock
/// timeout that kills the child if it overruns.
fn binary_supports_mcp_subcommand(exe: &Path) -> bool {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let mut child = match std::process::Command::new(exe)
        .args(["mcp", "--help"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return false,
        }
    }
}

fn register_mcp_server(home: &Path, exe: &Path, dry_run: bool, mcp_ready: bool) -> Result<InstallStep> {
    if !mcp_ready {
        return Ok(InstallStep {
            id: "mcp.pixel".into(),
            status: CheckStatus::Red,
            summary: "pixel MCP server NOT registered — `mcp` subcommand missing from this binary".into(),
            detail: Some(format!(
                "skipped writing {{\"command\": \"{}\", \"args\": [\"mcp\"]}} into mcpServers.pixel because \
                 `{} mcp --help` failed (unrecognized subcommand). Registering it anyway would install an MCP \
                 server entry that can never start. This is a blocker in crates/pixel/src/main.rs's CLI: the \
                 `mcp` subcommand must exist and actually run an MCP server before `pixel install` can safely \
                 register it.",
                exe.display(),
                exe.display(),
            )),
        });
    }
    let settings = home.join(".claude").join("settings.json");
    let mut value = read_settings(&settings)?;
    let servers = value
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: settings.clone(),
            reason: "settings.json root is not an object".into(),
        }))?;
    let mcp = servers
        .entry("mcpServers".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let obj = mcp
        .as_object_mut()
        .ok_or_else(|| InstallError::Config(config::ConfigError::InvalidSettings {
            path: settings.clone(),
            reason: "mcpServers is not an object".into(),
        }))?;
    let existing = obj.get(MCP_SERVER_NAME).cloned();
    obj.insert(
        MCP_SERVER_NAME.to_string(),
        serde_json::json!({
            "command": exe.display().to_string(),
            "args": ["mcp"],
        }),
    );
    let backup_path = write_settings(&settings, &value, dry_run)?;
    let action = if existing.is_some() {
        "pixel MCP server already registered; updated command"
    } else {
        "registered pixel MCP server"
    };
    Ok(InstallStep {
        id: "mcp.pixel".into(),
        status: CheckStatus::Green,
        summary: dry_run_summary(dry_run, "pixel MCP server registered"),
        detail: Some(with_backup_note(action.to_string(), backup_path)),
    })
}

fn scrub_deprecated(home: &Path, dry_run: bool, mcp_ready: bool) -> Result<InstallStep> {
    let settings = home.join(".claude").join("settings.json");
    // Only remove the old (working) usable-git/gitpixel/sniper MCP server
    // entries once pixel's own MCP server is confirmed capable of starting
    // — otherwise this step would leave the user with zero working MCP
    // retrieval tools. The guard-hook command rewrite is unrelated to MCP
    // registration and always proceeds regardless.
    let outcome = config::scrub_settings_json(&settings, dry_run, mcp_ready)?;
    let removed = outcome.mcp_servers_removed + outcome.guard_hooks_removed;
    let mut summary = format!(
        "removed {removed} deprecated MCP/hook entr{}",
        if removed == 1 { "y" } else { "ies" }
    );
    if !mcp_ready {
        summary.push_str(" (deprecated MCP servers kept: pixel's own MCP server isn't runnable yet)");
    }
    Ok(InstallStep {
        id: "mcp.deprecated".into(),
        status: CheckStatus::Green,
        summary: dry_run_summary(dry_run, &summary),
        detail: Some(with_backup_note(
            format!(
                "mcp_servers_removed={} guard_hooks_removed={} mcp_ready={mcp_ready}",
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

    if dry_run {
        return Ok(InstallStep {
            id: "hook.guard".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "guard hook installed"),
            detail: Some(format!(
                "would write {} (replaced_old={replaced_old})",
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
    let backup_path = new_backup.or(old_backup);
    Ok(InstallStep {
        id: "hook.guard".into(),
        status: CheckStatus::Green,
        summary: "guard hook installed".into(),
        detail: Some(with_backup_note(
            format!("wrote {} (replaced_old={replaced_old})", new.display()),
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

fn rewrite_agent_configs(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let managed = format!(
        "pixel is the unified retrieval + git engine. Use `pixel <verb>` for\n\
         search, resolve, targets, history, and safe git ops.\n\
         Binary: {}\n",
        exe.display()
    );
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
