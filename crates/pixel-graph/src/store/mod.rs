// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The graph store: `.pixel/graph.db`, one SQLite file, schema
//! versioned (`design/architecture.md`, "Graph store").
//!
//! - One writer at a time, through `.pixel/index.lock` ([`Writer`]).
//!   A second writer waits, or gives up when its wait runs out.
//! - Each run publishes one generation in one transaction, so a reader
//!   ([`Store`]) sees one generation whole; a crash leaves the last one.
//! - A corrupt or incompatible file is never read: the reader refuses it
//!   and the next writer rebuilds it (C-19).
//! - Unresolved edges are kept and counted, never served as callers
//!   (C-10).

mod checked;
mod read;
mod rows;
mod schema;
mod state_file;
mod write;

use std::fmt;
use std::path::{Path, PathBuf};

pub use read::{Caller, Store};
pub use schema::SCHEMA_VERSION;
pub use write::{Indexed, Writer};

/// The directory of a repository's state (C-18).
pub const DIR: &str = ".pixel";
/// The graph inside it.
pub const DB: &str = "graph.db";
/// The writer's lock file inside it.
pub const LOCK: &str = "index.lock";

/// Keeps everything in `.pixel/` but `plugins/` out of git
/// (`design/architecture.md`, "On-disk state").
const GITIGNORE: &str = "# pixel state: rebuildable from the tree, never committed.\n\
# plugins/ is source a team commits (design/plugins.md).\n\
*\n\
!plugins/\n\
!plugins/**\n";

pub fn dir(root: &Path) -> PathBuf {
    root.join(DIR)
}

pub fn db_path(root: &Path) -> PathBuf {
    dir(root).join(DB)
}

#[derive(Debug)]
pub enum StoreError {
    /// No generation has been published in this repository yet.
    NoIndex,
    /// The index exists but cannot be read; the next `index` rebuilds it.
    Unusable(String),
    /// A delta made against other rows than the stored generation's; it
    /// was not published.
    Refused(String),
    /// Another writer holds the lock past this one's wait.
    Busy,
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NoIndex => f.write_str("no index yet: run `pixel prepare`"),
            StoreError::Unusable(reason) => write!(
                f,
                "the index cannot be read ({reason}); `pixel prepare` rebuilds it"
            ),
            StoreError::Refused(reason) => write!(
                f,
                "the next generation was refused ({reason}); nothing was published"
            ),
            StoreError::Busy => f.write_str("another pixel process is writing the index"),
            StoreError::Io(err) => write!(f, "{err}"),
            StoreError::Sqlite(err) => write!(f, "{}", schema::describe(err)),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(err: std::io::Error) -> Self {
        StoreError::Io(err)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(err: rusqlite::Error) -> Self {
        if schema::is_corruption(&err) {
            StoreError::Unusable(schema::describe(&err))
        } else {
            StoreError::Sqlite(err)
        }
    }
}

/// The files SQLite keeps beside `graph.db`, by suffix.
const JOURNALS: [&str; 3] = ["-wal", "-shm", "-journal"];

/// The file beside `graph.db` with `suffix`, one of [`JOURNALS`].
fn beside(db: &Path, suffix: &str) -> PathBuf {
    db.with_file_name(format!("{DB}{suffix}"))
}

/// Whether `graph.db` exists. A directory, a link or anything else that
/// is not a regular file there or at one of its journal files is
/// `Unusable`: SQLite fails on it or, for `-shm`, falls back to a
/// read-only mapping no write gets past. The writer replaces it (C-19).
fn existing(db: &Path) -> Result<bool, StoreError> {
    for suffix in JOURNALS {
        if let Found::Other(what) = found(&beside(db, suffix))? {
            return Err(StoreError::Unusable(format!("{DB}{suffix} is {what}")));
        }
    }
    match found(db)? {
        Found::Nothing => Ok(false),
        Found::File => Ok(true),
        Found::Other(what) => Err(StoreError::Unusable(format!("{DB} is {what}"))),
    }
}

/// What is at a path.
enum Found {
    Nothing,
    File,
    /// A directory, a link or a special file, said in words.
    Other(&'static str),
}

fn found(path: &Path) -> std::io::Result<Found> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Found::File),
        Ok(meta) if meta.is_dir() => Ok(Found::Other("a directory")),
        Ok(_) => Ok(Found::Other("not a regular file")),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Found::Nothing),
        Err(err) => Err(err),
    }
}

/// Places a copy of git's index at `name` in the state directory (ADR-0039,
/// Q42): `bytes`, git's index as read, go to a temporary of this process
/// made where nothing is, `refresh` has git refresh its stat cache, and a
/// rename puts it at `name`, but only while `holds` says git's index is
/// still the version read. Every other name `old` accepts goes: copies of
/// other versions, a crashed run's temporaries. The writer calls this with
/// its lock held, so no other process places one meanwhile. Whether the
/// copy was placed; a state directory that is missing or not this user's
/// is left as it is.
pub fn place_git_index(
    root: &Path,
    name: &str,
    bytes: &[u8],
    refresh: impl FnOnce(&Path) -> bool,
    holds: impl Fn() -> bool,
    old: impl Fn(&str) -> bool,
) -> std::io::Result<bool> {
    if !matches!(own(root), Ok(true)) {
        return Ok(false);
    }
    let base = dir(root);
    for entry in std::fs::read_dir(&base)? {
        let entry = entry?;
        let other = entry.file_name().to_string_lossy().into_owned();
        if other != name && old(&other) {
            // By its name: a link a clone left there goes, never what it
            // names (C-18).
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let temp = base.join(format!("{name}.{}.tmp", std::process::id()));
    let placed = state_file::create_fresh(&temp)
        .and_then(|mut file| std::io::Write::write_all(&mut file, bytes))
        .map(|()| refresh(&temp) && holds());
    let placed = match placed {
        Ok(true) => std::fs::rename(&temp, base.join(name)).map(|()| true),
        other => other,
    };
    if !matches!(placed, Ok(true)) {
        let _ = std::fs::remove_file(&temp);
        let _ = std::fs::remove_file(base.join(format!("{name}.{}.tmp.lock", std::process::id())));
    }
    placed
}

/// Whether `.pixel/` of `root` exists, when it is a directory, not a
/// link: a clone that commits it as a link (to another repository's
/// state, or anywhere) or as anything else is refused, and no state is
/// read or written through it. The writer also refuses one another user
/// owns ([`ensure_dir`]).
fn own(root: &Path) -> Result<bool, StoreError> {
    match std::fs::symlink_metadata(dir(root)) {
        Ok(meta) if meta.is_symlink() => Err(not_ours(root, "a link")),
        Ok(meta) if !meta.is_dir() => Err(not_ours(root, "not a directory")),
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn not_ours(root: &Path, what: &str) -> StoreError {
    StoreError::Io(std::io::Error::other(format!(
        "{} is {what}, not a directory of yours: PIXEL keeps no state there; remove it, then run `pixel prepare`",
        dir(root).display()
    )))
}

/// Whether this user owns the state directory `base`: its owner is that
/// of a file made in it now (this crate asks the system for no user id).
#[cfg(unix)]
fn mine(base: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let probe = base.join(format!(".owner.{}.probe", std::process::id()));
    let made = state_file::create_fresh(&probe).and_then(|file| file.metadata());
    let _ = std::fs::remove_file(&probe);
    Ok(made?.uid() == std::fs::symlink_metadata(base)?.uid())
}

#[cfg(not(unix))]
fn mine(_: &Path) -> std::io::Result<bool> {
    Ok(true)
}

/// Creates `.pixel/` and its `.gitignore` when missing; refuses a
/// state directory that is a link, not a directory ([`own`]) or another
/// user's. The `.gitignore`
/// is created only where nothing is, a link included: it is never
/// written through one.
fn ensure_dir(root: &Path) -> Result<(), StoreError> {
    let base = dir(root);
    if !own(root)? {
        match std::fs::create_dir(&base) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(unwritable(root, err)),
        }
        own(root)?;
    }
    if !mine(&base).map_err(|err| unwritable(root, err))? {
        return Err(not_ours(root, "another user's directory"));
    }
    let ignore = base.join(".gitignore");
    if matches!(found(&ignore)?, Found::Nothing) {
        match state_file::create_new(&ignore) {
            Ok(mut file) => std::io::Write::write_all(&mut file, GITIGNORE.as_bytes())?,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

/// A state directory the writer cannot write in, a read-only checkout's,
/// said as the other refusals are, with what to do; any other error as it
/// is.
fn unwritable(root: &Path, err: std::io::Error) -> StoreError {
    use std::io::ErrorKind;
    if !matches!(
        err.kind(),
        ErrorKind::ReadOnlyFilesystem | ErrorKind::PermissionDenied
    ) {
        return err.into();
    }
    StoreError::Io(std::io::Error::new(
        err.kind(),
        format!(
            "{} cannot be written ({err}): PIXEL keeps the index there; make the checkout writable, then run `pixel prepare`",
            dir(root).display()
        ),
    ))
}

#[cfg(test)]
mod tests;
