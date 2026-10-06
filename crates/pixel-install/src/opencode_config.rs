// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! OpenCode integration through `~/.config/opencode/AGENTS.md` and the
//! `~/.config/opencode/plugins/` directory.
//!
//! AGENTS.md is the one mechanism both OpenCode generations honour for
//! global instructions: v1 loads it in the global slot and v2 — where the
//! `instructions` config field is accepted but never resolved — loads only
//! AGENTS.md files. The deployed prompt is embedded between the managed
//! markers, exactly like Pi's `APPEND_SYSTEM.md`, so `pixel install`
//! refreshes it in place while text the user keeps outside the markers
//! survives.
//!
//! One shadow to preserve: on v1 a *new* global AGENTS.md replaces the
//! `~/.claude/CLAUDE.md` fallback (the first matching file wins the global
//! slot). When install creates the file where none existed and a
//! `~/.claude/CLAUDE.md` is present, the file is seeded with that content —
//! the winning file then carries everything the shadowed one had, plus the
//! pixel block. On v2 there is no fallback to shadow, and the copied rules
//! are simply the rules the user wanted anyway.
//!
//! ## Why the plugin directory is written as well
//!
//! The AGENTS.md block is advice, and advice loses: a model reads "use pixel
//! search-content" and then runs `rtk git log` anyway. OpenCode is the only
//! supported host whose plugin hook can *stop* a call — `tool.execute.before`
//! may throw — so it is the only host where the substitutions and refusals
//! `pixel hook guard` already computes are reachable rather than merely
//! suggested. Writing `pixel.js` into `~/.config/opencode/plugins/` is what
//! puts the provider in front of a session: OpenCode auto-loads that
//! directory, so no `opencode.json` edit is needed and a config the user
//! cannot parse cannot stop the guard from loading. The file is `.js`, not
//! `.mjs` — that directory is auto-loaded by extension, and a `.mjs` there is
//! silently ignored on every launch (see `PIXEL_PLUGIN_FILE`).
//!
//! The same step sweeps two stale artifacts out of `opencode.json`:
//! `instructions` entries naming the deployed prompt (written by earlier
//! installs — dead config on v2 and a second copy of the prompt on v1) and
//! `plugin`/`plugins` entries whose `pixel.mjs` target no longer exists —
//! a load failure on every launch, left behind by manual attempts to wire
//! the repo's `.opencode/plugins/pixel.mjs` into a global config. A global
//! `plugin` entry pointing at the deployed file is redundant now that the
//! directory is auto-loaded, but it is left in place: it resolves, so it
//! costs nothing, and removing an entry a user may have written by hand is
//! not this install's business.

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

/// The enforcing plugin, relative to the OpenCode config directory. OpenCode
/// auto-loads every file in this directory, so this path is the whole load
/// path: there is nothing to register and nothing that can fail to register.
///
/// `.js`, and deliberately not `.mjs`. OpenCode's plugin directory documents
/// "JavaScript or TypeScript files", and its auto-loader honours that
/// literally: a `.mjs` file deployed here is silently ignored on every launch,
/// so the guard reads as installed and enforces nothing. Measured on opencode
/// 1.18.34 — the same file, renamed, went from no effect to rewriting calls.
/// The repo's own project-local plugin keeps the `.mjs` name because
/// `opencode.json` registers that one explicitly, where the extension does not
/// decide whether it loads.
const PLUGIN_PATH: &str = "plugins/pixel.js";

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
    let plugin_file = config_dir.join(PLUGIN_PATH);
    if plugin_file.is_file() && is_managed_plugin(&plugin_file) {
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

/// `pixel install` step: put the bundled prompt inside the managed markers
/// of `AGENTS.md`, seeding a new file with `~/.claude/CLAUDE.md` when one
/// exists so a v1 install shadows nothing, then sweep stale pixel entries
/// out of `opencode.json`.
#[cfg(test)]
pub(crate) fn install_opencode(
    config_dir: &Path,
    home: &Path,
    exe: &Path,
    dry_run: bool,
) -> Result<InstallStep> {
    let agents = agents_md_path(config_dir);
    let json = config_path(config_dir);
    let plugin_file_display = config_dir.join(PLUGIN_PATH).display().to_string();
    let detail = Some(format!(
        "agents={} config={} plugin={plugin_file_display}",
        agents.display(),
        json.display()
    ));
    let step = |status, summary: String| InstallStep {
        id: "opencode-agents-md".into(),
        status,
        summary,
        detail: detail.clone(),
    };

    // The managed block ----------------------------------------------------
    // Only NotFound means "no file": an unreadable AGENTS.md is an error to
    // surface, not a file to replace wholesale.
    let existing = match fs::read_to_string(&agents) {
        Ok(content) => Some(content),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let seeded = match &existing {
        Some(content) => content.clone(),
        // A new global AGENTS.md wins the slot ~/.claude/CLAUDE.md fills on
        // v1 — carry that content into it so nothing is shadowed.
        None => match fs::read_to_string(home.join(".claude/CLAUDE.md")) {
            Ok(content) => content,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        },
    };
    let wanted = config::apply_managed_markers(&seeded, install::AGENT_PROMPT_ASSET);
    if existing.as_deref() == Some(wanted.as_str()) {
        // Fall through to the sweep even when the block is already current.
    } else if dry_run {
        return Ok(step(
            CheckStatus::Green,
            install::dry_run_summary(true, "would update AGENTS.md managed block"),
        ));
    } else {
        fs::create_dir_all(config_dir)?;
        install::write_atomically(&agents, &wanted)?;
    }

    // The enforcing plugin --------------------------------------------------
    // Written unconditionally rather than only when the block changed: the
    // two carry different content, so a user who reinstalls after a pixel
    // upgrade must get the new plugin even when their AGENTS.md was already
    // current. `write_if_changed` semantics keep a no-op install a no-op.
    let plugin_file = config_dir.join(PLUGIN_PATH);
    let plugin_wanted = plugin_source(exe);
    let plugin_current = fs::read_to_string(&plugin_file).is_ok_and(|text| text == plugin_wanted);
    let plugin_written = if plugin_current || dry_run {
        false
    } else {
        if let Some(parent) = plugin_file.parent() {
            fs::create_dir_all(parent)?;
        }
        config::backup_if_changing(&plugin_file, plugin_wanted.as_bytes())?;
        fs::write(&plugin_file, &plugin_wanted)?;
        true
    };

    // The sweeps -----------------------------------------------------------
    let (removed_instructions, removed_plugins, sweep_note) = sweep_config(&json, home, dry_run);
    let mut summary = format!(
        "{} pixel block in {}",
        if wanted == seeded {
            "verified"
        } else {
            "installed"
        },
        agents.display()
    );
    summary.push_str(&format!(
        "; {} guard plugin at {}",
        if plugin_written { "wrote" } else { "verified" },
        plugin_file.display()
    ));
    if removed_instructions + removed_plugins > 0 {
        summary.push_str(&format!(
            "; swept {removed_instructions} instruction + {removed_plugins} plugin entr{}",
            if removed_instructions + removed_plugins == 1 {
                "y"
            } else {
                "ies"
            }
        ));
    }
    if let Some(note) = sweep_note {
        summary.push_str(&format!("; {note}"));
    }
    Ok(step(CheckStatus::Green, summary))
}

/// The plugin source with the installing executable and the managed markers
/// substituted in, matching how the pi guard is materialised.
#[cfg(test)]
pub(crate) fn plugin_source(exe: &Path) -> String {
    include_str!("../assets/opencode-pixel.js")
        .replace("__PIXEL_BIN__", &format!("{:?}", exe.display().to_string()))
        .replace("__MANAGED_BEGIN__", config::MANAGED_BEGIN)
        .replace("__MANAGED_END__", config::MANAGED_END)
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

    /// A stable stand-in for the installing executable. The plugin embeds this
    /// path, so tests need one value to compare against rather than
    /// `std::env::current_exe`, which differs per test binary.
    fn exe() -> PathBuf {
        PathBuf::from("/usr/local/bin/pixel")
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
        install_opencode(&dir, &home, &exe(), false).unwrap();
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
        install_opencode(&dir, &home, &exe(), false).unwrap();
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
