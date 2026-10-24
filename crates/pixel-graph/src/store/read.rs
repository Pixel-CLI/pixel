// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Reading a published generation. Nothing here writes the index, only
//! its stamp (`checked`); an index that is missing, damaged or of another
//! schema is an error, never served (C-19).

use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use pixel_model::hash::Hash;
use pixel_model::{Edge, FileRow, FileStat, Graph, GraphError, Provenance, Symbol, Tier};
use rusqlite::{Connection, OpenFlags};

use crate::store::checked::{self, Stamp};
use crate::store::rows::{self, Published, edge_of, symbol_of};
use crate::store::schema::{self, Shape};
use crate::store::{StoreError, db_path, existing};

/// A site that reaches a symbol, and the symbol that holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    pub edge: Edge,
    pub site: Symbol,
}

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
        if !crate::store::own(root)? || !existing(&path)? {
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

    /// Every indexed file with its content hash, sorted by path.
    pub fn file_rows(&self) -> Result<Vec<FileRow>, StoreError> {
        rows::files(&self.conn)
    }

    /// The symbols of one file, in line order.
    pub fn symbols_in(&self, file: &str) -> Result<Vec<Symbol>, StoreError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT uid, file, kind, name, signature, line, end_line, shape FROM symbols
             WHERE file = ?1 ORDER BY line, uid",
        )?;
        let mut rows = stmt.query([file])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(symbol_of(row, row.get(1)?)?);
        }
        Ok(out)
    }

    /// Every symbol of the generation, by file then line: what a read of the
    /// whole repository (the map, the word leads) starts from.
    pub fn all_symbols(&self) -> Result<Vec<Symbol>, StoreError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT uid, file, kind, name, signature, line, end_line, shape FROM symbols
             ORDER BY file, line, uid",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(symbol_of(row, row.get(1)?)?);
        }
        Ok(out)
    }

    /// The symbols a token names, by qualified name or by last segment,
    /// ignoring ASCII case, sorted by uid.
    pub fn named_symbols(&self, token: &str) -> Result<Vec<Symbol>, StoreError> {
        let mut stmt = self.conn.prepare_cached(NAMED)?;
        let mut rows = stmt.query([token])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(symbol_of(row, row.get(1)?)?);
        }
        Ok(out)
    }

    /// The symbol with this uid.
    pub fn symbol(&self, uid: &str) -> Result<Option<Symbol>, StoreError> {
        let mut stmt = self.conn.prepare_cached(SYMBOL)?;
        let mut rows = stmt.query([uid])?;
        match rows.next()? {
            Some(row) => Ok(Some(symbol_of(row, row.get(1)?)?)),
            None => Ok(None),
        }
    }

    /// The declaration header stored for `uid` (`symbols.shape`).
    pub fn signature_of(&self, uid: &str) -> Result<Option<String>, StoreError> {
        Ok(self.symbol(uid)?.map(|symbol| symbol.shape))
    }

    /// The sites that call or name `uid`, at `min_tier` or stronger
    /// (`Proved` only, or `Likely` too), by file then line; each with the
    /// symbol that holds the site. Imports are not callers.
    pub fn callers(&self, uid: &str, min_tier: Tier) -> Result<Vec<Caller>, StoreError> {
        let edges = self.edges(
            &format!(
                "{EDGES} WHERE e.dst = ?1 AND e.tier <= ?2 AND e.kind != 'import'
                 ORDER BY s.file, e.line, s.uid"
            ),
            rusqlite::params![uid, min_tier.as_i64()],
        )?;
        let mut out = Vec::with_capacity(edges.len());
        for edge in edges {
            // The site's symbol exists: the edge was read by joining it.
            if let Some(site) = self.symbol(&edge.src)? {
                out.push(Caller { edge, site });
            }
        }
        Ok(out)
    }

    /// The import sites that resolved to `uid` itself (a language that
    /// resolves a `use` to the item it names), by file then line.
    pub fn imports_of(&self, uid: &str) -> Result<Vec<Edge>, StoreError> {
        self.edges(
            &format!(
                "{EDGES} WHERE e.dst = ?1 AND e.kind = 'import' ORDER BY s.file, e.line, s.uid"
            ),
            [uid],
        )
    }

    /// The sites inside `uid` that resolved to a symbol, by line.
    pub fn callees(&self, uid: &str) -> Result<Vec<Edge>, StoreError> {
        self.edges(
            &format!(
                "{EDGES} WHERE s.uid = ?1 AND e.dst IS NOT NULL AND e.kind != 'import'
                 ORDER BY e.line, e.rowid"
            ),
            [uid],
        )
    }

    /// The sites no rule could resolve whose callee is named `leaf`
    /// (ignoring case): what a caller count of that name is only a lower
    /// bound of.
    pub fn open_sites(&self, leaf: &str) -> Result<Vec<Edge>, StoreError> {
        self.edges(
            &format!(
                "{EDGES} WHERE e.tier = 2 AND e.kind IN ('call', 'reference')
                 AND e.leaf = ?1 COLLATE NOCASE ORDER BY s.file, e.line, s.uid"
            ),
            [leaf],
        )
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
        self.file_rows().map_err(graph_error)
    }

    fn symbols(&self, file: &str) -> Result<Vec<Symbol>, GraphError> {
        self.symbols_in(file).map_err(graph_error)
    }

    fn named(&self, token: &str) -> Result<Vec<Symbol>, GraphError> {
        self.named_symbols(token).map_err(graph_error)
    }
}

/// An edge as [`edge_of`] reads it, with its site's symbol's uid for
/// `edges.src`, which holds that symbol's id.
const EDGES: &str = "SELECT s.uid, e.dst, e.kind, e.line, e.target, e.context, e.tier
     FROM edges e JOIN symbols s ON s.id = e.src";

const SYMBOL: &str = "SELECT uid, file, kind, name, signature, line, end_line, shape FROM symbols
     WHERE uid = ?1";

/// The symbols a token names, by name or by leaf: two searches, one per
/// name index, rather than one `OR` the planner may answer by walking the
/// uid index in order.
const NAMED: &str = "SELECT uid, file, kind, name, signature, line, end_line, shape FROM symbols
     WHERE name = ?1 COLLATE NOCASE
     UNION
     SELECT uid, file, kind, name, signature, line, end_line, shape FROM symbols
     WHERE leaf = ?1 COLLATE NOCASE
     ORDER BY uid";

fn graph_error(err: StoreError) -> GraphError {
    GraphError(err.to_string())
}

#[cfg(test)]
mod tests {
    use pixel_model::hash::Hash;
    use pixel_model::{Delta, EdgeKind, FileRow, FileRows, Lang, SymbolKind, Tier};

    use super::*;
    use crate::store::Writer;

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
                        shape: format!("fn {name}()"),
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

    #[test]
    fn every_symbol_is_read_by_file_then_line() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), &[("a.rs", &["x1", "x2"]), ("b.rs", &["y"])]);
        let all: Vec<(String, u32)> = store
            .all_symbols()
            .unwrap()
            .into_iter()
            .map(|symbol| (symbol.uid, symbol.line))
            .collect();
        assert_eq!(
            all,
            [
                ("a.rs#x1#0".to_owned(), 1),
                ("a.rs#x2#1".to_owned(), 2),
                ("b.rs#y#0".to_owned(), 1),
            ]
        );
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

    /// `a.rs` defines `run` and `helper`; `b.rs`'s `go` calls `run` provably,
    /// `c.rs`'s `try_it` calls it as a guess, `d.rs`'s `lost` names `run`
    /// where no rule could say which, and `go` imports `a.rs`.
    fn linked(dir: &Path) -> Store {
        let symbol = |file: &str, name: &str, kind: SymbolKind, line: u32| Symbol {
            uid: format!("{file}#{name}#{kind}"),
            file: file.to_owned(),
            kind,
            name: name.to_owned(),
            signature: format!("fn {name}(a: u8)"),
            shape: format!("fn {name}(a: u8)"),
            line,
            end_line: line + 1,
        };
        let edge =
            |src: &Symbol, dst: Option<&Symbol>, kind: EdgeKind, tier: Tier, line: u32| Edge {
                src: src.uid.clone(),
                dst: dst.map(|d| d.uid.clone()),
                kind,
                tier,
                line,
                target: "run".to_owned(),
                context: String::new(),
            };
        let file_of = |path: &str| symbol(path, path, SymbolKind::File, 1);
        let run = symbol("a.rs", "run", SymbolKind::Function, 1);
        let helper = symbol("a.rs", "helper", SymbolKind::Function, 4);
        let go = symbol("b.rs", "go", SymbolKind::Function, 3);
        let try_it = symbol("c.rs", "try_it", SymbolKind::Function, 2);
        let lost = symbol("d.rs", "lost", SymbolKind::Function, 2);
        let a = file_of("a.rs");
        let b = file_of("b.rs");
        let files = [
            ("a.rs", vec![a.clone(), run.clone(), helper.clone()], vec![]),
            (
                "b.rs",
                vec![b.clone(), go.clone()],
                vec![
                    edge(&go, Some(&run), EdgeKind::Call, Tier::Proved, 4),
                    edge(&go, Some(&helper), EdgeKind::Call, Tier::Proved, 5),
                    edge(&b, Some(&a), EdgeKind::Import, Tier::Proved, 1),
                ],
            ),
            (
                "c.rs",
                vec![file_of("c.rs"), try_it.clone()],
                vec![edge(&try_it, Some(&run), EdgeKind::Call, Tier::Likely, 3)],
            ),
            (
                "d.rs",
                vec![file_of("d.rs"), lost.clone()],
                vec![edge(&lost, None, EdgeKind::Call, Tier::Open, 3)],
            ),
        ];
        let row = |path: &str| FileRow {
            path: path.to_owned(),
            lang: Lang::Rust,
            hash: Hash::of(path.as_bytes()),
        };
        let add = files
            .iter()
            .map(|(path, symbols, edges)| FileRows {
                file: row(path),
                facts: String::new(),
                symbols: symbols.clone(),
                edges: edges.clone(),
            })
            .collect();
        let mut writer = Writer::open(dir, None).expect("writer");
        writer
            .publish(&Delta {
                provenance: Provenance {
                    commit: None,
                    dirty: true,
                    generation: Hash::of(b"linked").to_hex(),
                    extractor: "rust.1".to_owned(),
                    indexed_at: 1,
                },
                drop: Vec::new(),
                add,
                redirect: Vec::new(),
                stale: Vec::new(),
                files: files.iter().map(|(path, ..)| row(path)).collect(),
                stats: Vec::new(),
            })
            .expect("publish");
        Store::open(dir).expect("store")
    }

    #[test]
    fn callers_are_the_sites_at_the_asked_tier_and_stronger() {
        let dir = tempfile::tempdir().unwrap();
        let store = linked(dir.path());
        let run = "a.rs#run#function";
        let held = |tier| -> Vec<(String, u32, Tier)> {
            store
                .callers(run, tier)
                .unwrap()
                .into_iter()
                .map(|c| (c.site.name, c.edge.line, c.edge.tier))
                .collect()
        };
        assert_eq!(held(Tier::Proved), [("go".to_owned(), 4, Tier::Proved)]);
        assert_eq!(
            held(Tier::Likely),
            [
                ("go".to_owned(), 4, Tier::Proved),
                ("try_it".to_owned(), 3, Tier::Likely)
            ]
        );
        // The import of the file is nobody's call of `run`.
        assert!(
            store
                .callers("a.rs#a.rs#file", Tier::Likely)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn callees_are_the_resolved_sites_inside_a_symbol_and_open_sites_are_found_by_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let store = linked(dir.path());
        let callees: Vec<(String, u32)> = store
            .callees("b.rs#go#function")
            .unwrap()
            .into_iter()
            .map(|e| (e.dst.unwrap(), e.line))
            .collect();
        assert_eq!(
            callees,
            [
                ("a.rs#run#function".to_owned(), 4),
                ("a.rs#helper#function".to_owned(), 5)
            ]
        );
        let open: Vec<String> = store
            .open_sites("RUN")
            .unwrap()
            .into_iter()
            .map(|e| e.src)
            .collect();
        assert_eq!(open, ["d.rs#lost#function"]);
        assert!(store.open_sites("helper").unwrap().is_empty());
    }

    #[test]
    fn a_symbol_and_its_shape_are_read_by_uid() {
        let dir = tempfile::tempdir().unwrap();
        let store = linked(dir.path());
        let run = store.symbol("a.rs#run#function").unwrap().unwrap();
        assert_eq!((run.file.as_str(), run.line), ("a.rs", 1));
        assert_eq!(
            store.signature_of("a.rs#run#function").unwrap().as_deref(),
            Some("fn run(a: u8)")
        );
        assert_eq!(store.symbol("gone#x#function").unwrap(), None);
        assert_eq!(store.signature_of("gone#x#function").unwrap(), None);
    }
}
