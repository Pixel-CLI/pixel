// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Reading a published generation. Nothing here writes the index, only
//! its stamp (`checked`); an index that is missing, damaged or of another
//! schema is an error, never served (C-19).

use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use mvp_model::hash::Hash;
use mvp_model::{Edge, FileRow, FileStat, Graph, GraphError, Provenance, Symbol};
use rusqlite::{Connection, OpenFlags};

use crate::checked::{self, Stamp};
use crate::rows::{self, Published, edge_of, symbol_of};
use crate::schema::{self, Shape};
use crate::{StoreError, db_path, existing};

/// A read-only view of the last published generation.
pub struct Store {
    conn: Connection,
    /// `meta`, read and checked when the store was opened.
    published: Published,
}

impl Store {
    /// Opens the index of the repository at `root` for reading. The file is
    /// checked whole first (`schema::shape`), unless this version of it is
    /// stamped as checked (`checked`), so a damaged page or value is
    /// refused here rather than met by a later query. A full check that
    /// passes stamps the files, when nothing wrote them while it ran.
    pub fn open(root: &Path) -> Result<Store, StoreError> {
        let path = db_path(root);
        if !crate::own(root)? || !existing(&path)? {
            return Err(StoreError::NoIndex);
        }
        // Taken before the connection reads anything; no probe written when
        // the files look stamped already.
        let before = if checked::trusted(root) {
            None
        } else {
            Stamp::settled(root)
        };
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(StoreError::from)?;
        conn.busy_timeout(Duration::from_secs(2))?;
        // One read transaction for the reader's life: every query sees the
        // same generation, the one checked here, whatever a writer
        // publishes meanwhile.
        conn.execute_batch("BEGIN DEFERRED")?;
        let started = SystemTime::now();
        let full = Cell::new(false);
        let trusted = || {
            full.set(!checked::trusted(root));
            !full.get()
        };
        match schema::shape(&conn, trusted) {
            Shape::Current => {}
            Shape::Empty => return Err(StoreError::NoIndex),
            Shape::Unusable(reason) => return Err(StoreError::Unusable(reason)),
        }
        let Some(published) = rows::published(&conn)? else {
            return Err(StoreError::NoIndex);
        };
        checked::meanwhile();
        if full.get()
            && let Some(before) = before
            && Stamp::of(root).as_ref() == Some(&before)
        {
            checked::record(root, &before, started);
        }
        Ok(Store { conn, published })
    }

    /// The hash and stat of every file whose stat the generation keeps, by
    /// path: what the index's `scan_with` takes a file's hash from
    /// while its stat holds (ADR-0022 §2).
    pub fn known(&self) -> Result<HashMap<String, (Hash, FileStat)>, StoreError> {
        rows::known(&self.conn)
    }

    /// Every edge of the generation, resolved or not; for checks that compare
    /// the store with the indexer's output.
    pub fn all_edges(&self) -> Result<Vec<Edge>, StoreError> {
        self.edges(&format!("{EDGES} ORDER BY e.rowid"), [])
    }

    /// Every edge whose site is in `file`, resolved or not, in the order
    /// `all_edges` gives them: what a reader that follows the index one
    /// file at a time reads in place of every edge (v1's Ruby merge,
    /// ADR-0037).
    pub fn edges_of(&self, file: &str) -> Result<Vec<Edge>, StoreError> {
        self.edges(
            &format!(
                "{EDGES} WHERE e.src IN (SELECT id FROM symbols WHERE file = ?1) ORDER BY e.rowid"
            ),
            [file],
        )
    }

    /// The files holding a site resolved to a symbol of `file`, sorted,
    /// `file` among them when it calls itself: those whose answer
    /// changes when what `file` defines does.
    pub fn files_reaching(&self, file: &str) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT s.file FROM edges e JOIN symbols s ON s.id = e.src
              WHERE e.dst IN (SELECT uid FROM symbols WHERE file = ?1) ORDER BY s.file",
        )?;
        let rows = stmt.query_map([file], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn edges<P: rusqlite::Params>(&self, sql: &str, params: P) -> Result<Vec<Edge>, StoreError> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        let mut rows = stmt.query(params)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(edge_of(row, row.get(0)?)?);
        }
        Ok(out)
    }
}

impl Graph for Store {
    fn provenance(&self) -> Result<Provenance, GraphError> {
        Ok(self.published.provenance.clone())
    }

    fn files(&self) -> Result<Vec<FileRow>, GraphError> {
        rows::files(&self.conn).map_err(graph_error)
    }

    fn symbols(&self, file: &str) -> Result<Vec<Symbol>, GraphError> {
        let read = || -> Result<Vec<Symbol>, StoreError> {
            let mut stmt = self.conn.prepare_cached(
                "SELECT uid, file, kind, name, signature, line, end_line FROM symbols
                 WHERE file = ?1 ORDER BY line, uid",
            )?;
            let mut rows = stmt.query([file])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(symbol_of(row, row.get(1)?)?);
            }
            Ok(out)
        };
        read().map_err(graph_error)
    }

    fn named(&self, token: &str) -> Result<Vec<Symbol>, GraphError> {
        let read = || -> Result<Vec<Symbol>, StoreError> {
            let mut stmt = self.conn.prepare_cached(NAMED)?;
            let mut rows = stmt.query([token])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(symbol_of(row, row.get(1)?)?);
            }
            Ok(out)
        };
        read().map_err(graph_error)
    }
}

/// An edge as [`edge_of`] reads it, with its site's symbol's uid for
/// `edges.src`, which holds that symbol's id.
const EDGES: &str = "SELECT s.uid, e.dst, e.kind, e.line, e.target, e.context
     FROM edges e JOIN symbols s ON s.id = e.src";

/// The symbols a token names, by name or by leaf: two searches, one per
/// name index, rather than one `OR` the planner may answer by walking the
/// uid index in order.
const NAMED: &str = "SELECT uid, file, kind, name, signature, line, end_line FROM symbols
     WHERE name = ?1 COLLATE NOCASE
     UNION
     SELECT uid, file, kind, name, signature, line, end_line FROM symbols
     WHERE leaf = ?1 COLLATE NOCASE
     ORDER BY uid";

fn graph_error(err: StoreError) -> GraphError {
    GraphError(err.to_string())
}

#[cfg(test)]
mod tests {
    use mvp_model::hash::Hash;
    use mvp_model::{Delta, FileRow, FileRows, Lang, SymbolKind};

    use super::*;
    use crate::Writer;

    /// A store of `(file, symbol names)`, one file per entry.
    fn store(dir: &Path, files: &[(&str, &[&str])]) -> Store {
        let row = |path: &str| FileRow {
            path: path.to_owned(),
            lang: Lang::Rust,
            hash: Hash::of(path.as_bytes()),
        };
        let add = files
            .iter()
            .map(|(path, names)| FileRows {
                file: row(path),
                facts: String::new(),
                symbols: names
                    .iter()
                    .enumerate()
                    .map(|(i, name)| Symbol {
                        uid: format!("{path}#{name}#{i}"),
                        file: (*path).to_owned(),
                        kind: SymbolKind::Function,
                        name: (*name).to_owned(),
                        signature: format!("fn {name}()"),
                        line: i as u32 + 1,
                        end_line: i as u32 + 1,
                    })
                    .collect(),
                edges: Vec::new(),
            })
            .collect();
        let mut writer = Writer::open(dir, None).expect("writer");
        writer
            .publish(&Delta {
                provenance: Provenance {
                    commit: Some("c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00".to_owned()),
                    dirty: false,
                    generation: Hash::of(b"words").to_hex(),
                    extractor: "rust.1".to_owned(),
                    indexed_at: 1,
                },
                drop: Vec::new(),
                add,
                redirect: Vec::new(),
                stale: Vec::new(),
                files: files.iter().map(|(path, _)| row(path)).collect(),
                stats: Vec::new(),
            })
            .expect("publish");
        Store::open(dir).expect("store")
    }

    // Review of #53, M8: a name past SQLite's pattern limit failed the
    // whole chain ("LIKE or GLOB pattern too complex").
    #[test]
    fn a_name_of_any_length_is_looked_up() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), &[("a.rs", &["a"])]);
        assert!(store.named(&"a".repeat(50_010)).unwrap().is_empty());
    }

    // Review of #53, S: a token is looked up through the name indexes,
    // never by a walk of the table.
    #[test]
    fn a_name_is_looked_up_through_its_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), &[("a.rs", &["Store::open", "open", "OPEN"])]);
        let mut found: Vec<String> = store
            .named("open")
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        found.sort();
        assert_eq!(found, ["OPEN", "Store::open", "open"]);
        let plan: Vec<String> = {
            let mut stmt = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {NAMED}"))
                .unwrap();
            stmt.query_map(["open"], |row| row.get::<_, String>(3))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert!(
            plan.iter().any(|p| p.contains("USING INDEX symbols_name"))
                && plan.iter().any(|p| p.contains("USING INDEX symbols_leaf"))
                && !plan.iter().any(|p| p.starts_with("SCAN symbols")),
            "{plan:?}"
        );
    }
}
