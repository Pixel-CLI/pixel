// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The files of a state directory, opened the one way a cloned repository
//! cannot turn against the user (C-18). A clone can commit any name inside
//! `.pixel-mvp/` as a link to a file of the user's, and an archive can
//! carry a FIFO there; so every file Pixel keeps in it is
//!
//! - opened without following a link (`O_NOFOLLOW`), and kept only when
//!   what was opened is a regular file;
//! - opened without waiting (`O_NONBLOCK`): a FIFO with no writer, or a
//!   device, never holds the command;
//! - made, when it is a temporary of this process, only where nothing is
//!   (`O_EXCL`), after removing what the clone left at its name: a link
//!   there is removed, never written through;
//! - renamed onto and removed by its name, which acts on a link itself,
//!   never on what it names.
//!
//! SQLite opens `graph.db` and its journal files itself, each with
//! `O_NOFOLLOW` (its Unix VFS adds it to every open); `existing` refuses
//! one that is not a regular file before any open, a FIFO included.
//! `SQLITE_OPEN_NOFOLLOW` is not used: it refuses a link anywhere in the
//! path, a repository reached through a linked directory included.
//!
//! The flags come from rustix, which takes each from the target's kernel
//! headers: `O_NOFOLLOW` is 0x20000 on x86_64 Linux and 0x8000 on aarch64
//! Linux, where 0x20000 is `O_LARGEFILE`, so no value written by hand is
//! right on every target Pixel ships.
//!
//! `pixel-adapter` and `pixel-plugin` keep the user's own state
//! (`~/.pixel-mvp/`) by the same rules, each with its own copy of these
//! few calls: neither depends on this crate.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

/// `path` opened to read: never through a link, never waiting, only a
/// regular file.
pub fn open_read(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    open(path, &mut options)
}

/// The text of the regular file at `path`, read as [`open_read`] opens it.
pub fn read_to_string(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    open_read(path)?.read_to_string(&mut text)?;
    Ok(text)
}

/// `path` opened to append, made when missing.
pub fn open_append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    open(path, &mut options)
}

/// A lock file at `path`, opened to write without truncating it; made
/// when missing if `create`.
pub fn open_lock(path: &Path, create: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(create).truncate(false);
    open(path, &mut options)
}

/// A new file of this process at `path`: what is at the name is removed
/// first (a link the clone left there, itself), then the file is made
/// where nothing is, so nothing it names is ever written.
pub fn create_fresh(path: &Path) -> io::Result<File> {
    let _ = std::fs::remove_file(path);
    create_new(path)
}

/// A new file at `path`, made only where nothing is.
pub fn create_new(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    open(path, &mut options)
}

/// Writes `bytes` to `temp`, a new file of this process ([`create_fresh`]),
/// and renames it over `path`, so a reader never reads half of it and a
/// link at `path` is replaced, never written through. The temporary file
/// goes when anything fails.
pub fn replace(path: &Path, temp: &Path, bytes: &[u8]) -> io::Result<()> {
    let written = create_fresh(temp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| std::fs::rename(temp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    written
}

/// Opens `path` with `options`, `O_NOFOLLOW` and `O_NONBLOCK` added, and
/// keeps the file only when it is a regular one.
fn open(path: &Path, options: &mut OpenOptions) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
        options.custom_flags(flags.bits() as i32);
    }
    let file = options.open(path).map_err(|err| refused(path, err))?;
    if !file.metadata()?.is_file() {
        return Err(not_regular(path));
    }
    Ok(file)
}

/// The error of an open, said in words for a link (`O_NOFOLLOW` gives
/// `ELOOP`, "too many levels of symbolic links") and for a FIFO with no
/// reader (`ENXIO`, "no such device or address").
fn refused(path: &Path, err: io::Error) -> io::Error {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_symlink() => io::Error::other(format!(
            "{} is a link: Pixel never opens its state through one",
            path.display()
        )),
        Ok(meta) if !meta.is_file() && !meta.is_dir() => not_regular(path),
        _ => err,
    }
}

fn not_regular(path: &Path) -> io::Error {
    io::Error::other(format!(
        "{} is not a regular file: Pixel keeps no state in it",
        path.display()
    ))
}

/// Removes what is at `path` when it is a link, a FIFO, a socket or a
/// device: nothing Pixel made, so nothing a process of Pixel's holds. A
/// regular file or a directory is left.
pub fn clear_special(path: &Path) {
    if std::fs::symlink_metadata(path).is_ok_and(|meta| !meta.is_file() && !meta.is_dir()) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A FIFO at `path`, with no writer and no reader.
    fn fifo(path: &Path) {
        let made = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(made.success());
    }

    /// `f` on another thread, which must end within a second.
    fn within<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = send.send(f());
        });
        receive
            .recv_timeout(Duration::from_secs(5))
            .expect("held by the FIFO")
    }

    #[test]
    fn c18_r4_no_open_follows_a_link_and_every_one_refuses_what_is_not_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, "SECRET\n").unwrap();
        let absent = tmp.path().join("absent");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        for (name, target) in [("to-victim", &victim), ("dangling", &absent)] {
            let link = state.join(name);
            std::os::unix::fs::symlink(target, &link).unwrap();
            let err = read_to_string(&link).unwrap_err().to_string();
            assert!(err.contains("is a link"), "{err}");
            assert!(open_append(&link).is_err());
            assert!(open_lock(&link, true).is_err());
            assert!(open_lock(&link, false).is_err());
            assert!(create_new(&link).is_err());
            // A temporary of this process removes the link, not its target.
            let mut file = create_fresh(&link).unwrap();
            file.write_all(b"mine").unwrap();
            assert!(std::fs::symlink_metadata(&link).unwrap().is_file());
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(target, &link).unwrap();
            replace(&link, &state.join(format!("{name}.tmp")), b"new").unwrap();
            assert_eq!(std::fs::read(&link).unwrap(), b"new");
        }
        assert_eq!(std::fs::read(&victim).unwrap(), b"SECRET\n");
        assert!(!absent.exists());
        // A directory is no file either.
        std::fs::create_dir(state.join("dir")).unwrap();
        assert!(read_to_string(&state.join("dir")).is_err());
        assert!(open_lock(&state.join("dir"), true).is_err());
    }

    #[test]
    fn c18_r4_a_fifo_never_holds_an_open() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fifo");
        fifo(&path);
        let at = path.clone();
        assert!(within(move || read_to_string(&at).is_err()));
        let at = path.clone();
        assert!(within(move || open_append(&at).is_err()));
        let at = path.clone();
        assert!(within(move || open_lock(&at, true).is_err()));
        let at = path.clone();
        assert!(within(move || open_lock(&at, false).is_err()));
        // A temporary at a FIFO's name replaces the FIFO.
        let at = path.clone();
        assert!(within(
            move || replace(&tmp.path().join("out"), &at, b"x").is_ok()
        ));
    }
}
