// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The open check of C-19, once per version of the file (ADR-0022).
//!
//! The full check (`schema::shape`) reads every page and every row: about
//! 150 ms on the 36.9 MB index of `PIXEL-CLI/pixel`, more than half of a
//! prompt hook's budget. Once a check passes, `.pixel/checked` holds a
//! stamp of the files it read: device, inode, size, mtime and ctime of
//! `graph.db` and of its WAL and rollback journal, with the versions of the
//! binary, of the check and of the schema. A reader whose files carry the
//! same stamp skips the full check. Any write to a file changes its ctime,
//! which no user tool sets back, so a file changed through the file system
//! after the check is checked again.
//!
//! Two rules keep a stamp true:
//!
//! - It is recorded only when every time in it is older than the file
//!   system's clock, read from a file written just before the stat (git's
//!   racy-clean rule). Where times are coarse (a second on ext4 with
//!   128-byte inodes, HFS+ or FAT, a jiffy on a kernel without multigrain
//!   timestamps), a write in the tick of the stat leaves all five values as
//!   they were; a write in a later tick cannot.
//! - It is recorded only for files nobody else wrote since the check read
//!   them: the stat taken before the check must still hold after it. A
//!   reader whose full check passed records the stat it took before. The
//!   writer records after its own open check, and after each of its
//!   commits when `graph.db` is as it left it (a commit writes the WAL
//!   only), once its checkpoint has folded the WAL in. A write by another
//!   process during that checkpoint is the one window left.
//!
//! A stamp also names when the full check behind it ran: a writer's
//! stamp after its own commits carries the time of the check it trusted.
//! The writer trusts a stamp only while that check is less than
//! [`TRUSTED_FOR`] old, and forgets it whenever a run fails
//! (`Writer::retried`), so damage a stamp cannot show (a bad sector, a
//! file system whose ctime does not move) is caught by the first index run
//! after a day, or after the run it broke. Damage a run reads (every row of
//! `files`, `symbols` and `edges`) is caught by that run
//! (`Writer::retried`); damage in an index b-tree it does not read is not.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::store::schema::{CHECK_VERSION, SCHEMA_VERSION};
use crate::store::{DB, JOURNALS, beside, db_path, dir};

/// The stamp file inside `.pixel/`.
pub const CHECKED: &str = "checked";

/// How long the writer trusts a stamp after the full check behind it: at
/// least one index run a day checks the file whole. A writer re-stamps the
/// files after each of its commits, so without a bound a watched
/// repository would never be checked whole again.
pub const TRUSTED_FOR: Duration = Duration::from_secs(86_400);

/// A time as the file system keeps it: seconds and nanoseconds.
type Time = (i64, i64);

/// What a stamp holds of one file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stat {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: Time,
    ctime: Time,
}

/// The stat of `graph.db` and of its journal files. A journal file that
/// is absent or empty is `None`: SQLite deletes an empty WAL on close and
/// a read-only reader creates one, neither of which changes what the index
/// holds. `-shm` is left out: it holds no data, SQLite rebuilds it from
/// the WAL, and every reader writes its read marks there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stamp {
    db: Stat,
    journals: Vec<Option<Stat>>,
}

impl Stamp {
    /// The stamp of the files now; `None` when one cannot be stat'd, or on
    /// a platform with no inode and ctime.
    pub fn of(root: &Path) -> Option<Stamp> {
        let db = db_path(root);
        let journals = stamped_journals()
            .map(|suffix| journal(&beside(&db, suffix)))
            .collect::<Option<_>>()?;
        Some(Stamp {
            db: stat(&db).ok()?,
            journals,
        })
    }

    /// The stamp of the files now, when every time in it is older than the
    /// file system's clock read just before: any later write changes it.
    pub fn settled(root: &Path) -> Option<Stamp> {
        let now = now(root)?;
        let stamp = Stamp::of(root)?;
        let older = std::iter::once(&stamp.db)
            .chain(stamp.journals.iter().flatten())
            .all(|stat| stat.mtime < now && stat.ctime < now);
        older.then_some(stamp)
    }

    /// Whether `graph.db` is as it was in `other`, whatever its journals.
    pub fn same_db(&self, other: &Stamp) -> bool {
        self.db == other.db
    }

    /// One line for the versions, then one per file; [`record`] adds the
    /// time of the full check.
    fn text(&self) -> String {
        let mut text = format!(
            "pixel {} check {CHECK_VERSION} schema {SCHEMA_VERSION}\n",
            env!("CARGO_PKG_VERSION")
        );
        text.push_str(&format!("{DB} {}\n", line(Some(&self.db))));
        for (suffix, stat) in stamped_journals().zip(&self.journals) {
            text.push_str(&format!("{DB}{suffix} {}\n", line(stat.as_ref())));
        }
        text
    }
}

fn stamped_journals() -> impl Iterator<Item = &'static str> {
    JOURNALS.into_iter().filter(|suffix| *suffix != "-shm")
}

fn line(stat: Option<&Stat>) -> String {
    match stat {
        None => "-".to_owned(),
        Some(s) => format!(
            "{} {} {} {}.{} {}.{}",
            s.dev, s.ino, s.len, s.mtime.0, s.mtime.1, s.ctime.0, s.ctime.1
        ),
    }
}

/// Whether the files are those a passed check last stamped.
pub fn trusted(root: &Path) -> bool {
    checked_at(root).is_some()
}

/// When the full check ran whose stamp the files carry, if they carry
/// one: the files are those a passed check, or the writer that trusted it,
/// last stamped.
pub fn checked_at(root: &Path) -> Option<SystemTime> {
    let now = Stamp::of(root)?;
    // Never through a link the clone left at its name, nor held by a FIFO
    // there (C-18).
    let recorded = crate::store::state_file::read_to_string(&path(root)).ok()?;
    let secs = recorded
        .strip_prefix(&now.text())?
        .strip_prefix("full ")?
        .strip_suffix('\n')?
        .parse::<u64>()
        .ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(secs))
}

/// Whether a full check that ran `at` is recent enough at `now` for the
/// writer to trust its stamp: less than [`TRUSTED_FOR`] ago, and not after
/// `now` (a clock set back since then proves nothing about the age).
pub fn recent(at: SystemTime, now: SystemTime) -> bool {
    now.duration_since(at).is_ok_and(|age| age < TRUSTED_FOR)
}

/// Records `stamp` as checked by a full check that ran `at`, through a
/// temporary file of this process and a rename, so a reader never reads
/// half a stamp. The temporary is made where nothing is, so a link a clone
/// committed at its name is removed, never written through, and the
/// rename replaces a link at `checked` itself (C-18). Failing to record
/// costs only the next open a full check.
pub fn record(root: &Path, stamp: &Stamp, at: SystemTime) {
    let secs = at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let text = format!("{}full {secs}\n", stamp.text());
    let _ = crate::store::state_file::replace(&path(root), &unique(root, "tmp"), text.as_bytes());
}

/// Forgets the stamp: the files are no longer known to be checked.
pub fn forget(root: &Path) {
    let _ = std::fs::remove_file(path(root));
}

fn path(root: &Path) -> PathBuf {
    dir(root).join(CHECKED)
}

/// A file name in `.pixel/` that no other process or call uses.
fn unique(root: &Path, kind: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    dir(root).join(format!("{CHECKED}.{}.{n}.{kind}", std::process::id()))
}

/// The file system's clock: the ctime a file written in `.pixel/` gets
/// now. The probe is written twice, its times read between: with
/// multigrain timestamps (Linux 6.13 on), a file whose times were read
/// gets a fine-grained time at its next write.
///
/// The probe is a new file of this process, made where nothing is and
/// written through its own handle: a link a clone committed at its name is
/// removed, never written through (C-18).
fn now(root: &Path) -> Option<Time> {
    use std::io::Write;
    let probe = unique(root, "probe");
    let written = crate::store::state_file::create_fresh(&probe).and_then(|mut file| {
        held(&file)?;
        file.write_all(b"now")?;
        held(&file)
    });
    let _ = std::fs::remove_file(&probe);
    written.ok().map(|stat| stat.ctime)
}

/// A journal file's stat: `Some(None)` when it is absent or empty.
fn journal(path: &Path) -> Option<Option<Stat>> {
    match stat(path) {
        Ok(stat) if stat.len == 0 => Some(None),
        Ok(stat) => Some(Some(stat)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    }
}

#[cfg(unix)]
fn stat(path: &Path) -> std::io::Result<Stat> {
    of(&std::fs::symlink_metadata(path)?)
}

/// The stat of a file this process holds open.
fn held(file: &std::fs::File) -> std::io::Result<Stat> {
    of(&file.metadata()?)
}

#[cfg(unix)]
fn of(meta: &std::fs::Metadata) -> std::io::Result<Stat> {
    use std::os::unix::fs::MetadataExt;
    if !meta.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let time = |time: Time| if seam::frozen() { (0, 0) } else { time };
    Ok(Stat {
        dev: meta.dev(),
        ino: meta.ino(),
        len: meta.len(),
        mtime: time((meta.mtime(), meta.mtime_nsec())),
        ctime: time((meta.ctime(), meta.ctime_nsec())),
    })
}

#[cfg(not(unix))]
fn stat(_path: &Path) -> std::io::Result<Stat> {
    Err(std::io::Error::other("no inode and ctime here"))
}

#[cfg(not(unix))]
fn of(_meta: &std::fs::Metadata) -> std::io::Result<Stat> {
    Err(std::io::Error::other("no inode and ctime here"))
}

pub use seam::meanwhile;

/// What the tests reach into: a clock that never ticks, and a write run
/// where another process could write the files between a check and its
/// stamp.
#[cfg(not(test))]
mod seam {
    pub fn frozen() -> bool {
        false
    }

    /// Where another process could write the files between a check and
    /// its stamp; tests write there.
    pub fn meanwhile() {}
}

#[cfg(test)]
pub mod seam {
    use std::cell::{Cell, RefCell};

    thread_local! {
        static FROZEN: Cell<bool> = const { Cell::new(false) };
        static MEANWHILE: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }

    /// From now on, on this thread, every time of every file reads the
    /// same: a file system whose clock does not tick during the test.
    pub fn freeze() {
        FROZEN.with(|frozen| frozen.set(true));
    }

    pub fn frozen() -> bool {
        FROZEN.with(Cell::get)
    }

    /// Runs `write` at the next [`meanwhile`] on this thread.
    pub fn during(write: impl FnOnce() + 'static) {
        MEANWHILE.with(|slot| *slot.borrow_mut() = Some(Box::new(write)));
    }

    pub fn meanwhile() {
        if let Some(write) = MEANWHILE.with(|slot| slot.borrow_mut().take()) {
            write();
        }
    }
}
