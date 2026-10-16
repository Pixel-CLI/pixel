// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Antigravity integration module.
//!
//! Installs Antigravity prompt hooks and the Pixel metrics plugin.

use serde_json::{Value, json};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::install::{self, CheckStatus, InstallStep, Result};

pub(crate) fn antigravity_config_dir(home: &Path) -> PathBuf {
    home.join(".gemini/config")
}

pub(crate) fn plugin_dir(home: &Path) -> PathBuf {
    antigravity_config_dir(home).join("plugins/pixel")
}

pub(crate) fn cli_plugin_dir(home: &Path) -> PathBuf {
    home.join(".gemini/antigravity-cli/plugins/pixel")
}

pub(crate) fn hooks_path(home: &Path) -> PathBuf {
    antigravity_config_dir(home).join("hooks.json")
}

pub(crate) fn config_path(home: &Path) -> PathBuf {
    antigravity_config_dir(home).join("config.json")
}

fn run_agy_plugin_with(
    executable: &OsStr,
    home: &Path,
    action: &str,
    plugin_dir: Option<&Path>,
) -> Result<bool> {
    let mut command = Command::new(executable);
    command.env("HOME", home).arg("plugin").arg(action);
    if let Some(path) = plugin_dir {
        command.arg(path);
    } else {
        command.arg("pixel");
    }
    let output = match command.output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "agy plugin {action} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    Ok(true)
}

fn agy_pixel_registered_with(executable: &OsStr, home: &Path) -> Result<Option<bool>> {
    let output = match Command::new(executable)
        .env("HOME", home)
        .args(["plugin", "list"])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "agy plugin list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim();
    if stdout.is_empty() || stdout == "No imported plugins." {
        return Ok(Some(false));
    }
    // A list output we cannot parse is unknown, not an error: registration
    // stays a best-effort probe and the caller falls back to `plugin install`.
    let Ok(listing) = serde_json::from_str::<Value>(stdout) else {
        return Ok(None);
    };
    Ok(Some(
        listing
            .get("imports")
            .and_then(Value::as_array)
            .is_some_and(|imports| {
                imports
                    .iter()
                    .any(|entry| entry.get("name").and_then(Value::as_str) == Some("pixel"))
            }),
    ))
}

fn read_json_object(path: &Path) -> Result<Value> {
    if !path.is_file() {
        return Ok(json!({}));
    }
    let text = fs::read_to_string(path)?;
    let value: Value =
        serde_json::from_str(&text).map_err(|error| crate::InstallError::InvalidSettings {
            path: path.to_path_buf(),
            reason: format!("invalid JSON: {error}"),
        })?;
    if !value.is_object() {
        return Err(crate::InstallError::InvalidSettings {
            path: path.to_path_buf(),
            reason: "JSON root must be an object".into(),
        });
    }
    Ok(value)
}

/// Whether `path` is a plugin directory Pixel wrote: its `plugin.json`
/// says `managedBy: "pixel"`. Anything else under that name is the user's.
fn is_pixel_plugin(path: &Path) -> bool {
    fs::read_to_string(path.join("plugin.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .is_some_and(|manifest| manifest.get("managedBy").and_then(Value::as_str) == Some("pixel"))
}

/// The plugin directories under Pixel's name that Pixel did not write.
fn foreign_plugins(dirs: [&Path; 2]) -> Vec<&Path> {
    dirs.into_iter()
        .filter(|dir| dir.exists() && !is_pixel_plugin(dir))
        .collect()
}

/// The handler commands of a global `hooks.json` entry, by event:
/// `PreToolUse` and `PostToolUse` hold matcher groups of handlers,
/// `PreInvocation` holds handlers directly. `None` when the entry has a key
/// other than those and `enabled`, or a handler without a command: a shape
/// pixel never wrote.
fn global_entry_commands(entry: &Value) -> Option<Vec<(&str, &str)>> {
    let mut commands = Vec::new();
    for (event, value) in entry.as_object()? {
        let handlers: Vec<&Value> = match event.as_str() {
            "enabled" => continue,
            "PreInvocation" => value.as_array()?.iter().collect(),
            "PreToolUse" | "PostToolUse" => value
                .as_array()?
                .iter()
                .map(|group| group.get("hooks").and_then(Value::as_array))
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect(),
            _ => return None,
        };
        for handler in handlers {
            commands.push((event.as_str(), handler.get("command")?.as_str()?));
        }
    }
    Some(commands)
}

/// Whether `entry` is the Antigravity prompt brief hook Pixel registered.
pub(crate) fn is_pixel_brief(entry: &Value, exe: &Path) -> bool {
    global_entry_commands(entry).is_some_and(|commands| {
        commands.len() == 1
            && commands.iter().all(|(event, command)| {
                *event == "PreInvocation"
                    && crate::routing::pixel_run_hook_verb(command, exe)
                        == Some("task-event --provider antigravity --event prompt-submit")
            })
    })
}

/// Remove the prompt brief hook Pixel registered in the global `hooks.json`;
/// a `pixel-brief` entry the user wrote is kept. The file is read and
/// validated in a dry run too, so a dry run fails where the real run would.
fn remove_brief_hook(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let h_path = hooks_path(home);
    let step = |summary: String| InstallStep {
        id: "install.antigravity-hooks".into(),
        status: CheckStatus::Green,
        summary,
        detail: Some(format!("path={}", h_path.display())),
    };
    if !h_path.is_file() {
        return Ok(step("no Antigravity hooks file found".into()));
    }
    let mut root_val = read_json_object(&h_path)?;
    let root = root_val
        .as_object_mut()
        .ok_or_else(|| crate::InstallError::InvalidSettings {
            path: h_path.clone(),
            reason: "hooks.json root is not an object".into(),
        })?;
    if !root
        .get("pixel-brief")
        .is_some_and(|entry| is_pixel_brief(entry, exe))
    {
        return Ok(step("no Pixel Antigravity hooks found to remove".into()));
    }
    if dry_run {
        return Ok(step(format!(
            "would remove the prompt brief hook from {}",
            h_path.display()
        )));
    }
    root.remove("pixel-brief");
    install::write_settings(&h_path, &root_val, false)?;
    Ok(step(
        "removed the prompt brief hook from global hooks.json".into(),
    ))
}

/// The prompt-submission evidence brief for Gemini CLI: a `BeforeAgent`
/// entry in `~/.gemini/settings.json` (Claude's group schema, `matcher`
/// groups), whose stdout `additionalContext` carries the brief. Registered
/// only where the settings file already exists; re-running replaces Pixel's
/// earlier entries, never the user's.
pub fn install_gemini_brief(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let path = home.join(".gemini/settings.json");
    let step = |status, summary: String| InstallStep {
        id: "hooks.gemini-brief".into(),
        status,
        summary,
        detail: Some(format!("path={}", path.display())),
    };
    if !path.is_file() {
        return Ok(step(
            CheckStatus::Green,
            "Gemini CLI is not configured; skipped the prompt brief hook".into(),
        ));
    }
    let mut root_val = read_json_object(&path)?;
    let hooks = root_val
        .as_object_mut()
        .ok_or_else(|| crate::InstallError::InvalidSettings {
            path: path.clone(),
            reason: "settings.json root is not an object".into(),
        })?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| crate::InstallError::InvalidSettings {
            path: path.clone(),
            reason: "settings.json `hooks` is not an object".into(),
        })?;
    let command = format!(
        "{} run-hook task-event --provider gemini --event prompt-submit",
        crate::routing::quoted_executable(exe)
    );
    let groups = hooks
        .entry("BeforeAgent")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| crate::InstallError::InvalidSettings {
            path: path.clone(),
            reason: "hooks.BeforeAgent is not an array".into(),
        })?;
    for group in groups.iter_mut() {
        if let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            inner.retain(|hook| {
                !hook
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|cmd| {
                        crate::routing::pixel_run_hook_verb(cmd, exe)
                            == Some("task-event --provider gemini --event prompt-submit")
                    })
            });
        }
    }
    groups.retain(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_none_or(|inner| !inner.is_empty())
    });
    if dry_run {
        return Ok(step(
            CheckStatus::Green,
            format!("would register the prompt brief hook in {}", path.display()),
        ));
    }
    groups.push(json!({
        "matcher": "*",
        "hooks": [{
            "name": "pixel-brief",
            "type": "command",
            "command": command,
            "timeout": 10000
        }]
    }));
    install::write_settings(&path, &root_val, false)?;
    Ok(step(
        CheckStatus::Green,
        "registered the prompt brief hook for Gemini CLI".into(),
    ))
}

/// The prompt-submission evidence brief for Antigravity: a `pixel-brief`
/// entry in `~/.gemini/config/hooks.json` running on `PreInvocation`. The
/// hook gates on `invocationNum == 0` (the first model call of each turn)
/// and answers `injectSteps[].ephemeralMessage`. Registered only where the
/// file already exists; a re-run rewrites only an entry Pixel registered.
pub fn install_antigravity_brief(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let path = hooks_path(home);
    let step = |status, summary: String| InstallStep {
        id: "hooks.antigravity-brief".into(),
        status,
        summary,
        detail: Some(format!("path={}", path.display())),
    };
    if !path.is_file() {
        return Ok(step(
            CheckStatus::Green,
            "Antigravity is not configured; skipped the prompt brief hook".into(),
        ));
    }
    let mut root_val = read_json_object(&path)?;
    if !root_val.is_object() {
        return Err(crate::InstallError::InvalidSettings {
            path,
            reason: "hooks.json root is not an object".into(),
        });
    }
    if root_val
        .get("pixel-brief")
        .is_some_and(|entry| !is_pixel_brief(entry, exe))
    {
        return Ok(step(
            CheckStatus::Yellow,
            format!(
                "kept the user-defined pixel-brief entry in {}",
                path.display()
            ),
        ));
    }
    if dry_run {
        return Ok(step(
            CheckStatus::Green,
            format!("would register the prompt brief hook in {}", path.display()),
        ));
    }
    root_val["pixel-brief"] = json!({
        "enabled": true,
        "PreInvocation": [{
            "type": "command",
            "command": format!("{} run-hook task-event --provider antigravity --event prompt-submit", crate::routing::quoted_executable(exe)),
            "timeout": 10
        }]
    });
    install::write_settings(&path, &root_val, false)?;
    Ok(step(
        CheckStatus::Green,
        "registered the prompt brief hook for Antigravity".into(),
    ))
}

/// Install Pixel's Antigravity plugin with its deferred metrics relay.
pub fn install_antigravity_plugin(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    install_antigravity_plugin_with_agy(home, exe, dry_run, OsStr::new("agy"))
}

fn install_antigravity_plugin_with_agy(
    home: &Path,
    exe: &Path,
    dry_run: bool,
    agy: &OsStr,
) -> Result<InstallStep> {
    let ide_dir = plugin_dir(home);
    let cli_dir = cli_plugin_dir(home);
    if dry_run {
        return Ok(InstallStep {
            id: "install.antigravity-plugin".into(),
            status: CheckStatus::Green,
            summary: format!(
                "would deploy Antigravity metrics plugin to {} and {}",
                ide_dir.display(),
                cli_dir.display()
            ),
            detail: None,
        });
    }

    if let Some(foreign) = foreign_plugins([&ide_dir, &cli_dir]).first() {
        return Ok(InstallStep {
            id: "install.antigravity-plugin".into(),
            status: CheckStatus::Yellow,
            summary: format!(
                "kept the user's own plugin at {}; Antigravity metrics relay not installed",
                foreign.display()
            ),
            detail: Some(format!("kept_plugin_path={}", foreign.display())),
        });
    }
    let config = config_path(home);
    let mut config_value = read_json_object(&config)?;
    if config_value
        .get("plugins")
        .is_some_and(|plugins| !plugins.is_object())
        || config_value
            .get("plugins")
            .and_then(|plugins| plugins.get("pixel"))
            .is_some_and(|pixel| !pixel.is_object())
    {
        return Err(crate::InstallError::InvalidSettings {
            path: config,
            reason: "plugins and plugins.pixel settings must be JSON objects".into(),
        });
    }
    let manifest = json!({
        "$schema": "https://antigravity.google/schemas/v1/plugin.json",
        "name": "pixel",
        "displayName": "Pixel Code Intelligence",
        "description": "Deterministic AST code retrieval and code-graph navigation layer for Antigravity.",
        "version": env!("CARGO_PKG_VERSION"),
        "managedBy": "pixel"
    });
    let hooks = antigravity_metrics_plugin_hooks(exe);
    let manifest_text = serde_json::to_string_pretty(&manifest)? + "\n";
    let hooks_text = serde_json::to_string_pretty(&hooks)? + "\n";
    for path in [&ide_dir, &cli_dir] {
        fs::create_dir_all(path.join("skills/pixel"))?;
        install::write_atomically(&path.join("plugin.json"), &manifest_text)?;
        fs::write(
            path.join("skills/pixel/SKILL.md"),
            crate::skill::PIXEL_SKILL,
        )?;
        install::write_atomically(&path.join("hooks.json"), &hooks_text)?;
    }
    config_value["plugins"]["pixel"]["enabled"] = json!(true);
    install::write_settings(&config, &config_value, false)?;
    let registration = run_agy_plugin_with(agy, home, "install", Some(&cli_dir));
    let registration_error = registration.as_ref().err().map(ToString::to_string);
    let registered = registration.unwrap_or(false);
    let (status, registration_summary) = if let Some(error) = registration_error {
        (
            CheckStatus::Yellow,
            format!("; CLI plugin registration failed: {error}"),
        )
    } else if registered {
        (CheckStatus::Green, " and registered it with agy".into())
    } else {
        (
            CheckStatus::Green,
            " (agy not found; CLI registration deferred)".into(),
        )
    };
    Ok(InstallStep {
        id: "install.antigravity-plugin".into(),
        status,
        summary: format!(
            "deployed Antigravity metrics plugin to IDE {} and CLI {}{}",
            ide_dir.display(),
            cli_dir.display(),
            registration_summary
        ),
        detail: Some(format!(
            "ide_path={}; cli_path={}; agy_registered={registered}",
            ide_dir.display(),
            cli_dir.display()
        )),
    })
}

fn antigravity_metrics_plugin_hooks(exe: &Path) -> Value {
    let command = format!(
        "{} run-hook metrics --provider antigravity",
        crate::routing::quoted_executable(exe)
    );
    json!({
        "pixel-guard": {
            "enabled": true,
            "PostToolUse": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "timeout": 10
                }]
            }],
            "PostInvocation": [{
                "type": "command",
                "command": format!("{command} --event post-invocation"),
                "timeout": 10
            }]
        }
    })
}

/// `pixel doctor` check: verify both deferred-relay events and plugin setup.
/// Green-skips when Antigravity is not configured.
pub fn check_antigravity_install(
    home: &Path,
    exe: &Path,
) -> std::result::Result<(String, Value), String> {
    check_antigravity_install_with_agy(home, exe, OsStr::new("agy"))
}

fn check_antigravity_install_with_agy(
    home: &Path,
    exe: &Path,
    agy: &OsStr,
) -> std::result::Result<(String, Value), String> {
    let p_dir = plugin_dir(home);
    let cli_dir = cli_plugin_dir(home);
    let h_path = hooks_path(home);
    let cfg_path = config_path(home);
    let detail = json!({
        "plugin_dir": p_dir.display().to_string(),
        "cli_plugin_dir": cli_dir.display().to_string(),
        "hooks_path": h_path.display().to_string(),
        "config_path": cfg_path.display().to_string(),
    });
    if !antigravity_config_dir(home).is_dir() {
        return Ok((
            "Antigravity config directory not present (~/.gemini/config) — skipping".into(),
            detail,
        ));
    }
    let plugin_dirs = [p_dir.as_path(), cli_dir.as_path()];
    let has_managed_plugin = plugin_dirs.iter().any(|dir| is_pixel_plugin(dir));
    if !has_managed_plugin {
        return Ok((
            "no Pixel plugin or hook; Antigravity keeps its native tools".into(),
            detail,
        ));
    }

    let mut missing = Vec::new();
    let expected_hooks = antigravity_metrics_plugin_hooks(exe);
    for (surface, dir) in [("IDE", &p_dir), ("CLI", &cli_dir)] {
        let managed = is_pixel_plugin(dir);
        if !managed {
            missing.push(format!("{surface} plugin is not managed by Pixel"));
            continue;
        }
        if fs::read_to_string(dir.join("skills/pixel/SKILL.md"))
            .ok()
            .as_deref()
            != Some(crate::skill::PIXEL_SKILL)
        {
            missing.push(
                if surface == "IDE" {
                    "IDE skills/pixel/SKILL.md"
                } else {
                    "CLI skills/pixel/SKILL.md"
                }
                .into(),
            );
        }
        let installed_hooks = fs::read_to_string(dir.join("hooks.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        if installed_hooks.as_ref() != Some(&expected_hooks) {
            missing.push(
                if surface == "IDE" {
                    "IDE hooks.json (metrics relay)"
                } else {
                    "CLI hooks.json (metrics relay)"
                }
                .into(),
            );
        }
    }
    let plugin_enabled = fs::read_to_string(&cfg_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.get("plugins")?.get("pixel")?.get("enabled")?.as_bool())
        == Some(true);
    if !plugin_enabled {
        missing.push("config.json (pixel plugin enabled)".into());
    }
    if agy_pixel_registered_with(agy, home).map_err(|error| error.to_string())? == Some(false) {
        missing.push("Antigravity CLI Pixel plugin registration".into());
    }
    if !missing.is_empty() {
        return Err(format!(
            "Antigravity Pixel metrics plugin incomplete: {} — run `pixel setup`",
            missing.join(", ")
        ));
    }
    Ok((
        "Antigravity Pixel deferred metrics relay configured; native tools preserved".into(),
        detail,
    ))
}

/// Remove Antigravity integration during `pixel setup --remove`.
pub fn remove_antigravity(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    remove_antigravity_with_agy(home, exe, dry_run, OsStr::new("agy"))
}

fn remove_antigravity_with_agy(
    home: &Path,
    exe: &Path,
    dry_run: bool,
    agy: &OsStr,
) -> Result<InstallStep> {
    let p_dir = plugin_dir(home);
    let cli_dir = cli_plugin_dir(home);
    let cfg_path = config_path(home);

    if dry_run {
        let global = remove_brief_hook(home, exe, true)?;
        return Ok(InstallStep {
            id: "uninstall.antigravity".into(),
            status: global.status,
            summary: format!(
                "would remove Antigravity plugin and hooks; {}",
                global.summary
            ),
            detail: None,
        });
    }

    let mut removed_items = Vec::new();
    // A user's own plugin named `pixel` keeps its directory, its
    // registration and its config entry: none of them is Pixel's.
    let foreign = foreign_plugins([&p_dir, &cli_dir]);
    let kept = foreign
        .iter()
        .map(|dir| format!("; kept the user's own plugin at {}", dir.display()))
        .collect::<String>();
    let own_plugin = foreign.is_empty();

    if own_plugin && agy_pixel_registered_with(agy, home)? == Some(true) {
        run_agy_plugin_with(agy, home, "uninstall", None)?;
        removed_items.push("CLI plugin registration");
    }

    for (label, dir) in [
        ("IDE plugin directory", &p_dir),
        ("CLI plugin directory", &cli_dir),
    ] {
        if dir.is_dir() && is_pixel_plugin(dir) {
            fs::remove_dir_all(dir)?;
            removed_items.push(label);
        }
    }

    // Only the brief hook Pixel wrote; a user-defined `pixel-brief` stays.
    let global = remove_brief_hook(home, exe, false)?;
    if global.summary.starts_with("removed") {
        removed_items.push("hooks.json entry");
    }

    if own_plugin
        && cfg_path.is_file()
        && let Ok(text) = fs::read_to_string(&cfg_path)
        && let Ok(mut v) = serde_json::from_str::<Value>(&text)
        && let Some(plugins) = v.get_mut("plugins").and_then(Value::as_object_mut)
        && plugins.remove("pixel").is_some()
    {
        install::write_settings(&cfg_path, &v, false)?;
        removed_items.push("config.json entry");
    }

    let summary = if removed_items.is_empty() {
        format!("no Antigravity integration found to remove{kept}")
    } else {
        format!("removed Antigravity: {}{kept}", removed_items.join(", "))
    };

    Ok(InstallStep {
        id: "uninstall.antigravity".into(),
        status: CheckStatus::Green,
        summary,
        detail: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_rewrites_should_back_up_original_bytes_and_preserve_user_formatting() {
        for gemini in [true, false] {
            let home = tempfile::tempdir().unwrap();
            let path = if gemini {
                home.path().join(".gemini/settings.json")
            } else {
                hooks_path(home.path())
            };
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let original = "{ \"user\" : 1e2 }\n";
            fs::write(&path, original).unwrap();
            let exe = Path::new("/tmp/pixel");
            let install = || {
                if gemini {
                    install_gemini_brief(home.path(), exe, false)
                } else {
                    install_antigravity_brief(home.path(), exe, false)
                }
            };
            install().unwrap();
            let installed = fs::read(&path).unwrap();
            install().unwrap();
            let backups = || {
                fs::read_dir(path.parent().unwrap())
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .filter(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .contains(".pixel-bak.")
                    })
                    .collect::<Vec<_>>()
            };
            let first = backups();
            assert_eq!(first.len(), 1, "reinstall creates no no-op backup");
            assert_eq!(fs::read(&first[0]).unwrap(), original.as_bytes());
            if gemini {
                crate::uninstall::uninstall(&crate::uninstall::UninstallOptions {
                    home: Some(home.path().to_path_buf()),
                    executable_path: Some(exe.into()),
                    ..Default::default()
                })
                .unwrap();
            } else {
                remove_brief_hook(home.path(), exe, false).unwrap();
            }
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
            let backups = backups();
            assert_eq!(backups.len(), 2);
            assert!(
                backups
                    .iter()
                    .any(|path| fs::read(path).unwrap() == installed)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn check_should_report_incomplete_pixel_managed_plugin_without_legacy_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        fs::create_dir_all(antigravity_config_dir(home)).unwrap();
        let exe = Path::new("/usr/local/bin/pixel");
        let absent_agy = tmp.path().join("missing-agy");
        install_antigravity_plugin_with_agy(home, exe, false, absent_agy.as_os_str()).unwrap();
        fs::remove_file(cli_plugin_dir(home).join("hooks.json")).unwrap();

        let error =
            check_antigravity_install_with_agy(home, exe, absent_agy.as_os_str()).unwrap_err();

        assert!(
            error.contains("Antigravity Pixel metrics plugin incomplete"),
            "{error}"
        );
        assert!(error.contains("CLI hooks.json (metrics relay)"), "{error}");
    }

    #[test]
    fn install_should_replace_a_pixel_managed_brief_from_an_old_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        fs::create_dir_all(antigravity_config_dir(home)).unwrap();
        let exe = PathBuf::from("/usr/local/bin/pixel");
        let old_entry = json!({"enabled": true, "PreInvocation": [{
            "type": "command",
            "command": "/opt/pixel/bin/pixel run-hook task-event --provider antigravity --event prompt-submit",
            "timeout": 10
        }]});
        fs::write(
            hooks_path(home),
            serde_json::to_string_pretty(&json!({"pixel-brief": old_entry})).unwrap(),
        )
        .unwrap();

        let step = install_antigravity_brief(home, &exe, false).unwrap();

        assert_eq!(step.status, CheckStatus::Green);
        let hooks: Value = serde_json::from_slice(&fs::read(hooks_path(home)).unwrap()).unwrap();
        assert_eq!(
            hooks["pixel-brief"]["PreInvocation"][0]["command"],
            format!(
                "{} run-hook task-event --provider antigravity --event prompt-submit",
                crate::routing::quoted_executable(&exe)
            )
        );
    }

    #[test]
    fn uninstall_dry_run_should_report_brief_cleanup_with_a_user_guard() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        fs::create_dir_all(antigravity_config_dir(home)).unwrap();
        let exe = PathBuf::from("/usr/local/bin/pixel");
        let brief = json!({"enabled": true, "PreInvocation": [{
            "type": "command",
            "command": format!(
                "{} run-hook task-event --provider antigravity --event prompt-submit",
                crate::routing::quoted_executable(&exe)
            ),
            "timeout": 10
        }]});
        let foreign_guard = json!({"enabled": true, "PreInvocation": [{
            "type": "command",
            "command": "user-audit",
            "timeout": 10
        }]});
        let text = serde_json::to_string_pretty(&json!({
            "pixel-brief": brief,
            "pixel-guard": foreign_guard
        }))
        .unwrap();
        fs::write(hooks_path(home), &text).unwrap();

        let step = remove_antigravity(home, &exe, true).unwrap();

        assert_eq!(step.status, CheckStatus::Green);
        assert!(step.summary.contains("would remove the prompt brief hook"));
        assert_eq!(fs::read_to_string(hooks_path(home)).unwrap(), text);
    }

    #[cfg(unix)]
    #[test]
    fn antigravity_plugin_should_install_deferred_post_tool_metrics_relay() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        fs::create_dir_all(antigravity_config_dir(home)).unwrap();
        fs::write(
            config_path(home),
            r#"{"plugins":{"other":{"enabled":true}},"keep":1}"#,
        )
        .unwrap();
        let exe = Path::new("/opt/pixel tools/pixel");
        let agy = fake_agy(&tmp.path().join("agy"), r#"{"imports":[{"name":"pixel"}]}"#);

        let step = install_antigravity_plugin_with_agy(home, exe, false, agy.as_os_str()).unwrap();

        assert_eq!(step.status, CheckStatus::Green);
        assert!(step.summary.contains("registered it with agy"));
        let expected = json!({"pixel-guard": {"enabled": true, "PostToolUse": [{
            "matcher": "*", "hooks": [{
                "type": "command",
                "command": "'/opt/pixel tools/pixel' run-hook metrics --provider antigravity",
                "timeout": 10
            }]
        }], "PostInvocation": [{
            "type": "command",
            "command": "'/opt/pixel tools/pixel' run-hook metrics --provider antigravity --event post-invocation",
            "timeout": 10
        }]}});
        for dir in [plugin_dir(home), cli_plugin_dir(home)] {
            assert!(is_pixel_plugin(&dir));
            assert_eq!(read_json_object(&dir.join("hooks.json")).unwrap(), expected);
            assert_eq!(
                fs::read_to_string(dir.join("skills/pixel/SKILL.md")).unwrap(),
                crate::skill::PIXEL_SKILL
            );
        }
        assert_eq!(
            read_json_object(&config_path(home)).unwrap(),
            json!({"plugins":{"other":{"enabled":true},"pixel":{"enabled":true}},"keep":1})
        );
        let calls = fs::read_to_string(agy.with_extension("log")).unwrap();
        assert!(calls.contains("plugin install "));
    }

    #[cfg(unix)]
    #[test]
    fn antigravity_plugin_install_should_keep_foreign_plugin_and_return_yellow() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        fs::create_dir_all(antigravity_config_dir(home)).unwrap();
        let original_config =
            r#"{"plugins":{"pixel":{"enabled":false},"other":{"enabled":true}},"keep":1}"#;
        fs::write(config_path(home), original_config).unwrap();
        let foreign = plugin_dir(home);
        fs::create_dir_all(&foreign).unwrap();
        fs::write(foreign.join("plugin.json"), r#"{"name":"pixel"}"#).unwrap();
        let agy = fake_agy(&tmp.path().join("agy"), r#"{"imports":[{"name":"pixel"}]}"#);

        let step = install_antigravity_plugin_with_agy(
            home,
            Path::new("/opt/pixel/pixel"),
            false,
            agy.as_os_str(),
        )
        .unwrap();

        assert_eq!(step.status, CheckStatus::Yellow);
        assert!(
            step.summary.contains(&foreign.display().to_string()),
            "{}",
            step.summary
        );
        assert_eq!(
            fs::read_to_string(foreign.join("plugin.json")).unwrap(),
            r#"{"name":"pixel"}"#
        );
        assert!(!cli_plugin_dir(home).exists());
        assert_eq!(
            fs::read_to_string(config_path(home)).unwrap(),
            original_config
        );
        assert!(!agy.with_extension("log").exists(), "agy was called");
    }

    #[cfg(unix)]
    #[test]
    fn antigravity_plugin_install_should_finish_config_when_agy_registration_fails() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        fs::create_dir_all(antigravity_config_dir(home)).unwrap();
        fs::write(
            config_path(home),
            r#"{"plugins":{"other":{"enabled":true}},"keep":1}"#,
        )
        .unwrap();
        let agy = tmp.path().join("agy");
        fs::write(
            &agy,
            "#!/bin/sh\nprintf 'registration denied' >&2\nexit 9\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&agy).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&agy, permissions).unwrap();

        let step = install_antigravity_plugin_with_agy(
            home,
            Path::new("/opt/pixel/pixel"),
            false,
            agy.as_os_str(),
        )
        .unwrap();

        assert_eq!(step.status, CheckStatus::Yellow);
        assert!(
            step.summary.contains("registration failed"),
            "{}",
            step.summary
        );
        assert!(
            step.summary.contains("registration denied"),
            "{}",
            step.summary
        );
        assert!(plugin_dir(home).join("hooks.json").is_file());
        assert!(cli_plugin_dir(home).join("hooks.json").is_file());
        assert_eq!(
            read_json_object(&config_path(home)).unwrap(),
            json!({"plugins":{"other":{"enabled":true},"pixel":{"enabled":true}},"keep":1})
        );
    }

    #[test]
    fn gemini_brief_should_replace_only_pixel_owned_hooks_and_quote_executable() {
        let home = tempfile::tempdir().unwrap();
        let settings = home.path().join(".gemini/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(
            &settings,
            serde_json::to_vec(&json!({
                "hooks": {
                    "BeforeAgent": [{
                        "matcher": "*",
                        "hooks": [
                            {"type": "command", "command": "'/opt/pixel/pixel' run-hook task-event --provider gemini --event prompt-submit"},
                            {"type": "command", "command": "/opt/notpixel run-hook task-event --provider gemini --event prompt-submit"}
                        ]
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        install_gemini_brief(home.path(), Path::new("/opt/pixel's tools/pixel"), false).unwrap();

        let installed: Value = serde_json::from_slice(&fs::read(settings).unwrap()).unwrap();
        let groups = installed["hooks"]["BeforeAgent"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[0]["hooks"][0]["command"],
            "/opt/notpixel run-hook task-event --provider gemini --event prompt-submit"
        );
        assert_eq!(
            groups[1]["hooks"][0]["command"],
            "'/opt/pixel'\\''s tools/pixel' run-hook task-event --provider gemini --event prompt-submit"
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_agy_uninstall_preserves_pixel_plugin_files() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let plugin = plugin_dir(home);
        fs::create_dir_all(&plugin).unwrap();
        fs::write(
            plugin.join("plugin.json"),
            r#"{"name":"pixel","managedBy":"pixel"}"#,
        )
        .unwrap();
        let cli_plugin = cli_plugin_dir(home);
        fs::create_dir_all(&cli_plugin).unwrap();
        fs::write(
            cli_plugin.join("plugin.json"),
            r#"{"name":"pixel","managedBy":"pixel"}"#,
        )
        .unwrap();

        let fake_agy = tmp.path().join("agy");
        fs::write(
            &fake_agy,
            "#!/bin/sh\nif [ \"$*\" = \"plugin list\" ]; then printf '%s' '{\"imports\":[{\"name\":\"pixel\"}]}'; exit 0; fi\nexit 9\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_agy).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake_agy, permissions).unwrap();

        assert!(
            remove_antigravity_with_agy(
                home,
                Path::new("/opt/pixel/pixel"),
                false,
                fake_agy.as_os_str()
            )
            .is_err()
        );
        assert!(plugin.join("plugin.json").is_file());
        assert!(cli_plugin.join("plugin.json").is_file());
    }

    /// Write an executable `agy` stub that answers `plugin list` with
    /// `stdout` and logs every invocation beside itself, so a test can see
    /// whether the CLI was asked to uninstall.
    #[cfg(unix)]
    fn fake_agy(path: &Path, stdout: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(
            path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.log\"\nif [ \"$*\" = \"plugin list\" ]; then printf '%s' '{stdout}'; fi\n"
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
        path.to_path_buf()
    }

    /// Run `probe` again while it fails with ETXTBSY: a script this test just
    /// wrote is "busy" as long as a child another test thread forked in the
    /// meantime still holds a copy of the write descriptor, until that child
    /// execs. A fresh path does not avoid it; a short retry does (#455).
    /// Bounded, so a real error still surfaces within a second.
    #[cfg(unix)]
    fn unless_busy<T>(mut probe: impl FnMut() -> Result<T>) -> Result<T> {
        for _ in 0..50 {
            match probe() {
                Err(crate::InstallError::Io(e))
                    if e.kind() == std::io::ErrorKind::ExecutableFileBusy =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                outcome => return outcome,
            }
        }
        probe()
    }

    #[cfg(unix)]
    #[test]
    fn registration_probe_reads_imports_listings_and_missing_cli_as_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        for (index, (listing, expected)) in [
            (
                r#"{"imports":[{"name":"other"},{"name":"pixel"}]}"#,
                Some(true),
            ),
            (r#"{"imports":[{"name":"other"}]}"#, Some(false)),
            ("No imported plugins.", Some(false)),
        ]
        .into_iter()
        .enumerate()
        {
            // A fresh path per case: rewriting a script that was just executed
            // can fail the next execve with ETXTBSY on some filesystems.
            let agy = fake_agy(&tmp.path().join(format!("agy-{index}")), listing);
            assert_eq!(
                unless_busy(|| agy_pixel_registered_with(agy.as_os_str(), tmp.path())).unwrap(),
                expected,
                "{listing}"
            );
        }
        assert_eq!(
            agy_pixel_registered_with(OsStr::new("/nonexistent/agy"), tmp.path()).unwrap(),
            None,
            "an absent CLI leaves the registration unknown"
        );
        // A CLI that exists but cannot execute is an error, not "unknown":
        // only a missing executable leaves the registration undecided.
        let plain = tmp.path().join("plain");
        std::fs::write(&plain, "not executable").unwrap();
        assert!(agy_pixel_registered_with(plain.as_os_str(), tmp.path()).is_err());
        assert!(run_agy_plugin_with(plain.as_os_str(), tmp.path(), "list", None).is_err());
    }

    /// The plain wrapper names `agy` itself, so its PATH lookup needs a child
    /// process: a PATH pointing at the stub is process-global.
    #[cfg(unix)]
    #[test]
    fn agy_registration_should_resolve_the_cli_on_path() {
        for (listing, expected) in [("pixel", "true"), ("other", "false")] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "antigravity::tests::agy_registered_child",
                    "--nocapture",
                ])
                .env("PIXEL_ANTIGRAVITY_TEST_LISTING", listing)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{expected}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn agy_registered_child() {
        let Ok(listing) = std::env::var("PIXEL_ANTIGRAVITY_TEST_LISTING") else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        fake_agy(
            &bin.join("agy"),
            &format!(r#"{{"imports":[{{"name":"{listing}"}}]}}"#),
        );
        // SAFETY: single-threaded child process whose only ambient input is PATH.
        unsafe { std::env::set_var("PATH", &bin) };
        assert_eq!(
            agy_pixel_registered_with(OsStr::new("agy"), tmp.path()).unwrap(),
            Some(listing == "pixel")
        );
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_should_ask_the_cli_to_unregister_only_when_pixel_is_registered() {
        for (listing, unregistered) in [
            (r#"{"imports":[{"name":"pixel"}]}"#, true),
            (r#"{"imports":[{"name":"other"}]}"#, false),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            for dir in [plugin_dir(home), cli_plugin_dir(home)] {
                fs::create_dir_all(&dir).unwrap();
                fs::write(
                    dir.join("plugin.json"),
                    r#"{"name":"pixel","managedBy":"pixel"}"#,
                )
                .unwrap();
            }
            let agy = fake_agy(&tmp.path().join("agy"), listing);
            let step = remove_antigravity_with_agy(
                home,
                Path::new("/opt/pixel/pixel"),
                false,
                agy.as_os_str(),
            )
            .unwrap();
            assert_eq!(
                step.summary.contains("CLI plugin registration"),
                unregistered,
                "{listing}: {}",
                step.summary
            );
            let calls = fs::read_to_string(agy.with_extension("log")).unwrap_or_default();
            assert_eq!(calls.contains("plugin uninstall"), unregistered, "{calls}");
            assert!(!plugin_dir(home).exists());
            assert!(!cli_plugin_dir(home).exists());
        }
    }

    /// A user's own plugin named `pixel` is not a leftover: install keeps it,
    /// its registration and its config entry, and doctor stays green.
    #[cfg(unix)]
    #[test]
    fn cleanup_should_keep_a_users_own_pixel_plugin_and_its_registration() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let exe = Path::new("/opt/pixel/pixel");
        fs::create_dir_all(antigravity_config_dir(home)).unwrap();
        let config = r#"{"plugins":{"pixel":{"enabled":true}}}"#;
        fs::write(config_path(home), config).unwrap();
        let mine = plugin_dir(home);
        fs::create_dir_all(&mine).unwrap();
        fs::write(mine.join("plugin.json"), r#"{"name":"pixel"}"#).unwrap();
        let agy = fake_agy(&tmp.path().join("agy"), r#"{"imports":[{"name":"pixel"}]}"#);

        let step = remove_antigravity_with_agy(home, exe, false, agy.as_os_str()).unwrap();

        assert_eq!(step.status, CheckStatus::Green);
        assert_eq!(
            step.summary,
            format!(
                "no Antigravity integration found to remove; kept the user's own plugin at {}",
                mine.display()
            )
        );
        assert_eq!(
            fs::read_to_string(mine.join("plugin.json")).unwrap(),
            r#"{"name":"pixel"}"#
        );
        assert_eq!(fs::read_to_string(config_path(home)).unwrap(), config);
        assert!(!agy.with_extension("log").exists(), "agy was called");
        assert_eq!(
            check_antigravity_install(home, exe).unwrap().0,
            "no Pixel plugin or hook; Antigravity keeps its native tools"
        );
    }

    #[test]
    fn plugin_dir_and_helpers_return_the_expected_paths() {
        let home = Path::new("/home/user");
        assert_eq!(
            plugin_dir(home),
            Path::new("/home/user/.gemini/config/plugins/pixel")
        );
        assert_eq!(
            cli_plugin_dir(home),
            Path::new("/home/user/.gemini/antigravity-cli/plugins/pixel")
        );
        assert_eq!(
            hooks_path(home),
            Path::new("/home/user/.gemini/config/hooks.json")
        );
        assert_eq!(
            config_path(home),
            Path::new("/home/user/.gemini/config/config.json")
        );
    }
}
