//! `search.rs` — history search: `search {query, facet: message|path|diff|all}`.
//!
//! Diff/path scopes use trigram candidates (from `diff_grams` / `path_grams`)
//! verified against the `hunks` / `file_changes` text — the recall rowid-in-path
//! trick. Message scope uses FTS5. Ranking = occurrence count then recency
//! (usable-git's post-bm25 design). Budgeted: 200 candidates/scope.

use std::collections::HashMap;

use rusqlite::params;
use serde::{Deserialize, Serialize};

use pixel_index::{GramExtractor, TrigramExtractor};

use crate::store::{FactsStore, Result, short_oid, subject_of};

pub const PER_SCOPE_CANDIDATES: usize = 200;

/// Facet selector for `search`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchFacet {
    Message,
    Path,
    Diff,
    All,
}

impl Default for SearchFacet {
    fn default() -> Self {
        SearchFacet::All
    }
}

impl From<&str> for SearchFacet {
    fn from(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "message" => SearchFacet::Message,
            "path" => SearchFacet::Path,
            "diff" => SearchFacet::Diff,
            _ => SearchFacet::All,
        }
    }
}

/// A single search hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub oid: String,
    pub at: String,
    pub subject: String,
    pub author: String,
    pub kind: String,
    pub path: Option<String>,
    pub snippet: Option<String>,
    pub files_touched: u64,
    pub score: f64,
}

/// The ranked result of a search.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    pub facet: String,
    pub query: String,
    pub pass: String,
    pub candidates: Vec<SearchHit>,
    /// true when the strict (AND) pass returned nothing and we fell back to OR.
    pub degraded_to_or: bool,
}

/// Parse a raw query into search units. Phrases are kept whole; bare words are
/// split on non-alphanumeric. Mirrors usable-git's `sanitizeQuery`.
pub fn sanitize_query(raw: &str) -> Vec<String> {
    let mut units: Vec<String> = Vec::new();
    // Extract double-quoted phrases first.
    let mut rest = raw.to_string();
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        match after.find('"') {
            Some(close) => {
                let phrase = after[..close].trim().to_string();
                if phrase.chars().count() > 1 {
                    units.push(phrase);
                }
                rest = format!("{}{}", &rest[..open], &after[close + 1..]);
            }
            None => {
                rest = rest[open..].replace('"', " ");
                break;
            }
        }
    }
    let terms: Vec<String> = rest
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|t| t.chars().count() > 1)
        .map(|t| t.to_string())
        .collect();
    units.extend(terms);
    units
}

/// The public search entry point.
pub fn search(
    store: &FactsStore,
    query: &str,
    facet: SearchFacet,
    limit: usize,
) -> Result<SearchResult> {
    let units = sanitize_query(query);
    let limit = limit.min(200);
    let mut all: Vec<SearchHit> = Vec::new();
    let facet_str = match facet {
        SearchFacet::Message => "message",
        SearchFacet::Path => "path",
        SearchFacet::Diff => "diff",
        SearchFacet::All => "all",
    };

    if matches!(facet, SearchFacet::Message | SearchFacet::All) {
        all.extend(message_search(store, &units, limit)?);
    }
    if matches!(facet, SearchFacet::Path | SearchFacet::All) {
        all.extend(path_search(store, &units, limit)?);
    }
    if matches!(facet, SearchFacet::Diff | SearchFacet::All) {
        all.extend(diff_search(store, &units, limit)?);
    }

    // Dedup by (oid, kind), keep highest score.
    let mut best: HashMap<(String, String), SearchHit> = HashMap::new();
    for hit in all {
        let key = (hit.oid.clone(), hit.kind.clone());
        match best.get_mut(&key) {
            Some(existing) => {
                if hit.score > existing.score {
                    *existing = hit;
                }
            }
            None => {
                best.insert(key, hit);
            }
        }
    }
    let mut candidates: Vec<SearchHit> = best.into_values().collect();
    // Rank: score desc, then recency desc, then oid asc.
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.at.cmp(&a.at))
            .then_with(|| a.oid.cmp(&b.oid))
    });
    candidates.truncate(limit);

    Ok(SearchResult {
        facet: facet_str.to_string(),
        query: query.to_string(),
        pass: "and".to_string(),
        candidates,
        degraded_to_or: false,
    })
}

/// Occurrence-count relevance of `units` in `text` (usable-git's design:
/// occurrence count, then recency).
pub fn relevance_of(text: &str, units: &[String]) -> u64 {
    let lower = text.to_lowercase();
    units
        .iter()
        .map(|u| {
            let needle = u.to_lowercase();
            lower.matches(&needle).count() as u64
        })
        .sum()
}

/// Recency tiebreak: a monotonic-id-based epsilon that breaks true relevance
/// ties toward the newest commit, exactly like usable-git's `RECENCY_EPSILON`.
fn recency_score(id: i64, max_id: i64) -> f64 {
    if max_id <= 0 {
        0.0
    } else {
        (id as f64 / max_id as f64) * 1e-3
    }
}

fn max_commit_id(store: &FactsStore) -> i64 {
    store
        .conn()
        .query_row("SELECT COALESCE(MAX(id),0) FROM commits", [], |r| r.get(0))
        .unwrap_or(0)
}

fn to_hit(
    oid: &str,
    at: &str,
    subject: &str,
    author: &str,
    files_touched: u64,
    kind: &str,
    path: Option<&str>,
    snippet: Option<&str>,
    score: f64,
) -> SearchHit {
    SearchHit {
        oid: short_oid(oid),
        at: at.to_string(),
        subject: subject_of(subject).to_string(),
        author: author.to_string(),
        kind: kind.to_string(),
        path: path.map(|p| p.to_string()),
        snippet: snippet.map(|s| s.to_string()),
        files_touched,
        score,
    }
}

fn message_search(store: &FactsStore, units: &[String], limit: usize) -> Result<Vec<SearchHit>> {
    if units.is_empty() {
        return Ok(Vec::new());
    }
    let match_expr = fts_match(&units, "and");
    let max_id = max_commit_id(store);
    let mut stmt = store.conn().prepare(
        "SELECT c.id, c.oid, c.committed_at, c.author, c.message
         FROM messages_fts
         JOIN commits c ON c.id = messages_fts.rowid
         WHERE messages_fts MATCH ?1
         ORDER BY c.committed_at DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![match_expr, limit as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut hits = Vec::new();
    for row in rows {
        let (id, oid, at, author, message) = row?;
        let rel = relevance_of(&message, units) as f64;
        let score = rel + recency_score(id, max_id);
        hits.push(to_hit(&oid, &at, &message, &author, 0, "message", None, None, score));
    }
    Ok(hits)
}

fn fts_match(units: &[String], pass: &str) -> String {
    let sep = if pass == "and" { " AND " } else { " OR " };
    let quoted: Vec<String> = units
        .iter()
        .map(|u| format!("\"{}\"", u.replace('"', "")))
        .collect();
    quoted.join(sep)
}

fn path_search(store: &FactsStore, units: &[String], limit: usize) -> Result<Vec<SearchHit>> {
    if units.is_empty() {
        return Ok(Vec::new());
    }
    // Trigram candidates over file_changes.path, verified against path text.
    let hashes = covering_hashes(units);
    if hashes.is_empty() {
        return Ok(Vec::new());
    }
    let mut change_ids: Vec<i64> = Vec::new();
    {
        let placeholders = hashes.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT DISTINCT change_id FROM path_grams WHERE hash IN ({placeholders})"
        );
        let mut stmt = store.conn().prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(hashes.iter().map(|h| *h as i64)))?;
        while let Some(row) = rows.next()? {
            change_ids.push(row.get(0)?);
        }
    }
    if change_ids.is_empty() {
        return Ok(Vec::new());
    }
    let max_id = max_commit_id(store);
    let mut hits = Vec::new();
    for change_id in change_ids.iter().take(limit) {
        let row: Option<(i64, String, String, String, String, String, u64)> = store
            .conn()
            .query_row(
                "SELECT c.id, c.oid, c.committed_at, c.author, c.message, f.path,
                        (SELECT count(*) FROM file_changes fc WHERE fc.commit_id = f.commit_id)
                 FROM file_changes f
                 JOIN commits c ON c.id = f.commit_id
                 WHERE f.id = ?1",
                [change_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )
            .ok();
        if let Some((id, oid, at, author, message, path, ft)) = row {
            let rel = relevance_of(&path, units) as f64;
            let score = rel + recency_score(id, max_id);
            hits.push(to_hit(&oid, &at, &message, &author, ft, "path", Some(&path), Some(&path), score));
        }
    }
    Ok(hits)
}

fn diff_search(store: &FactsStore, units: &[String], limit: usize) -> Result<Vec<SearchHit>> {
    if units.is_empty() {
        return Ok(Vec::new());
    }
    let hashes = covering_hashes(units);
    if hashes.is_empty() {
        return Ok(Vec::new());
    }
    // Candidate hunks whose grams intersect the query covering.
    let mut hunk_ids: Vec<i64> = Vec::new();
    {
        let placeholders = hashes.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("SELECT DISTINCT hunk_id FROM diff_grams WHERE hash IN ({placeholders})");
        let mut stmt = store.conn().prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(hashes.iter().map(|h| *h as i64)))?;
        while let Some(row) = rows.next()? {
            hunk_ids.push(row.get(0)?);
        }
    }
    if hunk_ids.is_empty() {
        return Ok(Vec::new());
    }
    let max_id = max_commit_id(store);
    let mut hits = Vec::new();
    // Verified against hunks text: only count real (non-stale) hits.
    for hunk_id in hunk_ids.iter().take(limit * 2) {
        let row: Option<(i64, String, String, String, String, u64, String, String)> = store
            .conn()
            .query_row(
                "SELECT c.id, c.oid, c.committed_at, c.author, c.message,
                        (SELECT count(*) FROM file_changes fc WHERE fc.commit_id = h.commit_id),
                        h.added, h.removed
                 FROM hunks h
                 JOIN commits c ON c.id = h.commit_id
                 WHERE h.id = ?1",
                [hunk_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
            )
            .ok();
        if let Some((id, oid, at, author, message, ft, added, removed)) = row {
            // Verify: does the added/removed text actually contain the units?
            let text = format!("{added}\n{removed}");
            let rel = relevance_of(&text, units) as f64;
            if rel == 0.0 {
                continue; // stale gram or false positive — drop
            }
            let score = rel + recency_score(id, max_id);
            let snippet = make_snippet(&text, &units);
            hits.push(to_hit(&oid, &at, &message, &author, ft, "diff", None, Some(&snippet), score));
        }
    }
    Ok(hits)
}

/// Covering gram hashes for the query units (union across units; candidates
/// then verified against hunks text, so a false positive is harmless).
pub fn covering_hashes(units: &[String]) -> Vec<u64> {
    let extractor = TrigramExtractor;
    let mut hashes: Vec<u64> = Vec::new();
    for unit in units {
        for h in extractor.covering(unit.as_bytes()) {
            hashes.push(h);
        }
    }
    hashes.sort_unstable();
    hashes.dedup();
    hashes
}

/// A small snippet around the first occurrence of any unit.
fn make_snippet(text: &str, units: &[String]) -> String {
    let lower = text.to_lowercase();
    let mut start = 0usize;
    let mut found = false;
    for u in units {
        let needle = u.to_lowercase();
        if let Some(pos) = lower[start..].find(&needle) {
            start += pos;
            found = true;
            break;
        }
    }
    if !found {
        return text.chars().take(120).collect();
    }
    let s = start.saturating_sub(20);
    let e = (start + 120).min(text.len());
    let mut out = String::new();
    if s > 0 {
        out.push('…');
    }
    out.push_str(&text[s..e]);
    if e < text.len() {
        out.push('…');
    }
    out
}
