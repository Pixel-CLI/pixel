// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Lexical search over the turn corpus: trigram candidates from the
//! segments, authoritative regex verification against `turns.text`.
//!
//! Freshness: turns newer than the segments' high-water mark are always
//! candidates, so search never needs a rebuild to see new conversation.

use std::collections::HashSet;

use pixel_index::TrigramExtractor;
use pixel_index::plan::plan_pattern;
use pixel_index::posting::{GramQuery, resolve_query};
use regex::Regex;
use rusqlite::params_from_iter;

use crate::segment::SegmentSet;
use crate::store::RecallStore;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SearchFilters {
    pub agent: Option<String>,
    pub repo_prefix: Option<String>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub role: Option<String>,
    pub human_only: bool,
    pub session_id: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub turn_id: i64,
    pub session_id: i64,
    pub seq: i64,
    pub agent: String,
    pub source_session_id: String,
    pub cwd: Option<String>,
    pub role: String,
    pub ts: Option<i64>,
    pub ts_source: String,
    pub snippet: String,
    pub snippet_truncated: bool,
    pub turn_truncated: bool,
}

#[derive(Debug, Default)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub turns_considered: usize,
    /// True when the scan stopped early at the limit — more matches may
    /// exist beyond the returned page.
    pub truncated: bool,
}

const SNIPPET_RADIUS: usize = 120;
/// Above this candidate count, an ordered scan beats huge IN() fetches.
const CANDIDATE_FETCH_MAX: usize = 10_000;
/// Most turns an agent-filtered ordered scan reads through the session index
/// and sorts before the first row. Above it, the scan walks the ts index
/// instead and stops at the first page: sorting claude's 194k turns (text
/// included) took 260 ms for 50 rows where the walk takes under 10 ms, while
/// a walk for an agent with few turns in the repository can read the whole
/// index (120 ms) where the sort is instant. The walk is also refused when
/// more turns than the agent's own are newer than its last activity: it
/// would pass all of them before its first match.
const AGENT_SORT_MAX_TURNS: i64 = 20_000;

pub fn search(
    store: &RecallStore,
    segments: &SegmentSet,
    pattern: &str,
    whole_word: bool,
    filters: &SearchFilters,
    offset: usize,
    limit: usize,
) -> Result<SearchResult, String> {
    let effective_pattern = if whole_word {
        format!(r"\b(?:{pattern})\b")
    } else {
        pattern.to_string()
    };
    let re = Regex::new(&effective_pattern).map_err(|e| format!("bad pattern: {e}"))?;

    // Candidate turn ids from the trigram segments. `None` = every turn is
    // a candidate (pattern had no required literals).
    let plan =
        plan_pattern(&effective_pattern, &TrigramExtractor).map_err(|e| format!("pattern: {e}"))?;
    let candidates: Option<HashSet<i64>> = match plan {
        GramQuery::All => None,
        plan => {
            let mut set = HashSet::new();
            for shard in segments.open_shards() {
                let ids = resolve_query(&plan, shard.file_count(), &|h| shard.postings(h));
                for local in ids {
                    if let Some(turn_id) = shard.path_of(local).and_then(|p| p.parse::<i64>().ok())
                    {
                        set.insert(turn_id);
                    }
                }
            }
            Some(set)
        }
    };
    let tail_floor = segments.manifest.last_turn_id;
    // Unindexed tail turns are always candidates; a huge tail (mass ingest
    // before segments catch up) must push us to the ordered-scan path, or
    // the id-fetch path degenerates into fetching the whole tail.
    let tail_count: i64 = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM turns WHERE id > ?1",
            [tail_floor],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;

    // Small candidate sets: targeted fetch. Otherwise (or with no
    // narrowing): one ordered scan with SQL-side filters, early-stopped.
    let use_fetch = candidates
        .as_ref()
        .is_some_and(|c| c.len() + tail_count as usize <= CANDIDATE_FETCH_MAX);

    let mut result = SearchResult::default();
    let mut skipped = 0usize;

    let mut visit = |row: HitRow| -> bool {
        result.turns_considered += 1;
        let Some(m) = re.find(&row.text) else {
            return true;
        };
        if skipped < offset {
            skipped += 1;
            return true;
        }
        let (snippet, snippet_truncated) = snippet_around(&row.text, m.start(), m.end());
        result.hits.push(SearchHit {
            turn_id: row.turn_id,
            session_id: row.session_id,
            seq: row.seq,
            agent: row.agent,
            source_session_id: row.source_session_id,
            cwd: row.cwd,
            role: row.role,
            ts: row.ts,
            ts_source: row.ts_source,
            snippet,
            snippet_truncated,
            turn_truncated: row.turn_truncated,
        });
        result.hits.len() < limit
    };

    if use_fetch {
        let mut ids: Vec<i64> = candidates
            .as_ref()
            .unwrap()
            .iter()
            .copied()
            .filter(|id| *id <= tail_floor)
            .collect();
        // Tail turns (unindexed) join the candidate set unconditionally.
        ids.extend(tail_turn_ids(store, tail_floor)?);
        let mut rows = fetch_rows_by_ids(store, &ids, filters)?;
        rows.sort_by(|a, b| b.ts.cmp(&a.ts).then(b.turn_id.cmp(&a.turn_id)));
        for row in rows {
            if !visit(row) {
                result.truncated = true;
                break;
            }
        }
    } else {
        scan_ordered(
            store,
            filters,
            candidates.as_ref(),
            tail_floor,
            &mut |row| visit(row),
        )
        .map(|stopped_early| result.truncated = stopped_early)?;
    }
    Ok(result)
}

struct HitRow {
    turn_id: i64,
    session_id: i64,
    seq: i64,
    agent: String,
    source_session_id: String,
    cwd: Option<String>,
    role: String,
    ts: Option<i64>,
    ts_source: String,
    text: String,
    turn_truncated: bool,
}

const ROW_SELECT: &str = "SELECT t.id, t.session_id, t.seq, s.agent, s.source_session_id,
    s.cwd, t.role, t.ts, s.ts_source, t.text, t.truncated
    FROM turns t JOIN sessions s ON s.id = t.session_id";

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<HitRow> {
    Ok(HitRow {
        turn_id: r.get(0)?,
        session_id: r.get(1)?,
        seq: r.get(2)?,
        agent: r.get(3)?,
        source_session_id: r.get(4)?,
        cwd: r.get(5)?,
        role: r.get(6)?,
        ts: r.get(7)?,
        ts_source: r.get(8)?,
        text: r.get(9)?,
        turn_truncated: r.get::<_, i64>(10)? != 0,
    })
}

pub(crate) fn filter_sql(
    filters: &SearchFilters,
    args: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
) -> String {
    filter_sql_with(filters, args, "s.agent")
}

/// `filter_sql` with the agent compared through `agent_column`: `s.agent`
/// lets the planner start from `idx_sessions_agent_ts`, `+s.agent` (a unary
/// plus, same value) keeps it off that index so an ordered scan walks
/// `idx_turns_ts` and stops early.
fn filter_sql_with(
    filters: &SearchFilters,
    args: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
    agent_column: &str,
) -> String {
    let mut sql = String::new();
    if let Some(a) = &filters.agent {
        sql.push_str(&format!(" AND {agent_column} = ?"));
        args.push(Box::new(a.clone()));
    }
    if let Some(r) = &filters.repo_prefix {
        sql.push_str(" AND s.cwd LIKE ? || '%'");
        args.push(Box::new(r.clone()));
    }
    if let Some(s) = filters.since_ms {
        sql.push_str(" AND t.ts >= ?");
        args.push(Box::new(s));
    }
    if let Some(u) = filters.until_ms {
        sql.push_str(" AND t.ts <= ?");
        args.push(Box::new(u));
    }
    if let Some(role) = &filters.role {
        sql.push_str(" AND t.role = ?");
        args.push(Box::new(role.clone()));
    }
    if filters.human_only {
        sql.push_str(" AND (t.role != 'user' OR t.intent_source = 'human')");
    }
    if let Some(sid) = filters.session_id {
        sql.push_str(" AND t.session_id = ?");
        args.push(Box::new(sid));
    }
    sql
}

fn tail_turn_ids(store: &RecallStore, floor: i64) -> Result<Vec<i64>, String> {
    let mut stmt = store
        .connection()
        .prepare_cached("SELECT id FROM turns WHERE id > ?1")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([floor], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<i64>, _>>()
        .map_err(|e| e.to_string())
}

fn fetch_rows_by_ids(
    store: &RecallStore,
    ids: &[i64],
    filters: &SearchFilters,
) -> Result<Vec<HitRow>, String> {
    let mut rows = Vec::new();
    for chunk in ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = chunk
            .iter()
            .map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>)
            .collect();
        let mut sql = format!("{ROW_SELECT} WHERE t.id IN ({placeholders})");
        sql.push_str(&filter_sql(filters, &mut args));
        let mut stmt = store
            .connection()
            .prepare(&sql)
            .map_err(|e| e.to_string())?;
        let mapped = stmt
            .query_map(params_from_iter(args.iter().map(AsRef::as_ref)), row_from)
            .map_err(|e| e.to_string())?;
        for r in mapped {
            rows.push(r.map_err(|e| e.to_string())?);
        }
    }
    Ok(rows)
}

/// Turns in the sessions the session-level filters (agent, repo) keep, from
/// `sessions.turn_count`, and the newest of their turns (`ts_last`): what an
/// agent-filtered ordered scan would sort through the session index, and
/// where a ts walk would meet its first candidate.
fn session_span(
    store: &RecallStore,
    filters: &SearchFilters,
) -> Result<(i64, Option<i64>), String> {
    let mut sql = "SELECT COALESCE(SUM(s.turn_count), 0), MAX(s.ts_last) FROM sessions s WHERE 1=1"
        .to_string();
    let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(a) = &filters.agent {
        sql.push_str(" AND s.agent = ?");
        args.push(Box::new(a.clone()));
    }
    if let Some(r) = &filters.repo_prefix {
        sql.push_str(" AND s.cwd LIKE ? || '%'");
        args.push(Box::new(r.clone()));
    }
    store
        .connection()
        .query_row(
            &sql,
            params_from_iter(args.iter().map(AsRef::as_ref)),
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| e.to_string())
}

/// Turns newer than `ts` (all timestamped turns when `ts` is `None`),
/// counted up to `cap` only: a count past the agent's own turns is enough to
/// refuse the walk, and stopping there keeps the count cheaper than the sort
/// it decides about.
fn turns_after(store: &RecallStore, ts: Option<i64>, cap: i64) -> Result<i64, String> {
    store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM (SELECT 1 FROM turns WHERE ts > ?1 LIMIT ?2)",
            rusqlite::params![ts.unwrap_or(i64::MIN), cap],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())
}

/// True when the agent's sessions hold more turns than a sort should take
/// (see `AGENT_SORT_MAX_TURNS`).
fn over_sort_ceiling(agent_turns: i64, sort_max: i64) -> bool {
    agent_turns > sort_max
}

/// True when a ts walk reaches the agent's turns without first passing more
/// turns than sorting the agent's own would read.
fn newer_turns_fit(newer_turns: i64, agent_turns: i64) -> bool {
    newer_turns <= agent_turns
}

/// Whether an agent-filtered ordered scan should walk the ts index (compare
/// `+s.agent`) rather than sort the agent's turns from its sessions.
fn agent_scan_walks(
    store: &RecallStore,
    filters: &SearchFilters,
    sort_max: i64,
) -> Result<bool, String> {
    let (agent_turns, last) = session_span(store, filters)?;
    if !over_sort_ceiling(agent_turns, sort_max) {
        return Ok(false);
    }
    let newer = turns_after(store, last, agent_turns.saturating_add(1))?;
    Ok(newer_turns_fit(newer, agent_turns))
}

/// The SQL and arguments of the ordered ts-desc scan. With an agent filter,
/// the plan is picked by `agent_scan_walks`: walk the ts index, or start
/// from the agent's sessions and sort.
fn ordered_scan_query(
    store: &RecallStore,
    filters: &SearchFilters,
    sort_max: i64,
) -> Result<(String, Vec<Box<dyn rusqlite::types::ToSql>>), String> {
    let agent_column = match &filters.agent {
        Some(_) if agent_scan_walks(store, filters, sort_max)? => "+s.agent",
        _ => "s.agent",
    };
    let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut sql = format!("{ROW_SELECT} WHERE 1=1");
    sql.push_str(&filter_sql_with(filters, &mut args, agent_column));
    sql.push_str(" ORDER BY t.ts DESC NULLS LAST, t.id DESC");
    Ok((sql, args))
}

/// Ordered ts-desc scan with SQL filters; `visit` returns false to stop.
/// Returns whether the scan stopped early.
fn scan_ordered(
    store: &RecallStore,
    filters: &SearchFilters,
    candidates: Option<&HashSet<i64>>,
    tail_floor: i64,
    visit: &mut dyn FnMut(HitRow) -> bool,
) -> Result<bool, String> {
    let (sql, args) = ordered_scan_query(store, filters, AGENT_SORT_MAX_TURNS)?;
    let mut stmt = store
        .connection()
        .prepare(&sql)
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params_from_iter(args.iter().map(AsRef::as_ref)), row_from)
        .map_err(|e| e.to_string())?;
    for r in rows {
        let row = r.map_err(|e| e.to_string())?;
        if let Some(set) = candidates
            && row.turn_id <= tail_floor
            && !set.contains(&row.turn_id)
        {
            continue;
        }
        if !visit(row) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Trigram-candidate count for a pattern — `None` when the pattern has no
/// required literals (every turn matches). Cheap: postings only, no SQL,
/// no verification. Used to skip uselessly common words in ask's lexical
/// channel.
pub fn candidate_count(segments: &SegmentSet, pattern: &str) -> Option<usize> {
    let plan = plan_pattern(pattern, &TrigramExtractor).ok()?;
    if matches!(plan, GramQuery::All) {
        return None;
    }
    let mut total = 0usize;
    for shard in segments.open_shards() {
        total += resolve_query(&plan, shard.file_count(), &|h| shard.postings(h)).len();
    }
    Some(total)
}

/// Compact one-line rendering of a hit, shared by CLI and daemon.
pub fn format_hit(h: &SearchHit) -> String {
    let ts =
        h.ts.map_or_else(|| "?".to_string(), crate::model::format_ms);
    let cwd = h.cwd.as_deref().unwrap_or("-");
    format!(
        "{}:{} #{} t{} {} {} {} \"{}\"",
        h.agent,
        &h.source_session_id[..h.source_session_id.len().min(8)],
        h.session_id,
        h.seq,
        ts,
        cwd,
        h.role,
        h.snippet
    )
}

/// ±`SNIPPET_RADIUS` chars around the first match, on char boundaries,
/// newlines flattened.
pub(crate) fn snippet_around(text: &str, m_start: usize, m_end: usize) -> (String, bool) {
    let mut start = m_start.saturating_sub(SNIPPET_RADIUS);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (m_end + SNIPPET_RADIUS).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    let mut snippet = text[start..end].replace(['\n', '\r'], " ");
    let truncated = start > 0 || end < text.len();
    if start > 0 {
        snippet = format!("…{snippet}");
    }
    if end < text.len() {
        snippet.push('…');
    }
    (snippet, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IntentSource, Role};
    use crate::testutil::{TS, add_session, add_session_with_intents};

    /// Three turns mentioning `needle` across two sessions, indexed.
    fn corpus() -> (tempfile::TempDir, RecallStore, SegmentSet) {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = RecallStore::open(&tmp.path().join("recall.db")).unwrap();
        add_session(
            &mut store,
            "claude",
            "aaaa1111",
            &[
                (Role::User, "where is the needle kept"),
                (Role::Assistant, "the needle lives in the haystack module"),
            ],
        );
        add_session(
            &mut store,
            "codex",
            "bbbb2222",
            &[
                (Role::User, "unrelated question"),
                (Role::Assistant, "needle again, third mention"),
            ],
        );
        let mut segments = SegmentSet::open(&tmp.path().join("segments")).unwrap();
        segments.index_new(&store).unwrap();
        assert_eq!(segments.manifest.last_turn_id, 4);
        (tmp, store, segments)
    }

    /// A pattern with literals resolves candidates through the trigram
    /// segments and fetches those rows by id: every match comes back,
    /// newest first, and the page limit marks the result truncated.
    #[test]
    fn search_with_literals_fetches_the_candidate_rows_by_id() {
        let (_tmp, store, segments) = corpus();
        let all = search(
            &store,
            &segments,
            "needle",
            false,
            &SearchFilters::default(),
            0,
            10,
        )
        .unwrap();
        assert_eq!(all.hits.len(), 3, "{all:?}");
        assert!(!all.truncated);
        let ts: Vec<i64> = all.hits.iter().map(|h| h.ts.unwrap()).collect();
        assert_eq!(ts, vec![TS + 60_000, TS + 60_000, TS]);
        assert!(all.hits.iter().all(|h| h.snippet.contains("needle")));
        let page = search(
            &store,
            &segments,
            "needle",
            false,
            &SearchFilters::default(),
            0,
            2,
        )
        .unwrap();
        assert_eq!(page.hits.len(), 2);
        assert!(page.truncated);
        let agent = SearchFilters {
            agent: Some("codex".to_string()),
            ..SearchFilters::default()
        };
        let codex = search(&store, &segments, "needle", false, &agent, 0, 10).unwrap();
        assert_eq!(codex.hits.len(), 1);
        assert_eq!(codex.hits[0].source_session_id, "bbbb2222");
    }

    /// `offset` skips that many matches before the page starts: the second
    /// page of one hit is the second-newest match.
    #[test]
    fn search_offset_should_skip_the_first_matches() {
        let (_tmp, store, segments) = corpus();
        let f = SearchFilters::default();
        let all = search(&store, &segments, "needle", false, &f, 0, 10).unwrap();
        let second = search(&store, &segments, "needle", false, &f, 1, 1).unwrap();
        assert_eq!(second.hits.len(), 1);
        assert_eq!(second.hits[0].turn_id, all.hits[1].turn_id);
        let past = search(&store, &segments, "needle", false, &f, 2, 10).unwrap();
        assert_eq!(past.hits.len(), 1);
        assert_eq!(past.hits[0].turn_id, all.hits[2].turn_id);
    }

    #[test]
    fn candidate_count_should_count_trigram_candidates_and_none_without_literals() {
        let (_tmp, _store, segments) = corpus();
        assert_eq!(candidate_count(&segments, "needle"), Some(3));
        assert_eq!(candidate_count(&segments, "haystack"), Some(1));
        assert_eq!(candidate_count(&segments, "."), None);
    }

    /// A pattern without required literals scans the corpus in ts order and
    /// stops early only when the page fills up.
    #[test]
    fn search_without_literals_scans_in_order_and_reports_early_stop() {
        let (_tmp, store, segments) = corpus();
        let all = search(
            &store,
            &segments,
            ".",
            false,
            &SearchFilters::default(),
            0,
            10,
        )
        .unwrap();
        assert_eq!(all.hits.len(), 4);
        assert!(!all.truncated);
        let ts: Vec<i64> = all.hits.iter().map(|h| h.ts.unwrap()).collect();
        assert_eq!(ts, vec![TS + 60_000, TS + 60_000, TS, TS]);
        let page = search(
            &store,
            &segments,
            ".",
            false,
            &SearchFilters::default(),
            0,
            3,
        )
        .unwrap();
        assert_eq!(page.hits.len(), 3);
        assert!(page.truncated);
    }

    /// `--human-only` drops harness-injected user turns and nothing else:
    /// assistant and tool turns stay, because `ask`'s lexical channel sets
    /// the filter to skip boilerplate while still ranking the discussion.
    /// Human text alone takes `--role user` on top. Both the candidate-fetch
    /// path (a literal pattern) and the ordered scan (`.`) apply it.
    #[test]
    fn human_only_should_drop_injected_user_turns_and_keep_assistant_and_tool_turns() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = RecallStore::open(&tmp.path().join("recall.db")).unwrap();
        add_session_with_intents(
            &mut store,
            "claude",
            "cccc3333",
            &[
                (Role::User, Some(IntentSource::Human), "human needle"),
                (
                    Role::User,
                    Some(IntentSource::Orchestrator),
                    "injected needle",
                ),
                (Role::Assistant, None, "assistant needle"),
                (Role::Tool, None, "tool needle"),
            ],
        );
        let mut segments = SegmentSet::open(&tmp.path().join("segments")).unwrap();
        segments.index_new(&store).unwrap();
        let texts = |pattern: &str, filters: &SearchFilters| -> Vec<String> {
            let mut texts: Vec<String> = search(&store, &segments, pattern, false, filters, 0, 10)
                .unwrap()
                .hits
                .into_iter()
                .map(|h| h.snippet)
                .collect();
            texts.sort_unstable();
            texts
        };
        let human_only = SearchFilters {
            human_only: true,
            ..SearchFilters::default()
        };
        let human_user = SearchFilters {
            human_only: true,
            role: Some("user".to_string()),
            ..SearchFilters::default()
        };
        for pattern in ["needle", "."] {
            assert_eq!(
                texts(pattern, &SearchFilters::default()).len(),
                4,
                "{pattern}: unfiltered baseline"
            );
            assert_eq!(
                texts(pattern, &human_only),
                ["assistant needle", "human needle", "tool needle"],
                "{pattern}: only the injected user turn goes"
            );
            assert_eq!(
                texts(pattern, &human_user),
                ["human needle"],
                "{pattern}: --role user narrows to human text"
            );
        }
    }

    #[test]
    fn format_hit_is_one_line_with_agent_session_seq_time_cwd_role_and_snippet() {
        let hit = SearchHit {
            turn_id: 9,
            session_id: 3,
            seq: 2,
            agent: "claude".to_string(),
            source_session_id: "abcdef0123456789".to_string(),
            cwd: Some("/work/pixel".to_string()),
            role: "assistant".to_string(),
            ts: Some(TS),
            ts_source: "iso".to_string(),
            snippet: "the needle".to_string(),
            snippet_truncated: false,
            turn_truncated: false,
        };
        assert_eq!(
            format_hit(&hit),
            format!(
                "claude:abcdef01 #3 t2 {} /work/pixel assistant \"the needle\"",
                crate::model::format_ms(TS)
            )
        );
        let bare = SearchHit {
            ts: None,
            cwd: None,
            source_session_id: "ab".to_string(),
            ..hit
        };
        assert_eq!(
            format_hit(&bare),
            "claude:ab #3 t2 ? - assistant \"the needle\""
        );
    }

    #[test]
    fn over_sort_ceiling_and_newer_turns_fit_at_their_edges() {
        assert!(
            !over_sort_ceiling(20_000, AGENT_SORT_MAX_TURNS),
            "at the ceiling: sort"
        );
        assert!(over_sort_ceiling(20_001, AGENT_SORT_MAX_TURNS));
        assert!(!over_sort_ceiling(0, AGENT_SORT_MAX_TURNS));
        assert!(newer_turns_fit(0, 5));
        assert!(
            newer_turns_fit(5, 5),
            "as many newer turns as the agent's: still walk"
        );
        assert!(!newer_turns_fit(6, 5));
    }

    #[test]
    fn session_span_sums_the_turns_and_takes_the_newest_of_the_kept_sessions() {
        let (_tmp, store, _segments) = corpus();
        let span = |agent: Option<&str>, repo: Option<&str>| {
            let filters = SearchFilters {
                agent: agent.map(ToString::to_string),
                repo_prefix: repo.map(ToString::to_string),
                ..SearchFilters::default()
            };
            session_span(&store, &filters).unwrap()
        };
        assert_eq!(span(Some("claude"), None), (2, Some(TS + 60_000)));
        assert_eq!(span(Some("codex"), Some("/work/")), (2, Some(TS + 60_000)));
        assert_eq!(span(Some("codex"), Some("/elsewhere")), (0, None));
        assert_eq!(span(Some("nobody"), None), (0, None));
        assert_eq!(span(None, None), (4, Some(TS + 60_000)));
    }

    #[test]
    fn turns_after_counts_newer_timestamped_turns_up_to_the_cap() {
        let (_tmp, store, _segments) = corpus();
        assert_eq!(turns_after(&store, Some(TS), 100).unwrap(), 2);
        assert_eq!(turns_after(&store, Some(TS + 60_000), 100).unwrap(), 0);
        assert_eq!(
            turns_after(&store, None, 100).unwrap(),
            4,
            "no activity: every turn is newer"
        );
        assert_eq!(
            turns_after(&store, Some(TS), 1).unwrap(),
            1,
            "stops at the cap"
        );
    }

    fn query_plan(store: &RecallStore, filters: &SearchFilters, sort_max: i64) -> String {
        let (sql, args) = ordered_scan_query(store, filters, sort_max).unwrap();
        let mut stmt = store
            .connection()
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        let rows = stmt
            .query_map(params_from_iter(args.iter().map(AsRef::as_ref)), |r| {
                r.get::<_, String>(3)
            })
            .unwrap();
        rows.map(Result::unwrap).collect::<Vec<_>>().join(" | ")
    }

    fn scanned_ids(store: &RecallStore, filters: &SearchFilters, sort_max: i64) -> Vec<i64> {
        let (sql, args) = ordered_scan_query(store, filters, sort_max).unwrap();
        let mut stmt = store.connection().prepare(&sql).unwrap();
        let rows = stmt
            .query_map(params_from_iter(args.iter().map(AsRef::as_ref)), |r| {
                r.get::<_, i64>(0)
            })
            .unwrap();
        rows.map(Result::unwrap).collect()
    }

    /// An agent whose turns all sit behind more newer turns of another agent
    /// sorts even above the ceiling: a walk would pass every newer turn
    /// before its first match.
    #[test]
    fn ordered_scan_sorts_an_agent_buried_under_newer_turns_of_another() {
        let (_tmp, mut store, _segments) = corpus();
        let late = add_session(
            &mut store,
            "pi",
            "dddd4444",
            &[
                (Role::User, "later one"),
                (Role::Assistant, "later two"),
                (Role::User, "later three"),
            ],
        );
        store
            .connection()
            .execute(
                "UPDATE turns SET ts = ts + 3600000 WHERE session_id = ?1",
                [late],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "UPDATE sessions SET ts_last = ts_last + 3600000 WHERE id = ?1",
                [late],
            )
            .unwrap();
        let claude = SearchFilters {
            agent: Some("claude".to_string()),
            ..SearchFilters::default()
        };
        let buried = query_plan(&store, &claude, 1);
        assert!(buried.contains("idx_sessions_agent_ts"), "{buried}");
        let pi = SearchFilters {
            agent: Some("pi".to_string()),
            ..SearchFilters::default()
        };
        let newest = query_plan(&store, &pi, 1);
        assert!(newest.contains("idx_turns_ts"), "{newest}");
        assert!(!newest.contains("TEMP B-TREE"), "{newest}");
    }

    /// An agent with more turns than the ceiling walks the ts index, so the
    /// first page comes without sorting every turn of the agent; one under
    /// it starts from its sessions. Both plans return the same rows in the
    /// same order, and a session filter keeps its own index either way.
    #[test]
    fn ordered_scan_walks_the_ts_index_for_a_large_agent_and_sorts_a_small_one() {
        let (_tmp, store, _segments) = corpus();
        let claude = SearchFilters {
            agent: Some("claude".to_string()),
            ..SearchFilters::default()
        };
        let walk = query_plan(&store, &claude, 1);
        assert!(walk.contains("idx_turns_ts"), "{walk}");
        assert!(!walk.contains("TEMP B-TREE"), "{walk}");
        let sort = query_plan(&store, &claude, 2);
        assert!(sort.contains("idx_sessions_agent_ts"), "{sort}");
        assert!(sort.contains("TEMP B-TREE FOR ORDER BY"), "{sort}");
        assert_eq!(
            scanned_ids(&store, &claude, 1),
            scanned_ids(&store, &claude, 2)
        );
        assert_eq!(scanned_ids(&store, &claude, 1), vec![2, 1]);

        let session = SearchFilters {
            session_id: Some(2),
            ..claude.clone()
        };
        let one_session = query_plan(&store, &session, 1);
        assert!(one_session.contains("idx_turns_session"), "{one_session}");
        assert_eq!(scanned_ids(&store, &session, 1), Vec::<i64>::new());
    }
}
