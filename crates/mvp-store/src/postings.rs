// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Generation-bound postings for the selector's BM25 (ADR-0030): each
//! file's document is its path and its symbols' names and signatures, as
//! [`mvp_model::terms`] splits them. Counted at publication, checked
//! against the graph by every full C-19 check, read by term on the prompt
//! path, like `words`.

use std::collections::{BTreeMap, HashMap};

use rusqlite::{Connection, params};

/// Each file's terms with their counts, from the generation's rows.
fn documents(conn: &Connection) -> rusqlite::Result<BTreeMap<String, HashMap<String, u32>>> {
    let mut docs: BTreeMap<String, HashMap<String, u32>> = BTreeMap::new();
    let mut note = |file: &str, text: &str| {
        let doc = docs.entry(file.to_owned()).or_default();
        for term in mvp_model::terms(text) {
            *doc.entry(term).or_insert(0) += 1;
        }
    };
    let mut stmt = conn.prepare("SELECT path FROM files")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let path: String = row.get(0)?;
        note(&path, &path);
    }
    let mut stmt = conn.prepare("SELECT file, name, signature FROM symbols")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let file: String = row.get(0)?;
        let name: String = row.get(1)?;
        let signature: String = row.get(2)?;
        note(&file, &name);
        note(&file, &signature);
    }
    docs.retain(|_, doc| !doc.is_empty());
    Ok(docs)
}

/// Called inside the publication transaction, after the generation's rows
/// are in place: the postings and document lengths of every file.
pub(crate) fn publish(conn: &Connection) -> rusqlite::Result<()> {
    let docs = documents(conn)?;
    conn.execute("DELETE FROM postings", [])?;
    conn.execute("DELETE FROM documents", [])?;
    let mut posting =
        conn.prepare("INSERT INTO postings (term, file, count) VALUES (?1, ?2, ?3)")?;
    let mut document = conn.prepare("INSERT INTO documents (file, length) VALUES (?1, ?2)")?;
    for (file, doc) in &docs {
        document.execute(params![
            file,
            doc.values().map(|&c| i64::from(c)).sum::<i64>()
        ])?;
        for (term, &count) in doc {
            posting.execute(params![term, file, i64::from(count)])?;
        }
    }
    Ok(())
}

/// Exact contents: a missing, extra or wrong posting or length is damage,
/// though `integrity_check` accepts it.
pub(crate) fn valid(conn: &Connection) -> rusqlite::Result<bool> {
    let docs = documents(conn)?;
    let mut expected: HashMap<(String, String), i64> = HashMap::new();
    let mut lengths: HashMap<String, i64> = HashMap::new();
    for (file, doc) in docs {
        lengths.insert(file.clone(), doc.values().map(|&c| i64::from(c)).sum());
        for (term, count) in doc {
            expected.insert((term, file.clone()), i64::from(count));
        }
    }
    let mut stmt = conn.prepare("SELECT term, file, count FROM postings")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let key: (String, String) = (row.get(0)?, row.get(1)?);
        let count: i64 = row.get(2)?;
        if expected.remove(&key) != Some(count) {
            return Ok(false);
        }
    }
    let mut stmt = conn.prepare("SELECT file, length FROM documents")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let file: String = row.get(0)?;
        let length: i64 = row.get(1)?;
        if lengths.remove(&file) != Some(length) {
            return Ok(false);
        }
    }
    Ok(expected.is_empty() && lengths.is_empty())
}
