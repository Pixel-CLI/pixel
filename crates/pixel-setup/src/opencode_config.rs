// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The OpenCode integration under `~/.config/opencode/`.
//!
//! OpenCode keeps its native tools. `pixel setup` writes two files there: the
//! evidence-brief plugin `plugins/pixel-brief.js` and the TUI plugin
//! `plugins/pixel-brief-metrics/tui.js` that shows Pixel's metrics blocks to
//! the user, each recognised by the managed marker it carries
//! ([`config::MANAGED_BEGIN`]); a file at either path without the marker is
//! the user's and is never touched.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config;
use crate::install::{self, CheckStatus, InstallStep, Result};

/// The evidence-brief plugin `pixel setup` deploys; the `plugins`
/// directory auto-loads it, so no registration entry is needed.
const BRIEF_PLUGIN_PATH: &str = "plugins/pixel-brief.js";

/// The TUI plugin that toasts the brief's and each `pixel` call's metrics
/// block; a directory plugin, which OpenCode loads as a TUI plugin by its
/// `tui.js` entry.
const METRICS_PLUGIN_PATH: &str = "plugins/pixel-brief-metrics/tui.js";

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
    let metrics = config_dir.join(METRICS_PLUGIN_PATH);
    let managed: Vec<&Path> = [&plugin, &metrics]
        .into_iter()
        .filter(|path| path.is_file() && is_managed_plugin(path))
        .map(PathBuf::as_path)
        .collect();
    if managed.is_empty() {
        return Ok(step("no pixel OpenCode plugin — nothing to remove".into()));
    }
    if !dry_run {
        for path in &managed {
            fs::remove_file(path)?;
        }
        // The metrics plugin's own directory, only once nothing else is in it.
        if let Some(dir) = metrics.parent() {
            let _ = fs::remove_dir(dir);
        }
    }
    Ok(step(format!("removed plugin {}", plugin.display())))
}

/// The prompt-submission evidence brief for OpenCode: `plugins/` auto-loads
/// `pixel-brief.js`, a V2 plugin whose `prompt` hook runs `pixel brief` and
/// whose `context` hook adds the result to the turn's first request as a
/// system part (the V1 `chat.message` export no longer loads). Written only
/// where the config directory already exists; a managed earlier copy, V1
/// included, is replaced, a foreign file under that name is never touched.
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
    let managed = |asset: &str| {
        asset
            .replace("__PIXEL_BIN__", &format!("{:?}", exe.display().to_string()))
            .replace("__MANAGED_BEGIN__", config::MANAGED_BEGIN)
            .replace("__MANAGED_END__", config::MANAGED_END)
    };
    let metrics = config_dir.join(METRICS_PLUGIN_PATH);
    if metrics.exists() && !is_managed_plugin(&metrics) {
        return Ok(step(
            CheckStatus::Yellow,
            format!(
                "{} exists and is not Pixel's; left untouched",
                metrics.display()
            ),
        ));
    }
    let files = [
        (
            &plugin,
            managed(include_str!("../assets/opencode-brief.js")),
        ),
        (
            &metrics,
            managed(include_str!("../assets/opencode-metrics-tui.js")),
        ),
    ];
    let stale: Vec<_> = files
        .iter()
        .filter(|(path, source)| fs::read_to_string(path).map_or(true, |text| text != *source))
        .collect();
    if stale.is_empty() {
        return Ok(step(
            CheckStatus::Green,
            "the OpenCode prompt brief plugin is current".into(),
        ));
    }
    if dry_run {
        return Ok(step(
            CheckStatus::Green,
            format!(
                "would write the prompt brief and metrics plugins to {}",
                plugin.display()
            ),
        ));
    }
    for (path, source) in stale {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        install::write_atomically(path, source)?;
    }
    Ok(step(
        CheckStatus::Green,
        "wrote the OpenCode prompt brief and metrics plugins".into(),
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

    /// The plugin asset must load in OpenCode V2: a default export with an
    /// `id` and a `setup` that registers the prompt hook. V2 refuses a module
    /// without that default ("Plugin must export a default definition with an
    /// id and an effect or setup function"), which silently drops the brief,
    /// so the shape is pinned here rather than only in the asset.
    #[test]
    fn brief_asset_is_a_v2_plugin_with_the_prompt_hook() {
        let source = include_str!("../assets/opencode-brief.js");
        assert!(
            source.contains("export default {") && source.contains("async setup(ctx)"),
            "the V2 loader accepts only a default {{ id, setup }} definition"
        );
        assert!(
            source.contains(r#"ctx.session.hook("prompt""#),
            "V2 replaced the V1 chat.message hook with session.hook(\"prompt\")"
        );
        // The V1 entrypoint and its Bun shell template: both are recognized by
        // the V2 loader and by this test as dead legacy, and the two
        // substrings are split so this test's own text cannot match them.
        assert!(
            !source.contains(&format!("export const {}{}", "Pixel", "Brief")),
            "the V1 named export never runs in V2"
        );
        assert!(
            !source.contains("$`"),
            "V2 removed the Bun shell helper; the plugin spawns pixel itself"
        );
        assert!(
            source.contains("__PIXEL_BIN__")
                && source.contains("__MANAGED_BEGIN__")
                && source.contains("__MANAGED_END__"),
            "install replaces all three placeholders; without the managed \
             marker uninstall would treat the file as the user's"
        );
        // The brief bounds its own window and printing at six seconds
        // (`BRIEF_WINDOW_MAX_MS` plus slack); a shield below that bound kills
        // briefs still inside their budget, and 1_500 ms did exactly that. It
        // matches `BRIEF_TIMEOUT_MS` in `pi-brief.ts`, pinned the same way.
        assert_eq!(source.matches("const BRIEF_TIMEOUT_MS = 6_500;").count(), 1);
        assert!(
            source.contains("}, BRIEF_TIMEOUT_MS);"),
            "the timer must fire the named bound, not a literal"
        );
        // Ordering the in-flight briefs is per session. A process-wide counter
        // (the first V2 asset's `seen`) let a prompt in one session discard
        // another session's brief still in flight, so the sequence lives with
        // the session key and the newest prompt's guard reads it back.
        assert!(
            !source.contains("let seen")
                && source.contains("sessions.get(event.sessionID)?.seq === seq"),
            "the newest-prompt guard must be per session, not process-wide"
        );
    }

    /// Install writes the brief plugin and the metrics TUI plugin, both
    /// marked managed, is idempotent, spares a foreign file at the metrics
    /// path, and uninstall removes both plus the metrics directory.
    #[test]
    fn install_writes_both_plugins_and_uninstall_removes_both() {
        let home = scratch("metrics-install");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        let exe = Path::new("/bin/pixel");

        let step = install_brief(&dir, exe, false).unwrap();
        assert!(step.summary.contains("wrote"), "{}", step.summary);
        for path in [BRIEF_PLUGIN_PATH, METRICS_PLUGIN_PATH] {
            let text = fs::read_to_string(dir.join(path)).unwrap();
            assert!(text.contains(config::MANAGED_BEGIN), "{path} is unmarked");
            assert!(!text.contains("__MANAGED_"), "{path} kept a placeholder");
        }
        let again = install_brief(&dir, exe, false).unwrap();
        assert!(again.summary.contains("current"), "{}", again.summary);

        remove_opencode(&dir, false).unwrap();
        assert!(!dir.join(BRIEF_PLUGIN_PATH).exists());
        assert!(!dir.join(METRICS_PLUGIN_PATH).exists());
        assert!(!dir.join("plugins/pixel-brief-metrics").exists());

        let foreign = dir.join(METRICS_PLUGIN_PATH);
        fs::create_dir_all(foreign.parent().unwrap()).unwrap();
        fs::write(&foreign, "// mine\n").unwrap();
        let step = install_brief(&dir, exe, false).unwrap();
        assert!(matches!(step.status, CheckStatus::Yellow), "{step:?}");
        assert_eq!(fs::read_to_string(&foreign).unwrap(), "// mine\n");
        assert!(!dir.join(BRIEF_PLUGIN_PATH).exists());
    }

    /// The metrics path: the brief plugin leaves its `🟩` block, unfiltered,
    /// in the directory the TUI plugin reads once per turn, and the TUI
    /// plugin relays a footer only from a call whose own command ran `pixel`.
    #[test]
    fn metrics_assets_share_the_handoff_and_stay_off_the_model_channel() {
        let brief = include_str!("../assets/opencode-brief.js");
        let tui = include_str!("../assets/opencode-metrics-tui.js");
        let dir = r#"path.join(os.tmpdir(), "pixel-opencode-brief-metrics")"#;
        assert!(
            brief.contains(dir) && tui.contains(dir),
            "both sides must spell the handoff directory the same way"
        );
        assert!(
            !brief.contains(r#""--metrics""#),
            "the brief must keep its metrics block to hand to the user"
        );
        assert!(
            brief.contains(r#"err.indexOf("🟩")"#)
                && brief.contains("text: `${state.brief}\\n${NOTICE}`"),
            "the metrics block never joins the system part"
        );
        assert!(
            tui.contains("export default {")
                && tui.contains(r#"id: "pixel.metrics""#)
                && tui.contains("setup(api)")
                && tui.contains("api.data.listen")
                && tui.contains("api.ui.toast.show"),
            "a V2 TUI plugin: default {{ id, setup }}, event listener, toast"
        );
        assert!(
            tui.contains(r#""session.tool.called""#)
                && tui.contains(r#""session.tool.success""#)
                && tui.contains("PIXEL_COMMAND.test(command)"),
            "only a pixel call's own footer is relayed"
        );
        assert!(
            tui.contains("__MANAGED_BEGIN__") && tui.contains("__MANAGED_END__"),
            "without the marker uninstall would treat the file as the user's"
        );
    }
}
