// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The full build's staging tables (`snapshot_files`, `pending_calls`,
//! `pending_refs`): what its writer records for each file so that nothing
//! the build learns about the whole tree has to stay in memory until the
//! passes that need every file stored.

use std::os::unix::fs::MetadataExt;

use rusqlite::params;
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

use crate::resolve::{FileCalls, FileReferences, PendingCall, PendingReference};
use crate::store::{ExecCached, GraphStore, StoreError};

/// What the walk knew of one file when it read it: the xxh3 of the bytes
/// it read, and the size, mtime and inode the opened file had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Snapshot {
    pub hash: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub ino: u64,
}

impl Snapshot {
    /// The snapshot of `content`, read from a file whose stat was `meta`.
    pub fn of(content: &[u8], meta: &std::fs::Metadata) -> Self {
        Self {
            hash: xxh3_64(content),
            size: meta.size(),
            mtime_ns: mtime_ns(meta),
            ino: meta.ino(),
        }
    }
}

/// `meta`'s mtime in nanoseconds since the epoch, saturating.
fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(meta.mtime_nsec())
}

/// Empties every staging table, before a full build writes them again.
pub(super) fn clear(store: &GraphStore) -> Result<(), StoreError> {
    store.conn().execute_batch(
        "DELETE FROM snapshot_files; DELETE FROM pending_calls; DELETE FROM pending_refs;",
    )?;
    Ok(())
}

/// Empties the call and reference staging tables once they are resolved.
pub(super) fn clear_pending(store: &GraphStore) -> Result<(), StoreError> {
    store
        .conn()
        .execute_batch("DELETE FROM pending_calls; DELETE FROM pending_refs;")?;
    Ok(())
}

/// Records that the walk read `rel` as `snapshot`.
pub(super) fn record_snapshot(
    store: &GraphStore,
    rel: &str,
    snapshot: &Snapshot,
) -> Result<(), StoreError> {
    store.conn().exec_cached(
        "INSERT INTO snapshot_files (path, hash, size, mtime_ns, ino) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(path) DO UPDATE SET hash = excluded.hash, size = excluded.size,
                                         mtime_ns = excluded.mtime_ns, ino = excluded.ino",
        params![
            rel,
            snapshot.hash as i64,
            snapshot.size as i64,
            snapshot.mtime_ns,
            snapshot.ino as i64
        ],
    )?;
    Ok(())
}

/// Stages one call site of `file_id` for resolution.
pub(super) fn stage_call(
    store: &GraphStore,
    file_id: i64,
    call: &PendingCall,
) -> Result<(), StoreError> {
    store.conn().exec_cached(
        "INSERT INTO pending_calls (file_id, name, enclosing_symbol_id, site_line, receiver)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            file_id,
            call.callee_name,
            call.enclosing_symbol_id,
            call.site_line,
            call.receiver
        ],
    )?;
    Ok(())
}

/// Stages one reference site of `file_id` for resolution.
pub(super) fn stage_reference(
    store: &GraphStore,
    file_id: i64,
    reference: &PendingReference,
) -> Result<(), StoreError> {
    store.conn().exec_cached(
        "INSERT INTO pending_refs (file_id, name, enclosing_symbol_id, site_line, arg_of)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            file_id,
            reference.name,
            reference.enclosing_symbol_id,
            reference.site_line,
            reference.arg_of
        ],
    )?;
    Ok(())
}

/// Drops the staged sites of every `lang` file: when the graph's Rust edges
/// are pixel-mvp's, this resolver decides no Rust site.
pub(super) fn unstage_lang(store: &GraphStore, lang: &str) -> Result<(), StoreError> {
    for table in ["pending_calls", "pending_refs"] {
        store.conn().execute(
            &format!("DELETE FROM {table} WHERE file_id IN (SELECT id FROM files WHERE lang = ?1)"),
            params![lang],
        )?;
    }
    Ok(())
}

/// The freshness signature of the snapshot: the xxh3 of every recorded
/// path followed by its hash (little-endian), in path order. Streamed, and
/// equal to `signature_of` over the same entries: SQLite's binary collation
/// orders UTF-8 text as Rust orders `str`.
pub(super) fn signature(store: &GraphStore) -> Result<String, StoreError> {
    let mut hasher = Xxh3::new();
    let mut stmt = store
        .conn()
        .prepare("SELECT path, hash FROM snapshot_files ORDER BY path")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let path = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
        hasher.update(path.as_bytes());
        hasher.update(&(row.get::<_, i64>(1)? as u64).to_le_bytes());
    }
    Ok(format!("{:016x}", hasher.digest()))
}

/// Every staged call, grouped by file in staging order.
pub(super) fn pending_calls(store: &GraphStore) -> Result<Vec<FileCalls>, StoreError> {
    let mut stmt = store.conn().prepare(
        "SELECT file_id, name, enclosing_symbol_id, site_line, receiver
           FROM pending_calls ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            PendingCall {
                callee_name: r.get(1)?,
                enclosing_symbol_id: r.get(2)?,
                site_line: r.get(3)?,
                receiver: r.get(4)?,
            },
        ))
    })?;
    let mut out: Vec<FileCalls> = Vec::new();
    for row in rows {
        let (file_id, call) = row?;
        match out.last_mut() {
            Some(last) if last.file_id == file_id => last.calls.push(call),
            _ => out.push(FileCalls {
                file_id,
                calls: vec![call],
            }),
        }
    }
    Ok(out)
}

/// Every staged reference, grouped by file in staging order.
pub(super) fn pending_references(store: &GraphStore) -> Result<Vec<FileReferences>, StoreError> {
    let mut stmt = store.conn().prepare(
        "SELECT file_id, name, enclosing_symbol_id, site_line, arg_of
           FROM pending_refs ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            PendingReference {
                name: r.get(1)?,
                enclosing_symbol_id: r.get(2)?,
                site_line: r.get(3)?,
                arg_of: r.get(4)?,
            },
        ))
    })?;
    let mut out: Vec<FileReferences> = Vec::new();
    for row in rows {
        let (file_id, reference) = row?;
        match out.last_mut() {
            Some(last) if last.file_id == file_id => last.references.push(reference),
            _ => out.push(FileReferences {
                file_id,
                references: vec![reference],
            }),
        }
    }
    Ok(out)
}
