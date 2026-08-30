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
//! - **Symbol fallback** — no concept matched, but a symbol's ident words
//!   overlap the query (e.g. "checkout page" → `CheckoutPage`).
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
use crate::store::{ConceptRow, GraphStore, StoreError, SymbolKind, SymbolRow};

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
    /// Symbol fallback: no concept matched, but a symbol's ident words
    /// overlap the query. Emitted as `tier: "symbol"`.
    Symbol,
    /// Identifier tier: the query is a single identifier-shaped token (no
    /// spaces, e.g. `GUARD_MATCHER`, `CheckoutPage`) and an exact symbol name
    /// match was found. This runs BEFORE the concept cascade so that code
    /// definitions rank above string concepts that merely mention the
    /// identifier in test fixtures or command strings. Emitted as
    /// `tier: "ident"`.
    Ident,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::T0 => "T0",
            Tier::T1 => "T1",
            Tier::T2 => "T2",
            Tier::T3 => "T3",
            Tier::Symbol => "symbol",
            Tier::Ident => "ident",
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
    /// The symbol kind when this match came from the symbol fallback tier
    /// (`Some("function")`, `Some("class")`, …); `None` for concept matches.
    pub symbol_kind: Option<String>,
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
    /// True when a bounded table scan (T3 trigram / symbol fallback) hit its
    /// row cap: rows beyond the cap were never considered, so this outcome
    /// is a lower bound, not a closed-world answer. Always `false` for the
    /// indexed tiers (T0/T1/T2/ident), which probe complete indexes.
    pub scan_capped: bool,
    /// Human-readable provenance: which tier produced the answer and which
    /// caps (if any) bounded it. Empty only for `unresolved` with no capped
    /// scans.
    pub basis: String,
}

// ---------------------------------------------------------------------------
// reranker pluggable point
// ---------------------------------------------------------------------------

/// One candidate as produced by the cascade before reranking (mirrors
/// `pixel_rank::rerank::RankedCandidate`).
///
/// `id` is a stable unique key for the candidate (the concept/symbol row id
/// cast to `u64`). The daemon adapter (Phase 1c) must preserve it through the
/// `pixel_rank::rerank::rerank` round-trip so the local rebuild can look
/// matches back up by id — this is what keeps same-file concepts from being
/// collapsed to one-per-path.
#[derive(Debug, Clone)]
pub struct RankedCandidate {
    pub id: u64,
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

/// True when the normalized form is a single identifier-shaped token: no
/// spaces, at least 2 chars, and composed of alphanumeric + underscore
/// characters only. This distinguishes code identifiers (`guard_matcher`,
/// `checkoutpage`) from natural-language phrases (`submit the form`,
/// `the 503 error`) which contain spaces after normalization.
fn is_identifier_shaped(norm: &str) -> bool {
    if norm.len() < 2 || norm.contains(' ') {
        return false;
    }
    norm.chars()
        .all(|c| c.is_alphanumeric() || c == '_')
}

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

/// True when `word` is a 3-digit HTTP status code (100–599 — the same range
/// the extractor accepts; PLAN.md Engine 1 does not restrict this to
/// client/server error codes only, and neither does `push_status`/
/// `push_res_status`/`push_abort_status`, so narrowing it here would make
/// "the 204 response" or "301 redirect" unresolvable even though the concept
/// itself was correctly extracted).
fn is_status_code(word: &str) -> bool {
    if word.len() != 3 {
        return false;
    }
    word.chars().all(|c| c.is_ascii_digit())
        && word
            .parse::<i64>()
            .map(|n| (100..=599).contains(&n))
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
    // Caps fired by bounded scans along the way — carried into the outcome
    // even on a miss, because "nothing found in the first 20k rows" is a
    // weaker claim than "nothing found".
    let mut t3_capped = false;
    let mut symbol_capped = false;

    // Ident tier: when the query is a single identifier-shaped token (no
    // spaces after normalization — e.g. `GUARD_MATCHER`, `CheckoutPage`,
    // `useForm`), try an exact symbol name lookup BEFORE the concept
    // cascade. This prevents string concepts that merely mention the
    // identifier (test fixtures, command strings) from masking the real
    // code definition. Natural-language phrases ("submit the form", "the
    // 503 error") have spaces in their normalized form and skip this tier.
    if is_identifier_shaped(&norm) {
        tiers_attempted.push(Tier::Ident);
        // Try the exact original phrase first (symbol names are
        // case-sensitive in the DB).
        let mut syms = store.symbols_by_name(phrase, limit as u32)?;
        // If no exact-case hit, try the normalized (lowercased) form —
        // handles lowercase queries like "guard_matcher".
        if syms.is_empty() {
            syms = store.symbols_by_name(&norm, limit as u32)?;
        }
        if !syms.is_empty() {
            return finish_symbols(store, phrase, syms, opts, tiers_attempted, Tier::Ident, false);
        }
    }

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
            return finish(store, phrase, exact, confidence, tier, opts, tiers_attempted, false);
        }
    }

    // T1: kind-directed.
    let (tokens, head) = phrase_tokens(phrase);
    if !tokens.is_empty() {
        tiers_attempted.push(Tier::T1);
        let mut t1_rows: Vec<ConceptRow> = Vec::new();
        // "Match remaining tokens" per PLAN.md: the head noun is a
        // classifier word ("button", "endpoint", "error") that is not
        // expected to literally appear in the target concept's own text, so
        // it must be stripped before the word-intersection query below —
        // leaving it in made T1 require e.g. a UI text to literally contain
        // the word "button" for "submit button" to match, which it almost
        // never does, silently degrading nearly every multi-word phrase to
        // T2/T3. Falls back to the full token set for a bare single-word
        // phrase like "form", where the head noun IS the content to match.
        let remaining: Vec<&str> = if tokens.len() > 1 {
            tokens[..tokens.len() - 1]
                .iter()
                .map(String::as_str)
                .collect()
        } else {
            tokens.iter().map(String::as_str).collect()
        };
        if let Some(h) = &head
            && is_status_code(h)
        {
            // The status code digits ARE the content to match; other words
            // ("error", "the") are noise a status concept's norm never
            // contains (its norm is just the bare digits), so search on the
            // code alone rather than on `remaining`.
            let word_refs = [h.as_str()];
            t1_rows.extend(
                store.concepts_by_kind_words(ConceptKind::Status, &word_refs, limit as u32)?,
            );
        } else if let Some(h) = &head {
            for kind in kind_for_head_noun(h) {
                t1_rows.extend(store.concepts_by_kind_words(kind, &remaining, limit as u32)?);
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
                false,
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
                false,
            );
        }
    }

    // T3: trigram fallback (verified matches via real character-trigram
    // overlap, low confidence).
    if !norm.is_empty() {
        tiers_attempted.push(Tier::T3);
        let (rows, capped) = trigram_fallback(store, &norm, limit as u32)?;
        t3_capped = capped;
        if !rows.is_empty() {
            return finish(
                store,
                phrase,
                rows,
                Confidence::Ranked,
                Tier::T3,
                opts,
                tiers_attempted,
                capped,
            );
        }
    }

    // Symbol fallback: no concept matched, but a symbol's ident words overlap
    // the query's ident words (e.g. "checkout page" → `CheckoutPage`). This
    // is the last tier before an honest `unresolved`.
    if !tokens.is_empty() {
        tiers_attempted.push(Tier::Symbol);
        // Match on the query's camelCase-split ident words (e.g. "handleLogin"
        // → ["handle", "login"]) so a single camelCase query can hit a symbol.
        let ident_words = symbol_words(phrase);
        let (symbols, capped) = symbol_fallback(store, &ident_words, limit as u32)?;
        symbol_capped = capped;
        if !symbols.is_empty() {
            return finish_symbols(
                store,
                phrase,
                symbols,
                opts,
                tiers_attempted,
                Tier::Symbol,
                capped,
            );
        }
    }

    // Miss. A miss after capped scans is a weaker claim than a clean miss:
    // rows beyond the scan cap were never considered.
    let scan_capped = t3_capped || symbol_capped;
    let index_state = index_state(store)?;
    Ok(ResolveOutcome {
        confidence: Confidence::Unresolved,
        tier: None,
        matches: Vec::new(),
        inputs_digest: inputs_digest(phrase, &index_state),
        index_state,
        tiers_attempted,
        scan_capped,
        basis: if scan_capped {
            format!(
                "no tier matched, but fallback scans were capped at {TRIGRAM_SCAN_CAP} rows — \
                 unscanned rows may contain a match"
            )
        } else {
            "no tier matched; all attempted tiers were scanned to completion".to_string()
        },
    })
}

/// Build the final outcome from a set of candidate rows: attach path/owner,
/// score, reasons, rerank, and cap to `limit`.
///
/// Rerank is keyed by candidate `id` (the concept row id), not by path, so
/// multiple concepts in the same file are never collapsed to one-per-path.
#[allow(clippy::too_many_arguments)]
fn finish(
    store: &GraphStore,
    phrase: &str,
    rows: Vec<ConceptRow>,
    confidence: Confidence,
    tier: Tier,
    opts: &ResolveOptions,
    tiers_attempted: Vec<Tier>,
    scan_capped: bool,
) -> Result<ResolveOutcome, StoreError> {
    let limit = opts.limit.max(1);
    let mut candidates: Vec<RankedCandidate> = Vec::with_capacity(rows.len());
    let mut by_id: HashMap<u64, ConceptMatch> = HashMap::with_capacity(rows.len());
    for (i, row) in rows.into_iter().enumerate() {
        let id = row.id as u64;
        let path = file_path(store, row.file_id)?;
        let owner = match row.owner_symbol_id {
            Some(sid) => symbol_name(store, sid)?,
            None => None,
        };
        let reasons = match_reasons(&row, phrase);
        let score = score_match(&row, phrase, owner.as_deref(), &path);
        let m = ConceptMatch {
            path: path.clone(),
            start_line: row.start_line,
            end_line: row.end_line,
            kind: row.kind,
            raw: row.raw,
            norm: row.norm,
            owner,
            symbol_kind: None,
            score,
            reasons,
        };
        by_id.insert(id, m.clone());
        candidates.push(RankedCandidate {
            id,
            path,
            rrf_score: 1.0 / (i as f64 + 1.0),
            tier: tier.as_str().to_string(),
        });
    }

    // Rerank within the tier via the pluggable reranker.
    let reranker: &dyn Reranker = opts
        .reranker
        .as_deref()
        .unwrap_or(&LexicalReranker);
    let reordered = reranker.rerank(candidates, &opts.signals);
    let mut ordered: Vec<ConceptMatch> = reordered
        .into_iter()
        .filter_map(|c| by_id.get(&c.id).cloned())
        .collect();
    ordered.truncate(limit);

    let index_state = index_state(store)?;
    let basis = if scan_capped {
        format!(
            "tier {} (concept index); scan capped at {TRIGRAM_SCAN_CAP} rows — unscanned rows \
             may contain better matches",
            tier.as_str()
        )
    } else {
        format!("tier {} (concept index, scanned to completion)", tier.as_str())
    };
    Ok(ResolveOutcome {
        confidence,
        tier: Some(tier),
        matches: ordered,
        inputs_digest: inputs_digest(phrase, &index_state),
        index_state,
        tiers_attempted,
        scan_capped,
        basis,
    })
}

/// Build the final outcome for the symbol fallback tier. Each `SymbolRow`
/// becomes a [`ConceptMatch`] carrying its real symbol kind in `symbol_kind`
/// and a best-effort [`ConceptKind`] in `kind` (see [`symbol_kind_to_concept`]).
/// `tier` is the tier that produced these matches (`Tier::Symbol` for the
/// fallback cascade, `Tier::Ident` for the identifier-exact-match tier).
fn finish_symbols(
    store: &GraphStore,
    phrase: &str,
    rows: Vec<SymbolRow>,
    opts: &ResolveOptions,
    tiers_attempted: Vec<Tier>,
    tier: Tier,
    scan_capped: bool,
) -> Result<ResolveOutcome, StoreError> {
    let limit = opts.limit.max(1);
    // The symbol tiers must be able to express their best case: a single
    // exact match is `resolved`, not `ranked` (a hardcoded `Ranked` here
    // previously made `resolved` unreachable for identifier queries).
    // - Ident tier: the query already matched a symbol NAME exactly; one row
    //   means one definitive definition → Resolved.
    // - Symbol fallback: matches are fuzzy word-overlap, so a single row is
    //   Resolved only when its ident words are exactly the query's ident
    //   words (e.g. "checkout page" → `CheckoutPage`), never on partial
    //   overlap. A capped scan can never claim Resolved: unscanned rows may
    //   hold an equally-exact competitor.
    let confidence = if rows.len() == 1 && !scan_capped {
        let exact_words = {
            let mut q = symbol_words(phrase);
            let mut n = symbol_words(&rows[0].name);
            q.sort();
            n.sort();
            q == n
        };
        if tier == Tier::Ident || exact_words {
            Confidence::Resolved
        } else {
            Confidence::Ranked
        }
    } else {
        Confidence::Ranked
    };
    let mut candidates: Vec<RankedCandidate> = Vec::with_capacity(rows.len());
    let mut by_id: HashMap<u64, ConceptMatch> = HashMap::with_capacity(rows.len());
    let reason = if tier == Tier::Ident {
        "exact symbol name match"
    } else {
        "symbol fallback"
    };
    for (i, row) in rows.into_iter().enumerate() {
        let id = row.id as u64;
        let path = file_path(store, row.file_id)?;
        let score = score_symbol(&row, phrase, &path);
        let m = ConceptMatch {
            path: path.clone(),
            start_line: row.start_line,
            end_line: row.end_line,
            kind: symbol_kind_to_concept(row.kind),
            raw: row.name.clone(),
            norm: normalize(&row.name),
            owner: None,
            symbol_kind: Some(row.kind.as_str().to_string()),
            score,
            reasons: vec![reason.to_string()],
        };
        by_id.insert(id, m.clone());
        candidates.push(RankedCandidate {
            id,
            path,
            rrf_score: 1.0 / (i as f64 + 1.0),
            tier: tier.as_str().to_string(),
        });
    }

    let reranker: &dyn Reranker = opts
        .reranker
        .as_deref()
        .unwrap_or(&LexicalReranker);
    let reordered = reranker.rerank(candidates, &opts.signals);
    let mut ordered: Vec<ConceptMatch> = reordered
        .into_iter()
        .filter_map(|c| by_id.get(&c.id).cloned())
        .collect();
    ordered.truncate(limit);

    let index_state = index_state(store)?;
    let tier_desc = if tier == Tier::Ident {
        "tier ident (exact symbol-name index probe)".to_string()
    } else if scan_capped {
        format!(
            "tier symbol (fallback scan capped at {SYMBOL_SCAN_CAP} rows — unscanned symbols may \
             contain better matches)"
        )
    } else {
        "tier symbol (fallback scan, scanned to completion)".to_string()
    };
    Ok(ResolveOutcome {
        confidence,
        tier: Some(tier),
        matches: ordered,
        inputs_digest: inputs_digest(phrase, &index_state),
        index_state,
        tiers_attempted,
        scan_capped,
        basis: tier_desc,
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
// real scoring
// ---------------------------------------------------------------------------

/// Score for a candidate whose norm is byte-identical to the query's norm —
/// the strongest possible lexical evidence, so it saturates the scale.
const SCORE_EXACT_NORM: f64 = 1.0;
/// Score when the query's norm is a strict substring of the candidate's norm
/// — near-certain relevance, but weaker than identity (the candidate carries
/// extra text the user didn't say).
const SCORE_SUBSTRING: f64 = 0.8;
/// Base of the word-overlap band: any nonzero word overlap starts here, so a
/// partial match is always distinguishable from a scoreless non-match.
const SCORE_OVERLAP_BASE: f64 = 0.3;
/// Span of the word-overlap band: overlap ratio 0→1 maps to
/// `SCORE_OVERLAP_BASE..=SCORE_OVERLAP_BASE + SCORE_OVERLAP_SPAN` (0.3–0.7),
/// keeping even a full word-overlap below `SCORE_SUBSTRING` — word-bag
/// equality is weaker evidence than an in-order substring.
const SCORE_OVERLAP_SPAN: f64 = 0.4;
/// Multiplier applied when the match lives in a test path: a phrase's real
/// definition is almost always the production site, not the test that quotes
/// it, so tests are demoted but never eliminated.
const TEST_PATH_PENALTY: f64 = 0.7;
/// Additive bonus when the enclosing symbol's ident words overlap the query
/// — a concept owned by `submitButton` is better evidence for "submit
/// button" than the same string in an unrelated function. Small enough to
/// break ties without jumping a score band.
const OWNER_WORD_BONUS: f64 = 0.15;

/// Real per-match score for a concept row (see the named constants above for
/// each band's rationale). Clamped to `[0.0, 1.0]`.
fn score_match(row: &ConceptRow, phrase: &str, owner: Option<&str>, path: &str) -> f64 {
    let norm = normalize(phrase);
    let mut score = 0.0;
    if !norm.is_empty() && row.norm == norm {
        score = SCORE_EXACT_NORM;
    } else if !norm.is_empty() && row.norm.contains(&norm) {
        score = SCORE_SUBSTRING;
    } else {
        let words = concept_words(&row.norm);
        let qwords = concept_words(&norm);
        if !qwords.is_empty() {
            let overlap = qwords.iter().filter(|w| words.contains(w)).count();
            let ratio = overlap as f64 / qwords.len() as f64;
            score = SCORE_OVERLAP_BASE + ratio * SCORE_OVERLAP_SPAN;
        }
    }
    if is_test_path(path) {
        score *= TEST_PATH_PENALTY;
    }
    if let Some(owner) = owner {
        let owner_words = symbol_words(owner);
        let qwords = concept_words(&norm);
        if owner_words.iter().any(|w| qwords.contains(w)) {
            score += OWNER_WORD_BONUS;
        }
    }
    score.clamp(0.0, 1.0)
}

/// Score for a symbol-fallback match: word-overlap ratio of the query's ident
/// words against the symbol's camelCase-split name, in the word-overlap band,
/// with the same test-path penalty.
fn score_symbol(row: &SymbolRow, phrase: &str, path: &str) -> f64 {
    let qwords = symbol_words(phrase);
    let name_words = symbol_words(&row.name);
    let mut score = 0.0;
    if !qwords.is_empty() {
        let overlap = qwords.iter().filter(|w| name_words.contains(w)).count();
        let ratio = overlap as f64 / qwords.len() as f64;
        score = SCORE_OVERLAP_BASE + ratio * SCORE_OVERLAP_SPAN;
    }
    if is_test_path(path) {
        score *= TEST_PATH_PENALTY;
    }
    score.clamp(0.0, 1.0)
}

/// True when a path looks like a test file (`test`/`spec`/`__tests__`).
fn is_test_path(path: &str) -> bool {
    let p = path.to_lowercase();
    p.contains("/test/")
        || p.contains("/tests/")
        || p.contains("/__tests__/")
        || p.contains(".test.")
        || p.contains("_test.")
        || p.contains(".spec.")
        || p.contains("_spec.")
}

/// Split an identifier into lowercased words on non-alphanumeric and
/// camelCase boundaries: `ContactForm` → `["contact", "form"]`,
/// `WELCOME_MESSAGE` → `["welcome", "message"]`, `onSubmit` →
/// `["on", "submit"]`. Used to match query ident-words against symbol names
/// and owner-symbol names.
fn symbol_words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() {
            let boundary = !cur.is_empty()
                && c.is_ascii_uppercase()
                && cur.chars().last().map(|x| x.is_ascii_lowercase()).unwrap_or(false);
            if boundary {
                out.push(cur.to_lowercase());
                cur.clear();
            }
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(cur.to_lowercase());
            cur.clear();
        }
    }
    if !cur.is_empty() {
        out.push(cur.to_lowercase());
    }
    out
}

/// Best-effort [`ConceptKind`] for a symbol match's `kind` field. The real
/// kind is carried losslessly in `ConceptMatch::symbol_kind`; this mapping only
/// gives the response a non-arbitrary `kind` for consumers that read it.
fn symbol_kind_to_concept(kind: SymbolKind) -> ConceptKind {
    match kind {
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Const => ConceptKind::String,
        SymbolKind::Class
        | SymbolKind::Struct
        | SymbolKind::Enum
        | SymbolKind::Trait
        | SymbolKind::Interface
        | SymbolKind::Module => ConceptKind::Component,
    }
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

// ---------------------------------------------------------------------------
// T3 trigram fallback
// ---------------------------------------------------------------------------

/// Bound on how many concept rows a T3 scan will consider, to keep
/// worst-case cost sane on large repos. `store.concepts_like` previously did
/// a naive `LIKE '%needle%'` substring scan under the name "trigram
/// fallback" — real, but not actually trigram-based, so it could not
/// tolerate even a single typo (PLAN.md's stated purpose for this tier:
/// "fuzzier falls to the trigram index"). This scans `concepts.norm`
/// directly and scores by real character-trigram overlap instead.
///
/// This is a crate-local MVP, not the shared trigram index gitpixel/
/// pixel-index builds over raw file content: pixel-graph does not depend on
/// pixel-index (same kind of dependency constraint documented above for the
/// `Reranker` trait vs. pixel-rank), so a genuine trigram *index* belongs at
/// the daemon/pixel-index integration layer, not here. A bounded linear scan
/// with real trigram scoring is a correct, honest last-resort tier in the
/// meantime — it just doesn't scale to a huge concept table the way an
/// actual inverted trigram index would.
const TRIGRAM_SCAN_CAP: u32 = 20_000;
/// Minimum overlap coefficient to accept a T3 candidate. A query that is a
/// literal substring of the target scores 1.0 automatically (every trigram
/// of a short query survives inside a longer superstring), so this floor
/// only screens out near-unrelated norms while still tolerating a
/// misspelling or two.
const TRIGRAM_MIN_OVERLAP: f64 = 0.34;

fn trigram_set(s: &str) -> std::collections::HashSet<(char, char, char)> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = std::collections::HashSet::new();
    if chars.len() < 3 {
        return out;
    }
    for w in chars.windows(3) {
        out.insert((w[0], w[1], w[2]));
    }
    out
}

/// Overlap coefficient `|A ∩ B| / min(|A|, |B|)`, so a short query fully
/// contained in a longer target still scores 1.0 (the substring case),
/// while otherwise rewarding real character-level similarity.
fn trigram_overlap(
    a: &std::collections::HashSet<(char, char, char)>,
    b: &std::collections::HashSet<(char, char, char)>,
) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    inter as f64 / a.len().min(b.len()) as f64
}

/// T3: rank a bounded scan of concept rows by character-trigram overlap
/// against `norm`. Falls back to the plain substring scan for queries under
/// 3 chars (too short to form a single trigram, so overlap is meaningless).
///
/// The second return value is true when the scan HIT its row cap
/// ([`TRIGRAM_SCAN_CAP`]): rows beyond the cap were never considered, so the
/// result is a lower bound and the caller must surface that.
fn trigram_fallback(
    store: &GraphStore,
    norm: &str,
    limit: u32,
) -> Result<(Vec<ConceptRow>, bool), StoreError> {
    let query_grams = trigram_set(norm);
    if query_grams.is_empty() {
        // The `concepts_like` path is itself LIMIT-bounded; treat a full
        // page as a possibly-capped scan for the same honesty reason.
        let rows = store.concepts_like(norm, limit)?;
        let capped = rows.len() as u32 >= limit;
        return Ok((rows, capped));
    }
    let sql = "SELECT id, file_id, kind, raw, norm, detail, start_line, end_line, owner_symbol_id
               FROM concepts LIMIT ?1";
    let mut stmt = store.conn().prepare(sql)?;
    let mut scanned: u32 = 0;
    let mut scored: Vec<(f64, ConceptRow)> = stmt
        .query_map(params![TRIGRAM_SCAN_CAP], |r| {
            Ok(ConceptRow {
                id: r.get(0)?,
                file_id: r.get(1)?,
                kind: ConceptKind::parse(&r.get::<_, String>(2)?),
                raw: r.get(3)?,
                norm: r.get(4)?,
                detail: r.get(5)?,
                start_line: r.get(6)?,
                end_line: r.get(7)?,
                owner_symbol_id: r.get(8)?,
            })
        })?
        .filter_map(|row: rusqlite::Result<ConceptRow>| row.ok())
        .filter_map(|row| {
            scanned += 1;
            let score = trigram_overlap(&query_grams, &trigram_set(&row.norm));
            (score >= TRIGRAM_MIN_OVERLAP).then_some((score, row))
        })
        .collect();
    let capped = scanned >= TRIGRAM_SCAN_CAP;
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
    scored.truncate(limit as usize);
    Ok((scored.into_iter().map(|(_, row)| row).collect(), capped))
}

// ---------------------------------------------------------------------------
// symbol fallback tier
// ---------------------------------------------------------------------------

/// Bound on how many symbol rows the fallback scan will consider, mirroring
/// [`TRIGRAM_SCAN_CAP`].
const SYMBOL_SCAN_CAP: u32 = 20_000;

/// Symbol fallback: scan a bounded slice of the `symbols` table and keep rows
/// whose camelCase-split name shares at least one ident word with the query's
/// ident words, ranked by overlap ratio. This is the last tier before
/// `unresolved`.
/// The second return value is true when the scan HIT its row cap
/// ([`SYMBOL_SCAN_CAP`]): symbols beyond the cap were never considered, so
/// the result is a lower bound and the caller must surface that.
fn symbol_fallback(
    store: &GraphStore,
    words: &[String],
    limit: u32,
) -> Result<(Vec<SymbolRow>, bool), StoreError> {
    if words.is_empty() {
        return Ok((Vec::new(), false));
    }
    let sql = "SELECT id, uid, file_id, name, qualified, kind, start_line, end_line, sig
               FROM symbols LIMIT ?1";
    let mut stmt = store.conn().prepare(sql)?;
    let mut scanned: u32 = 0;
    let mut scored: Vec<(f64, SymbolRow)> = stmt
        .query_map(params![SYMBOL_SCAN_CAP], |r| {
            Ok(SymbolRow {
                id: r.get(0)?,
                uid: r.get(1)?,
                file_id: r.get(2)?,
                name: r.get(3)?,
                qualified: r.get(4)?,
                kind: SymbolKind::parse(&r.get::<_, String>(5)?),
                start_line: r.get(6)?,
                end_line: r.get(7)?,
                sig: r.get(8)?,
            })
        })?
        .filter_map(|row: rusqlite::Result<SymbolRow>| row.ok())
        .filter_map(|row| {
            scanned += 1;
            let name_words = symbol_words(&row.name);
            let overlap: Vec<&str> = words
                .iter()
                .filter(|t| name_words.contains(t))
                .map(|t| t.as_str())
                .collect();
            if overlap.is_empty() {
                None
            } else {
                let score = overlap.len() as f64 / words.len() as f64;
                Some((score, row))
            }
        })
        .collect();
    let capped = scanned >= SYMBOL_SCAN_CAP;
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
    scored.truncate(limit as usize);
    Ok((scored.into_iter().map(|(_, row)| row).collect(), capped))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concept::ConceptKind;

    fn store() -> GraphStore {
        GraphStore::open_in_memory().unwrap()
    }

    fn add_file(store: &mut GraphStore, path: &str) -> i64 {
        store.replace_file(path, "blob", "tsx").unwrap()
    }

    #[test]
    fn same_file_concepts_are_not_collapsed() {
        let mut store = store();
        let f1 = add_file(&mut store, "src/app.tsx");
        let f2 = add_file(&mut store, "src/app.test.tsx");
        store
            .insert_concept(f1, ConceptKind::UiText, "Submit", "submit", "", 10, 10, None)
            .unwrap();
        store
            .insert_concept(f1, ConceptKind::UiText, "Submit", "submit", "", 20, 20, None)
            .unwrap();
        store
            .insert_concept(f2, ConceptKind::UiText, "Submit", "submit", "", 5, 5, None)
            .unwrap();

        let out = resolve(&store, "submit", &ResolveOptions::default()).unwrap();
        assert_eq!(out.matches.len(), 3, "same-file concepts must not collapse");
        let same_file = out
            .matches
            .iter()
            .filter(|m| m.path == "src/app.tsx")
            .count();
        assert_eq!(same_file, 2);
    }

    #[test]
    fn exact_norm_scores_1_and_test_path_penalized() {
        let mut store = store();
        let f1 = add_file(&mut store, "src/app.tsx");
        let f2 = add_file(&mut store, "src/app.test.tsx");
        store
            .insert_concept(f1, ConceptKind::UiText, "Submit", "submit", "", 10, 10, None)
            .unwrap();
        store
            .insert_concept(f2, ConceptKind::UiText, "Submit", "submit", "", 5, 5, None)
            .unwrap();

        let out = resolve(&store, "submit", &ResolveOptions::default()).unwrap();
        let prod = out.matches.iter().find(|m| m.path == "src/app.tsx").unwrap();
        let test = out
            .matches
            .iter()
            .find(|m| m.path == "src/app.test.tsx")
            .unwrap();
        assert!((prod.score - 1.0).abs() < 1e-9, "prod score {}", prod.score);
        assert!((test.score - 0.7).abs() < 1e-9, "test score {}", test.score);
    }

    #[test]
    fn symbol_fallback_tier() {
        let mut store = store();
        let f1 = add_file(&mut store, "src/app.tsx");
        store
            .insert_symbol(
                f1,
                "src/app.tsx#handleLogin#function",
                "handleLogin",
                "handleLogin",
                SymbolKind::Function,
                1,
                3,
                "handleLogin()",
            )
            .unwrap();

        // "handleLogin" is identifier-shaped (no spaces) and matches the
        // symbol name exactly, so the ident tier catches it before the
        // symbol fallback cascade — and a single exact-unique identifier
        // match is the tier's best case: `resolved`, not `ranked`.
        let out = resolve(&store, "handleLogin", &ResolveOptions::default()).unwrap();
        assert_eq!(out.tier, Some(Tier::Ident));
        assert_eq!(out.confidence, Confidence::Resolved, "{out:?}");
        assert!(!out.scan_capped);
        assert!(out.basis.contains("ident"), "basis was {:?}", out.basis);
        assert_eq!(out.matches.len(), 1);
        assert_eq!(out.matches[0].raw, "handleLogin");
        assert_eq!(out.matches[0].symbol_kind.as_deref(), Some("function"));
    }

    #[test]
    fn ident_tier_with_multiple_matches_stays_ranked() {
        let mut store = store();
        let f1 = add_file(&mut store, "src/a.tsx");
        let f2 = add_file(&mut store, "src/b.tsx");
        for (f, uid) in [(f1, "src/a.tsx#dup#function"), (f2, "src/b.tsx#dup#function")] {
            store
                .insert_symbol(f, uid, "dup", "dup", SymbolKind::Function, 1, 3, "dup()")
                .unwrap();
        }
        let out = resolve(&store, "dup", &ResolveOptions::default()).unwrap();
        assert_eq!(out.tier, Some(Tier::Ident));
        assert_eq!(out.confidence, Confidence::Ranked, "{out:?}");
        assert_eq!(out.matches.len(), 2);
    }

    #[test]
    fn unresolved_miss_reports_uncapped_basis() {
        let store = store();
        let out = resolve(&store, "utterly absent phrase", &ResolveOptions::default()).unwrap();
        assert_eq!(out.confidence, Confidence::Unresolved);
        assert!(!out.scan_capped, "tiny store can never hit a scan cap");
        assert!(
            out.basis.contains("scanned to completion"),
            "basis was {:?}",
            out.basis
        );
    }

    #[test]
    fn owner_symbol_name_boosts_score() {
        let mut store = store();
        let f1 = add_file(&mut store, "src/app.tsx");
        let sym = store
            .insert_symbol(
                f1,
                "src/app.tsx#submitButton#function",
                "submitButton",
                "submitButton",
                SymbolKind::Function,
                1,
                3,
                "submitButton()",
            )
            .unwrap();
        store
            .insert_concept(f1, ConceptKind::UiText, "Submit", "submit", "", 10, 10, Some(sym))
            .unwrap();

        let out = resolve(&store, "submit button", &ResolveOptions::default()).unwrap();
        assert_eq!(out.matches.len(), 1);
        assert!(
            (out.matches[0].score - 0.65).abs() < 1e-9,
            "score was {}",
            out.matches[0].score
        );
    }

    #[test]
    fn symbol_words_splits_camel_case() {
        assert_eq!(symbol_words("ContactForm"), vec!["contact", "form"]);
        assert_eq!(symbol_words("WELCOME_MESSAGE"), vec!["welcome", "message"]);
        assert_eq!(symbol_words("onSubmit"), vec!["on", "submit"]);
    }
}

