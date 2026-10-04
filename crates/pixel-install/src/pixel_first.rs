// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The Pixel-first retrieval rule in a repository's root `AGENTS.md`.
//!
//! `pixel install --repo` writes one managed block telling every agent that
//! reads `AGENTS.md` (Codex, Claude Code through a `CLAUDE.md` symlink, and
//! any other) to try Pixel before native retrieval, without ever blocking the
//! native tools. Text outside the markers belongs to the user and is kept
//! byte for byte. Inside them, only the words are Pixel's: a block the user
//! re-wrapped (a Markdown formatter, an 80-column habit) is current and left
//! alone, so `install --repo` does not dirty a tree that commits `AGENTS.md`.

use std::fs;
use std::path::Path;

use crate::InstallError;
use crate::config;
use crate::install::{self, CheckStatus, InstallStep, Result};

/// The managed Pixel-first guidance block in a project-root `AGENTS.md`.
///
/// The markers keep the `warp-retrieval` spelling they shipped with: every
/// installed `AGENTS.md` carries it, and a new spelling would leave those
/// blocks unmanaged beside a second copy.
const RULES_BEGIN: &str = "<!-- pixel:warp-retrieval:begin -->";
const RULES_END: &str = "<!-- pixel:warp-retrieval:end -->";
const RULES_BODY: &str = "This repository has a Pixel index (`.pixel/`). Retrieval starts with Pixel: `pixel search-content -F '<identifier>'` for exact identifiers, `pixel find-code '<concept>'` for behavior-described code, and `pixel impact '<symbol>'` before renames — a native grep/rg over indexed code is a missed retrieval; native tools stay available for everything Pixel does not cover, and two fruitless pixel calls mean switch to grep.\n";

/// Install Pixel-first retrieval guidance in the repository's root `AGENTS.md`.
pub(crate) fn install_rules(repo: &Path, dry_run: bool) -> Result<InstallStep> {
    let path = repo.join("AGENTS.md");
    let existing = read_rules_file(&path)?;
    let block = rules_block();
    let updated = match rules_range(&existing, &path)? {
        Some(range) if block_is_current(&existing[range.clone()]) => existing.clone(),
        Some(range) => format!(
            "{}{}{}",
            &existing[..range.start],
            block,
            &existing[range.end..]
        ),
        None if existing.is_empty() => block,
        None => format!(
            "{}{}{}",
            existing,
            if existing.ends_with('\n') { "" } else { "\n" },
            block
        ),
    };
    let changed = updated != existing;
    if changed && !dry_run {
        config::backup_if_changing(&path, updated.as_bytes())?;
        fs::write(&path, updated)?;
    }
    Ok(InstallStep {
        id: "rules.pixel-first".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(
            dry_run,
            if changed {
                "Pixel-first project retrieval guidance installed"
            } else {
                "Pixel-first project retrieval guidance already current"
            },
        ),
        detail: Some(format!("file={}", path.display())),
    })
}

/// Remove Pixel's managed retrieval guidance while preserving all other text.
pub(crate) fn uninstall_rules(repo: &Path, dry_run: bool) -> Result<InstallStep> {
    let path = repo.join("AGENTS.md");
    let existing = read_rules_file(&path)?;
    let updated = rules_range(&existing, &path)?.map_or_else(
        || existing.clone(),
        |range| format!("{}{}", &existing[..range.start], &existing[range.end..]),
    );
    let changed = updated != existing;
    if changed && !dry_run {
        config::backup_if_changing(&path, updated.as_bytes())?;
        fs::write(&path, updated)?;
    }
    Ok(InstallStep {
        id: "rules.pixel-first".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(
            dry_run,
            if changed {
                "Pixel-first project retrieval guidance removed"
            } else {
                "no Pixel-first project retrieval guidance found"
            },
        ),
        detail: Some(format!("file={}", path.display())),
    })
}

/// Report whether the managed project guidance is absent, current, or stale.
pub(crate) fn check_rules(repo: &Path) -> Result<Option<bool>> {
    let path = repo.join("AGENTS.md");
    let existing = read_rules_file(&path)?;
    let Some(range) = rules_range(&existing, &path)? else {
        return Ok(None);
    };
    Ok(Some(block_is_current(&existing[range])))
}

/// Whether an installed block says what [`rules_block`] says, line breaks and
/// runs of spaces aside: those are layout, not policy, and rewriting a
/// re-wrapped block would back up and rewrite the file on every install.
fn block_is_current(installed: &str) -> bool {
    installed
        .split_whitespace()
        .eq(rules_block().split_whitespace())
}

fn read_rules_file(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

fn rules_block() -> String {
    format!("{RULES_BEGIN}\n{RULES_BODY}{RULES_END}")
}

use std::ops::Range;

fn rules_range(content: &str, path: &Path) -> Result<Option<Range<usize>>> {
    let begin = content.find(RULES_BEGIN);
    let end = content.find(RULES_END);
    if begin.is_none() && end.is_none() {
        return Ok(None);
    }
    if content.matches(RULES_BEGIN).count() != 1 || content.matches(RULES_END).count() != 1 {
        return Err(invalid_config(
            path,
            "AGENTS.md contains incomplete or duplicate Pixel retrieval markers",
        ));
    }
    let begin = begin.expect("checked for exactly one begin marker");
    let end = end.expect("checked for exactly one end marker");
    if begin >= end {
        return Err(invalid_config(
            path,
            "AGENTS.md Pixel retrieval markers are out of order",
        ));
    }
    let range_end = end + RULES_END.len();
    if content[begin..range_end].contains('\r') {
        return Err(invalid_config(
            path,
            "AGENTS.md Pixel retrieval block has unexpected line endings",
        ));
    }
    Ok(Some(begin..range_end))
}

fn invalid_config(path: &Path, reason: impl ToString) -> InstallError {
    InstallError::InvalidSettings {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    #[test]
    fn install_rules_should_append_and_preserve_existing_project_instructions() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join("AGENTS.md");
        let original = "# Project instructions\nKeep this text.\n";
        fs::write(&path, original).unwrap();

        install_rules(repo.path(), false).unwrap();

        let installed = fs::read_to_string(&path).unwrap();
        assert!(installed.starts_with(original));
        assert!(installed.contains("pixel search-content -F '<identifier>'"));
        assert!(installed.contains("native tools stay available"));
        assert!(installed.contains("two fruitless pixel calls"));
        assert_eq!(check_rules(repo.path()).unwrap(), Some(true));
    }

    #[test]
    fn install_rules_should_be_idempotent_and_dry_run_should_not_write() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join("AGENTS.md");

        let preview = install_rules(repo.path(), true).unwrap();
        assert!(preview.summary.starts_with("[dry-run]"));
        assert!(!path.exists());
        install_rules(repo.path(), false).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            rules_block(),
            "a repository without AGENTS.md gets the block alone, without filler"
        );
        let before = fs::read(&path).unwrap();
        install_rules(repo.path(), false).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(check_rules(repo.path()).unwrap(), Some(true));
    }

    #[test]
    fn check_rules_should_mark_changed_managed_content_stale() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join("AGENTS.md");
        fs::write(
            &path,
            rules_block().replace("missed retrieval", "optional retrieval"),
        )
        .unwrap();
        assert_eq!(check_rules(repo.path()).unwrap(), Some(false));

        install_rules(repo.path(), false).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), rules_block());
        assert_eq!(check_rules(repo.path()).unwrap(), Some(true));
    }

    /// The block re-wrapped by hand: every sentence break becomes a line
    /// break plus an indent, so both newlines and runs of spaces differ.
    fn reflowed_block() -> String {
        let reflowed = rules_block().replace(". ", ".\n  ");
        assert_ne!(
            reflowed,
            rules_block(),
            "the fixture must change the layout"
        );
        reflowed
    }

    fn backups_in(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".pixel-bak."))
            .collect()
    }

    #[test]
    fn install_rules_should_leave_a_reflowed_block_untouched_without_backup() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join("AGENTS.md");
        let original = format!("# Project\n\n{}\n", reflowed_block());
        fs::write(&path, &original).unwrap();

        let step = install_rules(repo.path(), false).unwrap();

        assert_eq!(
            step.summary,
            "Pixel-first project retrieval guidance already current"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(backups_in(repo.path()), Vec::<String>::new());
        assert_eq!(check_rules(repo.path()).unwrap(), Some(true));
    }

    #[test]
    fn install_rules_should_replace_a_reflowed_block_whose_words_changed() {
        // A changed word, and two words glued together: both are policy
        // edits, whatever the layout around them.
        for edited in [
            reflowed_block().replace("missed retrieval", "optional retrieval"),
            reflowed_block().replace("fruitless pixel", "fruitlesspixel"),
        ] {
            let repo = tempfile::tempdir().unwrap();
            let path = repo.path().join("AGENTS.md");
            assert_ne!(edited, reflowed_block(), "the fixture must edit a word");
            fs::write(&path, &edited).unwrap();
            assert_eq!(check_rules(repo.path()).unwrap(), Some(false));

            let step = install_rules(repo.path(), false).unwrap();

            assert_eq!(
                step.summary,
                "Pixel-first project retrieval guidance installed"
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), rules_block());
            let backups = backups_in(repo.path());
            assert_eq!(backups.len(), 1, "{backups:?}");
            assert_eq!(
                fs::read_to_string(repo.path().join(&backups[0])).unwrap(),
                edited
            );
        }
    }

    #[test]
    fn rules_lifecycle_should_leave_an_untouched_tree_alone() {
        let repo = tempfile::tempdir().unwrap();
        uninstall_rules(repo.path(), false).unwrap();
        assert!(
            !repo.path().join("AGENTS.md").exists(),
            "nothing to remove must not create an empty AGENTS.md"
        );
    }

    #[test]
    fn rules_should_report_an_unreadable_agents_md_instead_of_treating_it_as_absent() {
        let repo = tempfile::tempdir().unwrap();
        // A directory is not a missing file: treating the read error as
        // "nothing installed" would report a green check for a broken tree.
        fs::create_dir(repo.path().join("AGENTS.md")).unwrap();
        assert!(check_rules(repo.path()).is_err());
    }

    #[test]
    fn rules_lifecycle_should_refuse_malformed_markers_without_modifying_bytes() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join("AGENTS.md");
        let original = "# Keep this\n<!-- pixel:warp-retrieval:begin -->\nuser edit\n";
        fs::write(&path, original).unwrap();

        assert!(matches!(
            install_rules(repo.path(), false),
            Err(InstallError::InvalidSettings { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(matches!(
            uninstall_rules(repo.path(), false),
            Err(InstallError::InvalidSettings { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(matches!(
            check_rules(repo.path()),
            Err(InstallError::InvalidSettings { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
    }

    #[test]
    fn uninstall_rules_should_remove_only_the_managed_block() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join("AGENTS.md");
        let before = "# Before\nuser-owned leading instructions\n";
        let after = "\nuser-owned trailing instructions\n";
        fs::write(&path, format!("{before}{}{after}", rules_block())).unwrap();

        uninstall_rules(repo.path(), false).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{before}{after}")
        );
        assert_eq!(check_rules(repo.path()).unwrap(), None);
    }
}
