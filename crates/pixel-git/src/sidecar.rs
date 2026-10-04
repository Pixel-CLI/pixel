//! The trust boundary of a repository's `.pixel/` directory.
//!
//! Everything pixel keeps per repository (index shards, the graph and
//! history databases, the action log, task state) lives under
//! `<root>/.pixel/`, which pixel creates and git ignores. A repository can
//! still commit that directory, links included, and a clone then hands
//! pixel files it did not write: files whose integrity markers are
//! computable from public code, or links that redirect pixel's own writes.
//! So before a store trusts what it reads there, [`check`] refuses a
//! `.pixel` that is a link, or that holds anything git tracks; and every
//! directory pixel creates there goes through [`private_dir`], which refuses
//! a link instead of creating or chmodding through it.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::{GitError, GitRunner};

/// The per-repository state directory, relative to the repository root.
pub const DIR_NAME: &str = ".pixel";

/// Mode of every directory pixel creates for its state: owner-only.
pub const PRIVATE_DIR_MODE: u32 = 0o700;

/// The `ls-files` output cap: a list this long is refused whatever it holds,
/// and the message names only the first entries anyway.
const TRACKED_OUTPUT_CAP: usize = 65_536;

/// How many tracked entries the refusal names.
const NAMED_ENTRIES: usize = 3;

/// Why a `.pixel` directory was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// `.pixel` (or a directory pixel was about to create) is a symbolic
    /// link.
    Linked(PathBuf),
    /// The name exists and is not a directory.
    NotDirectory(PathBuf),
    /// Git tracks entries under `.pixel`: the directory came from the
    /// repository, not from pixel. `entries` holds the first few, `more`
    /// whether git listed others (or more than the output cap).
    Tracked {
        dir: PathBuf,
        entries: Vec<String>,
        more: bool,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Linked(path) => write!(
                f,
                "refusing {}: it is a symbolic link, and pixel keeps its state only in a real \
                 directory; remove the link and re-run",
                path.display()
            ),
            Refusal::NotDirectory(path) => write!(
                f,
                "refusing {}: it is not a directory; remove it and re-run",
                path.display()
            ),
            Refusal::Tracked { dir, entries, more } => {
                let more = if *more { ", …" } else { "" };
                write!(
                    f,
                    "refusing {}: git tracks files under it ({}{more}), so it comes from the \
                     repository rather than from pixel and is not trusted; delete it \
                     (`rm -rf {DIR_NAME}`) and, if the repository commits it, untrack it \
                     (`git rm -r --cached {DIR_NAME}`), then re-run",
                    dir.display(),
                    entries.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for Refusal {}

impl From<Refusal> for io::Error {
    fn from(refusal: Refusal) -> Self {
        io::Error::new(io::ErrorKind::PermissionDenied, refusal)
    }
}

/// The `.pixel` directory of `root`.
pub fn dir(root: &Path) -> PathBuf {
    root.join(DIR_NAME)
}

/// What sits at `path`, without following a link.
enum Entry {
    Absent,
    Directory,
    Refused(Refusal),
}

fn entry(path: &Path) -> io::Result<Entry> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Ok(Entry::Refused(Refusal::Linked(path.to_path_buf())))
        }
        Ok(meta) if !meta.is_dir() => Ok(Entry::Refused(Refusal::NotDirectory(path.to_path_buf()))),
        Ok(_) => Ok(Entry::Directory),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Entry::Absent),
        Err(error) => Err(error),
    }
}

/// The entries git tracks under `root/.pixel`, at most [`NAMED_ENTRIES`] of
/// them, and whether there are more. Outside a repository (or when git
/// cannot run) nothing is tracked: no clone could have planted anything.
fn tracked_entries(root: &Path) -> (Vec<String>, bool) {
    let runner = GitRunner::new(root).with_max_output_bytes(Some(TRACKED_OUTPUT_CAP));
    match runner.run(&["ls-files", "-z", "--", DIR_NAME]) {
        Ok(out) => {
            let mut names = out
                .split(|&b| b == 0)
                .filter(|name| !name.is_empty())
                .map(|name| String::from_utf8_lossy(name).into_owned());
            let entries: Vec<String> = names.by_ref().take(NAMED_ENTRIES).collect();
            let more = names.next().is_some();
            (entries, more)
        }
        Err(GitError::OutputTooLarge { .. }) => (vec![format!("{DIR_NAME}/…")], true),
        Err(_) => (Vec::new(), false),
    }
}

/// Refuse to use `root/.pixel` when it is a symbolic link, not a directory,
/// or holds entries git tracks. An absent `.pixel` passes without running
/// git. Stores call this once when they open, before trusting any file
/// there.
///
/// # Errors
///
/// A [`Refusal`] wrapped in a `PermissionDenied` [`io::Error`], or the
/// error of reading the entry's metadata.
pub fn check(root: &Path) -> io::Result<()> {
    let path = dir(root);
    match entry(&path)? {
        Entry::Absent => return Ok(()),
        Entry::Refused(refusal) => return Err(refusal.into()),
        Entry::Directory => {}
    }
    let (entries, more) = tracked_entries(root);
    if entries.is_empty() {
        return Ok(());
    }
    Err(Refusal::Tracked {
        dir: path,
        entries,
        more,
    }
    .into())
}

/// Create `dir` (and missing parents) owner-only, or accept it when it
/// exists; then set it to [`PRIVATE_DIR_MODE`] through a descriptor opened
/// without following a link. A link or a non-directory at `dir` is refused
/// rather than created or chmodded through.
///
/// # Errors
///
/// A [`Refusal`] wrapped in a `PermissionDenied` [`io::Error`], the refusal
/// of [`crate::nofollow::refuse_linked_ancestors`], or the I/O error of the
/// creation.
pub fn private_dir(dir: &Path) -> io::Result<()> {
    crate::nofollow::refuse_linked_ancestors(dir)?;
    if let Entry::Refused(refusal) = entry(dir)? {
        return Err(refusal.into());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIR_MODE)
        .create(dir)?;
    let handle: File = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir)?;
    if !handle.metadata()?.is_dir() {
        return Err(Refusal::NotDirectory(dir.to_path_buf()).into());
    }
    // Best effort, as a chmod by path was: a directory pixel cannot chmod
    // still holds its state.
    let _ = handle.set_permissions(fs::Permissions::from_mode(PRIVATE_DIR_MODE));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;

    use super::*;
    use crate::testutil::{git, init_repo, tmpdir};

    fn refusal(error: &io::Error) -> Refusal {
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error:?}");
        error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<Refusal>())
            .cloned()
            .expect("a Refusal")
    }

    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn check_should_pass_without_creating_an_absent_pixel_dir() {
        let root = tmpdir("check-absent");
        check(&root).unwrap();
        assert!(fs::symlink_metadata(dir(&root)).is_err());
    }

    #[test]
    fn check_should_refuse_a_linked_or_non_directory_pixel() {
        let root = tmpdir("check-linked");
        let elsewhere = tmpdir("check-linked-target");
        symlink(&elsewhere, dir(&root)).unwrap();
        assert_eq!(
            refusal(&check(&root).unwrap_err()),
            Refusal::Linked(dir(&root))
        );
        let file_root = tmpdir("check-file");
        fs::write(dir(&file_root), b"").unwrap();
        assert_eq!(
            refusal(&check(&file_root).unwrap_err()),
            Refusal::NotDirectory(dir(&file_root))
        );
    }

    #[test]
    fn check_should_refuse_a_pixel_dir_holding_tracked_files() {
        let root = tmpdir("check-tracked");
        init_repo(&root);
        fs::create_dir_all(dir(&root)).unwrap();
        fs::write(root.join(".pixel/history.db"), b"forged").unwrap();
        git(&root, &["add", "-f", ".pixel/history.db"]);
        let error = check(&root).unwrap_err();
        assert_eq!(
            refusal(&error),
            Refusal::Tracked {
                dir: dir(&root),
                entries: vec![".pixel/history.db".to_string()],
                more: false,
            }
        );
        let message = error.to_string();
        assert!(message.contains("rm -rf .pixel"), "{message}");
        assert!(message.contains("git rm -r --cached .pixel"), "{message}");
        assert!(!message.contains('…'), "{message}");
    }

    #[test]
    fn check_should_name_three_tracked_entries_and_flag_the_rest() {
        let root = tmpdir("check-more");
        init_repo(&root);
        fs::create_dir_all(dir(&root)).unwrap();
        for name in ["a", "b", "c", "d"] {
            fs::write(root.join(".pixel").join(name), name).unwrap();
        }
        git(&root, &["add", "-f", ".pixel"]);
        let Refusal::Tracked { entries, more, .. } = refusal(&check(&root).unwrap_err()) else {
            panic!("expected a tracked refusal");
        };
        assert_eq!(entries, vec![".pixel/a", ".pixel/b", ".pixel/c"]);
        assert!(more);
    }

    #[test]
    fn check_should_refuse_a_tracked_link_named_pixel() {
        let root = tmpdir("check-tracked-link");
        init_repo(&root);
        symlink("elsewhere", dir(&root)).unwrap();
        git(&root, &["add", "-f", ".pixel"]);
        assert_eq!(
            refusal(&check(&root).unwrap_err()),
            Refusal::Linked(dir(&root))
        );
    }

    #[test]
    fn check_should_accept_an_untracked_pixel_dir_inside_and_outside_a_repository() {
        let root = tmpdir("check-untracked");
        init_repo(&root);
        fs::write(root.join("a.rs"), b"fn main() {}\n").unwrap();
        git(&root, &["add", "a.rs"]);
        fs::create_dir_all(dir(&root)).unwrap();
        fs::write(root.join(".pixel/history.db"), b"ours").unwrap();
        check(&root).unwrap();
        let plain = tmpdir("check-no-git");
        fs::create_dir_all(dir(&plain)).unwrap();
        check(&plain).unwrap();
    }

    #[test]
    fn private_dir_should_create_and_tighten_owner_only() {
        let root = tmpdir("private-dir");
        let pixel = dir(&root);
        private_dir(&pixel).unwrap();
        assert_eq!(mode_of(&pixel), PRIVATE_DIR_MODE);
        fs::set_permissions(&pixel, fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&pixel).unwrap();
        assert_eq!(mode_of(&pixel), PRIVATE_DIR_MODE);
        let nested = pixel.join("tasks/t1");
        private_dir(&nested).unwrap();
        assert_eq!(mode_of(&nested), PRIVATE_DIR_MODE);
    }

    #[test]
    fn check_should_report_an_entry_it_cannot_read() {
        let base = tmpdir("check-unreadable");
        let file_root = base.join("not-a-dir");
        fs::write(&file_root, b"").unwrap();
        let error = check(&file_root).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENOTDIR), "{error:?}");
    }

    #[test]
    fn private_dir_should_not_create_below_a_linked_directory_under_pixel() {
        let root = tmpdir("private-dir-below-link");
        let elsewhere = tmpdir("private-dir-below-link-target");
        fs::create_dir_all(dir(&root)).unwrap();
        symlink(&elsewhere, dir(&root).join("tasks")).unwrap();
        let error = private_dir(&dir(&root).join("tasks/session-locks")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error:?}");
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());
    }

    #[test]
    fn private_dir_should_neither_create_nor_chmod_through_a_link() {
        let root = tmpdir("private-dir-link");
        let elsewhere = tmpdir("private-dir-link-target");
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&elsewhere, dir(&root)).unwrap();
        let error = private_dir(&dir(&root)).unwrap_err();
        assert_eq!(refusal(&error), Refusal::Linked(dir(&root)));
        assert_eq!(mode_of(&elsewhere), 0o755);
        let file = root.join("plain");
        fs::write(&file, b"").unwrap();
        assert_eq!(
            refusal(&private_dir(&file).unwrap_err()),
            Refusal::NotDirectory(file)
        );
    }
}
