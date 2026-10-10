// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel plugin …` and the dispatch of an unknown subcommand to a plugin.
//!
//! The host itself (manifests, trust, lookup) is `pixel-plugin`; this file
//! reads the process (HOME, PATH, the working directory), prints, and execs.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use clap::Subcommand;
use clap::error::{ContextKind, ContextValue, ErrorKind};
use pixel_plugin::{Env, Host, MOVED_COMMANDS};

#[derive(Subcommand)]
pub enum PluginCmd {
    /// List the plugins visible here: the repo's, your own, and `pixel-<name>` on PATH.
    List,
    /// Install a plugin into `~/.pixel/plugins/<name>/` from a local directory or a git URL.
    Add {
        /// A directory holding `pixel-plugin.toml`, or a git URL (https://, ssh://, file://, git@host:path).
        source: String,
        /// The plugin to install, when the source holds several.
        #[arg(long)]
        name: Option<String>,
    },
    /// Delete an installed plugin and forget its source and network opt-in.
    Remove { name: String },
    /// Allow a plugin that declares `network = true` to run.
    Enable { name: String },
    /// Allow a repo plugin (`<repo>/.pixel/plugins/<name>/`) as it is on disk now; any later change revokes it.
    Trust { name: String },
}

/// The host as this process sees it. The repo tier is the git root of the
/// working directory, and never the home directory itself (a dotfiles
/// repository there would otherwise turn the user's own plugins into
/// trust-gated "repo" plugins).
fn host() -> Result<Host, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| !h.as_os_str().is_empty())
        .ok_or("HOME is not set: plugins live under ~/.pixel")?;
    let cwd =
        std::env::current_dir().map_err(|e| format!("cannot read the working directory: {e}"))?;
    Ok(Host {
        repo_root: pixel_git::discover_root(&cwd),
        home,
        path: std::env::var_os("PATH").unwrap_or_default(),
    })
}

pub fn run(cmd: PluginCmd) -> Result<(), String> {
    let host = host()?;
    match cmd {
        PluginCmd::List => {
            let rows = pixel_plugin::list(&host).map_err(|e| e.to_string())?;
            print!("{}", pixel_plugin::render(&rows));
        }
        PluginCmd::Add { source, name } => {
            let added =
                pixel_plugin::add(&host, &source, name.as_deref()).map_err(|e| e.to_string())?;
            println!(
                "installed plugin {} {} in {}",
                added.name,
                added.manifest.version,
                added.dir.display()
            );
            if added.manifest.network {
                println!(
                    "{} declares network access: run `pixel plugin enable {}` to allow it",
                    added.name, added.name
                );
            }
        }
        PluginCmd::Remove { name } => {
            pixel_plugin::remove(&host, &name).map_err(|e| e.to_string())?;
            println!("removed plugin {name}");
        }
        PluginCmd::Enable { name } => {
            match pixel_plugin::enable(&host, &name).map_err(|e| e.to_string())? {
                pixel_plugin::Enabled::Yes => println!("enabled plugin {name}"),
                pixel_plugin::Enabled::NotNeeded => {
                    println!("plugin {name} declares no network access: nothing to enable");
                }
            }
        }
        PluginCmd::Trust { name } => {
            let digest = pixel_plugin::trust(&host, &name).map_err(|e| e.to_string())?;
            println!("trusted repo plugin {name} (sha256 {})", &digest[..12]);
        }
    }
    Ok(())
}

/// What an unknown subcommand turned into.
enum Plan {
    /// A plugin to exec.
    Run(Process),
    /// Not a plugin: print this and exit with the code.
    Stop(String, i32),
    /// Not ours: clap's own error stands.
    Unknown,
}

/// Decide what `pixel <name> <args…>` does when the core has no such command.
fn plan(host: &Host, name: &str, args: &[OsString], env: Env<'_>, moved: &[&str]) -> Plan {
    match pixel_plugin::resolve(host, name) {
        Ok(Some(plugin)) => Plan::Run(pixel_plugin::command(&plugin, args, env)),
        Ok(None) => pixel_plugin::moved_hint(moved, name).map_or(Plan::Unknown, |hint| {
            Plan::Stop(hint, pixel_plugin::HINT_EXIT)
        }),
        Err(error) => Plan::Stop(error.to_string(), 1),
    }
}

/// Hand an unrecognized subcommand to a plugin. `Some(code)` when it was
/// handled and the process should exit with `code` (the plugin replaces
/// this process on success, so only failures and hints return); `None`
/// leaves clap's error to be shown.
pub fn dispatch(error: &clap::Error, argv: &[String]) -> Option<i32> {
    let (index, typed) = pixel_plugin::command_word(argv)?;
    if !names_the_unknown_command(error, typed) {
        return None;
    }
    let name = moved_name(typed);
    let host = host().ok()?;
    let cwd = std::env::current_dir().ok()?;
    let root = crate::discover_root(Path::new(".")).unwrap_or(cwd);
    let graph_db = root
        .join(pixel_index::index::SHARD_DIR)
        .join(pixel_daemon::api::GRAPH_DB_FILE);
    let bin = std::env::current_exe().ok()?;
    let env = Env {
        repo_root: &root,
        graph_db: &graph_db,
        bin: &bin,
    };
    let args = pixel_plugin::os_args(&argv[index + 1..]);
    match plan(&host, name, &args, env, &MOVED_COMMANDS) {
        Plan::Run(process) => {
            // The pre-rename spellings of a moved command (`rescue`, `stats`, …)
            // keep teaching the new one, as they did when they were clap aliases.
            // `--help` never printed it (clap answered before the note).
            let help = argv[index + 1..].iter().any(|a| a == "--help" || a == "-h");
            if !help && let Some(note) = crate::rename_note(argv, true) {
                eprint!("{note}");
            }
            Some(exec(process, name))
        }
        Plan::Stop(message, code) => {
            eprintln!("pixel: {message}");
            Some(code)
        }
        Plan::Unknown => None,
    }
}

/// The moved command `word` stands for: itself when it is one, the command
/// a pre-rename alias was renamed to (`rescue` is `plan-rollback`, `update`
/// is `fast-forward`, …) when that is one, else `word` unchanged.
fn moved_name(word: &str) -> &str {
    let current = pixel_proto::commands::current_name(word);
    if MOVED_COMMANDS.contains(&current) {
        current
    } else {
        word
    }
}

/// Whether clap rejected exactly the top-level word `name` as an unknown
/// subcommand (and not, say, a misspelt nested one).
fn names_the_unknown_command(error: &clap::Error, name: &str) -> bool {
    error.kind() == ErrorKind::InvalidSubcommand
        && matches!(
            error.get(ContextKind::InvalidSubcommand),
            Some(ContextValue::String(unknown)) if unknown == name
        )
}

/// Replace this process with the plugin: stdio, signals and the exit code
/// are the plugin's own. Only a failure to start returns.
#[cfg_attr(test, mutants::skip)] // one-line adapter over exec(2)
fn exec(mut process: Process, name: &str) -> i32 {
    let failure = process.exec();
    eprintln!("pixel: cannot run plugin `{name}`: {failure}");
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cli;
    use clap::CommandFactory;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pixel-plugin-cmd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn host_at(dir: &Path) -> Host {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        Host {
            home: dir.join("home"),
            repo_root: None,
            path: bin.into_os_string(),
        }
    }

    fn env() -> Env<'static> {
        Env {
            repo_root: Path::new("/repo"),
            graph_db: Path::new("/repo/.pixel/graph.v2.db"),
            bin: Path::new("/bin/pixel"),
        }
    }

    #[test]
    fn a_name_in_the_table_that_no_plugin_provides_gets_the_hint_and_exit_two() {
        let dir = scratch("hint");
        let host = host_at(&dir);
        let Plan::Stop(message, code) = plan(&host, "made-up-cmd", &[], env(), &["made-up-cmd"])
        else {
            panic!("expected the moved-command hint");
        };
        assert_eq!(code, 2);
        assert_eq!(
            message,
            "\"made-up-cmd\" moved to a plugin: pixel plugin add https://github.com/Pixel-CLI/pixel-plugins --name made-up-cmd"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_outside_the_table_is_left_to_clap() {
        let dir = scratch("unknown");
        let host = host_at(&dir);
        assert!(matches!(
            plan(&host, "made-up-cmd", &[], env(), &["other"]),
            Plan::Unknown
        ));
        assert!(matches!(
            plan(&host, "Not Valid", &[], env(), &[]),
            Plan::Unknown
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plugin_on_path_wins_over_the_hint_and_receives_the_args() {
        let dir = scratch("path");
        let host = host_at(&dir);
        let exe = dir.join("bin/pixel-made-up-cmd");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let args = vec![OsString::from("a"), OsString::from("--b")];
        let Plan::Run(process) = plan(&host, "made-up-cmd", &args, env(), &["made-up-cmd"]) else {
            panic!("expected the PATH plugin to run");
        };
        assert_eq!(process.get_program(), exe.as_os_str());
        assert_eq!(process.get_args().collect::<Vec<_>>(), ["a", "--b"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_resolution_failure_stops_with_its_reason_and_exit_one() {
        let dir = scratch("untrusted");
        let host = Host {
            repo_root: Some(dir.join("repo")),
            ..host_at(&dir)
        };
        let plugin = dir.join("repo/.pixel/plugins/tool");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("pixel-plugin.toml"),
            "name = \"tool\"\nversion = \"1\"\napi = 1\ndescription = \"d\"\nrun = \"run\"\ncapabilities = [\"command\"]\n",
        )
        .unwrap();
        std::fs::write(plugin.join("run"), "#!/bin/sh\n").unwrap();
        let Plan::Stop(message, code) = plan(&host, "tool", &[], env(), &MOVED_COMMANDS) else {
            panic!("expected a refusal");
        };
        assert_eq!(code, 1);
        assert!(message.contains("`pixel plugin trust tool`"), "{message}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The clap error for `line`, built on a thread with room for the whole
    /// command tree (the test thread's default stack is too small for it).
    fn parse_error(line: &'static str) -> clap::Error {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                Cli::command()
                    .try_get_matches_from(line.split_whitespace())
                    .unwrap_err()
            })
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    fn only_the_top_level_unknown_word_is_dispatched() {
        let top = parse_error("pixel nosuchcmd arg");
        assert!(names_the_unknown_command(&top, "nosuchcmd"));
        assert!(!names_the_unknown_command(&top, "arg"));
        let nested = parse_error("pixel plugin nosuchcmd");
        assert!(
            !names_the_unknown_command(&nested, "plugin"),
            "a nested typo is clap's to report"
        );
        let usage = parse_error("pixel search-content");
        assert!(!names_the_unknown_command(&usage, "search-content"));
    }

    #[test]
    fn a_moved_command_alias_resolves_to_its_plugin_and_nothing_else_does() {
        for (alias, name) in [
            ("rescue", "plan-rollback"),
            ("stats", "index-stats"),
            ("update", "fast-forward"),
            ("savings", "token-savings"),
            ("rewrite", "squash-branch"),
            ("replay-flow", "flow"),
        ] {
            assert_eq!(moved_name(alias), name);
        }
        for name in MOVED_COMMANDS {
            assert_eq!(moved_name(name), name, "a moved command is itself");
        }
        // A rename that points at a command still in the core is not ours.
        assert_eq!(moved_name("ready"), "ready");
        assert_eq!(moved_name("made-up-cmd"), "made-up-cmd");
    }
}
