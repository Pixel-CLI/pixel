// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The schema of `graph.db` (`design/architecture.md`, "Graph store"), and
//! how a file on disk is recognised as a usable index.

use std::collections::BTreeSet;

use rusqlite::{Connection, ErrorCode};

use crate::rows;

/// Bumped by any change to the tables below or to how a value is written
/// in them; an index with another version is rebuilt, never read (C-19).
/// 2: the stale list is separated by NUL, not by newline.
/// 3: a symbol's uid writes `%` and `#` in its path as `%25` and `%23`.
/// 4: `symbols.leaf` and the name indexes the chain looks tokens up in.
/// 5: `files` keeps the stat each file was hashed under (ADR-0022 §2).
/// 6: `words` holds each sub-word's exact document frequency.
/// 7: `postings` and `documents`, the terms BM25 reads (ADR-0030).
/// 8: `edges.src` is the `symbols.id` of the site's symbol, not its uid:
///    the uid, written once per site and once more in `edges_src`, was
///    300 of the 336 MB of `antiwork/gumroad`'s index.
pub const SCHEMA_VERSION: i64 = 8;

/// The version of the full check of [`shape`], named in every stamp
/// (`checked`): bumped whenever the check gets stricter, so that no build
/// trusts a stamp a laxer check recorded.
pub const CHECK_VERSION: u32 = 2;

/// `PRAGMA application_id`: "PXMV". A SQLite file without it is not ours.
pub const APPLICATION_ID: i64 = 0x5058_4d56;

/// The graph tables of `design/architecture.md`, with three columns the
/// resolver needs to decide a site again without parsing its file:
/// `files.facts`, `edges.target` and `edges.context`, and `symbols.leaf`,
/// the last segment of the name ([`mvp_model::leaf`]), for the chain.
/// An edge names its site's symbol by `symbols.id` (`src`), which every
/// read turns back into the symbol's uid; its target by uid (`dst`), which
/// may name a symbol of another file.
const TABLES: &str = "
CREATE TABLE files (
  path TEXT PRIMARY KEY,
  hash TEXT NOT NULL,
  lang TEXT NOT NULL,
  indexed_at INTEGER NOT NULL,
  facts TEXT NOT NULL,
  stat_len INTEGER,
  stat_mtime INTEGER,
  stat_ctime INTEGER,
  stat_ino INTEGER
) WITHOUT ROWID;
CREATE TABLE symbols (
  id INTEGER PRIMARY KEY,
  uid TEXT NOT NULL UNIQUE,
  file TEXT NOT NULL,
  kind TEXT NOT NULL,
  name TEXT NOT NULL,
  leaf TEXT NOT NULL,
  signature TEXT NOT NULL,
  line INTEGER NOT NULL,
  end_line INTEGER NOT NULL
);
CREATE INDEX symbols_file ON symbols(file);
CREATE INDEX symbols_name ON symbols(name COLLATE NOCASE);
CREATE INDEX symbols_leaf ON symbols(leaf COLLATE NOCASE);
CREATE TABLE edges (
  src INTEGER NOT NULL,
  dst TEXT,
  kind TEXT NOT NULL,
  line INTEGER NOT NULL,
  resolved INTEGER NOT NULL,
  target TEXT NOT NULL,
  context TEXT NOT NULL
);
CREATE INDEX edges_src ON edges(src);
CREATE INDEX edges_dst ON edges(dst) WHERE dst IS NOT NULL;
CREATE TABLE words (
  word TEXT PRIMARY KEY,
  files INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE postings (
  term TEXT NOT NULL,
  file TEXT NOT NULL,
  count INTEGER NOT NULL,
  PRIMARY KEY (term, file)
) WITHOUT ROWID;
CREATE TABLE documents (
  file TEXT PRIMARY KEY,
  length INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
) WITHOUT ROWID;
";

/// What a database file on disk is.
#[derive(Debug, PartialEq, Eq)]
pub enum Shape {
    /// A new, empty database: no schema yet.
    Empty,
    /// Our schema, at our version, every page and row readable.
    Current,
    /// Unusable: another program's file, another schema, or damaged. The
    /// reason says which.
    Unusable(String),
}

/// Reads the header pragmas and the schema, then checks the whole file:
/// `PRAGMA integrity_check` reads every page and finds every table row in
/// each of its indexes, and [`rows::unreadable`] reads every row.
/// `quick_check` would not do: it skips the indexes, and a publish through
/// an index that disagrees with its table deletes too little, silently or
/// until a unique key fails. On the 36.9 MB index of `Pixel-CLI/pixel`,
/// release build, 2026-10-10: 90 ms for `integrity_check` (35 for
/// `quick_check`), 75 ms for the rows. Anything but our schema with both
/// checks clean is `Unusable`, and so is an error on the way.
///
/// `checked` is asked once the header is read, so after the connection's
/// read has begun: when it says the file is a version a full check already
/// passed (`checked::trusted`), the two whole-file checks are skipped
/// (ADR-0022). A write after that read began changes the file's stamp,
/// since a stamp is recorded only once the file system's clock is past
/// every time in it, so it cannot make a damaged file look checked. The
/// writer always answers no.
pub fn shape(conn: &Connection, checked: impl FnOnce() -> bool) -> Shape {
    read_shape(conn, checked).unwrap_or_else(|err| Shape::Unusable(describe(&err)))
}

fn read_shape(conn: &Connection, checked: impl FnOnce() -> bool) -> rusqlite::Result<Shape> {
    let app: i64 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let found = objects(conn)?;
    if app == 0 && version == 0 && found.is_empty() {
        return Ok(Shape::Empty);
    }
    if app != APPLICATION_ID {
        return Ok(Shape::Unusable(format!(
            "not a pixel-mvp index (application id {app:#x})"
        )));
    }
    if version != SCHEMA_VERSION {
        return Ok(Shape::Unusable(format!(
            "schema version {version}, this binary reads {SCHEMA_VERSION}"
        )));
    }
    let mut reference = Connection::open_in_memory()?;
    create(&mut reference)?;
    let expected = objects(&reference)?;
    let differ: BTreeSet<&str> = found
        .symmetric_difference(&expected)
        .map(|object| object.1.as_str())
        .collect();
    if !differ.is_empty() {
        let names: Vec<&str> = differ.into_iter().collect();
        return Ok(Shape::Unusable(format!(
            "the schema differs from this binary's at {}",
            names.join(", ")
        )));
    }
    if checked() {
        return Ok(Shape::Current);
    }
    #[cfg(test)]
    FULL_CHECKS.with(|n| n.set(n.get() + 1));
    let check: String = conn.query_row("PRAGMA integrity_check(1)", [], |r| r.get(0))?;
    if check != "ok" {
        // "*** in database main ***" on a line of its own, then the error.
        let error: Vec<&str> = check.lines().filter(|l| !l.starts_with("***")).collect();
        return Ok(Shape::Unusable(format!("damaged: {}", error.join(" "))));
    }
    if let Some(row) = rows::unreadable(conn)? {
        return Ok(Shape::Unusable(format!(
            "damaged: {row} cannot be read back"
        )));
    }
    Ok(Shape::Current)
}

#[cfg(test)]
thread_local! {
    /// How many full checks ran on this thread.
    pub static FULL_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A row of `sqlite_master`: type, name, table, and the statement that
/// made it.
type Object = (String, String, String, Option<String>);

/// Every table and index with the statement that made it: the exact set,
/// columns included, that `create` makes.
fn objects(conn: &Connection) -> rusqlite::Result<BTreeSet<Object>> {
    let mut stmt = conn.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    rows.collect()
}

/// Creates the schema in an empty database.
pub fn create(conn: &mut Connection) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute_batch(TABLES)?;
    tx.pragma_update(None, "application_id", APPLICATION_ID)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()
}

/// Whether an error means the file is damaged rather than busy or misused:
/// SQLite found a page it cannot read, or a stored value is not of the
/// type the store wrote there, text that is not UTF-8 included. The open
/// check refuses such values; a query meets them only in a file damaged
/// after it.
pub fn is_corruption(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    ) || matches!(
        err,
        rusqlite::Error::IntegralValueOutOfRange(..)
            | rusqlite::Error::InvalidColumnType(..)
            | rusqlite::Error::Utf8Error(..)
    )
}

pub fn describe(err: &rusqlite::Error) -> String {
    if is_corruption(err) {
        format!("corrupt: {err}")
    } else {
        err.to_string()
    }
}
