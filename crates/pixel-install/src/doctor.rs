//! `pixel doctor` — checks install state, binary path, daemon health, and
//! index/graph/facts freshness, reporting green/red per check.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::config;
use crate::InstallError;

pub type Result<T> = std::result::Result<T, InstallError>;

/// The five mandatory scenarios the rule text and the SessionStart usage
/// string must agree on. One name per scenario (the guard-verb that anchors
/// it): targets (sniper scoping — mandatory first call, advisory fence),
/// resolve (phrase → code), rescue (history recovery, includes excavate),
/// reconcile (branch sync), impact (blast radius, includes changes).
pub const MANDATORY_SCENARIOS: &[&str] =
    &["targets", "resolve", "rescue", "reconcile", "impact"];

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
    /// Dry-run parser for one `pixel …` argv (including the leading
    /// "pixel"), supplied by the CLI binary from its real clap definition.
    /// When present, the `rule.parity` check parses every pixel command
    /// line found in the installed rule text against it — documented
    /// syntax the binary rejects goes red. When None (library callers
    /// without access to the CLI parser), the parity check is skipped.
    pub syntax_validator: Option<fn(&[String]) -> std::result::Result<(), String>>,
}

impl Default for DoctorOptions {
    fn default() -> Self {
        DoctorOptions {
            executable_path: None,
            home: None,
            repo_root: None,
            syntax_validator: None,
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

    checks.push(check("install.rules-conflict", || -> std::result::Result<DoctorCheckDetail, String> {
        // A retired-tool rule file left in a per-tool rules directory keeps
        // offering the model a tool pixel replaced. Devin in particular
        // advertises every file under `~/.devin/rules/` to the model as an
        // available rule it may read, so a stale `usable-git.md` competes
        // with `pixel.md` inside the same rule set. `pixel install` scrubs
        // these; this check fails if any survived or came back.
        let stale = config::find_deprecated_rule_files(&home);
        if !stale.is_empty() {
            return Err(format!(
                "retired-tool rule file(s) still present — run `pixel install`: {}",
                stale
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Ok(DoctorCheckDetail {
            summary: "no retired-tool rule files in any agent rules directory".into(),
            detail: Some(serde_json::json!({
                "dirs_scanned": config::AGENT_RULES_DIRS,
                "names_checked": config::DEPRECATED_RULE_FILES,
            })),
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

    checks.push(check("install.prompt-submit-hook", || -> std::result::Result<DoctorCheckDetail, String> {
        let hooks_dir = home.join(config::CLAUDE_HOOKS_DIR);
        let path = hooks_dir.join(config::PROMPT_SUBMIT_HOOK);
        if !path.is_file() {
            return Err("UserPromptSubmit (task boundary) hook not installed".into());
        }
        Ok(DoctorCheckDetail {
            summary: "UserPromptSubmit (task boundary) hook installed".into(),
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
        // Config-file hooks are disabled by default — check enabled: true.
        let hooks_enabled = value
            .get("hooks")
            .and_then(|v| v.get("enabled"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if !hooks_enabled {
            return Err("zcode hooks.enabled is false (or missing) — config-file hooks won't fire".into());
        }
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
        // Check ~/.zcode/AGENTS.md has pixel rules.
        let agents_md = home.join(".zcode").join("AGENTS.md");
        if !agents_md.is_file() {
            return Err("zcode AGENTS.md not deployed (no ~/.zcode/AGENTS.md)".into());
        }
        let agents_raw = fs::read_to_string(&agents_md).map_err(|e| e.to_string())?;
        if !agents_raw.contains(config::MANAGED_BEGIN) {
            return Err("zcode AGENTS.md missing pixel managed markers".into());
        }
        Ok(DoctorCheckDetail {
            summary: "zcode hooks + AGENTS.md rules wired (PreToolUse, hooks.enabled)".into(),
            detail: Some(serde_json::json!({ "path": config_path.display().to_string(), "agents_md": agents_md.display().to_string() })),
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
        let ext_file = config_dir.join("extensions").join("pixel-guard.ts");
        if !ext_file.is_file() {
            return Err("pi pixel-guard.ts extension not installed".into());
        }
        let raw = fs::read_to_string(&ext_file).map_err(|e| e.to_string())?;
        if !raw.contains(config::MANAGED_BEGIN) {
            return Err("pi pixel-guard.ts missing managed markers".into());
        }
        if !raw.contains("tool_call") {
            return Err("pi pixel-guard.ts does not intercept tool_call event".into());
        }
        // Check ~/.pi/agent/AGENTS.md has pixel rules.
        let agents_md = config_dir.join("AGENTS.md");
        if !agents_md.is_file() {
            return Err("pi AGENTS.md not deployed (no ~/.pi/agent/AGENTS.md)".into());
        }
        let agents_raw = fs::read_to_string(&agents_md).map_err(|e| e.to_string())?;
        if !agents_raw.contains(config::MANAGED_BEGIN) {
            return Err("pi AGENTS.md missing pixel managed markers".into());
        }
        if !agents_raw.contains("# pixel — Deterministic") {
            return Err("pi AGENTS.md missing pixel rule text".into());
        }
        Ok(DoctorCheckDetail {
            summary: "pi guard extension + AGENTS.md rules installed (tool_call interception)".into(),
            detail: Some(serde_json::json!({ "extension": ext_file.display().to_string(), "agents_md": agents_md.display().to_string() })),
        })
    }));

    checks.push(check("install.agent-config", || -> std::result::Result<DoctorCheckDetail, String> {
        let configs = config::find_agent_configs(&home);
        if configs.is_empty() {
            return Err("no CLAUDE.md/AGENTS.md found to manage".into());
        }
        // A config file is "managed" if it EITHER:
        //   (a) contains a pixel managed block (legacy: pixel install wrote it), OR
        //   (b) contains the pixel rule text (current: build-agent-config's
        //       aggregate includes pixel.md, so the rule is present without a
        //       managed block).
        // The duplicate-prevention logic in rewrite_agent_configs now skips
        // writing a managed block when the rule is already present via the
        // aggregate, so (b) is the expected state for agent configs managed
        // by build-agent-config.
        let unmanaged: Vec<&PathBuf> = configs
            .iter()
            .filter(|p| {
                fs::read_to_string(p)
                    .map(|s| {
                        !s.contains(config::MANAGED_BEGIN)
                            && !s.contains("# pixel — Deterministic")
                    })
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

    // Rule-vs-binary parity: every `pixel …` command line documented in the
    // INSTALLED rule text must dry-run parse against the binary's real clap
    // definition. Drift between documented CLI syntax and the binary was the
    // largest defect category found — this makes it a red doctor check
    // instead of a silent lie agents follow into parse errors.
    if let Some(validator) = options.syntax_validator {
        let home_for_rule = home.clone();
        checks.push(check_status("rule.parity", move || {
            let Some((source, rule_text)) = installed_rule_text(&home_for_rule) else {
                return Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: "no installed rule text found (managed block or rule file) — parity not checked".into(),
                    detail: None,
                }));
            };
            let commands = extract_rule_commands(&rule_text);
            if commands.is_empty() {
                return Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: format!(
                        "installed rule text at {} contains no `pixel …` command lines — parity not checked",
                        source.display()
                    ),
                    detail: None,
                }));
            }
            let mut parsed_ok = 0usize;
            let mut unparsed: Vec<String> = Vec::new();
            let mut failures: Vec<String> = Vec::new();
            for line in &commands {
                match normalize_rule_command(line) {
                    None => unparsed.push(line.clone()),
                    Some(argv) => match validator(&argv) {
                        Ok(()) => parsed_ok += 1,
                        Err(e) => failures.push(format!("`{line}` → {e}")),
                    },
                }
            }
            let detail = Some(serde_json::json!({
                "source": source.display().to_string(),
                "command_lines": commands.len(),
                "parsed_ok": parsed_ok,
                "unparsed": unparsed,
                "failures": failures,
            }));
            if !failures.is_empty() {
                return Err(format!(
                    "{} documented command line(s) rejected by the CLI parser: {}",
                    failures.len(),
                    failures.join("; ")
                ));
            }
            Ok((CheckStatus::Green, DoctorCheckDetail {
                summary: format!(
                    "{parsed_ok}/{} documented pixel command lines parse against the CLI ({} unparsed placeholder line(s) skipped)",
                    commands.len(),
                    unparsed.len()
                ),
                detail,
            }))
        }));
    }

    // Scenario-count consistency: the installed rule text and the
    // SessionStart usage string must agree on the FIVE mandatory scenarios
    // (targets/resolve/rescue/reconcile/impact). A scenario the rule
    // mandates but the injected session never hears about — or vice versa —
    // is exactly the drift class this doctor exists to catch.
    {
        let home_for_rule = home.clone();
        checks.push(check_status("rule.scenarios", move || {
            let Some((source, rule_text)) = installed_rule_text(&home_for_rule) else {
                return Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: "no installed rule text found — scenario consistency not checked".into(),
                    detail: None,
                }));
            };
            let mismatches =
                scenario_mismatches(&rule_text, pixel_proto::op::SESSION_USAGE);
            if !mismatches.is_empty() {
                return Err(format!(
                    "scenario drift between installed rule text ({}) and session usage string: {}",
                    source.display(),
                    mismatches.join("; ")
                ));
            }
            Ok((CheckStatus::Green, DoctorCheckDetail {
                summary: format!(
                    "rule text and session usage agree on all {} mandatory scenarios",
                    MANDATORY_SCENARIOS.len()
                ),
                detail: Some(serde_json::json!({
                    "scenarios": MANDATORY_SCENARIOS,
                    "source": source.display().to_string(),
                })),
            }))
        }));
    }

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

        // Epistemics-presence probe: when a daemon answers, one retrieval op
        // should carry an `epistemics` object in its response. Warning-only
        // (Yellow), never red — the envelope is landing concurrently and a
        // daemon built from an older binary is a staleness note, not a
        // broken install.
        checks.push(check_status("daemon.epistemics", || {
            let sock = pixel_daemon::daemon::socket_path(root);
            if !sock.exists() {
                return Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: "no daemon running — epistemics probe skipped".into(),
                    detail: None,
                }));
            }
            match probe_daemon_epistemics(&sock) {
                Ok(true) => Ok((CheckStatus::Green, DoctorCheckDetail {
                    summary: "daemon retrieval response carries an epistemics object".into(),
                    detail: None,
                })),
                Ok(false) => Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: "daemon retrieval response has NO epistemics object — daemon may predate the epistemics envelope; restart it".into(),
                    detail: None,
                })),
                Err(e) => Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: format!("epistemics probe inconclusive: {e}"),
                    detail: None,
                })),
            }
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

/// Locate the installed pixel rule text: the managed block inside the first
/// CLAUDE.md/AGENTS.md that carries one, else the canonical rule source at
/// `~/.agent-config/rules/pixel.md`. Returns the source path and the text.
fn installed_rule_text(home: &Path) -> Option<(PathBuf, String)> {
    for path in config::find_agent_configs(home) {
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(start) = content.find(config::MANAGED_BEGIN) {
            let body = &content[start + config::MANAGED_BEGIN.len()..];
            let block = match body.find(config::MANAGED_END) {
                Some(end) => &body[..end],
                None => body,
            };
            return Some((path, block.to_string()));
        }
    }
    let rules = home.join(config::PIXEL_RULES_REL);
    fs::read_to_string(&rules).ok().map(|text| (rules, text))
}

/// Extract every `pixel …` command line from the fenced code blocks of a
/// rule document. Trailing `# comments` are stripped; prose and non-pixel
/// lines are ignored.
pub fn extract_rule_commands(rule_text: &str) -> Vec<String> {
    let mut in_fence = false;
    let mut out = Vec::new();
    for line in rule_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            continue;
        }
        // Strip a trailing shell comment (` # …`) — rule examples annotate
        // commands this way.
        let code = match trimmed.find(" #") {
            Some(i) => trimmed[..i].trim_end(),
            None => trimmed,
        };
        if code.starts_with("pixel ") {
            out.push(code.to_string());
        }
    }
    out
}

/// Normalize one documented `pixel …` line into a parseable argv:
/// - `[…]` optional groups are UNWRAPPED (their flags get tested too);
/// - `a|b|c` alternations pick the first alternative;
/// - `<placeholder>` tokens (quoted or bare) become a dummy value;
/// - bare `N` becomes `3` (numeric flag placeholders);
/// - `/path/to/repo` becomes `.`;
/// - a trailing `...` variadic marker is dropped.
///
/// Returns `None` when the line contains syntax this normalizer cannot
/// handle — the caller reports such lines as "unparsed" instead of silently
/// passing them.
pub fn normalize_rule_command(line: &str) -> Option<Vec<String>> {
    // Unwrap bracketed optional groups: brackets may span several
    // whitespace-separated tokens, so strip the characters up front.
    let unbracketed: String = line.chars().filter(|c| *c != '[' && *c != ']').collect();

    // Tokenize, honoring double quotes.
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in unbracketed.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if in_quotes {
        return None; // unbalanced quotes — can't normalize
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    let mut argv = Vec::with_capacity(tokens.len());
    for token in tokens {
        // Drop a trailing variadic marker (`<f>...` → `<f>`).
        let token = token.strip_suffix("...").unwrap_or(&token).to_string();
        // Placeholder → dummy value. A quoted multi-word placeholder is one
        // token by now (`<what broke, in the user's words>`).
        let token = if token.starts_with('<') && token.ends_with('>') {
            "x".to_string()
        } else {
            token
        };
        // Alternation outside placeholders: pick the first alternative
        // (`report|rebase-if-clean` → `report`, `--merge|--stash-first` →
        // `--merge`).
        let token = match token.split('|').next() {
            Some(first) if first.len() < token.len() => first.to_string(),
            _ => token,
        };
        // Well-known placeholder spellings.
        let token = match token.as_str() {
            "/path/to/repo" => ".".to_string(),
            "N" => "3".to_string(),
            _ => token,
        };
        // Anything still carrying placeholder syntax is beyond this
        // normalizer.
        if token.contains('<') || token.contains('>') || token.contains('…') {
            return None;
        }
        argv.push(token);
    }
    if argv.first().map(String::as_str) != Some("pixel") {
        return None;
    }
    Some(argv)
}

/// Compare the installed rule text and the session usage string on the
/// mandatory scenarios. Returns one message per drift found (empty = agree).
pub fn scenario_mismatches(rule_text: &str, session_usage: &str) -> Vec<String> {
    let mut out = Vec::new();
    for scenario in MANDATORY_SCENARIOS {
        let anchored = format!("pixel {scenario}");
        let in_rule = rule_text.contains(&anchored);
        let in_usage = session_usage.contains(scenario);
        match (in_rule, in_usage) {
            (true, false) => out.push(format!(
                "'{scenario}' is mandated by the rule text but missing from the session usage string"
            )),
            (false, true) => out.push(format!(
                "'{scenario}' is in the session usage string but the rule text never mentions `pixel {scenario}`"
            )),
            (false, false) => out.push(format!(
                "'{scenario}' is missing from BOTH the rule text and the session usage string"
            )),
            (true, true) => {}
        }
    }
    out
}

/// One NDJSON retrieval round trip against a running daemon socket, checking
/// whether the response carries an `epistemics` object (envelope- or
/// data-level). Short timeouts — this is a health probe, not a query.
fn probe_daemon_epistemics(sock: &Path) -> std::result::Result<bool, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let mut stream = UnixStream::connect(sock).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    let req = serde_json::json!({
        "op": "search",
        "pattern": "fn ",
        "json": true,
        "limit": 1,
    });
    let mut line = req.to_string();
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    stream.flush().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader
        .read_line(&mut buf)
        .map_err(|e| format!("read: {e}"))?;
    let resp: serde_json::Value =
        serde_json::from_str(&buf).map_err(|e| format!("parse: {e}"))?;
    let has = resp.get("epistemics").is_some()
        || resp
            .get("data")
            .map(|d| d.get("epistemics").is_some())
            .unwrap_or(false);
    Ok(has)
}

/// Re-export the daemon socket-path helper for the CLI.
pub use pixel_daemon::daemon::socket_path as daemon_socket_path;

#[cfg(test)]
mod tests {
    use super::facts_dead_reason;
    use super::{extract_rule_commands, normalize_rule_command, scenario_mismatches};

    // -- rule-vs-binary parity: extraction + normalization ------------------

    const SAMPLE_RULE: &str = r#"
## Scenario 1

```bash
# Deleted or currently-nonexistent code: search all history, stash, and reflog
pixel excavate --phrase "<what you're looking for>" [--path <path>] [--json]

pixel rescue "<what broke, in the user's words>" /path/to/repo [--json]

pixel rescue --apply <oid> --file <path> /path/to/repo [--merge|--stash-first|--allow-dirty]
```

Prose mentioning `pixel doctor` inline must NOT be extracted.

```bash
pixel targets --clear /path/to/repo   # when the task ends
pixel reconcile /path/to/repo [--strategy report|rebase-if-clean] [--push auto|never]
git clone https://example.com/repo.git
```
"#;

    #[test]
    fn extracts_only_fenced_pixel_lines_and_strips_comments() {
        let commands = extract_rule_commands(SAMPLE_RULE);
        assert_eq!(
            commands,
            vec![
                "pixel excavate --phrase \"<what you're looking for>\" [--path <path>] [--json]",
                "pixel rescue \"<what broke, in the user's words>\" /path/to/repo [--json]",
                "pixel rescue --apply <oid> --file <path> /path/to/repo [--merge|--stash-first|--allow-dirty]",
                "pixel targets --clear /path/to/repo",
                "pixel reconcile /path/to/repo [--strategy report|rebase-if-clean] [--push auto|never]",
            ],
            "must extract exactly the fenced pixel lines, comment-stripped, no inline prose"
        );
    }

    #[test]
    fn normalizes_placeholders_brackets_and_alternations() {
        assert_eq!(
            normalize_rule_command(
                "pixel resolve \"<phrase>\" /path/to/repo [--json] [--limit N]"
            ),
            Some(vec![
                "pixel".into(),
                "resolve".into(),
                "x".into(),
                ".".into(),
                "--json".into(),
                "--limit".into(),
                "3".into(),
            ])
        );
        assert_eq!(
            normalize_rule_command(
                "pixel reconcile /path/to/repo [--strategy report|rebase-if-clean] [--push auto|never]"
            ),
            Some(vec![
                "pixel".into(),
                "reconcile".into(),
                ".".into(),
                "--strategy".into(),
                "report".into(),
                "--push".into(),
                "auto".into(),
            ])
        );
        assert_eq!(
            normalize_rule_command(
                "pixel publish --files <f>... --message \"<msg>\" --request-id <id> /path/to/repo"
            ),
            Some(vec![
                "pixel".into(),
                "publish".into(),
                "--files".into(),
                "x".into(),
                "--message".into(),
                "x".into(),
                "--request-id".into(),
                "x".into(),
                ".".into(),
            ])
        );
        // Bracketed flag alternation picks the first flag.
        assert_eq!(
            normalize_rule_command(
                "pixel rescue --apply <oid> --file <path> /path/to/repo [--merge|--stash-first|--allow-dirty]"
            )
            .as_deref()
            .and_then(|v| v.last().cloned()),
            Some("--merge".to_string())
        );
    }

    #[test]
    fn unnormalizable_lines_are_reported_not_silently_passed() {
        // Unbalanced quotes.
        assert_eq!(normalize_rule_command("pixel search \"unclosed"), None);
        // Ellipsis placeholder syntax the normalizer doesn't understand.
        assert_eq!(normalize_rule_command("pixel search a…b"), None);
        // Not a pixel line at all.
        assert_eq!(normalize_rule_command("git status"), None);
    }

    // -- scenario consistency ------------------------------------------------

    #[test]
    fn scenario_agreement_is_empty_when_both_sides_name_all_five() {
        let rule = "use pixel targets first, pixel resolve for phrases, \
                    pixel rescue for history, pixel reconcile for sync, \
                    pixel impact before edits";
        assert!(
            scenario_mismatches(rule, pixel_proto::op::SESSION_USAGE).is_empty(),
            "all five scenarios present on both sides must produce zero mismatches"
        );
    }

    #[test]
    fn scenario_drift_is_flagged_per_missing_side() {
        let rule_without_impact =
            "pixel targets, pixel resolve, pixel rescue, pixel reconcile";
        let usage_without_impact =
            "targets resolve rescue reconcile — four scenarios only";
        // Rule lacks impact → usage-only drift message.
        let drift = scenario_mismatches(rule_without_impact, pixel_proto::op::SESSION_USAGE);
        assert_eq!(drift.len(), 1, "exactly the impact scenario drifts: {drift:?}");
        assert!(drift[0].contains("impact"));
        // Usage lacks impact while the rule mandates it → red-worthy drift.
        let rule_full = "pixel targets pixel resolve pixel rescue pixel reconcile pixel impact";
        let drift = scenario_mismatches(rule_full, usage_without_impact);
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert!(drift[0].contains("missing from the session usage string"));
    }

    #[test]
    fn live_session_usage_and_live_rule_source_agree_when_rule_readable() {
        // The real parity gate runs inside `pixel doctor` against the
        // installed text; here we only pin that the SESSION_USAGE constant
        // itself names every mandatory scenario.
        for scenario in super::MANDATORY_SCENARIOS {
            assert!(
                pixel_proto::op::SESSION_USAGE.contains(scenario),
                "SESSION_USAGE must name '{scenario}'"
            );
        }
    }

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
