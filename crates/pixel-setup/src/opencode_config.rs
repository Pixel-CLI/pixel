// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The OpenCode integration under `~/.config/opencode/`.
//!
//! OpenCode keeps its native tools. `pixel setup` writes one file there, the
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

/// The retired TUI plugin that toasted the metrics blocks (#1301); the brief
/// plugin now posts them as chat messages. Setup and uninstall delete pixel's
/// copy so the toasts stop; a foreign file there is left alone.
const RETIRED_METRICS_PLUGIN_PATH: &str = "plugins/pixel-brief-metrics/tui.js";

/// Deletes pixel's copy of the retired metrics plugin and then its directory
/// when nothing else is in it. Whether a file was deleted.
fn remove_retired_metrics_plugin(config_dir: &Path) -> Result<bool> {
    let path = config_dir.join(RETIRED_METRICS_PLUGIN_PATH);
    if !(path.is_file() && is_managed_plugin(&path)) {
        return Ok(false);
    }
    fs::remove_file(&path)?;
    if let Some(dir) = path.parent() {
        let _ = fs::remove_dir(dir);
    }
    Ok(true)
}

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
    let retired = config_dir.join(RETIRED_METRICS_PLUGIN_PATH);
    let managed = |path: &Path| path.is_file() && is_managed_plugin(path);
    if !managed(&plugin) && !managed(&retired) {
        return Ok(step("no pixel OpenCode plugin — nothing to remove".into()));
    }
    if !dry_run {
        if managed(&plugin) {
            fs::remove_file(&plugin)?;
        }
        remove_retired_metrics_plugin(config_dir)?;
    }
    Ok(step(format!("removed plugin {}", plugin.display())))
}

/// The prompt-submission evidence brief for OpenCode: `plugins/` auto-loads
/// `pixel-brief.js`, a V2 plugin whose `prompt` hook runs `pixel brief`, whose
/// `context` hook adds the result to the turn's first request as a system part,
/// and whose non-resuming synthetic messages show the brief's and each `pixel`
/// call's metrics block in chat (the V1 `chat.message` export no longer
/// loads). Written only where the config directory already exists; a managed
/// earlier copy, V1 included, is replaced, a foreign file under that name is
/// never touched. Pixel's copy of the retired toast plugin is deleted.
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
    let retired = config_dir.join(RETIRED_METRICS_PLUGIN_PATH);
    let retired_stale = retired.is_file() && is_managed_plugin(&retired);
    if !retired_stale && fs::read_to_string(&plugin).is_ok_and(|text| text == source) {
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
    remove_retired_metrics_plugin(config_dir)?;
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
        assert!(
            source.contains("\"--metrics\", \"on\"")
                && source.contains("metricsBlock")
                && source.contains("ctx.session.synthetic")
                && source.contains("description: text, resume: false"),
            "the managed plugin must expose brief and finalized Pixel metrics in chat"
        );
        assert!(
            source.contains("ctx.tool.hook(\"execute.after\"")
                && source.contains("event.result")
                && source.contains("event.error"),
            "the V2 tool hook must relay finalized Pixel metrics"
        );
        assert!(
            !source.contains("session.prompt")
                && !source.contains("execution-brief")
                && !source.contains("session.idle"),
            "evidence must not send a model-directed retry or task-independent retrieval"
        );
        // Ordering the in-flight briefs is per session. A process-wide counter
        // (the first V2 asset's `seen`) let a prompt in one session discard
        // another session's brief still in flight, so the sequence lives with
        // the session key and the newest prompt's guard reads it back.
        assert!(
            !source.contains("let seen")
                && source.contains("sessions.get(event.sessionID)?.seq !== seq"),
            "the newest-prompt guard must be per session, not process-wide"
        );
    }

    /// Install writes the brief plugin, marked managed, is idempotent, and
    /// deletes pixel's copy of the retired toast plugin (#1301) while sparing
    /// a foreign file at that path; uninstall removes both of pixel's files.
    #[test]
    fn install_writes_the_brief_and_retires_the_toast_plugin() {
        let home = scratch("metrics-install");
        let dir = config_dir(&home);
        let retired = dir.join(RETIRED_METRICS_PLUGIN_PATH);
        fs::create_dir_all(retired.parent().unwrap()).unwrap();
        fs::write(&retired, format!("// {}\n", config::MANAGED_BEGIN)).unwrap();
        let exe = Path::new("/bin/pixel");

        let step = install_brief(&dir, exe, false).unwrap();
        assert_eq!(step.summary, "wrote the OpenCode prompt brief plugin");
        let text = fs::read_to_string(dir.join(BRIEF_PLUGIN_PATH)).unwrap();
        assert!(
            text.contains(config::MANAGED_BEGIN),
            "the brief is unmarked"
        );
        assert!(!text.contains("__MANAGED_"), "the brief kept a placeholder");
        assert!(!retired.exists(), "pixel's toast plugin survived setup");
        assert!(!dir.join("plugins/pixel-brief-metrics").exists());
        let again = install_brief(&dir, exe, false).unwrap();
        assert_eq!(again.summary, "the OpenCode prompt brief plugin is current");

        fs::create_dir_all(retired.parent().unwrap()).unwrap();
        fs::write(&retired, "// mine\n").unwrap();
        let step = install_brief(&dir, exe, false).unwrap();
        assert!(matches!(step.status, CheckStatus::Green), "{step:?}");
        assert_eq!(fs::read_to_string(&retired).unwrap(), "// mine\n");

        fs::write(&retired, format!("// {}\n", config::MANAGED_BEGIN)).unwrap();
        remove_opencode(&dir, false).unwrap();
        assert!(!dir.join(BRIEF_PLUGIN_PATH).exists());
        assert!(!retired.exists());
    }

    /// The metrics reach the user as chat messages and never the brief text a
    /// second time: OpenCode sends a synthetic message to the model as a user
    /// message. Only a `pixel` call's own trailing footer is relayed.
    #[test]
    fn metrics_go_to_chat_without_repeating_the_brief() {
        let brief = include_str!("../assets/opencode-brief.js");
        assert_eq!(
            brief.matches("showEvidence(ctx, event.sessionID,").count(),
            2,
            "one message for the brief's metrics, one per pixel call"
        );
        assert!(
            brief.contains("void showEvidence(ctx, event.sessionID, state.metrics);")
                && brief.contains("if (footer) await showEvidence(ctx, event.sessionID, footer);"),
            "only metrics blocks become chat messages"
        );
        assert!(
            brief.contains("text: `${state.brief}\\n${NOTICE}`"),
            "the brief text reaches the model once, as the system part"
        );
        assert!(
            brief.contains("PIXEL_COMMAND.test(command)") && brief.contains("\\s*$/u;"),
            "only a pixel call's own footer, anchored at the end of its output"
        );
        assert!(
            !brief.contains("toast") && !brief.contains("os.tmpdir()"),
            "no toast handoff remains"
        );
    }
}
