//! File opens that never follow a symbolic link at the file they name.
//!
//! Pixel writes its derived state under `<root>/.pixel/` and restores
//! working-tree files for `rename` and `plan-rollback`. A repository can
//! commit a symbolic link at any of those names, so an open that follows it
//! writes, truncates or chmods whatever file the link points at. Every
//! function here opens with `O_NOFOLLOW`, applies permissions through the
//! open descriptor (`fchmod`), never through the path, and, for a path under
//! a `.pixel` directory, also refuses a linked directory between `.pixel`
//! and the file (`.pixel/tasks -> ../..`), which `O_NOFOLLOW` alone does not
//! cover.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::sidecar::DIR_NAME;

/// Mode of every file pixel creates under `.pixel/`: owner-only, since the
/// action log and flow state may carry fill values (passwords, OTPs).
pub const PRIVATE_MODE: u32 = 0o600;

/// Distinguishes the temporary files of concurrent writers in one process.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Open options that refuse a link at the final name (`ELOOP`); the
/// standard library already opens every file close-on-exec.
fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.custom_flags(libc::O_NOFOLLOW);
    options
}

/// The error an open through a symbolic link reports, so a refused ancestor
/// reads like the `O_NOFOLLOW` refusal of the file itself.
fn linked(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "{} goes through a symbolic link; pixel does not follow links under {DIR_NAME}/",
            path.display()
        ),
    )
}

/// Refuse `path` when a directory strictly between the nearest `.pixel`
/// ancestor and the file is a symbolic link. Paths outside any `.pixel`
/// directory are not checked here: the caller confines them
/// (`crate::repo_path::confine`).
pub fn refuse_linked_ancestors(path: &Path) -> io::Result<()> {
    let names: Vec<Component<'_>> = path.components().collect();
    let Some(sidecar) = names.iter().rposition(|part| part.as_os_str() == DIR_NAME) else {
        return Ok(());
    };
    let mut dir: std::path::PathBuf = names[..=sidecar].iter().collect();
    // Every directory below `.pixel`, up to the file's parent.
    let below = &names[sidecar + 1..];
    for part in below.iter().take(below.len().saturating_sub(1)) {
        dir.push(part);
        if fs::symlink_metadata(&dir).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(linked(&dir));
        }
    }
    Ok(())
}

/// Reject a descriptor that is not a regular file (a FIFO or a device would
/// block or be written to), then apply `mode` through it, best effort: a
/// file pixel cannot chmod is still usable.
fn regular_with_mode(file: File, path: &Path, mode: Option<u32>) -> io::Result<File> {
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if let Some(mode) = mode {
        let _ = file.set_permissions(fs::Permissions::from_mode(mode));
    }
    Ok(file)
}

/// Open `path` for appending, creating it owner-only, without following a
/// link; an existing file is brought to [`PRIVATE_MODE`] through the
/// descriptor.
///
/// # Errors
///
/// `ELOOP` when `path` is a symbolic link, the refusal of
/// [`refuse_linked_ancestors`], or any I/O error of the open.
pub fn open_append(path: &Path) -> io::Result<File> {
    refuse_linked_ancestors(path)?;
    let file = options()
        .create(true)
        .append(true)
        .mode(PRIVATE_MODE)
        .open(path)?;
    regular_with_mode(file, path, Some(PRIVATE_MODE))
}

/// Open (creating owner-only) a lock file for `flock`, without truncating
/// it and without following a link.
///
/// # Errors
///
/// As [`open_append`].
pub fn open_lock(path: &Path) -> io::Result<File> {
    refuse_linked_ancestors(path)?;
    let file = options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(PRIVATE_MODE)
        .open(path)?;
    regular_with_mode(file, path, None)
}

/// Open an existing file for reading without following a link.
///
/// # Errors
///
/// As [`open_append`]; `NotFound` when the file is absent.
pub fn open_read(path: &Path) -> io::Result<File> {
    refuse_linked_ancestors(path)?;
    let file = options().read(true).open(path)?;
    regular_with_mode(file, path, None)
}

/// Open an existing file for an in-place rewrite (truncated, inode and mode
/// kept) without following a link: what `pixel rename` does to a source
/// file.
///
/// # Errors
///
/// As [`open_append`]; `NotFound` when the file is absent.
pub fn open_rewrite(path: &Path) -> io::Result<File> {
    refuse_linked_ancestors(path)?;
    let file = options().write(true).truncate(true).open(path)?;
    regular_with_mode(file, path, None)
}

/// Create `path` as a new file with `mode` (before the umask), failing when
/// any entry, a link included, already holds the name.
///
/// # Errors
///
/// `AlreadyExists` when the name is taken, the refusal of
/// [`refuse_linked_ancestors`], or any I/O error of the open.
pub fn create_new(path: &Path, mode: u32) -> io::Result<File> {
    refuse_linked_ancestors(path)?;
    options().write(true).create_new(true).mode(mode).open(path)
}

/// Replace `path` with `bytes` atomically: a fresh temporary file beside it
/// (never an existing name, so never a planted link), then a rename over
/// `path`, which replaces a link instead of writing through it. The new file
/// is created with `mode` (before the umask).
///
/// # Errors
///
/// The refusal of [`refuse_linked_ancestors`], or any I/O error of the
/// write or the rename; the temporary file is removed on failure.
pub fn write_replace(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let tmp = path.with_file_name(format!(
        ".{}.{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let written = create_new(&tmp, mode).and_then(|mut file| {
        file.write_all(bytes)?;
        file.flush()
    });
    match written.and_then(|()| fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::testutil::tmpdir;

    /// A file outside the repository that a planted link points at.
    struct Sentinel(PathBuf);

    impl Sentinel {
        const CONTENT: &'static [u8] = b"victim content\n";
        const MODE: u32 = 0o644;

        fn new(dir: &Path) -> Self {
            let path = dir.join("victim.txt");
            fs::write(&path, Self::CONTENT).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(Self::MODE)).unwrap();
            Sentinel(path)
        }

        /// Content and mode are both what the test wrote: neither a write,
        /// a truncation nor a chmod went through the link.
        fn assert_intact(&self) {
            assert_eq!(fs::read(&self.0).unwrap(), Self::CONTENT, "content changed");
            let mode = fs::metadata(&self.0).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, Self::MODE, "mode changed");
        }
    }

    /// A repository-like root with `.pixel/` and a sentinel beside it.
    fn layout(tag: &str) -> (PathBuf, PathBuf, Sentinel) {
        let base = tmpdir(tag);
        let root = base.join("repo");
        fs::create_dir_all(root.join(DIR_NAME)).unwrap();
        let sentinel = Sentinel::new(&base);
        (base, root, sentinel)
    }

    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn is_eloop(error: &io::Error) -> bool {
        error.raw_os_error() == Some(libc::ELOOP)
    }

    #[test]
    fn open_append_should_refuse_a_link_when_it_names_the_log() {
        let (_base, root, sentinel) = layout("append-link");
        let log = root.join(".pixel/actions.jsonl");
        symlink(&sentinel.0, &log).unwrap();
        let error = open_append(&log).unwrap_err();
        assert!(is_eloop(&error), "{error:?}");
        sentinel.assert_intact();
    }

    #[test]
    fn open_append_should_create_owner_only_and_tighten_an_existing_file() {
        let (_base, root, _sentinel) = layout("append-mode");
        let log = root.join(".pixel/actions.jsonl");
        open_append(&log).unwrap().write_all(b"one\n").unwrap();
        assert_eq!(mode_of(&log), PRIVATE_MODE);
        fs::set_permissions(&log, fs::Permissions::from_mode(0o644)).unwrap();
        open_append(&log).unwrap().write_all(b"two\n").unwrap();
        assert_eq!(mode_of(&log), PRIVATE_MODE);
        assert_eq!(fs::read(&log).unwrap(), b"one\ntwo\n");
    }

    #[test]
    fn open_append_should_refuse_a_directory() {
        let (_base, root, _sentinel) = layout("append-dir");
        let dir = root.join(".pixel/actions.jsonl");
        fs::create_dir(&dir).unwrap();
        assert!(open_append(&dir).is_err());
    }

    #[test]
    fn open_lock_should_neither_truncate_nor_create_through_a_link() {
        let (base, root, sentinel) = layout("lock-link");
        let lock = root.join(".pixel/history.db.lock");
        symlink(&sentinel.0, &lock).unwrap();
        assert!(is_eloop(&open_lock(&lock).unwrap_err()));
        sentinel.assert_intact();

        let missing = base.join("created-by-a-link");
        let dangling = root.join(".pixel/build.lock");
        symlink(&missing, &dangling).unwrap();
        assert!(is_eloop(&open_lock(&dangling).unwrap_err()));
        assert!(
            fs::symlink_metadata(&missing).is_err(),
            "the link target was created"
        );
    }

    #[test]
    fn open_lock_should_keep_an_existing_lock_file_content() {
        let (_base, root, _sentinel) = layout("lock-keep");
        let lock = root.join(".pixel/build.lock");
        fs::write(&lock, b"held").unwrap();
        drop(open_lock(&lock).unwrap());
        assert_eq!(fs::read(&lock).unwrap(), b"held");
        let fresh = root.join(".pixel/fresh.lock");
        drop(open_lock(&fresh).unwrap());
        assert_eq!(mode_of(&fresh), PRIVATE_MODE);
    }

    #[test]
    fn open_read_should_refuse_a_link_and_read_a_regular_file() {
        let (_base, root, sentinel) = layout("read");
        let link = root.join(".pixel/base.shard");
        symlink(&sentinel.0, &link).unwrap();
        assert!(is_eloop(&open_read(&link).unwrap_err()));
        let plain = root.join(".pixel/state.json");
        fs::write(&plain, b"{}").unwrap();
        let mut body = String::new();
        open_read(&plain)
            .unwrap()
            .read_to_string(&mut body)
            .unwrap();
        assert_eq!(body, "{}");
    }

    #[test]
    fn open_rewrite_should_refuse_a_link_and_keep_mode_and_inode_of_a_file() {
        use std::os::unix::fs::MetadataExt;
        let (_base, root, sentinel) = layout("rewrite");
        let link = root.join("src.rs");
        symlink(&sentinel.0, &link).unwrap();
        assert!(is_eloop(&open_rewrite(&link).unwrap_err()));
        sentinel.assert_intact();

        let script = root.join("run.sh");
        fs::write(&script, b"old old old").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let inode = fs::metadata(&script).unwrap().ino();
        open_rewrite(&script).unwrap().write_all(b"new").unwrap();
        assert_eq!(fs::read(&script).unwrap(), b"new");
        assert_eq!(mode_of(&script), 0o755);
        assert_eq!(fs::metadata(&script).unwrap().ino(), inode);
        assert_eq!(
            open_rewrite(&root.join("absent.rs")).unwrap_err().kind(),
            io::ErrorKind::NotFound,
            "a rewrite never creates the file"
        );
    }

    #[test]
    fn create_new_should_fail_on_a_planted_link() {
        let (_base, root, sentinel) = layout("create-new");
        let tmp = root.join("a.rs.gpx-rescue-tmp");
        symlink(&sentinel.0, &tmp).unwrap();
        assert_eq!(
            create_new(&tmp, 0o644).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        sentinel.assert_intact();
        let fresh = root.join("fresh.txt");
        drop(create_new(&fresh, 0o640).unwrap());
        assert_eq!(
            mode_of(&fresh) & 0o640,
            mode_of(&fresh),
            "created within the mode"
        );
    }

    #[test]
    fn write_replace_should_replace_a_link_instead_of_writing_through_it() {
        let (_base, root, sentinel) = layout("replace");
        let target = root.join(".pixel/targets.json");
        symlink(&sentinel.0, &target).unwrap();
        write_replace(&target, b"{\"tasks\":[]}", PRIVATE_MODE).unwrap();
        sentinel.assert_intact();
        let meta = fs::symlink_metadata(&target).unwrap();
        assert!(meta.file_type().is_file(), "the link was not replaced");
        assert_eq!(fs::read(&target).unwrap(), b"{\"tasks\":[]}");
        assert_eq!(mode_of(&target), PRIVATE_MODE);
        let names: Vec<String> = fs::read_dir(root.join(DIR_NAME))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["targets.json".to_string()],
            "no temporary file left"
        );
    }

    #[test]
    fn write_replace_should_refuse_a_path_without_a_file_name() {
        assert_eq!(
            write_replace(Path::new("/"), b"x", PRIVATE_MODE)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn refuse_linked_ancestors_should_refuse_a_linked_directory_below_pixel() {
        let (base, root, _sentinel) = layout("ancestors");
        let outside = base.join("outside");
        fs::create_dir_all(outside.join("t1")).unwrap();
        symlink(&outside, root.join(".pixel/tasks")).unwrap();
        let journal = root.join(".pixel/tasks/t1/journal.jsonl");
        let error = open_append(&journal).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error:?}");
        assert!(error.to_string().contains("tasks"), "{error}");
        assert!(fs::read_dir(outside.join("t1")).unwrap().next().is_none());
    }

    #[test]
    fn refuse_linked_ancestors_should_accept_real_directories_and_paths_outside_pixel() {
        let (base, root, _sentinel) = layout("ancestors-ok");
        fs::create_dir_all(root.join(".pixel/tasks/t1")).unwrap();
        refuse_linked_ancestors(&root.join(".pixel/tasks/t1/journal.jsonl")).unwrap();
        // Outside `.pixel`, a linked directory is the caller's to confine.
        symlink(&base, root.join("up")).unwrap();
        refuse_linked_ancestors(&root.join("up/repo/file")).unwrap();
    }
}
