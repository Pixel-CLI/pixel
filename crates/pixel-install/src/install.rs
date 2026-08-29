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
}

impl Default for InstallOptions {
    fn default() -> Self {
        InstallOptions {
            executable_path: None,
            home: None,
            capability_block: None,
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

    let mut steps = Vec::new();

    // 1. Register the pixel MCP server in settings.json.
    steps.push(register_mcp_server(&home, &exe)?);

    // 2. Remove deprecated MCP servers + old guard hooks from settings.json.
    steps.push(scrub_deprecated(&home)?);

    // 3. Replace the old guard hook with `exec pixel hook guard "$@"`.
    steps.push(install_guard_hook(&home, &exe)?);

    // 4. Install the SessionStart hook.
    steps.push(install_session_start_hook(&home, &exe, options.capability_block.as_deref())?);

    // 5. Rewrite agent-config with managed markers.
    steps.push(rewrite_agent_configs(&home, &exe)?);

    let green = steps.iter().filter(|s| s.status == CheckStatus::Green).count();
    let yellow = steps.iter().filter(|s| s.status == CheckStatus::Yellow).count();
    let red = steps.iter().filter(|s| s.status == CheckStatus::Red).count();
    let ok = red == 0;

    Ok(InstallReport {
        version: "v1".into(),
        ok,
        executable_path: exe.display().to_string(),
        home: home.display().to_string(),
        steps,
        summary: InstallSummary { green, yellow, red },
    })
}

fn register_mcp_server(home: &Path, exe: &Path) -> Result<InstallStep> {
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
    write_settings(&settings, &value)?;
    let detail = if existing.is_some() {
        Some("pixel MCP server already registered; updated command".into())
    } else {
        Some("registered pixel MCP server".into())
    };
    Ok(InstallStep {
        id: "mcp.pixel".into(),
        status: CheckStatus::Green,
        summary: "pixel MCP server registered".into(),
        detail,
    })
}

fn scrub_deprecated(home: &Path) -> Result<InstallStep> {
    let settings = home.join(".claude").join("settings.json");
    let outcome = config::scrub_settings_json(&settings)?;
    let removed = outcome.mcp_servers_removed + outcome.guard_hooks_removed;
    Ok(InstallStep {
        id: "mcp.deprecated".into(),
        status: CheckStatus::Green,
        summary: format!(
            "removed {removed} deprecated MCP/hook entr{}",
            if removed == 1 { "y" } else { "ies" }
        ),
        detail: Some(format!(
            "mcp_servers_removed={} guard_hooks_removed={}",
            outcome.mcp_servers_removed, outcome.guard_hooks_removed
        )),
    })
}

fn install_guard_hook(home: &Path, exe: &Path) -> Result<InstallStep> {
    let hooks_dir = home.join(config::CLAUDE_HOOKS_DIR);
    fs::create_dir_all(&hooks_dir)?;
    let old = hooks_dir.join(config::OLD_GUARD_HOOK);
    let new = hooks_dir.join(config::GUARD_HOOK);
    let body = format!("#!/bin/sh\nexec {} hook guard \"$@\"\n", exe.display());
    let mut replaced_old = false;
    if old.exists() {
        let _ = fs::remove_file(&old);
        replaced_old = true;
    }
    fs::write(&new, &body)?;
    set_executable(&new);
    Ok(InstallStep {
        id: "hook.guard".into(),
        status: CheckStatus::Green,
        summary: "guard hook installed".into(),
        detail: Some(format!(
            "wrote {} (replaced_old={replaced_old})",
            new.display()
        )),
    })
}

fn install_session_start_hook(
    home: &Path,
    exe: &Path,
    capability_block: Option<&str>,
) -> Result<InstallStep> {
    let hooks_dir = home.join(config::CLAUDE_HOOKS_DIR);
    fs::create_dir_all(&hooks_dir)?;
    let path = hooks_dir.join(config::SESSION_START_HOOK);
    let body = format!("#!/bin/sh\nexec {} hook session-start \"$@\"\n", exe.display());
    fs::write(&path, &body)?;
    set_executable(&path);

    // Register the SessionStart hook in settings.json so Claude invokes it.
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
    obj.insert(
        "SessionStart".to_string(),
        serde_json::json!([{
            "matcher": "SessionStart",
            "hooks": [{
                "type": "command",
                "command": format!("{} hook session-start", exe.display()),
            }],
        }]),
    );
    write_settings(&settings, &value)?;

    let _ = capability_block; // emitted by the hook itself from the op registry
    Ok(InstallStep {
        id: "hook.session-start".into(),
        status: CheckStatus::Green,
        summary: "SessionStart hook installed".into(),
        detail: Some(format!("wrote {}", path.display())),
    })
}

fn rewrite_agent_configs(home: &Path, exe: &Path) -> Result<InstallStep> {
    let managed = format!(
        "pixel is the unified retrieval + git engine. Use `pixel <verb>` for\n\
         search, resolve, targets, history, and safe git ops.\n\
         Binary: {}\n",
        exe.display()
    );
    let mut rewritten = 0usize;
    let mut stale_removed = 0usize;
    for path in config::find_agent_configs(home) {
        let outcome = config::rewrite_agent_config(&path, &managed)?;
        if outcome.rewritten {
            rewritten += 1;
        }
        stale_removed += outcome.stale_blocks_removed;
    }
    Ok(InstallStep {
        id: "agent-config".into(),
        status: CheckStatus::Green,
        summary: format!("rewrote {rewritten} agent-config file(s)"),
        detail: Some(format!("stale_blocks_removed={stale_removed}")),
    })
}

fn read_settings(path: &Path) -> Result<serde_json::Value> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(serde_json::from_str(&s)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(e.into()),
    }
}

fn write_settings(path: &Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let serialized = serde_json::to_string_pretty(value)?;
    fs::write(path, format!("{serialized}\n"))?;
    Ok(())
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
