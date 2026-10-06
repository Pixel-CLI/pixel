// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Antigravity integration module.
//!
//! Deploys the Pixel plugin to both the IDE and CLI plugin directories,
//! ensures the plugin is enabled in `~/.gemini/config/config.json`,
//! and installs the `pixel-guard` hooks in `~/.gemini/config/hooks.json`.

use serde_json::{Value, json};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::install::{CheckStatus, InstallStep, Result};

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

/// Whether `entry` is the global `pixel-guard` pixel registered before its
/// Antigravity plugin owned the guard: every handler is a pixel executable's
/// Antigravity `guard` or `metrics` hook. A user's own hook under the same
/// name is anything else, and is left alone.
fn is_retired_global_guard(entry: &Value, exe: &Path) -> bool {
    global_entry_commands(entry).is_some_and(|commands| {
        !commands.is_empty()
            && commands.iter().all(|(_, command)| {
                matches!(
                    crate::routing::pixel_run_hook_verb(command, exe),
                    Some("guard --provider antigravity" | "metrics --provider antigravity")
                )
            })
    })
}

/// Whether a global `hooks.json` entry would run Pixel's Antigravity guard
/// beside the plugin's: enabled, with a guard handler under `PreToolUse` or
/// `PreInvocation` (Antigravity runs the two events independently).
fn runs_global_guard(entry: &Value) -> bool {
    entry.get("enabled") != Some(&Value::Bool(false))
        && global_entry_commands(entry).is_some_and(|commands| {
            commands.iter().any(|(event, command)| {
                matches!(*event, "PreToolUse" | "PreInvocation")
                    && command.contains("run-hook guard --provider antigravity")
            })
        })
}

/// Remove the retired global guard now owned by Pixel's Antigravity plugin.
///
/// Only the entry pixel registered is removed ([`is_retired_global_guard`]);
/// a `pixel-guard` a user wrote is kept, and reported yellow when it still
/// runs Pixel's guard beside the plugin. The file is read and validated in a
/// dry run too, so a dry run fails where the real run would.
pub fn remove_global_hooks(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let h_path = hooks_path(home);
    let step = |status, summary: String| InstallStep {
        id: "install.antigravity-hooks".into(),
        status,
        summary,
        detail: Some(format!("path={}", h_path.display())),
    };
    if !h_path.is_file() {
        return Ok(step(
            CheckStatus::Green,
            "no retired Antigravity global guard found".into(),
        ));
    }
    let mut root_val = read_json_object(&h_path)?;
    let root = root_val
        .as_object_mut()
        .ok_or_else(|| crate::InstallError::InvalidSettings {
            path: h_path.clone(),
            reason: "hooks.json root is not an object".into(),
        })?;
    match root.get("pixel-guard") {
        None => {
            return Ok(step(
                CheckStatus::Green,
                "no retired Antigravity global guard found".into(),
            ));
        }
        Some(entry) if !is_retired_global_guard(entry, exe) => {
            let (status, note) = if runs_global_guard(entry) {
                (
                    CheckStatus::Yellow,
                    "; it still runs Pixel's guard beside the plugin, remove it by hand",
                )
            } else {
                (CheckStatus::Green, "")
            };
            return Ok(step(
                status,
                format!(
                    "kept the user-defined pixel-guard in {}: it is not the entry pixel registered{note}",
                    h_path.display()
                ),
            ));
        }
        Some(_) => {}
    }
    if dry_run {
        return Ok(step(
            CheckStatus::Green,
            format!("would remove retired pixel-guard from {}", h_path.display()),
        ));
    }
    root.remove("pixel-guard");
    fs::write(&h_path, serde_json::to_string_pretty(&root_val)? + "\n")?;
    Ok(step(
        CheckStatus::Green,
        "removed retired pixel-guard from global hooks.json".into(),
    ))
}

/// `pixel doctor` check: Antigravity keeps its native tools, so no Pixel
/// plugin directory, plugin entry in `config.json` or global guard Pixel
/// registered may remain. Green-skips when Antigravity is not configured.
pub fn check_antigravity_install(
    home: &Path,
    exe: &Path,
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
    let mut left: Vec<String> = [&p_dir, &cli_dir]
        .into_iter()
        .filter(|dir| dir.is_dir() && is_pixel_plugin(dir))
        .map(|dir| dir.display().to_string())
        .collect();
    // The `pixel` config entry belongs to a user's own plugin of that name.
    let plugin_entry = foreign_plugins([&p_dir, &cli_dir]).is_empty()
        && fs::read_to_string(&cfg_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .is_some_and(|v| v.get("plugins").and_then(|p| p.get("pixel")).is_some());
    if plugin_entry {
        left.push(format!("the pixel plugin entry in {}", cfg_path.display()));
    }
    let retired_guard = fs::read_to_string(&h_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.get("pixel-guard").cloned())
        .is_some_and(|entry| is_retired_global_guard(&entry, exe));
    if retired_guard {
        left.push(format!("the pixel-guard in {}", h_path.display()));
    }
    if !left.is_empty() {
        return Err(format!(
            "retired Pixel Antigravity integration remains: {} — run `pixel install` to remove it",
            left.join(", ")
        ));
    }
    Ok((
        "no Pixel plugin or hook; Antigravity keeps its native tools".into(),
        detail,
    ))
}

/// Remove Antigravity integration during `pixel uninstall`.
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
        return Ok(InstallStep {
            id: "uninstall.antigravity".into(),
            status: CheckStatus::Green,
            summary: "would remove Antigravity plugin and hooks".into(),
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

    // Only the global guard Pixel wrote; a user-defined `pixel-guard` stays.
    let global = remove_global_hooks(home, exe, false)?;
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
        let _ = fs::write(
            &cfg_path,
            serde_json::to_string_pretty(&v).unwrap_or_default() + "\n",
        );
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
    fn check_should_be_red_until_install_removes_an_earlier_plugin_and_keep_user_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let exe = PathBuf::from("/usr/local/bin/pixel");
        let (summary, _) = check_antigravity_install(home, &exe).unwrap();
        assert!(summary.contains("skipping"), "{summary}");

        fs::create_dir_all(antigravity_config_dir(home)).unwrap();
        fs::write(
            config_path(home),
            r#"{"plugins":{"other":{"enabled":true},"pixel":{"enabled":true}},"keep":"config"}"#,
        )
        .unwrap();
        let mut hooks = json!({"other-hook": {"enabled": true}});
        hooks["pixel-guard"] = retired_global_guard("/usr/local/bin/pixel");
        fs::write(hooks_path(home), serde_json::to_vec(&hooks).unwrap()).unwrap();
        for dir in [plugin_dir(home), cli_plugin_dir(home)] {
            fs::create_dir_all(dir.join("rules")).unwrap();
            fs::write(
                dir.join("plugin.json"),
                r#"{"name":"pixel","managedBy":"pixel"}"#,
            )
            .unwrap();
        }
        let error = check_antigravity_install(home, &exe).unwrap_err();
        assert!(
            error.contains("retired Pixel Antigravity integration"),
            "{error}"
        );
        assert!(error.contains("pixel-guard"), "{error}");
        assert!(error.contains("plugin entry"), "{error}");

        // No `agy` on this test's PATH lookup: the fake binary name is absent.
        remove_antigravity_with_agy(home, &exe, false, OsStr::new("pixel-test-no-agy")).unwrap();
        assert!(!plugin_dir(home).exists());
        assert!(!cli_plugin_dir(home).exists());
        let hooks: Value = serde_json::from_slice(&fs::read(hooks_path(home)).unwrap()).unwrap();
        assert_eq!(hooks, json!({"other-hook": {"enabled": true}}));
        let config: Value = serde_json::from_slice(&fs::read(config_path(home)).unwrap()).unwrap();
        assert_eq!(config["keep"], "config");
        assert_eq!(config["plugins"], json!({"other": {"enabled": true}}));
        let (summary, _) = check_antigravity_install(home, &exe).unwrap();
        assert!(summary.contains("native tools"), "{summary}");

        // A user-defined `pixel-guard` is theirs: kept, and not reported.
        let mine = json!({"enabled": true, "PreToolUse": []});
        fs::write(
            hooks_path(home),
            serde_json::to_vec(&json!({"pixel-guard": mine})).unwrap(),
        )
        .unwrap();
        remove_antigravity_with_agy(home, &exe, false, OsStr::new("pixel-test-no-agy")).unwrap();
        let hooks: Value = serde_json::from_slice(&fs::read(hooks_path(home)).unwrap()).unwrap();
        assert_eq!(hooks["pixel-guard"], mine);
        assert!(check_antigravity_install(home, &exe).is_ok());
    }

    /// The global `pixel-guard` an install before the plugin wrote: guard on
    /// both pre-events, metrics after each tool.
    fn retired_global_guard(exe: &str) -> Value {
        let guard = format!("'{exe}' run-hook guard --provider antigravity");
        let metrics = format!("'{exe}' run-hook metrics --provider antigravity");
        json!({
            "enabled": true,
            "PreToolUse": [{"matcher": "run_command|grep_search|find_by_name|find_file_by_name|list_dir|file_search|view_file|read|read_file|notebook_read",
                "hooks": [{"type": "command", "command": guard, "timeout": 10}]}],
            "PreInvocation": [{"type": "command", "command": guard, "timeout": 10}],
            "PostToolUse": [{"matcher": "*",
                "hooks": [{"type": "command", "command": metrics, "timeout": 10}]}]
        })
    }

    #[test]
    fn remove_global_hooks_should_remove_only_the_guard_pixel_registered() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        fs::create_dir_all(antigravity_config_dir(home)).unwrap();
        let exe = PathBuf::from("/usr/local/bin/pixel");
        // Written by a release at another path: still pixel's own entry.
        let planted = json!({
            "pixel-guard": retired_global_guard("/opt/homebrew/bin/pixel"),
            "other-hook": {"enabled": true}
        });
        let text = serde_json::to_string_pretty(&planted).unwrap();
        fs::write(hooks_path(home), &text).unwrap();

        let dry = remove_global_hooks(home, &exe, true).unwrap();
        assert_eq!(dry.status, CheckStatus::Green);
        assert!(
            dry.summary.starts_with("would remove retired pixel-guard"),
            "{}",
            dry.summary
        );
        assert_eq!(fs::read_to_string(hooks_path(home)).unwrap(), text);

        let step = remove_global_hooks(home, &exe, false).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        assert_eq!(
            step.summary,
            "removed retired pixel-guard from global hooks.json"
        );
        let hooks: Value = serde_json::from_slice(&fs::read(hooks_path(home)).unwrap()).unwrap();
        assert_eq!(hooks, json!({"other-hook": {"enabled": true}}));
    }

    #[test]
    fn remove_global_hooks_should_keep_a_user_defined_pixel_guard() {
        let exe = PathBuf::from("/usr/local/bin/pixel");
        let mut with_user_hook = retired_global_guard("/usr/local/bin/pixel");
        with_user_hook["PreInvocation"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "command", "command": "user-audit", "timeout": 10}));
        for (case, entry, status) in [
            (
                "a user's own command",
                json!({"enabled": true, "PreInvocation":
                    [{"type": "command", "command": "user-audit", "timeout": 10}]}),
                CheckStatus::Green,
            ),
            (
                "pixel's guard with a user hook added",
                with_user_hook,
                CheckStatus::Yellow,
            ),
            (
                "another program's run-hook guard",
                retired_global_guard("/usr/local/bin/notpixel"),
                CheckStatus::Yellow,
            ),
            (
                "a key pixel never wrote",
                json!({"enabled": true, "Stop": []}),
                CheckStatus::Green,
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            fs::create_dir_all(antigravity_config_dir(home)).unwrap();
            let text = serde_json::to_string_pretty(&json!({"pixel-guard": entry})).unwrap();
            fs::write(hooks_path(home), &text).unwrap();
            for dry_run in [true, false] {
                let step = remove_global_hooks(home, &exe, dry_run).unwrap();
                assert_eq!(step.status, status, "{case}: {}", step.summary);
                assert!(
                    step.summary
                        .starts_with("kept the user-defined pixel-guard"),
                    "{case}: {}",
                    step.summary
                );
                assert_eq!(
                    fs::read_to_string(hooks_path(home)).unwrap(),
                    text,
                    "{case}"
                );
            }
        }
    }

    #[test]
    fn global_guard_is_detected_on_either_pre_event() {
        let guard = json!({"type": "command",
            "command": "'/usr/local/bin/pixel' run-hook guard --provider antigravity"});
        let pre_invocation_only = json!({"enabled": true, "PreInvocation": [guard.clone()]});
        assert!(runs_global_guard(&pre_invocation_only));
        // Not first in its group, behind a user's own handler.
        let pre_tool_second = json!({"PreToolUse": [{"matcher": "*", "hooks": [
            {"type": "command", "command": "user-audit"}, guard.clone()]}]});
        assert!(runs_global_guard(&pre_tool_second));
        let disabled = json!({"enabled": false, "PreInvocation": [guard.clone()]});
        assert!(!runs_global_guard(&disabled));
        let metrics_only = json!({"PostToolUse": [{"matcher": "*", "hooks": [
            {"type": "command",
             "command": "'/usr/local/bin/pixel' run-hook metrics --provider antigravity"}]}]});
        assert!(!runs_global_guard(&metrics_only));
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
