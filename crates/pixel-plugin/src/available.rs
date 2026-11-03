// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Which of the commands that moved out of the core a plugin answers on this
//! machine, for the text that would otherwise name a command nobody can run:
//! the rules `pixel setup` deploys, the next-step suggestions, the doctor.

use std::collections::{BTreeMap, BTreeSet};

use crate::discover::{Host, resolve};
use crate::error::Error;
use crate::invoke::{MOVED_COMMANDS, PLUGINS_ORG};

/// Whether a plugin answers to one command, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// `pixel <command>` runs a plugin now.
    Runnable,
    /// A plugin claims the command but cannot run: untrusted, not enabled,
    /// a broken manifest. The text is the host's own message, which names
    /// the repair.
    Blocked(String),
    /// No plugin answers.
    Absent,
}

/// Whether a plugin answers `command` for `host`.
pub fn command_status(host: &Host, command: &str) -> Status {
    match resolve(host, command) {
        Ok(Some(_)) => Status::Runnable,
        Ok(None) | Err(Error::NotFound(_)) => Status::Absent,
        Err(error) => Status::Blocked(error.to_string()),
    }
}

/// The moved commands a plugin answers to, as found by [`Available::scan`]
/// or named by a test with [`Available::of`]. Text that suggests a moved
/// command asks this first and falls back to what needs no plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Available {
    runnable: BTreeSet<String>,
    blocked: BTreeMap<String, String>,
}

impl Available {
    /// No plugin: what a fresh machine has.
    pub fn none() -> Self {
        Self::default()
    }

    /// Exactly `commands` runnable (tests, and callers that already know).
    pub fn of(commands: &[&str]) -> Self {
        Self {
            runnable: commands.iter().map(|c| (*c).to_owned()).collect(),
            blocked: BTreeMap::new(),
        }
    }

    /// `self` with `command` claimed by a plugin that cannot run, for `why`.
    #[must_use]
    pub fn blocking(mut self, command: &str, why: &str) -> Self {
        self.blocked.insert(command.to_owned(), why.to_owned());
        self
    }

    /// Resolve every command of [`MOVED_COMMANDS`] against `host`.
    pub fn scan(host: &Host) -> Self {
        let mut found = Self::none();
        for (command, _) in MOVED_COMMANDS {
            match command_status(host, command) {
                Status::Runnable => {
                    found.runnable.insert(command.to_owned());
                }
                Status::Blocked(why) => {
                    found.blocked.insert(command.to_owned(), why);
                }
                Status::Absent => {}
            }
        }
        found
    }

    /// Whether `pixel <command>` runs a plugin.
    pub fn has(&self, command: &str) -> bool {
        self.runnable.contains(command)
    }

    /// Why a plugin that claims `command` cannot run, when one does.
    pub fn blocked(&self, command: &str) -> Option<&str> {
        self.blocked.get(command).map(String::as_str)
    }
}

/// Whether the core binary builds `command` in although a plugin repository
/// publishes it too: `check-release` (the release workflow runs it before any
/// plugin could be installed) and `search-history` (the brief's next step
/// names it, so it must run without a plugin). Text that names one needs no
/// plugin, and clap answers it before any plugin is looked up.
pub fn stays_in_core(command: &str) -> bool {
    matches!(command, "check-release" | "search-history")
}

/// The repository that publishes `command`, when it is a moved one.
pub fn repo_of(command: &str) -> Option<&'static str> {
    MOVED_COMMANDS
        .iter()
        .find(|(moved, _)| *moved == command)
        .map(|(_, repo)| *repo)
}

/// `pixel install github:Pixel-CLI/<repo>`, the command that provides `repo`.
pub fn install_command(repo: &str) -> String {
    format!("pixel install github:{PLUGINS_ORG}/{repo}")
}

/// Whether `pixel <command> <args…>` only reads, so an agent may run it
/// without it counting as an edit. The plugin does the work; the core only
/// has to know which of its words and modes never write, and it errs toward
/// "writes": a command with modes is read-only only when its **first**
/// argument names a read mode, so an option value can never be mistaken for
/// the mode (`recall --limit 5 index` is not read-only). `recall index`,
/// `setup`, `embed`, `export` and `daemon`, `list-errors gc` and `run` (which
/// executes a wrapped command), and `list-branches --fetch` (a `git fetch
/// --prune`) write. A name that is not a moved read command, or that is not
/// moved at all, is not read-only.
pub fn read_only_invocation(command: &str, args: &[String]) -> bool {
    const RECALL_READ_MODES: [&str; 6] = ["search", "ask", "context", "show", "sessions", "status"];
    const LIST_ERRORS_READ_MODES: [&str; 4] = ["last", "since", "show", "cursor"];
    let first_is = |modes: &[&str]| {
        args.first()
            .is_some_and(|mode| modes.contains(&mode.as_str()))
    };
    match command {
        "recall" => first_is(&RECALL_READ_MODES),
        "list-errors" => first_is(&LIST_ERRORS_READ_MODES),
        "list-branches" => !args
            .iter()
            .any(|arg| arg == "--fetch" || arg.starts_with("--fetch=")),
        "repo-state" | "review-changes" | "commit-history" | "diff" | "file-history"
        | "search-history" | "dig-history" => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{fixture, plugin_dir};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_fresh_machine_has_no_moved_command() {
        let f = fixture();
        assert_eq!(Available::scan(&f.host), Available::none());
        assert!(!Available::none().has("commit"));
        assert_eq!(command_status(&f.host, "commit"), Status::Absent);
    }

    #[test]
    fn a_plugin_that_lists_commands_makes_each_of_them_runnable_and_no_other() {
        let f = fixture();
        plugin_dir(
            &f.host.user_plugins(),
            "git",
            "commands = [\"commit\", \"who-wrote\"]",
        );
        let found = Available::scan(&f.host);
        assert!(found.has("commit") && found.has("who-wrote"));
        assert!(!found.has("push"), "a command the manifest does not list");
        assert!(!found.has("web-search"));
        assert_eq!(command_status(&f.host, "commit"), Status::Runnable);
    }

    #[test]
    fn a_plugin_that_needs_enabling_is_blocked_with_the_hosts_own_words() {
        let f = fixture();
        plugin_dir(
            &f.host.user_plugins(),
            "git",
            "network = true\ncommands = [\"commit\"]",
        );
        let found = Available::scan(&f.host);
        assert!(!found.has("commit"));
        assert_eq!(
            found.blocked("commit"),
            Some(
                "plugin `git` declares network access and is not enabled: run `pixel plugin enable git`"
            )
        );
        assert_eq!(found.blocked("push"), None);
    }

    #[test]
    fn the_repository_and_install_command_come_from_the_moved_table() {
        assert_eq!(repo_of("who-wrote"), Some("pixel-git"));
        assert_eq!(repo_of("web-search"), Some("pixel-utilities"));
        assert_eq!(repo_of("search-content"), None);
        assert!(stays_in_core("check-release") && stays_in_core("search-history"));
        assert!(
            !stays_in_core("dig-history") && !stays_in_core("web-search") && !stays_in_core("")
        );
        assert_eq!(
            install_command("pixel-git"),
            "pixel install github:Pixel-CLI/pixel-git"
        );
    }

    #[test]
    fn the_read_only_commands_stay_read_only_unless_a_mode_or_flag_writes() {
        for name in [
            "repo-state",
            "review-changes",
            "commit-history",
            "diff",
            "file-history",
            "search-history",
            "dig-history",
        ] {
            assert!(read_only_invocation(name, &args(&["--json"])), "{name}");
            assert!(read_only_invocation(name, &[]), "{name}");
        }
        for mode in ["search", "ask", "context", "show", "sessions", "status"] {
            assert!(
                read_only_invocation("recall", &args(&[mode, "x"])),
                "{mode}"
            );
        }
        for mode in ["index", "setup", "embed", "export", "daemon", "bogus"] {
            assert!(!read_only_invocation("recall", &args(&[mode])), "{mode}");
        }
        assert!(
            !read_only_invocation("recall", &[]),
            "no mode is not a read"
        );
        for mode in ["last", "since", "show", "cursor"] {
            assert!(
                read_only_invocation("list-errors", &args(&[mode, "--json"])),
                "{mode}"
            );
        }
        for mode in ["gc", "run", "bogus"] {
            assert!(
                !read_only_invocation("list-errors", &args(&[mode])),
                "{mode}"
            );
        }
        assert!(read_only_invocation("list-branches", &args(&["."])));
        assert!(!read_only_invocation(
            "list-branches",
            &args(&[".", "--fetch"])
        ));
        assert!(!read_only_invocation(
            "list-branches",
            &args(&["--fetch=true"])
        ));
    }

    /// An option's value is not the mode: with an option first the core
    /// cannot tell, so the invocation is not a read.
    #[test]
    fn an_option_before_the_mode_makes_the_invocation_not_read_only() {
        assert!(!read_only_invocation(
            "recall",
            &args(&["--limit", "5", "index"])
        ));
        assert!(!read_only_invocation(
            "recall",
            &args(&["--limit", "index", "search"])
        ));
        assert!(!read_only_invocation(
            "recall",
            &args(&["--json", "search", "x"])
        ));
        assert!(!read_only_invocation(
            "list-errors",
            &args(&["--repo", "last", "gc"])
        ));
        assert!(!read_only_invocation(
            "list-errors",
            &args(&["--json", "last"])
        ));
    }

    #[test]
    fn a_mutating_or_unmoved_command_is_never_read_only() {
        for name in [
            "commit",
            "push",
            "new-branch",
            "fast-forward",
            "squash-branch",
            "sync-branch",
            "fetch",
            "plan-rollback",
            "edit-env",
            "record-event",
            "flow",
            "search-content",
            "status",
            "",
        ] {
            assert!(!read_only_invocation(name, &[]), "{name:?}");
        }
    }
}
