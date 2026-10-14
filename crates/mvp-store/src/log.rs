// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The local invocation log (C-17): one JSON line per command or hook,
//! appended to `log.jsonl` in a state directory that already exists. It
//! never leaves the machine and never creates a directory of its own: a
//! repository Pixel never touched, or a home where Pixel keeps no state,
//! gets no log (ADR-0024).

use std::fs::{File, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mvp_model::log::Event;

/// The log inside a state directory.
pub const LOG: &str = mvp_model::log::LOG_FILES[0];
/// The log before the last rotation, kept once.
pub const LOG_PREVIOUS: &str = mvp_model::log::LOG_FILES[1];
/// Past this size the log is rotated before the next line (guessed: some
/// ten thousand prompts).
pub const LOG_CAP: u64 = 4 * 1024 * 1024;
/// How long a writer waits for another's lock on the log before it
/// writes its line without rotating (guessed: a line takes microseconds,
/// and the hook has its deadline, C-8).
const LOCK_WAIT: Duration = Duration::from_millis(50);

/// Where a command may log (ADR-0024).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The repository's state, else the nearest one at or above the
    /// working directory, else the home's.
    Anywhere,
    /// The state of the repository the command acted on, and nowhere else:
    /// `install` and `uninstall`, whose writes in `$HOME` C-3 and C-4
    /// count.
    RepositoryOnly,
}

impl Scope {
    /// The scope of `event`'s line.
    pub fn of(event: Event) -> Scope {
        match event {
            Event::Install | Event::Uninstall => Scope::RepositoryOnly,
            Event::Classify
            | Event::Config
            | Event::HookPrompt
            | Event::HookSessionStart
            | Event::Index
            | Event::Minimap
            | Event::Plugin
            | Event::PluginCommand
            | Event::Status => Scope::Anywhere,
        }
    }
}

/// Appends `line` and its newline to the log of the first directory of
/// `dirs` that exists, in one write, and returns the file written; `None`
/// when none exists or the write failed. A failure to log never fails the
/// command that logs, nor holds it: a log that is a FIFO is refused
/// without waiting for a reader.
///
/// A repository is not the user's to trust: a state directory or a log
/// that is a link is never followed, so a cloned `.pixel-mvp/log.jsonl`
/// pointing at `~/.gitconfig` gets nothing appended to it.
///
/// A state directory another user owns takes no line either (C-18).
pub fn append(dirs: &[PathBuf], line: &str) -> Option<PathBuf> {
    let dir = dirs.iter().find(|dir| real_dir(dir))?;
    if !crate::mine(dir).unwrap_or(false) {
        return None;
    }
    let path = dir.join(LOG);
    let mut bytes = Vec::with_capacity(line.len() + 1);
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    write(dir, &path, open(&path)?, &bytes, File::try_lock)?;
    Some(path)
}

/// The directories a command's line may go to, in order (ADR-0024). With
/// [`Scope::RepositoryOnly`], the state directory of the repository it
/// acted on and nothing else, unless that is `home_state` (a home that is
/// a repository): then none. With [`Scope::Anywhere`], that one, then the
/// nearest `.pixel-mvp/` at or above `cwd` (found without running git: a
/// repository with no state of its own inside one that has some, or a
/// path that never learnt its repository), then `home_state`.
pub fn dirs(
    scope: Scope,
    root: Option<&Path>,
    cwd: Option<&Path>,
    home_state: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = root.map(crate::dir).into_iter().collect();
    if scope == Scope::RepositoryOnly {
        // A home that is a repository: its state is the home's.
        dirs.retain(|dir| {
            home_state
                .as_deref()
                .is_none_or(|home| !same_dir(dir, home))
        });
        return dirs;
    }
    if let Some(found) =
        cwd.and_then(|cwd| cwd.ancestors().map(crate::dir).find(|dir| real_dir(dir)))
    {
        dirs.push(found);
    }
    dirs.extend(home_state);
    dirs
}

/// Whether `a` and `b` name the same directory, which may not exist yet:
/// the same name in the same parent, the parents compared by device and
/// inode on Unix (a bind mount gives a directory two paths), else by their
/// paths with links resolved.
fn same_dir(a: &Path, b: &Path) -> bool {
    let (Some(a_parent), Some(b_parent)) = (a.parent(), b.parent()) else {
        return a == b;
    };
    if a.file_name() != b.file_name() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(a), Ok(b)) = (std::fs::metadata(a_parent), std::fs::metadata(b_parent)) {
            return a.dev() == b.dev() && a.ino() == b.ino();
        }
    }
    match (a_parent.canonicalize(), b_parent.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a_parent == b_parent,
    }
}

/// Whether `dir` is a directory and not a link to one.
fn real_dir(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir())
}

/// The log at `path`, opened to append, made when missing: `None` when it
/// is a link, a FIFO (the open does not wait for its reader), a device or
/// a directory.
fn open(path: &Path) -> Option<File> {
    crate::state_file::open_append(path).ok()
}

/// Writes `bytes` through `file`, the log at `path` as it was opened. A
/// log past [`LOG_CAP`] is moved to [`LOG_PREVIOUS`], replacing the one
/// there, and the line goes to a new log. The rename is done under the
/// log's lock (`try_lock`, the OS's), and only while `path` is still the
/// file locked, so a writer that opened the log before another rotated it
/// writes into the file it holds, now the previous one, and no line of it
/// is lost. The lock is taken only for a log at its cap. On a file system
/// with no lock the log is rotated unlocked: two writers rotating at the
/// same instant can drop the previous file (ADR-0024 §3).
fn write(
    dir: &Path,
    path: &Path,
    mut file: File,
    bytes: &[u8],
    try_lock: impl Fn(&File) -> Result<(), TryLockError>,
) -> Option<()> {
    if full(&file)
        && may_rotate(&file, try_lock)
        && same_file(&file, path)
        && std::fs::rename(path, dir.join(LOG_PREVIOUS)).is_ok()
        && let Some(fresh) = open(path)
    {
        file = fresh;
    }
    file.write_all(bytes).ok()
}

/// Whether this writer may rotate `file`: it holds the exclusive lock,
/// waiting at most [`LOCK_WAIT`] for another writer to let it go; or the
/// file system has no lock to give (`ENOLCK` on NFS without its lock
/// daemon, `ENOSYS` on some FUSE ones), and the log is rotated unlocked,
/// so its cap holds there too. Unlocked, nothing guards the moment
/// between [`same_file`] and the rename: a second writer rotating then
/// moves the first one's new log onto [`LOG_PREVIOUS`], and the lines
/// that were there are gone. The window is a few system calls wide; a
/// lock file of its own would leave a file behind when a writer dies in
/// it, which the cap is not worth.
fn may_rotate(file: &File, try_lock: impl Fn(&File) -> Result<(), TryLockError>) -> bool {
    let started = Instant::now();
    loop {
        match try_lock(file) {
            Ok(()) => return true,
            Err(TryLockError::WouldBlock) if started.elapsed() < LOCK_WAIT => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(TryLockError::WouldBlock) => return false,
            Err(TryLockError::Error(_)) => return true,
        }
    }
}

/// Whether the log `file` is at its cap.
fn full(file: &File) -> bool {
    file.metadata().is_ok_and(|meta| meta.len() >= LOG_CAP)
}

/// Whether `path` is still the file `file` was opened from, not a link and
/// not a log another writer made since it rotated this one.
fn same_file(file: &File, path: &Path) -> bool {
    let (Ok(held), Ok(now)) = (file.metadata(), std::fs::symlink_metadata(path)) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        now.is_file() && held.dev() == now.dev() && held.ino() == now.ino()
    }
    #[cfg(not(unix))]
    {
        now.is_file() && held.len() == now.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c17_the_log_goes_to_the_first_directory_that_exists_and_makes_none() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("repo/.pixel-mvp");
        let home = tmp.path().join("home/.pixel-mvp");
        assert_eq!(append(&[missing.clone(), home.clone()], "{}"), None);
        assert!(!missing.exists() && !home.exists());
        std::fs::create_dir_all(&home).unwrap();
        let written = append(&[missing.clone(), home.clone()], "{\"a\":1}").unwrap();
        assert_eq!(written, home.join(LOG));
        assert!(!missing.exists());
        std::fs::create_dir_all(&missing).unwrap();
        append(&[missing.clone(), home.clone()], "{\"b\":2}").unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join(LOG)).unwrap(),
            "{\"a\":1}\n"
        );
        assert_eq!(
            std::fs::read_to_string(missing.join(LOG)).unwrap(),
            "{\"b\":2}\n"
        );
    }

    /// The directory the line goes to: the first of `dirs` that exists.
    fn place(dirs: &[PathBuf]) -> Option<PathBuf> {
        dirs.iter().find(|dir| real_dir(dir)).cloned()
    }

    #[test]
    fn c17_a_command_logs_in_its_repository_else_the_nearest_state_else_the_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home/.pixel-mvp");
        std::fs::create_dir_all(&home).unwrap();
        let outer = tmp.path().join("outer");
        std::fs::create_dir_all(outer.join(".pixel-mvp")).unwrap();
        // A repository with no state of its own inside one that has some.
        let inner = outer.join("vendor/inner");
        std::fs::create_dir_all(inner.join("src")).unwrap();
        let cwd = inner.join("src");
        let anywhere = |root: Option<&Path>, cwd: &Path| {
            place(&dirs(Scope::Anywhere, root, Some(cwd), Some(home.clone())))
        };
        assert_eq!(anywhere(Some(&inner), &cwd), Some(outer.join(".pixel-mvp")));
        // A path that never learnt its repository.
        assert_eq!(anywhere(None, &cwd), Some(outer.join(".pixel-mvp")));
        // Its own state, once it has one, comes first.
        std::fs::create_dir(inner.join(".pixel-mvp")).unwrap();
        assert_eq!(anywhere(Some(&inner), &cwd), Some(inner.join(".pixel-mvp")));
        // The repository a command named comes first, whatever its cwd.
        assert_eq!(
            anywhere(Some(&inner), tmp.path()),
            Some(inner.join(".pixel-mvp"))
        );
        // No state above: the home's.
        let lone = tmp.path().join("lone");
        std::fs::create_dir(&lone).unwrap();
        assert_eq!(anywhere(Some(&lone), &lone), Some(home.clone()));
        assert_eq!(anywhere(None, &lone), Some(home.clone()));
        assert_eq!(place(&dirs(Scope::Anywhere, None, Some(&lone), None)), None);
    }

    #[test]
    fn c3_install_and_uninstall_log_only_in_the_repository_they_act_on() {
        assert_eq!(Scope::of(Event::Install), Scope::RepositoryOnly);
        assert_eq!(Scope::of(Event::Uninstall), Scope::RepositoryOnly);
        for event in [
            Event::Classify,
            Event::HookPrompt,
            Event::Index,
            Event::Minimap,
            Event::Status,
        ] {
            assert_eq!(Scope::of(event), Scope::Anywhere);
        }
        let tmp = tempfile::tempdir().unwrap();
        // A home that has state, and a working directory under it in no
        // repository: the nearest state above is the home's.
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".pixel-mvp")).unwrap();
        let sub = home.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let state = Some(home.join(".pixel-mvp"));
        assert_eq!(
            dirs(Scope::RepositoryOnly, None, Some(&sub), state.clone()),
            Vec::<PathBuf>::new()
        );
        assert_eq!(
            dirs(Scope::RepositoryOnly, None, Some(&home), state.clone()),
            Vec::<PathBuf>::new()
        );
        // In a repository with no state, inside the home: nowhere either.
        let repo = home.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let only = dirs(
            Scope::RepositoryOnly,
            Some(&repo),
            Some(&repo),
            state.clone(),
        );
        assert_eq!(only, [repo.join(".pixel-mvp")]);
        assert_eq!(place(&only), None);
        std::fs::create_dir(repo.join(".pixel-mvp")).unwrap();
        assert_eq!(place(&only), Some(repo.join(".pixel-mvp")));
        // A home that is itself a repository: its state is the home's, and
        // takes no line, whatever link the path goes through.
        let mut roots = vec![home.clone()];
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&home, tmp.path().join("link")).unwrap();
            roots.push(tmp.path().join("link"));
        }
        for root in roots {
            for cwd in [&root, &sub] {
                assert_eq!(
                    dirs(Scope::RepositoryOnly, Some(&root), Some(cwd), state.clone()),
                    Vec::<PathBuf>::new(),
                    "{}",
                    root.display()
                );
            }
            // Other commands log there.
            assert_eq!(
                place(&dirs(
                    Scope::Anywhere,
                    Some(&root),
                    Some(&root),
                    state.clone()
                )),
                Some(root.join(".pixel-mvp"))
            );
        }
    }

    #[test]
    fn c17_a_writer_that_opened_the_log_before_another_rotated_it_loses_no_line() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let path = dir.join(LOG);
        let old = "{\"old\":1}\n".repeat(LOG_CAP as usize / 10 + 1);
        std::fs::write(&path, &old).unwrap();
        // A writer opens the full log, then waits; another rotates it and
        // writes its line in the new log.
        let stale = open(&path).unwrap();
        append(std::slice::from_ref(&dir), "{\"new\":1}").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(LOG_PREVIOUS)).unwrap(),
            old
        );
        // The first writes now: into the file it holds, the previous log.
        // Rotating again would have replaced the old lines with the new.
        write(&dir, &path, stale, b"{\"late\":1}\n", File::try_lock).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(LOG_PREVIOUS)).unwrap(),
            format!("{old}{{\"late\":1}}\n")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"new\":1}\n");
    }

    #[test]
    fn c17_a_file_system_without_locks_still_rotates_the_log() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let path = dir.join(LOG);
        std::fs::write(&path, vec![b'x'; LOG_CAP as usize]).unwrap();
        // `ENOLCK`, `ENOSYS`: the file system gives no lock.
        let no_lock = |_: &File| {
            Err(TryLockError::Error(std::io::Error::from(
                std::io::ErrorKind::Unsupported,
            )))
        };
        write(&dir, &path, open(&path).unwrap(), b"next\n", no_lock).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "next\n");
        assert_eq!(
            std::fs::metadata(dir.join(LOG_PREVIOUS)).unwrap().len(),
            LOG_CAP
        );
    }

    #[test]
    fn c17_the_lock_is_taken_only_for_a_log_at_its_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let path = dir.join(LOG);
        let tries = std::cell::Cell::new(0);
        let counted = |file: &File| {
            tries.set(tries.get() + 1);
            file.try_lock()
        };
        write(&dir, &path, open(&path).unwrap(), b"a\n", counted).unwrap();
        assert_eq!(tries.get(), 0);
        std::fs::write(&path, vec![b'x'; LOG_CAP as usize]).unwrap();
        write(&dir, &path, open(&path).unwrap(), b"b\n", counted).unwrap();
        assert_eq!(tries.get(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "b\n");
    }

    #[test]
    fn c17_a_log_locked_by_another_writer_still_gets_the_line() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let path = dir.join(LOG);
        std::fs::write(&path, vec![b'x'; LOG_CAP as usize]).unwrap();
        let holder = File::open(&path).unwrap();
        holder.lock().unwrap();
        let started = Instant::now();
        append(std::slice::from_ref(&dir), "next").unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        // Not rotated without the lock, and the line is there.
        assert!(!dir.join(LOG_PREVIOUS).exists());
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("xnext\n"));
    }

    #[cfg(unix)]
    #[test]
    fn c17_a_log_that_is_a_link_is_never_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, "[user]\n\tname = v\n").unwrap();
        // The log a cloned repository holds is a link.
        let state = tmp.path().join("repo/.pixel-mvp");
        std::fs::create_dir_all(&state).unwrap();
        std::os::unix::fs::symlink(&victim, state.join(LOG)).unwrap();
        assert_eq!(append(std::slice::from_ref(&state), "{\"x\":1}"), None);
        // The state directory itself is a link.
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let linked = tmp.path().join("other/.pixel-mvp");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &linked).unwrap();
        assert_eq!(append(std::slice::from_ref(&linked), "{\"x\":1}"), None);
        // A full log that is a link is not renamed through either.
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "[user]\n\tname = v\n"
        );
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn c17_a_log_that_is_a_fifo_is_refused_without_waiting() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        // A FIFO with no reader: opening it to write would wait for one.
        let made = std::process::Command::new("mkfifo")
            .arg(dir.join(LOG))
            .status()
            .unwrap();
        assert!(made.success());
        let (send, receive) = std::sync::mpsc::channel();
        let dirs = vec![dir.clone()];
        std::thread::spawn(move || {
            let _ = send.send(append(&dirs, "{\"x\":1}"));
        });
        let appended = receive.recv_timeout(std::time::Duration::from_secs(1));
        if appended.is_err() {
            // Frees the thread still waiting: a reader opens the FIFO.
            let _ = std::fs::File::open(dir.join(LOG));
        }
        assert_eq!(appended, Ok(None), "waited on the FIFO, or wrote to it");
        // With a reader at the other end the open succeeds: the file is no
        // regular file, and the line is not written to it.
        let mut reader = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
                .open(dir.join(LOG))
                .unwrap()
        };
        assert_eq!(append(std::slice::from_ref(&dir), "{\"x\":2}"), None);
        let mut read = Vec::new();
        let _ = std::io::Read::read_to_end(&mut reader, &mut read);
        assert_eq!(read, b"");
    }

    #[test]
    fn c17_a_full_log_is_rotated_once() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        std::fs::write(dir.join(LOG), vec![b'x'; LOG_CAP as usize]).unwrap();
        append(std::slice::from_ref(&dir), "new").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(LOG)).unwrap(), "new\n");
        assert_eq!(
            std::fs::metadata(dir.join(LOG_PREVIOUS)).unwrap().len(),
            LOG_CAP
        );
        append(std::slice::from_ref(&dir), "next").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(LOG)).unwrap(),
            "new\nnext\n"
        );
    }
}
