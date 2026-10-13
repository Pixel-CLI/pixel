// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Running a resolved plugin: its argv, its environment, and the hint for a
//! command that moved out of the core binary.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

use crate::discover::Resolved;

/// The plugin API version exported to a plugin as `PIXEL_API`.
pub const PIXEL_API: &str = "1";

/// The GitHub organisation that publishes the first-party plugins.
pub const PLUGINS_ORG: &str = "Pixel-CLI";

/// Commands that live in a first-party plugin repository under
/// [`PLUGINS_ORG`], each with that repository. The core still builds most
/// of them in; the hint is only reached for a name clap does not know.
pub const MOVED_COMMANDS: [(&str, &str); 30] = [
    ("commit", "pixel-git"),
    ("push", "pixel-git"),
    ("commit-and-push", "pixel-git"),
    ("new-branch", "pixel-git"),
    ("fast-forward", "pixel-git"),
    ("squash-branch", "pixel-git"),
    ("sync-branch", "pixel-git"),
    ("fetch", "pixel-git"),
    ("list-branches", "pixel-git"),
    ("diff", "pixel-git"),
    ("commit-history", "pixel-git"),
    ("repo-state", "pixel-git"),
    ("review-changes", "pixel-git"),
    ("who-wrote", "pixel-git"),
    ("edit-env", "pixel-git"),
    ("plan-rollback", "pixel-git"),
    ("search-history", "pixel-git"),
    ("file-history", "pixel-git"),
    ("dig-history", "pixel-git"),
    ("flow", "pixel-flow"),
    ("ultraflow", "pixel-flow"),
    ("recall", "pixel-recall"),
    ("list-errors", "pixel-telemetry"),
    ("record-event", "pixel-telemetry"),
    ("action-log", "pixel-telemetry"),
    ("token-savings", "pixel-telemetry"),
    ("space", "pixel-telemetry"),
    ("web-search", "pixel-utilities"),
    ("check-release", "pixel-utilities"),
    ("ai-cli-readify", "pixel-utilities"),
];

/// What a plugin is told about the core that launched it.
#[derive(Debug, Clone, Copy)]
pub struct Env<'a> {
    pub repo_root: &'a Path,
    pub graph_db: &'a Path,
    pub bin: &'a Path,
}

/// The process to run: the plugin's executable with `args` unchanged (after
/// the command word, for a plugin that lists `commands`),
/// stdio inherited, and `PIXEL_API`, `PIXEL_REPO_ROOT`, `PIXEL_GRAPH_DB`
/// and `PIXEL_BIN` set. Nothing else of the environment is touched.
pub fn command(plugin: &Resolved, args: &[OsString], env: Env<'_>) -> Command {
    let mut command = Command::new(&plugin.program);
    command.args(&plugin.command);
    command
        .args(args)
        .env("PIXEL_API", PIXEL_API)
        .env("PIXEL_REPO_ROOT", env.repo_root)
        .env("PIXEL_GRAPH_DB", env.graph_db)
        .env("PIXEL_BIN", env.bin);
    command
}

/// `"<name>" moved to a plugin: pixel install github:Pixel-CLI/<repo>`, when
/// `name` is in `moved`.
pub fn moved_hint(moved: &[(&str, &str)], name: &str) -> Option<String> {
    moved
        .iter()
        .find(|(command, _)| *command == name)
        .map(|(_, repo)| {
            format!("\"{name}\" moved to a plugin: pixel install github:{PLUGINS_ORG}/{repo}")
        })
}

/// Whether `name` is a command of [`MOVED_COMMANDS`].
pub fn is_moved(name: &str) -> bool {
    MOVED_COMMANDS.iter().any(|(command, _)| *command == name)
}

/// Exit status of a hint: the usage-error convention of the CLI.
pub const HINT_EXIT: i32 = 2;

/// The first word of `argv` (program name excluded) that names a command:
/// not an option, and not the value of `--metrics`, the one global option
/// that takes a separate value. Its index in `argv` comes with it.
pub fn command_word(argv: &[String]) -> Option<(usize, &str)> {
    let mut index = 1;
    while let Some(word) = argv.get(index) {
        if word == "--metrics" {
            index += 2;
        } else if word.starts_with('-') {
            index += 1;
        } else {
            return Some((index, word));
        }
    }
    None
}

/// `OsStr` view for callers holding `String` args.
pub fn os_args(args: &[String]) -> Vec<OsString> {
    args.iter()
        .map(OsStr::new)
        .map(OsStr::to_os_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_table_maps_every_planned_command_to_its_repository() {
        let by_repo = |repo: &str| -> Vec<&str> {
            MOVED_COMMANDS
                .iter()
                .filter(|(_, r)| *r == repo)
                .map(|(command, _)| *command)
                .collect()
        };
        assert_eq!(
            by_repo("pixel-git"),
            [
                "commit",
                "push",
                "commit-and-push",
                "new-branch",
                "fast-forward",
                "squash-branch",
                "sync-branch",
                "fetch",
                "list-branches",
                "diff",
                "commit-history",
                "repo-state",
                "review-changes",
                "who-wrote",
                "edit-env",
                "plan-rollback",
                "search-history",
                "file-history",
                "dig-history"
            ]
        );
        assert_eq!(by_repo("pixel-flow"), ["flow", "ultraflow"]);
        assert_eq!(by_repo("pixel-recall"), ["recall"]);
        assert_eq!(
            by_repo("pixel-telemetry"),
            [
                "list-errors",
                "record-event",
                "action-log",
                "token-savings",
                "space"
            ]
        );
        assert_eq!(
            by_repo("pixel-utilities"),
            ["web-search", "check-release", "ai-cli-readify"]
        );
        let mut names: Vec<&str> = MOVED_COMMANDS.iter().map(|(c, _)| *c).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            MOVED_COMMANDS.len(),
            "a command is listed twice"
        );
        for name in names {
            assert!(
                crate::valid_name(name),
                "{name} must be a valid plugin name"
            );
        }
    }

    #[test]
    fn a_moved_name_gets_the_exact_hint_text() {
        assert_eq!(
            moved_hint(&MOVED_COMMANDS, "flow").as_deref(),
            Some("\"flow\" moved to a plugin: pixel install github:Pixel-CLI/pixel-flow")
        );
        assert_eq!(
            moved_hint(&MOVED_COMMANDS, "commit-history").as_deref(),
            Some("\"commit-history\" moved to a plugin: pixel install github:Pixel-CLI/pixel-git")
        );
    }

    #[test]
    fn the_hint_uses_the_table_it_is_given_and_nothing_else() {
        let table = [("made-up-cmd", "pixel-made-up")];
        let hint = moved_hint(&table, "made-up-cmd").unwrap();
        assert_eq!(
            hint,
            "\"made-up-cmd\" moved to a plugin: pixel install github:Pixel-CLI/pixel-made-up"
        );
        assert_eq!(moved_hint(&table, "flow"), None);
        assert_eq!(moved_hint(&MOVED_COMMANDS, "made-up-cmd"), None);
        assert_eq!(moved_hint(&MOVED_COMMANDS, ""), None);
        assert_eq!(moved_hint(&MOVED_COMMANDS, "flo"), None, "whole names only");
        assert!(is_moved("flow") && !is_moved("flo") && !is_moved(""));
    }

    #[test]
    fn a_multi_command_plugin_gets_the_command_word_before_the_args() {
        let plugin = Resolved {
            name: "git".into(),
            place: crate::Place::User,
            program: PathBuf::from("/plugins/git/pixel-git"),
            dir: None,
            command: Some("commit".into()),
        };
        let env = Env {
            repo_root: Path::new("/repo"),
            graph_db: Path::new("/repo/.pixel/graph.v2.db"),
            bin: Path::new("/usr/bin/pixel"),
        };
        let cmd = command(&plugin, &os_args(&["--message".into(), "m".into()]), env);
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            ["commit", "--message", "m"]
        );
    }

    #[test]
    fn the_command_carries_args_and_the_four_variables() {
        let plugin = Resolved {
            name: "t".into(),
            place: crate::Place::User,
            program: PathBuf::from("/plugins/t/run"),
            dir: None,
            command: None,
        };
        let args = os_args(&["a".into(), "--b".into(), "c d".into()]);
        let env = Env {
            repo_root: Path::new("/repo"),
            graph_db: Path::new("/repo/.pixel/graph.v2.db"),
            bin: Path::new("/usr/bin/pixel"),
        };
        let cmd = command(&plugin, &args, env);
        assert_eq!(cmd.get_program(), "/plugins/t/run");
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["a", "--b", "c d"]);
        let vars: std::collections::BTreeMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_str().unwrap(), v.map(|v| v.to_str().unwrap())))
            .collect();
        assert_eq!(
            vars,
            std::collections::BTreeMap::from([
                ("PIXEL_API", Some("1")),
                ("PIXEL_BIN", Some("/usr/bin/pixel")),
                ("PIXEL_GRAPH_DB", Some("/repo/.pixel/graph.v2.db")),
                ("PIXEL_REPO_ROOT", Some("/repo")),
            ])
        );
    }

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn the_command_word_skips_options_and_the_metrics_value() {
        assert_eq!(command_word(&words("pixel flow a b")), Some((1, "flow")));
        assert_eq!(
            command_word(&words("pixel --metrics off flow x")),
            Some((3, "flow"))
        );
        assert_eq!(
            command_word(&words("pixel --metrics=off flow")),
            Some((2, "flow"))
        );
        assert_eq!(command_word(&words("pixel -V")), None);
        assert_eq!(command_word(&words("pixel")), None);
        assert_eq!(command_word(&words("pixel --metrics")), None);
    }
}
