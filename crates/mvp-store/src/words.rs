// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Generation-bound document frequencies: counted at publication, checked
//! against the graph by every full C-19 check, read by primary key on the
//! prompt path. No sidecar can outlive the generation it describes.

use rusqlite::{Connection, params};
use std::collections::HashMap;

pub(crate) fn frequency(row: &rusqlite::Row<'_>) -> rusqlite::Result<usize> {
    count(row, 0)
}

fn count(row: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<usize> {
    let value: i64 = row.get(at)?;
    usize::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(at, value))
}

/// Counts each file once per word, across its path and all symbol names.
fn counts(conn: &Connection) -> rusqlite::Result<HashMap<String, usize>> {
    let mut ids = HashMap::new();
    let mut holders = Holders::default();
    let mut stmt = conn.prepare("SELECT path FROM files")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let path: String = row.get(0)?;
        let id = ids.len();
        holders.note(&path, id);
        ids.insert(path, id);
    }
    let mut stmt = conn.prepare("SELECT file, name FROM symbols")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let file: String = row.get(0)?;
        let name: String = row.get(1)?;
        let next = ids.len();
        let id = *ids.entry(file).or_insert(next);
        holders.note(&name, id);
    }
    Ok(holders.counts())
}

/// Called inside the graph publication transaction, after applying its
/// additions and removals. Kept and stale rows count exactly as before.
pub(crate) fn publish(conn: &Connection) -> rusqlite::Result<()> {
    let counts = counts(conn)?;
    conn.execute("DELETE FROM words", [])?;
    let mut insert = conn.prepare("INSERT INTO words (word, files) VALUES (?1, ?2)")?;
    for (word, files) in counts {
        insert.execute(params![word, files as i64])?;
    }
    Ok(())
}

/// Valid types and exact contents: missing, extra and plausible but wrong
/// counts are corruption too, even though integrity_check accepts them.
pub(crate) fn valid(conn: &Connection) -> rusqlite::Result<bool> {
    let mut expected = counts(conn)?;
    let mut stmt = conn.prepare("SELECT word, files FROM words")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let word: String = row.get(0)?;
        let files = count(row, 1)?;
        if expected.remove(&word) != Some(files) {
            return Ok(false);
        }
    }
    Ok(expected.is_empty())
}

/// The files that hold each sub-word, as ids, while names are read.
#[derive(Default)]
struct Holders {
    files: HashMap<String, Vec<usize>>,
    word: String,
}

impl Holders {
    /// Notes that file `id` holds the sub-words of `text`.
    fn note(&mut self, text: &str, id: usize) {
        if !text.is_ascii() {
            for word in mvp_model::sub_words(text) {
                self.word = word;
                self.hold(id);
            }
            return;
        }
        // `sub_words` on ASCII, without a string per word.
        let bytes = text.as_bytes();
        self.word.clear();
        for (i, &c) in bytes.iter().enumerate() {
            if !c.is_ascii_alphanumeric() {
                if !self.word.is_empty() {
                    self.hold(id);
                }
                continue;
            }
            let prev = i.checked_sub(1).map(|j| bytes[j]);
            let next = bytes.get(i + 1);
            let boundary = c.is_ascii_uppercase()
                && prev.is_some_and(|p| {
                    p.is_ascii_lowercase()
                        || p.is_ascii_digit()
                        || (p.is_ascii_uppercase() && next.is_some_and(u8::is_ascii_lowercase))
                });
            if boundary && !self.word.is_empty() {
                self.hold(id);
            }
            self.word.push(c.to_ascii_lowercase() as char);
        }
        if !self.word.is_empty() {
            self.hold(id);
        }
    }

    /// Records the current word for file `id`, and clears it.
    fn hold(&mut self, id: usize) {
        match self.files.get_mut(self.word.as_str()) {
            Some(files) if files.last() == Some(&id) => {}
            Some(files) => files.push(id),
            None => {
                self.files.insert(self.word.clone(), vec![id]);
            }
        }
        self.word.clear();
    }

    /// Each word's number of files.
    fn counts(self) -> HashMap<String, usize> {
        self.files
            .into_iter()
            .map(|(word, mut files)| {
                files.sort_unstable();
                files.dedup();
                (word, files.len())
            })
            .collect()
    }
}
