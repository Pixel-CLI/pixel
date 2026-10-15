// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Reading a published generation. Nothing here writes the index, only
//! its stamp (`checked`); an index that is missing, damaged or of another
//! schema is an error, never served (C-19).

use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use mvp_model::hash::Hash;
use mvp_model::{Corpus, Edge, FileRow, FileStat, Graph, GraphError, Posting, Provenance, Symbol};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

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

/// Counts of a generation, for `status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stats {
    pub files: u64,
    pub symbols: u64,
    pub edges: u64,
    /// Sites the rules could not resolve (C-10): kept, counted, never served.
    pub unresolved: u64,
    /// Files the last run's deadline left stale (`Delta::stale`): a
    /// changed one keeps its previous rows; a new one, or one another
    /// extractor version read, is absent, and no site of its language is
    /// resolved.
    pub stale: Vec<String>,
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
            checked::record(root, &before);
        }
        Ok(Store { conn, published })
    }

    pub fn stats(&self) -> Result<Stats, StoreError> {
        stats(&self.conn, self.published.stale.clone())
    }

    /// The files the generation's run left stale, as published: what
    /// [`Store::stats`] holds beside its counts, without counting every
    /// row (about 60 ms for the 970k edges of `antiwork/gumroad`).
    pub fn stale(&self) -> &[String] {
        &self.published.stale
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

/// The counts of the generation `conn` reads, with its stale list.
pub(crate) fn stats(conn: &Connection, stale: Vec<String>) -> Result<Stats, StoreError> {
    let count = |sql: &str| -> Result<u64, StoreError> {
        Ok(conn.query_row(sql, [], |r| r.get::<_, i64>(0))? as u64)
    };
    Ok(Stats {
        files: count("SELECT count(*) FROM files")?,
        symbols: count("SELECT count(*) FROM symbols")?,
        edges: count("SELECT count(*) FROM edges")?,
        unresolved: count("SELECT count(*) FROM edges WHERE resolved = 0")?,
        stale,
    })
}

impl Graph for Store {
    fn provenance(&self) -> Result<Provenance, GraphError> {
        Ok(self.published.provenance.clone())
    }

    fn files(&self) -> Result<Vec<FileRow>, GraphError> {
        rows::files(&self.conn).map_err(graph_error)
    }

    fn languages(&self) -> Result<std::collections::BTreeSet<String>, GraphError> {
        rows::languages(&self.conn).map_err(graph_error)
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

    fn callers(&self, uid: &str) -> Result<Vec<Edge>, GraphError> {
        self.edges(
            &format!("{EDGES} WHERE e.dst = ?1 AND e.resolved = 1 ORDER BY s.uid, e.line"),
            [uid],
        )
        .map_err(graph_error)
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

    fn postings(&self, term: &str) -> Result<Vec<Posting>, GraphError> {
        let read = || -> rusqlite::Result<Vec<Posting>> {
            let mut stmt = self.conn.prepare_cached(
                "SELECT p.file, p.count, d.length FROM postings p JOIN documents d ON d.file = p.file
                 WHERE p.term = ?1 ORDER BY p.file",
            )?;
            let rows = stmt.query_map([term], |row| {
                let number = |at: usize| -> rusqlite::Result<u32> {
                    let value: i64 = row.get(at)?;
                    u32::try_from(value)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(at, value))
                };
                Ok(Posting {
                    file: row.get(0)?,
                    count: number(1)?,
                    length: number(2)?,
                })
            })?;
            rows.collect()
        };
        read().map_err(|err| graph_error(err.into()))
    }

    fn corpus(&self) -> Result<Corpus, GraphError> {
        self.conn
            .prepare_cached("SELECT count(*), coalesce(avg(length), 0.0) FROM documents")
            .and_then(|mut stmt| {
                stmt.query_row([], |row| {
                    let documents: i64 = row.get(0)?;
                    Ok(Corpus {
                        documents: usize::try_from(documents).unwrap_or(0),
                        mean_length: row.get(1)?,
                    })
                })
            })
            .map_err(|err| graph_error(err.into()))
    }

    fn files_with_word(&self, word: &str) -> Result<usize, GraphError> {
        let word = word.to_lowercase();
        self.conn
            .prepare_cached("SELECT files FROM words WHERE word = ?1")
            .and_then(|mut stmt| stmt.query_row([word], crate::words::frequency).optional())
            .map(|count| count.unwrap_or(0))
            .map_err(|err| graph_error(err.into()))
    }

    fn callees(&self, uid: &str) -> Result<Vec<Edge>, GraphError> {
        self.edges(
            &format!(
                "{EDGES} WHERE e.src = (SELECT id FROM symbols WHERE uid = ?1) AND e.resolved = 1
                 ORDER BY e.line, e.dst"
            ),
            [uid],
        )
        .map_err(graph_error)
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
    use mvp_model::{Delta, FileRow, FileRows, Lang, SymbolKind, sub_words};

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

    // Review of #53, S: a word's document frequency read every file row and
    // scanned the symbols once per word. It is now counted in one pass;
    // the count is the one `sub_words` gives, ASCII or not.
    #[test]
    fn a_words_document_frequency_is_counted_on_its_sub_words() {
        let dir = tempfile::tempdir().unwrap();
        let names: &[(&str, &[&str])] = &[
            ("lib/a.rs", &["Ünïcode_Name", "x"]),
            ("src/Parser.rs", &["Parser::parse", "ÜberParser::écrire"]),
            (
                "src/http_server.rs",
                &["HTTPServer::serve", "parse_json2Value"],
            ),
            ("src/json.rs", &["to_json", "Value2Json"]),
        ];
        let store = store(dir.path(), names);
        for word in [
            "http",
            "server",
            "serve",
            "parse",
            "json",
            "json2",
            "value",
            "parser",
            "über",
            "écrire",
            "ünïcode",
            "name",
            "src",
            "rs",
            "2",
            "x",
            "absent",
        ] {
            let expected = names
                .iter()
                .filter(|(path, symbols)| {
                    sub_words(path).contains(&word.to_owned())
                        || symbols
                            .iter()
                            .any(|s| sub_words(s).contains(&word.to_owned()))
                })
                .count();
            assert_eq!(store.files_with_word(word).unwrap(), expected, "{word}");
        }
        assert_eq!(store.files_with_word("JSON").unwrap(), 1, "ignoring case");
        assert_eq!(store.files_with_word("Parse").unwrap(), 2);
    }

    // Review of #60, round 3, m3: a plugin's `languages` are read from the
    // caller's store in one query, the set its files give.
    #[test]
    fn the_languages_are_those_of_the_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), &[("a.rs", &["a"]), ("b.rs", &["b"])]);
        let from_files: std::collections::BTreeSet<String> = store
            .files()
            .unwrap()
            .iter()
            .map(|file| file.lang.as_str().to_owned())
            .collect();
        assert_eq!(store.languages().unwrap(), from_files);
        assert_eq!(from_files.len(), 1);
    }

    // Review of #53, M8: a word past SQLite's pattern limit failed the
    // whole chain ("LIKE or GLOB pattern too complex").
    #[test]
    fn a_word_of_any_length_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), &[("a.rs", &["a"])]);
        assert_eq!(store.files_with_word(&"a".repeat(50_010)).unwrap(), 0);
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
