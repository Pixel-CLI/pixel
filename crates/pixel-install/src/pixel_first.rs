// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The Pixel-first retrieval rule in a repository's root `AGENTS.md`.
//!
//! `pixel install --repo` removes the managed Pixel block from `AGENTS.md`.
//! Text outside the markers belongs to the user and is kept byte for byte.

use std::fs;
use std::path::Path;

use crate::InstallError;
use crate::config;
use crate::install::{self, CheckStatus, InstallStep, Result};

/// The retired Pixel-first guidance block in a project-root `AGENTS.md`.
const RULES_BEGIN: &str = "<!-- pixel:warp-retrieval:begin -->";
const RULES_END: &str = "<!-- pixel:warp-retrieval:end -->";
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
        if updated.trim().is_empty() {
            fs::remove_file(&path)?;
        } else {
            fs::write(&path, updated)?;
        }
    }
    Ok(InstallStep {
        id: "rules.pixel-first".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(
            dry_run,
            if changed {
                "retired Pixel-first project guidance removed"
            } else {
                "no retired Pixel-first project guidance found"
            },
        ),
        detail: Some(format!("file={}", path.display())),
    })
}

/// Report whether the retired managed project guidance is present.
pub(crate) fn check_rules(repo: &Path) -> Result<Option<bool>> {
    let path = repo.join("AGENTS.md");
    let existing = read_rules_file(&path)?;
    if rules_range(&existing, &path)?.is_none() {
        return Ok(None);
    }
    Ok(Some(false))
}

fn read_rules_file(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
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
    fn removal_should_preserve_project_instructions_around_the_retired_block() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join("AGENTS.md");
        let before = "# Project instructions\nKeep this text.\n\n";
        let after = "\nKeep this trailing text.\n";
        let original = format!("{before}{RULES_BEGIN}\nold Pixel guidance\n{RULES_END}{after}");
        fs::write(&path, original).unwrap();

        let step = uninstall_rules(repo.path(), false).unwrap();

        assert_eq!(step.status, CheckStatus::Green);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{before}{after}")
        );
        assert_eq!(check_rules(repo.path()).unwrap(), None);
    }

    #[test]
    fn absent_file_is_not_created_and_dry_run_does_not_remove_the_block() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join("AGENTS.md");

        let preview = uninstall_rules(repo.path(), true).unwrap();
        assert!(preview.summary.starts_with("[dry-run]"));
        assert!(!path.exists());
        let original = format!("# Keep me\n\n{RULES_BEGIN}\nretired\n{RULES_END}\n");
        fs::write(&path, &original).unwrap();
        uninstall_rules(repo.path(), true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn block_only_file_is_removed_after_backup() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join("AGENTS.md");
        let original = format!("{RULES_BEGIN}\nretired\n{RULES_END}\n");
        fs::write(&path, &original).unwrap();

        uninstall_rules(repo.path(), false).unwrap();
        assert!(
            !repo.path().join("AGENTS.md").exists(),
            "a file containing only retired Pixel text is removed"
        );
        assert_eq!(check_rules(repo.path()).unwrap(), None);
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
        let malformed = [
            format!("# Keep\n{RULES_BEGIN}\npartial\n"),
            format!("# Keep\n{RULES_END}\n"),
            format!("# Keep\n{RULES_END}\n{RULES_BEGIN}\n"),
            format!("{RULES_BEGIN}\n{RULES_BEGIN}\n{RULES_END}\n"),
            format!("{RULES_BEGIN}\n{RULES_END}\n{RULES_END}\n"),
        ];
        for original in malformed {
            fs::write(&path, &original).unwrap();
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
    }

    #[test]
    fn uninstall_rules_should_remove_only_the_managed_block() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join("AGENTS.md");
        let before = "# Before\nuser-owned leading instructions\n";
        let after = "\nuser-owned trailing instructions\n";
        fs::write(
            &path,
            format!("{before}{RULES_BEGIN}\nretired\n{RULES_END}{after}"),
        )
        .unwrap();

        uninstall_rules(repo.path(), false).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{before}{after}")
        );
        assert_eq!(check_rules(repo.path()).unwrap(), None);
    }
}
