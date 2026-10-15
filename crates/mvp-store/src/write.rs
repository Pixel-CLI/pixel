// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The one writer: holds the lock file, rebuilds an unusable index, reads
//! the rows a run keeps, and publishes the next generation in one
//! transaction.

use std::collections::HashMap;
use std::fs::{File, TryLockError};
use std::path::Path;
use std::time::{Duration, Instant};

use mvp_model::{Delta, FileRow, Kept};

use rusqlite::{Connection, OptionalExtension, params};

use crate::checked::{self, Stamp};
use crate::read::stats;
use crate::rows::{self, edge_of, file_of, symbol_of};
use crate::schema::{self, Shape};
use crate::{JOURNALS, LOCK, Stats, StoreError, beside, db_path, dir, ensure_dir, existing};

/// How often a waiting writer retries the lock.
const LOCK_POLL: Duration = Duration::from_millis(10);
/// How long a statement waits for a lock SQLite holds.
const BUSY: Duration = Duration::from_secs(5);

/// What the next run plans against: the current generation's files, the
/// extractor version it was built with and the files its run left stale.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Indexed {
    pub files: Vec<FileRow>,
    /// `None` before the first generation.
    pub extractor: Option<String>,
    /// `Delta::stale` of the run that published it.
    pub stale: Vec<String>,
}

/// The single writer of a repository's index. Dropping it releases the lock.
pub struct Writer {
    conn: Connection,
    root: std::path::PathBuf,
    /// Why the index was rebuilt from nothing, if it was.
    rebuilt: Option<String>,
    /// Whether damage met after the open check emptied the index under
    /// the run in progress (see [`Writer::retried`]).
    emptied: bool,
    /// The stamp this writer last recorded of the files it left, checked
    /// (`checked`); `None` when none could be recorded, or once something
    /// else may have written them.
    seen: Option<Stamp>,
    _lock: File,
}

impl Writer {
    /// Takes the lock, waiting at most `wait` (`None`: as long as it takes),
    /// then opens the index, rebuilding it when it is damaged or of another
    /// schema (C-19). The file is checked whole whatever its stamp says.
    pub fn open(root: &Path, wait: Option<Duration>) -> Result<Writer, StoreError> {
        ensure_dir(root)?;
        let lock = acquire(&dir(root).join(LOCK), wait)?;
        let (conn, rebuilt, known) = open_or_rebuild(root)?;
        let mut writer = Writer {
            conn,
            root: root.to_path_buf(),
            rebuilt,
            emptied: false,
            seen: None,
            _lock: lock,
        };
        if known {
            writer.stamp();
        } else {
            checked::forget(root);
        }
        Ok(writer)
    }

    /// Runs `refresh`, the reading, planning and publishing of one
    /// generation, on this writer. When it fails on damage the open check
    /// did not see, done to the file after it, which rebuilds the index
    /// empty, it runs once more on the empty index and indexes every
    /// file: the run that meets the damage still succeeds, and never
    /// loops (C-19).
    pub fn retried<T, E>(
        &mut self,
        mut refresh: impl FnMut(&mut Writer) -> Result<T, E>,
    ) -> Result<T, E> {
        self.emptied = false;
        let first = refresh(self);
        if first.is_err() && std::mem::take(&mut self.emptied) {
            return refresh(self);
        }
        first
    }

    /// Why the index was rebuilt from nothing, if it was.
    pub fn rebuilt(&self) -> Option<&str> {
        self.rebuilt.as_deref()
    }

    /// The current generation, as the next run plans against it; empty
    /// before the first generation. An index found damaged here is rebuilt
    /// and reads as empty.
    pub fn indexed(&mut self) -> Result<Indexed, StoreError> {
        match read_indexed(&self.conn) {
            Err(StoreError::Unusable(reason)) => {
                self.rebuild(reason)?;
                Ok(Indexed::default())
            }
            other => other,
        }
    }

    /// Every row of the current generation outside `drop`. Damage found
    /// here rebuilds the index empty and is an error: the plan was made
    /// against rows that are gone, so nothing is published from it and
    /// [`Writer::retried`] runs it again from the empty index.
    pub fn kept(&mut self, drop: &[String]) -> Result<Kept, StoreError> {
        let read = read_kept(&self.conn, drop);
        self.rebuilt_on_damage(read)
    }

    /// Publishes the next generation in one transaction. Damage met here,
    /// done to the file after the open check, rebuilds the index empty, as
    /// in [`Writer::kept`].
    pub fn publish(&mut self, delta: &Delta) -> Result<(), StoreError> {
        // Files changed since this writer last left them were written by
        // someone else: what they hold is no longer known to be checked.
        let seen = self
            .seen
            .take()
            .filter(|seen| Stamp::of(&self.root).as_ref() == Some(seen));
        if seen.is_none() {
            checked::forget(&self.root);
        }
        let applied = apply(&mut self.conn, delta);
        let applied = self.rebuilt_on_damage(applied);
        checked::meanwhile();
        if applied.is_ok() {
            // The commit wrote the WAL only (`wal_autocheckpoint` is off):
            // a `graph.db` changed since was written by someone else.
            let unchanged = seen
                .is_some_and(|seen| Stamp::of(&self.root).is_some_and(|now| now.same_db(&seen)));
            if unchanged {
                self.stamp();
            } else {
                checkpoint(&self.conn);
                checked::forget(&self.root);
            }
        }
        applied
    }

    /// Folds the WAL into `graph.db`, then records the files as checked:
    /// called only when they hold what a check passed and this writer's
    /// own writes, nothing else (ADR-0022). A reader holding the WAL leaves
    /// it partly folded or whole; the stamp then holds it as it is.
    fn stamp(&mut self) {
        checkpoint(&self.conn);
        self.seen = Stamp::settled(&self.root);
        match &self.seen {
            Some(stamp) => checked::record(&self.root, stamp),
            None => checked::forget(&self.root),
        }
    }

    /// The counts of the generation this writer last published, read on
    /// its own connection: no second open, no second check of the file.
    pub fn stats(&mut self) -> Result<Stats, StoreError> {
        let read = rows::published(&self.conn).and_then(|published| {
            stats(&self.conn, published.map(|p| p.stale).unwrap_or_default())
        });
        self.rebuilt_on_damage(read)
    }

    fn rebuilt_on_damage<T>(&mut self, result: Result<T, StoreError>) -> Result<T, StoreError> {
        match result {
            Err(StoreError::Unusable(reason)) => {
                self.rebuild(reason.clone())?;
                self.emptied = true;
                Err(StoreError::Unusable(format!("{reason}; rebuilt empty")))
            }
            other => other,
        }
    }

    fn rebuild(&mut self, reason: String) -> Result<(), StoreError> {
        // Closed first: closing a connection in WAL mode deletes `-wal` by
        // name, which would by then be the new index's.
        self.conn = Connection::open_in_memory()?;
        self.conn = fresh(&self.root)?;
        self.rebuilt = Some(reason);
        self.stamp();
        Ok(())
    }
}

/// The delta's rows, in a transaction committed only when the stored
/// generation becomes the one the delta names.
fn apply(conn: &mut Connection, delta: &Delta) -> Result<(), StoreError> {
    let tx = conn.transaction()?;
    {
        let mut drop_edges =
            tx.prepare("DELETE FROM edges WHERE src IN (SELECT id FROM symbols WHERE file = ?1)")?;
        let mut drop_symbols = tx.prepare("DELETE FROM symbols WHERE file = ?1")?;
        let mut drop_file = tx.prepare("DELETE FROM files WHERE path = ?1")?;
        for path in &delta.drop {
            drop_edges.execute([path])?;
            drop_symbols.execute([path])?;
            drop_file.execute([path])?;
        }
        let mut add_file = tx.prepare(
            "INSERT INTO files (path, hash, lang, indexed_at, facts) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let mut add_symbol = tx.prepare(
            "INSERT INTO symbols (uid, file, kind, name, leaf, signature, line, end_line)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        let mut add_edge = tx.prepare(
            "INSERT INTO edges (src, dst, kind, line, resolved, target, context)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        let mut symbol_id = tx.prepare("SELECT id FROM symbols WHERE uid = ?1")?;
        for rows in &delta.add {
            add_file.execute(params![
                rows.file.path,
                rows.file.hash.to_hex(),
                rows.file.lang.as_str(),
                delta.provenance.indexed_at,
                rows.facts,
            ])?;
            let mut ids: HashMap<&str, i64> = HashMap::with_capacity(rows.symbols.len());
            for symbol in &rows.symbols {
                add_symbol.execute(params![
                    symbol.uid,
                    symbol.file,
                    symbol.kind.as_str(),
                    symbol.name,
                    mvp_model::leaf(&symbol.name),
                    symbol.signature,
                    symbol.line,
                    symbol.end_line,
                ])?;
                ids.insert(symbol.uid.as_str(), tx.last_insert_rowid());
            }
            for edge in &rows.edges {
                // A site's symbol is its file's; one of another file the
                // index holds is looked up, one it does not hold refused.
                let src = match ids.get(edge.src.as_str()) {
                    Some(&id) => id,
                    None => symbol_id
                        .query_row([&edge.src], |r| r.get(0))
                        .optional()?
                        .ok_or_else(|| {
                            StoreError::Refused(format!(
                                "an edge of {} leaves {}, which no symbol of the index is",
                                rows.file.path, edge.src
                            ))
                        })?,
                };
                add_edge.execute(params![
                    src,
                    edge.dst,
                    edge.kind.as_str(),
                    edge.line,
                    edge.dst.is_some(),
                    edge.target,
                    edge.context,
                ])?;
            }
        }
        let mut redirect = tx.prepare(
            "UPDATE edges SET dst = ?1, resolved = ?2
             WHERE src = (SELECT id FROM symbols WHERE uid = ?3) AND kind = ?4 AND line = ?5
               AND target = ?6 AND context = ?7 AND dst IS ?8",
        )?;
        for (edge, dst) in &delta.redirect {
            redirect.execute(params![
                dst,
                dst.is_some(),
                edge.src,
                edge.kind.as_str(),
                edge.line,
                edge.target,
                edge.context,
                edge.dst,
            ])?;
        }
        let mut meta = tx.prepare("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)")?;
        let provenance = &delta.provenance;
        meta.execute(["generation", &provenance.generation])?;
        meta.execute(["commit", provenance.commit.as_deref().unwrap_or("")])?;
        meta.execute(["dirty", if provenance.dirty { "1" } else { "0" }])?;
        meta.execute(["extractor", &provenance.extractor])?;
        meta.execute(["indexed_at", &provenance.indexed_at.to_string()])?;
        meta.execute(["stale", &rows::stale_value(&delta.stale)])?;
        // Every file's stat as this scan saw it, kept or written anew: a
        // file only touched since keeps its rows and gets its new stat.
        let mut stat = tx.prepare(
            "UPDATE files SET stat_len = ?2, stat_mtime = ?3, stat_ctime = ?4, stat_ino = ?5
             WHERE path = ?1",
        )?;
        for (path, s) in &delta.stats {
            let s = s.map(|s| (s.len as i64, s.mtime_ns, s.ctime_ns, s.ino as i64));
            stat.execute(params![
                path,
                s.map(|s| s.0),
                s.map(|s| s.1),
                s.map(|s| s.2),
                s.map(|s| s.3)
            ])?;
        }
    }
    // The delta must turn the stored generation into the one it names,
    // file for file; a delta made against other rows is refused, never
    // published.
    let stored = rows::files(&tx)?;
    if stored != delta.files {
        return Err(StoreError::Refused(difference(&delta.files, &stored)));
    }
    crate::words::publish(&tx)?;
    crate::postings::publish(&tx)?;
    tx.commit()?;
    Ok(())
}

/// The first file where what a delta names and what it would leave part,
/// and in what.
fn difference(named: &[FileRow], stored: &[FileRow]) -> String {
    let counts = format!(
        "it names {} files where the store would hold {}",
        named.len(),
        stored.len()
    );
    let at = named
        .iter()
        .zip(stored)
        .position(|(named, stored)| named != stored)
        .unwrap_or(named.len().min(stored.len()));
    match (named.get(at), stored.get(at)) {
        (Some(n), Some(s)) if n.path == s.path && n.hash != s.hash => format!(
            "{counts}: {} with hash {} against {}",
            n.path,
            &n.hash.to_hex()[..8],
            &s.hash.to_hex()[..8]
        ),
        (Some(n), Some(s)) if n.path == s.path => format!(
            "{counts}: {} in {} against {}",
            n.path,
            n.lang.as_str(),
            s.lang.as_str()
        ),
        _ => format!(
            "{counts}: {} against {}",
            path_at(named, at),
            path_at(stored, at)
        ),
    }
}

fn path_at(files: &[FileRow], at: usize) -> &str {
    files.get(at).map_or("nothing", |file| file.path.as_str())
}

/// Whether a writer holds the lock of the repository at `root` now. For a
/// caller that would start one only when none runs; the answer may be stale
/// the moment it is given, and a writer started anyway waits its turn.
pub fn writing(root: &Path) -> bool {
    if !matches!(crate::own(root), Ok(true)) {
        return false;
    }
    // Never made here, never through a link, never held by a FIFO (C-18).
    let Ok(file) = crate::state_file::open_lock(&dir(root).join(LOCK), false) else {
        return false;
    };
    matches!(file.try_lock(), Err(TryLockError::WouldBlock))
}

/// Takes the writer's lock at `path`, a regular file opened never
/// through a link and never held by a FIFO. A link, a FIFO or a device a
/// clone left at its name is no writer's lock: it is removed, and the
/// lock made in its place (C-18).
fn acquire(path: &Path, wait: Option<Duration>) -> Result<File, StoreError> {
    crate::state_file::clear_special(path);
    let file = crate::state_file::open_lock(path, true)?;
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {
                if wait.is_some_and(|limit| started.elapsed() >= limit) {
                    return Err(StoreError::Busy);
                }
                std::thread::sleep(LOCK_POLL);
            }
            Err(TryLockError::Error(err)) => return Err(StoreError::Io(err)),
        }
    }
}

/// Opens `graph.db` and checks it whole; an empty file or an unusable
/// one is replaced by a new index, and the reason for an unusable one
/// returned. The flag says whether the files hold only what the check
/// passed or what this writer wrote: false when another process wrote
/// them while the check read them.
fn open_or_rebuild(root: &Path) -> Result<(Connection, Option<String>, bool), StoreError> {
    match existing(&db_path(root)) {
        Err(StoreError::Unusable(reason)) => return Ok((fresh(root)?, Some(reason), true)),
        Err(err) => return Err(err),
        Ok(_) => {}
    }
    let before = Stamp::settled(root);
    let attempt = Connection::open(db_path(root)).map_err(StoreError::from);
    let shape = match &attempt {
        Ok(conn) => schema::shape(conn, || false),
        Err(err) => Shape::Unusable(err.to_string()),
    };
    match (attempt, shape) {
        (Ok(conn), Shape::Current) => {
            checked::meanwhile();
            let unchanged = before.is_some_and(|before| Stamp::of(root) == Some(before));
            configure(&conn)?;
            Ok((conn, None, unchanged))
        }
        (attempt, Shape::Empty) => {
            drop(attempt);
            Ok((fresh(root)?, None, true))
        }
        (attempt, Shape::Unusable(reason)) => {
            drop(attempt);
            Ok((fresh(root)?, Some(reason), true))
        }
        (Err(err), _) => Err(err),
    }
}

/// Deletes `graph.db` and its journal files, whatever they are, and
/// creates an empty index.
fn fresh(root: &Path) -> Result<Connection, StoreError> {
    checked::forget(root);
    let db = db_path(root);
    remove(&db)?;
    for suffix in JOURNALS {
        remove(&beside(&db, suffix))?;
    }
    let mut conn = Connection::open(&db)?;
    configure(&conn)?;
    schema::create(&mut conn)?;
    Ok(conn)
}

/// Removes what is at `path`: a file, a link, or a directory with its
/// contents. Nothing there is fine.
fn remove(path: &Path) -> std::io::Result<()> {
    let removed = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(err) => Err(err),
    };
    match removed {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// WAL lets readers see the last published generation while a writer
/// works on the next one. Only [`checkpoint`] writes `graph.db`, so that a
/// change to it during a publish is someone else's.
fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "wal_autocheckpoint", 0)?;
    conn.busy_timeout(BUSY)
}

/// Folds the WAL into `graph.db` and empties it, as far as no reader holds
/// it, without waiting for one: a publish never waits on a prompt hook.
fn checkpoint(conn: &Connection) {
    let _ = conn.busy_timeout(Duration::ZERO);
    let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
    let _ = conn.busy_timeout(BUSY);
}

fn read_indexed(conn: &Connection) -> Result<Indexed, StoreError> {
    let Some(published) = rows::published(conn)? else {
        return Ok(Indexed::default());
    };
    Ok(Indexed {
        files: rows::files(conn)?,
        extractor: Some(published.provenance.extractor),
        stale: published.stale,
    })
}

fn read_kept(conn: &Connection, drop: &[String]) -> Result<Kept, StoreError> {
    let dropped: std::collections::HashSet<&str> = drop.iter().map(String::as_str).collect();
    let mut kept = Kept::default();
    let mut stmt = conn.prepare("SELECT path, hash, lang, facts FROM files ORDER BY path")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let file = file_of(row)?;
        if !dropped.contains(file.path.as_str()) {
            kept.files.push((file, row.get(3)?));
        }
    }
    let mut stmt =
        conn.prepare("SELECT uid, file, kind, name, signature, line, end_line, id FROM symbols")?;
    let mut rows = stmt.query([])?;
    // A kept symbol's id → its index in `kept.symbols`.
    let mut kept_ids: HashMap<i64, usize> = HashMap::new();
    while let Some(row) = rows.next()? {
        let file: String = row.get(1)?;
        if !dropped.contains(file.as_str()) {
            kept_ids.insert(row.get(7)?, kept.symbols.len());
            kept.symbols.push(symbol_of(row, file)?);
        }
    }
    let mut stmt = conn.prepare("SELECT src, dst, kind, line, target, context FROM edges")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let src: i64 = row.get(0)?;
        if let Some(&at) = kept_ids.get(&src) {
            kept.edges.push(edge_of(row, kept.symbols[at].uid.clone())?);
        }
    }
    Ok(kept)
}
