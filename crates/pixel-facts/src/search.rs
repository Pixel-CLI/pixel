// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `search.rs` — history search: `search {query, facet: message|path|diff|all}`.
//!
//! Diff/path scopes take trigram candidates from `diff_fts` / `path_fts`
//! (`text_index`), verified against the `hunks` / `file_changes` text.
//! Message scope uses FTS5. Ranking = occurrence count then recency
//! (usable-git's post-bm25 design). Budgeted: 200 candidates/scope.

use std::collections::{HashMap, HashSet};

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::store::{FactsStore, Result, short_oid, subject_of};
use crate::text_index::{CANDIDATE_CAP, matching_changes, matching_hunks};

pub const PER_SCOPE_CANDIDATES: usize = 200;

/// Facet selector for `search`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SearchFacet {
    Message,
    Path,
    Diff,
    #[default]
    All,
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
        .map(ToString::to_string)
        .collect();
    units.extend(terms);
    units
}

/// The public search entry point.
/// Snippet cap per hit. Snippets are diff text: a single large commit diff
/// can be 50 KB+, and 200 hits × 50 KB = 10 MB into the agent's context
/// window. 500 chars is enough to show the relevant context line; the
/// agent can `pixel diff <oid>` for the full diff if needed.
const SNIPPET_CAP_CHARS: usize = 500;

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

    // Cap snippet size per hit (see `SNIPPET_CAP_CHARS`).
    for hit in candidates.iter_mut() {
        if let Some(snippet) = hit.snippet.as_mut()
            && snippet.chars().count() > SNIPPET_CAP_CHARS
        {
            let truncated: String = snippet.chars().take(SNIPPET_CAP_CHARS).collect();
            *snippet = format!("{truncated}… [snippet truncated at {SNIPPET_CAP_CHARS} chars]");
        }
    }

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

#[allow(clippy::too_many_arguments)]
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
        path: path.map(ToString::to_string),
        snippet: snippet.map(ToString::to_string),
        files_touched,
        score,
    }
}

fn message_search(store: &FactsStore, units: &[String], limit: usize) -> Result<Vec<SearchHit>> {
    if units.is_empty() {
        return Ok(Vec::new());
    }
    let match_expr = fts_match(units, "and");
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
        hits.push(to_hit(
            &oid, &at, &message, &author, 0, "message", None, None, score,
        ));
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
    let change_ids = matching_changes(store.conn(), units, CANDIDATE_CAP)?.unwrap_or_default();
    let max_id = max_commit_id(store);
    let mut hits = Vec::new();
    // `limit` counts commits: several changed paths of one commit are one
    // result once `search` dedups them, so they must not use up the page.
    let mut commits: HashSet<String> = HashSet::new();
    for change_id in &change_ids {
        if commits.len() >= limit {
            break;
        }
        let row: Option<(i64, String, String, String, String, String, u64)> = store
            .conn()
            .query_row(
                "SELECT c.id, c.oid, c.committed_at, c.author, c.message, f.path,
                        (SELECT count(*) FROM file_changes fc WHERE fc.commit_id = f.commit_id)
                 FROM file_changes f
                 JOIN commits c ON c.id = f.commit_id
                 WHERE f.id = ?1",
                [change_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get::<_, i64>(6)? as u64,
                    ))
                },
            )
            .ok();
        if let Some((id, oid, at, author, message, path, ft)) = row {
            let rel = relevance_of(&path, units) as f64;
            if rel == 0.0 {
                continue; // trigrams present but not adjacent — drop
            }
            commits.insert(oid.clone());
            let score = rel + recency_score(id, max_id);
            hits.push(to_hit(
                &oid,
                &at,
                &message,
                &author,
                ft,
                "path",
                Some(&path),
                Some(&path),
                score,
            ));
        }
    }
    Ok(hits)
}

fn diff_search(store: &FactsStore, units: &[String], limit: usize) -> Result<Vec<SearchHit>> {
    if units.is_empty() {
        return Ok(Vec::new());
    }
    // Candidate hunks holding every trigram of a unit, newest first.
    let hunk_ids = matching_hunks(store.conn(), units, CANDIDATE_CAP)?.unwrap_or_default();
    let max_id = max_commit_id(store);
    let mut hits = Vec::new();
    // Verified against hunks text: only count real hits, until `limit`
    // commits. Several hunks of one commit are one result once `search`
    // dedups them, so they must not use up the page.
    let mut commits: HashSet<String> = HashSet::new();
    for hunk_id in &hunk_ids {
        if commits.len() >= limit {
            break;
        }
        #[allow(clippy::type_complexity)]
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
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get::<_, i64>(5)? as u64,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .ok();
        if let Some((id, oid, at, author, message, ft, added, removed)) = row {
            // Verify: does the added/removed text actually contain the units?
            let text = format!("{added}\n{removed}");
            let rel = relevance_of(&text, units) as f64;
            if rel == 0.0 {
                continue; // stale gram or false positive — drop
            }
            commits.insert(oid.clone());
            let score = rel + recency_score(id, max_id);
            let snippet = make_snippet(&text, units);
            hits.push(to_hit(
                &oid,
                &at,
                &message,
                &author,
                ft,
                "diff",
                None,
                Some(&snippet),
                score,
            ));
        }
    }
    Ok(hits)
}

/// A small snippet around the first occurrence of any unit.
fn make_snippet(text: &str, units: &[String]) -> String {
    units
        .iter()
        .find_map(|u| find_case_insensitive(text, u))
        .map_or_else(
            || text.chars().take(120).collect(),
            |pos| window_around(text, pos),
        )
}

/// Byte offset in `text` of the first case-insensitive occurrence of
/// `needle`, always on a char boundary of `text`.
///
/// Searching `text.to_lowercase()` alone gives an offset into the
/// lowercased string, which is not `text`'s: `İ` (2 bytes) lowercases to
/// `i̇` (3 bytes), so the offset drifts and can land past `text`'s end or
/// inside one of its characters (#769). Each byte of the lowercased string
/// is mapped back to the start of the character it came from.
///
/// Both sides fold character by character with [`fold_case`], never with
/// `str::to_lowercase`, whose word-final `Σ` → `ς` depends on context and
/// would stop a needle from matching the same word in `text`.
pub(crate) fn find_case_insensitive(text: &str, needle: &str) -> Option<usize> {
    let needle: String = needle.chars().flat_map(fold_case).collect();
    let mut lower = String::with_capacity(text.len());
    let mut origin = Vec::with_capacity(text.len());
    for (at, ch) in text.char_indices() {
        for lc in fold_case(ch) {
            lower.push(lc);
            origin.resize(lower.len(), at);
        }
    }
    lower
        .find(&needle)
        .map(|pos| origin.get(pos).copied().unwrap_or(text.len()))
}

/// Context-free lowercase of one character, with the final sigma `ς`
/// folded into `σ` so a word matches whether its sigma is final or not.
fn fold_case(ch: char) -> impl Iterator<Item = char> {
    ch.to_lowercase().map(|lc| if lc == 'ς' { 'σ' } else { lc })
}

/// Up to 20 bytes before `pos` and 120 after it, widened to whole
/// characters, with `…` marking each elided end.
pub(crate) fn window_around(text: &str, pos: usize) -> String {
    let s = text.floor_char_boundary(pos.saturating_sub(20));
    let e = text.ceil_char_boundary((pos + 120).min(text.len()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::tests::ingest_within;
    use crate::testutil::two_commit_repo;

    #[test]
    fn sanitize_query_keeps_quoted_phrases_whole_and_drops_one_char_terms() {
        assert_eq!(
            sanitize_query(r#"fix "rate limit" x ab retry_once, y"#),
            vec!["rate limit", "fix", "ab", "retry_once"]
        );
        // An unterminated quote is plain text.
        assert_eq!(sanitize_query(r#""quote here"#), vec!["quote", "here"]);
        assert!(sanitize_query("").is_empty());
        assert!(sanitize_query(r#""x""#).is_empty());
    }

    #[test]
    fn to_hit_carries_every_field_with_the_short_oid_and_subject_line() {
        let hit = to_hit(
            "0123456789abcdef0123456789abcdef01234567",
            "2026-01-01T00:00:00Z",
            "subject line\n\nbody",
            "Ann",
            3,
            "diff",
            Some("src/a.rs"),
            Some("+added"),
            2.5,
        );
        assert_eq!(
            hit.oid,
            short_oid("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(hit.subject, "subject line");
        assert_eq!(hit.at, "2026-01-01T00:00:00Z");
        assert_eq!(hit.author, "Ann");
        assert_eq!(hit.kind, "diff");
        assert_eq!(hit.path.as_deref(), Some("src/a.rs"));
        assert_eq!(hit.snippet.as_deref(), Some("+added"));
        assert_eq!(hit.files_touched, 3);
        assert_eq!(hit.score, 2.5);
    }

    #[test]
    fn search_finds_a_commit_by_message_and_by_diff_text() {
        let (dir, _, second) = two_commit_repo();
        let mut store = FactsStore::open(dir.path()).unwrap();
        ingest_within(&mut store);
        let by_message = search(&store, "helper", SearchFacet::Message, 50).unwrap();
        assert_eq!(by_message.candidates.len(), 1, "{by_message:?}");
        assert_eq!(by_message.candidates[0].oid, short_oid(&second));
        let by_diff = search(&store, "secret_token", SearchFacet::Diff, 50).unwrap();
        assert_eq!(by_diff.candidates.len(), 1, "{by_diff:?}");
        assert_eq!(by_diff.candidates[0].oid, short_oid(&second));
        assert!(
            by_diff.candidates[0]
                .snippet
                .as_deref()
                .unwrap_or("")
                .contains("secret_token")
        );
    }

    /// A path holding a unit's trigrams apart is a candidate of the index
    /// but not a hit: path search verifies the path text like diff search.
    #[test]
    fn path_search_reports_only_paths_that_contain_the_unit() {
        let dir = crate::testutil::init_repo();
        let root = dir.path();
        let apart = crate::testutil::commit(root, &[("abc/xbcd.txt", b"1\n")], "apart");
        let whole = crate::testutil::commit(root, &[("src/abcd.txt", b"2\n")], "whole");
        let mut store = FactsStore::open(root).unwrap();
        ingest_within(&mut store);
        let hits = search(&store, "abcd", SearchFacet::Path, 50).unwrap();
        let got: Vec<(String, Option<String>)> = hits
            .candidates
            .iter()
            .map(|h| (h.oid.clone(), h.path.clone()))
            .collect();
        assert_eq!(
            got,
            vec![(short_oid(&whole), Some("src/abcd.txt".to_string()))]
        );
        assert_ne!(short_oid(&apart), short_oid(&whole));
    }

    /// The page counts commits, not hunks or paths: a recent commit that
    /// touched three matching files must not push an older matching commit
    /// off a two-result page.
    #[test]
    fn diff_and_path_search_fill_the_limit_with_distinct_commits() {
        let dir = crate::testutil::init_repo();
        let root = dir.path();
        let older = crate::testutil::commit_at(
            root,
            &[("keep/one.txt", b"shared_word\n")],
            "older",
            crate::testutil::days_ago(2),
        );
        let newer = crate::testutil::commit_at(
            root,
            &[
                ("keep/a.txt", b"shared_word a\n"),
                ("keep/b.txt", b"shared_word b\n"),
                ("keep/c.txt", b"shared_word c\n"),
            ],
            "newer",
            crate::testutil::days_ago(1),
        );
        let mut store = FactsStore::open(root).unwrap();
        ingest_within(&mut store);
        let oids = |facet: SearchFacet, query: &str| -> Vec<String> {
            let mut got: Vec<String> = search(&store, query, facet, 2)
                .unwrap()
                .candidates
                .into_iter()
                .map(|h| h.oid)
                .collect();
            got.sort_unstable();
            got
        };
        let mut want = vec![short_oid(&older), short_oid(&newer)];
        want.sort_unstable();
        assert_eq!(oids(SearchFacet::Diff, "shared_word"), want);
        assert_eq!(oids(SearchFacet::Path, "keep/"), want);
    }

    #[test]
    fn find_case_insensitive_should_return_an_offset_into_the_original_text() {
        // `İ` is 2 bytes and lowercases to 3: the hit is at byte 4 of `text`,
        // byte 6 of its lowercased form (#769).
        assert_eq!(find_case_insensitive("İİneedle", "NEEDLE"), Some(4));
        assert_eq!(find_case_insensitive("aBc", "b"), Some(1));
        assert_eq!(find_case_insensitive("日本needle", "Needle"), Some(6));
        assert_eq!(find_case_insensitive("abc", "x"), None);
        assert_eq!(find_case_insensitive("", ""), Some(0));
        assert_eq!(find_case_insensitive("ab", ""), Some(0));
    }

    #[test]
    fn find_case_insensitive_should_match_greek_sigma_in_any_position_and_case() {
        // `str::to_lowercase` turns a word-final `Σ` into `ς` while
        // `char::to_lowercase` gives `σ`: both sides fold the same way, so
        // the final, medial and capital forms all meet.
        assert_eq!(find_case_insensitive("ΟΔΟΣ x", "ΟΔΟΣ"), Some(0));
        assert_eq!(find_case_insensitive("x οδος", "ΟΔΟΣ"), Some(2));
        assert_eq!(find_case_insensitive("x ΟΔΟΣ", "οδος"), Some(2));
        assert_eq!(find_case_insensitive("ΟΔΟΣΟ", "οδοσ"), Some(0));
    }

    #[test]
    fn make_snippet_should_center_on_the_original_offset_when_lowercasing_changes_the_length() {
        // The window is 20 bytes of `text` before the hit: ten `İ`, not the
        // lowercased offset that would have shifted it towards the end.
        let text = format!("{}NEEDLE tail", "İ".repeat(30));
        assert_eq!(
            make_snippet(&text, &["needle".to_string()]),
            format!("…{}NEEDLE tail", "İ".repeat(10))
        );
    }

    #[test]
    fn make_snippet_should_use_the_first_unit_found_and_fall_back_to_the_head() {
        let units = ["absent".to_string(), "two".to_string()];
        assert_eq!(make_snippet("one two three", &units), "one two three");
        let long = "x".repeat(200);
        assert_eq!(make_snippet(&long, &units), "x".repeat(120));
        let text = format!("{}two{}", "a".repeat(30), "b".repeat(200));
        assert_eq!(
            make_snippet(&text, &units),
            format!("…{}two{}…", "a".repeat(20), "b".repeat(117))
        );
    }
}
