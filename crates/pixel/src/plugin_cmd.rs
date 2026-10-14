// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel install <source>`, `pixel uninstall <plugin>`, `pixel plugin …` and
//! the dispatch of an unknown subcommand to a plugin.
//!
//! The host itself (manifests, trust, lookup) is `pixel-plugin`; this file
//! reads the process (HOME, PATH, the working directory), prints, and execs.

use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitStatus};
use std::time::{Duration, Instant};

use clap::Subcommand;
use clap::error::{ContextKind, ContextValue, ErrorKind};
use pixel_plugin::{Binary, Env, Error, Host, MOVED_COMMANDS};

#[derive(Subcommand)]
pub enum PluginCmd {
    /// List the plugins visible here: the repo's, your own, and `pixel-<name>` on PATH.
    List,
    /// Allow a plugin that declares `network = true` to run.
    Enable { name: String },
    /// Allow a repo plugin (`<repo>/.pixel/plugins/<name>/`) as it is on disk now; any later change revokes it.
    Trust { name: String },
}

/// Environment variable that redirects `github:<owner>/<repo>` sources (and
/// their release downloads) from `https://github.com` to a mirror, a GitHub
/// Enterprise host or a `file://` directory laid out the same way.
const GITHUB_BASE_ENV: &str = "PIXEL_GITHUB_BASE_URL";

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
        github_base: std::env::var(GITHUB_BASE_ENV)
            .ok()
            .filter(|base| !base.is_empty()),
    })
}

/// The moved commands a plugin runs here, as this process sees the machine:
/// what a rule, a next-step suggestion or the doctor may name without
/// teaching a command that fails. A host that cannot be built (no `HOME`)
/// has none.
pub fn available() -> pixel_plugin::Available {
    host().map_or_else(
        |_| pixel_plugin::Available::none(),
        |host| pixel_plugin::Available::scan(&host),
    )
}

/// `pixel install <source>`. An unsupported source kind (`npm:`) is a usage
/// error: it prints its reason and owns exit status 2, the code of a refusal
/// that no retry changes.
pub fn install(
    source: &str,
    name: Option<&str>,
    owned_exit: &std::cell::Cell<Option<i32>>,
) -> Result<(), String> {
    let host = host()?;
    match pixel_plugin::add(&host, source, name) {
        Ok(added) => {
            print!("{}", render_installed(&added));
            Ok(())
        }
        Err(error @ Error::Unsupported(_)) => {
            eprintln!("pixel: {error}");
            owned_exit.set(Some(pixel_plugin::HINT_EXIT));
            Ok(())
        }
        Err(error) => Err(error.to_string()),
    }
}

/// What `pixel install` prints once the plugin is in place.
fn render_installed(added: &pixel_plugin::Added) -> String {
    let mut out = format!(
        "installed plugin {} {} in {}\n",
        added.name,
        added.manifest.version,
        added.dir.display()
    );
    match &added.binary {
        Binary::Present => {}
        Binary::Downloaded(got) => {
            let checked = if got.verified {
                "sha256 verified"
            } else {
                "no .sha256 published: checksum not verified"
            };
            out.push_str(&format!("downloaded {} ({checked})\n", got.url));
        }
        Binary::Missing {
            git_url,
            reference,
            download_error,
        } => {
            let build =
                pixel_plugin::cargo_install_command(git_url, reference.as_deref(), &added.dir);
            out.push_str(&format!(
                "{name} is installed but not runnable: its program `{run}` is not built and no release asset was usable ({download_error})\nbuild it with: {build}\nthe build must leave `{run}` under {dir}; `pixel plugin list` shows when {name} runs\n",
                name = added.name,
                run = added.manifest.run,
                dir = added.dir.display(),
            ));
        }
    }
    if added.manifest.network {
        out.push_str(&format!(
            "{} declares network access: run `pixel plugin enable {}` to allow it\n",
            added.name, added.name
        ));
    }
    out
}

/// `pixel uninstall <plugin>`.
pub fn uninstall(name: &str) -> Result<(), String> {
    let host = host()?;
    pixel_plugin::remove(&host, name).map_err(|e| e.to_string())?;
    println!("removed plugin {name}");
    Ok(())
}

pub fn run(cmd: PluginCmd) -> Result<(), String> {
    let host = host()?;
    match cmd {
        PluginCmd::List => {
            let rows = pixel_plugin::list(&host).map_err(|e| e.to_string())?;
            print!("{}", pixel_plugin::render(&rows));
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
fn plan(host: &Host, name: &str, args: &[OsString], env: Env<'_>, moved: &[(&str, &str)]) -> Plan {
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
            Some(run_logged(process, name, argv, &root))
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
    if pixel_plugin::is_moved(current) {
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

/// Run the plugin as a child with this process's stdio, wait for it, and
/// write its run to the repository's action log like any core command:
/// `.pixel/actions.jsonl`, the command word and the arguments through
/// [`crate::logged_args`] (a credential in them is masked the same way), the
/// outcome and the duration. The exit code is the plugin's own. A signal
/// that ends the plugin is `128 + signal`, the shell's convention.
fn run_logged(mut process: Process, name: &str, argv: &[String], root: &Path) -> i32 {
    let started = Instant::now();
    let code = match process.status() {
        Ok(status) => exit_code(status),
        Err(failure) => {
            eprintln!("pixel: cannot run plugin `{name}`: {failure}");
            1
        }
    };
    log_run(name, argv, root, code, started.elapsed());
    code
}

/// The code `pixel` exits with for a plugin that ended with `status`.
fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

/// One action-log record for a plugin run. The record is on disk before the
/// process exits: `pixel action-log` and the metrics history read it back.
fn log_run(name: &str, argv: &[String], root: &Path, code: i32, elapsed: Duration) {
    let mut logger = pixel_actionlog::ActionLog::spawn_for_root(root);
    let result = if code == 0 {
        Ok(())
    } else {
        Err(format!("{name} exited {code}"))
    };
    let mut event = pixel_actionlog::ActionEvent::new(name, crate::logged_args(&argv[1..]))
        .with_result(&result, elapsed);
    if let Some(correlation) = crate::task_commands::action_correlation(root) {
        event = event.with_task(correlation);
    }
    logger.log(event);
    logger.finish_flush();
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
            github_base: None,
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
        let table = [("made-up-cmd", "pixel-made-up")];
        let Plan::Stop(message, code) = plan(&host, "made-up-cmd", &[], env(), &table) else {
            panic!("expected the moved-command hint");
        };
        assert_eq!(code, 2);
        assert_eq!(
            message,
            "\"made-up-cmd\" moved to a plugin: pixel install github:Pixel-CLI/pixel-made-up"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_outside_the_table_is_left_to_clap() {
        let dir = scratch("unknown");
        let host = host_at(&dir);
        assert!(matches!(
            plan(
                &host,
                "made-up-cmd",
                &[],
                env(),
                &[("other", "pixel-other")]
            ),
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
        let Plan::Run(process) = plan(
            &host,
            "made-up-cmd",
            &args,
            env(),
            &[("made-up-cmd", "pixel-made-up")],
        ) else {
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
            ("update", "fast-forward"),
            ("savings", "token-savings"),
            ("rewrite", "squash-branch"),
            ("replay-flow", "flow"),
            ("publish", "commit"),
            ("history", "commit-history"),
        ] {
            assert_eq!(moved_name(alias), name);
        }
        for (name, _) in MOVED_COMMANDS {
            assert_eq!(moved_name(name), name, "a moved command is itself");
        }
        // A rename that points at a command that stays in the core is not ours,
        // nor is one that points at a command that was deleted.
        assert_eq!(moved_name("ready"), "ready");
        assert_eq!(moved_name("stats"), "stats");
        assert_eq!(moved_name("made-up-cmd"), "made-up-cmd");
    }

    #[test]
    fn a_moved_name_is_dispatched_only_when_the_core_does_not_build_it_in() {
        let built_in = |name: &'static str| {
            on_big_stack(move || Cli::command().find_subcommand(name).is_some())
        };
        // The moved commands #878 dropped from the binary reach the hint ...
        for name in [
            "flow",
            "plan-rollback",
            "token-savings",
            "squash-branch",
            "fast-forward",
            "commit-history",
            "recall",
            "ultraflow",
        ] {
            assert!(is_moved_name(name) && !built_in(name), "{name}");
        }
        // ... the one moved command still built in is clap's, whatever the table
        // says.
        assert!(is_moved_name("check-release") && built_in("check-release"));
    }

    fn is_moved_name(name: &str) -> bool {
        MOVED_COMMANDS.iter().any(|(moved, _)| *moved == name)
    }

    fn on_big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap()
    }

    fn installed(dir: &Path, name: &str, binary: Binary) -> pixel_plugin::Added {
        pixel_plugin::Added {
            name: name.to_owned(),
            dir: dir.join(name),
            manifest: pixel_plugin::Manifest {
                name: name.to_owned(),
                version: "1.4.2".to_owned(),
                api: 1,
                description: "d".to_owned(),
                run: "bin/tool".to_owned(),
                capabilities: vec![pixel_plugin::Capability::Command],
                network: false,
                commands: vec![],
            },
            binary,
        }
    }

    #[test]
    fn the_install_report_names_what_was_downloaded_or_how_to_build_it() {
        let dir = Path::new("/h/.pixel/plugins");
        assert_eq!(
            render_installed(&installed(dir, "tool", Binary::Present)),
            "installed plugin tool 1.4.2 in /h/.pixel/plugins/tool\n"
        );
        let url = "https://github.com/acme/tool/releases/latest/download/pixel-tool-x".to_owned();
        let verified = Binary::Downloaded(pixel_plugin::Downloaded {
            url: url.clone(),
            verified: true,
        });
        assert_eq!(
            render_installed(&installed(dir, "tool", verified)),
            format!(
                "installed plugin tool 1.4.2 in /h/.pixel/plugins/tool\ndownloaded {url} (sha256 verified)\n"
            )
        );
        let unverified = Binary::Downloaded(pixel_plugin::Downloaded {
            url: url.clone(),
            verified: false,
        });
        assert!(
            render_installed(&installed(dir, "tool", unverified)).ends_with(&format!(
                "downloaded {url} (no .sha256 published: checksum not verified)\n"
            ))
        );
        let missing = Binary::Missing {
            git_url: "https://github.com/acme/tool.git".to_owned(),
            reference: Some("v2".to_owned()),
            download_error: "cannot download u: HTTP 404".to_owned(),
        };
        let text = render_installed(&installed(dir, "tool", missing));
        assert!(
            text.contains("tool is installed but not runnable: its program `bin/tool` is not built and no release asset was usable (cannot download u: HTTP 404)\n"),
            "{text}"
        );
        assert!(
            text.contains("build it with: cargo install --git https://github.com/acme/tool.git --rev v2 --root /h/.pixel/plugins/tool\n"),
            "{text}"
        );
    }

    #[test]
    fn a_network_plugin_tells_the_user_to_enable_it() {
        let dir = Path::new("/h");
        let mut added = installed(dir, "net", Binary::Present);
        added.manifest.network = true;
        assert!(
            render_installed(&added).ends_with(
                "net declares network access: run `pixel plugin enable net` to allow it\n"
            )
        );
    }

    #[test]
    fn a_plugins_exit_code_is_its_own_and_a_signal_is_128_plus_it() {
        assert_eq!(exit_code(ExitStatus::from_raw(0)), 0);
        assert_eq!(exit_code(ExitStatus::from_raw(7 << 8)), 7);
        assert_eq!(exit_code(ExitStatus::from_raw(9)), 137);
        assert_eq!(exit_code(ExitStatus::from_raw(15)), 143);
    }
}
