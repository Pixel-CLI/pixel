// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

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

/// Maximum number of direct-index candidates made available to the reranker.
/// The public result limit is applied only after this bounded quality pass.
const RERANK_CANDIDATE_CAP: u32 = 20_000;
/// Maximum number of filename-only candidates merged into a weak T2 result.
/// This keeps the fallback bounded even in unusually large repositories.
const FILENAME_CANDIDATE_CAP: usize = 256;

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
    /// What the concept's extractor recorded beside its text: for a Rails
    /// route its handler (`admin/orders#create
    /// (Admin::OrdersController#create)`), for other kinds their own label
    /// (`component`, `key`, an HTTP route's text). Omitted when empty.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
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
    /// Identity of the concept rows a bounded T3 scan reads: a hash over the
    /// first `TRIGRAM_SCAN_CAP` rows in the scan's own `norm, id` order,
    /// rowids included. Not serialized; [`inputs_digest`] folds it in so a
    /// reindex that moves that window without changing `concepts` still moves
    /// the digest a caller caches on (a pure rowid move invalidates too, the
    /// safe direction for a cache key).
    #[serde(skip)]
    pub concept_scan_identity: u64,
    /// Mirror of [`IndexState::concept_scan_identity`] for the symbol
    /// fallback window (`name, id` order, `SYMBOL_SCAN_CAP` rows).
    #[serde(skip)]
    pub symbol_scan_identity: u64,
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
    norm.chars().all(|c| c.is_alphanumeric() || c == '_')
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
        "env" | "envvar" | "variable" => vec![ConceptKind::EnvRead],
        _ => Vec::new(),
    }
}

/// True when `phrase` has an uppercase letter and no lowercase one
/// (`CODEX_HOME`, `PATH`): the spelling of a constant or an environment
/// variable rather than of a function.
fn is_all_caps(phrase: &str) -> bool {
    phrase.chars().any(|c| c.is_ascii_uppercase())
        && !phrase.chars().any(|c| c.is_ascii_lowercase())
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
        && word.parse::<i64>().is_ok_and(|n| (100..=599).contains(&n))
}

/// Split a phrase into significant tokens (lowercased, len ≥ 2, articles
/// stripped). Returns the tokens and the head noun (last significant token).
fn phrase_tokens(phrase: &str) -> (Vec<String>, Option<String>) {
    let norm = normalize(phrase);
    let mut tokens: Vec<String> = norm
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2)
        .map(str::to_lowercase)
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
    // Apply the user-visible limit only after every tier has scored and
    // reranked its candidate set. A bounded wider pool prevents database row
    // order from hiding a stronger candidate before the reranker sees it.
    let candidate_limit = u32::try_from(limit)
        .unwrap_or(u32::MAX)
        .max(RERANK_CANDIDATE_CAP);
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
        let mut syms = non_script_symbols_by_name(store, phrase, candidate_limit)?;
        // If no exact-case hit, try the normalized (lowercased) form —
        // handles lowercase queries like "guard_matcher". Not for an
        // all-caps token: it names a constant or an environment variable
        // (`CODEX_HOME`), which a lowercase `codex_home` function is not, so
        // the cascade goes on to the concepts (an `env_read` matches at T0)
        // and the symbol fallback still offers the function after them.
        if syms.is_empty() && !is_all_caps(phrase) {
            syms = non_script_symbols_by_name(store, &norm, candidate_limit)?;
        }
        if !syms.is_empty() {
            let ident_capped = syms.len() as u32 >= candidate_limit;
            return finish_symbols(
                store,
                phrase,
                syms,
                opts,
                tiers_attempted,
                Tier::Ident,
                ident_capped,
            );
        }
    }

    // T0: exact-norm probe.
    if !norm.is_empty() {
        tiers_attempted.push(Tier::T0);
        let exact = store.concepts_by_norm(&norm, candidate_limit)?;
        if !exact.is_empty() {
            let exact_capped = exact.len() as u32 >= candidate_limit;
            let (confidence, tier) = if exact.len() == 1 {
                (Confidence::Resolved, Tier::T0)
            } else {
                (Confidence::Ranked, Tier::T0)
            };
            return finish(
                store,
                phrase,
                exact,
                confidence,
                tier,
                opts,
                tiers_attempted,
                exact_capped.then_some(RERANK_CANDIDATE_CAP),
            );
        }
    }

    // T1: kind-directed.
    let (tokens, head) = phrase_tokens(phrase);
    if !tokens.is_empty() {
        tiers_attempted.push(Tier::T1);
        let mut t1_rows: Vec<ConceptRow> = Vec::new();
        let mut t1_capped = false;
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
            let rows =
                store.concepts_by_kind_words(ConceptKind::Status, &word_refs, candidate_limit)?;
            t1_capped |= rows.len() as u32 >= candidate_limit;
            t1_rows.extend(rows);
        } else if let Some(h) = &head {
            for kind in kind_for_head_noun(h) {
                let rows = store.concepts_by_kind_words(kind, &remaining, candidate_limit)?;
                t1_capped |= rows.len() as u32 >= candidate_limit;
                t1_rows.extend(rows);
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
                t1_capped.then_some(RERANK_CANDIDATE_CAP),
            );
        }
    }

    // T2: word intersection, all kinds (AND, degrade to OR).
    if !tokens.is_empty() {
        tiers_attempted.push(Tier::T2);
        let word_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
        let and = store.concepts_by_words(&word_refs, None, candidate_limit)?;
        let (rows, t2_capped) = if and.is_empty() {
            let rows = store.concepts_by_any_word(&word_refs, None, candidate_limit)?;
            let capped = rows.len() as u32 >= candidate_limit;
            (rows, capped)
        } else {
            let capped = and.len() as u32 >= candidate_limit;
            (and, capped)
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
                t2_capped.then_some(RERANK_CANDIDATE_CAP),
            );
        }
    }

    // T3: trigram fallback (verified matches via real character-trigram
    // overlap, low confidence).
    if !norm.is_empty() {
        tiers_attempted.push(Tier::T3);
        let (rows, capped) = trigram_fallback(store, &norm, candidate_limit, TRIGRAM_SCAN_CAP)?;
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
                capped.then_some(TRIGRAM_SCAN_CAP),
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
        let (symbols, capped) =
            symbol_fallback(store, &ident_words, candidate_limit, SYMBOL_SCAN_CAP)?;
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
    scan_cap: Option<u32>,
) -> Result<ResolveOutcome, StoreError> {
    let limit = opts.limit.max(1);
    let mut candidates: Vec<RankedCandidate> = Vec::with_capacity(rows.len());
    let mut by_id: HashMap<u64, ConceptMatch> = HashMap::with_capacity(rows.len());
    for row in rows {
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
            detail: row.detail,
            owner,
            symbol_kind: None,
            score,
            reasons,
        };
        by_id.insert(id, m.clone());
        candidates.push(RankedCandidate {
            id,
            path,
            rrf_score: score,
            tier: tier.as_str().to_string(),
        });
    }

    // T2's OR fallback can otherwise stop at a single incidental string
    // match, even when a file's whole basename directly names a query term.
    // Add that evidence only when every concept candidate has at most one
    // query-word match: filename evidence is a bounded recovery for sparse
    // lexical results, never a way to displace stronger concept content.
    let mut filename_capped = false;
    if tier == Tier::T2 {
        let (filename_matches, capped) = weak_filename_matches(store, phrase, by_id.values())?;
        filename_capped = capped;
        let filename_by_path: HashMap<String, (u64, ConceptMatch)> = filename_matches
            .into_iter()
            .map(|(id, m)| (m.path.clone(), (id, m)))
            .collect();
        let existing_paths: std::collections::HashSet<String> = candidates
            .iter()
            .map(|candidate| candidate.path.clone())
            .collect();

        // A real concept row is preferable to synthetic filename evidence in
        // the same file. Promote its score and make the filename provenance
        // visible rather than returning two rows for one path.
        for candidate in &mut candidates {
            if let Some((_, evidence)) = filename_by_path.get(&candidate.path)
                && evidence.score > candidate.rrf_score
            {
                candidate.rrf_score = evidence.score;
                if let Some(m) = by_id.get_mut(&candidate.id) {
                    m.score = evidence.score;
                    m.reasons.extend(evidence.reasons.clone());
                }
            }
        }
        for (path, (id, m)) in filename_by_path {
            if !existing_paths.contains(&path) {
                candidates.push(RankedCandidate {
                    id,
                    path: m.path.clone(),
                    rrf_score: m.score,
                    tier: tier.as_str().to_string(),
                });
                by_id.insert(id, m);
            }
        }
    }

    // Rerank within the tier via the pluggable reranker.
    let reranker: &dyn Reranker = opts.reranker.as_deref().unwrap_or(&LexicalReranker);
    let reordered = reranker.rerank(candidates, &opts.signals);
    let mut ordered: Vec<ConceptMatch> = reordered
        .into_iter()
        .filter_map(|c| by_id.get(&c.id).cloned())
        .collect();
    ordered.truncate(limit);

    let index_state = index_state(store)?;
    let scan_capped = scan_cap.is_some() || filename_capped;
    let basis = match (scan_cap, filename_capped) {
        (Some(scan_cap), true) => format!(
            "tier {} (concept index); candidate scan capped at {scan_cap} rows and filename fallback \
             capped at {FILENAME_CANDIDATE_CAP} candidates — unscanned rows or filenames may contain \
             better matches",
            tier.as_str()
        ),
        (Some(scan_cap), false) => format!(
            "tier {} (concept index); candidate scan capped at {scan_cap} rows — unscanned rows \
             may contain better matches",
            tier.as_str()
        ),
        (None, true) => format!(
            "tier {} (concept index); filename fallback capped at {FILENAME_CANDIDATE_CAP} candidates \
             — unscanned filenames may contain better matches",
            tier.as_str()
        ),
        (None, false) => format!(
            "tier {} (concept index, scanned to completion)",
            tier.as_str()
        ),
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

/// Return exact whole-component basename evidence only for a sparse T2
/// candidate set. A synthetic candidate carries line `0` and an explicit
/// reason so consumers cannot mistake filename evidence for extracted source
/// text.
fn weak_filename_matches<'a>(
    store: &GraphStore,
    phrase: &str,
    current: impl IntoIterator<Item = &'a ConceptMatch>,
) -> Result<(Vec<(u64, ConceptMatch)>, bool), StoreError> {
    let qwords = concept_words(&normalize(phrase));
    if qwords.len() < 2 {
        return Ok((Vec::new(), false));
    }

    let current: Vec<&ConceptMatch> = current.into_iter().collect();
    if current
        .iter()
        .any(|m| 2 * match_word_count(&m.norm, &qwords) > qwords.len())
    {
        return Ok((Vec::new(), false));
    }
    let mut matches = Vec::new();
    let mut files = store.files()?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    for file in files {
        let stem = file
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&file.path)
            .split('.')
            .next()
            .unwrap_or_default()
            .to_string();
        let components = symbol_words(&stem);
        let overlap: Vec<&str> = qwords
            .iter()
            .filter(|word| components.contains(word))
            .map(String::as_str)
            .collect();
        if overlap.is_empty() {
            continue;
        }
        let score = (SCORE_OVERLAP_BASE
            + overlap.len() as f64 / qwords.len() as f64 * SCORE_OVERLAP_SPAN
            + FILENAME_COMPONENT_BONUS)
            .min(SCORE_SUBSTRING);
        let id = u64::MAX - file.id as u64;
        matches.push((
            id,
            ConceptMatch {
                path: file.path,
                start_line: 0,
                end_line: 0,
                kind: ConceptKind::String,
                raw: format!("filename: {stem}"),
                norm: stem,
                detail: String::new(),
                owner: None,
                symbol_kind: None,
                score,
                reasons: vec![format!(
                    "filename component overlap: {}",
                    overlap.join(", ")
                )],
            },
        ));
        if matches.len() >= FILENAME_CANDIDATE_CAP {
            break;
        }
    }
    let capped = matches.len() >= FILENAME_CANDIDATE_CAP;
    Ok((matches, capped))
}

fn match_word_count(norm: &str, qwords: &[String]) -> usize {
    let words = concept_words(norm);
    qwords.iter().filter(|word| words.contains(word)).count()
}

/// Read exact-name candidates after excluding synthetic script owners, so
/// those rows cannot consume the bounded candidate window.
fn non_script_symbols_by_name(
    store: &GraphStore,
    name: &str,
    limit: u32,
) -> Result<Vec<SymbolRow>, StoreError> {
    let mut stmt = store.conn().prepare(
        "SELECT id, uid, file_id, name, qualified, kind, start_line, end_line, sig
           FROM symbols
          WHERE name = ?1 AND kind != 'script'
          ORDER BY kind, uid
          LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![name, limit], |row| {
        Ok(SymbolRow {
            id: row.get(0)?,
            uid: row.get(1)?,
            file_id: row.get(2)?,
            name: row.get(3)?,
            qualified: row.get(4)?,
            kind: SymbolKind::parse(&row.get::<_, String>(5)?),
            start_line: row.get(6)?,
            end_line: row.get(7)?,
            sig: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Build the final symbol-tier outcome. Each row becomes a [`ConceptMatch`]
/// carrying its real symbol kind and a best-effort [`ConceptKind`].
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
    let rows: Vec<SymbolRow> = rows
        .into_iter()
        .filter(|row| row.kind != SymbolKind::Script)
        .collect();
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
    for row in rows {
        let Some(kind) = symbol_kind_to_concept(row.kind) else {
            continue;
        };
        let id = row.id as u64;
        let path = file_path(store, row.file_id)?;
        let score = score_symbol(&row, phrase, &path);
        let m = ConceptMatch {
            path: path.clone(),
            start_line: row.start_line,
            end_line: row.end_line,
            kind,
            raw: row.name.clone(),
            norm: normalize(&row.name),
            detail: String::new(),
            owner: None,
            symbol_kind: Some(row.kind.as_str().to_string()),
            score,
            reasons: vec![reason.to_string()],
        };
        by_id.insert(id, m.clone());
        candidates.push(RankedCandidate {
            id,
            path,
            rrf_score: score,
            tier: tier.as_str().to_string(),
        });
    }

    let reranker: &dyn Reranker = opts.reranker.as_deref().unwrap_or(&LexicalReranker);
    let reordered = reranker.rerank(candidates, &opts.signals);
    let mut ordered: Vec<ConceptMatch> = reordered
        .into_iter()
        .filter_map(|c| by_id.get(&c.id).cloned())
        .collect();
    ordered.truncate(limit);

    let index_state = index_state(store)?;
    let tier_desc = if tier == Tier::Ident && scan_capped {
        format!(
            "tier ident (exact symbol-name index probe; candidate scan capped at {RERANK_CANDIDATE_CAP} rows — \
             unscanned symbols may contain better matches)"
        )
    } else if tier == Tier::Ident {
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
            .map(String::as_str)
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
/// Small provenance-specific lift for an exact basename component. Used only
/// by the weak T2 fallback, after it has proved no concept has more than one
/// matching query word.
const FILENAME_COMPONENT_BONUS: f64 = 0.12;
/// Multiplier applied when the match lives in a test path: a phrase's real
/// definition is almost always the production site, not the test that quotes
/// it, so tests are demoted but never eliminated.
const TEST_PATH_PENALTY: f64 = 0.7;
/// Maximum additive bonus when every distinct query word appears in the
/// enclosing symbol name. Partial owner overlap is scaled by its query
/// coverage, so an owner-name hint can break lexical ties but cannot outweigh
/// stronger concept evidence.
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
        if !qwords.is_empty() {
            let overlap = qwords.iter().filter(|w| owner_words.contains(w)).count();
            score += OWNER_WORD_BONUS * overlap as f64 / qwords.len() as f64;
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
                && cur.chars().last().is_some_and(|x| x.is_ascii_lowercase());
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
fn symbol_kind_to_concept(kind: SymbolKind) -> Option<ConceptKind> {
    match kind {
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Const => Some(ConceptKind::String),
        SymbolKind::Class
        | SymbolKind::Struct
        | SymbolKind::Enum
        | SymbolKind::Variant
        | SymbolKind::Trait
        | SymbolKind::Interface
        | SymbolKind::Module => Some(ConceptKind::Component),
        SymbolKind::Script => None,
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
///
/// Callers pass the cap in, so tests can exercise the boundary with a small
/// fixture (the production value is not reachable in one).
const TRIGRAM_SCAN_CAP: u32 = 20_000;
/// The order the T3 scan reads the concept table in, and the order
/// [`concept_scan_identity`] hashes that same window in — one spelling for
/// both, so the digest cannot see a different window than the scan reads.
/// `ORDER BY` (never physical row order) is what makes the window a function
/// of the indexed content: `replace_file` deletes and reinserts a file's
/// concepts, so a physical `LIMIT` moved the boundary on every reindex.
const CONCEPT_SCAN_ORDER: &str = "ORDER BY norm, id";
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
/// against `norm`. The scan reads `cap` rows in [`CONCEPT_SCAN_ORDER`], so
/// the window is the same set of rows on every rebuild of identical content.
/// Falls back to the plain substring scan for queries under 3 chars (too
/// short to form a single trigram, so overlap is meaningless).
///
/// The second return value is true when the scan HIT its row cap (`cap`):
/// rows beyond it were never considered, so the result is a lower bound and
/// the caller must surface that.
fn trigram_fallback(
    store: &GraphStore,
    norm: &str,
    limit: u32,
    cap: u32,
) -> Result<(Vec<ConceptRow>, bool), StoreError> {
    let query_grams = trigram_set(norm);
    if query_grams.is_empty() {
        // The `concepts_like` path is itself LIMIT-bounded; treat a full
        // page as a possibly-capped scan for the same honesty reason.
        let rows = store.concepts_like(norm, limit)?;
        let capped = rows.len() as u32 >= limit;
        return Ok((rows, capped));
    }
    let sql = format!(
        "SELECT id, file_id, kind, raw, norm, detail, start_line, end_line, owner_symbol_id
         FROM concepts {CONCEPT_SCAN_ORDER} LIMIT ?1"
    );
    let mut stmt = store.conn().prepare(&sql)?;
    let mut scanned: u32 = 0;
    let mut scored: Vec<(f64, ConceptRow)> = stmt
        .query_map(params![cap], |r| {
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
    let capped = scanned >= cap;
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
    scored.truncate(limit as usize);
    Ok((scored.into_iter().map(|(_, row)| row).collect(), capped))
}

// ---------------------------------------------------------------------------
// symbol fallback tier
// ---------------------------------------------------------------------------

/// Bound on how many symbol rows the fallback scan will consider, mirroring
/// [`TRIGRAM_SCAN_CAP`]; the caller passes it in for the same testability
/// reason.
const SYMBOL_SCAN_CAP: u32 = 20_000;
/// The order the symbol fallback reads the `symbols` table in, and the order
/// [`symbol_scan_identity`] hashes that same window in — one spelling for
/// both, so the digest cannot see a different window than the scan reads.
const SYMBOL_SCAN_ORDER: &str = "ORDER BY name, id";

/// Symbol fallback: scan a bounded slice of the `symbols` table and keep rows
/// whose camelCase-split name shares at least one ident word with the query's
/// ident words, ranked by overlap ratio. This is the last tier before
/// `unresolved`; like T3 it reads `cap` rows in [`SYMBOL_SCAN_ORDER`], so a
/// reindex cannot move the window.
/// The second return value is true when the scan HIT its row cap (`cap`):
/// symbols beyond it were never considered, so the result is a lower bound
/// and the caller must surface that.
fn symbol_fallback(
    store: &GraphStore,
    words: &[String],
    limit: u32,
    cap: u32,
) -> Result<(Vec<SymbolRow>, bool), StoreError> {
    if words.is_empty() {
        return Ok((Vec::new(), false));
    }
    let sql = format!(
        "SELECT id, uid, file_id, name, qualified, kind, start_line, end_line, sig
         FROM symbols WHERE kind != 'script' {SYMBOL_SCAN_ORDER} LIMIT ?1"
    );
    let mut stmt = store.conn().prepare(&sql)?;
    let mut scanned: u32 = 0;
    let mut scored: Vec<(f64, SymbolRow)> = stmt
        .query_map(params![cap], |r| {
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
                .map(String::as_str)
                .collect();
            if overlap.is_empty() {
                None
            } else {
                let score = overlap.len() as f64 / words.len() as f64;
                Some((score, row))
            }
        })
        .collect();
    let capped = scanned >= cap;
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
    scored.truncate(limit as usize);
    Ok((scored.into_iter().map(|(_, row)| row).collect(), capped))
}

/// The response header plus the two scan-window identities [`inputs_digest`]
/// folds in.
fn index_state(store: &GraphStore) -> Result<IndexState, StoreError> {
    let concepts = store.concept_count()?;
    let concepts_version = store.concepts_version()?;
    Ok(IndexState {
        concepts,
        concepts_version,
        fresh: concepts > 0,
        concept_scan_identity: concept_scan_identity(store, TRIGRAM_SCAN_CAP)?,
        symbol_scan_identity: symbol_scan_identity(store, SYMBOL_SCAN_CAP)?,
    })
}

/// Hash the bounded window a fallback scan reads: the first `cap` rows
/// `sql` returns, as `(rowid, text)` pairs in the query's own order. Folded
/// into [`inputs_digest`] so the digest moves with the window.
fn scan_window_identity(store: &GraphStore, sql: &str, cap: u32) -> Result<u64, StoreError> {
    let mut stmt = store.conn().prepare(sql)?;
    let rows = stmt.query_map(params![cap], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut buf = Vec::new();
    for row in rows {
        let (id, text) = row?;
        buf.extend_from_slice(&id.to_le_bytes());
        buf.extend_from_slice(text.as_bytes());
        buf.push(0);
    }
    Ok(xxh3_64(&buf))
}

/// Identity of the concept window a T3 scan reads: `TRIGRAM_SCAN_CAP` rows
/// in [`CONCEPT_SCAN_ORDER`].
fn concept_scan_identity(store: &GraphStore, cap: u32) -> Result<u64, StoreError> {
    let sql = format!("SELECT id, norm FROM concepts {CONCEPT_SCAN_ORDER} LIMIT ?1");
    scan_window_identity(store, &sql, cap)
}

/// Identity of the symbol window the fallback scan reads: `SYMBOL_SCAN_CAP`
/// rows in [`SYMBOL_SCAN_ORDER`].
fn symbol_scan_identity(store: &GraphStore, cap: u32) -> Result<u64, StoreError> {
    let sql =
        format!("SELECT id, name FROM symbols WHERE kind != 'script' {SYMBOL_SCAN_ORDER} LIMIT ?1");
    scan_window_identity(store, &sql, cap)
}

/// `xxh3(phrase ‖ concepts_version ‖ concept_count ‖ both scan-window
/// identities)` — the digest every resolve response carries so a caller can
/// detect when the underlying index changed.
///
/// The window identities are what keep that promise across a reindex: the
/// bounded fallback scans read a window of the index, and `replace_file`
/// deletes and reinserts a file's rows, so the window can move while the
/// concept count stays put.
fn inputs_digest(phrase: &str, state: &IndexState) -> u64 {
    let mut buf = Vec::new();
    buf.extend_from_slice(phrase.as_bytes());
    buf.push(0);
    if let Some(v) = &state.concepts_version {
        buf.extend_from_slice(v.as_bytes());
    }
    buf.push(0);
    buf.extend_from_slice(&state.concepts.to_le_bytes());
    buf.extend_from_slice(&state.concept_scan_identity.to_le_bytes());
    buf.extend_from_slice(&state.symbol_scan_identity.to_le_bytes());
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

    #[test]
    fn is_all_caps_should_need_an_uppercase_letter_and_no_lowercase_one() {
        assert!(is_all_caps("CODEX_HOME"));
        assert!(is_all_caps("PATH"));
        assert!(!is_all_caps("codex_home"));
        assert!(!is_all_caps("CodexHome"));
        assert!(!is_all_caps("_9"), "no letter at all is not all caps");
    }

    fn store() -> GraphStore {
        GraphStore::open_in_memory().unwrap()
    }

    fn add_file(store: &mut GraphStore, path: &str) -> i64 {
        store.replace_file(path, "blob", "tsx").unwrap()
    }

    #[test]
    fn lexical_rank_applies_quality_to_exact_concepts_and_symbols() {
        let mut store = store();
        let test_file = add_file(&mut store, "src/a.test.rs");
        let production_file = add_file(&mut store, "src/z.rs");
        for (file, uid) in [(test_file, "test"), (production_file, "prod")] {
            store
                .insert_concept(
                    file,
                    ConceptKind::UiText,
                    "Submit",
                    "submit",
                    "",
                    1,
                    1,
                    None,
                )
                .unwrap();
            store
                .insert_symbol(
                    file,
                    uid,
                    "SubmitForm",
                    "SubmitForm",
                    SymbolKind::Function,
                    1,
                    3,
                    "SubmitForm()",
                )
                .unwrap();
        }
        for query in ["submit", "SubmitForm"] {
            let out = resolve(&store, query, &ResolveOptions::default()).unwrap();
            assert_eq!(out.matches[0].path, "src/z.rs", "{query}");
            assert!(out.matches[0].score > out.matches[1].score);
        }
    }

    #[test]
    fn exact_identifier_reranks_before_applying_result_limit() {
        let mut store = store();
        let test_file = add_file(&mut store, "src/a.test.rs");
        let production_file = add_file(&mut store, "src/z.rs");
        for (file, uid) in [(test_file, "a"), (production_file, "z")] {
            store
                .insert_symbol(
                    file,
                    uid,
                    "SubmitForm",
                    "SubmitForm",
                    SymbolKind::Function,
                    1,
                    3,
                    "SubmitForm()",
                )
                .unwrap();
        }
        let out = resolve(
            &store,
            "SubmitForm",
            &ResolveOptions {
                limit: 1,
                ..ResolveOptions::default()
            },
        )
        .unwrap();
        assert_eq!(out.matches[0].path, "src/z.rs");
    }

    #[test]
    fn synthetic_script_symbols_continue_to_later_concept_tiers() {
        let mut store = store();
        let file = add_file(&mut store, "scripts/run.rb");
        store
            .insert_symbol(
                file,
                "scripts/run.rb#RunScriptOwner#script",
                "RunScriptOwner",
                "scripts/run.rb",
                SymbolKind::Script,
                1,
                3,
                "scripts/run.rb",
            )
            .unwrap();
        store
            .insert_concept(
                file,
                ConceptKind::UiText,
                "RunScriptOwner",
                &normalize("RunScriptOwner"),
                "",
                1,
                1,
                None,
            )
            .unwrap();

        let out = resolve(&store, "RunScriptOwner", &ResolveOptions::default()).unwrap();
        assert_eq!(out.confidence, Confidence::Resolved, "{out:?}");
        assert_eq!(out.tier, Some(Tier::T0), "{out:?}");
        assert_eq!(out.matches.len(), 1, "{out:?}");
        assert_eq!(out.matches[0].path, "scripts/run.rb");
        assert_eq!(out.matches[0].kind, ConceptKind::UiText);
        assert_eq!(out.tiers_attempted, [Tier::Ident, Tier::T0]);
    }

    #[test]
    fn word_intersection_reranks_before_applying_result_limit() {
        let mut store = store();
        let test_file = add_file(&mut store, "src/a.test.rs");
        let production_file = add_file(&mut store, "src/z.rs");
        for file in [test_file, production_file] {
            store
                .insert_concept(
                    file,
                    ConceptKind::UiText,
                    "alpha beta",
                    "alpha beta",
                    "",
                    1,
                    1,
                    None,
                )
                .unwrap();
        }
        let out = resolve(
            &store,
            "alpha beta",
            &ResolveOptions {
                limit: 1,
                ..ResolveOptions::default()
            },
        )
        .unwrap();
        assert_eq!(out.matches[0].path, "src/z.rs");
    }

    #[test]
    fn lexical_collector_ranks_coverage_before_limit() {
        let mut store = store();
        let weak = add_file(&mut store, "src/a_weak.rs");
        let strong = add_file(&mut store, "src/z_strong.rs");
        for (file, text) in [(weak, "alpha"), (strong, "alpha beta")] {
            store
                .insert_concept(file, ConceptKind::UiText, text, text, "", 1, 1, None)
                .unwrap();
        }
        let opts = ResolveOptions {
            limit: 1,
            ..ResolveOptions::default()
        };
        let out = resolve(&store, "alpha beta gamma", &opts).unwrap();
        assert_eq!(out.matches[0].path, "src/z_strong.rs");
    }

    #[test]
    fn lexical_rank_uses_match_quality_not_insertion_order() {
        let mut store = store();
        let weak = add_file(&mut store, "src/a_weak.rs");
        let strong = add_file(&mut store, "src/z_strong.rs");
        for (file, text) in [(weak, "alpha"), (strong, "alpha beta")] {
            store
                .insert_concept(file, ConceptKind::UiText, text, text, "", 1, 1, None)
                .unwrap();
        }
        let out = resolve(&store, "alpha beta gamma", &ResolveOptions::default()).unwrap();
        assert_eq!(out.matches[0].path, "src/z_strong.rs");
        assert!(out.matches[0].score > out.matches[1].score);
    }

    #[test]
    fn same_file_concepts_are_not_collapsed() {
        let mut store = store();
        let f1 = add_file(&mut store, "src/app.tsx");
        let f2 = add_file(&mut store, "src/app.test.tsx");
        store
            .insert_concept(
                f1,
                ConceptKind::UiText,
                "Submit",
                "submit",
                "",
                10,
                10,
                None,
            )
            .unwrap();
        store
            .insert_concept(
                f1,
                ConceptKind::UiText,
                "Submit",
                "submit",
                "",
                20,
                20,
                None,
            )
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
            .insert_concept(
                f1,
                ConceptKind::UiText,
                "Submit",
                "submit",
                "",
                10,
                10,
                None,
            )
            .unwrap();
        store
            .insert_concept(f2, ConceptKind::UiText, "Submit", "submit", "", 5, 5, None)
            .unwrap();

        let out = resolve(&store, "submit", &ResolveOptions::default()).unwrap();
        let prod = out
            .matches
            .iter()
            .find(|m| m.path == "src/app.tsx")
            .unwrap();
        let test = out
            .matches
            .iter()
            .find(|m| m.path == "src/app.test.tsx")
            .unwrap();
        assert!((prod.score - 1.0).abs() < 1e-9, "prod score {}", prod.score);
        assert!((test.score - 0.7).abs() < 1e-9, "test score {}", test.score);
    }

    #[test]
    fn symbol_fallback_excludes_scripts_before_its_scan_cap() {
        let mut store = store();
        let file = add_file(&mut store, "scripts/run.rb");
        store
            .insert_symbol(
                file,
                "a-script",
                "RunScriptOwner",
                "scripts/run.rb",
                SymbolKind::Script,
                1,
                3,
                "scripts/run.rb",
            )
            .unwrap();
        store
            .insert_symbol(
                file,
                "b-function",
                "RunWorker",
                "RunWorker",
                SymbolKind::Function,
                5,
                7,
                "RunWorker()",
            )
            .unwrap();

        let (rows, capped) = symbol_fallback(&store, &["run".to_string()], 10, 1).unwrap();
        assert!(capped);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "RunWorker");
        assert_eq!(rows[0].kind, SymbolKind::Function);
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
        for (f, uid) in [
            (f1, "src/a.tsx#dup#function"),
            (f2, "src/b.tsx#dup#function"),
        ] {
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
            .insert_concept(
                f1,
                ConceptKind::UiText,
                "Submit",
                "submit",
                "",
                10,
                10,
                Some(sym),
            )
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
    fn owner_bonus_cannot_outweigh_stronger_concept_coverage() {
        let partial = ConceptRow {
            id: 1,
            file_id: 1,
            kind: ConceptKind::String,
            raw: "concept".into(),
            norm: "concept".into(),
            detail: String::new(),
            start_line: 1,
            end_line: 1,
            owner_symbol_id: None,
        };
        let stronger = ConceptRow {
            id: 2,
            file_id: 1,
            kind: ConceptKind::String,
            raw: "resolve phrase".into(),
            norm: "resolve phrase".into(),
            detail: String::new(),
            start_line: 1,
            end_line: 1,
            owner_symbol_id: None,
        };
        let phrase = ["concept", "index", "resolve", "phrase", "map", "marked"].join(" ");

        assert!(
            score_match(&stronger, &phrase, None, "src/concept_resolve.rs")
                > score_match(&partial, &phrase, Some("concept_index"), "src/store.rs"),
            "an owner-name hint must only break equal lexical coverage"
        );
    }

    #[test]
    fn owner_bonus_scales_with_distinct_query_word_coverage() {
        let row = ConceptRow {
            id: 1,
            file_id: 1,
            kind: ConceptKind::String,
            raw: "unrelated".into(),
            norm: "unrelated".into(),
            detail: String::new(),
            start_line: 1,
            end_line: 1,
            owner_symbol_id: None,
        };
        let phrase = "alpha beta gamma delta";
        let baseline = score_match(&row, phrase, None, "src/app.rs");

        for (owner, expected_coverage) in [
            ("unrelated", 0.0),
            ("alpha", 0.25),
            ("alphaBeta", 0.50),
            ("alphaBetaGammaDelta", 1.0),
        ] {
            let bonus = score_match(&row, phrase, Some(owner), "src/app.rs") - baseline;
            assert!(
                (bonus - OWNER_WORD_BONUS * expected_coverage).abs() < 1e-9,
                "{owner}: expected coverage {expected_coverage}, got bonus {bonus}"
            );
        }
    }

    #[test]
    fn weak_t2_matches_are_augmented_by_exact_filename_components() {
        let mut store = store();
        let incidental = add_file(&mut store, "src/extract.rs");
        let _target = add_file(&mut store, "src/impact.rs");
        store
            .insert_concept(
                incidental,
                ConceptKind::String,
                "impact walks method-to-method",
                "impact walks method to method",
                "",
                1,
                1,
                None,
            )
            .unwrap();

        let out = resolve(
            &store,
            &["callers", "callees", "impact", "trace", "reachability"].join(" "),
            &ResolveOptions::default(),
        )
        .unwrap();

        assert_eq!(out.tier, Some(Tier::T2));
        assert_eq!(out.matches[0].path, "src/impact.rs", "{out:?}");
        assert!(
            out.matches[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("filename component")),
            "filename evidence must be explicit: {out:?}"
        );
    }

    #[test]
    fn filename_fallback_cap_marks_resolve_outcome_degraded() {
        let mut store = store();
        for index in 0..=FILENAME_CANDIDATE_CAP {
            add_file(&mut store, &format!("src/a{index:04}_impact.rs"));
        }
        let incidental = add_file(&mut store, "src/incidental.rs");
        store
            .insert_concept(
                incidental,
                ConceptKind::String,
                "impact walks method-to-method",
                "impact walks method to method",
                "",
                1,
                1,
                None,
            )
            .unwrap();

        let out = resolve(
            &store,
            &["callers", "callees", "impact", "trace", "reachability"].join(" "),
            &ResolveOptions::default(),
        )
        .unwrap();

        assert!(out.scan_capped, "filename cap must be reported: {out:?}");
        assert!(
            out.basis.contains("filename fallback capped"),
            "filename cap provenance must be visible: {out:?}"
        );
    }

    #[test]
    fn filename_evidence_outranks_sparse_two_word_concept_evidence() {
        let mut store = store();
        let strong = add_file(&mut store, "src/extract.rs");
        let _filename_only = add_file(&mut store, "src/gamma.rs");
        store
            .insert_concept(
                strong,
                ConceptKind::String,
                "alpha delta",
                "alpha delta",
                "",
                1,
                1,
                None,
            )
            .unwrap();

        let out = resolve(
            &store,
            &["alpha", "beta", "gamma", "delta", "epsilon"].join(" "),
            &ResolveOptions::default(),
        )
        .unwrap();

        assert_eq!(out.matches[0].path, "src/gamma.rs", "{out:?}");
        assert!(
            out.matches[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("filename component")),
            "sparse two-word content must admit filename evidence: {out:?}"
        );
    }

    #[test]
    fn filename_evidence_does_not_displace_majority_concept_evidence() {
        let mut store = store();
        let strong = add_file(&mut store, "src/extract.rs");
        let _filename_only = add_file(&mut store, "src/gamma.rs");
        store
            .insert_concept(
                strong,
                ConceptKind::String,
                "alpha beta delta",
                "alpha beta delta",
                "",
                1,
                1,
                None,
            )
            .unwrap();

        let out = resolve(
            &store,
            &["alpha", "beta", "gamma", "delta", "epsilon"].join(" "),
            &ResolveOptions::default(),
        )
        .unwrap();

        assert_eq!(out.matches[0].path, "src/extract.rs", "{out:?}");
        assert!(out.matches.iter().all(|m| {
            !m.reasons
                .iter()
                .any(|reason| reason.contains("filename component"))
        }));
    }

    #[test]
    fn filename_evidence_outranks_exactly_half_coverage() {
        let mut store = store();
        let content = add_file(&mut store, "src/extract.rs");
        let _filename = add_file(&mut store, "src/gamma.rs");
        store
            .insert_concept(
                content,
                ConceptKind::String,
                "alpha beta",
                "alpha beta",
                "",
                1,
                1,
                None,
            )
            .unwrap();

        let out = resolve(
            &store,
            &["alpha", "beta", "gamma", "delta"].join(" "),
            &ResolveOptions::default(),
        )
        .unwrap();

        assert_eq!(out.matches[0].path, "src/gamma.rs", "{out:?}");
    }

    #[test]
    fn filename_evidence_promotes_a_real_match_in_its_own_file() {
        let mut store = store();
        let incidental = add_file(&mut store, "src/targets.rs");
        let target = add_file(&mut store, "src/cluster.rs");
        for (file, raw, norm) in [
            (incidental, "cluster records", "cluster records"),
            (target, "cluster members", "cluster members"),
        ] {
            store
                .insert_concept(file, ConceptKind::String, raw, norm, "", 1, 1, None)
                .unwrap();
        }

        let out = resolve(
            &store,
            &["cluster", "functional", "area", "detect", "co", "locate"].join(" "),
            &ResolveOptions::default(),
        )
        .unwrap();

        assert_eq!(out.matches[0].path, "src/cluster.rs", "{out:?}");
        assert!(
            out.matches[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("filename component"))
        );
    }

    #[test]
    fn filename_evidence_ignores_compound_extension_components() {
        let mut store = store();
        let incidental = add_file(&mut store, "src/other.rs");
        let _types = add_file(&mut store, "src/types.d.ts");
        store
            .insert_concept(
                incidental,
                ConceptKind::String,
                "other",
                "other",
                "",
                1,
                1,
                None,
            )
            .unwrap();

        let out = resolve(&store, "d other", &ResolveOptions::default()).unwrap();
        assert!(
            out.matches.iter().all(|m| m.path != "src/types.d.ts"),
            "extension components are not filename evidence: {out:?}"
        );
    }

    #[test]
    fn symbol_words_splits_camel_case() {
        assert_eq!(symbol_words("ContactForm"), vec!["contact", "form"]);
        assert_eq!(symbol_words("WELCOME_MESSAGE"), vec!["welcome", "message"]);
        assert_eq!(symbol_words("onSubmit"), vec!["on", "submit"]);
    }

    #[test]
    fn is_status_code_accepts_three_digits_in_the_http_range_only() {
        assert!(is_status_code("404"));
        assert!(is_status_code("100"));
        assert!(is_status_code("599"));
        assert!(!is_status_code("999"), "three digits but not a status");
        assert!(!is_status_code("042"));
        assert!(!is_status_code("99"));
        assert!(!is_status_code("4o4"));
    }

    // -----------------------------------------------------------------------
    // bounded fallback scans: a content-defined window, not the physical order
    // -----------------------------------------------------------------------

    /// A capped T3 scan reads its rows in `norm, id` order, so a reindex that
    /// moves a file's rowids to the end of the table (`replace_file` deletes
    /// and reinserts them) must leave the matched window identical. The
    /// pre-fix scan took the first `LIMIT` rows in physical order, so the
    /// reindexed file fell out of the window and its match disappeared.
    #[test]
    fn capped_trigram_scan_keeps_the_same_window_when_a_reindex_moves_rowids() {
        const CAP: u32 = 3;
        let mut store = store();
        let reindexed = add_file(&mut store, "src/reindexed.tsx");
        let other = add_file(&mut store, "src/other.tsx");
        // Insertion order is the physical order the cap cuts through, and the
        // reindexed file's rows come first — the arrangement the cap used to
        // be sensitive to.
        for (file, norm) in [
            (reindexed, "aaa"),
            (reindexed, "email"),
            (reindexed, "zzz1"),
            (other, "bbbb"),
            (other, "zzz2"),
        ] {
            store
                .insert_concept(file, ConceptKind::String, norm, norm, "", 1, 1, None)
                .unwrap();
        }

        let (before, capped_before) = trigram_fallback(&store, "mail", 8, CAP).unwrap();
        assert!(capped_before, "the fixture must cross the cap: {before:?}");
        let before_rows: Vec<(&str, i64, u32)> = before
            .iter()
            .map(|r| (r.norm.as_str(), r.file_id, r.start_line))
            .collect();
        assert_eq!(before_rows, vec![("email", reindexed, 1)], "{before:?}");

        let file_id = store
            .replace_file("src/reindexed.tsx", "blob2", "tsx")
            .unwrap();
        assert_eq!(file_id, reindexed, "a reindex keeps the file row");
        for norm in ["aaa", "email", "zzz1"] {
            store
                .insert_concept(file_id, ConceptKind::String, norm, norm, "", 1, 1, None)
                .unwrap();
        }

        let (after, capped_after) = trigram_fallback(&store, "mail", 8, CAP).unwrap();
        assert!(capped_after, "the fixture must cross the cap: {after:?}");
        let after_rows: Vec<(&str, i64, u32)> = after
            .iter()
            .map(|r| (r.norm.as_str(), r.file_id, r.start_line))
            .collect();
        assert_eq!(
            after_rows, before_rows,
            "a reindex must not move the scanned window"
        );
    }

    /// The capped symbol scan reads its rows in `name, id` order for the same
    /// reason: a reindex must not move the window, and with it the match set.
    #[test]
    fn capped_symbol_scan_keeps_the_same_window_when_a_reindex_moves_rowids() {
        const CAP: u32 = 2;
        let mut store = store();
        let reindexed = add_file(&mut store, "src/reindexed.rs");
        let other = add_file(&mut store, "src/other.rs");
        for (file, uid, name) in [
            (reindexed, "reindexed#alpha", "alpha"),
            (reindexed, "reindexed#checkoutPage", "checkoutPage"),
            (other, "other#zeta", "zeta"),
            (other, "other#zetaTwo", "zetaTwo"),
        ] {
            store
                .insert_symbol(file, uid, name, name, SymbolKind::Function, 1, 1, "()")
                .unwrap();
        }
        let words = symbol_words("checkout page");

        let (before, capped_before) = symbol_fallback(&store, &words, 8, CAP).unwrap();
        assert!(capped_before, "the fixture must cross the cap: {before:?}");
        let before_names: Vec<&str> = before.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(before_names, vec!["checkoutPage"], "{before:?}");

        let file_id = store
            .replace_file("src/reindexed.rs", "blob2", "rs")
            .unwrap();
        assert_eq!(file_id, reindexed, "a reindex keeps the file row");
        for (uid, name) in [
            ("reindexed#alpha", "alpha"),
            ("reindexed#checkoutPage", "checkoutPage"),
        ] {
            store
                .insert_symbol(file_id, uid, name, name, SymbolKind::Function, 1, 1, "()")
                .unwrap();
        }

        let (after, capped_after) = symbol_fallback(&store, &words, 8, CAP).unwrap();
        assert!(capped_after, "the fixture must cross the cap: {after:?}");
        let after_names: Vec<&str> = after.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            after_names, before_names,
            "a reindex must not move the scanned window"
        );
    }

    /// The production cap reaches the scan: a T3 match sitting behind the
    /// first row of the window is still found, which a cap collapsed to one
    /// row (or none) would miss.
    #[test]
    fn resolve_finds_a_t3_match_behind_the_first_scanned_row() {
        let mut store = store();
        let first = add_file(&mut store, "src/first.rs");
        let matching = add_file(&mut store, "src/matching.rs");
        store
            .insert_concept(first, ConceptKind::String, "alpha", "alpha", "", 1, 1, None)
            .unwrap();
        store
            .insert_concept(
                matching,
                ConceptKind::String,
                "email",
                "email",
                "",
                1,
                1,
                None,
            )
            .unwrap();

        let out = resolve(&store, "mail", &ResolveOptions::default()).unwrap();

        assert_eq!(out.tier, Some(Tier::T3), "{out:?}");
        assert_eq!(out.matches.len(), 1, "{out:?}");
        assert_eq!(out.matches[0].path, "src/matching.rs", "{out:?}");
    }

    fn row(kind: ConceptKind, norm: &str) -> ConceptRow {
        ConceptRow {
            id: 1,
            file_id: 1,
            kind,
            raw: norm.to_string(),
            norm: norm.to_string(),
            detail: String::new(),
            start_line: 1,
            end_line: 1,
            owner_symbol_id: None,
        }
    }

    #[test]
    fn match_reasons_name_the_lexical_relation_or_fall_back_to_the_kind() {
        let exact = row(ConceptKind::UiText, &normalize("Sign in"));
        assert_eq!(match_reasons(&exact, "Sign in"), vec!["exact norm match"]);
        let longer = row(ConceptKind::UiText, &normalize("Sign in with Google"));
        assert_eq!(
            match_reasons(&longer, "sign in"),
            vec!["word overlap: sign, in", "substring match"]
        );
        let partial = row(ConceptKind::UiText, &normalize("google login"));
        assert_eq!(
            match_reasons(&partial, "login page"),
            vec!["word overlap: login"]
        );
        let unrelated = row(ConceptKind::Route, &normalize("/api/users"));
        assert_eq!(match_reasons(&unrelated, "checkout"), vec!["kind route"]);
    }
}
