//! `pixel doctor` — checks install state, binary path, daemon health, and
//! index/graph/facts freshness, reporting green/red per check.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::config;
use crate::gain::GainLedger;
use crate::InstallError;

pub type Result<T> = std::result::Result<T, InstallError>;

/// Per-check status for the doctor report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Green,
    Yellow,
    Red,
}

/// One doctor check.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub id: String,
    pub status: CheckStatus,
    pub required: bool,
    pub duration_ms: u64,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// The full doctor report.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub version: String,
    pub ok: bool,
    pub executable_path: String,
    pub home: String,
    pub checks: Vec<DoctorCheck>,
    pub summary: DoctorSummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorSummary {
    pub green: usize,
    pub yellow: usize,
    pub red: usize,
}

/// Options controlling a doctor run.
#[derive(Debug, Clone)]
pub struct DoctorOptions {
    /// Path to the pixel binary to check. Defaults to the current exe.
    pub executable_path: Option<PathBuf>,
    /// Home directory. Defaults to `$HOME`.
    pub home: Option<PathBuf>,
    /// Repo root to check index/graph/facts freshness for. If None, only
    /// install-state checks run.
    pub repo_root: Option<PathBuf>,
}

impl Default for DoctorOptions {
    fn default() -> Self {
        DoctorOptions {
            executable_path: None,
            home: None,
            repo_root: None,
        }
    }
}

/// Run `pixel doctor`.
pub fn doctor(options: &DoctorOptions) -> Result<DoctorReport> {
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

    let mut checks = Vec::new();

    checks.push(check("binary.path", || -> std::result::Result<DoctorCheckDetail, String> {
        if !exe.is_file() {
            return Err(format!("binary not found at {}", exe.display()));
        }
        Ok(DoctorCheckDetail {
            summary: format!("binary present at {}", exe.display()),
            detail: Some(serde_json::json!({ "path": exe.display().to_string() })),
        })
    }));

    checks.push(check("binary.executable", || -> std::result::Result<DoctorCheckDetail, String> {
        let out = Command::new(&exe).arg("--version").output();
        match out {
            Ok(o) if o.status.success() => Ok(DoctorCheckDetail {
                summary: format!(
                    "binary runs ({} bytes stdout)",
                    String::from_utf8_lossy(&o.stdout).trim().len()
                ),
                detail: None,
            }),
            Ok(o) => Err(format!(
                "binary exited {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => Err(format!("failed to run binary: {e}")),
        }
    }));

    checks.push(check("install.mcp", || -> std::result::Result<DoctorCheckDetail, String> {
        let settings = home.join(".claude").join("settings.json");
        if !settings.is_file() {
            return Err("no .claude/settings.json found".into());
        }
        let value: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&settings).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let servers = value
            .get("mcpServers")
            .and_then(serde_json::Value::as_object)
            .ok_or("mcpServers missing from settings.json")?;
        if !servers.contains_key("pixel") {
            return Err("pixel MCP server not registered".into());
        }
        let deprecated: Vec<String> = servers
            .keys()
            .filter(|k| config::DEPRECATED_MCP_SERVERS.contains(&k.as_str()))
            .cloned()
            .collect();
        if !deprecated.is_empty() {
            return Err(format!(
                "deprecated MCP servers still present: {}",
                deprecated.join(", ")
            ));
        }
        Ok(DoctorCheckDetail {
            summary: "pixel MCP server registered; no deprecated servers".into(),
            detail: Some(serde_json::json!({ "servers": servers.keys().collect::<Vec<_>>() })),
        })
    }));

    checks.push(check("install.guard-hook", || -> std::result::Result<DoctorCheckDetail, String> {
        let hooks_dir = home.join(config::CLAUDE_HOOKS_DIR);
        let new = hooks_dir.join(config::GUARD_HOOK);
        let old = hooks_dir.join(config::OLD_GUARD_HOOK);
        if old.exists() {
            return Err("old gitpixel-targets-guard hook still present".into());
        }
        if !new.is_file() {
            return Err("pixel guard hook not installed".into());
        }
        Ok(DoctorCheckDetail {
            summary: "guard hook installed; old guard removed".into(),
            detail: Some(serde_json::json!({ "path": new.display().to_string() })),
        })
    }));

    checks.push(check("install.session-start", || -> std::result::Result<DoctorCheckDetail, String> {
        let hooks_dir = home.join(config::CLAUDE_HOOKS_DIR);
        let path = hooks_dir.join(config::SESSION_START_HOOK);
        if !path.is_file() {
            return Err("SessionStart hook not installed".into());
        }
        Ok(DoctorCheckDetail {
            summary: "SessionStart hook installed".into(),
            detail: Some(serde_json::json!({ "path": path.display().to_string() })),
        })
    }));

    checks.push(check("install.agent-config", || -> std::result::Result<DoctorCheckDetail, String> {
        let configs = config::find_agent_configs(&home);
        if configs.is_empty() {
            return Err("no CLAUDE.md/AGENTS.md found to manage".into());
        }
        let unmanaged: Vec<&PathBuf> = configs
            .iter()
            .filter(|p| {
                fs::read_to_string(p)
                    .map(|s| !s.contains(config::MANAGED_BEGIN))
                    .unwrap_or(true)
            })
            .collect();
        if !unmanaged.is_empty() {
            return Err(format!(
                "agent-config not managed: {}",
                unmanaged
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Ok(DoctorCheckDetail {
            summary: format!("{} agent-config file(s) managed", configs.len()),
            detail: Some(serde_json::json!({ "files": configs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>() })),
        })
    }));

    checks.push(check("ledger.readable", || -> std::result::Result<DoctorCheckDetail, String> {
        let ledger = GainLedger::open().map_err(|e| e.to_string())?;
        let events = ledger.read().map_err(|e| e.to_string())?;
        Ok(DoctorCheckDetail {
            summary: format!("gain ledger readable ({} events)", events.len()),
            detail: Some(serde_json::json!({
                "events": events.len(),
                "path": ledger.path().display().to_string(),
                "directory": ledger.directory().display().to_string(),
            })),
        })
    }));

    if let Some(root) = &options.repo_root {
        checks.push(check("daemon.health", || -> std::result::Result<DoctorCheckDetail, String> {
            let sock = pixel_daemon::daemon::socket_path(root);
            if !sock.exists() {
                return Err(format!("no daemon socket at {}", sock.display()));
            }
            Ok(DoctorCheckDetail {
                summary: format!("daemon socket present at {}", sock.display()),
                detail: Some(serde_json::json!({ "socket": sock.display().to_string() })),
            })
        }));

        checks.push(check("index.freshness", || -> std::result::Result<DoctorCheckDetail, String> {
            let shard = root.join(pixel_index::index::SHARD_DIR).join(pixel_index::index::SHARD_FILE);
            if !shard.is_file() {
                return Err("index not built".into());
            }
            let mtime = fs::metadata(&shard)
                .map_err(|e| e.to_string())?
                .modified()
                .map_err(|e| e.to_string())?;
            let age = age_secs(mtime);
            Ok(DoctorCheckDetail {
                summary: format!("index present ({}s old)", age),
                detail: Some(serde_json::json!({ "age_secs": age })),
            })
        }));

        checks.push(check("graph.freshness", || -> std::result::Result<DoctorCheckDetail, String> {
            let db = root.join(pixel_index::index::SHARD_DIR).join("graph.db");
            if !db.is_file() {
                return Err("graph not built".into());
            }
            let mtime = fs::metadata(&db)
                .map_err(|e| e.to_string())?
                .modified()
                .map_err(|e| e.to_string())?;
            let age = age_secs(mtime);
            Ok(DoctorCheckDetail {
                summary: format!("graph present ({}s old)", age),
                detail: Some(serde_json::json!({ "age_secs": age })),
            })
        }));

        checks.push(check("facts.freshness", || -> std::result::Result<DoctorCheckDetail, String> {
            let db = root.join(pixel_index::index::SHARD_DIR).join("history.db");
            if !db.is_file() {
                return Err("facts/history db not built".into());
            }
            let mtime = fs::metadata(&db)
                .map_err(|e| e.to_string())?
                .modified()
                .map_err(|e| e.to_string())?;
            let age = age_secs(mtime);
            Ok(DoctorCheckDetail {
                summary: format!("facts db present ({}s old)", age),
                detail: Some(serde_json::json!({ "age_secs": age })),
            })
        }));
    }

    let green = checks.iter().filter(|c| c.status == CheckStatus::Green).count();
    let yellow = checks.iter().filter(|c| c.status == CheckStatus::Yellow).count();
    let red = checks.iter().filter(|c| c.status == CheckStatus::Red).count();
    let ok = red == 0;

    Ok(DoctorReport {
        version: "v1".into(),
        ok,
        executable_path: exe.display().to_string(),
        home: home.display().to_string(),
        checks,
        summary: DoctorSummary { green, yellow, red },
    })
}

struct DoctorCheckDetail {
    summary: String,
    detail: Option<serde_json::Value>,
}

fn check(
    id: &str,
    run: impl FnOnce() -> std::result::Result<DoctorCheckDetail, String>,
) -> DoctorCheck {
    let started = Instant::now();
    match run() {
        Ok(d) => DoctorCheck {
            id: id.into(),
            status: CheckStatus::Green,
            required: true,
            duration_ms: started.elapsed().as_millis() as u64,
            summary: d.summary,
            reason: None,
            detail: d.detail,
        },
        Err(reason) => DoctorCheck {
            id: id.into(),
            status: CheckStatus::Red,
            required: true,
            duration_ms: started.elapsed().as_millis() as u64,
            summary: "check failed".into(),
            reason: Some(reason),
            detail: None,
        },
    }
}

fn age_secs(mtime: SystemTime) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let m = mtime
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.saturating_sub(m)
}

/// Re-export the daemon socket-path helper for the CLI.
pub use pixel_daemon::daemon::socket_path as daemon_socket_path;
