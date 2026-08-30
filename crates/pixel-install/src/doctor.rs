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
        // pixel is a CLI + hooks tool, not an MCP server. This check now
        // only verifies that no deprecated usable-git/gitpixel/sniper MCP
        // server entries linger in settings.json — it does NOT require
        // pixel itself to be registered as an MCP server (that would give
        // agents a transport that bypasses the PreToolUse guard hook).
        let settings = home.join(".claude").join("settings.json");
        if !settings.is_file() {
            return Ok(DoctorCheckDetail {
                summary: "no .claude/settings.json — nothing to scrub".into(),
                detail: None,
            });
        }
        let value: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&settings).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let servers = value
            .get("mcpServers")
            .and_then(serde_json::Value::as_object);
        let deprecated: Vec<String> = servers
            .map(|s| {
                s.keys()
                    .filter(|k| config::DEPRECATED_MCP_SERVERS.contains(&k.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if !deprecated.is_empty() {
            return Err(format!(
                "deprecated MCP servers still present: {}",
                deprecated.join(", ")
            ));
        }
        Ok(DoctorCheckDetail {
            summary: "no deprecated MCP servers present".into(),
            detail: Some(serde_json::json!({ "servers": servers.map(|s| s.keys().collect::<Vec<_>>()).unwrap_or_default() })),
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

    checks.push(check("install.devin-hooks", || -> std::result::Result<DoctorCheckDetail, String> {
        let config_path = home.join(config::DEVIN_CONFIG_DIR).join(config::DEVIN_CONFIG_FILE);
        if !config_path.is_file() {
            return Ok(DoctorCheckDetail {
                summary: "no Devin config.json — skipping".into(),
                detail: None,
            });
        }
        let raw = fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
        let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        let hooks = value.get("hooks").and_then(serde_json::Value::as_object);
        if hooks.is_none() {
            return Err("Devin config.json has no hooks key".into());
        }
        let hooks = hooks.unwrap();
        let guard_command = format!("~/.claude/hooks/{}", config::GUARD_HOOK);
        let has_guard = hooks.get("PreToolUse")
            .and_then(serde_json::Value::as_array)
            .map(|entries| entries.iter().any(|e| {
                e.get("hooks")
                    .and_then(serde_json::Value::as_array)
                    .map(|hs| hs.iter().any(|h| {
                        h.get("command").and_then(|c| c.as_str()).map(|c| c.contains(&guard_command)).unwrap_or(false)
                    }))
                    .unwrap_or(false)
            }))
            .unwrap_or(false);
        if !has_guard {
            return Err("Devin PreToolUse guard hook not wired".into());
        }
        let session_command = format!("~/.claude/hooks/{}", config::SESSION_START_HOOK);
        let has_session = hooks.get("SessionStart")
            .and_then(serde_json::Value::as_array)
            .map(|entries| entries.iter().any(|e| {
                e.get("hooks")
                    .and_then(serde_json::Value::as_array)
                    .map(|hs| hs.iter().any(|h| {
                        h.get("command").and_then(|c| c.as_str()).map(|c| c.contains(&session_command)).unwrap_or(false)
                    }))
                    .unwrap_or(false)
            }))
            .unwrap_or(false);
        if !has_session {
            return Err("Devin SessionStart hook not wired".into());
        }
        Ok(DoctorCheckDetail {
            summary: "Devin hooks wired (PreToolUse + SessionStart)".into(),
            detail: Some(serde_json::json!({ "path": config_path.display().to_string() })),
        })
    }));

    checks.push(check("install.codex-hooks", || -> std::result::Result<DoctorCheckDetail, String> {
        let config_path = home.join(config::CODEX_HOOKS_FILE);
        if !config_path.is_file() {
            return Ok(DoctorCheckDetail {
                summary: "no Codex hooks.json — skipping".into(),
                detail: None,
            });
        }
        let raw = fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
        let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        let hooks = value.get("hooks").and_then(serde_json::Value::as_object);
        if hooks.is_none() {
            return Err("Codex hooks.json has no hooks key".into());
        }
        let hooks = hooks.unwrap();
        let guard_command = format!("~/.claude/hooks/{}", config::GUARD_HOOK);
        let has_guard = hooks.get("PreToolUse")
            .and_then(serde_json::Value::as_array)
            .map(|entries| entries.iter().any(|e| {
                e.get("hooks").and_then(serde_json::Value::as_array)
                    .map(|hs| hs.iter().any(|h| h.get("command").and_then(|c| c.as_str()).map(|c| c.contains(&guard_command)).unwrap_or(false)))
                    .unwrap_or(false)
            }))
            .unwrap_or(false);
        if !has_guard {
            return Err("Codex PreToolUse guard hook not wired".into());
        }
        Ok(DoctorCheckDetail {
            summary: "Codex hooks wired (PreToolUse)".into(),
            detail: Some(serde_json::json!({ "path": config_path.display().to_string() })),
        })
    }));

    checks.push(check("install.gemini-hooks", || -> std::result::Result<DoctorCheckDetail, String> {
        let config_path = home.join(config::GEMINI_SETTINGS_FILE);
        if !config_path.is_file() {
            return Ok(DoctorCheckDetail {
                summary: "no Gemini settings.json — skipping".into(),
                detail: None,
            });
        }
        let raw = fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
        let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        let hooks = value.get("hooks").and_then(serde_json::Value::as_object);
        if hooks.is_none() {
            return Err("Gemini settings.json has no hooks key".into());
        }
        let hooks = hooks.unwrap();
        let guard_command = format!("~/.claude/hooks/{}", config::GUARD_HOOK);
        let has_guard = hooks.get("BeforeTool")
            .and_then(serde_json::Value::as_array)
            .map(|entries| entries.iter().any(|e| {
                e.get("hooks").and_then(serde_json::Value::as_array)
                    .map(|hs| hs.iter().any(|h| h.get("command").and_then(|c| c.as_str()).map(|c| c.contains(&guard_command)).unwrap_or(false)))
                    .unwrap_or(false)
            }))
            .unwrap_or(false);
        if !has_guard {
            return Err("Gemini BeforeTool guard hook not wired".into());
        }
        Ok(DoctorCheckDetail {
            summary: "Gemini hooks wired (BeforeTool)".into(),
            detail: Some(serde_json::json!({ "path": config_path.display().to_string() })),
        })
    }));

    checks.push(check("install.zcode-hooks", || -> std::result::Result<DoctorCheckDetail, String> {
        let config_path = home.join(config::ZCODE_CONFIG_FILE);
        if !config_path.is_file() {
            return Ok(DoctorCheckDetail {
                summary: "no zcode config.json — skipping".into(),
                detail: None,
            });
        }
        let raw = fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
        let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        // zcode nests hooks under `hooks.events.<Event>`.
        let hooks = value
            .get("hooks")
            .and_then(|v| v.get("events"))
            .and_then(serde_json::Value::as_object);
        if hooks.is_none() {
            return Ok(DoctorCheckDetail {
                summary: "zcode config.json has no hooks.events — skipping".into(),
                detail: None,
            });
        }
        let hooks = hooks.unwrap();
        let guard_command = format!("~/.claude/hooks/{}", config::GUARD_HOOK);
        let has_guard = hooks.get("PreToolUse")
            .and_then(serde_json::Value::as_array)
            .map(|entries| entries.iter().any(|e| {
                e.get("hooks").and_then(serde_json::Value::as_array)
                    .map(|hs| hs.iter().any(|h| h.get("command").and_then(|c| c.as_str()).map(|c| c.contains(&guard_command)).unwrap_or(false)))
                    .unwrap_or(false)
            }))
            .unwrap_or(false);
        if !has_guard {
            return Err("zcode PreToolUse guard hook not wired".into());
        }
        Ok(DoctorCheckDetail {
            summary: "zcode hooks wired (PreToolUse)".into(),
            detail: Some(serde_json::json!({ "path": config_path.display().to_string() })),
        })
    }));

    checks.push(check("install.pi-rules", || -> std::result::Result<DoctorCheckDetail, String> {
        let config_dir = home.join(config::PI_CONFIG_DIR);
        if !config_dir.is_dir() {
            return Ok(DoctorCheckDetail {
                summary: "no pi config dir — skipping".into(),
                detail: None,
            });
        }
        let memory_file = config_dir.join("memory").join("pixel-rules.md");
        if !memory_file.is_file() {
            return Err("pi pixel-rules.md not installed".into());
        }
        let raw = fs::read_to_string(&memory_file).map_err(|e| e.to_string())?;
        if !raw.contains(config::MANAGED_BEGIN) {
            return Err("pi pixel-rules.md missing managed markers".into());
        }
        Ok(DoctorCheckDetail {
            summary: "pi rules installed (no guard hooks — extension API only)".into(),
            detail: Some(serde_json::json!({ "path": memory_file.display().to_string() })),
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

        checks.push(check_status("facts.freshness", || -> std::result::Result<(CheckStatus, DoctorCheckDetail), String> {
            let store = pixel_facts::FactsStore::open(root).map_err(|e| e.to_string())?;
            let state = store.index_state();
            // Red: schema version mismatch — the db was written by a different
            // build and must be rebuilt before it can be trusted.
            if state.schema_version != pixel_facts::store::FACTS_SCHEMA_VERSION {
                return Err(format!(
                    "facts schema version mismatch: on-disk {} != expected {} (rebuild required)",
                    state.schema_version,
                    pixel_facts::store::FACTS_SCHEMA_VERSION
                ));
            }
            // Counter-based dead/poisoned detection: mtime and diff_state
            // alone lie (the historical poisoned DB had every commit marked
            // INDEXED with empty hunk text), so measure the actual text and
            // gram rows.
            let count = |sql: &str| -> i64 {
                store.conn().query_row(sql, [], |r| r.get(0)).unwrap_or(0)
            };
            let hunks_with_text = count(
                "SELECT count(*) FROM hunks WHERE length(added) > 0 OR length(removed) > 0",
            );
            let diff_grams = count("SELECT count(*) FROM diff_grams");
            let repo_commits = Command::new("git")
                .args(["rev-list", "--count", "--all"])
                .current_dir(root)
                .output()
                .ok()
                .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok())
                .unwrap_or(0);
            if let Some(reason) =
                facts_dead_reason(state.commits_indexed, repo_commits, diff_grams)
            {
                return Err(reason);
            }
            let detail = Some(serde_json::json!({
                "phase": state.phase,
                "commits_indexed": state.commits_indexed,
                "total_commits": repo_commits.max(state.total_commits),
                "diff_indexed_pct": state.diff_indexed_pct,
                "hunks_with_text": hunks_with_text,
                "diff_grams": diff_grams,
                "fresh": state.fresh,
                "schema_version": state.schema_version,
            }));
            if !state.fresh {
                // Yellow: stale — ingest has not caught up to the current refs.
                return Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: format!(
                        "facts db present but stale (phase {}, {} commits, {:.0}% diff coverage)",
                        state.phase,
                        state.commits_indexed,
                        state.diff_indexed_pct * 100.0
                    ),
                    detail,
                }));
            }
            Ok((CheckStatus::Green, DoctorCheckDetail {
                summary: format!(
                    "facts db fresh ({} commits, {:.0}% diff coverage, {} hunks with text, {} grams)",
                    state.commits_indexed,
                    state.diff_indexed_pct * 100.0,
                    hunks_with_text,
                    diff_grams
                ),
                detail,
            }))
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

/// Like `check`, but the closure may also report a non-fatal `Yellow` status
/// (e.g. a stale-but-valid index) in addition to `Green`/`Red`.
fn check_status(
    id: &str,
    run: impl FnOnce() -> std::result::Result<(CheckStatus, DoctorCheckDetail), String>,
) -> DoctorCheck {
    let started = Instant::now();
    match run() {
        Ok((status, d)) => DoctorCheck {
            id: id.into(),
            status,
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

/// The dead/poisoned-DB predicate for `facts.freshness`, factored out so it
/// is unit-testable without a real repo:
/// - a repo with commits but an empty facts db is DEAD (never ingested, or a
///   just-wiped poisoned db that nothing has re-ingested yet);
/// - indexed commits with ZERO diff-gram postings is the poisoned signature
///   (the historical bug stored every hunk with empty added/removed text, so
///   `diff_grams` had no rows and excavate/search returned nothing forever
///   while diff_state claimed INDEXED).
///
/// Returns `Some(reason)` when the check must go RED.
pub fn facts_dead_reason(
    commits_indexed: u64,
    repo_commits: u64,
    diff_grams: i64,
) -> Option<String> {
    if commits_indexed == 0 && repo_commits > 0 {
        return Some(format!(
            "facts db has 0 commits indexed but the repo has {repo_commits} — \
             history queries will return nothing; run `pixel index --history`"
        ));
    }
    if commits_indexed > 0 && diff_grams == 0 {
        return Some(format!(
            "facts db poisoned: {commits_indexed} commits indexed but 0 diff-gram \
             postings — diff text was never stored; delete .pixel/history.db or \
             re-run `pixel index --history`"
        ));
    }
    None
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

#[cfg(test)]
mod tests {
    use super::facts_dead_reason;

    #[test]
    fn poisoned_db_signature_is_red() {
        // The real-world poisoned DB: 11 commits marked indexed, 323 hunks all
        // with empty text, therefore 0 diff_grams rows.
        let reason = facts_dead_reason(11, 21, 0);
        assert!(
            reason.as_deref().unwrap_or("").contains("poisoned"),
            "indexed commits with zero grams must be flagged poisoned, got {reason:?}"
        );
    }

    #[test]
    fn empty_db_in_nonempty_repo_is_red() {
        let reason = facts_dead_reason(0, 21, 0);
        assert!(
            reason.is_some(),
            "0 indexed commits while the repo has commits must be RED"
        );
    }

    #[test]
    fn healthy_and_trivially_empty_cases_are_not_red() {
        assert_eq!(facts_dead_reason(21, 21, 50_000), None, "healthy db");
        assert_eq!(facts_dead_reason(0, 0, 0), None, "empty repo, empty db");
    }
}
