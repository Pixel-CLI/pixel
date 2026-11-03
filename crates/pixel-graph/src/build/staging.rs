// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The full build's staging tables (`snapshot_files`, `pending_calls`,
//! `pending_refs`): what its writer records for each file so that nothing
//! the build learns about the whole tree has to stay in memory until the
//! passes that need every file stored.

use std::os::unix::fs::MetadataExt;

use rusqlite::params;
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

use crate::imports::{PathLookup, has_path_suffix};
use rustc_hash::FxHashSet as HashSet;

use crate::resolve::{
    BatchResolver, FileCalls, FileReferences, PendingCall, PendingReference, resolve_calls_in,
    resolve_references_in,
};
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

/// The bounds of the full build's resolution passes: at most `files` files
/// and `sites` staged sites per batch, and about `cache_bytes` of per-name
/// facts kept between batches.
#[derive(Debug, Clone, Copy)]
pub(super) struct ResolveLimits {
    pub files: usize,
    pub sites: usize,
    pub cache_bytes: usize,
}

/// The production resolution bounds.
pub(super) const RESOLVE_LIMITS: ResolveLimits = ResolveLimits {
    files: 2048,
    sites: 65_536,
    cache_bytes: 32 * 1024 * 1024,
};

/// One file's staged sites: its id and the sites, in staging order.
type FileSites<T> = (i64, Vec<T>);

/// The next batch of staged rows after row `after`, read by `read_row`
/// from `sql` (which selects `id, file_id, …` ordered by id): at most
/// `limits.sites` rows of at most `limits.files` files,
/// grouped by file in staging order. Returns the groups and the last row id
/// read; empty groups when nothing is left.
fn next_batch<T>(
    store: &GraphStore,
    limits: ResolveLimits,
    sql: &str,
    after: i64,
    read_row: impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<(Vec<FileSites<T>>, i64), StoreError> {
    let mut stmt = store.conn().prepare_cached(sql)?;
    let mut rows = stmt.query(params![after, limits.sites as i64])?;
    let mut groups: Vec<FileSites<T>> = Vec::new();
    let mut last = after;
    while let Some(row) = rows.next()? {
        let (id, file_id): (i64, i64) = (row.get(0)?, row.get(1)?);
        let same_file = groups.last().is_some_and(|(file, _)| *file == file_id);
        if !same_file && groups.len() == limits.files {
            break;
        }
        let site = read_row(row)?;
        if same_file && let Some((_, sites)) = groups.last_mut() {
            sites.push(site);
        } else {
            groups.push((file_id, vec![site]));
        }
        last = id;
    }
    Ok((groups, last))
}

/// Resolves every staged call, then every staged reference, a batch at a
/// time, each batch against an index of the per-file facts of its files and
/// the per-name facts of the names it reads ([`BatchResolver`]). Returns the
/// time each pass took.
pub(super) fn resolve_staged(
    store: &GraphStore,
    limits: ResolveLimits,
) -> Result<(std::time::Duration, std::time::Duration), StoreError> {
    let mut resolver = BatchResolver::new(store, limits.cache_bytes)?;
    let clock = std::time::Instant::now();
    let mut after = 0;
    loop {
        let (groups, last) = next_batch(
            store,
            limits,
            "SELECT id, file_id, name, enclosing_symbol_id, site_line, receiver
               FROM pending_calls WHERE id > ?1 ORDER BY id LIMIT ?2",
            after,
            |r| {
                Ok(PendingCall {
                    callee_name: r.get(2)?,
                    enclosing_symbol_id: r.get(3)?,
                    site_line: r.get(4)?,
                    receiver: r.get(5)?,
                })
            },
        )?;
        if groups.is_empty() {
            break;
        }
        after = last;
        let batch: Vec<FileCalls> = groups
            .into_iter()
            .map(|(file_id, calls)| FileCalls { file_id, calls })
            .collect();
        let files: HashSet<i64> = batch.iter().map(|f| f.file_id).collect();
        let idx = resolver.index_for(
            store,
            &files,
            batch
                .iter()
                .flat_map(|f| f.calls.iter().map(|c| c.callee_name.as_str())),
        )?;
        resolve_calls_in(store, &idx, &batch)?;
    }
    let calls = clock.elapsed();
    let clock = std::time::Instant::now();
    let mut after = 0;
    loop {
        let (groups, last) = next_batch(
            store,
            limits,
            "SELECT id, file_id, name, enclosing_symbol_id, site_line, arg_of
               FROM pending_refs WHERE id > ?1 ORDER BY id LIMIT ?2",
            after,
            |r| {
                Ok(PendingReference {
                    name: r.get(2)?,
                    enclosing_symbol_id: r.get(3)?,
                    site_line: r.get(4)?,
                    arg_of: r.get(5)?,
                })
            },
        )?;
        if groups.is_empty() {
            break;
        }
        after = last;
        let batch: Vec<FileReferences> = groups
            .into_iter()
            .map(|(file_id, references)| FileReferences {
                file_id,
                references,
            })
            .collect();
        let files: HashSet<i64> = batch.iter().map(|f| f.file_id).collect();
        let idx = resolver.index_for(
            store,
            &files,
            batch
                .iter()
                .flat_map(|f| f.references.iter().map(|r| r.name.as_str())),
        )?;
        resolve_references_in(store, &idx, &batch)?;
    }
    Ok((calls, clock.elapsed()))
}

/// A [`PathLookup`] answered by the store's `files` rows: the full build
/// resolves its imports without holding the file list. A query that fails
/// answers "no such file" and is kept for [`Self::finish`].
pub(super) struct SqlPathLookup<'a> {
    store: &'a GraphStore,
    error: std::cell::RefCell<Option<StoreError>>,
}

impl<'a> SqlPathLookup<'a> {
    pub fn new(store: &'a GraphStore) -> Self {
        Self {
            store,
            error: std::cell::RefCell::new(None),
        }
    }

    /// The first query error any answer swallowed.
    pub fn finish(&self) -> Result<(), StoreError> {
        self.error.borrow_mut().take().map_or(Ok(()), Err)
    }

    /// `query`'s answer, or `default` after keeping its error.
    fn answer<T>(&self, default: T, query: impl FnOnce() -> Result<T, StoreError>) -> T {
        query().unwrap_or_else(|error| {
            self.error.borrow_mut().get_or_insert(error);
            default
        })
    }

    /// The paths of the rows `sql` selects with `params`, kept by `keep`.
    fn paths(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
        keep: impl Fn(&str) -> bool,
        limit: usize,
    ) -> Vec<String> {
        self.answer(Vec::new(), || {
            let mut stmt = self.store.conn().prepare_cached(sql)?;
            let mut rows = stmt.query(params)?;
            let mut out = Vec::new();
            while out.len() < limit
                && let Some(row) = rows.next()?
            {
                let path: String = row.get(0)?;
                if keep(&path) {
                    out.push(path);
                }
            }
            Ok(out)
        })
    }

    /// The id of the file row at `path`.
    pub fn id_of(&self, path: &str) -> Result<Option<i64>, StoreError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .store
            .conn()
            .prepare_cached("SELECT id FROM files WHERE path = ?1")?
            .query_row(params![path], |r| r.get(0))
            .optional()?)
    }
}

impl PathLookup for SqlPathLookup<'_> {
    fn contains(&self, path: &str) -> bool {
        self.answer(false, || Ok(self.id_of(path)?.is_some()))
    }

    fn ending_with(&self, suffix: &str, limit: usize) -> Vec<String> {
        // A file ending with `/suffix` has the suffix's last segment, and
        // its directory the last segment of the suffix's directory.
        let keep = |path: &str| has_path_suffix(path, suffix);
        match suffix.rsplit_once('/') {
            Some((dir, basename)) => self.paths(
                "SELECT path FROM files WHERE basename = ?1 AND dir_base = ?2",
                params![basename, dir.rsplit('/').next().unwrap_or(dir)],
                keep,
                limit,
            ),
            None => self.paths(
                "SELECT path FROM files WHERE basename = ?1",
                params![suffix],
                keep,
                limit,
            ),
        }
    }

    fn in_dirs_ending_with(&self, dir_suffix: &str, ext: &str) -> Vec<String> {
        let dir_base = dir_suffix.rsplit('/').next().unwrap_or(dir_suffix);
        self.paths(
            "SELECT path FROM files WHERE dir_base = ?1",
            params![dir_base],
            |path| {
                path.ends_with(ext)
                    && has_path_suffix(path.rsplit_once('/').map_or("", |(dir, _)| dir), dir_suffix)
            },
            usize::MAX,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imports::{PathSet, resolve_import_in, ruby::Projects};

    const FILES: &[&str] = &[
        "Gemfile",
        "lib/shop/cart.rb",
        "lib/shop.rb",
        "app/models/order.rb",
        "src/lib.rs",
        "src/util.rs",
        "src/net/mod.rs",
        "src/net/http.rs",
        "crates/a/src/util.rs",
        "web/a.ts",
        "web/b.ts",
        "web/c/index.tsx",
        "pkg/__init__.py",
        "pkg/mod.py",
        "other/pkg/__init__.py",
        "go/net/http/a.go",
        "go/net/http/b.go",
        "vendor/net/http/c.go",
        "go/util/u.go",
        "java/com/acme/Order.java",
        "java/com/acme/Line.java",
        "root.go",
    ];

    /// The store answers every question of import resolution as the file
    /// list does, so the full build (which asks the store) and the
    /// incremental update (which asks a list) store the same import rows.
    #[test]
    fn the_store_should_resolve_imports_as_the_file_list_does() {
        let mut store = GraphStore::open_in_memory().unwrap();
        for path in FILES {
            store.replace_file(path, "0", "x").unwrap();
        }
        let files: Vec<String> = FILES.iter().map(ToString::to_string).collect();
        let list = PathSet::new(&files);
        let sql = SqlPathLookup::new(&store);
        let sorted = |mut v: Vec<String>| {
            v.sort();
            v
        };
        for suffix in [
            "util.rs",
            "src/util.rs",
            "net/mod.rs",
            "__init__.py",
            "pkg/__init__.py",
            "b.ts",
            "nothing.rs",
            "Gemfile",
        ] {
            assert_eq!(
                sorted(sql.ending_with(suffix, usize::MAX)),
                sorted(list.ending_with(suffix, usize::MAX)),
                "{suffix}"
            );
        }
        for (dir, ext) in [
            ("net/http", ".go"),
            ("http", ".go"),
            ("go/util", ".go"),
            ("com/acme", ".java"),
            ("", ".go"),
            ("none", ".go"),
        ] {
            assert_eq!(
                sorted(sql.in_dirs_ending_with(dir, ext)),
                sorted(list.in_dirs_ending_with(dir, ext)),
                "{dir} {ext}"
            );
        }
        let projects = Projects::from_paths(&files);
        for (spec, importer) in [
            ("crate::util::f", "src/lib.rs"),
            ("crate::net::http", "src/lib.rs"),
            ("util::f", "src/net/http.rs"),
            ("./b", "web/a.ts"),
            ("./c", "web/a.ts"),
            ("pkg.mod", "x.py"),
            ("pkg", "x.py"),
            ("net/http", "go/util/u.go"),
            ("github.com/x/util", "go/net/http/a.go"),
            ("com.acme.*", "java/com/acme/Order.java"),
            ("com.acme.Line", "java/com/acme/Order.java"),
            ("shop/cart", "app/models/order.rb"),
            ("./shop/cart", "lib/shop.rb"),
        ] {
            assert_eq!(
                resolve_import_in(spec, importer, &sql, &projects),
                resolve_import_in(spec, importer, &list, &projects),
                "{spec} from {importer}"
            );
        }
        sql.finish().unwrap();
    }

    /// The streamed signature is the one-shot xxh3 of the same bytes.
    #[test]
    fn the_snapshot_signature_should_hash_paths_and_hashes_in_path_order() {
        let store = GraphStore::open_in_memory().unwrap();
        let snapshot = |hash| Snapshot {
            hash,
            size: 1,
            mtime_ns: 2,
            ino: 3,
        };
        record_snapshot(&store, "b.rs", &snapshot(7)).unwrap();
        record_snapshot(&store, "a.rs", &snapshot(u64::MAX)).unwrap();
        let mut bytes = b"a.rs".to_vec();
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        bytes.extend_from_slice(b"b.rs");
        bytes.extend_from_slice(&7u64.to_le_bytes());
        assert_eq!(
            signature(&store).unwrap(),
            format!("{:016x}", xxh3_64(&bytes))
        );
    }
}
