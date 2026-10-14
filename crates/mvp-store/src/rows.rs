// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Reading back what the store wrote. Every value parses into what was
//! written, or the index is `Unusable`: a row is never skipped and a value
//! never guessed (C-19).

use std::collections::{HashMap, HashSet};

use mvp_model::hash::Hash;
use mvp_model::{
    Edge, EdgeKind, FileRow, FileStat, Lang, Provenance, Symbol, SymbolKind, uid_path,
};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, Row};

use crate::StoreError;

/// What `meta` says of the published generation.
pub(crate) struct Published {
    pub provenance: Provenance,
    /// Files the last run's deadline left stale (`Delta::stale`).
    pub stale: Vec<String>,
}

/// A stored value the store did not write.
fn damaged(what: String) -> StoreError {
    StoreError::Unusable(format!("damaged: {what}"))
}

/// The provenance and stale list of the published generation; `None`
/// before the first.
pub(crate) fn published(conn: &Connection) -> Result<Option<Published>, StoreError> {
    let mut stmt = conn.prepare("SELECT key, value FROM meta")?;
    let mut meta: HashMap<String, String> = HashMap::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        match (text(row, 0)?, text(row, 1)?) {
            (Some(key), Some(value)) => meta.insert(key.to_owned(), value.to_owned()),
            _ => {
                let key = shown(row.get_ref(0)?);
                return Err(damaged(format!("meta row {key} cannot be read back")));
            }
        };
    }
    let Some(generation) = meta.remove("generation") else {
        // Before the first generation, nothing at all is stored.
        if meta.is_empty() && empty(conn)? {
            return Ok(None);
        }
        return Err(damaged("meta has no generation".to_owned()));
    };
    let mut value = |key: &str| {
        meta.remove(key)
            .ok_or_else(|| damaged(format!("meta has no {key}")))
    };
    let (commit, dirty, extractor, indexed_at, stale) = (
        value("commit")?,
        value("dirty")?,
        value("extractor")?,
        value("indexed_at")?,
        value("stale")?,
    );
    let bad = |key: &str, value: &str| damaged(format!("meta {key} is {value:?}"));
    if !hex(&generation, 64) {
        return Err(bad("generation", &generation));
    }
    let commit = match commit.len() {
        0 => None,
        // A SHA-1 or a SHA-256 object name, as `git rev-parse` prints it.
        40 | 64 if hex(&commit, commit.len()) => Some(commit),
        _ => return Err(bad("commit", &commit)),
    };
    let dirty = match dirty.as_str() {
        "0" => false,
        "1" => true,
        _ => return Err(bad("dirty", &dirty)),
    };
    if extractor.is_empty() || !extractor.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(bad("extractor", &extractor));
    }
    // Seconds since 1970 as `i64::to_string` writes them: no sign, no
    // leading zero.
    let parsed = indexed_at.parse::<i64>().ok();
    let Some(indexed_at) = parsed.filter(|n| *n >= 0 && n.to_string() == indexed_at) else {
        return Err(bad("indexed_at", &indexed_at));
    };
    let Some(stale) = stale_list(&stale) else {
        return Err(bad("stale", &stale));
    };
    Ok(Some(Published {
        provenance: Provenance {
            commit,
            dirty,
            generation,
            extractor,
            indexed_at,
        },
        stale,
    }))
}

/// Whether `text` is `len` hex digits in lower case, as `Hash::to_hex` and
/// git write them.
fn hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn empty(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT NOT EXISTS (SELECT 1 FROM files) AND NOT EXISTS (SELECT 1 FROM symbols)
           AND NOT EXISTS (SELECT 1 FROM edges)",
        [],
        |r| r.get(0),
    )
}

/// The stale list as `meta` holds it: paths separated by NUL, the one byte
/// no path holds (a newline can be in a path).
pub(crate) fn stale_value(paths: &[String]) -> String {
    paths.join("\0")
}

fn stale_list(value: &str) -> Option<Vec<String>> {
    if value.is_empty() {
        return Some(Vec::new());
    }
    let paths: Vec<String> = value.split('\0').map(str::to_owned).collect();
    paths.iter().all(|p| !p.is_empty()).then_some(paths)
}

/// Every row of `files`, sorted by path.
pub(crate) fn files(conn: &Connection) -> Result<Vec<FileRow>, StoreError> {
    let mut stmt = conn.prepare("SELECT path, hash, lang FROM files ORDER BY path")?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(file_of(row)?);
    }
    Ok(out)
}

/// The hash and stat of every file whose stat the store keeps, by path
/// (ADR-0022 §2).
pub(crate) fn known(conn: &Connection) -> Result<HashMap<String, (Hash, FileStat)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT path, hash, stat_len, stat_mtime, stat_ctime, stat_ino FROM files
         WHERE stat_len IS NOT NULL",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = HashMap::new();
    while let Some(row) = rows.next()? {
        let path: String = row.get(0)?;
        let hash: String = row.get(1)?;
        let Some(hash) = Hash::from_hex(&hash) else {
            return Err(damaged(format!("files row {path:?} has hash {hash:?}")));
        };
        let stat = FileStat {
            len: row.get::<_, i64>(2)? as u64,
            mtime_ns: row.get(3)?,
            ctime_ns: row.get(4)?,
            ino: row.get::<_, i64>(5)? as u64,
        };
        out.insert(path, (hash, stat));
    }
    Ok(out)
}

/// The distinct languages of `files`: one pass over the table, no row
/// kept.
pub(crate) fn languages(
    conn: &Connection,
) -> Result<std::collections::BTreeSet<String>, StoreError> {
    let mut stmt = conn.prepare_cached("SELECT DISTINCT lang FROM files")?;
    let mut rows = stmt.query([])?;
    let mut out = std::collections::BTreeSet::new();
    while let Some(row) = rows.next()? {
        let lang: String = row.get(0)?;
        let Some(lang) = Lang::parse(&lang) else {
            return Err(damaged(format!(
                "a file's language `{lang}` is not one Pixel knows"
            )));
        };
        out.insert(lang.as_str().to_owned());
    }
    Ok(out)
}

/// A `files` row whose first columns are `path, hash, lang`.
pub(crate) fn file_of(row: &Row<'_>) -> Result<FileRow, StoreError> {
    let path: String = row.get(0)?;
    let hash: String = row.get(1)?;
    let lang: String = row.get(2)?;
    match (Hash::from_hex(&hash), Lang::parse(&lang)) {
        (Some(hash), Some(lang)) => Ok(FileRow { path, hash, lang }),
        _ => Err(damaged(format!(
            "files row {path:?} has hash {hash:?} and lang {lang:?}"
        ))),
    }
}

/// A `symbols` row: `uid, file, kind, name, signature, line, end_line`.
pub(crate) fn symbol_of(row: &Row<'_>, file: String) -> Result<Symbol, StoreError> {
    let uid: String = row.get(0)?;
    let kind: String = row.get(2)?;
    let Some(parsed) = SymbolKind::parse(&kind) else {
        return Err(damaged(format!("symbols row {uid:?} has kind {kind:?}")));
    };
    Ok(Symbol {
        uid,
        file,
        kind: parsed,
        name: row.get(3)?,
        signature: row.get(4)?,
        line: row.get(5)?,
        end_line: row.get(6)?,
    })
}

/// An `edges` row: `src, dst, kind, line, target, context`.
pub(crate) fn edge_of(row: &Row<'_>, src: String) -> Result<Edge, StoreError> {
    let kind: String = row.get(2)?;
    let Some(parsed) = EdgeKind::parse(&kind) else {
        return Err(damaged(format!("edges row from {src:?} has kind {kind:?}")));
    };
    Ok(Edge {
        src,
        dst: row.get(1)?,
        kind: parsed,
        line: row.get(3)?,
        target: row.get(4)?,
        context: row.get(5)?,
    })
}

/// The first row of `files`, `symbols` or `edges` the store could not read
/// back, or `None`. Every row is read once and each value checked against
/// what the writer puts there: its type, text that is UTF-8, a hash,
/// language, kind or line it writes, and a symbol or edge that belongs to
/// a file of the index, which a publish would otherwise neither delete
/// nor replace. Done here rather than by a query, which cannot tell UTF-8,
/// or by CHECKs, which SQLite does not parse in a file opened read-only.
pub(crate) fn unreadable(conn: &Connection) -> rusqlite::Result<Option<String>> {
    let mut paths: HashSet<String> = HashSet::new();
    let mut stmt = conn.prepare(
        "SELECT path, hash, lang, indexed_at, facts, stat_len, stat_mtime, stat_ctime, stat_ino
         FROM files",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let path = text(row, 0)?.filter(|path| !path.is_empty());
        let stat: Vec<ValueRef<'_>> = (5..9).map(|i| row.get_ref(i)).collect::<Result<_, _>>()?;
        let readable = path.is_some()
            && text(row, 1)?.is_some_and(|hash| hex(hash, 64))
            && text(row, 2)?.and_then(Lang::parse).is_some()
            && matches!(row.get_ref(3)?, ValueRef::Integer(_))
            && text(row, 4)?.is_some()
            // A stat is whole or absent (ADR-0022 §2).
            && (stat.iter().all(|v| matches!(v, ValueRef::Null))
                || stat.iter().all(|v| matches!(v, ValueRef::Integer(_))));
        match path {
            Some(path) if readable => paths.insert(path.to_owned()),
            _ => return Ok(Some(format!("files row {}", shown(row.get_ref(0)?)))),
        };
    }
    // `<file>#…`, the file written by `uid_path`, of a file the index
    // holds: the extractor's uid.
    let owned = |uid: &str, file: &str| {
        paths.contains(file)
            && uid
                .strip_prefix(&*uid_path(file))
                .is_some_and(|r| r.starts_with('#'))
    };
    let mut uids: HashSet<String> = HashSet::new();
    let mut stmt =
        conn.prepare("SELECT uid, file, kind, name, signature, line, end_line, leaf FROM symbols")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let (uid, file) = (text(row, 0)?, text(row, 1)?);
        let name = text(row, 3)?;
        let readable = uid.zip(file).is_some_and(|(uid, file)| owned(uid, file))
            && text(row, 2)?.and_then(SymbolKind::parse).is_some()
            && name.is_some()
            && text(row, 4)?.is_some()
            && line(row, 5)?
            && line(row, 6)?
            && text(row, 7)? == name.map(mvp_model::leaf);
        match uid {
            Some(uid) if readable => uids.insert(uid.to_owned()),
            _ => return Ok(Some(format!("symbols row {}", shown(row.get_ref(0)?)))),
        };
    }
    let mut stmt =
        conn.prepare("SELECT src, dst, kind, line, resolved, target, context FROM edges")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        // NULL, or the uid of the symbol the site resolved to.
        let resolved = match row.get_ref(1)? {
            ValueRef::Null => Some(false),
            _ => text(row, 1)?.map(|_| true),
        };
        let readable = text(row, 0)?.is_some_and(|src| uids.contains(src))
            && resolved.is_some()
            && text(row, 2)?.and_then(EdgeKind::parse).is_some()
            && line(row, 3)?
            && row.get_ref(4)? == ValueRef::Integer(i64::from(resolved == Some(true)))
            && text(row, 5)?.is_some()
            && text(row, 6)?.is_some();
        if !readable {
            return Ok(Some(format!("edges row from {}", shown(row.get_ref(0)?))));
        }
    }
    if !crate::words::valid(conn)? {
        return Ok(Some("words differ from the generation".to_owned()));
    }
    if !crate::postings::valid(conn)? {
        return Ok(Some("postings differ from the generation".to_owned()));
    }
    Ok(None)
}

/// Column `i` as the store writes text: TEXT, and UTF-8.
fn text<'r>(row: &'r Row<'_>, i: usize) -> rusqlite::Result<Option<&'r str>> {
    Ok(match row.get_ref(i)? {
        ValueRef::Text(bytes) => std::str::from_utf8(bytes).ok(),
        _ => None,
    })
}

/// Whether column `i` is a line as the store writes it: an INTEGER `u32`.
fn line(row: &Row<'_>, i: usize) -> rusqlite::Result<bool> {
    Ok(matches!(row.get_ref(i)?, ValueRef::Integer(n) if u32::try_from(n).is_ok()))
}

/// A value for a message, whatever it holds.
fn shown(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Text(bytes) => format!("{:?}", String::from_utf8_lossy(bytes)),
        other => format!("of type {}", other.data_type()),
    }
}
