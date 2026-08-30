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
    /// Stable identity (concept/symbol row id, or index for targets).
    /// Preserved through the rerank round-trip so callers can restore
    /// same-file distinct candidates by id (not path).
    pub id: u64,
    pub path: String,
    /// The unmodified RRF score (tier assignment ran on this).
    pub rrf_score: f64,
    /// "P0" | "P1" | "P2" — assigned on the unmodified RRF families.
    pub tier: String,
}

/// The rerank formula from PLAN.md:
/// `final = rrf_score * (1 + 0.15*activity_norm + 0.35*session_norm) * penalty(path)`.
///
/// `penalty` is a per-candidate multiplier (e.g. a test penalty that only
/// applies to test paths when the task does NOT mention tests — see
/// [`crate::signals::test_penalty_fn`]). Reorders within each tier only; tier
/// order (P0, P1, P2) and the candidate set are preserved. Deterministic:
/// ties broken by path ascending.
pub fn rerank<F>(
    candidates: Vec<RankedCandidate>,
    signals: &SignalBundle,
    penalty: F,
) -> Vec<RankedCandidate>
where
    F: Fn(&str) -> f64,
{
    let activity = &signals.activity;
    let session = &signals.session;

    let mut out: Vec<RankedCandidate> = candidates
        .into_iter()
        .map(|mut c| {
            let act = activity.get(&c.path).copied().unwrap_or(0.0);
            let ses = session.get(&c.path).copied().unwrap_or(0.0);
            c.rrf_score = c.rrf_score
                * (1.0 + 0.15 * act + 0.35 * ses)
                * penalty(&c.path);
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
pub fn rerank_targets<F>(
    targets: Vec<crate::TargetFile>,
    signals: &SignalBundle,
    penalty: F,
) -> Vec<crate::TargetFile>
where
    F: Fn(&str) -> f64,
{
    let candidates: Vec<RankedCandidate> = targets
        .iter()
        .enumerate()
        .map(|(i, t)| RankedCandidate {
            id: i as u64,
            path: t.path.clone(),
            rrf_score: t.score,
            tier: t.tier.clone(),
        })
        .collect();
    let reordered = rerank(candidates, signals, penalty);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::test_penalty_fn;

    fn cand(path: &str, score: f64, tier: &str) -> RankedCandidate {
        RankedCandidate {
            id: 0,
            path: path.to_string(),
            rrf_score: score,
            tier: tier.to_string(),
        }
    }

    #[test]
    fn per_candidate_penalty_reorders_within_tier() {
        let candidates = vec![
            cand("src/foo.rs", 10.0, "P1"),
            cand("src/foo_test.rs", 10.0, "P1"),
        ];
        let signals = SignalBundle::default();
        // Task does NOT mention tests → test paths get 0.7, others 1.0.
        let penalty = test_penalty_fn(false, 0.7);
        let out = rerank(candidates, &signals, penalty);
        // foo.rs (no penalty) now outranks foo_test.rs despite equal RRF.
        assert_eq!(out[0].path, "src/foo.rs");
        assert_eq!(out[1].path, "src/foo_test.rs");
        assert!((out[0].rrf_score - 10.0).abs() < 1e-9);
        assert!((out[1].rrf_score - 7.0).abs() < 1e-9);
    }

    #[test]
    fn penalty_gated_off_when_task_mentions_tests() {
        let candidates = vec![
            cand("src/foo.rs", 10.0, "P1"),
            cand("src/foo_test.rs", 10.0, "P1"),
        ];
        let signals = SignalBundle::default();
        let penalty = test_penalty_fn(true, 0.7);
        let out = rerank(candidates, &signals, penalty);
        // Task mentions tests → penalty gated off; both keep full score;
        // tie broken by path asc.
        assert_eq!(out[0].path, "src/foo.rs");
        assert_eq!(out[1].path, "src/foo_test.rs");
        assert!((out[0].rrf_score - 10.0).abs() < 1e-9);
        assert!((out[1].rrf_score - 10.0).abs() < 1e-9);
    }

    #[test]
    fn tier_order_is_preserved() {
        let candidates = vec![
            cand("src/a.rs", 100.0, "P2"),
            cand("src/b.rs", 1.0, "P0"),
            cand("src/c.rs", 50.0, "P1"),
        ];
        let signals = SignalBundle::default();
        let out = rerank(candidates, &signals, |_| 1.0);
        let tiers: Vec<&str> = out.iter().map(|c| c.tier.as_str()).collect();
        assert_eq!(tiers, vec!["P0", "P1", "P2"]);
    }
}
