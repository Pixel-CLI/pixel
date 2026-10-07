// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Retirement of the OpenCode integration earlier releases wrote under
//! `~/.config/opencode/`.
//!
//! OpenCode keeps its native tools, so `pixel install` writes nothing there.
//! Earlier releases embedded the deployed prompt between the managed markers
//! of `AGENTS.md` and wrote an enforcing guard plugin to
//! `plugins/pixel.js`; `pixel install` and `pixel uninstall` still take both
//! out, and `pixel doctor` reports a leftover (`install.opencode-agents-md`).
//!
//! The plugin is recognised by the managed marker it carried
//! ([`config::MANAGED_BEGIN`]), not by its content: the plugin source no
//! longer ships, and a file at that path without the marker is the user's.
//! Text the user kept outside the `AGENTS.md` markers survives.
//!
//! The same step sweeps two stale artifacts out of `opencode.json`:
//! `instructions` entries naming the deployed prompt (written by earlier
//! installs) and `plugin`/`plugins` entries whose `pixel.mjs` target no longer
//! exists, a load failure on every launch left behind by manual attempts to
//! wire the repo's `.opencode/plugins/pixel.mjs` into a global config. A
//! `plugin` entry that resolves is left in place: removing an entry a user may
//! have written by hand is not this install's business.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config;
use crate::install::{self, CheckStatus, InstallStep, Result};

/// `AGENTS.md`, relative to the OpenCode config directory.
pub const AGENTS_MD_FILE: &str = "AGENTS.md";

/// `opencode.json`, relative to the OpenCode config directory — sweep-only:
/// the mechanism lives in AGENTS.md, so an unreadable config never blocks
/// the block.
pub const OPENCODE_CONFIG_FILE: &str = "opencode.json";

/// Path tail identifying the deployed prompt inside a leftover
/// `instructions` entry from the earlier mechanism.
const PROMPT_PATH_TAIL: &str = ".local/share/pixel/agent-prompt.md";

/// File name of the plugin the repo ships for project-local use; a global
/// `plugin` entry pointing at one that does not exist is a guaranteed load
/// failure.
const PIXEL_PLUGIN_FILE: &str = "pixel.mjs";

/// The guard plugin an earlier release wrote, relative to the OpenCode config
/// directory. It is `.js` because OpenCode's plugin directory loads by that
/// extension and silently ignores a `.mjs` there.
const PLUGIN_PATH: &str = "plugins/pixel.js";

/// The evidence-brief plugin `pixel install` deploys; the `plugins`
/// directory auto-loads it, so no registration entry is needed.
const BRIEF_PLUGIN_PATH: &str = "plugins/pixel-brief.js";

/// Whether `path` is a plugin pixel wrote: a file carrying the managed
/// marker. Anything else under that name belongs to the user, so uninstall
/// leaves it alone rather than deleting work it did not create.
fn is_managed_plugin(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|text| text.contains(config::MANAGED_BEGIN))
}

/// The OpenCode config directory: `$XDG_CONFIG_HOME/opencode` on a real
/// install (the variable OpenCode itself honours), `<home>/.config/opencode`
/// for tests and explicit `--home` overrides.
pub(crate) fn opencode_config_dir(home: &Path, home_was_explicit: bool) -> PathBuf {
    resolve_config_dir(home, home_was_explicit, std::env::var_os("XDG_CONFIG_HOME"))
}

/// Pure resolution behind [`opencode_config_dir`], so tests pin every arm
/// without mutating the process environment.
fn resolve_config_dir(
    home: &Path,
    home_was_explicit: bool,
    xdg_config_home: Option<std::ffi::OsString>,
) -> PathBuf {
    if !home_was_explicit
        && let Some(dir) = xdg_config_home
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("opencode");
    }
    home.join(".config").join("opencode")
}

fn agents_md_path(config_dir: &Path) -> PathBuf {
    config_dir.join(AGENTS_MD_FILE)
}

fn config_path(config_dir: &Path) -> PathBuf {
    config_dir.join(OPENCODE_CONFIG_FILE)
}

/// True when an `instructions` entry names the deployed Pixel prompt,
/// wherever it was installed from. Suffix-matched, not substring: a
/// `.../agent-prompt.md.bak` backup is the user's file, not our entry.
fn is_pixel_instruction(entry: &str) -> bool {
    entry
        .replace('\\', "/")
        .trim_end()
        .ends_with(PROMPT_PATH_TAIL)
}

/// True when a `plugin`/`plugins` entry names `pixel.mjs` and resolves to a
/// path that does not exist — `~/` against `home`, relative paths against
/// the config directory, like OpenCode. An entry that resolves to a real
/// file is left alone even if it is ours: v2 warns on file plugins but
/// still loads them.
fn is_stale_pixel_plugin(entry: &str, config_dir: &Path, home: &Path) -> bool {
    let entry = entry.trim();
    if entry.rsplit('/').next() != Some(PIXEL_PLUGIN_FILE) {
        return false;
    }
    let path = if let Some(rest) = entry.strip_prefix("~/") {
        home.join(rest)
    } else if entry.starts_with('~') {
        // `~other/...` — cannot resolve without a user database; leave it.
        return false;
    } else {
        let p = Path::new(entry);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            config_dir.join(p)
        }
    };
    !path.exists()
}

/// Remove pixel entries from `opencode.json`'s `instructions` and
/// `plugin`/`plugins` arrays, writing the file only when something came
/// out. Returns (instructions removed, plugins removed, note when the file
/// could not be swept).
fn sweep_config(path: &Path, home: &Path, dry_run: bool) -> (usize, usize, Option<String>) {
    let config_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut config: Value = match fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.is_object() => v,
            _ => {
                return (
                    0,
                    0,
                    Some(format!(
                        "{} left untouched (not strict JSON)",
                        path.display()
                    )),
                );
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (0, 0, None),
        Err(e) => {
            return (
                0,
                0,
                Some(format!("{} left untouched ({e})", path.display())),
            );
        }
    };
    let object = config.as_object_mut().expect("filtered to objects above");
    let mut removed_instructions = 0;
    if let Some(slot) = object.get_mut("instructions").and_then(Value::as_array_mut) {
        let before = slot.len();
        slot.retain(|v| !v.as_str().is_some_and(is_pixel_instruction));
        removed_instructions = before - slot.len();
        if slot.is_empty() {
            object.remove("instructions");
        }
    }
    let mut removed_plugins = 0;
    for key in ["plugin", "plugins"] {
        if let Some(slot) = object.get_mut(key).and_then(Value::as_array_mut) {
            let before = slot.len();
            slot.retain(|v| {
                !v.as_str()
                    .is_some_and(|s| is_stale_pixel_plugin(s, config_dir, home))
            });
            removed_plugins += before - slot.len();
        }
    }
    if removed_instructions + removed_plugins > 0
        && !dry_run
        && let Err(e) = install::write_settings(path, &config, false)
    {
        return (
            0,
            0,
            Some(format!("sweep of {} failed: {e}", path.display())),
        );
    }
    (removed_instructions, removed_plugins, None)
}

/// `pixel uninstall` step: take the managed block out of `AGENTS.md` (the
/// file goes when nothing else was in it), and drop the earlier mechanism's
/// `instructions` entries.
pub(crate) fn remove_opencode(
    config_dir: &Path,
    home: &Path,
    dry_run: bool,
) -> Result<InstallStep> {
    let agents = agents_md_path(config_dir);
    let step = |status, summary: String| InstallStep {
        id: "opencode-agents-md".into(),
        status,
        summary,
        detail: Some(format!("agents={}", agents.display())),
    };
    let mut removed = Vec::new();
    // The plugin goes only when pixel wrote it. A file carrying the managed
    // marker is ours; anything else under that name is the user's, and
    // deleting it would take work this step never created.
    for plugin_file in [
        config_dir.join(PLUGIN_PATH),
        config_dir.join(BRIEF_PLUGIN_PATH),
    ] {
        if !(plugin_file.is_file() && is_managed_plugin(&plugin_file)) {
            continue;
        }
        if dry_run {
            removed.push(format!("plugin {}", plugin_file.display()));
        } else if let Err(e) = fs::remove_file(&plugin_file) {
            return Err(e.into());
        } else {
            removed.push(format!("plugin {}", plugin_file.display()));
        }
    }
    if agents.is_file() {
        let content = fs::read_to_string(&agents)?;
        let stripped = config::strip_managed_block(&content);
        if stripped != content {
            if dry_run {
                return Ok(step(
                    CheckStatus::Green,
                    install::dry_run_summary(true, "would strip the AGENTS.md managed block"),
                ));
            }
            if stripped.trim().is_empty() {
                fs::remove_file(&agents)?;
                removed.push(format!("{} (block was the whole file)", agents.display()));
            } else {
                install::write_atomically(&agents, &stripped)?;
                removed.push(format!("block from {}", agents.display()));
            }
        }
    }
    let (swept, plugins, _) = sweep_config(&config_path(config_dir), home, dry_run);
    if swept + plugins > 0 {
        removed.push(format!("{swept} instruction + {plugins} plugin entries"));
    }
    let summary = if removed.is_empty() {
        "no pixel OpenCode config — nothing to remove".to_string()
    } else {
        format!("removed {}", removed.join(" and "))
    };
    Ok(step(CheckStatus::Green, summary))
}

/// The prompt-submission evidence brief for OpenCode: `plugins/` auto-loads
/// `pixel-brief.js`, whose `chat.message` hook runs `pixel brief` and
/// prepends the result as a synthetic part. Written only where the config
/// directory already exists; a managed earlier copy is replaced, a foreign
/// file under that name is never touched.
pub(crate) fn install_brief(config_dir: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let plugin = config_dir.join(BRIEF_PLUGIN_PATH);
    let step = |status, summary: String| InstallStep {
        id: "opencode-brief".into(),
        status,
        summary,
        detail: Some(format!("plugin={}", plugin.display())),
    };
    if !config_dir.is_dir() {
        return Ok(step(
            CheckStatus::Green,
            "OpenCode is not configured; skipped the prompt brief plugin".into(),
        ));
    }
    if plugin.exists() && !is_managed_plugin(&plugin) {
        return Ok(step(
            CheckStatus::Yellow,
            format!(
                "{} exists and is not Pixel's; left untouched",
                plugin.display()
            ),
        ));
    }
    let source = include_str!("../assets/opencode-brief.js")
        .replace("__PIXEL_BIN__", &format!("{:?}", exe.display().to_string()))
        .replace("__MANAGED_BEGIN__", config::MANAGED_BEGIN)
        .replace("__MANAGED_END__", config::MANAGED_END);
    let current = fs::read_to_string(&plugin).is_ok_and(|text| text == source);
    if current {
        return Ok(step(
            CheckStatus::Green,
            "the OpenCode prompt brief plugin is current".into(),
        ));
    }
    if dry_run {
        return Ok(step(
            CheckStatus::Green,
            format!(
                "would write the prompt brief plugin to {}",
                plugin.display()
            ),
        ));
    }
    if let Some(dir) = plugin.parent() {
        fs::create_dir_all(dir)?;
    }
    install::write_atomically(&plugin, &source)?;
    Ok(step(
        CheckStatus::Green,
        "wrote the OpenCode prompt brief plugin".into(),
    ))
}

/// `pixel doctor` check: OpenCode keeps its native tools, so neither the
/// managed block in `AGENTS.md` nor a guard plugin Pixel wrote may remain.
/// Green-skips when OpenCode has no config directory; a plugin under that
/// name that Pixel did not write is the user's.
pub(crate) fn check_opencode(config_dir: &Path) -> std::result::Result<(String, Value), String> {
    let agents = agents_md_path(config_dir);
    let plugin = config_dir.join(PLUGIN_PATH);
    let detail = serde_json::json!({
        "path": agents.display().to_string(),
        "plugin": plugin.display().to_string(),
    });
    if !config_dir.is_dir() {
        return Ok((
            "OpenCode config directory not present — skipping".into(),
            detail,
        ));
    }
    let mut left = Vec::new();
    if fs::read_to_string(&agents).is_ok_and(|content| content.contains(config::MANAGED_BEGIN)) {
        left.push(format!("the Pixel block in {}", agents.display()));
    }
    if plugin.is_file() && is_managed_plugin(&plugin) {
        left.push(format!("the guard plugin {}", plugin.display()));
    }
    if !left.is_empty() {
        return Err(format!(
            "retired Pixel OpenCode integration remains: {} — run `pixel install` to remove it",
            left.join(" and ")
        ));
    }
    Ok((
        "no Pixel prompt or plugin; OpenCode keeps its native tools".into(),
        detail,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique temp dir that removes itself when the test ends — `scratch`
    /// without this left every fixture under `/tmp` forever.
    struct Scratch(PathBuf);

    impl std::ops::Deref for Scratch {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "pixel-opencode-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn config_dir(home: &Path) -> PathBuf {
        opencode_config_dir(home, true)
    }

    /// A plugin as an earlier release wrote it: the managed marker, then code.
    fn write_managed_plugin(dir: &Path) {
        let plugin = dir.join(PLUGIN_PATH);
        fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        fs::write(
            &plugin,
            format!(
                "// {}\nexport default async () => ({{}});\n// {}\n",
                config::MANAGED_BEGIN,
                config::MANAGED_END
            ),
        )
        .unwrap();
    }

    /// The prompt block an earlier release put in `AGENTS.md`.
    fn write_managed_agents_md(dir: &Path) {
        fs::write(
            dir.join(AGENTS_MD_FILE),
            format!(
                "{}\nprompt\n{}\n",
                config::MANAGED_BEGIN,
                config::MANAGED_END
            ),
        )
        .unwrap();
    }

    /// Uninstall takes the plugin out, and only pixel's: a file the user put
    /// at that path is left alone even though the name is ours. Deleting it
    /// would remove work this step never created, and the marker is the only
    /// evidence that distinguishes the two.
    #[test]
    fn uninstall_removes_the_managed_plugin_and_spares_a_foreign_one() {
        let home = scratch("plugin-remove");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        write_managed_plugin(&dir);
        assert!(dir.join(PLUGIN_PATH).is_file());

        let step = remove_opencode(&dir, &home, false).unwrap();
        assert!(step.summary.contains("plugin"), "{}", step.summary);
        assert!(!dir.join(PLUGIN_PATH).exists());

        // A file of the user's own at the same path survives.
        let plugin = dir.join(PLUGIN_PATH);
        fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        fs::write(&plugin, "// mine\n").unwrap();
        let step = remove_opencode(&dir, &home, false).unwrap();
        assert!(!step.summary.contains("plugin"), "{}", step.summary);
        assert!(plugin.is_file(), "a plugin pixel did not write was deleted");
        assert_eq!(fs::read_to_string(&plugin).unwrap(), "// mine\n");
    }

    #[test]
    fn remove_strips_the_block_and_deletes_a_block_only_file() {
        let home = scratch("remove");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        write_managed_agents_md(&dir);
        // File holding only the block: removed entirely.
        let step = remove_opencode(&dir, &home, false).unwrap();
        assert_eq!(step.status, CheckStatus::Green, "{}", step.summary);
        assert!(!dir.join(AGENTS_MD_FILE).exists());
        // File holding user text + block: text survives.
        fs::write(
            dir.join(AGENTS_MD_FILE),
            format!(
                "mine\n{}\nprompt\n{}\n",
                config::MANAGED_BEGIN,
                config::MANAGED_END
            ),
        )
        .unwrap();
        let step = remove_opencode(&dir, &home, false).unwrap();
        assert_eq!(step.status, CheckStatus::Green, "{}", step.summary);
        assert_eq!(
            fs::read_to_string(dir.join(AGENTS_MD_FILE)).unwrap(),
            "mine\n"
        );
        // Nothing left: green no-op.
        let step = remove_opencode(&dir, &home, false).unwrap();
        assert!(
            step.summary.contains("nothing to remove"),
            "{}",
            step.summary
        );
    }

    #[test]
    fn remove_also_drops_leftover_instructions_entries() {
        let home = scratch("remove-instructions");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(OPENCODE_CONFIG_FILE),
            serde_json::to_string_pretty(&serde_json::json!({
                "instructions": [
                    format!("{}/.local/share/pixel/agent-prompt.md", home.display())
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let step = remove_opencode(&dir, &home, false).unwrap();
        assert!(
            step.summary.contains("1 instruction + 0 plugin entries"),
            "{}",
            step.summary
        );
        let config: Value =
            serde_json::from_str(&fs::read_to_string(dir.join(OPENCODE_CONFIG_FILE)).unwrap())
                .unwrap();
        assert!(config.get("instructions").is_none(), "{config}");

        // Plugin-only removal reports its own count.
        fs::write(
            dir.join(OPENCODE_CONFIG_FILE),
            serde_json::to_string_pretty(&serde_json::json!({
                "plugin": ["~/nowhere/pixel.mjs"]
            }))
            .unwrap(),
        )
        .unwrap();
        let step = remove_opencode(&dir, &home, false).unwrap();
        assert!(
            step.summary.contains("0 instruction + 1 plugin entries"),
            "{}",
            step.summary
        );
    }

    #[test]
    fn config_dir_honours_xdg_only_on_a_real_install() {
        let home = Path::new("/home/u");
        let xdg = Some(std::ffi::OsString::from("/xdg"));
        assert_eq!(
            resolve_config_dir(home, false, xdg.clone()),
            PathBuf::from("/xdg/opencode")
        );
        // An explicit --home is a test fixture, not the user's machine:
        // XDG must not leak into it.
        assert_eq!(
            resolve_config_dir(home, true, xdg),
            home.join(".config/opencode")
        );
        assert_eq!(
            resolve_config_dir(home, false, None),
            home.join(".config/opencode")
        );
        // An empty XDG variable is unset, not a root-relative path.
        assert_eq!(
            resolve_config_dir(home, false, Some(std::ffi::OsString::new())),
            home.join(".config/opencode")
        );
    }
}
