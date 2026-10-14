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

/// Whether the core binary keeps `command` although a plugin repository
/// publishes it too (`check-release`: the release workflow runs it before any
/// plugin could be installed). Text that names it needs no plugin.
pub fn stays_in_core(command: &str) -> bool {
    command == "check-release"
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
/// has to know which of its words and nested modes never write:
/// `recall index` ingests, `list-errors gc` applies retention and
/// `list-branches --fetch` runs `git fetch --prune`. A name that is not a
/// moved read command, or that is not moved at all, is not read-only.
pub fn read_only_invocation(command: &str, args: &[String]) -> bool {
    const READ_ONLY: [&str; 10] = [
        "recall",
        "list-errors",
        "repo-state",
        "review-changes",
        "commit-history",
        "diff",
        "file-history",
        "search-history",
        "dig-history",
        "list-branches",
    ];
    if !READ_ONLY.contains(&command) {
        return false;
    }
    let first = args.iter().find(|arg| !arg.starts_with('-'));
    match command {
        "recall" => first.map(String::as_str) != Some("index"),
        "list-errors" => first.map(String::as_str) != Some("gc"),
        "list-branches" => !args.iter().any(|arg| arg == "--fetch"),
        _ => true,
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
        assert!(stays_in_core("check-release"));
        assert!(!stays_in_core("web-search") && !stays_in_core(""));
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
        }
        assert!(read_only_invocation("recall", &args(&["search", "x"])));
        assert!(!read_only_invocation("recall", &args(&["index"])));
        assert!(!read_only_invocation("recall", &args(&["--json", "index"])));
        assert!(read_only_invocation("list-errors", &args(&["--json"])));
        assert!(!read_only_invocation("list-errors", &args(&["gc"])));
        assert!(read_only_invocation("list-branches", &args(&["."])));
        assert!(!read_only_invocation(
            "list-branches",
            &args(&[".", "--fetch"])
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
