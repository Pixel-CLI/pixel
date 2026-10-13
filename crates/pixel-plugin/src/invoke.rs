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

/// Where the first-party plugins live.
pub const PLUGINS_REPO: &str = "https://github.com/Pixel-CLI/pixel-plugins";

/// Commands that moved out of core into a plugin of the same name.
pub const MOVED_COMMANDS: [&str; 8] = [
    "workspace",
    "flow",
    "plan-rollback",
    "coverage",
    "token-savings",
    "index-stats",
    "squash-branch",
    "fast-forward",
];

/// What a plugin is told about the core that launched it.
#[derive(Debug, Clone, Copy)]
pub struct Env<'a> {
    pub repo_root: &'a Path,
    pub graph_db: &'a Path,
    pub bin: &'a Path,
}

/// The process to run: the plugin's executable with `args` unchanged,
/// stdio inherited, and `PIXEL_API`, `PIXEL_REPO_ROOT`, `PIXEL_GRAPH_DB`
/// and `PIXEL_BIN` set. Nothing else of the environment is touched.
pub fn command(plugin: &Resolved, args: &[OsString], env: Env<'_>) -> Command {
    let mut command = Command::new(&plugin.program);
    command
        .args(args)
        .env("PIXEL_API", PIXEL_API)
        .env("PIXEL_REPO_ROOT", env.repo_root)
        .env("PIXEL_GRAPH_DB", env.graph_db)
        .env("PIXEL_BIN", env.bin);
    command
}

/// `"<name>" moved to a plugin: pixel plugin add <repo> --name <name>`, when
/// `name` is in `moved`.
pub fn moved_hint(moved: &[&str], name: &str) -> Option<String> {
    moved.contains(&name).then(|| {
        format!("\"{name}\" moved to a plugin: pixel plugin add {PLUGINS_REPO} --name {name}")
    })
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
    fn the_table_is_exactly_the_eight_moved_commands() {
        assert_eq!(
            MOVED_COMMANDS,
            [
                "workspace",
                "flow",
                "plan-rollback",
                "coverage",
                "token-savings",
                "index-stats",
                "squash-branch",
                "fast-forward"
            ]
        );
        for name in MOVED_COMMANDS {
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
            Some(
                "\"flow\" moved to a plugin: pixel plugin add https://github.com/Pixel-CLI/pixel-plugins --name flow"
            )
        );
    }

    #[test]
    fn the_hint_uses_the_table_it_is_given_and_nothing_else() {
        let table = ["made-up-cmd"];
        let hint = moved_hint(&table, "made-up-cmd").unwrap();
        assert!(
            hint.starts_with("\"made-up-cmd\" moved to a plugin: "),
            "{hint}"
        );
        assert!(hint.ends_with("--name made-up-cmd"), "{hint}");
        assert_eq!(moved_hint(&table, "flow"), None);
        assert_eq!(moved_hint(&MOVED_COMMANDS, "made-up-cmd"), None);
        assert_eq!(moved_hint(&MOVED_COMMANDS, ""), None);
        assert_eq!(moved_hint(&MOVED_COMMANDS, "flo"), None, "whole names only");
    }

    #[test]
    fn the_command_carries_args_and_the_four_variables() {
        let plugin = Resolved {
            name: "t".into(),
            place: crate::Place::User,
            program: PathBuf::from("/plugins/t/run"),
            dir: None,
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
