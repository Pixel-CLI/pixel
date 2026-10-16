// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel setup --remove` — the inverse of `pixel setup`.
//!
//! Removes every trace pixel setup wrote:
//!   - pixel run-hook entries from the Claude, Devin, Codex and Gemini
//!     settings files
//!   - the OpenCode, Antigravity and Pi integrations
//!   - the `pixel` skill and the first-party brief probes
//!   - the pixel binary (~/.local/bin/pixel by default)
//!
//! Idempotent: safe to re-run. Each step reports what was removed (or that
//! nothing was found). Backups are written before every destructive write,
//! same as install.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::InstallError;
use crate::config;
use crate::install::{self, CheckStatus, InstallReport, InstallStep};
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
    /// If true, compute and report every step's outcome exactly as a real
    /// run would, but perform no filesystem writes.
    pub dry_run: bool,
}

/// Run `pixel setup --remove`. Idempotent: safe to re-run.
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
    let codex_home = crate::codex_config::codex_home(&home, options.home.is_some());
    let opencode_dir = crate::opencode_config::opencode_config_dir(&home, options.home.is_some());
    let pi_paths = crate::pi_global::PiPaths::resolve(&home, options.home.is_some());
    let steps = vec![
        // 1. Remove pixel run-hook entries from every tool's settings file.
        remove_claude_hooks(&home, &exe, dry_run)?,
        remove_devin_hooks(&home, &exe, dry_run)?,
        remove_codex_hooks(&home, &exe, dry_run)?,
        remove_gemini_hooks(&home, &exe, dry_run)?,
        // 2. Remove the skills and the Pi package.
        remove_classify_skill(&home, dry_run)?,
        remove_pixel_skill(&home, dry_run)?,
        crate::pi_global::uninstall(&pi_paths, dry_run)?,
        crate::pi_global::uninstall_classify_package(&pi_paths, &home, dry_run)?,
        // 3. Remove the Antigravity plugin and hooks, and OpenCode's brief plugin.
        crate::antigravity::remove_antigravity(&home, &exe, dry_run)?,
        crate::opencode_config::remove_opencode(&opencode_dir, dry_run)?,
        // 4. Remove the first-party brief probes setup wrote.
        crate::bundled_probes::remove(&home, dry_run)?,
        // 5. Remove the pixel binary.
        remove_binary(&target, dry_run)?,
        // 6. Name the backups install and uninstall kept, and how to drop them.
        backups_step(
            &find_backups(&global_backup_dirs(&home, &codex_home, &opencode_dir)),
            dry_run,
        ),
    ];

    Ok(install::install_report(&binary_path, &home, dry_run, steps))
}

// -------------------------------------------------------------------------
// Step 1: strip managed blocks from agent-config Markdown files
// -------------------------------------------------------------------------

// -------------------------------------------------------------------------
// Step 2: remove Claude hooks (settings.json entries + hook scripts)
// -------------------------------------------------------------------------

fn remove_claude_hooks(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let settings = home.join(".claude").join("settings.json");
    let (removed_entries, backup_path) = remove_pixel_hooks_from_settings(&settings, exe, dry_run)?;
    let summary = format!("removed {removed_entries} Claude hook event(s)");
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

pub(crate) fn remove_devin_hooks(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
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

// -------------------------------------------------------------------------
// Step 3e: remove Cursor hooks (flat schema)
// -------------------------------------------------------------------------

// -------------------------------------------------------------------------
// Step 3f: remove pi guard extension + AGENTS.md managed block
// -------------------------------------------------------------------------

/// Rename the skill `dir` (`<root>/skills/<name>`) to
/// `<root>/<name>.pixel-bak.<nanos>-<seq>` — the directory counterpart of
/// [`config::backup_if_changing`], so a user-edited copy survives an
/// uninstall instead of being deleted outright. The backup leaves `skills/`:
/// a harness loads every `skills/*/SKILL.md`, so a backup beside the skill
/// would keep it active after the uninstall.
fn rename_as_backup(dir: &Path) -> Result<()> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let name = dir
        .file_name()
        .map_or_else(|| "dir".into(), |n| n.to_string_lossy().into_owned());
    let root = dir.parent().and_then(Path::parent).unwrap_or(dir);
    fs::rename(dir, root.join(format!("{name}.pixel-bak.{nanos}-0")))?;
    Ok(())
}

// -------------------------------------------------------------------------
// Step 4b: remove the classify helpers' skill dirs from every harness
// -------------------------------------------------------------------------

/// Remove the `pixel-classify` skill the install-time helpers proposal can
/// write into every configured harness's skills dir — the same roots
/// [`config::SKILL_ROOTS`] installs into, plus Claude's always-present one.
fn remove_classify_skill(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let dirs: Vec<PathBuf> = [PathBuf::from(".claude")]
        .into_iter()
        .chain(config::SKILL_ROOTS.iter().map(PathBuf::from))
        .map(|root| home.join(root).join("skills/pixel-classify"))
        .collect();
    let mut removed = Vec::new();
    for dir in &dirs {
        if dir.is_dir() {
            if !dry_run {
                rename_as_backup(dir)?;
            }
            removed.push(dir.display().to_string());
        }
    }
    Ok(InstallStep {
        id: "classify.skill".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(
            dry_run,
            &if removed.is_empty() {
                "no classify skill found".to_string()
            } else {
                format!("removed {} classify skill dir(s)", removed.len())
            },
        ),
        detail: (!removed.is_empty()).then(|| removed.join(" ")),
    })
}

// -------------------------------------------------------------------------
// Step 4c: remove the pixel skill from every harness's skills dir
// -------------------------------------------------------------------------

/// Remove a `skills/pixel` dir only when its `SKILL.md` is the managed copy
/// the install step wrote — a user-authored skill under the same name stays.
fn remove_pixel_skill(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let mut removed = Vec::new();
    for dir in crate::skill::managed_skill_dirs(home) {
        let file = dir.join("SKILL.md");
        if !fs::read_to_string(&file).is_ok_and(|text| text == crate::skill::PIXEL_SKILL) {
            continue;
        }
        if !dry_run {
            rename_as_backup(&dir)?;
        }
        removed.push(dir.display().to_string());
    }
    Ok(InstallStep {
        id: "skills.pixel".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(
            dry_run,
            &if removed.is_empty() {
                "no pixel skill found".to_string()
            } else {
                format!("removed {} pixel skill dir(s)", removed.len())
            },
        ),
        detail: (!removed.is_empty()).then(|| removed.join(" ")),
    })
}

// -------------------------------------------------------------------------
// Step 4: remove pixel hooks from project-level .codex/hooks.json files
// -------------------------------------------------------------------------

// -------------------------------------------------------------------------
// Step 5: remove the pixel rule source file
// -------------------------------------------------------------------------

// -------------------------------------------------------------------------
// Step 6: remove the pixel agent system prompt
// -------------------------------------------------------------------------

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

/// Every directory where a global `pixel setup` or `pixel setup --remove`
/// rewrites a file, hence may have left a backup beside it.
fn global_backup_dirs(home: &Path, codex_home: &Path, opencode_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![
        home.join(".claude"),
        home.join(config::DEVIN_CONFIG_DIR),
        parent_of(home, config::CODEX_HOOKS_FILE),
        codex_home.to_path_buf(),
        parent_of(home, config::GEMINI_SETTINGS_FILE),
        home.join(config::PI_CONFIG_DIR),
        home.join(".local/share/pixel"),
        crate::antigravity::antigravity_config_dir(home),
        crate::antigravity::plugin_dir(home),
        crate::antigravity::cli_plugin_dir(home),
        opencode_dir.to_path_buf(),
    ];
    // The skill steps rename each harness's `skills/pixel*` to a
    // `.pixel-bak` dir in that harness's root (`.claude` is above).
    for root in config::SKILL_ROOTS {
        dirs.push(home.join(root));
    }
    dirs
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
/// duplicates: two entries of `dirs` may name the same directory. Directory
/// backups count too — the classify-skill step undoes whole dirs by rename.
fn find_backups(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = dirs
        .iter()
        .filter_map(|dir| fs::read_dir(dir).ok())
        .flat_map(|entries| entries.flatten().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
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
    let (dirs, files): (Vec<&PathBuf>, Vec<&PathBuf>) =
        backups.iter().partition(|path| path.is_dir());
    let quoted = |paths: &[&PathBuf]| {
        paths
            .iter()
            .map(|path| routing::quoted_executable(path))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut commands = Vec::new();
    if !files.is_empty() {
        commands.push(format!("rm -- {}", quoted(&files)));
    }
    if !dirs.is_empty() {
        commands.push(format!("rm -rf -- {}", quoted(&dirs)));
    }
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
        detail: Some(commands.join(" && ")),
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
        removed += usize::from(*hooks != before);
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

    /// Settings without a Pixel hook are left byte for byte: no rewrite.
    #[test]
    fn remove_claude_hooks_leaves_settings_without_pixel_hooks_untouched() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let path = home.join(".claude/settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"keep-me"}]}]}}"#;
        fs::write(&path, text).unwrap();
        let step = remove_claude_hooks(home, Path::new("/tmp/pixel"), false).unwrap();
        assert_eq!(step.summary, "removed 0 Claude hook event(s)");
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
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
        // A directory backup counts too: the classify-skill step undoes a
        // whole skills dir by renaming it with this suffix.
        fs::create_dir_all(a.join("dir.pixel-bak.3-0")).unwrap();

        // `a` twice: the global list can name one directory two ways
        // (`~/.codex` and `$CODEX_HOME`).
        let found = find_backups(&[b.clone(), a.clone(), a.clone(), root.path().join("absent")]);

        assert_eq!(
            found,
            vec![
                a.join("dir.pixel-bak.3-0"),
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

#[cfg(test)]
mod contract_tests;

#[cfg(test)]
mod roundtrip_tests {
    use super::*;

    #[test]
    fn setup_should_return_user_json_and_toml_bytes_after_removal() {
        let home = tempfile::tempdir().unwrap();
        let exe = Path::new("/tmp/pixel");
        for relative in [
            ".claude/settings.json",
            ".codex/hooks.json",
            ".pi/agent/settings.json",
            ".config/devin/config.json",
        ] {
            let path = home.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let text = "{\n  \"theme\" : \"dark\", \"number\": 1e2\n}\n";
            fs::write(&path, text).unwrap();
            let mut value: serde_json::Value = serde_json::from_str(text).unwrap();
            value["hooks"] = serde_json::json!({"SessionStart":[{"hooks":[{"command":"pixel run-hook task-event --provider claude --event session-start"}]}]});
            install::write_settings(&path, &value, false).unwrap();
            assert!(
                fs::read_to_string(&path)
                    .unwrap()
                    .contains("\"number\": 1e2")
            );
            remove_pixel_hooks_from_settings(&path, exe, false).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), text, "{relative}");
        }
        let toml = home.path().join(".codex/config.toml");
        let text = "# my model\nmodel  = 'o3'\n";
        fs::write(&toml, text).unwrap();
        crate::codex_config::install_task_hooks(&home.path().join(".codex"), exe, 1, false)
            .unwrap();
        remove_pixel_hooks_from_settings(&home.path().join(config::CODEX_HOOKS_FILE), exe, false)
            .unwrap();
        assert_eq!(fs::read_to_string(toml).unwrap(), text);
    }
}
