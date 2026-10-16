// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Subcommand names 0.8.0 removed, kept only to name their replacement.
//!
//! The verb-first rename (0.2.5) kept every old name as a hidden alias; #965
//! dropped the aliases in 0.8.0. A script, hook or prompt still calling
//! `pixel ready` then got clap's `tip: a similar subcommand exists:
//! 'rename'`, which points at the wrong command. The names stay removed:
//! nothing here parses them, the CLI only says what replaces them when clap
//! has already rejected the word.

/// The release that removed the names of [`REMOVED_COMMANDS`].
pub const REMOVED_IN: &str = "0.8.0";

/// `(removed name, current command)`, sorted by removed name: the
/// pre-rename aliases #965 dropped, as they stood before it.
pub const REMOVED_COMMANDS: &[(&str, &str)] = &[
    ("ask", "search-meaning"),
    ("branch", "new-branch"),
    ("branches", "list-branches"),
    ("changes", "what-changed"),
    ("clusters", "list-areas"),
    ("context", "pack-context"),
    ("excavate", "dig-history"),
    ("graph", "rebuild-graph"),
    ("history", "commit-history"),
    ("history-search", "search-history"),
    ("hook", "run-hook"),
    ("index", "build-index"),
    ("inspect", "repo-state"),
    ("lifecycle", "file-history"),
    ("log", "action-log"),
    ("map", "repo-map"),
    ("processes", "list-flows"),
    ("publish", "commit"),
    ("query", "run-recipe"),
    ("ready", "prepare-repo"),
    ("reconcile", "sync-branch"),
    ("release-check", "check-release"),
    ("replay-flow", "flow"),
    ("rescue", "plan-rollback"),
    ("resolve", "find-code"),
    ("review", "review-changes"),
    ("rewrite", "squash-branch"),
    ("savings", "token-savings"),
    ("search", "search-content"),
    ("search-compat", "search-like-rg"),
    ("ship", "commit-and-push"),
    ("skeleton", "list-signatures"),
    ("sniper", "list-errors"),
    ("symbol", "find-symbol"),
    ("sync", "fetch"),
    ("targets", "scope-task"),
    ("task", "task-state"),
    ("trace", "call-path"),
    ("update", "fast-forward"),
    ("upgrade", "self-update"),
    ("uses", "who-calls"),
];

/// The command that replaced the removed name `name`, if it is one.
pub fn replacement(name: &str) -> Option<&'static str> {
    REMOVED_COMMANDS
        .iter()
        .find(|(removed, _)| *removed == name)
        .map(|(_, current)| *current)
}

/// What `pixel <name>` says when `name` is a removed command:
/// `'ready' was removed in 0.8.0; use `pixel prepare-repo``. When the
/// replacement moved to a plugin and `plugin_installed` is false, the
/// install command follows, so the advice is one the user can run.
pub fn removed_hint(
    name: &str,
    moved: &[(&str, &str)],
    plugin_installed: impl FnOnce(&str) -> bool,
) -> Option<String> {
    let current = replacement(name)?;
    let mut hint = format!("'{name}' was removed in {REMOVED_IN}; use `pixel {current}`");
    let repo = moved
        .iter()
        .find(|(command, _)| *command == current)
        .map(|(_, repo)| *repo);
    if let Some(repo) = repo
        && !pixel_plugin::stays_in_core(current)
        && !plugin_installed(current)
    {
        hint.push_str(&format!(
            ", a plugin command: {}",
            pixel_plugin::install_command(repo)
        ));
    }
    Some(hint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use pixel_plugin::MOVED_COMMANDS;
    use std::collections::HashSet;

    fn on_big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    fn the_table_is_the_41_aliases_965_dropped_sorted_and_unique() {
        assert_eq!(REMOVED_COMMANDS.len(), 41);
        let removed: HashSet<&str> = REMOVED_COMMANDS.iter().map(|(r, _)| *r).collect();
        assert_eq!(
            removed.len(),
            REMOVED_COMMANDS.len(),
            "a name is listed twice"
        );
        let mut sorted = REMOVED_COMMANDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, REMOVED_COMMANDS, "keep the table sorted");
    }

    #[test]
    fn a_removed_name_never_parses_and_its_replacement_always_exists() {
        // A removed name clap still accepts would never reach the hint, and a
        // replacement that is neither built in nor a plugin command would
        // send the user to a second unknown command.
        let (still_parsed, missing) = on_big_stack(|| {
            let cli = crate::Cli::command();
            let still: Vec<&str> = REMOVED_COMMANDS
                .iter()
                .map(|(removed, _)| *removed)
                .filter(|removed| cli.find_subcommand(removed).is_some())
                .collect();
            let missing: Vec<&str> = REMOVED_COMMANDS
                .iter()
                .map(|(_, current)| *current)
                .filter(|current| {
                    cli.find_subcommand(current).is_none() && !pixel_plugin::is_moved(current)
                })
                .collect();
            (still, missing)
        });
        assert_eq!(still_parsed, Vec::<&str>::new());
        assert_eq!(missing, Vec::<&str>::new());
    }

    #[test]
    fn a_removed_name_is_never_a_moved_command() {
        // A moved command gets the plugin hint first; listing it here too
        // would make the two hints disagree.
        for (removed, _) in REMOVED_COMMANDS {
            assert!(!pixel_plugin::is_moved(removed), "{removed}");
        }
    }

    #[test]
    fn a_built_in_replacement_is_named_alone() {
        assert_eq!(
            removed_hint("ready", &MOVED_COMMANDS, |_| false).as_deref(),
            Some("'ready' was removed in 0.8.0; use `pixel prepare-repo`")
        );
        // check-release is published by a plugin but stays in the core.
        assert_eq!(
            removed_hint("release-check", &MOVED_COMMANDS, |_| false).as_deref(),
            Some("'release-check' was removed in 0.8.0; use `pixel check-release`")
        );
    }

    #[test]
    fn a_plugin_replacement_carries_its_install_command_until_installed() {
        let mut asked = None;
        assert_eq!(
            removed_hint("history", &MOVED_COMMANDS, |command| {
                asked = Some(command.to_owned());
                false
            })
            .as_deref(),
            Some(
                "'history' was removed in 0.8.0; use `pixel commit-history`, a plugin command: pixel install github:Pixel-CLI/pixel-git"
            )
        );
        assert_eq!(asked.as_deref(), Some("commit-history"));
        assert_eq!(
            removed_hint("history", &MOVED_COMMANDS, |_| true).as_deref(),
            Some("'history' was removed in 0.8.0; use `pixel commit-history`")
        );
    }

    #[test]
    fn a_word_outside_the_table_gets_no_hint() {
        assert_eq!(replacement("prepare-repo"), None);
        assert_eq!(
            removed_hint("no-such-command", &MOVED_COMMANDS, |_| false),
            None
        );
    }
}
