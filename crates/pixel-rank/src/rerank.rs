//! Engine 3 shared reranker — reorders candidates *within* their tier using
//! activity + session signals. Never promotes across tiers (protects the
//! closed-world claim).
//!
//! The single [`rerank`] helper is used by both `targets` (within-tier) and
//! `resolve` (candidate ordering): both feed it a list of
//! (path, rrf_score, tier) and get back the same list reordered inside each
//! tier, ties broken by path ascending.

use std::collections::HashMap;

use crate::signals::SignalBundle;

/// One candidate as produced by the fusion core before reranking.
#[derive(Debug, Clone)]
pub struct RankedCandidate {
    pub path: String,
    /// The unmodified RRF score (tier assignment ran on this).
    pub rrf_score: f64,
    /// "P0" | "P1" | "P2" — assigned on the unmodified RRF families.
    pub tier: String,
}

/// The rerank formula from PLAN.md:
/// `final = rrf_score * (1 + 0.15*activity_norm + 0.35*session_norm) * test_penalty`.
///
/// Reorders within each tier only; tier order (P0, P1, P2) and the candidate
/// set are preserved. Deterministic: ties broken by path ascending.
pub fn rerank(
    candidates: Vec<RankedCandidate>,
    signals: &SignalBundle,
    test_penalty: f64,
) -> Vec<RankedCandidate> {
    let activity = &signals.activity;
    let session = &signals.session;

    let mut out: Vec<RankedCandidate> = candidates
        .into_iter()
        .map(|mut c| {
            let act = activity.get(&c.path).copied().unwrap_or(0.0);
            let ses = session.get(&c.path).copied().unwrap_or(0.0);
            c.rrf_score = c.rrf_score
                * (1.0 + 0.15 * act + 0.35 * ses)
                * test_penalty;
            c
        })
        .collect();

    // Stable sort by tier (P0 < P1 < P2), then final score desc, then path asc.
    out.sort_by(|a, b| {
        a.tier
            .cmp(&b.tier)
            .then(b.rrf_score.total_cmp(&a.rrf_score))
            .then(a.path.cmp(&b.path))
    });
    out
}

/// Convenience: rerank a `TargetFile` list (from `compute_targets`) within
/// tiers, preserving the `TargetFile` shape. Returns the reordered list.
pub fn rerank_targets(
    targets: Vec<crate::TargetFile>,
    signals: &SignalBundle,
    test_penalty: f64,
) -> Vec<crate::TargetFile> {
    let candidates: Vec<RankedCandidate> = targets
        .iter()
        .map(|t| RankedCandidate {
            path: t.path.clone(),
            rrf_score: t.score,
            tier: t.tier.clone(),
        })
        .collect();
    let reordered = rerank(candidates, signals, test_penalty);
    let by_path: HashMap<&str, &crate::TargetFile> =
        targets.iter().map(|t| (t.path.as_str(), t)).collect();
    reordered
        .into_iter()
        .map(|c| {
            let t = by_path[c.path.as_str()];
            crate::TargetFile {
                path: t.path.clone(),
                tier: t.tier.clone(),
                score: c.rrf_score,
                reasons: t.reasons.clone(),
                symbols: t.symbols.clone(),
            }
        })
        .collect()
}
