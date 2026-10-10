// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! CLI subcommands renamed by the clean-break rename (commit 08268b0).
//!
//! The rename gave every command a verb-first name and kept no alias, so
//! every script, hook entry and agent prompt written against 0.2.x broke on
//! the next build of the integration branch. This table is the single list of those
//! renames: the CLI registers each old name as a hidden clap alias (a test
//! in `pixel-cli` fails if the two drift), and the surfaces that read a
//! command name back as text (the action log, hook entries, the doctor's
//! scenario check) canonicalise through [`current_name`].
//!
//! Only CLI subcommand names live here. Protocol op tags (`"op": "search"`)
//! and JSON field names were never renamed and must not be.

/// `(old, new)` for every renamed subcommand, sorted by old name.
pub const RENAMED_COMMANDS: &[(&str, &str)] = &[
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
    ("stats", "index-stats"),
    ("symbol", "find-symbol"),
    ("sync", "fetch"),
    ("targets", "scope-task"),
    ("task", "task-state"),
    ("trace", "call-path"),
    ("update", "fast-forward"),
    ("upgrade", "self-update"),
    ("uses", "who-calls"),
];

/// The release that drops the old names. Until then each stays accepted.
pub const ALIAS_REMOVAL_VERSION: &str = "1.0";

/// The current name of a command `old` was renamed to, or `None` when
/// `old` is not a pre-rename name (including when it is already current).
pub fn renamed_to(old: &str) -> Option<&'static str> {
    RENAMED_COMMANDS
        .iter()
        .find(|(from, _)| *from == old)
        .map(|(_, to)| *to)
}

/// The pre-rename name of the current command `new`, if it has one.
pub fn former_name(new: &str) -> Option<&'static str> {
    RENAMED_COMMANDS
        .iter()
        .find(|(_, to)| *to == new)
        .map(|(from, _)| *from)
}

/// `name` under its current spelling: an old name maps to its new one,
/// anything else (a current name, an unknown word) is returned unchanged.
pub fn current_name(name: &str) -> &str {
    renamed_to(name).unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn old_and_new_names_are_unique_and_disjoint() {
        let old: HashSet<&str> = RENAMED_COMMANDS.iter().map(|(o, _)| *o).collect();
        let new: HashSet<&str> = RENAMED_COMMANDS.iter().map(|(_, n)| *n).collect();
        assert_eq!(
            old.len(),
            RENAMED_COMMANDS.len(),
            "an old name is listed twice"
        );
        assert_eq!(
            new.len(),
            RENAMED_COMMANDS.len(),
            "two commands share a new name"
        );
        // An old name that is also a current name would make `pixel <x>`
        // ambiguous: clap would reject the duplicate at startup.
        assert!(old.is_disjoint(&new), "{:?}", old.intersection(&new));
        let mut sorted = RENAMED_COMMANDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(
            sorted, RENAMED_COMMANDS,
            "keep the table sorted by old name"
        );
    }

    #[test]
    fn lookups_map_between_the_two_spellings() {
        assert_eq!(RENAMED_COMMANDS.len(), 42);
        assert_eq!(renamed_to("ready"), Some("prepare-repo"));
        assert_eq!(renamed_to("hook"), Some("run-hook"));
        assert_eq!(
            renamed_to("prepare-repo"),
            None,
            "a current name is not a rename"
        );
        assert_eq!(renamed_to("impact"), None, "never renamed");
        assert_eq!(former_name("prepare-repo"), Some("ready"));
        assert_eq!(former_name("sync-branch"), Some("reconcile"));
        assert_eq!(former_name("ready"), None);
        assert_eq!(former_name("impact"), None);
        assert_eq!(current_name("publish"), "commit");
        assert_eq!(current_name("commit"), "commit");
        assert_eq!(current_name("impact"), "impact");
        assert_eq!(current_name("no-such-command"), "no-such-command");
    }
}
