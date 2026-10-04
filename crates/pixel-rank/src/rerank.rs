// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Engine 3 shared reranker — reorders candidates *within* their tier using
//! activity + session signals. Never promotes across tiers (protects the
//! closed-world claim).
//!
//! The single [`rerank`] helper is used by both `targets` (within-tier) and
//! `resolve` (candidate ordering): both feed it a list of
//! (path, rrf_score, tier) and get back the same list reordered inside each
//! tier, ties broken by path ascending.

use std::collections::HashMap;

use crate::signals::{SignalBundle, SignalOptions};

/// The three coefficients the rerank formula applies, read from the
/// [`SignalOptions`] the signals were computed with — the only table of
/// rerank weights in the workspace (no literals duplicated in the formula).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RerankWeights {
    /// `activity_norm` coefficient (`SignalOptions::activity_weight`, 0.15).
    pub activity: f64,
    /// `session_norm` coefficient (`SignalOptions::session_weight`, 0.35).
    pub session: f64,
    /// `fan_in_norm` coefficient (`SignalOptions::fan_in_weight`, 0.20).
    pub fan_in: f64,
}

impl From<&SignalOptions> for RerankWeights {
    fn from(opts: &SignalOptions) -> Self {
        RerankWeights {
            activity: opts.activity_weight,
            session: opts.session_weight,
            fan_in: opts.fan_in_weight,
        }
    }
}

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
/// `final = rrf_score * (1 + activity_weight*activity_norm +
/// session_weight*session_norm + fan_in_weight*fan_in_norm) * penalty(path)`,
/// with the three coefficients taken from `weights` (the `SignalOptions`
/// table; its defaults are PLAN.md's 0.15/0.35/0.20).
///
/// `penalty` is a per-candidate multiplier (e.g. a test penalty that only
/// applies to test paths when the task does NOT mention tests — see
/// [`crate::signals::test_penalty_fn`]). Reorders within each tier only; tier
/// order (P0, P1, P2) and the candidate set are preserved. Deterministic:
/// ties broken by path ascending.
pub fn rerank<F>(
    candidates: Vec<RankedCandidate>,
    signals: &SignalBundle,
    weights: &RerankWeights,
    penalty: F,
) -> Vec<RankedCandidate>
where
    F: Fn(&str) -> f64,
{
    let activity = &signals.activity;
    let session = &signals.session;
    let fan_in = &signals.fan_in;

    let mut out: Vec<RankedCandidate> = candidates
        .into_iter()
        .map(|mut c| {
            let act = activity.get(&c.path).copied().unwrap_or(0.0);
            let ses = session.get(&c.path).copied().unwrap_or(0.0);
            let fanin = fan_in.get(&c.path).copied().unwrap_or(0.0);
            c.rrf_score = c.rrf_score
                * (1.0 + weights.activity * act + weights.session * ses + weights.fan_in * fanin)
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
    weights: &RerankWeights,
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
    let reordered = rerank(candidates, signals, weights, penalty);
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
    use crate::signals::{SessionEvent, SessionEventKind, score_signals, test_penalty_fn};

    /// One bundle with every channel present on both candidates: activity and
    /// session maxed on `src/b.rs` only, and fan-in counts (`1` for `a.rs`,
    /// `2` for `b.rs`) that normalize to 0.5 and 1.0. An exact final score
    /// then reads back each coefficient of the formula.
    fn signals_with_b_hot() -> SignalBundle {
        // `SignalOptions::default().session_window_ms` is 24 h and the event
        // is 60 s old; both timestamps are literals (no relative subtraction
        // between them) so a mutated `now` or event time leaves the window.
        let opts = SignalOptions {
            now_ms: 1_700_000_000_000,
            ..Default::default()
        };
        let activity_raw: HashMap<String, f64> =
            [("src/b.rs".to_string(), 1.0)].into_iter().collect();
        let fan_in_raw: HashMap<String, u64> =
            [("src/a.rs".to_string(), 1), ("src/b.rs".to_string(), 2)]
                .into_iter()
                .collect();
        let events = [SessionEvent {
            ts_ms: 1_699_999_940_000, // 60 s before `opts.now_ms`
            kind: SessionEventKind::Edit,
            path: "src/b.rs".to_string(),
            detail: None,
        }];
        score_signals(
            &activity_raw,
            &[],
            &events,
            &[],
            &fan_in_raw,
            &["src/a.rs".to_string(), "src/b.rs".to_string()],
            &opts,
        )
    }

    fn cand(path: &str, score: f64, tier: &str) -> RankedCandidate {
        RankedCandidate {
            id: 0,
            path: path.to_string(),
            rrf_score: score,
            tier: tier.to_string(),
        }
    }

    /// Two candidates tied on RRF; the same bundle reranked with the default
    /// table, a tuned one, and an all-zero one.
    fn tied_candidates() -> Vec<RankedCandidate> {
        vec![cand("src/a.rs", 10.0, "P1"), cand("src/b.rs", 10.0, "P1")]
    }

    #[test]
    fn rerank_applies_the_tunable_weights_read_from_signal_options() {
        let signals = signals_with_b_hot();

        // PLAN.md's defaults, read from `SignalOptions::default()`:
        // b.rs 10 * (1 + 0.15 + 0.35 + 0.20) = 17.0,
        // a.rs 10 * (1 + 0.20 * 0.5) = 11.0.
        let plan = RerankWeights::from(&SignalOptions::default());
        let out = rerank(tied_candidates(), &signals, &plan, |_| 1.0);
        assert_eq!(out[0].path, "src/b.rs");
        assert!((out[0].rrf_score - 17.0).abs() < 1e-9, "{out:?}");
        assert!((out[1].rrf_score - 11.0).abs() < 1e-9, "{out:?}");

        // A tuned table changes every term of the same formula:
        // b.rs 10 * (1 + 0.5 + 0.25 + 0.125) = 18.75,
        // a.rs 10 * (1 + 0.125 * 0.5) = 10.625.
        let tuned = RerankWeights::from(&SignalOptions {
            activity_weight: 0.5,
            session_weight: 0.25,
            fan_in_weight: 0.125,
            ..Default::default()
        });
        let out = rerank(tied_candidates(), &signals, &tuned, |_| 1.0);
        assert_eq!(out[0].path, "src/b.rs");
        assert!((out[0].rrf_score - 18.75).abs() < 1e-9, "{out:?}");
        assert!((out[1].rrf_score - 10.625).abs() < 1e-9, "{out:?}");

        // Every coefficient off leaves the RRF tie the signals were meant to
        // break: the opposite order, and no score change at all.
        let off = RerankWeights {
            activity: 0.0,
            session: 0.0,
            fan_in: 0.0,
        };
        let out = rerank(tied_candidates(), &signals, &off, |_| 1.0);
        assert_eq!(out[0].path, "src/a.rs");
        assert_eq!(out[1].path, "src/b.rs");
        assert!((out[0].rrf_score - 10.0).abs() < 1e-9, "{out:?}");
        assert!((out[1].rrf_score - 10.0).abs() < 1e-9, "{out:?}");
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
        let out = rerank(
            candidates,
            &signals,
            &RerankWeights::from(&SignalOptions::default()),
            penalty,
        );
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
        let out = rerank(
            candidates,
            &signals,
            &RerankWeights::from(&SignalOptions::default()),
            penalty,
        );
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
        let out = rerank(
            candidates,
            &signals,
            &RerankWeights::from(&SignalOptions::default()),
            |_| 1.0,
        );
        let tiers: Vec<&str> = out.iter().map(|c| c.tier.as_str()).collect();
        assert_eq!(tiers, vec!["P0", "P1", "P2"]);
    }
}
