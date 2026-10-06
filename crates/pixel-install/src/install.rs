// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Idempotent provider integration: safe shell routing and lifecycle context.
//! Unknown overlapping hooks are preserved rather than double-rewritten.
//! Configuration alone is not proof that an agent has executed the hooks.

use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::InstallError;
use crate::config;

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
#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    /// Path to the pixel binary the installed hooks point at. Defaults to
    /// the current exe.
    pub executable_path: Option<PathBuf>,
    /// Home directory. Defaults to `$HOME`.
    pub home: Option<PathBuf>,
    /// Shell whose profile is checked for a legacy `claude()` wrapper block
    /// to remove, as a `$SHELL`-style value (`fish`, `/bin/bash`, …).
    /// Defaults to the account's login shell, then `$SHELL`.
    pub shell: Option<String>,
    /// Deprecated and ignored, retained for source compatibility. Install no
    /// longer probes the Claude executable or its version.
    pub claude_executable: Option<PathBuf>,
    /// If true, compute and report every step's outcome exactly as a real
    /// run would, but perform no filesystem writes: no settings.json edits,
    /// no hook files, no agent-config rewrites, no backups, no directory
    /// creation. Safe to run against a real `$HOME` to preview an install.
    pub dry_run: bool,
    /// Repository root for native-hook cleanup and project task adapters
    /// (`pixel install --repo <path>`). When set, ONLY repo-local steps run:
    /// retired retrieval callbacks are removed from Claude and Codex while
    /// native hooks are preserved; Devin task hooks and the Pi project guard
    /// are installed. Retired Pixel material is also removed from
    /// `.codex/config.toml`, `AGENTS.md`, and `.warp/.mcp.json`. Global prompt
    /// and lifecycle-hook installation is skipped.
    pub repo: Option<PathBuf>,
}

/// First executable file named `name` on PATH.
#[cfg(test)]
pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    find_in_paths(name, &std::env::var_os("PATH")?)
}

/// First executable file named `name` in the PATH-style list `path`.
#[cfg(test)]
fn find_in_paths(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path).find_map(|dir| {
        let candidate = dir.join(name);
        if !candidate.is_file() {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = candidate
                .metadata()
                .is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0);
            executable.then_some(candidate)
        }
        #[cfg(not(unix))]
        {
            Some(candidate)
        }
    })
}

/// Run `pixel install`. Idempotent: safe to re-run.
///
/// Global install deploys the shared agent prompt, keeps task-lifecycle
/// accounting hooks for configured hosts, and removes retired automatic
/// retrieval registrations. It installs Pi's explicit impact command only
/// when the standard `~/.pi/agent` configuration directory already exists;
/// a machine without Pi is left untouched. Repo install preserves native task
/// hooks while removing retired retrieval callbacks. The retired Claude
/// `claude()` shell wrapper is removed because native task hooks now provide
/// the lifecycle integration.
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
    let exe = stable_exe_path(executable_path);

    let dry_run = options.dry_run;
    let codex_home = crate::codex_config::codex_home(&home, options.home.is_some());
    if let Some(repo) = &options.repo {
        return install_project(repo, &home, &codex_home, &exe, dry_run);
    }
    let mut steps = vec![
        deploy_agent_prompt(&home, dry_run)?,
        crate::pi_global::install(&home, &exe, dry_run)?,
        // Keep task accounting and configured task gates available to every
        // Claude session. Automatic retrieval guidance, post-edit advice and
        // metrics are excluded from the native-default profile.
        crate::routing::install_at_scoped(
            &home,
            &crate::routing::Provider::Claude.path(&home),
            &exe,
            crate::routing::Provider::Claude,
            crate::routing::HookScope::TaskEventsOnly,
            &[],
            dry_run,
        )?,
        // The zsh `claude()` wrapper only ever fired for human login shells
        // and would now double-inject alongside SessionStart — strip it.
        remove_legacy_wrappers(&home, options.shell.as_deref(), dry_run)?,
        crate::codex_config::remove_developer_instructions(&codex_home, dry_run)?,
        crate::codex_config::install_task_hooks(&codex_home, &exe, dry_run)?,
    ];
    let opencode_dir = crate::opencode_config::opencode_config_dir(&home, options.home.is_some());
    if opencode_dir.is_dir() {
        steps.push(crate::opencode_config::install_opencode(
            &opencode_dir,
            &home,
            &exe,
            dry_run,
        )?);
    }
    // Devin imports Claude's hooks (`read_config_from.claude`, on by
    // default), so without its own lifecycle entries a Devin session's only
    // Pixel guidance is the imported Claude text — measured at ~11 KB of
    // Claude-specific doctrine per session. Its own protocol is the three
    // lifecycle hooks with `--provider devin`; the guard and the metrics
    // relay stay repo-scoped (`pixel install --repo`), like Claude's
    // enforcement. Skipped when Devin has never run: creating a config for
    // a tool that is not installed would be intrusive, and `doctor` judges
    // what Pixel wrote.
    if home.join(crate::config::DEVIN_CONFIG_DIR).is_dir() {
        steps.push(crate::routing::install_at_scoped(
            &home,
            &crate::routing::Provider::Devin.path(&home),
            &exe,
            crate::routing::Provider::Devin,
            crate::routing::HookScope::LifecycleOnly,
            &[],
            dry_run,
        )?);
    }
    if crate::antigravity::antigravity_config_dir(&home).is_dir() {
        steps.push(crate::antigravity::deploy_plugin_assets(
            &home, &exe, dry_run,
        )?);
        steps.push(crate::antigravity::enable_plugin_in_config(&home, dry_run)?);
        steps.push(crate::antigravity::remove_global_hooks(
            &home, &exe, dry_run,
        )?);
    }
    // Copilot CLI keeps user-level hooks in `~/.copilot/hooks/*.json`;
    // pixel writes a dedicated `pixel.json` rather than merging into
    // foreign hook files.
    if crate::copilot_config::copilot_hooks_dir(&home).is_some() {
        steps.push(crate::copilot_config::install_copilot_hooks(
            &home, &exe, dry_run,
        )?);
    }
    steps.push(crate::routing::install_zcode_at(&home, &exe, dry_run)?);
    if home.join(".cursor").is_dir() {
        steps.push(install_cursor_hooks(&home, &exe, dry_run)?);
    }

    Ok(install_report(&exe, &home, dry_run, steps))
}

/// The report over finished install or uninstall `steps`: one count per
/// status, and `ok` while no step is red. `home` is the home directory, or
/// the repository for a `--repo` run.
pub(crate) fn install_report(
    executable: &Path,
    home: &Path,
    dry_run: bool,
    steps: Vec<InstallStep>,
) -> InstallReport {
    let count = |status| steps.iter().filter(|s| s.status == status).count();
    let summary = InstallSummary {
        green: count(CheckStatus::Green),
        yellow: count(CheckStatus::Yellow),
        red: count(CheckStatus::Red),
    };
    InstallReport {
        version: "v1".into(),
        ok: summary.red == 0,
        executable_path: executable.display().to_string(),
        home: home.display().to_string(),
        dry_run,
        steps,
        summary,
    }
}

/// Repo-local install (`pixel install --repo <path>`): project-scoped agent
/// enforcement instead of the global prompt deploy. Writes:
///   - `<repo>/.codex/config.toml` — any retired Pixel
///     `developer_instructions` block is removed while user text is preserved;
///   - `<repo>/.codex/hooks.json` — the composed-guard PreToolUse group plus
///     its `pixel-composed-guard-backup.json` sidecar, which snapshots any
///     pre-existing project hooks so the composed runtime can replay them;
///   - `<repo>/.devin/config.local.json` — Pixel's Devin PreToolUse rewrite,
///     PermissionRequest retrieval approval, and prompt-context hooks merged
///     alongside any foreign entries;
///   - `<repo>/.claude/settings.local.json` — a pixel `run-hook guard
///     --provider claude` PreToolUse group merged alongside any foreign
///     entries (the personal project settings: the command names this
///     machine's binary, so the shared `settings.json` never carries it);
///   - `<repo>/.pi/extensions/pixel-guard.ts` — pi's guard extension
///     ([`crate::pi_project`]).
///   - any retired Pixel-first managed block in `<repo>/AGENTS.md` is removed
///     ([`crate::pixel_first`]).
///
/// It also removes the `pixel mcp` entry an older release wrote into
/// `<repo>/.warp/.mcp.json` ([`crate::warp`]).
///
/// Every one of those files is then listed in the clone's `info/exclude`
/// ([`crate::repo_git`]). Codex has no personal project file, so a
/// `.codex/hooks.json` the repository tracks is left alone: the composed
/// guard would put this machine's path into a file every clone runs.
fn install_project(
    repo: &Path,
    home: &Path,
    codex_home: &Path,
    exe: &Path,
    dry_run: bool,
) -> Result<InstallReport> {
    let codex_dir = repo.join(".codex");
    let codex_hooks = codex_dir.join(crate::codex_config::HOOKS_FILE);
    // These two cleanup steps are independent of hook ownership. Run them
    // before validating project hooks so a foreign hook conflict cannot leave
    // the retired always-on Codex guidance in place.
    let retired_codex_instructions =
        crate::codex_config::remove_developer_instructions(&codex_dir, dry_run)?;
    let retired_pixel_first_rules = crate::pixel_first::uninstall_rules(repo, dry_run)?;
    let codex_step = if crate::repo_git::is_tracked(repo, CODEX_PROJECT_HOOKS) {
        InstallStep {
            id: "hooks.codex".into(),
            status: CheckStatus::Yellow,
            summary: dry_run_summary(
                dry_run,
                "codex project guard not installed: .codex/hooks.json is tracked by git, and the guard would carry this machine's pixel path into every clone",
            ),
            detail: Some(codex_hooks.display().to_string()),
        }
    } else {
        crate::routing::install_project_codex_at(
            home,
            &codex_home.join(crate::codex_config::HOOKS_FILE),
            &codex_hooks,
            exe,
            dry_run,
        )?
    };
    let steps = vec![
        crate::routing::install_project_claude_at(repo, home, exe, dry_run)?,
        retired_codex_instructions,
        codex_step,
        crate::routing::install_project_devin_at(repo, exe, dry_run)?,
        crate::pi_project::install(repo, exe, dry_run)?,
        crate::warp::retire(repo, dry_run)?,
        retired_pixel_first_rules,
        exclude_project_artifacts(repo, dry_run)?,
    ];

    Ok(install_report(exe, repo, dry_run, steps))
}

/// The Codex project hook file, relative to the repository.
const CODEX_PROJECT_HOOKS: &str = ".codex/hooks.json";

/// Resolve the path written into hook commands. Hook entries live for months,
/// so they must survive package-manager upgrades: canonicalizing a stable
/// symlink such as `/opt/homebrew/bin/pixel` bakes in a versioned store path
/// (`Cellar/pixel/<ver>/bin/pixel`) that dangles after the next upgrade.
/// Prefer the first PATH entry whose canonical form is this binary, keeping
/// the symlink path itself; fall back to the canonicalized path when PATH has
/// no match (uninstalled-location runs, custom `--executable-path`).
pub fn stable_exe_path(executable_path: PathBuf) -> PathBuf {
    let paths = std::env::var_os("PATH").unwrap_or_default();
    stable_exe_path_in(executable_path, &paths)
}

fn stable_exe_path_in(executable_path: PathBuf, paths: &std::ffi::OsStr) -> PathBuf {
    let canonical = executable_path
        .canonicalize()
        .unwrap_or_else(|_| executable_path.clone());
    let Some(name) = executable_path.file_name() else {
        return canonical;
    };
    let name = name.to_string_lossy();
    std::env::split_paths(paths)
        .map(|dir| {
            let dir = if dir.is_absolute() {
                dir
            } else {
                std::env::current_dir().map_or(dir.clone(), |cwd| cwd.join(dir))
            };
            dir.join(name.as_ref())
        })
        .find(|candidate| {
            candidate.is_file()
                && candidate.canonicalize().ok().as_deref() == Some(canonical.as_path())
        })
        .unwrap_or(canonical)
}

/// A file `pixel install --repo` may write, relative to the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoArtifact {
    /// Path relative to the repository root.
    pub path: &'static str,
    /// Whether the file names this machine's pixel binary, so the install
    /// lists it in the clone's `info/exclude` to keep it out of commits.
    pub machine_local: bool,
}

/// Every file `pixel install --repo` may write. This includes the two
/// portable migration targets, which are rewritten only when they contain a
/// retired Pixel block. The per-project list in `website/content/docs.md` and
/// the `--repo` help name exactly these paths, which `docs_drift::` checks.
pub const REPO_ARTIFACTS: &[RepoArtifact] = &[
    RepoArtifact {
        path: crate::routing::CLAUDE_LOCAL_SETTINGS,
        machine_local: true,
    },
    RepoArtifact {
        path: crate::routing::RTK_BACKUP,
        machine_local: true,
    },
    RepoArtifact {
        path: ".codex/config.toml",
        machine_local: false,
    },
    RepoArtifact {
        path: CODEX_PROJECT_HOOKS,
        machine_local: true,
    },
    RepoArtifact {
        path: ".codex/pixel-composed-guard-backup.json",
        machine_local: true,
    },
    RepoArtifact {
        path: crate::routing::DEVIN_LOCAL_CONFIG,
        machine_local: true,
    },
    RepoArtifact {
        path: crate::pi_project::EXTENSION,
        machine_local: true,
    },
    RepoArtifact {
        path: "AGENTS.md",
        machine_local: false,
    },
];

/// The [`REPO_ARTIFACTS`] that name this machine's pixel binary.
fn machine_local_artifacts() -> Vec<&'static str> {
    REPO_ARTIFACTS
        .iter()
        .filter(|artifact| artifact.machine_local)
        .map(|artifact| artifact.path)
        .collect()
}

/// List the machine-local [`REPO_ARTIFACTS`] in the clone's `info/exclude`, so a
/// `git add -A` cannot publish a hook that points at this machine's binary.
fn exclude_project_artifacts(repo: &Path, dry_run: bool) -> Result<InstallStep> {
    let added = crate::repo_git::exclude_locally(repo, &machine_local_artifacts(), dry_run)?;
    Ok(InstallStep {
        id: "repo.git-exclude".into(),
        status: CheckStatus::Green,
        summary: dry_run_summary(
            dry_run,
            &format!(
                "{} machine-local path(s) added to the clone's info/exclude",
                added.len()
            ),
        ),
        detail: (!added.is_empty()).then(|| added.join(" ")),
    })
}

/// File name, under `~/.local/share/pixel/`, of the short prompt appended to
/// Claude Code sub-agents (`--append-subagent-system-prompt-file`, print mode
/// only). Kept apart from `agent-prompt.md` because a sub-agent gets neither
/// the session's `--append-system-prompt-file` nor its history, and because a
/// long prompt loses to a long agent body: this one stays under 2 KB.
pub(crate) const SUBAGENT_PROMPT_FILE: &str = "subagent-prompt.md";

/// The agent prompt as bundled in the binary.
pub(crate) const AGENT_PROMPT_ASSET: &str = include_str!("../assets/pixel-agent-prompt.md");

/// Pi keeps operational policy in its extension and exposes only this short rule.
#[cfg(test)]
pub(crate) const PI_PROMPT_ASSET: &str = "Use the pixel tool for repository retrieval and repository Git workflows. The extension injects task context and post-edit impact automatically. Policy is advisory by default; `pixel config policy enforce` (or PIXEL_POLICY=enforce) opts into supported retrieval checks, `pixel config policy off` disables them. A prior-pixel-call edit gate applies only while Pixel reports healthy — when Pixel health is unhealthy or unknown, edits stay allowed. Native compositions and unsupported capabilities remain available; a failed Pixel operation allows native tools.";

const LEGACY_PI_PROMPT_BEGIN: &str = "# Pixel Retrieval Layer\n";
const LEGACY_PI_PROMPT_END: &str = "All commands accept `[PATH]`, default current directory.\n";
const LEGACY_PI_PROMPT_BEGIN_V0_2: &str = "# Pixel Retrieval Layer — Mandatory Agent Protocol\n";
const LEGACY_PI_PROMPT_END_V0_2: &str =
    "All commands accept `[PATH]` (default: current directory).\n";

/// The sub-agent prompt as bundled in the binary.
pub(crate) const SUBAGENT_PROMPT_ASSET: &str = include_str!("../assets/pixel-subagent-prompt.md");

/// Pi's system-prompt file, relative to home. Pi reads it automatically — no
/// flag, no hook — and a user may already keep instructions in it, so pixel
/// owns only the managed block inside it.
pub(crate) const PI_PROMPT_REL: &str = ".pi/agent/APPEND_SYSTEM.md";

/// The prompt files a `pixel install` deployed under `home` that no longer
/// match the copies bundled in this binary, by file name. Claude's
/// SessionStart hook and the sub-agent flag read these files as they are, so
/// after an upgrade every agent keeps the old command map until the install
/// is rerun. A file that was never deployed is not listed: an install that
/// never happened is `pixel doctor`'s report, not an upgrade's.
pub fn stale_prompts(home: &Path) -> Vec<&'static str> {
    let dir = home.join(".local/share/pixel");
    [
        ("agent-prompt.md", AGENT_PROMPT_ASSET),
        (SUBAGENT_PROMPT_FILE, SUBAGENT_PROMPT_ASSET),
    ]
    .into_iter()
    .filter(|(name, asset)| {
        fs::read_to_string(dir.join(name)).is_ok_and(|deployed| deployed != *asset)
    })
    .map(|(name, _)| name)
    .collect()
}

/// Copy Pixel's explicit-use prompt references to `~/.local/share/pixel/` and
/// migrate the retired automatic prompt out of Pi's global system-prompt file.
fn deploy_agent_prompt(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let dest_dir = home.join(".local/share/pixel");
    let dest = dest_dir.join("agent-prompt.md");
    let subagent_dest = dest_dir.join(SUBAGENT_PROMPT_FILE);
    let pi_dest = home.join(PI_PROMPT_REL);
    if dry_run {
        return Ok(InstallStep {
            id: "agent-prompt".into(),
            status: CheckStatus::Green,
            summary: format!(
                "would deploy agent-prompt.md and {SUBAGENT_PROMPT_FILE}; remove automatic Pi prompt"
            ),
            detail: Some(format!(
                "dest={} subagent={} pi={}",
                dest.display(),
                subagent_dest.display(),
                pi_dest.display()
            )),
        });
    }
    fs::create_dir_all(&dest_dir)?;
    let needs_write = write_if_changed(&dest, AGENT_PROMPT_ASSET)?;
    let subagent_written = write_if_changed(&subagent_dest, SUBAGENT_PROMPT_ASSET)?;
    let pi_removed = remove_pi_prompt(&pi_dest)?;
    Ok(InstallStep {
        id: "agent-prompt".into(),
        status: CheckStatus::Green,
        summary: format!(
            "{} agent-prompt.md, {} {SUBAGENT_PROMPT_FILE}{}",
            if needs_write { "deployed" } else { "verified" },
            if subagent_written {
                "deployed"
            } else {
                "verified"
            },
            if pi_removed {
                ", removed Pixel's automatic Pi APPEND_SYSTEM.md block"
            } else {
                ""
            }
        ),
        detail: Some(format!(
            "path={} subagent={} pi={}",
            dest.display(),
            subagent_dest.display(),
            pi_dest.display()
        )),
    })
}

/// Remove Pixel-owned prompt material from Pi's automatically loaded global
/// system prompt, preserving all user text and backing up changed bytes.
fn remove_pi_prompt(path: &Path) -> Result<bool> {
    let existing = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false);
        }
        Err(e) => return Err(e.into()),
    };
    let wanted = config::strip_managed_block(&strip_unmarked_pi_prompts(&existing));
    if wanted == existing {
        return Ok(false);
    }
    if wanted.trim().is_empty() {
        config::backup_if_changing(path, wanted.as_bytes())?;
        fs::remove_file(path)?;
    } else {
        write_atomically(path, &wanted)?;
    }
    Ok(true)
}

/// Write `content` to `path` unless the file already holds exactly it.
/// Returns whether a write happened.
fn write_if_changed(path: &Path, content: &str) -> Result<bool> {
    let needs_write = match fs::read_to_string(path) {
        Ok(existing) => existing != content,
        Err(_) => true,
    };
    if needs_write {
        fs::write(path, content)?;
    }
    Ok(needs_write)
}

/// Put the bundled prompt inside the managed markers in Pi's system-prompt
/// file, backing the previous bytes up first. Pi reads that file
/// automatically and a user may already keep instructions in it: everything
/// outside the markers survives, and a failed write is an error rather than a
/// silently green step (the prompts under `~/.local/share/pixel/` are pixel's
/// own files; this one is not).
#[cfg(test)]
fn write_pi_prompt(path: &Path) -> Result<bool> {
    let existing = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let wanted = managed_pi_content(&existing, PI_PROMPT_ASSET);
    if wanted == existing {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_atomically(path, &wanted)?;
    Ok(true)
}

/// The content Pi's system-prompt file should hold, given `existing`.
///
/// A copy of the prompt that is not inside the markers yet — what an install
/// before the markers wrote verbatim, or a hand copy — is wrapped in place
/// instead of appended a second time. Known historical copies beside an
/// existing managed block are removed by their original boundaries; user
/// text before and after survives. Anything else follows the Markdown
/// agent-config rules ([`config::apply_managed_markers`]).
#[cfg(test)]
pub(crate) fn managed_pi_content(existing: &str, asset: &str) -> String {
    if !existing.contains(config::MANAGED_BEGIN) {
        let (cleaned, removed) = config::strip_stale_blocks(existing);
        let source = if removed == 0 { existing } else { &cleaned };
        let legacy_range = source
            .find(AGENT_PROMPT_ASSET)
            .map(|start| (start, start + AGENT_PROMPT_ASSET.len()))
            .or_else(|| legacy_pi_prompt_range(source, ""));
        let replace_range =
            legacy_range.or_else(|| source.find(asset).map(|start| (start, start + asset.len())));
        if let Some((start, end)) = replace_range {
            let begin = config::MANAGED_BEGIN;
            let marker_end = config::MANAGED_END;
            let block = format!("{begin}\n{asset}\n{marker_end}\n");
            return strip_unmarked_pi_prompts(&format!(
                "{}{}{}",
                &source[..start],
                block,
                &source[end..]
            ));
        }
    }
    config::apply_managed_markers(&strip_unmarked_pi_prompts(existing), asset)
}

fn legacy_pi_prompt_range(source: &str, preceding: &str) -> Option<(usize, usize)> {
    [
        (
            LEGACY_PI_PROMPT_BEGIN,
            LEGACY_PI_PROMPT_END,
            "## MANDATORY WORKFLOW\n",
            "## REPLACEMENT MAP\n",
        ),
        (
            LEGACY_PI_PROMPT_BEGIN_V0_2,
            LEGACY_PI_PROMPT_END_V0_2,
            "## THE COMPLETE REPLACEMENT MAP\n",
            "## ENVIRONMENT\n",
        ),
    ]
    .into_iter()
    .filter_map(|(begin, end, section_a, section_b)| {
        source.rmatch_indices(begin).find_map(|(start, _)| {
            if (start > 0 && source.as_bytes()[start - 1] != b'\n')
                || inside_markdown_fence(preceding, &source[..start])
            {
                return None;
            }
            let after_start = &source[start..];
            let end = after_start.find(end)? + end.len();
            let section = &after_start[..end];
            (section.contains(section_a) && section.contains(section_b))
                .then_some((start, start + end))
        })
    })
    .max_by_key(|(start, _)| *start)
}

/// Remove only known historical prompt bodies outside Pi's managed block.
pub(crate) fn strip_unmarked_pi_prompts(text: &str) -> String {
    let Some(marker_start) = text.find(config::MANAGED_BEGIN) else {
        return strip_legacy_pi_prompts(text, "");
    };
    let managed_and_tail = &text[marker_start..];
    let marker_end = managed_and_tail
        .find(config::MANAGED_END)
        .map_or(managed_and_tail.len(), |end| {
            end + config::MANAGED_END.len()
        });
    let prefix = strip_legacy_pi_prompts(&text[..marker_start], "");
    // User fences span the managed block; fences owned by that block do not
    // affect the surrounding text.
    let suffix = strip_legacy_pi_prompts(&managed_and_tail[marker_end..], &prefix);
    format!("{}{}{}", prefix, &managed_and_tail[..marker_end], suffix)
}

fn strip_legacy_pi_prompts(text: &str, preceding: &str) -> String {
    let mut cleaned = text.to_owned();
    // Each removed prompt owns at least one line. Bound the scan even if a
    // future range detector accidentally returns a zero-width match.
    for _ in 0..text.lines().count() {
        let Some((start, end)) = legacy_pi_prompt_range(&cleaned, preceding) else {
            break;
        };
        cleaned.replace_range(start..end, "");
    }
    cleaned
}

/// A pasted prompt inside fenced user prose is not an installed prompt.
fn inside_markdown_fence(preceding: &str, prefix: &str) -> bool {
    let mut fence = None;
    for line in preceding.lines().chain(prefix.lines()) {
        let trimmed = line.trim_start_matches(' ');
        if line.len() - trimmed.len() > 3 {
            continue;
        }
        let Some(marker @ (b'`' | b'~')) = trimmed.as_bytes().first().copied() else {
            continue;
        };
        let width = trimmed.bytes().take_while(|byte| *byte == marker).count();
        if width < 3 {
            continue;
        }
        match fence {
            None if marker == b'`' && trimmed[width..].contains('`') => {}
            None => fence = Some((marker, width)),
            Some((open_marker, open_width))
                if marker == open_marker
                    && width >= open_width
                    && trimmed[width..].trim().is_empty() =>
            {
                fence = None;
            }
            Some(_) => {}
        }
    }
    fence.is_some()
}

/// Replace `path` with `content` in one step: the bytes already there are
/// backed up first, the new bytes go to a sibling temp file, and that file is
/// renamed over the target. A crash mid-write leaves the old profile intact
/// instead of a half-written one — a shell profile is read by every
/// interactive shell, and it is the user's file.
pub(crate) fn write_atomically(path: &Path, content: &str) -> Result<()> {
    config::backup_if_changing(path, content.as_bytes())?;
    let tmp = path.with_extension("pixel-tmp");
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Which shell dialect the managed wrapper block is written in.
///
/// fish is not a POSIX shell: `claude() { ...; }` is a syntax error there and
/// `$@` does not exist, so supporting it means generating a different block —
/// not just writing the same block somewhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// bash, zsh, and anything else that accepts POSIX function syntax.
    Posix,
    /// fish.
    Fish,
}

impl ShellKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ShellKind::Posix => "posix",
            ShellKind::Fish => "fish",
        }
    }
}

/// File name of the fish drop-in. pixel creates this file, owns all of it, and
/// deletes it on uninstall.
pub(crate) const FISH_DROPIN: &str = "pixel.fish";

/// The shell whose profile is checked for legacy wrappers: the caller's
/// override, else the
/// account's login shell, else `$SHELL`.
///
/// The wrappers are a `claude` function a human runs from an interactive
/// shell, so the shell that matters is the login shell. `$SHELL` is not a
/// reliable witness of it: a coding agent's command tool (Claude Code's
/// runs under `/bin/zsh` on a fish machine), `env -i` or cron report their
/// own. The account database is asked first; `$SHELL` is the fallback when
/// it cannot be read, and the override is for the case where both are
/// wrong.
pub(crate) fn resolve_shell(shell_override: Option<&str>) -> String {
    resolve_shell_from(
        shell_override,
        account_login_shell(),
        std::env::var("SHELL").ok(),
    )
}

/// The resolution order behind [`resolve_shell`], with every source passed
/// in. An empty source counts as absent.
pub(crate) fn resolve_shell_from(
    shell_override: Option<&str>,
    account: Option<String>,
    env_shell: Option<String>,
) -> String {
    let present = |s: Option<String>| s.filter(|v| !v.trim().is_empty());
    shell_override
        .map(ToString::to_string)
        .or_else(|| present(account))
        .or_else(|| present(env_shell))
        .unwrap_or_default()
}

/// The login shell recorded for the current account: Directory Services on
/// macOS (`dscl . -read /Users/<user> UserShell`), the passwd database
/// elsewhere (`getent passwd <user>`, then `/etc/passwd`). `None` when the
/// user name is unknown or nothing answers.
#[cfg_attr(test, mutants::skip)] // process spawns and /etc reads over the tested parsers; reason: mutation cannot affect observable contract, all inner parsing helpers are tested
fn account_login_shell() -> Option<String> {
    let user = std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| {
            let out = std::process::Command::new("id").arg("-un").output().ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
                .filter(|u| !u.is_empty())
        })?;
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("dscl")
            .args([".", "-read", &format!("/Users/{user}"), "UserShell"])
            .output()
            .ok()?;
        return out
            .status
            .success()
            .then(|| parse_dscl_user_shell(&String::from_utf8_lossy(&out.stdout)))
            .flatten();
    }
    let getent = std::process::Command::new("getent")
        .args(["passwd", &user])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned());
    let passwd = match getent {
        Some(text) => text,
        None => fs::read_to_string("/etc/passwd").ok()?,
    };
    parse_passwd_shell(&passwd, &user)
}

/// The shell in `dscl` output: the value after `UserShell:`.
pub(crate) fn parse_dscl_user_shell(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("UserShell:"))
        .map(str::trim)
        .filter(|shell| !shell.is_empty())
        .map(ToString::to_string)
}

/// The shell of `user` in passwd text: the seventh field of the line whose
/// first field is exactly `user`.
pub(crate) fn parse_passwd_shell(passwd: &str, user: &str) -> Option<String> {
    passwd
        .lines()
        .map(|line| line.split(':').collect::<Vec<_>>())
        .find(|fields| fields.first() == Some(&user))
        .and_then(|fields| fields.get(6).map(|s| s.trim().to_string()))
        .filter(|shell| !shell.is_empty())
}

/// Classify a `$SHELL` value by its executable name. Matching the file name
/// rather than a substring of the whole path keeps a path like
/// `/home/fisher/bin/zsh` out of the fish branch.
pub(crate) fn shell_kind_from(shell: &str) -> ShellKind {
    let name = shell.rsplit('/').next().unwrap_or(shell);
    if name.eq_ignore_ascii_case("fish") {
        ShellKind::Fish
    } else {
        ShellKind::Posix
    }
}

/// fish's config root: `$XDG_CONFIG_HOME/fish` when that variable points inside
/// the home being installed into, otherwise fish's default `~/.config/fish`.
/// The containment test keeps an ambient `XDG_CONFIG_HOME` from redirecting an
/// install that was explicitly aimed at another home (`--home`, tests).
pub(crate) fn fish_config_dir(home: &Path) -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let dir = PathBuf::from(&xdg);
        if !xdg.is_empty() && dir.starts_with(home) {
            return dir.join("fish");
        }
    }
    home.join(".config").join("fish")
}

/// Where the managed block lives for `shell`, and which dialect it must be
/// written in: `~/.bashrc` for bash, `~/.config/fish/conf.d/pixel.fish` for
/// fish, `~/.zshrc` otherwise.
///
/// fish reads neither `~/.zshrc` nor `~/.bashrc`. Its `conf.d/` directory is
/// sourced automatically for every session, so the block gets its own file
/// there rather than being appended to the user's `config.fish`.
pub(crate) fn shell_profile_for(shell: &str, home: &Path) -> (ShellKind, PathBuf) {
    match shell_kind_from(shell) {
        ShellKind::Fish => (
            ShellKind::Fish,
            fish_config_dir(home).join("conf.d").join(FISH_DROPIN),
        ),
        ShellKind::Posix
            if shell
                .rsplit('/')
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case("bash")) =>
        {
            (ShellKind::Posix, home.join(".bashrc"))
        }
        ShellKind::Posix => (ShellKind::Posix, home.join(".zshrc")),
    }
}

/// The profiles of the other shells `pixel install` knows, with their
/// shell name, that hold a pixel-managed block: the residue of an install
/// that targeted the wrong shell (an agent's `$SHELL` on a fish machine).
/// `resolved_profile` is the one the current shell loads and is skipped.
pub(crate) fn stray_wrapper_profiles(
    home: &Path,
    resolved_profile: &Path,
) -> Vec<(&'static str, PathBuf)> {
    let candidates = [
        ("zsh", home.join(".zshrc")),
        ("bash", home.join(".bashrc")),
        (
            "fish",
            fish_config_dir(home).join("conf.d").join(FISH_DROPIN),
        ),
    ];
    candidates
        .into_iter()
        .filter(|(_, profile)| profile != resolved_profile)
        .filter(|(_, profile)| {
            fs::read_to_string(profile).is_ok_and(|text| extract_managed_block(&text).is_some())
        })
        .collect()
}

pub(crate) const PIXEL_MANAGED_BEGIN: &str = "# >>> pixel-managed >>>";
pub(crate) const PIXEL_MANAGED_END: &str = "# <<< pixel-managed <<<";

/// Return the pixel-managed block found in `content`, markers included.
pub(crate) fn extract_managed_block(content: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    let mut in_block = false;
    for line in content.lines() {
        if line.trim_start().starts_with(PIXEL_MANAGED_BEGIN) {
            in_block = true;
        }
        if in_block {
            out.push(line.trim_end());
            if line.trim_start().starts_with(PIXEL_MANAGED_END) {
                return Some(out.join("\n"));
            }
        }
    }
    None
}

/// Strip an existing pixel-managed block from a file's content.
///
/// An unterminated block is refused instead of swallowing the rest of the
/// file: the caller reports it and writes nothing. "Everything from the begin
/// marker to EOF is ours" is what used to delete the end of a user's profile.
fn strip_shell_wrappers(content: &str) -> Result<String> {
    let begin = PIXEL_MANAGED_BEGIN;
    let end = PIXEL_MANAGED_END;
    let mut out = String::new();
    let mut skipping = false;
    for line in content.lines() {
        if line.trim_start().starts_with(begin) {
            skipping = true;
            continue;
        }
        if skipping && line.trim_start().starts_with(end) {
            skipping = false;
            continue;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    if skipping {
        return Err(InstallError::UnterminatedManagedBlock);
    }
    // Remove trailing blank lines left by the stripped block.
    while out.ends_with("\n\n") {
        out.pop();
    }
    Ok(out)
}

/// Remove the legacy `claude()` shell-wrapper block — from the resolved
/// shell's profile AND from any other candidate profile an earlier install
/// wrote to. The wrapper only ever fired for human login shells; now that
/// the SessionStart hook injects the prompt into every Claude process, a
/// surviving wrapper double-injects on every wrapped launch.
fn remove_legacy_wrappers(
    home: &Path,
    shell_override: Option<&str>,
    dry_run: bool,
) -> Result<InstallStep> {
    let shell = resolve_shell(shell_override);
    let (_, resolved) = shell_profile_for(&shell, home);
    let mut candidates: Vec<(ShellKind, PathBuf)> =
        vec![(shell_kind_from(&shell), resolved.clone())];
    for (_, path) in stray_wrapper_profiles(home, &resolved) {
        let kind = if path.file_name().is_some_and(|n| n == FISH_DROPIN) {
            ShellKind::Fish
        } else {
            ShellKind::Posix
        };
        candidates.push((kind, path));
    }
    let mut removed: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for (kind, profile) in &candidates {
        let Ok(existing) = fs::read_to_string(profile) else {
            continue;
        };
        let cleaned = match strip_shell_wrappers(&existing) {
            Ok(cleaned) => cleaned,
            Err(e) => {
                return Ok(InstallStep {
                    id: "shell-wrappers".into(),
                    status: CheckStatus::Red,
                    summary: format!("{} in {} — profile not touched", e, profile.display()),
                    detail: Some(format!("profile={}", profile.display())),
                });
            }
        };
        if cleaned == existing {
            skipped.push(profile.display().to_string());
            continue;
        }
        removed.push(profile.display().to_string());
        if dry_run {
            continue;
        }
        if *kind == ShellKind::Fish && cleaned.trim().is_empty() {
            let _ = config::backup_if_changing(profile, cleaned.as_bytes())?;
            fs::remove_file(profile)?;
        } else {
            write_atomically(profile, &cleaned)?;
        }
    }
    Ok(InstallStep {
        id: "shell-wrappers".into(),
        status: CheckStatus::Green,
        summary: dry_run_summary(
            dry_run,
            &if removed.is_empty() {
                "no legacy shell wrappers found — nothing to remove".to_string()
            } else {
                format!("removed legacy shell wrappers from {}", removed.join(", "))
            },
        ),
        detail: Some(format!("removed={removed:?} clean={skipped:?}")),
    })
}

/// Remove the pixel-managed shell wrapper block from the user's shell profile.
pub(crate) fn remove_shell_wrappers(
    home: &Path,
    shell_override: Option<&str>,
    dry_run: bool,
) -> Result<InstallStep> {
    let shell = resolve_shell(shell_override);
    let (kind, profile) = shell_profile_for(&shell, home);
    let detail = Some(format!(
        "profile={} shell={}",
        profile.display(),
        kind.as_str()
    ));
    let existing = match fs::read_to_string(&profile) {
        Ok(s) => s,
        Err(_) => {
            return Ok(InstallStep {
                id: "shell-wrappers".into(),
                status: CheckStatus::Green,
                summary: dry_run_summary(dry_run, "no shell profile — skipping"),
                detail,
            });
        }
    };
    let cleaned = match strip_shell_wrappers(&existing) {
        Ok(cleaned) => cleaned,
        Err(e) => {
            return Ok(InstallStep {
                id: "shell-wrappers".into(),
                status: CheckStatus::Red,
                summary: format!("{e} — profile not touched"),
                detail,
            });
        }
    };
    if cleaned == existing {
        return Ok(InstallStep {
            id: "shell-wrappers".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "no shell wrappers found — skipping"),
            detail,
        });
    }
    if !dry_run {
        // The fish drop-in is a file pixel created and owns end to end: once
        // the block is stripped there is nothing left in it worth keeping. A
        // user's own .zshrc/.bashrc is only ever edited in place.
        if kind == ShellKind::Fish && cleaned.trim().is_empty() {
            let _ = config::backup_if_changing(&profile, cleaned.as_bytes())?;
            fs::remove_file(&profile)?;
        } else {
            write_atomically(&profile, &cleaned)?;
        }
    }
    Ok(InstallStep {
        id: "shell-wrappers".into(),
        status: CheckStatus::Green,
        summary: dry_run_summary(dry_run, "removed shell wrappers"),
        detail,
    })
}

/// Cursor's flat hook schema (`hooks.<event>` is a plain array of
/// `{command, matcher?}`): the guard goes on `preToolUse` — Cursor's payload
/// carries no `hook_event_name`, so the guard treats it as an implicit
/// PreToolUse — and the metrics relay on `postToolUse`, which answers with
/// the Cursor-native `additional_context` field.
fn install_cursor_hooks(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let path = home.join(crate::config::CURSOR_HOOKS_FILE);
    // Validate the existing settings in both modes so a dry run honestly
    // reports a red step when the file is unreadable or malformed; only the
    // write (and backup/dir creation) is suppressed during a dry run.
    let mut value = read_settings(&path)?;
    let root = value
        .as_object_mut()
        .ok_or_else(|| crate::InstallError::InvalidSettings {
            path: path.clone(),
            reason: "hooks.json root is not an object".into(),
        })?;
    // Cursor documents `version` as required. A fresh install (read_settings
    // returned `{}`) must carry it, and an existing value is preserved.
    if root.get("version").is_none() {
        root.insert("version".into(), serde_json::json!(1));
    }
    let hooks = root.entry("hooks").or_insert_with(|| serde_json::json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| crate::InstallError::InvalidSettings {
            path: path.clone(),
            reason: "hooks is not an object".into(),
        })?;
    // Pixel owns the entry whose command runs through this executable; the
    // marker is the executable itself, never the bare `run-hook guard` or
    // `run-hook metrics` phrase, so an unrelated command that merely mentions
    // either phrase is preserved (and replaced only if pixel really wrote it).
    let exe_marker = crate::routing::quoted_executable(exe);
    let guard = serde_json::json!({
        "command": format!("{exe_marker} run-hook guard --provider cursor"),
        "matcher": crate::config::GUARD_MATCHER,
    });
    let metrics = serde_json::json!({
        "command": format!("{exe_marker} run-hook metrics --provider cursor"),
    });
    let pre = config::merge_flat_hook_entry(hooks.get("preToolUse"), &exe_marker, guard);
    hooks.insert("preToolUse".into(), pre);
    let post = config::merge_flat_hook_entry(hooks.get("postToolUse"), &exe_marker, metrics);
    hooks.insert("postToolUse".into(), post);
    let backup = write_settings(&path, &value, dry_run)?;
    let summary = if dry_run {
        format!(
            "would configure pixel guard + metrics in {}",
            path.display()
        )
    } else {
        "configured pixel guard + metrics in Cursor hooks.json".into()
    };
    let detail = if dry_run {
        None
    } else {
        Some(with_backup_note(format!("path={}", path.display()), backup))
    };
    Ok(InstallStep {
        id: "hooks.cursor".into(),
        status: CheckStatus::Green,
        summary,
        detail,
    })
}

pub(crate) fn read_settings(path: &Path) -> Result<serde_json::Value> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(serde_json::from_str(&s)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(e.into()),
    }
}

/// Serialize and write `value` to `path`, backing up any pre-existing,
/// content-differing file first. In dry-run mode, performs no write, no
/// backup, and no directory creation, and always returns `Ok(None)`.
pub(crate) fn write_settings(
    path: &Path,
    value: &serde_json::Value,
    dry_run: bool,
) -> Result<Option<PathBuf>> {
    let serialized = format!("{}\n", serde_json::to_string_pretty(value)?);
    if dry_run {
        return Ok(None);
    }
    // A settings path that is a symlink into a managed dotfile must stay a
    // symlink: write and rename the sibling temp onto the link's *target*, so
    // the host keeps pointing at the managed file instead of being disconnected.
    let resolved = resolve_symlink_target(path);
    let target: &Path = resolved.as_deref().unwrap_or(path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let backup_path = config::backup_if_changing(target, serialized.as_bytes())?;
    // A live hooks file (Cursor's above all) is read by a concurrently
    // running host; write to a uniquely named sibling temp and atomically
    // rename so a mid-write reader never sees empty or partial JSON, and two
    // concurrent writers never share (and truncate each other's) temp file.
    let tmp = unique_temp_path(target);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp)?;
    file.write_all(serialized.as_bytes())?;
    // Retain an existing settings file's permission bits (a 0600 credentials
    // file must stay 0600); a brand-new file is already private because the
    // temp was opened 0600 regardless of the ambient umask.
    #[cfg(unix)]
    if let Ok(meta) = fs::metadata(target) {
        fs::set_permissions(&tmp, meta.permissions())?;
    }
    fs::rename(&tmp, target)?;
    Ok(backup_path)
}

/// The real file a settings path points at if it is a symlink, else `None`.
fn resolve_symlink_target(path: &Path) -> Option<PathBuf> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_symlink() {
        return None;
    }
    let target = fs::read_link(path).ok()?;
    Some(if target.is_absolute() {
        target
    } else {
        path.parent().unwrap_or(Path::new("")).join(target)
    })
}

/// A sibling temp file name unique to this process so concurrent writers of
/// the same settings path can never collide on one shared temp file.
fn unique_temp_path(path: &Path) -> PathBuf {
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map_or_else(|| "settings".into(), |n| n.to_string_lossy().into_owned());
    let tmp_name = format!("{name}.pixel-tmp-{nanos}-{seq}");
    match path.parent() {
        Some(parent) => parent.join(tmp_name),
        None => PathBuf::from(tmp_name),
    }
}

pub(crate) fn dry_run_summary(dry_run: bool, summary: &str) -> String {
    if dry_run {
        format!("[dry-run] would report: {summary}")
    } else {
        summary.to_string()
    }
}

pub(crate) fn with_backup_note(detail: String, backup_path: Option<PathBuf>) -> String {
    match backup_path {
        Some(p) => format!("{detail} (backup={})", p.display()),
        None => detail,
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
    /// Compatibility field: false because this command does not rebuild indexes.
    pub new_state_rebuilt: bool,
    /// True once `.pixel/` exists; existing contents are preserved.
    pub new_state_directory_prepared: bool,
}

/// Remove legacy `.gitpixel/` state and prepare the current `.pixel/` directory.
///
/// Existing `.pixel/` contents are preserved. Missing indexes are built lazily
/// on first use, not by this command. (The old gain-ledger
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

    // Preparing a directory is not proof that any index has been rebuilt.
    fs::create_dir_all(&new_dir)?;

    Ok(MigrateReport {
        version: "v1".into(),
        ok: true,
        repo_root: repo_root.display().to_string(),
        old_state_removed,
        new_state_rebuilt: false,
        new_state_directory_prepared: true,
    })
}

#[cfg(test)]
mod pi_prompt_io_tests {
    use super::{CheckStatus, InstallOptions, PI_PROMPT_REL, install};
    use crate::InstallError;
    use std::fs;
    use std::io::ErrorKind;

    #[cfg(unix)]
    #[test]
    fn install_should_propagate_pi_prompt_read_errors_without_touching_prompt_directory() {
        let home = tempfile::tempdir().unwrap();
        let prompt_path = home.path().join(PI_PROMPT_REL);
        fs::create_dir_all(&prompt_path).unwrap();
        let sentinel = prompt_path.join("keep.txt");
        fs::write(&sentinel, "user-owned Pi data").unwrap();

        let error = install(&InstallOptions {
            home: Some(home.path().to_path_buf()),
            ..Default::default()
        })
        .expect_err("a directory at the prompt path is an I/O error, not an absent file");

        assert!(
            matches!(&error, InstallError::Io(error) if error.kind() == ErrorKind::IsADirectory),
            "the prompt read error must propagate: {error:?}"
        );
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "user-owned Pi data");
        assert!(prompt_path.is_dir());
        assert!(
            !home
                .path()
                .join(crate::routing::CLAUDE_SHARED_SETTINGS)
                .exists(),
            "install must stop before writing later provider hooks"
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_should_skip_pi_prompt_cleanup_when_agent_parent_is_a_file() {
        let home = tempfile::tempdir().unwrap();
        let agent_path = home.path().join(".pi/agent");
        fs::create_dir_all(agent_path.parent().unwrap()).unwrap();
        fs::write(&agent_path, "user-owned file").unwrap();

        let report = install(&InstallOptions {
            home: Some(home.path().to_path_buf()),
            ..Default::default()
        })
        .expect("a non-directory Pi agent path contains no retired prompt to clean");

        assert_eq!(fs::read_to_string(&agent_path).unwrap(), "user-owned file");
        let pi_step = report
            .steps
            .iter()
            .find(|step| step.id == "hooks.pi-impact")
            .expect("Pi status is reported");
        assert_eq!(pi_step.status, CheckStatus::Yellow);
        assert!(pi_step.summary.contains("not a directory"));
        assert!(
            !home
                .path()
                .join(".pi/agent/extensions/pixel-impact.ts")
                .exists()
        );
    }
}

#[cfg(test)]
mod stale_prompt_tests {
    use super::*;

    fn deploy(home: &Path, name: &str, content: &str) {
        let dir = home.join(".local/share/pixel");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn nothing_deployed_is_not_stale() {
        let home = tempfile::tempdir().unwrap();
        assert!(stale_prompts(home.path()).is_empty());
    }

    #[test]
    fn prompts_matching_this_binary_are_not_stale() {
        let home = tempfile::tempdir().unwrap();
        deploy(home.path(), "agent-prompt.md", AGENT_PROMPT_ASSET);
        deploy(home.path(), SUBAGENT_PROMPT_FILE, SUBAGENT_PROMPT_ASSET);
        assert!(stale_prompts(home.path()).is_empty());
    }

    /// An older release's prompt, the case an upgrade leaves behind: each
    /// file is judged on its own, against its own bundled copy.
    #[test]
    fn a_prompt_from_another_release_is_named() {
        let home = tempfile::tempdir().unwrap();
        deploy(home.path(), "agent-prompt.md", "# Pixel, an older prompt\n");
        deploy(home.path(), SUBAGENT_PROMPT_FILE, SUBAGENT_PROMPT_ASSET);
        assert_eq!(stale_prompts(home.path()), vec!["agent-prompt.md"]);
        deploy(home.path(), SUBAGENT_PROMPT_FILE, AGENT_PROMPT_ASSET);
        assert_eq!(
            stale_prompts(home.path()),
            vec!["agent-prompt.md", SUBAGENT_PROMPT_FILE]
        );
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn migrate_preserves_current_state_and_reports_no_rebuild() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        fs::create_dir_all(root.join(".pixel")).unwrap();
        fs::create_dir_all(root.join(".gitpixel")).unwrap();
        let state = root.join(".pixel/user-state.json");
        fs::write(&state, b"{\"preserve\":true}").unwrap();
        let first = migrate(root).unwrap();
        assert!(first.old_state_removed);
        assert!(first.new_state_directory_prepared);
        assert!(
            !first.new_state_rebuilt,
            "preparing a directory is not rebuilding an index"
        );
        assert_eq!(fs::read(&state).unwrap(), b"{\"preserve\":true}");
        let second = migrate(root).unwrap();
        assert!(!second.old_state_removed);
        assert!(second.new_state_directory_prepared);
        assert!(!second.new_state_rebuilt);
        assert_eq!(fs::read(&state).unwrap(), b"{\"preserve\":true}");
    }
}

#[cfg(test)]
mod path_tests {
    use super::{find_in_paths, find_on_path};

    #[test]
    fn find_in_paths_returns_the_first_executable_file_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pixel-find-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = dir.join("first");
        let second = dir.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        // `tool` is a plain file in `first` and an executable in `second`.
        std::fs::write(first.join("tool"), b"#!/bin/sh\n").unwrap();
        std::fs::write(second.join("tool"), b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(second.join("tool"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::create_dir_all(first.join("dirtool")).unwrap();
        let path = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(find_in_paths("tool", &path), Some(second.join("tool")));
        assert_eq!(
            find_in_paths("dirtool", &path),
            None,
            "a directory is not a binary"
        );
        assert_eq!(find_in_paths("absent", &path), None);
        let only_first = std::env::join_paths([&first]).unwrap();
        assert_eq!(find_in_paths("tool", &only_first), None, "not executable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_on_path_reads_the_process_path() {
        let sh = find_on_path("sh").expect("sh is on every unix PATH");
        assert!(sh.ends_with("sh"), "{}", sh.display());
        assert!(sh.is_absolute(), "{}", sh.display());
        assert_eq!(find_on_path("pixel-definitely-not-installed-xyz"), None);
    }
}

#[cfg(test)]
mod stable_exe_path_tests {
    use super::stable_exe_path_in;
    use std::path::PathBuf;

    #[cfg(unix)]
    #[test]
    fn a_cellar_binary_resolves_to_its_stable_path_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("Cellar/pixel/9.9.9/bin");
        let bin = dir.path().join("homebrew/bin");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let real = store.join("pixel");
        std::fs::write(&real, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = bin.join("pixel");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let paths = std::env::join_paths([&bin]).unwrap();
        assert_eq!(
            stable_exe_path_in(real, &paths),
            link,
            "hook commands must name the surviving symlink, not the store path"
        );
    }

    #[test]
    fn an_absent_path_match_keeps_the_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("pixel");
        std::fs::write(&exe, b"x").unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let paths = std::env::join_paths([elsewhere.path()]).unwrap();
        assert_eq!(
            stable_exe_path_in(exe.clone(), &paths),
            exe.canonicalize().unwrap()
        );
    }

    #[test]
    fn a_path_hit_for_a_different_binary_is_not_substituted() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("mine").join("pixel");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"real").unwrap();
        let other_dir = tempfile::tempdir().unwrap();
        let impostor = other_dir.path().join("pixel");
        std::fs::write(&impostor, b"other").unwrap();
        let paths = std::env::join_paths([other_dir.path()]).unwrap();
        assert_eq!(
            stable_exe_path_in(exe.clone(), &paths),
            exe.canonicalize().unwrap(),
            "an unrelated PATH binary must not be written into hooks"
        );
    }

    #[test]
    fn a_nameless_exe_falls_back_to_canonical() {
        assert_eq!(
            stable_exe_path_in(PathBuf::new(), std::ffi::OsStr::new("")),
            PathBuf::new()
        );
    }
}

#[cfg(test)]
mod pi_prompt_content_tests;

#[cfg(test)]
mod shell_resolution_tests;

#[cfg(test)]
mod shell_wrapper_strip_tests;

#[cfg(test)]
mod cursor_hooks_tests {
    use super::{install_cursor_hooks, read_settings};

    fn cursor_command(exe: &std::path::Path, verb: &str) -> String {
        format!("{} run-hook {verb}", crate::routing::quoted_executable(exe))
    }

    fn commands_for(value: &serde_json::Value, event: &str) -> Vec<String> {
        value["hooks"][event]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|entry| entry["command"].as_str())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn fresh_install_writes_version_and_both_flat_hook_commands() {
        let home = tempfile::tempdir().unwrap();
        let exe = std::path::Path::new("/opt/pixel");
        let step = install_cursor_hooks(home.path(), exe, false).unwrap();
        assert!(step.summary.contains("configured"), "{}", step.summary);

        let path = home.path().join(crate::config::CURSOR_HOOKS_FILE);
        let value = read_settings(&path).unwrap();
        // Cursor documents `version` as required; a fresh install must carry it.
        assert_eq!(value["version"], serde_json::json!(1));
        let pre = commands_for(&value, "preToolUse");
        assert_eq!(
            pre,
            vec![cursor_command(exe, "guard --provider cursor")],
            "{pre:?}"
        );
        assert_eq!(
            value["hooks"]["preToolUse"][0]["matcher"],
            serde_json::json!(crate::config::GUARD_MATCHER)
        );
        let post = commands_for(&value, "postToolUse");
        assert_eq!(
            post,
            vec![cursor_command(exe, "metrics --provider cursor")],
            "{post:?}"
        );
    }

    #[test]
    fn reinstall_preserves_version_and_foreign_entries_without_duplication() {
        let home = tempfile::tempdir().unwrap();
        let exe = std::path::Path::new("/opt/pixel");
        let path = home.path().join(crate::config::CURSOR_HOOKS_FILE);
        // A pre-existing Cursor config: a non-1 version plus a foreign hook.
        super::write_settings(
            &path,
            &serde_json::json!({
                "version": 2,
                "hooks": {
                    "preToolUse": [{"command": "notify-send done"}]
                }
            }),
            false,
        )
        .unwrap();

        install_cursor_hooks(home.path(), exe, false).unwrap();
        // Idempotence: a second install must not duplicate pixel's own entry.
        install_cursor_hooks(home.path(), exe, false).unwrap();

        let value = read_settings(&path).unwrap();
        assert_eq!(value["version"], serde_json::json!(2));
        let pre = commands_for(&value, "preToolUse");
        assert_eq!(
            pre,
            vec![
                "notify-send done".to_owned(),
                cursor_command(exe, "guard --provider cursor"),
            ],
            "foreign preToolUse entries survive and pixel's stays singular: {pre:?}"
        );
    }

    /// A foreign command that merely *mentions* `run-hook guard` (without
    /// running through pixel's executable) must survive an install untouched,
    /// while the actual pixel-owned entry is replaced and never duplicated.
    #[test]
    fn install_preserves_foreign_commands_that_mention_run_hook() {
        let home = tempfile::tempdir().unwrap();
        let exe = std::path::Path::new("/opt/pixel");
        let path = home.path().join(crate::config::CURSOR_HOOKS_FILE);
        super::write_settings(
            &path,
            &serde_json::json!({
                "hooks": {
                    "preToolUse": [
                        {"command": "some-tool run-hook guard --for-everyone"},
                        {"command": cursor_command(exe, "guard --provider cursor")}
                    ]
                }
            }),
            false,
        )
        .unwrap();

        install_cursor_hooks(home.path(), exe, false).unwrap();
        let value = read_settings(&path).unwrap();
        let pre = commands_for(&value, "preToolUse");
        assert_eq!(
            pre,
            vec![
                "some-tool run-hook guard --for-everyone".to_owned(),
                cursor_command(exe, "guard --provider cursor"),
            ],
            "a foreign command mentioning 'run-hook guard' survives; the \
             pixel entry is replaced, not duplicated: {pre:?}"
        );
    }

    #[test]
    fn dry_run_reports_planned_changes_without_writing() {
        let home = tempfile::tempdir().unwrap();
        let exe = std::path::Path::new("/opt/pixel");
        let step = install_cursor_hooks(home.path(), exe, true).unwrap();
        assert!(step.summary.contains("would configure"), "{}", step.summary);
        assert!(
            !home.path().join(crate::config::CURSOR_HOOKS_FILE).exists(),
            "dry-run must not create the hooks file"
        );
    }

    /// A settings path that is a symlink into a dotfiles manager must stay a
    /// symlink: the write updates the managed target, never replaces the link
    /// (this is what `resolve_symlink_target` exists for).
    #[cfg(unix)]
    #[test]
    fn write_settings_updates_a_symlink_target_and_keeps_the_link() {
        use super::write_settings;
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("dotfiles");
        std::fs::create_dir_all(&store).unwrap();
        let managed = store.join("hooks.json");
        std::fs::write(&managed, b"old").unwrap();
        let path = dir.path().join("settings").join("hooks.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&managed, &path).unwrap();
        write_settings(&path, &serde_json::json!({"a": 1}), false).unwrap();
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink survives the write"
        );
        assert_eq!(
            std::fs::read(&managed).unwrap(),
            br#"{
  "a": 1
}
"#,
            "the managed target, not the link, holds the new bytes"
        );
        // A broken symlink resolves too: the write lands on the target path.
        let missing = dir.path().join("absent").join("hooks.json");
        let broken = dir.path().join("settings").join("broken.json");
        std::os::unix::fs::symlink(&missing, &broken).unwrap();
        write_settings(&broken, &serde_json::json!({"b": 2}), false).unwrap();
        assert_eq!(
            std::fs::read(&missing).unwrap(),
            br#"{
  "b": 2
}
"#
        );
        assert!(
            std::fs::symlink_metadata(&broken)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    /// The temp a settings write creates is private and unique, and the
    /// replacement keeps an existing 0600 file private (the symlink test
    /// pins the link; this one pins mode + no stray temp + backup).
    #[cfg(unix)]
    #[test]
    fn write_settings_is_private_and_leaves_only_live_file_and_backup() {
        use super::write_settings;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_settings(&path, &serde_json::json!({"a": 1}), false).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the replacement keeps the live file's mode");
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left.len(), 2, "only live file + backup remain: {left:?}");
        assert!(
            left.iter()
                .any(|name| name.starts_with("hooks.json.pixel-bak.")),
            "the pre-image was backed up: {left:?}"
        );
    }
}

#[cfg(test)]
mod report_tests {
    use super::*;

    fn step(id: &str, status: CheckStatus) -> InstallStep {
        InstallStep {
            id: id.into(),
            status,
            summary: String::new(),
            detail: None,
        }
    }

    #[test]
    fn install_report_counts_each_status_and_fails_only_on_red() {
        let passing = install_report(
            Path::new("/bin/pixel"),
            Path::new("/home/u"),
            true,
            vec![
                step("a", CheckStatus::Green),
                step("b", CheckStatus::Green),
                step("c", CheckStatus::Yellow),
            ],
        );
        assert!(passing.ok);
        assert!(passing.dry_run);
        assert_eq!(
            (
                passing.summary.green,
                passing.summary.yellow,
                passing.summary.red
            ),
            (2, 1, 0)
        );
        assert_eq!(passing.executable_path, "/bin/pixel");
        assert_eq!(passing.home, "/home/u");
        assert_eq!(passing.steps.len(), 3);

        let failing = install_report(
            Path::new("/bin/pixel"),
            Path::new("/repo"),
            false,
            vec![step("a", CheckStatus::Red)],
        );
        assert!(!failing.ok);
        assert_eq!(
            (
                failing.summary.green,
                failing.summary.yellow,
                failing.summary.red
            ),
            (0, 0, 1)
        );
    }
}
