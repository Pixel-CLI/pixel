// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The retired Pi project extension of `pixel install --repo`.
//!
//! Releases up to this one wrote a guard extension under the repository's
//! `.pi/extensions/` (and, up to 0.4.0, under `<repo>/.pi/agent/`, a
//! directory Pi reads only under `~`). Pi now keeps its native tools: the
//! only Pixel capability left in Pi is the explicit `/pixel-impact` command
//! of the global package (`pi_global`). Both `install --repo` and `uninstall
//! --repo` take Pixel's files out of the repository, and doctor reports one
//! still there. A file Pixel did not write is the user's and stays.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config;
use crate::install::{self, CheckStatus, InstallStep, Result};

/// The retired guard extension, relative to the repository.
pub(crate) const EXTENSION: &str = ".pi/extensions/pixel-guard.ts";

/// The directory, relative to the repository, where releases up to 0.4.0
/// wrote the guard (`extensions/pixel-guard.ts`) and a rules block
/// (`AGENTS.md`).
pub(crate) const LEGACY_DIR: &str = ".pi/agent";

/// Whether `path` is a guard extension pixel wrote: a file carrying the
/// managed marker. Anything else under that name belongs to the user.
fn is_managed_extension(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|text| text.contains(config::MANAGED_BEGIN))
}

/// `pixel install --repo` and `pixel uninstall --repo` step: remove the
/// guard Pixel wrote at [`EXTENSION`] and Pixel's files in [`LEGACY_DIR`].
pub(crate) fn remove(repo: &Path, dry_run: bool) -> Result<InstallStep> {
    let ext_file = repo.join(EXTENSION);
    let mut removed = remove_legacy(repo, dry_run)?;
    if is_managed_extension(&ext_file) {
        if !dry_run {
            fs::remove_file(&ext_file)?;
            remove_empty_dirs(&[&repo.join(".pi/extensions"), &repo.join(".pi")]);
        }
        removed.insert(0, EXTENSION.to_string());
    }
    let summary = if removed.is_empty() {
        "no Pixel Pi project extension found".to_string()
    } else {
        format!(
            "removed the retired Pi project extension: {}",
            removed.join(" ")
        )
    };
    Ok(InstallStep {
        id: "hooks.pi".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(format!("ext={}", ext_file.display())),
    })
}

/// Take pixel's files out of `<repo>/`[`LEGACY_DIR`]: the managed guard is
/// deleted, the managed block leaves `AGENTS.md` (deleted when nothing else
/// is left in it, backed up otherwise), and the directories are removed once
/// empty. Returns the repository-relative paths touched.
fn remove_legacy(repo: &Path, dry_run: bool) -> Result<Vec<String>> {
    let dir = repo.join(LEGACY_DIR);
    let ext_file = dir.join("extensions").join("pixel-guard.ts");
    let agents_md = dir.join("AGENTS.md");
    let mut removed = Vec::new();
    if is_managed_extension(&ext_file) {
        if !dry_run {
            fs::remove_file(&ext_file)?;
        }
        removed.push(format!("{LEGACY_DIR}/extensions/pixel-guard.ts"));
    }
    // Best effort: an unreadable legacy file holds nothing pi ever loaded.
    let agents = fs::read_to_string(&agents_md).unwrap_or_default();
    if agents.contains(config::MANAGED_BEGIN) {
        let rest = config::strip_managed_block(&agents);
        if !dry_run {
            if rest.trim().is_empty() {
                fs::remove_file(&agents_md)?;
            } else {
                config::backup_if_changing(&agents_md, rest.as_bytes())?;
                fs::write(&agents_md, &rest)?;
            }
        }
        removed.push(format!("{LEGACY_DIR}/AGENTS.md"));
    }
    if !dry_run {
        remove_empty_dirs(&[&dir.join("extensions"), &dir]);
    }
    Ok(removed)
}

/// Remove each directory in order when it is empty; a directory that still
/// holds anything, or is absent, is left as it is.
fn remove_empty_dirs(dirs: &[&Path]) {
    for dir in dirs {
        // `remove_dir` refuses a non-empty directory, which is the rule here.
        let _ = fs::remove_dir(dir);
    }
}

/// What of the retired Pi project extension a repository still holds, as
/// `pixel doctor` reports it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GuardState {
    /// Nothing of Pixel's.
    Absent,
    /// The guard Pixel wrote at [`EXTENSION`].
    Retired(PathBuf),
    /// Only the guard of an older release, in [`LEGACY_DIR`].
    Legacy(PathBuf),
}

/// Where the repository's retired Pi guard stands; a file at [`EXTENSION`]
/// that Pixel did not write counts as absent.
pub(crate) fn guard_state(repo: &Path) -> GuardState {
    let ext_file = repo.join(EXTENSION);
    if is_managed_extension(&ext_file) {
        return GuardState::Retired(ext_file);
    }
    let legacy = repo
        .join(LEGACY_DIR)
        .join("extensions")
        .join("pixel-guard.ts");
    if is_managed_extension(&legacy) {
        GuardState::Legacy(legacy)
    } else {
        GuardState::Absent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_ext(repo: &Path) -> PathBuf {
        repo.join(LEGACY_DIR)
            .join("extensions")
            .join("pixel-guard.ts")
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn managed(text: &str) -> String {
        format!(
            "{}\n{text}\n{}\n",
            config::MANAGED_BEGIN,
            config::MANAGED_END
        )
    }

    #[test]
    fn remove_should_take_out_the_guard_and_the_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(&repo.join(EXTENSION), &managed("guard"));
        write(&legacy_ext(repo), &managed("old guard"));
        write(
            &repo.join(LEGACY_DIR).join("AGENTS.md"),
            &managed("old rules"),
        );

        let step = remove(repo, false).unwrap();

        assert_eq!(step.status, CheckStatus::Green);
        assert!(!repo.join(".pi").exists(), "empty .pi dirs must go");
        assert_eq!(
            step.summary,
            "removed the retired Pi project extension: .pi/extensions/pixel-guard.ts \
             .pi/agent/extensions/pixel-guard.ts .pi/agent/AGENTS.md"
        );
        assert_eq!(guard_state(repo), GuardState::Absent);
    }

    #[test]
    fn remove_should_keep_user_text_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let agents = repo.join(LEGACY_DIR).join("AGENTS.md");
        write(&agents, &format!("mine\n{}", managed("old rules")));
        write(
            &repo.join(LEGACY_DIR).join("extensions/other.ts"),
            "user ext",
        );
        write(&legacy_ext(repo), &managed("old guard"));
        write(&repo.join(EXTENSION), "the user's own guard");
        write(&repo.join(".pi/extensions/mine.ts"), "user ext");

        remove(repo, false).unwrap();

        assert_eq!(fs::read_to_string(&agents).unwrap(), "mine\n");
        assert!(repo.join(LEGACY_DIR).join("extensions/other.ts").is_file());
        assert!(!legacy_ext(repo).exists());
        assert_eq!(
            fs::read_to_string(repo.join(EXTENSION)).unwrap(),
            "the user's own guard"
        );
        assert!(repo.join(".pi/extensions/mine.ts").is_file());
    }

    #[test]
    fn remove_should_report_a_repository_without_pixel_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(&legacy_ext(repo), "the user's own guard");
        write(&repo.join(LEGACY_DIR).join("AGENTS.md"), "the user's rules");

        let step = remove(repo, false).unwrap();

        assert_eq!(step.summary, "no Pixel Pi project extension found");
        assert_eq!(
            fs::read_to_string(legacy_ext(repo)).unwrap(),
            "the user's own guard"
        );
        assert_eq!(
            fs::read_to_string(repo.join(LEGACY_DIR).join("AGENTS.md")).unwrap(),
            "the user's rules"
        );
    }

    #[test]
    fn remove_dry_run_should_report_without_removing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(&repo.join(EXTENSION), &managed("guard"));

        let step = remove(repo, true).unwrap();

        assert!(repo.join(EXTENSION).is_file());
        assert_eq!(
            step.summary,
            "[dry-run] would report: removed the retired Pi project extension: .pi/extensions/pixel-guard.ts"
        );
    }

    #[test]
    fn remove_empty_dirs_should_keep_a_directory_that_holds_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let full = dir.path().join("full");
        let empty = dir.path().join("empty");
        write(&full.join("f"), "x");
        fs::create_dir_all(&empty).unwrap();

        remove_empty_dirs(&[&full, &empty]);

        assert!(full.join("f").is_file());
        assert!(!empty.exists());
    }

    #[test]
    fn guard_state_should_tell_every_case_apart() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        assert_eq!(guard_state(repo), GuardState::Absent);

        write(&legacy_ext(repo), "not pixel's");
        assert_eq!(guard_state(repo), GuardState::Absent);

        write(&legacy_ext(repo), &managed("old guard"));
        assert_eq!(guard_state(repo), GuardState::Legacy(legacy_ext(repo)));

        write(&repo.join(EXTENSION), "not pixel's");
        assert_eq!(guard_state(repo), GuardState::Legacy(legacy_ext(repo)));

        write(&repo.join(EXTENSION), &managed("guard"));
        assert_eq!(guard_state(repo), GuardState::Retired(repo.join(EXTENSION)));
    }
}
