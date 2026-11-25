// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! What earlier releases wrote into an agent's configuration and this one
//! neither uses nor removes.
//!
//! `pixel setup` stopped cleaning up after earlier releases in #965, and a
//! hook verb the binary does not know exits 0, so such an entry never fails
//! a host. It still costs something: a retired hook is a process launched
//! for nothing on every event, and a managed instruction block is read by
//! the agent in every session and names commands the binary no longer has.
//! This module only finds them, for `pixel doctor` to name; deleting them
//! stays the user's edit.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{MANAGED_BEGIN, MANAGED_END};
use crate::routing::{self, Provider};

/// Pi's auto-loaded prompt file, where releases before 0.8 kept their block.
const PI_APPEND_FILE: &str = "APPEND_SYSTEM.md";

/// One thing an earlier release left in `path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Leftover {
    /// The file that holds it.
    pub path: PathBuf,
    /// What it is, in words a user can search the file for.
    pub what: String,
}

/// Bytes of the managed block in `text`, from its begin marker through its
/// end marker, or to the end of the text when the end marker is missing.
/// `None` without a begin marker.
pub(crate) fn managed_block_len(text: &str) -> Option<usize> {
    let begin = text.find(MANAGED_BEGIN)?;
    let block = &text[begin..];
    Some(
        block
            .find(MANAGED_END)
            .map_or(block.len(), |end| end + MANAGED_END.len()),
    )
}

/// The retired hooks of one settings file, when it holds any. A file that is
/// missing or does not parse has no leftover to name: the hook checks report
/// those.
fn retired_hooks(path: PathBuf, exe: &Path) -> Option<Leftover> {
    if !path.is_file() {
        return None;
    }
    let value = crate::install::read_settings(&path).ok()?;
    let retired = routing::retired_pixel_hooks(&value, exe);
    (!retired.is_empty()).then(|| Leftover {
        what: format!(
            "{} retired hook(s) this binary ignores: {}",
            retired.len(),
            retired.join(", ")
        ),
        path,
    })
}

/// Everything an earlier release left for the agents of `home`: retired
/// hooks in the Claude, Devin and Codex hook files, and the managed
/// instruction block in Codex's `developer_instructions` and in Pi's
/// `APPEND_SYSTEM.md`. In that order; empty on a current install.
pub(crate) fn find(
    home: &Path,
    codex_home: &Path,
    pi_agent_dir: &Path,
    exe: &Path,
) -> Vec<Leftover> {
    let mut found: Vec<Leftover> = [
        Provider::Claude.path(home),
        Provider::Devin.path(home),
        codex_home.join(crate::codex_config::HOOKS_FILE),
    ]
    .into_iter()
    .filter_map(|path| retired_hooks(path, exe))
    .collect();
    if let Some(bytes) = crate::codex_config::retired_instructions_block(codex_home) {
        found.push(Leftover {
            path: codex_home.join(crate::codex_config::CODEX_CONFIG_FILE),
            what: format!(
                "a {MANAGED_BEGIN} block of {bytes} bytes in `developer_instructions`, read by Codex in every session"
            ),
        });
    }
    let append = pi_agent_dir.join(PI_APPEND_FILE);
    if let Some(bytes) = fs::read_to_string(&append)
        .ok()
        .as_deref()
        .and_then(managed_block_len)
    {
        found.push(Leftover {
            path: append,
            what: format!("a {MANAGED_BEGIN} block of {bytes} bytes, read by Pi in every session"),
        });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A managed block the size a test can count: both markers and `body`.
    fn block(body: &str) -> String {
        format!("{MANAGED_BEGIN}\n{body}\n{MANAGED_END}")
    }

    /// A home holding what `pixel setup` 0.4.0 left on a maintainer's
    /// machine (paths replaced): four retired Claude hooks, one current Codex
    /// relay, and the instruction block in Codex and Pi.
    fn home_of_0_4_0() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let hook = |verb: &str| serde_json::json!({"type":"command","command":format!("'/home/dev/.local/bin/pixel' run-hook {verb}"),"timeout":10});
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(
            home.join(".claude/settings.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks": {
                    "PostToolUse": [{"matcher":"Edit","hooks":[hook("post-tool-use --provider claude")]}],
                    "SessionStart": [
                        {"hooks":[hook("session-start")]},
                        {"matcher":"compact","hooks":[hook("post-compaction --provider claude")]}
                    ],
                    "UserPromptSubmit": [{"hooks":[hook("prompt-submit --provider claude")]}]
                },
                "theme": "dark"
            }))
            .unwrap(),
        )
        .unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::write(
            home.join(".codex/hooks.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks": {"PostToolUse": [{"hooks":[hook("metrics --provider codex")]}]}
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            home.join(".codex/config.toml"),
            [
                "model = \"example\"",
                "developer_instructions = '''",
                MANAGED_BEGIN,
                "# Pixel Retrieval Layer",
                MANAGED_END,
                "'''",
                "",
                "[features]",
                "hooks = true",
                "",
            ]
            .join("\n"),
        )
        .unwrap();
        fs::create_dir_all(home.join(".pi/agent")).unwrap();
        fs::write(
            home.join(".pi/agent/APPEND_SYSTEM.md"),
            format!("Mine first.\n{}\nMine after.\n", block("old prompt")),
        )
        .unwrap();
        dir
    }

    fn find_in(home: &Path) -> Vec<Leftover> {
        find(
            home,
            &home.join(".codex"),
            &home.join(".pi/agent"),
            Path::new("/usr/local/bin/pixel"),
        )
    }

    #[test]
    fn find_should_name_every_leftover_of_a_0_4_0_install_with_its_file() {
        let dir = home_of_0_4_0();
        let home = dir.path();
        let codex_block = block("# Pixel Retrieval Layer").len();
        let pi_block = block("old prompt").len();
        assert_eq!(
            find_in(home),
            [
                Leftover {
                    path: home.join(".claude/settings.json"),
                    what: "4 retired hook(s) this binary ignores: \
                           PostToolUse→post-tool-use --provider claude, \
                           SessionStart→session-start, \
                           SessionStart→post-compaction --provider claude, \
                           UserPromptSubmit→prompt-submit --provider claude"
                        .to_owned(),
                },
                Leftover {
                    path: home.join(".codex/config.toml"),
                    what: format!(
                        "a {MANAGED_BEGIN} block of {codex_block} bytes in `developer_instructions`, read by Codex in every session"
                    ),
                },
                Leftover {
                    path: home.join(".pi/agent/APPEND_SYSTEM.md"),
                    what: format!(
                        "a {MANAGED_BEGIN} block of {pi_block} bytes, read by Pi in every session"
                    ),
                },
            ]
        );
    }

    #[test]
    fn find_should_report_nothing_on_an_empty_home_and_on_a_current_install() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        assert_eq!(find_in(home), []);

        let exe = Path::new("/usr/local/bin/pixel");
        fs::create_dir_all(home.join(".claude")).unwrap();
        routing::install_task_events(
            &Provider::Claude.path(home),
            exe,
            Provider::Claude,
            1,
            true,
            false,
        )
        .unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        crate::codex_config::install_task_hooks(&home.join(".codex"), exe, 1, true, false).unwrap();
        fs::write(
            home.join(".codex/config.toml"),
            "developer_instructions = \"Mine, mentioning pixel:managed in passing.\"\n",
        )
        .unwrap();
        fs::create_dir_all(home.join(".pi/agent")).unwrap();
        fs::write(home.join(".pi/agent/APPEND_SYSTEM.md"), "Mine only.\n").unwrap();
        assert_eq!(find_in(home), []);
    }

    #[test]
    fn find_should_name_retired_hooks_in_the_devin_and_codex_hook_files_too() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let file = |verb: &str| {
            serde_json::json!({"hooks":{"PreToolUse":[{"hooks":[
                {"type":"command","command":format!("pixel run-hook {verb}")}
            ]}]}})
            .to_string()
        };
        fs::create_dir_all(home.join(".config/devin")).unwrap();
        fs::write(
            home.join(".config/devin/config.json"),
            file("guard --provider devin"),
        )
        .unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::write(
            home.join(".codex/hooks.json"),
            file("composed-guard --provider codex"),
        )
        .unwrap();
        assert_eq!(
            find_in(home),
            [
                Leftover {
                    path: home.join(".config/devin/config.json"),
                    what: "1 retired hook(s) this binary ignores: PreToolUse→guard --provider devin"
                        .to_owned(),
                },
                Leftover {
                    path: home.join(".codex/hooks.json"),
                    what: "1 retired hook(s) this binary ignores: PreToolUse→composed-guard --provider codex"
                        .to_owned(),
                },
            ]
        );
    }

    #[test]
    fn find_should_skip_a_hook_file_that_does_not_parse() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(
            home.join(".claude/settings.json"),
            "{ pixel run-hook session-start",
        )
        .unwrap();
        assert_eq!(find_in(home), []);
    }

    #[test]
    fn managed_block_len_should_measure_marker_to_marker_or_to_the_end() {
        assert_eq!(managed_block_len("no marker here"), None);
        assert_eq!(managed_block_len(MANAGED_END), None);
        let whole = block("body");
        assert_eq!(managed_block_len(&whole), Some(whole.len()));
        assert_eq!(
            managed_block_len(&format!(
                "before\n{whole}\nafter, longer than the block itself"
            )),
            Some(whole.len())
        );
        let open = format!("{MANAGED_BEGIN}\nnever closed");
        assert_eq!(
            managed_block_len(&format!("before {open}")),
            Some(open.len())
        );
    }
}
