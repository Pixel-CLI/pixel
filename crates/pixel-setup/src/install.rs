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
use crate::setup::AgentTarget;

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
    /// If true, compute and report every step's outcome exactly as a real
    /// run would, but perform no filesystem writes: no settings.json edits,
    /// no hook files, no agent-config rewrites, no backups, no directory
    /// creation. Safe to run against a real `$HOME` to preview an install.
    pub dry_run: bool,
    /// Repository root of a `pixel setup --repo` run. Every global step is
    /// skipped and nothing is written under it: pixel keeps no repo-local
    /// hooks.
    pub repo: Option<PathBuf>,
}

/// What a guided `pixel setup` run asked the installer to deploy; see
/// [`install_selected`].
///
/// An agent that is not selected is not touched; a selected agent gets the hook group its feature names, and only that.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallSelection {
    /// The agents selected.
    pub agents: Vec<AgentTarget>,
    /// Register the task-event hooks of Claude Code, Devin, Gemini CLI,
    /// Antigravity and OpenCode, including the Claude and Antigravity metrics relays.
    pub task_hooks: bool,
    /// Register Codex's task hooks.
    pub codex_hooks: bool,
    /// Install the pi impact extension.
    pub pi_extension: bool,
}

impl InstallSelection {
    /// Whether `agent` was selected.
    fn selected(&self, agent: AgentTarget) -> bool {
        self.agents.contains(&agent)
    }
}

/// Which steps of an install run apply: everything when no selection was
/// made, else only what the selection names.
#[derive(Clone, Copy)]
struct Wants<'a>(Option<&'a InstallSelection>);

impl Wants<'_> {
    /// The task-event hooks of `agent`.
    fn task_hooks(self, agent: AgentTarget) -> bool {
        self.0.is_none_or(|s| s.selected(agent) && s.task_hooks)
    }
    /// Codex's task hooks.
    fn codex_hooks(self) -> bool {
        self.0
            .is_none_or(|s| s.selected(AgentTarget::Codex) && s.codex_hooks)
    }
    /// The pi impact extension.
    fn pi_extension(self) -> bool {
        self.0
            .is_none_or(|s| s.selected(AgentTarget::Pi) && s.pi_extension)
    }
}

/// First executable file named `name` on PATH.
#[cfg(test)]
pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    find_in_paths(name, &std::env::var_os("PATH")?)
}

/// First executable file named `name` in the PATH-style list `path`.
pub(crate) fn find_in_paths(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
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

/// Run `pixel setup`. Idempotent: safe to re-run.
///
/// Global install deploys the `pixel` skill and keeps task-lifecycle
/// accounting hooks for configured hosts. It installs Pi's explicit impact
/// command only when the standard `~/.pi/agent` configuration directory
/// already exists; a machine without Pi is left untouched.
pub fn install(options: &InstallOptions) -> Result<InstallReport> {
    install_selected(options, None)
}

/// [`install`], limited to what a guided `pixel setup` run picked.
///
/// `None` is the unattended install: every configured host. With a selection,
/// an agent that was not picked is not touched, and a picked agent gets the
/// hook group its feature names, and only that. `options.dry_run` makes it the
/// preview the guided review prints.
pub fn install_selected(
    options: &InstallOptions,
    selection: Option<&InstallSelection>,
) -> Result<InstallReport> {
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
    let wants = Wants(selection);
    if let Some(repo) = &options.repo {
        return Ok(install_report(&exe, repo, dry_run, Vec::new()));
    }
    // How many hooks carry the prompt brief: a host caps what each injects.
    let prompt_parts = crate::config::brief_parts(&home, None);
    // The pixel skill is the retrieval workflow every harness reads as a
    // skill: a guided run writes the selected agents' roots, the unattended
    // install every configured one.
    let mut steps = vec![crate::skill::install(&home, selection, dry_run)?];
    if wants.pi_extension() {
        steps.push(crate::pi_global::install(
            &crate::pi_global::PiPaths::resolve(&home, options.home.is_some()),
            &exe,
            options.home.is_none() && crate::pi_global::pi_on_path(),
            dry_run,
        )?);
    }
    // Keep task accounting, configured task gates and metrics available to
    // every Claude session.
    if wants.task_hooks(AgentTarget::ClaudeCode) {
        steps.push(crate::routing::install_task_events(
            &crate::routing::Provider::Claude.path(&home),
            &exe,
            crate::routing::Provider::Claude,
            prompt_parts,
            dry_run,
        )?);
    }
    if wants.codex_hooks() {
        steps.push(crate::codex_config::install_task_hooks(
            &codex_home,
            &exe,
            prompt_parts,
            dry_run,
        )?);
    }
    // The first-party brief probes the prompt-submit brief runs: with Claude's
    // or Codex's brief hook, the probes that make it complete come along.
    if wants.task_hooks(AgentTarget::ClaudeCode)
        || wants.codex_hooks()
        || wants.task_hooks(AgentTarget::Gemini)
        || wants.task_hooks(AgentTarget::Devin)
        || wants.task_hooks(AgentTarget::OpenCode)
        || wants.task_hooks(AgentTarget::Antigravity)
        || wants.pi_extension()
    {
        steps.push(crate::bundled_probes::install(&home, dry_run)?);
    }
    // The Gemini CLI brief hook: BeforeAgent in ~/.gemini/settings.json,
    // only where the file already exists.
    if wants.task_hooks(AgentTarget::Gemini) {
        steps.push(crate::antigravity::install_gemini_brief(
            &home, &exe, dry_run,
        )?);
    }
    // Every other host keeps its native retrieval. Each step runs only where
    // that host's configuration exists, so a machine without it is left
    // untouched.
    let opencode_dir = crate::opencode_config::opencode_config_dir(&home, options.home.is_some());
    if opencode_dir.is_dir() && wants.task_hooks(AgentTarget::OpenCode) {
        steps.push(crate::opencode_config::install_brief(
            &opencode_dir,
            &exe,
            dry_run,
        )?);
    }
    // Devin shares Claude's hook schema and reads every configured hooks
    // file; its task events include prompt-submit, which carries the brief.
    if home.join(crate::config::DEVIN_CONFIG_DIR).is_dir() && wants.task_hooks(AgentTarget::Devin) {
        steps.push(crate::routing::install_task_events(
            &crate::routing::Provider::Devin.path(&home),
            &exe,
            crate::routing::Provider::Devin,
            1,
            dry_run,
        )?);
    }
    if crate::antigravity::antigravity_config_dir(&home).is_dir()
        && wants.task_hooks(AgentTarget::Antigravity)
    {
        steps.push(crate::antigravity::install_antigravity_brief(
            &home, &exe, dry_run,
        )?);
        steps.push(crate::antigravity::install_antigravity_plugin(
            &home, &exe, dry_run,
        )?);
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

/// The dialect of a login shell: fish answers `which`, anything else
/// `command -v`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// bash, zsh, and anything else that accepts POSIX function syntax.
    Posix,
    /// fish.
    Fish,
}

/// The shell whose `PATH` the doctor checks: the caller's override, else the
/// account's login shell, else `$SHELL`.
///
/// The shell that matters is the login shell. `$SHELL` is not a
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

pub(crate) fn read_settings(path: &Path) -> Result<serde_json::Value> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(serde_json::from_str(&s)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(e.into()),
    }
}

/// Edit changed JSON keys in `path`, backing up any pre-existing,
/// content-differing file first. In dry-run mode, performs no write, no
/// backup, and no directory creation, and always returns `Ok(None)`.
pub(crate) fn write_settings(
    path: &Path,
    value: &serde_json::Value,
    dry_run: bool,
) -> Result<Option<PathBuf>> {
    let serialized = match fs::read_to_string(path) {
        Ok(text) if serde_json::from_str::<serde_json::Value>(&text).is_ok() => {
            crate::json_edit::patch(&text, value)?
        }
        Ok(_) => format!("{}\n", serde_json::to_string_pretty(value)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            format!("{}\n", serde_json::to_string_pretty(value)?)
        }
        Err(error) => return Err(error.into()),
    };
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
pub(crate) fn resolve_symlink_target(path: &Path) -> Option<PathBuf> {
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
pub(crate) fn unique_temp_path(path: &Path) -> PathBuf {
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
mod shell_resolution_tests;

#[cfg(test)]
mod settings_write_tests {

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
