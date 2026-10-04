//! `pixel uninstall` — the inverse of `pixel install`.
//!
//! Removes every trace pixel install wrote:
//!   - managed blocks from CLAUDE.md / AGENTS.md / .zcode/AGENTS.md /
//!     .pi/agent/AGENTS.md
//!   - pixel run-hook entries from Claude, Devin, Codex, Gemini, zcode, Cursor,
//!     and project-level .codex/hooks.json settings files
//!   - pixel run-hook scripts from ~/.claude/hooks/
//!   - the pi guard extension (~/.pi/agent/extensions/pixel-guard.ts)
//!   - the pixel block from Pi's ~/.pi/agent/APPEND_SYSTEM.md (the rest of
//!     that shared file is the user's and is kept)
//!   - the pixel rule source file (~/.agent-config/rules/pixel.md)
//!   - the pixel binary (~/.local/bin/pixel by default)
//!
//! Idempotent: safe to re-run. Each step reports what was removed (or that
//! nothing was found). Backups are written before every destructive write,
//! same as install.

use std::fs;
use std::path::{Path, PathBuf};

use crate::InstallError;
use crate::config;
use crate::install::{self, CheckStatus, InstallReport, InstallStep, InstallSummary};
use crate::routing;

pub type Result<T> = std::result::Result<T, InstallError>;

/// Options controlling an uninstall run.
#[derive(Debug, Clone, Default)]
pub struct UninstallOptions {
    /// Home directory. Defaults to `$HOME`.
    pub home: Option<PathBuf>,
    /// Path to the pixel binary to remove, as the user named it: removed even
    /// when a package manager owns it. When `None`, `running_binary` is the
    /// target, then `~/.local/bin/pixel`.
    pub binary_path: Option<PathBuf>,
    /// The binary running this uninstall (the CLI passes its current exe).
    /// It is the removal target when `binary_path` is `None`, so a binary
    /// `install.sh` put in `PIXEL_INSTALL_DIR` goes, not only one at
    /// `~/.local/bin`; one that Homebrew or mise installed is left to them.
    pub running_binary: Option<PathBuf>,
    /// The pixel binary whose hook entries count as pixel's, beside those of
    /// a binary named `pixel` (see [`routing::pixel_hook_verb`]). Defaults to
    /// the current exe, so a build installed as `pixel-dev` removes the
    /// entries it wrote.
    pub executable_path: Option<PathBuf>,
    /// Shell whose wrapper block should be removed, as a `$SHELL`-style value.
    /// Defaults to `$SHELL`.
    pub shell: Option<String>,
    /// If true, compute and report every step's outcome exactly as a real
    /// run would, but perform no filesystem writes.
    pub dry_run: bool,
    /// Remove only the shell wrapper block of `shell` and leave every other
    /// artifact in place: the way out of a block written for a shell that
    /// never loads it (`pixel doctor` names the file) without losing the
    /// install that works.
    pub wrappers_only: bool,
    /// Repository root whose project-local artifacts `pixel install --repo`
    /// wrote (`pixel uninstall --repo <path>`). When set, ONLY repo-local
    /// removal runs: `.codex/hooks.json` + composed backup,
    /// `.codex/config.toml`, `.claude/settings.local.json`,
    /// `.devin/config.local.json`, `.pi/extensions/pixel-guard.ts` (plus
    /// pixel's files in the `.pi/agent/` older releases used),
    /// Pixel's managed root `AGENTS.md` block, and the retired Pixel entry
    /// in `.warp/.mcp.json`.
    pub repo: Option<PathBuf>,
}

/// Markers that identify pixel-authored hook entries in any settings file.
/// Each corresponds to a hook script filename installed by `pixel install`.
const PIXEL_HOOK_MARKERS: &[&str] = &[
    config::GUARD_HOOK,
    config::SESSION_START_HOOK,
    config::PROMPT_SUBMIT_HOOK,
    config::POST_COMPACTION_HOOK,
    crate::codex_config::METRICS_HOOK_MARKER,
    crate::codex_config::PROMPT_SUBMIT_HOOK_MARKER,
    "run-hook guard --provider zcode",
    "run-hook metrics --provider cursor",
    "run-hook guard",
];

/// Run `pixel uninstall`. Idempotent: safe to re-run.
pub fn uninstall(options: &UninstallOptions) -> Result<InstallReport> {
    let home = options
        .home
        .clone()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .ok_or(InstallError::NoHome)?;
    let target = removal_target(options, &home);
    let binary_path = target.path.clone();
    let executable_path = options
        .executable_path
        .clone()
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| binary_path.clone());
    let exe = executable_path.canonicalize().unwrap_or(executable_path);

    let dry_run = options.dry_run;
    if let Some(repo) = &options.repo {
        return uninstall_project(repo, &binary_path, &exe, dry_run);
    }
    if options.wrappers_only {
        let step = install::remove_shell_wrappers(&home, options.shell.as_deref(), dry_run)?;
        let summary = InstallSummary {
            green: usize::from(step.status == CheckStatus::Green),
            yellow: usize::from(step.status == CheckStatus::Yellow),
            red: usize::from(step.status == CheckStatus::Red),
        };
        let ok = summary.red == 0;
        return Ok(InstallReport {
            version: "v1".into(),
            ok,
            executable_path: binary_path.display().to_string(),
            home: home.display().to_string(),
            dry_run,
            steps: vec![step],
            summary,
        });
    }
    let steps = vec![
        // 1. Remove shell wrappers from the shell's profile (~/.zshrc,
        //    ~/.bashrc, or fish's ~/.config/fish/conf.d/pixel.fish).
        install::remove_shell_wrappers(&home, options.shell.as_deref(), dry_run)?,
        // 2. Strip managed blocks from all agent-config Markdown files.
        strip_agent_configs(&home, dry_run)?,
        // 3. Remove pixel run-hook entries from Claude settings.json + delete hook
        //    scripts from ~/.claude/hooks/.
        remove_claude_hooks(&home, &exe, dry_run)?,
        // 4. Remove pixel run-hook entries from every other tool's settings file.
        remove_devin_hooks(&home, &exe, dry_run)?,
        remove_codex_hooks(&home, &exe, dry_run)?,
        remove_gemini_hooks(&home, &exe, dry_run)?,
        remove_zcode_hooks(&home, dry_run)?,
        remove_cursor_hooks(&home, &exe, dry_run)?,
        crate::copilot_config::remove_copilot_hooks(&home, dry_run)?,
        remove_pi_extension(&home, dry_run)?,
        // 5. Remove pixel hooks from project-level .codex/hooks.json files.
        remove_project_codex_hooks(&home, &exe, dry_run)?,
        // 6. Remove the pixel rule source file.
        remove_rule_source(&home, dry_run)?,
        // 7. Remove the pixel agent system prompt.
        remove_agent_prompt(&home, dry_run)?,
        // 7b. Take the pixel block out of Codex's developer_instructions.
        crate::codex_config::remove_developer_instructions(
            &crate::codex_config::codex_home(&home, options.home.is_some()),
            dry_run,
        )?,
        // 7c. Remove Antigravity plugin and hooks.
        crate::antigravity::remove_antigravity(&home, dry_run)?,
        // 7d. Take the pixel block out of OpenCode's global AGENTS.md.
        crate::opencode_config::remove_opencode(
            &crate::opencode_config::opencode_config_dir(&home, options.home.is_some()),
            &home,
            dry_run,
        )?,
        // 8. Remove the pixel binary.
        remove_binary(&target, dry_run)?,
        // 9. Name the backups install and uninstall kept, and how to drop them.
        backups_step(
            &find_backups(&global_backup_dirs(
                &home,
                &crate::codex_config::codex_home(&home, options.home.is_some()),
                &crate::opencode_config::opencode_config_dir(&home, options.home.is_some()),
            )),
            dry_run,
        ),
    ];

    let green = steps
        .iter()
        .filter(|s| s.status == CheckStatus::Green)
        .count();
    let yellow = steps
        .iter()
        .filter(|s| s.status == CheckStatus::Yellow)
        .count();
    let red = steps
        .iter()
        .filter(|s| s.status == CheckStatus::Red)
        .count();
    let ok = red == 0;

    Ok(InstallReport {
        version: "v1".into(),
        ok,
        executable_path: binary_path.display().to_string(),
        home: home.display().to_string(),
        dry_run,
        steps,
        summary: InstallSummary { green, yellow, red },
    })
}

/// Repo-scoped uninstall (`pixel uninstall --repo <path>`): removes exactly
/// the artifacts `pixel install --repo` writes, nothing global:
///   - `<repo>/.codex/hooks.json` — restores the snapshotted PreToolUse groups
///     from the composed-guard sidecar (or strips pixel entries when no
///     sidecar exists), preserving a diverged file for manual reconciliation;
///   - `<repo>/.codex/config.toml` — the `developer_instructions` block;
///   - `<repo>/.claude/settings.local.json` — the pixel guard, with an RTK
///     group it adopted put back from `<repo>/.claude/pixel-rtk-hooks.json`
///     (then deleted), and any guard an earlier install left in the shared
///     `settings.json`;
///   - `<repo>/.devin/config.local.json` (and the legacy `.devin/hooks.json`)
///     — the pixel guard group only;
///   - `<repo>/.pi/extensions/pixel-guard.ts`, and pixel's files in the
///     `<repo>/.pi/agent/` an older release used ([`crate::pi_project`]);
///   - the Pixel-first managed block in `<repo>/AGENTS.md`, preserving all
///     instructions outside its markers ([`crate::pixel_first`]);
///   - the retired Pixel entry in `<repo>/.warp/.mcp.json` ([`crate::warp`]).
fn uninstall_project(
    repo: &Path,
    binary_path: &Path,
    exe: &Path,
    dry_run: bool,
) -> Result<InstallReport> {
    let codex_hooks = repo.join(".codex").join(crate::codex_config::HOOKS_FILE);
    let mut patched = Vec::new();
    let mut conflicts = Vec::new();
    if codex_hooks.is_file() {
        match restore_project_codex_composed_guard(&codex_hooks, dry_run)? {
            ComposedGuardRestore::Restored => {
                patched.push(codex_hooks.display().to_string());
                // The composed install also registers lifecycle entries
                // (PostToolUse, SessionStart, UserPromptSubmit, compaction);
                // strip them now that PreToolUse holds the restored groups.
                let (removed, _) = remove_pixel_hooks_from_settings(&codex_hooks, exe, dry_run)?;
                if removed > 0 {
                    patched.push(codex_hooks.display().to_string());
                }
            }
            // A snapshot exists but its schema or the managed guard was
            // edited; preserve both files rather than lose recovery data.
            ComposedGuardRestore::Conflict => conflicts.push(codex_hooks.display().to_string()),
            ComposedGuardRestore::NotManaged => {
                let (removed, _) = remove_pixel_hooks_from_settings(&codex_hooks, exe, dry_run)?;
                if removed > 0 {
                    patched.push(codex_hooks.display().to_string());
                }
            }
        }
    }
    let codex_summary = if patched.is_empty() && conflicts.is_empty() {
        "no project .codex/hooks.json needed patching".to_string()
    } else {
        format!(
            "patched {} project .codex/hooks.json file(s); {} composed guard conflict(s) preserved",
            patched.len(),
            conflicts.len()
        )
    };
    let steps = vec![
        InstallStep {
            id: "hooks.codex_project".into(),
            status: if conflicts.is_empty() {
                CheckStatus::Green
            } else {
                CheckStatus::Yellow
            },
            summary: install::dry_run_summary(dry_run, &codex_summary),
            detail: Some(format!("config={}", codex_hooks.display())),
        },
        remove_project_claude_guard(repo, exe, dry_run)?,
        crate::codex_config::remove_developer_instructions(&repo.join(".codex"), dry_run)?,
        {
            let devin_hooks = repo.join(routing::DEVIN_LOCAL_CONFIG);
            let (legacy_removed, _) = remove_pixel_hooks_from_settings(
                &repo.join(routing::DEVIN_LEGACY_HOOKS),
                exe,
                dry_run,
            )?;
            let (removed, backup_path) =
                remove_pixel_hooks_from_settings(&devin_hooks, exe, dry_run)?;
            let removed = removed + legacy_removed;
            InstallStep {
                id: "hooks.devin".into(),
                status: CheckStatus::Green,
                summary: install::dry_run_summary(
                    dry_run,
                    &format!("removed {removed} Devin hook entry/entries"),
                ),
                detail: Some(install::with_backup_note(
                    format!("config={}", devin_hooks.display()),
                    backup_path,
                )),
            }
        },
        crate::pi_project::uninstall(repo, dry_run)?,
        crate::warp::retire(repo, dry_run)?,
        crate::pixel_first::uninstall_rules(repo, dry_run)?,
        backups_step(&find_backups(&project_backup_dirs(repo)), dry_run),
    ];

    let green = steps
        .iter()
        .filter(|s| s.status == CheckStatus::Green)
        .count();
    let yellow = steps
        .iter()
        .filter(|s| s.status == CheckStatus::Yellow)
        .count();
    let red = steps
        .iter()
        .filter(|s| s.status == CheckStatus::Red)
        .count();

    Ok(InstallReport {
        version: "v1".into(),
        ok: red == 0,
        executable_path: binary_path.display().to_string(),
        home: repo.display().to_string(),
        dry_run,
        steps,
        summary: InstallSummary { green, yellow, red },
    })
}

/// Take the repo-local Claude guard out of `<repo>/.claude/settings.local.json`
/// and put back the RTK group it adopted, read from the repository's own
/// backup, which is deleted afterwards. A guard an earlier install wrote into
/// the shared `settings.json` is removed as well.
fn remove_project_claude_guard(repo: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let local = repo.join(routing::CLAUDE_LOCAL_SETTINGS);
    let backup_file = repo.join(routing::RTK_BACKUP);
    let mut removed = 0usize;
    let mut backup_path = None;
    if local.is_file() {
        let mut value = install::read_settings(&local)?;
        if let Some(hooks) = value
            .get_mut("hooks")
            .and_then(serde_json::Value::as_object_mut)
        {
            let saved = if routing::has_delegate(hooks, exe) {
                let saved = routing::load_rtk_backup(repo)?;
                if saved.is_empty() {
                    return Err(InstallError::InvalidSettings {
                        path: backup_file,
                        reason: "RTK delegate backup missing; refusing to lose its registration"
                            .into(),
                    });
                }
                saved
            } else {
                Vec::new()
            };
            let before = hooks.clone();
            routing::remove_pixel_hooks(hooks, exe);
            if !saved.is_empty() {
                routing::restore_rtk(hooks, &saved);
            }
            if *hooks != before {
                removed += 1;
                backup_path = install::write_settings(&local, &value, dry_run)?;
            }
        }
    }
    if !dry_run && backup_file.is_file() {
        fs::remove_file(&backup_file)?;
    }
    let shared = repo.join(routing::CLAUDE_SHARED_SETTINGS);
    let (_, shared_changed) = routing::remove_pre_tool_use_guard(&shared, exe, dry_run)?;
    removed += usize::from(shared_changed);
    Ok(InstallStep {
        id: "hooks.claude".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(
            dry_run,
            &format!("removed the pixel guard from {removed} Claude settings file(s)"),
        ),
        detail: Some(install::with_backup_note(
            format!("config={}", local.display()),
            backup_path,
        )),
    })
}

// -------------------------------------------------------------------------
// Step 1: strip managed blocks from agent-config Markdown files
// -------------------------------------------------------------------------

fn strip_agent_configs(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let targets = config::find_agent_configs(home);
    // Also strip managed blocks from zcode and pi AGENTS.md files.
    let mut all_targets = targets;
    let zcode_agents = home.join(".zcode").join("AGENTS.md");
    if zcode_agents.is_file() {
        all_targets.push(zcode_agents);
    }
    let pi_agents = home.join(config::PI_CONFIG_DIR).join("AGENTS.md");
    if pi_agents.is_file() {
        all_targets.push(pi_agents);
    }

    let mut stripped = 0usize;
    let mut skipped = 0usize;
    let mut backups: Vec<String> = Vec::new();

    for path in &all_targets {
        let original = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !original.contains(config::MANAGED_BEGIN) {
            skipped += 1;
            continue;
        }
        let cleaned = config::strip_managed_block(&original);
        if dry_run {
            stripped += 1;
            continue;
        }
        let bk = config::backup_if_changing(path, cleaned.as_bytes())?;
        fs::write(path, &cleaned)?;
        if bk.is_some() {
            backups.push(path.display().to_string());
        }
        stripped += 1;
    }

    let summary =
        format!("stripped managed block from {stripped} file(s) ({skipped} already clean)");
    let detail = if backups.is_empty() {
        None
    } else {
        Some(format!("files=[{}]", backups.join(",")))
    };
    Ok(InstallStep {
        id: "agent-config".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail,
    })
}

// -------------------------------------------------------------------------
// Step 2: remove Claude hooks (settings.json entries + hook scripts)
// -------------------------------------------------------------------------

fn remove_claude_hooks(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let settings = home.join(".claude").join("settings.json");
    let mut removed_entries = 0usize;
    let mut backup_path = None;

    if settings.is_file() {
        let mut value = install::read_settings(&settings)?;
        if let Some(hooks) = value
            .get_mut("hooks")
            .and_then(serde_json::Value::as_object_mut)
        {
            let saved = if routing::has_delegate(hooks, exe) {
                let saved = routing::load_rtk_backup(home)?;
                if saved.is_empty() {
                    return Err(InstallError::InvalidSettings {
                        path: home.join(routing::RTK_BACKUP),
                        reason: "RTK delegate backup missing; refusing to lose its registration"
                            .into(),
                    });
                }
                saved
            } else {
                Vec::new()
            };
            let before = hooks.clone();
            routing::remove_pixel_hooks(hooks, exe);
            // Flat-schema (Cursor-style) entries are pixel's by executable
            // ownership, not by a command substring.
            routing::remove_flat_pixel_hooks(hooks, exe);
            if !saved.is_empty() {
                routing::restore_rtk(hooks, &saved);
            }
            removed_entries += usize::from(*hooks != before);
            // Remove pixel entries from every event. Events that pixel
            // registered under: PreToolUse, SessionStart, UserPromptSubmit,
            // PostCompaction. But iterate ALL event keys — a user may have
            // moved things around, and we want to be thorough.
            let event_keys: Vec<String> = hooks.keys().cloned().collect();
            for event in event_keys {
                if let Some(existing) = hooks.get(&event) {
                    let mut filtered = existing.clone();
                    for marker in PIXEL_HOOK_MARKERS {
                        filtered = config::remove_hook_entries(&filtered, marker);
                    }
                    if filtered.as_array().is_some_and(std::vec::Vec::is_empty) {
                        hooks.remove(&event);
                        removed_entries += 1;
                    } else if filtered != *existing {
                        hooks.insert(event, filtered);
                        removed_entries += 1;
                    }
                }
            }
            // If the hooks object is now empty, remove it entirely.
            if hooks.is_empty()
                && let Some(obj) = value.as_object_mut()
            {
                obj.remove("hooks");
            }
        }
        if !dry_run && removed_entries > 0 {
            backup_path = install::write_settings(&settings, &value, dry_run)?;
        }
        if !dry_run && home.join(routing::RTK_BACKUP).is_file() {
            fs::remove_file(home.join(routing::RTK_BACKUP))?;
        }
    }

    // Delete hook scripts from ~/.claude/hooks/.
    let hooks_dir = home.join(config::CLAUDE_HOOKS_DIR);
    let hook_files = [
        config::GUARD_HOOK,
        config::SESSION_START_HOOK,
        config::PROMPT_SUBMIT_HOOK,
        config::POST_COMPACTION_HOOK,
        config::OLD_GUARD_HOOK,
    ];
    let mut scripts_removed = 0usize;
    for name in &hook_files {
        let path = hooks_dir.join(name);
        if !path.is_file() {
            continue;
        }
        if dry_run {
            scripts_removed += 1;
            continue;
        }
        // Back up before deleting.
        let current = fs::read(&path).unwrap_or_default();
        let _ = config::backup_if_changing(&path, &{
            let mut s = current.clone();
            s.push(0);
            s
        });
        let _ = fs::remove_file(&path);
        scripts_removed += 1;
    }

    let summary = format!(
        "removed {removed_entries} Claude hook event(s), deleted {scripts_removed} hook script(s)"
    );
    Ok(InstallStep {
        id: "hooks.claude".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(install::with_backup_note(
            format!("settings={}", settings.display()),
            backup_path,
        )),
    })
}

// -------------------------------------------------------------------------
// Step 3a: remove Devin hooks
// -------------------------------------------------------------------------

fn remove_devin_hooks(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_path = home
        .join(config::DEVIN_CONFIG_DIR)
        .join(config::DEVIN_CONFIG_FILE);
    let (removed, backup_path) = remove_pixel_hooks_from_settings(&config_path, exe, dry_run)?;
    let summary = format!("removed {removed} Devin hook entry/entries");
    Ok(InstallStep {
        id: "hooks.devin".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(install::with_backup_note(
            format!("config={}", config_path.display()),
            backup_path,
        )),
    })
}

// -------------------------------------------------------------------------
// Step 3b: remove Codex hooks
// -------------------------------------------------------------------------

fn remove_codex_hooks(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_path = home.join(config::CODEX_HOOKS_FILE);
    let (removed, backup_path) = remove_pixel_hooks_from_settings(&config_path, exe, dry_run)?;
    let summary = format!("removed {removed} Codex hook entry/entries");
    Ok(InstallStep {
        id: "hooks.codex".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(install::with_backup_note(
            format!("config={}", config_path.display()),
            backup_path,
        )),
    })
}

// -------------------------------------------------------------------------
// Step 3c: remove Gemini hooks
// -------------------------------------------------------------------------

fn remove_gemini_hooks(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_path = home.join(config::GEMINI_SETTINGS_FILE);
    let (removed, backup_path) = remove_pixel_hooks_from_settings(&config_path, exe, dry_run)?;
    let summary = format!("removed {removed} Gemini hook entry/entries");
    Ok(InstallStep {
        id: "hooks.gemini".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(install::with_backup_note(
            format!("config={}", config_path.display()),
            backup_path,
        )),
    })
}

// -------------------------------------------------------------------------
// Step 3d: remove zcode hooks + AGENTS.md managed block
// -------------------------------------------------------------------------

fn remove_zcode_hooks(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_path = home.join(config::ZCODE_CONFIG_FILE);
    if !config_path.is_file() {
        return Ok(InstallStep {
            id: "hooks.zcode".into(),
            status: CheckStatus::Green,
            summary: install::dry_run_summary(dry_run, "no zcode config — skipping"),
            detail: None,
        });
    }
    let mut value = install::read_settings(&config_path)?;
    let mut removed = 0usize;
    // zcode nests hooks under `hooks.events.<Event>`.
    if let Some(hooks_root) = value
        .get_mut("hooks")
        .and_then(serde_json::Value::as_object_mut)
    {
        if let Some(events) = hooks_root
            .get_mut("events")
            .and_then(serde_json::Value::as_object_mut)
        {
            let event_keys: Vec<String> = events.keys().cloned().collect();
            for event in event_keys {
                if let Some(existing) = events.get(&event) {
                    let mut filtered = existing.clone();
                    for marker in PIXEL_HOOK_MARKERS {
                        filtered = config::remove_hook_entries(&filtered, marker);
                    }
                    if filtered.as_array().is_some_and(std::vec::Vec::is_empty) {
                        events.remove(&event);
                        removed += 1;
                    } else if filtered != *events.get(&event).unwrap() {
                        events.insert(event, filtered);
                        removed += 1;
                    }
                }
            }
            if events.is_empty() {
                hooks_root.remove("events");
            }
        }
        if hooks_root.is_empty()
            && let Some(obj) = value.as_object_mut()
        {
            obj.remove("hooks");
        }
    }
    let mut backup_path = None;
    if !dry_run && removed > 0 {
        backup_path = install::write_settings(&config_path, &value, dry_run)?;
    }

    // Also strip the managed block from ~/.zcode/AGENTS.md.
    let agents_md = home.join(".zcode").join("AGENTS.md");
    let mut agents_stripped = false;
    if agents_md.is_file() {
        let original = fs::read_to_string(&agents_md).unwrap_or_default();
        if original.contains(config::MANAGED_BEGIN) {
            let cleaned = config::strip_managed_block(&original);
            if !dry_run {
                let bk = config::backup_if_changing(&agents_md, cleaned.as_bytes())?;
                fs::write(&agents_md, &cleaned)?;
                backup_path = backup_path.or(bk);
            }
            agents_stripped = true;
        }
    }

    let summary = format!(
        "removed {removed} zcode hook entry/entries{}",
        if agents_stripped {
            " + stripped AGENTS.md managed block"
        } else {
            ""
        }
    );
    Ok(InstallStep {
        id: "hooks.zcode".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(install::with_backup_note(
            format!("config={}", config_path.display()),
            backup_path,
        )),
    })
}

// -------------------------------------------------------------------------
// Step 3e: remove Cursor hooks (flat schema)
// -------------------------------------------------------------------------

pub(crate) fn remove_cursor_hooks(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_path = home.join(config::CURSOR_HOOKS_FILE);
    if !config_path.is_file() {
        return Ok(InstallStep {
            id: "hooks.cursor".into(),
            status: CheckStatus::Green,
            summary: install::dry_run_summary(dry_run, "no Cursor hooks.json — skipping"),
            detail: None,
        });
    }
    let mut value = install::read_settings(&config_path)?;
    let mut removed = 0usize;
    if let Some(hooks) = value
        .get_mut("hooks")
        .and_then(serde_json::Value::as_object_mut)
    {
        // Match pixel's own commands by executable ownership, never by a
        // command substring: a foreign hook whose command merely contains a
        // pixel verb (e.g. `run-hook guard`) survives.
        let before = hooks.clone();
        routing::remove_flat_pixel_hooks(hooks, exe);
        removed += usize::from(*hooks != before);
        if hooks.is_empty()
            && let Some(obj) = value.as_object_mut()
        {
            obj.remove("hooks");
        }
    }
    let mut backup_path = None;
    if !dry_run && removed > 0 {
        backup_path = install::write_settings(&config_path, &value, dry_run)?;
    }
    let summary = format!("removed {removed} Cursor hook entry/entries");
    Ok(InstallStep {
        id: "hooks.cursor".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(install::with_backup_note(
            format!("config={}", config_path.display()),
            backup_path,
        )),
    })
}

// -------------------------------------------------------------------------
// Step 3f: remove pi guard extension + AGENTS.md managed block
// -------------------------------------------------------------------------

fn remove_pi_extension(home: &Path, dry_run: bool) -> Result<InstallStep> {
    remove_pi_extension_dir(&home.join(config::PI_CONFIG_DIR), dry_run)
}

/// Remove the pi guard extension and the AGENTS.md managed block from the
/// global pi agent config dir (`~/.pi/agent`).
fn remove_pi_extension_dir(config_dir: &Path, dry_run: bool) -> Result<InstallStep> {
    if !config_dir.is_dir() {
        return Ok(InstallStep {
            id: "hooks.pi".into(),
            status: CheckStatus::Green,
            summary: install::dry_run_summary(dry_run, "no pi config dir — skipping"),
            detail: None,
        });
    }
    let ext_file = config_dir.join("extensions").join("pixel-guard.ts");
    let mut ext_removed = false;
    if ext_file.is_file() {
        if !dry_run {
            let current = fs::read(&ext_file).unwrap_or_default();
            let _ = config::backup_if_changing(&ext_file, &{
                let mut s = current.clone();
                s.push(0);
                s
            });
            let _ = fs::remove_file(&ext_file);
        }
        ext_removed = true;
    }

    // Strip managed block from <config_dir>/AGENTS.md.
    let agents_md = config_dir.join("AGENTS.md");
    let mut agents_stripped = false;
    if agents_md.is_file() {
        let original = fs::read_to_string(&agents_md).unwrap_or_default();
        if original.contains(config::MANAGED_BEGIN) {
            let cleaned = config::strip_managed_block(&original);
            if !dry_run {
                let _ = config::backup_if_changing(&agents_md, cleaned.as_bytes())?;
                fs::write(&agents_md, &cleaned)?;
            }
            agents_stripped = true;
        }
    }

    let summary = format!(
        "{}{}",
        if ext_removed {
            "removed pi guard extension"
        } else {
            "no pi extension found"
        },
        if agents_stripped {
            " + stripped AGENTS.md managed block"
        } else {
            ""
        },
    );
    Ok(InstallStep {
        id: "hooks.pi".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(format!("ext={}", ext_file.display())),
    })
}

// -------------------------------------------------------------------------
// Step 4: remove pixel hooks from project-level .codex/hooks.json files
// -------------------------------------------------------------------------

fn remove_project_codex_hooks(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let mut patched = Vec::new();
    let mut conflicts = Vec::new();
    for root in project_hook_search_roots(home) {
        let config_path = root.join(".codex").join("hooks.json");
        if !config_path.is_file() {
            continue;
        }
        match restore_project_codex_composed_guard(&config_path, dry_run)? {
            ComposedGuardRestore::Restored => {
                patched.push(config_path.display().to_string());
                continue;
            }
            // A snapshot exists but its schema or the managed guard has been
            // edited.  Do not fall through to generic Pixel-hook removal: it
            // would delete the only record needed to recover the user's
            // original guard configuration.
            ComposedGuardRestore::Conflict => {
                conflicts.push(config_path.display().to_string());
                continue;
            }
            ComposedGuardRestore::NotManaged => {}
        }
        let (removed, _) = remove_pixel_hooks_from_settings(&config_path, exe, dry_run)?;
        if removed > 0 {
            patched.push(config_path.display().to_string());
        }
    }
    let summary = if patched.is_empty() && conflicts.is_empty() {
        "no project-level .codex/hooks.json needed patching".to_string()
    } else {
        format!(
            "patched {} project-level .codex/hooks.json file(s); {} composed guard conflict(s) preserved",
            patched.len(),
            conflicts.len()
        )
    };
    Ok(InstallStep {
        id: "hooks.codex_project_shadow".into(),
        status: if conflicts.is_empty() {
            CheckStatus::Green
        } else {
            CheckStatus::Yellow
        },
        summary: install::dry_run_summary(dry_run, &summary),
        detail: if patched.is_empty() && conflicts.is_empty() {
            None
        } else {
            let mut detail = Vec::new();
            if !patched.is_empty() {
                detail.push(format!("restored=[{}]", patched.join(", ")));
            }
            if !conflicts.is_empty() {
                // Paths explain the actionable conflict without exposing the
                // snapshot payload, which may contain arbitrary user hooks.
                detail.push(format!("preserved_conflicts=[{}]", conflicts.join(", ")));
            }
            Some(detail.join("; "))
        },
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposedGuardRestore {
    /// No sidecar belongs to this project; generic Pixel-hook cleanup may run.
    NotManaged,
    /// Exact original `PreToolUse` was restored and the sidecar was removed.
    Restored,
    /// A sidecar exists, but the current managed value was changed or the
    /// sidecar is invalid.  Preserve both files for manual reconciliation.
    Conflict,
}

/// Restore a project-local Codex guard adoption without ever overwriting a
/// user change made after install.
///
/// New sidecars carry `managed_pre_tool_use`, so comparison is byte-for-byte
/// at the JSON value layer.  Early compositions did not record that field; a
/// deliberately strict legacy signature permits their recovery only when the
/// current array is exactly one unfiltered composed-guard command pointing at
/// this project's own sidecar.
fn restore_project_codex_composed_guard(
    config_path: &Path,
    dry_run: bool,
) -> Result<ComposedGuardRestore> {
    let Some(codex_dir) = config_path.parent() else {
        return Ok(ComposedGuardRestore::NotManaged);
    };
    let sidecar = codex_dir.join(routing::CODEX_COMPOSED_BACKUP);
    if !sidecar.is_file() {
        return Ok(ComposedGuardRestore::NotManaged);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(&sidecar)?.permissions().mode() & 0o077 != 0 {
            return Ok(ComposedGuardRestore::Conflict);
        }
    }

    let snapshot: serde_json::Value = match fs::read_to_string(&sidecar)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
    {
        Some(value) => value,
        None => return Ok(ComposedGuardRestore::Conflict),
    };
    if snapshot.get("version").and_then(serde_json::Value::as_u64) != Some(1)
        || snapshot.get("provider").and_then(serde_json::Value::as_str) != Some("codex")
    {
        return Ok(ComposedGuardRestore::Conflict);
    }
    let Some(original_pre) = snapshot
        .get("pre_tool_use")
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(ComposedGuardRestore::Conflict);
    };

    let mut settings = install::read_settings(config_path)?;
    let Some(current_pre) = settings
        .get("hooks")
        .and_then(serde_json::Value::as_object)
        .and_then(|hooks| hooks.get("PreToolUse"))
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(ComposedGuardRestore::Conflict);
    };

    let unchanged = snapshot
        .get("managed_pre_tool_use")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|managed| managed == current_pre)
        || (snapshot.get("managed_pre_tool_use").is_none()
            && legacy_composed_guard_signature(current_pre, &sidecar));
    if !unchanged {
        return Ok(ComposedGuardRestore::Conflict);
    }

    if !dry_run {
        let hooks = settings
            .get_mut("hooks")
            .and_then(serde_json::Value::as_object_mut)
            // `current_pre` above proves this cannot fail unless an internal
            // mutation happened between reads, which it cannot in this value.
            .expect("validated hooks object");
        hooks.insert(
            "PreToolUse".into(),
            serde_json::Value::Array(original_pre.clone()),
        );
        install::write_settings(config_path, &settings, false)?;
        // Delete only after the restored settings are durable.  If deletion
        // fails, retain the sidecar rather than risk silently losing recovery
        // data; a subsequent uninstall will report a conflict instead of
        // overwriting user state.
        fs::remove_file(&sidecar)?;
    }
    Ok(ComposedGuardRestore::Restored)
}

fn legacy_composed_guard_signature(current_pre: &[serde_json::Value], sidecar: &Path) -> bool {
    let [group] = current_pre else {
        return false;
    };
    let Some(group) = group.as_object() else {
        return false;
    };
    // An unfiltered group has no matcher and no opaque fields that could be
    // meaningful to Codex.  `hooks` must contain exactly one command hook.
    if group.len() != 1 || !group.contains_key("hooks") {
        return false;
    }
    let Some(hooks) = group.get("hooks").and_then(serde_json::Value::as_array) else {
        return false;
    };
    let [hook] = hooks.as_slice() else {
        return false;
    };
    if hook.get("type").and_then(serde_json::Value::as_str) != Some("command") {
        return false;
    }
    let Some(command) = hook.get("command").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let escaped_sidecar = sidecar.to_string_lossy().replace('\'', "'\\''");
    // Installs since the command rename write `run-hook`; 0.2.x wrote `hook`.
    ["run-hook", "hook"].iter().any(|verb| {
        command.contains(&format!(
            " {verb} composed-guard --provider codex --backup "
        ))
    }) && command.ends_with(&format!("'{escaped_sidecar}'"))
}

/// Directories commonly holding project checkouts — mirrors the same logic
/// in install.rs.
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

// -------------------------------------------------------------------------
// Step 5: remove the pixel rule source file
// -------------------------------------------------------------------------

fn remove_rule_source(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let path = home.join(config::PIXEL_RULES_REL);
    if !path.is_file() {
        return Ok(InstallStep {
            id: "rule.source".into(),
            status: CheckStatus::Green,
            summary: install::dry_run_summary(dry_run, "no rule source file — skipping"),
            detail: None,
        });
    }
    if !dry_run {
        let current = fs::read(&path).unwrap_or_default();
        let _ = config::backup_if_changing(&path, &{
            let mut s = current.clone();
            s.push(0);
            s
        });
        let _ = fs::remove_file(&path);
    }
    Ok(InstallStep {
        id: "rule.source".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, "removed pixel rule source file"),
        detail: Some(format!("path={}", path.display())),
    })
}

// -------------------------------------------------------------------------
// Step 6: remove the pixel agent system prompt
// -------------------------------------------------------------------------

fn remove_agent_prompt(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let path = home.join(".local/share/pixel/agent-prompt.md");
    let subagent_path = home
        .join(".local/share/pixel")
        .join(install::SUBAGENT_PROMPT_FILE);
    let pi_path = home.join(install::PI_PROMPT_REL);
    let existed = path.is_file();
    if !existed && !subagent_path.is_file() && !pi_path.is_file() {
        return Ok(InstallStep {
            id: "agent-prompt".into(),
            status: CheckStatus::Green,
            summary: install::dry_run_summary(dry_run, "no agent-prompt file — skipping"),
            detail: None,
        });
    }
    // Pi's system-prompt file is shared: pixel owns its managed block and
    // recognized pre-marker prompts, not the user's surrounding text. Remove
    // the file only when nothing else remains.
    let pi_original = match fs::read_to_string(&pi_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let pi_cleaned = config::strip_managed_block(&install::strip_unmarked_pi_prompts(&pi_original));
    let pi_touched = pi_cleaned != pi_original;
    let pi_removed = pi_touched && pi_cleaned.trim().is_empty();
    if !dry_run {
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&subagent_path);
        if pi_touched {
            if pi_cleaned.trim().is_empty() {
                let _ = config::backup_if_changing(&pi_path, pi_cleaned.as_bytes())?;
                fs::remove_file(&pi_path)?;
            } else {
                let _ = config::backup_if_changing(&pi_path, pi_cleaned.as_bytes())?;
                fs::write(&pi_path, &pi_cleaned)?;
            }
        }
    }
    let summary = if pi_removed {
        "removed agent-prompt.md, subagent-prompt.md and the pi prompt file"
    } else if pi_touched {
        "removed agent-prompt.md and subagent-prompt.md, kept the text around the pixel block in APPEND_SYSTEM.md"
    } else {
        "removed agent-prompt.md and subagent-prompt.md"
    };
    Ok(InstallStep {
        id: "agent-prompt".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, summary),
        detail: Some(format!(
            "path={} subagent={} pi={}",
            path.display(),
            subagent_path.display(),
            pi_path.display()
        )),
    })
}

// -------------------------------------------------------------------------
// Step 7: remove the pixel binary
// -------------------------------------------------------------------------

/// The binary `uninstall` removes, and whether the user named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemovalTarget {
    path: PathBuf,
    explicit: bool,
}

/// `--binary-path` when given; else the running binary; else the historical
/// `~/.local/bin/pixel`.
fn removal_target(options: &UninstallOptions, home: &Path) -> RemovalTarget {
    if let Some(path) = &options.binary_path {
        return RemovalTarget {
            path: path.clone(),
            explicit: true,
        };
    }
    RemovalTarget {
        path: options
            .running_binary
            .clone()
            .unwrap_or_else(|| home.join(".local").join("bin").join("pixel")),
        explicit: false,
    }
}

/// The package manager whose install tree holds `path` (after resolving
/// symlinks, so `/opt/homebrew/bin/pixel` counts as its Cellar file), and the
/// way to remove it: `(manager, how)`. Homebrew keeps a formula under
/// `…/Cellar/<formula>/<version>/`, whatever its prefix (macOS or Linuxbrew);
/// mise keeps a tool under `…/mise/installs/<dir>/<version>/`, where `<dir>`
/// is the tool name with its backend mangled (`ubi-owner-repo`), so the hint
/// names the directory rather than guess the `mise uninstall` spelling.
fn package_manager_owner(path: &Path) -> Option<(&'static str, String)> {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let parts: Vec<String> = resolved
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let after = |marker: &[&str]| {
        parts
            .windows(marker.len() + 1)
            .find(|w| w[..marker.len()].iter().zip(marker).all(|(a, b)| a == b))
            .map(|w| w[marker.len()].clone())
    };
    if let Some(formula) = after(&["Cellar"]) {
        return Some(("Homebrew", format!("`brew uninstall {formula}`")));
    }
    after(&["mise", "installs"]).map(|dir| {
        (
            "mise",
            format!("`mise uninstall` on the tool installed under `installs/{dir}`"),
        )
    })
}

fn remove_binary(target: &RemovalTarget, dry_run: bool) -> Result<InstallStep> {
    let binary_path = target.path.as_path();
    if !binary_path.is_file() {
        return Ok(InstallStep {
            id: "binary".into(),
            status: CheckStatus::Green,
            summary: install::dry_run_summary(dry_run, "no binary found — skipping"),
            detail: Some(format!("path={}", binary_path.display())),
        });
    }
    if !target.explicit
        && let Some((manager, how)) = package_manager_owner(binary_path)
    {
        // Deleting a managed binary would leave the manager listing a
        // version that is no longer there; its own command removes both.
        return Ok(InstallStep {
            id: "binary".into(),
            status: CheckStatus::Yellow,
            summary: install::dry_run_summary(
                dry_run,
                &format!("left the pixel binary to {manager}: remove it with {how}"),
            ),
            detail: Some(format!("path={}", binary_path.display())),
        });
    }
    if !dry_run {
        let _ = fs::remove_file(binary_path);
    }
    Ok(InstallStep {
        id: "binary".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, "removed pixel binary"),
        detail: Some(format!("path={}", binary_path.display())),
    })
}

// -------------------------------------------------------------------------
// Step 9: the backups left behind
// -------------------------------------------------------------------------

/// What [`config::backup_if_changing`] puts between a file's name and the
/// timestamp of the copy it keeps.
const BACKUP_MARKER: &str = ".pixel-bak.";

/// The directory holding `rel` under `root`.
fn parent_of(root: &Path, rel: &str) -> PathBuf {
    let path = root.join(rel);
    path.parent()
        .map_or_else(|| root.to_path_buf(), Path::to_path_buf)
}

/// Every directory where a global `pixel install` or `pixel uninstall`
/// rewrites a file, hence may have left a backup beside it.
fn global_backup_dirs(home: &Path, codex_home: &Path, opencode_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![
        // Shell profiles, CLAUDE.md and AGENTS.md at the root of the home.
        home.to_path_buf(),
        install::fish_config_dir(home).join("conf.d"),
        home.join(".claude"),
        home.join(config::CLAUDE_HOOKS_DIR),
        home.join(config::DEVIN_CONFIG_DIR),
        parent_of(home, config::CODEX_HOOKS_FILE),
        codex_home.to_path_buf(),
        parent_of(home, config::GEMINI_SETTINGS_FILE),
        home.join(".zcode"),
        parent_of(home, config::ZCODE_CONFIG_FILE),
        parent_of(home, config::CURSOR_HOOKS_FILE),
        home.join(config::PI_CONFIG_DIR),
        home.join(config::PI_CONFIG_DIR).join("extensions"),
        parent_of(home, config::PIXEL_RULES_REL),
        home.join(".local/share/pixel"),
        crate::antigravity::antigravity_config_dir(home),
        crate::antigravity::plugin_dir(home),
        crate::antigravity::cli_plugin_dir(home),
        opencode_dir.to_path_buf(),
    ];
    for root in project_hook_search_roots(home) {
        dirs.push(root.join(".codex"));
    }
    dirs
}

/// Every directory where `pixel install --repo` or `pixel uninstall --repo`
/// rewrites a file inside `repo`.
fn project_backup_dirs(repo: &Path) -> Vec<PathBuf> {
    vec![
        // The root AGENTS.md.
        repo.to_path_buf(),
        repo.join(".codex"),
        repo.join(".claude"),
        repo.join(".devin"),
        repo.join(".pi/extensions"),
        repo.join(".pi/agent"),
        repo.join(".pi/agent/extensions"),
        parent_of(repo, crate::warp::CONFIG_FILE),
    ]
}

/// Whether `name` ends the way [`config::backup_if_changing`] names a copy:
/// `.pixel-bak.<nanos>-<seq>`, both numbers in digits. Anything looser would
/// put a file of the user's (`notes.pixel-bak.txt`) in the `rm` command.
fn is_backup_name(name: &str) -> bool {
    name.rsplit_once(BACKUP_MARKER)
        .and_then(|(_, suffix)| suffix.split_once('-'))
        .is_some_and(|(nanos, seq)| is_digits(nanos) && is_digits(seq))
}

/// A non-empty run of ASCII digits.
fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// The backups present in `dirs` (not recursive), sorted and without
/// duplicates: two entries of `dirs` may name the same directory.
fn find_backups(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = dirs
        .iter()
        .filter_map(|dir| fs::read_dir(dir).ok())
        .flat_map(|entries| entries.flatten().map(|entry| entry.path()))
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .is_some_and(|name| is_backup_name(&name.to_string_lossy()))
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Report the backups left in place. They are never deleted here: each one
/// is the only undo of a write install or uninstall made, and the user's own
/// files are already restored without them, so dropping them is the user's
/// call, made with the command this step prints.
fn backups_step(backups: &[PathBuf], dry_run: bool) -> InstallStep {
    if backups.is_empty() {
        return InstallStep {
            id: "backups".into(),
            status: CheckStatus::Green,
            summary: install::dry_run_summary(dry_run, "no pixel backup left"),
            detail: None,
        };
    }
    let quoted: Vec<String> = backups
        .iter()
        .map(|path| routing::quoted_executable(path))
        .collect();
    InstallStep {
        id: "backups".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(
            dry_run,
            &format!(
                "kept {} backup(s) of the files pixel rewrote, each the undo of one write; remove them once you no longer need them",
                backups.len()
            ),
        ),
        detail: Some(format!("rm -- {}", quoted.join(" "))),
    }
}

// -------------------------------------------------------------------------
// Shared helper: remove pixel run-hook entries from a settings file with a
// top-level `hooks` object (Claude/Devin/Codex/Gemini schema).
// -------------------------------------------------------------------------

fn remove_pixel_hooks_from_settings(
    config_path: &Path,
    exe: &Path,
    dry_run: bool,
) -> Result<(usize, Option<PathBuf>)> {
    if !config_path.is_file() {
        return Ok((0, None));
    }
    let mut value = install::read_settings(config_path)?;
    let mut removed = 0usize;
    if let Some(hooks) = value
        .get_mut("hooks")
        .and_then(serde_json::Value::as_object_mut)
    {
        let before = hooks.clone();
        routing::remove_pixel_hooks(hooks, exe);
        // Flat-schema (Cursor-style) entries are matched by executable
        // ownership, not by a command substring.
        routing::remove_flat_pixel_hooks(hooks, exe);
        removed += usize::from(*hooks != before);
        let event_keys: Vec<String> = hooks.keys().cloned().collect();
        for event in event_keys {
            if let Some(existing) = hooks.get(&event) {
                let mut filtered = existing.clone();
                for marker in PIXEL_HOOK_MARKERS {
                    filtered = config::remove_hook_entries(&filtered, marker);
                }
                let changed = filtered != *hooks.get(&event).unwrap();
                if filtered.as_array().is_some_and(std::vec::Vec::is_empty) {
                    hooks.remove(&event);
                    removed += 1;
                } else if changed {
                    hooks.insert(event, filtered);
                    removed += 1;
                }
            }
        }
        if hooks.is_empty()
            && let Some(obj) = value.as_object_mut()
        {
            obj.remove("hooks");
        }
    }
    let mut backup_path = None;
    if !dry_run && removed > 0 {
        backup_path = install::write_settings(config_path, &value, dry_run)?;
    }
    Ok((removed, backup_path))
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn routing_uninstall_restores_rtk_fragment_and_preserves_later_user_changes() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let path = home.join(".claude/settings.json");
        let rtk =
            json!({"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]});
        let foreign = json!({"matcher":"startup","hooks":[{"type":"command","command":"keep-security-check"}]});
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":[rtk.clone()],"SessionStart":[foreign.clone()]}}),
            false,
        )
        .unwrap();
        routing::install_provider(
            home,
            Path::new("/tmp/pixel"),
            routing::Provider::Claude,
            false,
        )
        .unwrap();
        assert!(home.join(routing::RTK_BACKUP).is_file());
        let mut installed = install::read_settings(&path).unwrap();
        installed["later_user_change"] = json!(true);
        install::write_settings(&path, &installed, false).unwrap();
        remove_claude_hooks(home, Path::new("/tmp/pixel"), true).unwrap();
        assert_eq!(install::read_settings(&path).unwrap(), installed);
        remove_claude_hooks(home, Path::new("/tmp/pixel"), false).unwrap();
        let restored = install::read_settings(&path).unwrap();
        assert_eq!(restored["hooks"]["PreToolUse"], json!([rtk]));
        assert_eq!(restored["hooks"]["SessionStart"], json!([foreign]));
        assert_eq!(restored["later_user_change"], true);
        assert!(!home.join(routing::RTK_BACKUP).exists());
        remove_claude_hooks(home, Path::new("/tmp/pixel"), false).unwrap();
        assert_eq!(install::read_settings(&path).unwrap(), restored);
    }

    #[test]
    fn remove_cursor_hooks_counts_and_preserves_foreign_entries() {
        let home = tempfile::tempdir().unwrap();
        let exe = Path::new("/tmp/pixel");
        let path = home.path().join(config::CURSOR_HOOKS_FILE);
        let pixel_guard =
            json!({"command":format!("'{}' run-hook guard --provider cursor", exe.display())});
        let pixel_metrics =
            json!({"command":format!("'{}' run-hook metrics --provider cursor", exe.display())});
        let foreign = json!({"command":"notify-send done"});
        install::write_settings(
            &path,
            &json!({"hooks":{
                "preToolUse":[pixel_guard, foreign.clone()],
                "postToolUse":[pixel_metrics],
            }}),
            false,
        )
        .unwrap();
        let step = remove_cursor_hooks(home.path(), exe, false).unwrap();
        assert!(step.summary.contains("removed 1"), "{}", step.summary);
        let restored = install::read_settings(&path).unwrap();
        assert_eq!(restored["hooks"]["preToolUse"], json!([foreign]));
        assert!(restored["hooks"].get("postToolUse").is_none());
        // Nothing pixel-owned left: a second pass reports zero and does not
        // rewrite the file.
        let step = remove_cursor_hooks(home.path(), exe, false).unwrap();
        assert!(step.summary.contains("removed 0"), "{}", step.summary);
        assert_eq!(install::read_settings(&path).unwrap(), restored);
    }

    #[test]
    fn routing_uninstall_removes_direct_codex_and_devin_commands() {
        for provider in [routing::Provider::Codex, routing::Provider::Devin] {
            let home = tempfile::tempdir().unwrap();
            routing::install_provider(home.path(), Path::new("/tmp/pixel"), provider, false)
                .unwrap();
            let path = provider.path(home.path());
            assert!(
                remove_pixel_hooks_from_settings(&path, Path::new("/tmp/pixel"), false)
                    .unwrap()
                    .0
                    > 0
            );
            assert_eq!(install::read_settings(&path).unwrap(), json!({}));
        }
    }

    /// A mixed event keeps its foreign entries after the pixel ones go, an
    /// event holding only pixel entries disappears, an untouched event is
    /// neither rewritten nor counted.
    #[test]
    fn remove_pixel_hooks_from_settings_rewrites_only_the_events_that_changed() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("settings.json");
        let lint = json!({"matcher":"Bash","hooks":[{"type":"command","command":"lint"}]});
        let guard = json!({"matcher":"Bash","hooks":[{"type":"command","command":format!("sh {}", config::GUARD_HOOK)}]});
        let start = json!({"hooks":[{"type":"command","command":format!("pixel run-hook {}", config::SESSION_START_HOOK)}]});
        let stop = json!({"hooks":[{"type":"command","command":"say done"}]});
        install::write_settings(
            &path,
            &json!({"hooks":{
                "PreToolUse":[guard, lint.clone()],
                "SessionStart":[start],
                "Stop":[stop.clone()],
            }, "theme":"dark"}),
            false,
        )
        .unwrap();
        let (removed, backup) =
            remove_pixel_hooks_from_settings(&path, Path::new("/tmp/pixel"), false).unwrap();
        assert_eq!(removed, 2, "PreToolUse rewritten, SessionStart dropped");
        assert!(backup.is_some());
        let after = install::read_settings(&path).unwrap();
        assert_eq!(after["hooks"]["PreToolUse"], json!([lint]));
        assert!(after["hooks"].get("SessionStart").is_none(), "{after}");
        assert_eq!(after["hooks"]["Stop"], json!([stop]));
        assert_eq!(after["theme"], "dark");
        let (again, _) =
            remove_pixel_hooks_from_settings(&path, Path::new("/tmp/pixel"), false).unwrap();
        assert_eq!(again, 0, "nothing left to remove");
        assert_eq!(
            remove_pixel_hooks_from_settings(
                &home.path().join("absent.json"),
                Path::new("/tmp/pixel"),
                false
            )
            .unwrap(),
            (0, None)
        );
    }

    #[test]
    fn legacy_composed_guard_signature_accepts_both_hook_verbs() {
        let sidecar = Path::new("/p/.codex/pixel-composed.json");
        let group = |command: &str| vec![json!({"hooks":[{"type":"command","command": command}]})];
        for verb in ["run-hook", "hook"] {
            let command = format!(
                "'/tmp/pixel' {verb} composed-guard --provider codex --backup '/p/.codex/pixel-composed.json'"
            );
            assert!(
                legacy_composed_guard_signature(&group(&command), sidecar),
                "`{verb}` composed-guard entry is Pixel's: {command}"
            );
        }
        assert!(!legacy_composed_guard_signature(
            &group(
                "'/tmp/pixel' run-hook guard --provider codex --backup '/p/.codex/pixel-composed.json'"
            ),
            sidecar
        ));
        assert!(!legacy_composed_guard_signature(
            &group("'/tmp/pixel' run-hook composed-guard --provider codex --backup '/other.json'"),
            sidecar
        ));
    }

    #[test]
    fn project_composed_guard_uninstall_restores_exact_snapshot() {
        let home = tempfile::tempdir().unwrap();
        let codex = home.path().join("Documents/project/.codex");
        let path = codex.join("hooks.json");
        let original =
            json!([{"matcher":"Bash","hooks":[{"type":"command","command":"keep-guard"}]}]);
        let managed = json!([{"hooks":[{"type":"command","command":"'/tmp/pixel' hook composed-guard --provider codex --backup '/unused'"}]}]);
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":managed.clone()}}),
            false,
        )
        .unwrap();
        install::write_settings(
            &codex.join(routing::CODEX_COMPOSED_BACKUP),
            &json!({
                "version": 1,
                "provider": "codex",
                "pre_tool_use": original.clone(),
                "managed_pre_tool_use": managed,
            }),
            false,
        )
        .unwrap();
        make_private(&codex.join(routing::CODEX_COMPOSED_BACKUP));

        let step = remove_project_codex_hooks(home.path(), Path::new("/tmp/pixel"), false).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        assert_eq!(
            install::read_settings(&path).unwrap()["hooks"]["PreToolUse"],
            original
        );
        assert!(!codex.join(routing::CODEX_COMPOSED_BACKUP).exists());
    }

    #[test]
    fn project_composed_guard_uninstall_preserves_changed_managed_value() {
        let home = tempfile::tempdir().unwrap();
        let codex = home.path().join("Documents/project/.codex");
        let path = codex.join("hooks.json");
        let managed = json!([{"hooks":[{"type":"command","command":"'/tmp/pixel' hook composed-guard --provider codex --backup '/unused'"}]}]);
        let changed = json!([
            {"hooks":[{"type":"command","command":"'/tmp/pixel' hook composed-guard --provider codex --backup '/unused'"}]},
            {"matcher":"Bash","hooks":[{"type":"command","command":"user-change"}]}
        ]);
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":changed.clone()}}),
            false,
        )
        .unwrap();
        install::write_settings(
            &codex.join(routing::CODEX_COMPOSED_BACKUP),
            &json!({
                "version": 1,
                "provider": "codex",
                "pre_tool_use": [],
                "managed_pre_tool_use": managed,
            }),
            false,
        )
        .unwrap();
        make_private(&codex.join(routing::CODEX_COMPOSED_BACKUP));

        let step = remove_project_codex_hooks(home.path(), Path::new("/tmp/pixel"), false).unwrap();
        assert_eq!(step.status, CheckStatus::Yellow);
        assert_eq!(
            install::read_settings(&path).unwrap()["hooks"]["PreToolUse"],
            changed
        );
        assert!(codex.join(routing::CODEX_COMPOSED_BACKUP).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn remove_agent_prompt_fails_when_pi_file_is_unreadable() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let pi_path = home.path().join(install::PI_PROMPT_REL);
        fs::create_dir_all(pi_path.parent().unwrap()).unwrap();
        fs::write(&pi_path, b"user note").unwrap();
        // Remove read permission but keep write permission.
        fs::set_permissions(&pi_path, fs::Permissions::from_mode(0o200)).unwrap();

        // An unreadable shared file must be an error, never "empty".
        let result = remove_agent_prompt(home.path(), false);
        assert!(
            result.is_err(),
            "expected error for unreadable pi file, got {result:?}"
        );
    }

    fn make_private(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = path;
    }
}

#[cfg(test)]
mod backup_tests {
    use super::*;

    #[test]
    fn find_backups_lists_each_backup_file_once_in_path_order() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        for path in [
            b.join("settings.json.pixel-bak.2-0"),
            a.join("hooks.json.pixel-bak.1-1"),
            // The user's own files and pixel's temp files are not backups.
            a.join("hooks.json"),
            a.join("hooks.pixel-tmp"),
        ] {
            fs::write(path, "x").unwrap();
        }
        // A directory is never a backup, whatever its name.
        fs::create_dir_all(a.join("dir.pixel-bak.3-0")).unwrap();

        // `a` twice: the global list can name one directory two ways
        // (`~/.codex` and `$CODEX_HOME`).
        let found = find_backups(&[b.clone(), a.clone(), a.clone(), root.path().join("absent")]);

        assert_eq!(
            found,
            vec![
                a.join("hooks.json.pixel-bak.1-1"),
                b.join("settings.json.pixel-bak.2-0")
            ]
        );
    }

    /// Only the name `backup_if_changing` writes is a backup: a file of the
    /// user's that merely contains the marker must never reach the `rm`.
    #[test]
    fn is_backup_name_accepts_only_the_suffix_pixel_writes() {
        assert!(is_backup_name(
            "settings.json.pixel-bak.1790778294642045925-1"
        ));
        assert!(is_backup_name("a.pixel-bak.x.pixel-bak.12-0"));
        for name in [
            "notes.pixel-bak.txt",
            "settings.json.pixel-bak.",
            "settings.json.pixel-bak.12",
            "settings.json.pixel-bak.12-",
            "settings.json.pixel-bak.-3",
            "settings.json.pixel-bak.12-3a",
            "settings.json.pixel-bak.1a-3",
            "settings.json",
        ] {
            assert!(!is_backup_name(name), "{name}");
        }
    }

    #[test]
    fn parent_of_names_the_directory_holding_a_managed_file() {
        let home = Path::new("/home/me");
        assert_eq!(
            parent_of(home, config::GEMINI_SETTINGS_FILE),
            home.join(".gemini")
        );
        assert_eq!(
            parent_of(home, config::ZCODE_CONFIG_FILE),
            home.join(".zcode/cli")
        );
    }

    #[test]
    fn backups_step_quotes_each_path_of_the_removal_command() {
        let step = backups_step(
            &[
                PathBuf::from("/home/it's me/.claude/settings.json.pixel-bak.1-0"),
                PathBuf::from("/home/me/.codex/hooks.json.pixel-bak.2-1"),
            ],
            true,
        );
        assert_eq!(step.id, "backups");
        assert_eq!(step.status, CheckStatus::Green);
        assert_eq!(
            step.summary,
            "[dry-run] would report: kept 2 backup(s) of the files pixel rewrote, each the undo of one write; remove them once you no longer need them"
        );
        assert_eq!(
            step.detail.as_deref(),
            Some(
                "rm -- '/home/it'\\''s me/.claude/settings.json.pixel-bak.1-0' '/home/me/.codex/hooks.json.pixel-bak.2-1'"
            )
        );
    }
}
