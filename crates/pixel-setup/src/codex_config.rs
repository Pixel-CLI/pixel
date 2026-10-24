// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Codex integration: the hooks in `~/.codex/hooks.json`. Codex keeps its
//! review of them in `~/.codex/config.toml`; setup reads that state to report
//! it (`pixel doctor`) and never writes it, so a new hook stays unreviewed until
//! the user runs `/hooks`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::hosts::Host;
use crate::install::{CheckStatus, InstallStep, Result, dry_run_summary};

/// `hooks.json`, relative to the Codex home — Codex's hook registry, in the
/// nested `hooks.<Event>[{hooks: [{command}]}]` shape Claude Code settings
/// also use. (Distinct from `config::CODEX_HOOKS_FILE`, which is
/// home-relative.)
pub const HOOKS_FILE: &str = "hooks.json";

/// The Codex home directory: `$CODEX_HOME` when the caller did not pin a
/// home directory (a real `pixel setup`, where Codex itself honours the
/// variable), `<home>/.codex` otherwise (tests and explicit overrides, which
/// must not depend on the invoking environment).
pub(crate) fn codex_home(home: &Path, home_was_explicit: bool) -> PathBuf {
    if !home_was_explicit
        && let Some(dir) = std::env::var_os("CODEX_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    home.join(".codex")
}

pub(crate) fn read_hooks(path: &Path) -> std::result::Result<serde_json::Value, String> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| format!("{} does not parse as JSON: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({"hooks": {}})),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn write_hooks(path: &Path, value: &serde_json::Value) -> Result<()> {
    crate::install::write_settings(path, value, false)?;
    Ok(())
}

/// `pixel setup` step: keep Codex's lifecycle hooks installed. Only PIXEL's own
/// entries are replaced, each in the group slot the
/// previous one held, because Codex keys a hook's review by its position.
pub(crate) fn install_hooks(
    codex_home: &Path,
    exe: &Path,
    prompt_parts: u8,
    dry_run: bool,
) -> Result<InstallStep> {
    let path = codex_home.join(HOOKS_FILE);
    let detail = Some(format!("path={} lifecycle hooks", path.display()));
    let step = |status, summary: String| InstallStep {
        id: "hooks.codex".into(),
        status,
        summary,
        detail: detail.clone(),
    };
    let mut value = match read_hooks(&path) {
        Ok(v) => v,
        Err(e) => return Ok(step(CheckStatus::Red, format!("{e} — not touched"))),
    };
    let hooks = value
        .as_object_mut()
        .map(|o| o.entry("hooks").or_insert_with(|| serde_json::json!({})))
        .and_then(serde_json::Value::as_object_mut);
    let Some(hooks) = hooks else {
        return Ok(step(
            CheckStatus::Red,
            format!(
                "`hooks` in {} is not an object — not touched",
                path.display()
            ),
        ));
    };
    let before = hooks.clone();
    if let Err(error) = crate::routing::merge_hooks(hooks, Host::Codex, exe, prompt_parts) {
        return Ok(step(CheckStatus::Red, format!("{error} — not touched")));
    }
    if *hooks == before {
        return Ok(step(
            CheckStatus::Green,
            format!("verified hooks in {}", path.display()),
        ));
    }
    let summary = format!(
        "{} hooks in {}",
        if path.is_file() {
            "updated"
        } else {
            "installed"
        },
        path.display()
    );
    if dry_run {
        return Ok(step(CheckStatus::Green, dry_run_summary(true, &summary)));
    }
    write_hooks(&path, &value)?;
    Ok(step(CheckStatus::Green, summary))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_codex_home(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pixel-codex-hooks-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn hook_events(home: &Path) -> serde_json::Value {
        let text = fs::read_to_string(home.join(HOOKS_FILE)).unwrap();
        serde_json::from_str::<serde_json::Value>(&text).unwrap()["hooks"].clone()
    }

    #[test]
    fn the_codex_home_is_codex_home_unless_the_caller_pinned_a_home() {
        let home = Path::new("/h");
        assert_eq!(codex_home(home, true), PathBuf::from("/h/.codex"));
    }

    #[test]
    fn reinstalling_refreshes_the_binary_path_without_duplicating_events() {
        let home = scratch_codex_home("refresh");
        install_hooks(&home, Path::new("/old/pixel"), 1, false).unwrap();
        install_hooks(&home, Path::new("/new/pixel"), 1, false).unwrap();
        let hooks = hook_events(&home);
        let commands = hooks.to_string();
        assert!(commands.contains("/new/pixel"), "{commands}");
        assert!(!commands.contains("/old/pixel"), "{commands}");
        for (event, groups) in hooks.as_object().unwrap() {
            assert_eq!(groups.as_array().unwrap().len(), 1, "{event}: {groups}");
        }
        assert_eq!(
            hooks.as_object().unwrap().len(),
            4,
            "SessionStart, UserPromptSubmit, PostToolUse, Stop"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_second_identical_install_changes_nothing_and_says_so() {
        let home = scratch_codex_home("again");
        let first = install_hooks(&home, Path::new("/x/pixel"), 3, false).unwrap();
        assert!(first.summary.starts_with("installed"), "{}", first.summary);
        let bytes = fs::read(home.join(HOOKS_FILE)).unwrap();
        let second = install_hooks(&home, Path::new("/x/pixel"), 3, false).unwrap();
        assert!(second.summary.starts_with("verified"), "{}", second.summary);
        assert_eq!(fs::read(home.join(HOOKS_FILE)).unwrap(), bytes);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_dry_run_reports_the_install_and_writes_nothing() {
        let home = scratch_codex_home("dry");
        let step = install_hooks(&home, Path::new("/x/pixel"), 1, true).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        assert!(step.summary.starts_with("[dry-run]"), "{}", step.summary);
        assert!(!home.join(HOOKS_FILE).exists());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn install_keeps_foreign_hooks_and_replaces_only_its_own() {
        let home = scratch_codex_home("foreign");
        let existing = serde_json::json!({"hooks": {
            "Stop": [
                {"hooks": [{"type": "command", "command": "/x/pixel run-hook task-event --provider codex --event stop"}]},
                {"hooks": [{"type": "command", "command": "user-check"}]},
            ],
            "PostToolUse": [
                {"hooks": [{"type": "command", "command": "/x/pixel run-hook metrics --provider codex"}]},
            ],
        }});
        fs::write(home.join(HOOKS_FILE), existing.to_string()).unwrap();
        install_hooks(&home, Path::new("/x/pixel"), 1, false).unwrap();
        let hooks = hook_events(&home);
        assert!(hooks.to_string().contains("/x/pixel run-hook"), "{hooks}");
        let stop: Vec<_> = hooks["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .map(|group| group["hooks"][0]["command"].as_str().unwrap().to_owned())
            .collect();
        assert!(stop.contains(&"user-check".to_owned()), "{stop:?}");
        assert!(
            stop.iter()
                .any(|command| command.contains("hook stop --host codex"))
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn install_refuses_unparseable_hooks_json() {
        let home = scratch_codex_home("broken");
        fs::write(home.join(HOOKS_FILE), "not json").unwrap();
        let step = install_hooks(&home, Path::new("/x"), 1, false).unwrap();
        assert_eq!(step.status, CheckStatus::Red);
        assert_eq!(
            fs::read_to_string(home.join(HOOKS_FILE)).unwrap(),
            "not json",
            "an unparseable file is never rewritten"
        );
        fs::write(home.join(HOOKS_FILE), "{\"hooks\": [1]}").unwrap();
        let step = install_hooks(&home, Path::new("/x"), 1, false).unwrap();
        assert_eq!(
            step.status,
            CheckStatus::Red,
            "a non-object hooks key is refused, not silently replaced"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn install_reports_an_unreadable_hooks_json() {
        let home = scratch_codex_home("unreadable");
        // A directory where the file is expected fails the read for every
        // user including root — and is not "absent": it must come back Red,
        // never Ok-treated-as-empty.
        fs::create_dir(home.join(HOOKS_FILE)).unwrap();
        let step = install_hooks(&home, Path::new("/x"), 1, false).unwrap();
        assert_eq!(step.status, CheckStatus::Red, "{}", step.summary);
        assert!(step.summary.contains("not touched"), "{}", step.summary);
        let _ = fs::remove_dir_all(&home);
    }
}
