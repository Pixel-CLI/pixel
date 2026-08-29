//! Engine 1 — `resolve "<phrase>"` cascade.
//!
//! Each tier short-circuits with explicit confidence:
//! - **T0 exact-unique**: `WHERE norm = ?` — one row → `resolved` (the
//!   copy-pasted-label case is literally one index probe); 2–15 rows → ranked.
//! - **T1 kind-directed**: strip article, map head noun to a [`ConceptKind`],
//!   match remaining tokens in `concept_words` restricted to that kind + symbol
//!   names.
//! - **T2 word intersection** all kinds (AND, degrade to OR).
//! - **T3 trigram fallback** (verified matches, low confidence).
//! - Miss → `unresolved` with the tiers attempted (honest signal that real
//!   search/LLM is warranted).
//!
//! Ranked candidates use Engine 3's shared reranker via a pluggable
//! [`Reranker`]. pixel-graph cannot depend on pixel-rank (pixel-rank depends
//! on pixel-graph), so the daemon adapts `pixel_rank::rerank::rerank` into the
//! trait; the default is a deterministic lexical fallback.

use std::collections::HashMap;

use rusqlite::params;
use serde::Serialize;
use xxhash_rust::xxh3::xxh3_64;

use crate::concept::{ConceptKind, concept_words, normalize};
use crate::store::{ConceptRow, GraphStore, StoreError};

// ---------------------------------------------------------------------------
// response structs
// ---------------------------------------------------------------------------

/// The confidence of a resolve outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// T0 exact-unique (or T1 unique) — one definitive hit.
    Resolved,
    /// 2–15 T0 rows, or any ranked tier — ordered candidates.
    Ranked,
    /// No tier produced a verified match.
    Unresolved,
}

/// The tier that produced the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    T0,
    T1,
    T2,
    T3,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::T0 => "T0",
            Tier::T1 => "T1",
            Tier::T2 => "T2",
            Tier::T3 => "T3",
        }
    }
}

/// One resolved concept match.
#[derive(Debug, Clone, Serialize)]
pub struct ConceptMatch {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub kind: ConceptKind,
    pub raw: String,
    pub norm: String,
    /// Owner symbol name (smallest enclosing symbol), if any.
    pub owner: Option<String>,
    pub score: f64,
    pub reasons: Vec<String>,
}

/// The index state carried on every response (honesty header).
#[derive(Debug, Clone, Serialize)]
pub struct IndexState {
    pub concepts: u64,
    pub concepts_version: Option<String>,
    pub fresh: bool,
}

/// The full resolve outcome.
#[derive(Debug, Clone, Serialize)]
pub struct ResolveOutcome {
    pub confidence: Confidence,
    pub tier: Option<Tier>,
    pub matches: Vec<ConceptMatch>,
    pub inputs_digest: u64,
    pub index_state: IndexState,
    /// Tiers attempted, in order (for `unresolved` honesty).
    pub tiers_attempted: Vec<Tier>,
}

// ---------------------------------------------------------------------------
// reranker pluggable point
// ---------------------------------------------------------------------------

/// One candidate as produced by the cascade before reranking (mirrors
/// `pixel_rank::rerank::RankedCandidate`).
#[derive(Debug, Clone)]
pub struct RankedCandidate {
    pub path: String,
    pub rrf_score: f64,
    pub tier: String,
}

/// Per-path rerank signals (mirrors `pixel_rank::signals::SignalBundle`).
#[derive(Debug, Clone, Default)]
pub struct SignalBundle {
    pub activity: HashMap<String, f64>,
    pub session: HashMap<String, f64>,
    pub session_reasons: Vec<String>,
    pub error_reasons: Vec<String>,
}

/// The pluggable reranker. pixel-graph cannot depend on pixel-rank (circular),
/// so the daemon adapts `pixel_rank::rerank::rerank` into this trait; the
/// default [`LexicalReranker`] is a deterministic lexical fallback.
pub trait Reranker {
    fn rerank(
        &self,
        candidates: Vec<RankedCandidate>,
        signals: &SignalBundle,
    ) -> Vec<RankedCandidate>;
    fn clone_box(&self) -> Box<dyn Reranker>;
}

/// Deterministic lexical fallback: sort by score desc, then path asc. Used
/// when no Engine-3 reranker is supplied.
#[derive(Clone)]
pub struct LexicalReranker;

impl Reranker for LexicalReranker {
    fn rerank(
        &self,
        mut candidates: Vec<RankedCandidate>,
        _signals: &SignalBundle,
    ) -> Vec<RankedCandidate> {
        candidates.sort_by(|a, b| {
            b.rrf_score
                .total_cmp(&a.rrf_score)
                .then(a.path.cmp(&b.path))
        });
        candidates
    }

    fn clone_box(&self) -> Box<dyn Reranker> {
        Box::new(self.clone())
    }
}

// ---------------------------------------------------------------------------
// options
// ---------------------------------------------------------------------------

/// Options for a resolve call.
#[derive(Clone)]
pub struct ResolveOptions {
    /// Max matches returned (default 8).
    pub limit: usize,
    /// Optional Engine-3 reranker; defaults to [`LexicalReranker`].
    pub reranker: Option<Box<dyn Reranker>>,
    /// Optional per-path signals for the reranker.
    pub signals: SignalBundle,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        ResolveOptions {
            limit: 8,
            reranker: None,
            signals: SignalBundle::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// head-noun → kind mapping (T1)
// ---------------------------------------------------------------------------

const ARTICLES: &[&str] = &["the", "a", "an"];

/// Map a head noun to the concept kind(s) it implies. Returns empty when the
/// noun carries no kind signal.
fn kind_for_head_noun(noun: &str) -> Vec<ConceptKind> {
    match noun {
        "form" => vec![ConceptKind::Form],
        "button" | "label" | "toast" | "input" | "field" => {
            vec![ConceptKind::UiText, ConceptKind::AttrText]
        }
        "endpoint" | "route" | "api" | "url" => vec![ConceptKind::Route],
        "component" | "modal" | "page" | "screen" => vec![ConceptKind::Component],
        "error" | "exception" => vec![ConceptKind::String, ConceptKind::Status],
        _ => Vec::new(),
    }
}

/// True when `word` is a 3-digit HTTP status code (400–599).
fn is_status_code(word: &str) -> bool {
    if word.len() != 3 {
        return false;
    }
    word.chars().all(|c| c.is_ascii_digit())
        && word
            .parse::<i64>()
            .map(|n| (400..=599).contains(&n))
            .unwrap_or(false)
}

/// Split a phrase into significant tokens (lowercased, len ≥ 2, articles
/// stripped). Returns the tokens and the head noun (last significant token).
fn phrase_tokens(phrase: &str) -> (Vec<String>, Option<String>) {
    let norm = normalize(phrase);
    let mut tokens: Vec<String> = norm
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2)
        .map(|w| w.to_lowercase())
        .filter(|w| !ARTICLES.contains(&w.as_str()))
        .collect();
    tokens.dedup();
    let head = tokens.last().cloned();
    (tokens, head)
}

// ---------------------------------------------------------------------------
// the cascade
// ---------------------------------------------------------------------------

/// Resolve a phrase against the concept index. `store` must be open; the
/// cascade degrades gracefully to `unresolved` on any store error.
pub fn resolve(
    store: &GraphStore,
    phrase: &str,
    opts: &ResolveOptions,
) -> Result<ResolveOutcome, StoreError> {
    let limit = opts.limit.max(1);
    let norm = normalize(phrase);
    let mut tiers_attempted: Vec<Tier> = Vec::new();

    // T0: exact-norm probe.
    if !norm.is_empty() {
        tiers_attempted.push(Tier::T0);
        let exact = store.concepts_by_norm(&norm, 16)?;
        if !exact.is_empty() {
            let (confidence, tier) = if exact.len() == 1 {
                (Confidence::Resolved, Tier::T0)
            } else {
                (Confidence::Ranked, Tier::T0)
            };
            return finish(store, phrase, exact, confidence, tier, opts, tiers_attempted);
        }
    }

    // T1: kind-directed.
    let (tokens, head) = phrase_tokens(phrase);
    if !tokens.is_empty() {
        tiers_attempted.push(Tier::T1);
        let mut t1_rows: Vec<ConceptRow> = Vec::new();
        if let Some(h) = &head
            && is_status_code(h)
        {
            let word_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
            t1_rows.extend(
                store.concepts_by_kind_words(ConceptKind::Status, &word_refs, limit as u32)?,
            );
        } else if let Some(h) = &head {
            let word_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
            for kind in kind_for_head_noun(h) {
                t1_rows.extend(store.concepts_by_kind_words(kind, &word_refs, limit as u32)?);
            }
        }
        if !t1_rows.is_empty() {
            return finish(
                store,
                phrase,
                t1_rows,
                Confidence::Ranked,
                Tier::T1,
                opts,
                tiers_attempted,
            );
        }
    }

    // T2: word intersection, all kinds (AND, degrade to OR).
    if !tokens.is_empty() {
        tiers_attempted.push(Tier::T2);
        let word_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
        let and = store.concepts_by_words(&word_refs, None, limit as u32)?;
        let rows = if and.is_empty() {
            store.concepts_by_any_word(&word_refs, None, limit as u32)?
        } else {
            and
        };
        if !rows.is_empty() {
            return finish(
                store,
                phrase,
                rows,
                Confidence::Ranked,
                Tier::T2,
                opts,
                tiers_attempted,
            );
        }
    }

    // T3: trigram fallback (substring LIKE, low confidence).
    if !norm.is_empty() {
        tiers_attempted.push(Tier::T3);
        let rows = store.concepts_like(&norm, limit as u32)?;
        if !rows.is_empty() {
            return finish(
                store,
                phrase,
                rows,
                Confidence::Ranked,
                Tier::T3,
                opts,
                tiers_attempted,
            );
        }
    }

    // Miss.
    let index_state = index_state(store)?;
    Ok(ResolveOutcome {
        confidence: Confidence::Unresolved,
        tier: None,
        matches: Vec::new(),
        inputs_digest: inputs_digest(phrase, &index_state),
        index_state,
        tiers_attempted,
    })
}

/// Build the final outcome from a set of candidate rows: attach path/owner,
/// score, reasons, rerank, and cap to `limit`.
fn finish(
    store: &GraphStore,
    phrase: &str,
    rows: Vec<ConceptRow>,
    confidence: Confidence,
    tier: Tier,
    opts: &ResolveOptions,
    tiers_attempted: Vec<Tier>,
) -> Result<ResolveOutcome, StoreError> {
    let limit = opts.limit.max(1);
    let mut matches: Vec<ConceptMatch> = Vec::with_capacity(rows.len());
    for row in rows {
        let path = file_path(store, row.file_id)?;
        let owner = match row.owner_symbol_id {
            Some(id) => symbol_name(store, id)?,
            None => None,
        };
        let reasons = match_reasons(&row, phrase);
        matches.push(ConceptMatch {
            path,
            start_line: row.start_line,
            end_line: row.end_line,
            kind: row.kind,
            raw: row.raw,
            norm: row.norm,
            owner,
            score: 1.0,
            reasons,
        });
    }

    // Rerank within the tier via the pluggable reranker.
    let candidates: Vec<RankedCandidate> = matches
        .iter()
        .enumerate()
        .map(|(i, m)| RankedCandidate {
            path: m.path.clone(),
            rrf_score: 1.0 / (i as f64 + 1.0),
            tier: tier.as_str().to_string(),
        })
        .collect();
    let reranker: &dyn Reranker = opts
        .reranker
        .as_deref()
        .unwrap_or(&LexicalReranker);
    let reordered = reranker.rerank(candidates, &opts.signals);
    let by_path: HashMap<&str, ConceptMatch> = matches
        .iter()
        .map(|m| (m.path.as_str(), m.clone()))
        .collect();
    let mut ordered: Vec<ConceptMatch> = reordered
        .into_iter()
        .filter_map(|c| by_path.get(c.path.as_str()).cloned())
        .collect();
    ordered.truncate(limit);

    let index_state = index_state(store)?;
    Ok(ResolveOutcome {
        confidence,
        tier: Some(tier),
        matches: ordered,
        inputs_digest: inputs_digest(phrase, &index_state),
        index_state,
        tiers_attempted,
    })
}

/// Human-readable reasons for a match, based on how its norm relates to the
/// phrase.
fn match_reasons(row: &ConceptRow, phrase: &str) -> Vec<String> {
    let mut reasons = Vec::new();
    let norm = normalize(phrase);
    if !norm.is_empty() && row.norm == norm {
        reasons.push("exact norm match".to_string());
    } else {
        let words = concept_words(&row.norm);
        let qwords = concept_words(&norm);
        let overlap: Vec<&str> = qwords
            .iter()
            .filter(|w| words.contains(w))
            .map(|w| w.as_str())
            .collect();
        if !overlap.is_empty() {
            reasons.push(format!("word overlap: {}", overlap.join(", ")));
        }
        if !norm.is_empty() && row.norm.contains(&norm) {
            reasons.push("substring match".to_string());
        }
    }
    if reasons.is_empty() {
        reasons.push(format!("kind {}", row.kind.as_str()));
    }
    reasons
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn file_path(store: &GraphStore, file_id: i64) -> Result<String, StoreError> {
    Ok(store
        .conn()
        .query_row(
            "SELECT path FROM files WHERE id = ?1",
            params![file_id],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default())
}

fn symbol_name(store: &GraphStore, symbol_id: i64) -> Result<Option<String>, StoreError> {
    Ok(store
        .conn()
        .query_row(
            "SELECT name FROM symbols WHERE id = ?1",
            params![symbol_id],
            |r| r.get::<_, String>(0),
        )
        .ok())
}

fn index_state(store: &GraphStore) -> Result<IndexState, StoreError> {
    let concepts = store.concept_count()?;
    let concepts_version = store.concepts_version()?;
    Ok(IndexState {
        concepts,
        concepts_version,
        fresh: concepts > 0,
    })
}

/// `xxh3(phrase ‖ concepts_version ‖ concept_count)` — the digest every
/// resolve response carries so a caller can detect when the underlying index
/// changed.
fn inputs_digest(phrase: &str, state: &IndexState) -> u64 {
    let mut buf = Vec::new();
    buf.extend_from_slice(phrase.as_bytes());
    buf.push(0);
    if let Some(v) = &state.concepts_version {
        buf.extend_from_slice(v.as_bytes());
    }
    buf.push(0);
    buf.extend_from_slice(&state.concepts.to_le_bytes());
    xxh3_64(&buf)
}

// Allow cloning a boxed Reranker (needed to avoid borrowing opts while
// holding the result).
impl Clone for Box<dyn Reranker> {
    fn clone(&self) -> Box<dyn Reranker> {
        self.clone_box()
    }
}
