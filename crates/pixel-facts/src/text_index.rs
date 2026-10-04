// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `text_index.rs` — candidate lookup in the trigram FTS5 indexes
//! (`diff_fts` over hunk text, `path_fts` over changed paths).
//!
//! A query unit matches a row when the row holds every trigram of the unit;
//! a query matches when at least one of its units does. The result is a
//! candidate superset (trigrams present, not necessarily adjacent), so every
//! caller verifies the row's text before reporting it.

use rusqlite::Connection;

use crate::store::Result;

/// Most candidate rows one lookup returns. Candidates come newest commit
/// first, so the cap drops the oldest ones.
pub const CANDIDATE_CAP: usize = 10_000;

/// The FTS5 `MATCH` expression for `units`: each unit of three characters
/// or more becomes the AND of its distinct trigrams, and the units are
/// OR-ed. `None` when no unit is long enough to have a trigram.
///
/// Each trigram is an FTS5 string, so the table's tokenizer folds its case
/// exactly as it folded the indexed text.
pub fn trigram_match(units: &[String]) -> Option<String> {
    let mut clauses: Vec<String> = Vec::new();
    for unit in units {
        let chars: Vec<char> = unit.chars().collect();
        let mut grams: Vec<String> = chars
            .windows(3)
            .map(|w| w.iter().collect::<String>())
            .collect();
        if grams.is_empty() {
            continue;
        }
        grams.sort_unstable();
        grams.dedup();
        let terms: Vec<String> = grams
            .iter()
            .map(|g| format!("\"{}\"", g.replace('"', "\"\"")))
            .collect();
        clauses.push(format!("({})", terms.join(" AND ")));
    }
    if clauses.is_empty() {
        None
    } else {
        Some(clauses.join(" OR "))
    }
}

/// `hunks.id` of the hunks whose text may contain one of `units`, newest
/// commit first, at most `cap` of them. `None` when no unit has a trigram
/// (the caller falls back to a scan or answers nothing).
pub fn matching_hunks(conn: &Connection, units: &[String], cap: usize) -> Result<Option<Vec<i64>>> {
    candidates(
        conn,
        units,
        cap,
        "SELECT h.id FROM diff_fts
         JOIN hunks h ON h.id = diff_fts.rowid
         JOIN commits c ON c.id = h.commit_id
         WHERE diff_fts MATCH ?1
         ORDER BY c.committed_at DESC, h.id DESC
         LIMIT ?2",
    )
}

/// `file_changes.id` of the changes whose path may contain one of `units`,
/// newest commit first, at most `cap` of them. `None` when no unit has a
/// trigram.
pub fn matching_changes(
    conn: &Connection,
    units: &[String],
    cap: usize,
) -> Result<Option<Vec<i64>>> {
    candidates(
        conn,
        units,
        cap,
        "SELECT f.id FROM path_fts
         JOIN file_changes f ON f.id = path_fts.rowid
         JOIN commits c ON c.id = f.commit_id
         WHERE path_fts MATCH ?1
         ORDER BY c.committed_at DESC, f.id DESC
         LIMIT ?2",
    )
}

fn candidates(
    conn: &Connection,
    units: &[String],
    cap: usize,
    sql: &str,
) -> Result<Option<Vec<i64>>> {
    let Some(expr) = trigram_match(units) else {
        return Ok(None);
    };
    // SQLite reads a negative LIMIT as "no limit".
    let limit = i64::try_from(cap).unwrap_or(-1);
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(rusqlite::params![expr, limit], |r| r.get::<_, i64>(0))?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row?);
    }
    Ok(Some(ids))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FactsStore;
    use crate::ingest::tests::ingest_within;
    use crate::testutil::{commit_at, days_ago, init_repo};

    fn units(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn trigram_match_ands_the_trigrams_of_a_unit_and_ors_the_units() {
        assert_eq!(
            trigram_match(&units(&["ab"])),
            None,
            "two chars: no trigram"
        );
        assert_eq!(trigram_match(&units(&[])), None);
        assert_eq!(
            trigram_match(&units(&["abc"])).as_deref(),
            Some(r#"("abc")"#)
        );
        assert_eq!(
            trigram_match(&units(&["abcd"])).as_deref(),
            Some(r#"("abc" AND "bcd")"#)
        );
        assert_eq!(
            trigram_match(&units(&["xy", "abc", "defg"])).as_deref(),
            Some(r#"("abc") OR ("def" AND "efg")"#),
            "a unit too short is dropped, the others are OR-ed"
        );
        assert_eq!(
            trigram_match(&units(&["aaaaa"])).as_deref(),
            Some(r#"("aaa")"#),
            "a repeated trigram is asked once"
        );
        assert_eq!(
            trigram_match(&units(&[r#"a"bc"#])).as_deref(),
            Some(r#"("""bc" AND "a""b")"#),
            "a double quote is doubled inside the FTS5 string"
        );
        assert_eq!(
            trigram_match(&units(&["é_ü"])).as_deref(),
            Some(r#"("é_ü")"#),
            "trigrams are characters, not bytes, like the tokenizer's"
        );
    }

    /// One repo, three commits a day apart: `alpha_beta` (oldest),
    /// `alpha` alone with `beta` elsewhere in the same file, `alpha_beta`
    /// again (newest).
    fn three_commits() -> (tempfile::TempDir, FactsStore) {
        let dir = init_repo();
        let root = dir.path();
        commit_at(root, &[("a.txt", b"alpha_beta\n")], "one", days_ago(3));
        commit_at(root, &[("b.txt", b"alpha\nbeta\n")], "two", days_ago(2));
        commit_at(
            root,
            &[("docs/c.txt", b"alpha_beta again\n")],
            "three",
            days_ago(1),
        );
        let mut store = FactsStore::open(root).unwrap();
        ingest_within(&mut store);
        (dir, store)
    }

    fn hunk_paths(store: &FactsStore, ids: &[i64]) -> Vec<String> {
        ids.iter()
            .map(|id| {
                store
                    .conn()
                    .query_row("SELECT path FROM hunks WHERE id = ?1", [id], |r| r.get(0))
                    .unwrap()
            })
            .collect()
    }

    /// Every trigram of the unit must be present: `b.txt` holds `alpha`
    /// and `beta` but not the `a_b` trigram, so it is no candidate. The
    /// others come newest commit first.
    #[test]
    fn matching_hunks_requires_every_trigram_and_orders_newest_first() {
        let (_dir, store) = three_commits();
        let ids = matching_hunks(store.conn(), &units(&["ALPHA_beta"]), CANDIDATE_CAP)
            .unwrap()
            .unwrap();
        assert_eq!(hunk_paths(&store, &ids), vec!["docs/c.txt", "a.txt"]);
        let capped = matching_hunks(store.conn(), &units(&["alpha_beta"]), 1)
            .unwrap()
            .unwrap();
        assert_eq!(
            hunk_paths(&store, &capped),
            vec!["docs/c.txt"],
            "cap keeps the newest"
        );
        assert_eq!(
            matching_hunks(store.conn(), &units(&["zz"]), CANDIDATE_CAP).unwrap(),
            None
        );
        assert_eq!(
            matching_hunks(store.conn(), &units(&["gamma"]), CANDIDATE_CAP).unwrap(),
            Some(vec![])
        );
    }

    #[test]
    fn matching_changes_finds_paths_by_every_trigram_newest_first() {
        let (_dir, store) = three_commits();
        let paths = |ids: Vec<i64>| -> Vec<String> {
            ids.iter()
                .map(|id| {
                    store
                        .conn()
                        .query_row("SELECT path FROM file_changes WHERE id = ?1", [id], |r| {
                            r.get(0)
                        })
                        .unwrap()
                })
                .collect()
        };
        let txt = matching_changes(store.conn(), &units(&[".txt"]), CANDIDATE_CAP)
            .unwrap()
            .unwrap();
        assert_eq!(paths(txt), vec!["docs/c.txt", "b.txt", "a.txt"]);
        let docs = matching_changes(store.conn(), &units(&["DOCS/"]), CANDIDATE_CAP)
            .unwrap()
            .unwrap();
        assert_eq!(paths(docs), vec!["docs/c.txt"]);
        assert_eq!(
            matching_changes(store.conn(), &units(&["a"]), CANDIDATE_CAP).unwrap(),
            None
        );
    }
}
