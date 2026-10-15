// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The OpenCode integration under `~/.config/opencode/`.
//!
//! OpenCode keeps its native tools. `pixel setup` writes one file there: the
//! evidence-brief plugin `plugins/pixel-brief.js`, recognised by the managed
//! marker it carries ([`config::MANAGED_BEGIN`]); a file at that path without
//! the marker is the user's and is never touched.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config;
use crate::install::{self, CheckStatus, InstallStep, Result};

/// The evidence-brief plugin `pixel setup` deploys; the `plugins`
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

/// `pixel setup --remove` step: delete the brief plugin, only when pixel wrote
/// it.
pub(crate) fn remove_opencode(config_dir: &Path, dry_run: bool) -> Result<InstallStep> {
    let plugin = config_dir.join(BRIEF_PLUGIN_PATH);
    let step = |summary: String| InstallStep {
        id: "opencode-brief".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(format!("plugin={}", plugin.display())),
    };
    if !(plugin.is_file() && is_managed_plugin(&plugin)) {
        return Ok(step("no pixel OpenCode plugin — nothing to remove".into()));
    }
    if !dry_run {
        fs::remove_file(&plugin)?;
    }
    Ok(step(format!("removed plugin {}", plugin.display())))
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

    /// The brief plugin as `pixel setup` writes it: the managed marker, then code.
    fn write_managed_plugin(dir: &Path) {
        let plugin = dir.join(BRIEF_PLUGIN_PATH);
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
        assert!(dir.join(BRIEF_PLUGIN_PATH).is_file());

        let step = remove_opencode(&dir, false).unwrap();
        assert!(step.summary.contains("removed plugin"), "{}", step.summary);
        assert!(!dir.join(BRIEF_PLUGIN_PATH).exists());

        // A file of the user's own at the same path survives.
        let plugin = dir.join(BRIEF_PLUGIN_PATH);
        fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        fs::write(&plugin, "// mine\n").unwrap();
        let step = remove_opencode(&dir, false).unwrap();
        assert!(
            step.summary.contains("nothing to remove"),
            "{}",
            step.summary
        );
        assert!(plugin.is_file(), "a plugin pixel did not write was deleted");
        assert_eq!(fs::read_to_string(&plugin).unwrap(), "// mine\n");
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
