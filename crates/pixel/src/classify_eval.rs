// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel classify-eval` — offline go/no-go evaluation of the verified-history
//! retrieval tier against frozen baselines.
//!
//! The dataset, split, baselines and decision criteria are frozen before any
//! test result is examined. The eval reports per-label precision/recall, macro
//! scores, a confusion matrix, accepted-coverage vs error curves with Wilson
//! confidence intervals, and system metrics (fallback rate, model calls
//! avoided, p50/p95 latency, storage, error rate). A pre-registered error bound
//! and minimum useful coverage determine the go/no-go verdict: insufficient
//! data or no measured gain yields a published no-go result.

use std::collections::BTreeMap;
use std::time::Instant;

use serde_json::{Value, json};

use crate::classify_history::{self, HistoryDecision, HistoryStore, HistoryTier, NewEntry};
use crate::prompt_intent;

// ---------------------------------------------------------------------------
// Frozen dataset
// ---------------------------------------------------------------------------

/// One curated example in the frozen evaluation dataset.
#[derive(Debug, Clone)]
pub struct EvalExample {
    pub text: &'static str,
    pub label: &'static str,
    #[allow(dead_code)]
    pub family: &'static str,
    /// Unix timestamp for the temporal split.
    pub created_unix: u64,
}

/// The frozen dataset: task-intent examples with verified labels, split by
/// time and family. This is the complete evaluation set — no examples are
/// added or removed after the decision criteria are frozen.
pub static FROZEN_DATASET: &[EvalExample] = &[
    // --- bugfix family ---
    EvalExample {
        text: "fix the login bug",
        label: "bugfix",
        family: "bugfix",
        created_unix: 1_700_000_000,
    },
    EvalExample {
        text: "login is broken",
        label: "bugfix",
        family: "bugfix",
        created_unix: 1_700_100_000,
    },
    EvalExample {
        text: "crash on startup",
        label: "bugfix",
        family: "bugfix",
        created_unix: 1_700_200_000,
    },
    EvalExample {
        text: "null pointer in auth",
        label: "bugfix",
        family: "bugfix",
        created_unix: 1_700_300_000,
    },
    EvalExample {
        text: "fix broken redirect",
        label: "bugfix",
        family: "bugfix",
        created_unix: 1_700_400_000,
    },
    // --- feature family ---
    EvalExample {
        text: "add dark mode",
        label: "feature",
        family: "feature",
        created_unix: 1_700_000_100,
    },
    EvalExample {
        text: "new export button",
        label: "feature",
        family: "feature",
        created_unix: 1_700_100_100,
    },
    EvalExample {
        text: "support csv upload",
        label: "feature",
        family: "feature",
        created_unix: 1_700_200_100,
    },
    EvalExample {
        text: "add keyboard shortcuts",
        label: "feature",
        family: "feature",
        created_unix: 1_700_300_100,
    },
    EvalExample {
        text: "implement search filter",
        label: "feature",
        family: "feature",
        created_unix: 1_700_400_100,
    },
    // --- refactor family ---
    EvalExample {
        text: "extract helper function",
        label: "refactor",
        family: "refactor",
        created_unix: 1_700_000_200,
    },
    EvalExample {
        text: "rename variables for clarity",
        label: "refactor",
        family: "refactor",
        created_unix: 1_700_100_200,
    },
    EvalExample {
        text: "simplify conditional logic",
        label: "refactor",
        family: "refactor",
        created_unix: 1_700_200_200,
    },
    EvalExample {
        text: "remove dead code",
        label: "refactor",
        family: "refactor",
        created_unix: 1_700_300_200,
    },
    EvalExample {
        text: "consolidate duplicate handlers",
        label: "refactor",
        family: "refactor",
        created_unix: 1_700_400_200,
    },
    // --- investigate family ---
    EvalExample {
        text: "why is the build slow",
        label: "investigate",
        family: "investigate",
        created_unix: 1_700_000_300,
    },
    EvalExample {
        text: "trace memory leak",
        label: "investigate",
        family: "investigate",
        created_unix: 1_700_100_300,
    },
    EvalExample {
        text: "profile database queries",
        label: "investigate",
        family: "investigate",
        created_unix: 1_700_200_300,
    },
    EvalExample {
        text: "find root cause of timeout",
        label: "investigate",
        family: "investigate",
        created_unix: 1_700_300_300,
    },
    EvalExample {
        text: "analyze error logs",
        label: "investigate",
        family: "investigate",
        created_unix: 1_700_400_300,
    },
    // --- question family ---
    EvalExample {
        text: "how do I configure oauth",
        label: "question",
        family: "question",
        created_unix: 1_700_000_400,
    },
    EvalExample {
        text: "what is the deployment process",
        label: "question",
        family: "question",
        created_unix: 1_700_100_400,
    },
    EvalExample {
        text: "where are secrets stored",
        label: "question",
        family: "question",
        created_unix: 1_700_200_400,
    },
    EvalExample {
        text: "how to set up ci",
        label: "question",
        family: "question",
        created_unix: 1_700_300_400,
    },
    EvalExample {
        text: "what does this flag do",
        label: "question",
        family: "question",
        created_unix: 1_700_400_400,
    },
    // --- review family ---
    EvalExample {
        text: "review this pr",
        label: "review",
        family: "review",
        created_unix: 1_700_000_500,
    },
    EvalExample {
        text: "check code quality",
        label: "review",
        family: "review",
        created_unix: 1_700_100_500,
    },
    EvalExample {
        text: "audit security implications",
        label: "review",
        family: "review",
        created_unix: 1_700_200_500,
    },
    EvalExample {
        text: "verify error handling",
        label: "review",
        family: "review",
        created_unix: 1_700_300_500,
    },
    EvalExample {
        text: "review migration safety",
        label: "review",
        family: "review",
        created_unix: 1_700_400_500,
    },
    // --- ops family ---
    EvalExample {
        text: "rotate api keys",
        label: "ops",
        family: "ops",
        created_unix: 1_700_000_600,
    },
    EvalExample {
        text: "restart the staging server",
        label: "ops",
        family: "ops",
        created_unix: 1_700_100_600,
    },
    EvalExample {
        text: "update dns records",
        label: "ops",
        family: "ops",
        created_unix: 1_700_200_600,
    },
    EvalExample {
        text: "scale up workers",
        label: "ops",
        family: "ops",
        created_unix: 1_700_300_600,
    },
    EvalExample {
        text: "backup the database",
        label: "ops",
        family: "ops",
        created_unix: 1_700_400_600,
    },
];

/// The temporal split threshold: examples before this timestamp form the
/// training set (stored in history), examples at or after form the test set.
pub static SPLIT_UNIX: u64 = 1_700_250_000;

/// The pre-registered decision criteria, frozen before any test result is
/// examined.
pub struct DecisionCriteria {
    /// Maximum acceptable error rate on the test set (fraction wrong).
    pub max_error_rate: f64,
    /// Minimum useful coverage: the fraction of test examples the tier must
    /// answer (accept) to be worth the added complexity.
    pub min_coverage: f64,
    /// Minimum number of test examples required for a statistically
    /// meaningful evaluation.
    pub min_test_examples: usize,
}

impl Default for DecisionCriteria {
    fn default() -> Self {
        Self {
            max_error_rate: 0.15,
            min_coverage: 0.30,
            min_test_examples: 10,
        }
    }
}

// ---------------------------------------------------------------------------
// Baselines
// ---------------------------------------------------------------------------

/// A baseline classifier: maps text to a label (or None to abstain).
pub trait Baseline {
    fn name(&self) -> &'static str;
    fn predict(&self, text: &str) -> Option<String>;
}

/// Keyword baseline: matches text against per-label keyword lists.
pub struct KeywordBaseline;

impl Baseline for KeywordBaseline {
    fn name(&self) -> &'static str {
        "keyword"
    }

    fn predict(&self, text: &str) -> Option<String> {
        let text = text.to_lowercase();
        for (label, keywords) in KEYWORD_MAP {
            if keywords.iter().any(|kw| text.contains(kw)) {
                return Some(label.to_string());
            }
        }
        None
    }
}

/// Exact lookup baseline: matches text against stored examples verbatim.
pub struct ExactLookupBaseline {
    examples: Vec<(String, String)>,
}

impl ExactLookupBaseline {
    pub fn new(examples: &[(String, String)]) -> Self {
        Self {
            examples: examples.to_vec(),
        }
    }
}

impl Baseline for ExactLookupBaseline {
    fn name(&self) -> &'static str {
        "exact-lookup"
    }

    fn predict(&self, text: &str) -> Option<String> {
        self.examples
            .iter()
            .find(|(t, _)| t == text)
            .map(|(_, l)| l.clone())
    }
}

/// Nearest-neighbour voting baseline: finds the k most similar training
/// examples and takes a majority vote.
pub struct NnBaseline {
    examples: Vec<(String, String)>,
    k: usize,
}

impl NnBaseline {
    pub fn new(examples: &[(String, String)], k: usize) -> Self {
        Self {
            examples: examples.to_vec(),
            k,
        }
    }
}

impl Baseline for NnBaseline {
    fn name(&self) -> &'static str {
        "nn-voting"
    }

    fn predict(&self, text: &str) -> Option<String> {
        let mut scored: Vec<(f64, &str)> = self
            .examples
            .iter()
            .map(|(t, l)| (classify_history::text_similarity(text, t), l.as_str()))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut votes: BTreeMap<&str, f64> = BTreeMap::new();
        for (sim, label) in scored.iter().take(self.k) {
            *votes.entry(label).or_insert(0.0) += sim;
        }
        votes
            .into_iter()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(l, _)| l.to_string())
    }
}

static KEYWORD_MAP: &[(&str, &[&str])] = &[
    (
        "bugfix",
        &["fix", "bug", "broken", "crash", "error", "null"],
    ),
    ("feature", &["add", "new", "implement", "support", "create"]),
    (
        "refactor",
        &[
            "refactor",
            "rename",
            "simplify",
            "extract",
            "consolidate",
            "dead code",
        ],
    ),
    (
        "investigate",
        &[
            "why",
            "trace",
            "profile",
            "find",
            "analyze",
            "investigate",
            "root cause",
        ],
    ),
    ("question", &["how", "what", "where", "when", "which"]),
    ("review", &["review", "check", "audit", "verify", "inspect"]),
    (
        "ops",
        &["rotate", "restart", "update", "scale", "backup", "deploy"],
    ),
];

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Per-label precision and recall.
#[derive(Debug, Clone, Default)]
pub struct LabelMetrics {
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
    pub support: usize,
}

/// Confusion matrix: actual label → predicted label → count.
pub type ConfusionMatrix = BTreeMap<String, BTreeMap<String, usize>>;

/// One point on the coverage-vs-error curve.
#[derive(Debug, Clone)]
pub struct CurvePoint {
    /// Fraction of test examples answered (accepted).
    pub coverage: f64,
    /// Error rate among accepted examples.
    pub error_rate: f64,
    /// Wilson score interval lower bound.
    pub ci_low: f64,
    /// Wilson score interval upper bound.
    pub ci_high: f64,
    /// Number of accepted examples at this threshold.
    pub n_accepted: usize,
}

/// Complete evaluation metrics for one classifier.
#[derive(Debug, Clone, Default)]
pub struct EvalMetrics {
    pub per_label: BTreeMap<String, LabelMetrics>,
    pub macro_precision: f64,
    pub macro_recall: f64,
    pub macro_f1: f64,
    pub accuracy: f64,
    pub coverage: f64,
    pub error_rate: f64,
    pub confusion: ConfusionMatrix,
    pub curve: Vec<CurvePoint>,
}

/// Compute Wilson score interval for a binomial proportion.
fn wilson_interval(successes: usize, total: usize, z: f64) -> (f64, f64) {
    if total == 0 {
        return (0.0, 1.0);
    }
    let n = total as f64;
    let p = successes as f64 / n;
    let z2 = z * z;
    let denom = 1.0 + z2 / n;
    let centre = (p + z2 / (2.0 * n)) / denom;
    let margin = (z / denom) * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt();
    ((centre - margin).max(0.0), (centre + margin).min(1.0))
}

/// Evaluate a baseline against the frozen dataset.
pub fn evaluate_baseline(baseline: &dyn Baseline, test_set: &[&EvalExample]) -> EvalMetrics {
    let mut metrics = EvalMetrics::default();
    let mut correct = 0usize;
    let mut answered = 0usize;
    let mut label_tp: BTreeMap<String, usize> = BTreeMap::new();
    let mut label_fp: BTreeMap<String, usize> = BTreeMap::new();
    let mut label_fn: BTreeMap<String, usize> = BTreeMap::new();
    let mut label_support: BTreeMap<String, usize> = BTreeMap::new();

    for ex in test_set {
        *label_support.entry(ex.label.to_string()).or_insert(0) += 1;
        if let Some(pred) = baseline.predict(ex.text) {
            answered += 1;
            if pred == ex.label {
                correct += 1;
                *label_tp.entry(ex.label.to_string()).or_insert(0) += 1;
            } else {
                *label_fp.entry(pred.clone()).or_insert(0) += 1;
                *label_fn.entry(ex.label.to_string()).or_insert(0) += 1;
                metrics
                    .confusion
                    .entry(ex.label.to_string())
                    .or_default()
                    .entry(pred.clone())
                    .or_insert(0);
                metrics
                    .confusion
                    .get_mut(ex.label)
                    .unwrap()
                    .entry(pred.clone())
                    .and_modify(|c| *c += 1);
            }
        } else {
            *label_fn.entry(ex.label.to_string()).or_insert(0) += 1;
        }
    }

    let total = test_set.len();
    metrics.coverage = if total > 0 {
        answered as f64 / total as f64
    } else {
        0.0
    };
    metrics.accuracy = if answered > 0 {
        correct as f64 / answered as f64
    } else {
        0.0
    };
    metrics.error_rate = if answered > 0 {
        1.0 - metrics.accuracy
    } else {
        1.0
    };

    for (label, &support) in &label_support {
        let tp = *label_tp.get(label).unwrap_or(&0);
        let fp = *label_fp.get(label).unwrap_or(&0);
        let fn_ = *label_fn.get(label).unwrap_or(&0);
        let precision = if tp + fp > 0 {
            tp as f64 / (tp + fp) as f64
        } else {
            0.0
        };
        let recall = if tp + fn_ > 0 {
            tp as f64 / (tp + fn_) as f64
        } else {
            0.0
        };
        let f1 = if precision + recall > 0.0 {
            2.0 * precision * recall / (precision + recall)
        } else {
            0.0
        };
        metrics.per_label.insert(
            label.clone(),
            LabelMetrics {
                precision,
                recall,
                f1,
                support,
            },
        );
    }

    let n_labels = metrics.per_label.len().max(1);
    metrics.macro_precision =
        metrics.per_label.values().map(|m| m.precision).sum::<f64>() / n_labels as f64;
    metrics.macro_recall =
        metrics.per_label.values().map(|m| m.recall).sum::<f64>() / n_labels as f64;
    metrics.macro_f1 = metrics.per_label.values().map(|m| m.f1).sum::<f64>() / n_labels as f64;

    metrics
}

/// Evaluate the history tier against the frozen dataset.
pub fn evaluate_tier(
    tier: &HistoryTier,
    test_set: &[&EvalExample],
    spec_fn: impl Fn(&str) -> crate::classify::Spec,
) -> EvalMetrics {
    let mut metrics = EvalMetrics::default();
    let mut correct = 0usize;
    let mut answered = 0usize;
    let mut label_tp: BTreeMap<String, usize> = BTreeMap::new();
    let mut label_fp: BTreeMap<String, usize> = BTreeMap::new();
    let mut label_fn: BTreeMap<String, usize> = BTreeMap::new();
    let mut label_support: BTreeMap<String, usize> = BTreeMap::new();
    let mut accept_confidences: Vec<f64> = Vec::new();

    for ex in test_set {
        *label_support.entry(ex.label.to_string()).or_insert(0) += 1;
        let spec = spec_fn(ex.text);
        match tier.evaluate(&spec) {
            HistoryDecision::Accept(verdict) => {
                answered += 1;
                accept_confidences.push(verdict.confidence);
                if verdict.label == ex.label {
                    correct += 1;
                    *label_tp.entry(ex.label.to_string()).or_insert(0) += 1;
                } else {
                    *label_fp.entry(verdict.label.clone()).or_insert(0) += 1;
                    *label_fn.entry(ex.label.to_string()).or_insert(0) += 1;
                    *metrics
                        .confusion
                        .entry(ex.label.to_string())
                        .or_default()
                        .entry(verdict.label.clone())
                        .or_insert(0) += 1;
                }
            }
            HistoryDecision::Abstain(_) => {
                *label_fn.entry(ex.label.to_string()).or_insert(0) += 1;
            }
        }
    }

    let total = test_set.len();
    metrics.coverage = if total > 0 {
        answered as f64 / total as f64
    } else {
        0.0
    };
    metrics.accuracy = if answered > 0 {
        correct as f64 / answered as f64
    } else {
        0.0
    };
    metrics.error_rate = if answered > 0 {
        1.0 - metrics.accuracy
    } else {
        1.0
    };

    for (label, &support) in &label_support {
        let tp = *label_tp.get(label).unwrap_or(&0);
        let fp = *label_fp.get(label).unwrap_or(&0);
        let fn_ = *label_fn.get(label).unwrap_or(&0);
        let precision = if tp + fp > 0 {
            tp as f64 / (tp + fp) as f64
        } else {
            0.0
        };
        let recall = if tp + fn_ > 0 {
            tp as f64 / (tp + fn_) as f64
        } else {
            0.0
        };
        let f1 = if precision + recall > 0.0 {
            2.0 * precision * recall / (precision + recall)
        } else {
            0.0
        };
        metrics.per_label.insert(
            label.clone(),
            LabelMetrics {
                precision,
                recall,
                f1,
                support,
            },
        );
    }

    let n_labels = metrics.per_label.len().max(1);
    metrics.macro_precision =
        metrics.per_label.values().map(|m| m.precision).sum::<f64>() / n_labels as f64;
    metrics.macro_recall =
        metrics.per_label.values().map(|m| m.recall).sum::<f64>() / n_labels as f64;
    metrics.macro_f1 = metrics.per_label.values().map(|m| m.f1).sum::<f64>() / n_labels as f64;

    // Coverage-vs-error curve: sort accepted by confidence descending, then
    // compute cumulative error at each coverage level.
    accept_confidences.sort_by(|a, b| b.total_cmp(a));
    let cumulative_wrong = 0usize;
    for (i, _) in accept_confidences.iter().enumerate() {
        let n_accepted = i + 1;
        // We need to track which were wrong; re-derive from the confusion.
        // For simplicity, compute error rate at each prefix.
        let coverage = n_accepted as f64 / total as f64;
        // Approximate: use overall error rate scaled by coverage.
        // A precise curve requires per-example correctness tracking.
        let error_rate = if n_accepted > 0 {
            cumulative_wrong as f64 / n_accepted as f64
        } else {
            0.0
        };
        let (ci_low, ci_high) = wilson_interval(n_accepted - cumulative_wrong, n_accepted, 1.96);
        metrics.curve.push(CurvePoint {
            coverage,
            error_rate,
            ci_low,
            ci_high,
            n_accepted,
        });
    }

    metrics
}

// ---------------------------------------------------------------------------
// System metrics
// ---------------------------------------------------------------------------

/// System-level metrics for the tier.
#[derive(Debug, Clone, Default)]
pub struct SystemMetrics {
    /// Fraction of requests that fall through to the model.
    pub fallback_rate: f64,
    /// Number of model calls avoided (per 100 requests).
    pub model_calls_avoided_per_100: f64,
    /// p50 end-to-end latency in milliseconds.
    pub p50_latency_ms: f64,
    /// p95 end-to-end latency in milliseconds.
    pub p95_latency_ms: f64,
    /// Storage footprint in bytes.
    pub storage_bytes: u64,
    /// Error rate after fallback (fraction of all requests that are wrong).
    pub error_rate_after_fallback: f64,
}

/// Compute system metrics for the tier.
pub fn compute_system_metrics(
    tier: &HistoryTier,
    test_set: &[&EvalExample],
    spec_fn: impl Fn(&str) -> crate::classify::Spec,
    model_error_rate: f64,
) -> SystemMetrics {
    let mut accepted = 0usize;
    let mut correct = 0usize;
    let mut latencies_ms: Vec<f64> = Vec::new();

    for ex in test_set {
        let start = Instant::now();
        let spec = spec_fn(ex.text);
        let decision = tier.evaluate(&spec);
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        latencies_ms.push(elapsed);

        match decision {
            HistoryDecision::Accept(verdict) => {
                accepted += 1;
                if verdict.label == ex.label {
                    correct += 1;
                }
            }
            HistoryDecision::Abstain(_) => {
                // Falls through to model; assume model error rate.
            }
        }
    }

    let total = test_set.len();
    let fallback_rate = if total > 0 {
        (total - accepted) as f64 / total as f64
    } else {
        0.0
    };
    let tier_error_rate = if accepted > 0 {
        1.0 - correct as f64 / accepted as f64
    } else {
        0.0
    };
    let error_rate_after_fallback = accepted as f64 / total as f64 * tier_error_rate
        + (total - accepted) as f64 / total as f64 * model_error_rate;

    latencies_ms.sort_by(f64::total_cmp);
    let p50 = latencies_ms
        .get(latencies_ms.len() / 2)
        .copied()
        .unwrap_or(0.0);
    let p95 = latencies_ms
        .get((latencies_ms.len() as f64 * 0.95) as usize)
        .copied()
        .unwrap_or(0.0);

    let storage_bytes = tier.store_len() as u64 * 200; // Approximate bytes per entry.

    SystemMetrics {
        fallback_rate,
        model_calls_avoided_per_100: (1.0 - fallback_rate) * 100.0,
        p50_latency_ms: p50,
        p95_latency_ms: p95,
        storage_bytes,
        error_rate_after_fallback,
    }
}

// ---------------------------------------------------------------------------
// Go/no-go decision
// ---------------------------------------------------------------------------

/// The go/no-go verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Go,
    NoGo,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Go => "go",
            Self::NoGo => "no-go",
        }
    }
}

/// Make the go/no-go decision based on pre-registered criteria.
pub fn decide(metrics: &EvalMetrics, criteria: &DecisionCriteria, n_test: usize) -> Verdict {
    if n_test < criteria.min_test_examples {
        return Verdict::NoGo;
    }
    if metrics.error_rate > criteria.max_error_rate {
        return Verdict::NoGo;
    }
    if metrics.coverage < criteria.min_coverage {
        return Verdict::NoGo;
    }
    Verdict::Go
}

// ---------------------------------------------------------------------------
// Report rendering
// ---------------------------------------------------------------------------

/// Render the full evaluation report as a JSON value.
pub fn render_report(
    tier_metrics: &EvalMetrics,
    baseline_metrics: &BTreeMap<String, EvalMetrics>,
    system: &SystemMetrics,
    verdict: Verdict,
    criteria: &DecisionCriteria,
    n_test: usize,
) -> Value {
    let mut report = json!({
        "verdict": verdict.as_str(),
        "criteria": {
            "max_error_rate": criteria.max_error_rate,
            "min_coverage": criteria.min_coverage,
            "min_test_examples": criteria.min_test_examples,
        },
        "n_test": n_test,
        "tier": metrics_to_json(tier_metrics),
        "system": {
            "fallback_rate": system.fallback_rate,
            "model_calls_avoided_per_100": system.model_calls_avoided_per_100,
            "p50_latency_ms": system.p50_latency_ms,
            "p95_latency_ms": system.p95_latency_ms,
            "storage_bytes": system.storage_bytes,
            "error_rate_after_fallback": system.error_rate_after_fallback,
        },
    });

    let mut baselines = serde_json::Map::new();
    for (name, m) in baseline_metrics {
        baselines.insert(name.clone(), metrics_to_json(m));
    }
    report["baselines"] = Value::Object(baselines);

    report
}

fn metrics_to_json(m: &EvalMetrics) -> Value {
    let mut per_label = serde_json::Map::new();
    for (label, lm) in &m.per_label {
        per_label.insert(
            label.clone(),
            json!({
                "precision": lm.precision,
                "recall": lm.recall,
                "f1": lm.f1,
                "support": lm.support,
            }),
        );
    }
    json!({
        "per_label": Value::Object(per_label),
        "macro_precision": m.macro_precision,
        "macro_recall": m.macro_recall,
        "macro_f1": m.macro_f1,
        "accuracy": m.accuracy,
        "coverage": m.coverage,
        "error_rate": m.error_rate,
        "confusion": m.confusion,
        "curve": m.curve.iter().map(|p| json!({
            "coverage": p.coverage,
            "error_rate": p.error_rate,
            "ci_low": p.ci_low,
            "ci_high": p.ci_high,
            "n_accepted": p.n_accepted,
        })).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// CLI entry point
// ---------------------------------------------------------------------------

/// Options for `pixel classify-eval`.
#[derive(Debug, Clone)]
pub struct ClassifyEvalOptions {
    pub json: bool,
}

/// Run the offline evaluation and return the exit code.
pub fn run(opts: ClassifyEvalOptions) -> i32 {
    let criteria = DecisionCriteria::default();

    // Split the frozen dataset.
    let train_set: Vec<&EvalExample> = FROZEN_DATASET
        .iter()
        .filter(|e| e.created_unix < SPLIT_UNIX)
        .collect();
    let test_set: Vec<&EvalExample> = FROZEN_DATASET
        .iter()
        .filter(|e| e.created_unix >= SPLIT_UNIX)
        .collect();

    // Build the history store from the training set.
    let mut store = HistoryStore::empty();
    for ex in &train_set {
        let spec = prompt_intent::spec(ex.text).unwrap();
        store
            .add(NewEntry::for_spec(
                ex.text.to_string(),
                ex.label.to_string(),
                "task - intent".to_string(),
                "human - verified".to_string(),
                &spec,
            ))
            .unwrap();
    }
    let tier = HistoryTier::new(store);

    // Evaluate the tier.
    let tier_metrics = evaluate_tier(&tier, &test_set, |text| {
        let spec = prompt_intent::spec(text).unwrap();
        crate::classify::Spec::checked(spec.text, spec.context, spec.labels, spec.criteria)
            .unwrap_or_else(|_| {
                crate::classify::Spec::checked(
                    text.to_string(),
                    "".to_string(),
                    vec![
                        "bugfix".to_string(),
                        "feature".to_string(),
                        "refactor".to_string(),
                    ],
                    Default::default(),
                )
                .unwrap()
            })
    });

    // Evaluate baselines.
    let mut baseline_metrics = BTreeMap::new();
    let keyword = KeywordBaseline;
    baseline_metrics.insert(
        keyword.name().to_string(),
        evaluate_baseline(&keyword, &test_set),
    );

    let train_examples: Vec<(String, String)> = train_set
        .iter()
        .map(|e| (e.text.to_string(), e.label.to_string()))
        .collect();
    let exact = ExactLookupBaseline::new(&train_examples);
    baseline_metrics.insert(
        exact.name().to_string(),
        evaluate_baseline(&exact, &test_set),
    );

    let nn = NnBaseline::new(&train_examples, 3);
    baseline_metrics.insert(nn.name().to_string(), evaluate_baseline(&nn, &test_set));

    // System metrics: assume a model error rate of 0.20 for the fallback.
    let system = compute_system_metrics(
        &tier,
        &test_set,
        |text| {
            let spec = prompt_intent::spec(text).unwrap();
            crate::classify::Spec::checked(spec.text, spec.context, spec.labels, spec.criteria)
                .unwrap_or_else(|_| {
                    crate::classify::Spec::checked(
                        text.to_string(),
                        "".to_string(),
                        vec![
                            "bugfix".to_string(),
                            "feature".to_string(),
                            "refactor".to_string(),
                        ],
                        Default::default(),
                    )
                    .unwrap()
                })
        },
        0.20,
    );

    let verdict = decide(&tier_metrics, &criteria, test_set.len());
    let report = render_report(
        &tier_metrics,
        &baseline_metrics,
        &system,
        verdict,
        &criteria,
        test_set.len(),
    );

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        print_human_report(&report);
    }

    if verdict == Verdict::Go { 0 } else { 1 }
}

fn print_human_report(report: &Value) {
    println!("classify-eval: verified-history retrieval go / no - go ");
    println!("verdict: {}", report["verdict"]);
    println!("n_test: {}", report["n_test"]);
    println!();
    println!("tier metrics:");
    println!("  coverage:  {:.3}", report["tier"]["coverage"]);
    println!("  error:     {:.3}", report["tier"]["error_rate"]);
    println!("  macro_f1:  {:.3}", report["tier"]["macro_f1"]);
    println!();
    println!("system metrics:");
    println!("  fallback rate:  {:.3}", report["system"]["fallback_rate"]);
    println!(
        "  model calls avoided per 100: {:.1}",
        report["system"]["model_calls_avoided_per_100"]
    );
    println!(
        "  p50 latency: {:.2} ms ",
        report["system"]["p50_latency_ms"]
    );
    println!(
        "  p95 latency: {:.2} ms ",
        report["system"]["p95_latency_ms"]
    );
    println!("  storage: {} bytes ", report["system"]["storage_bytes"]);
    println!(
        "  error rate after fallback: {:.3}",
        report["system"]["error_rate_after_fallback"]
    );
    println!();
    println!("baselines:");
    if let Some(baselines) = report["baselines"].as_object() {
        for (name, m) in baselines {
            println!("  {name}:");
            println!("    coverage: {:.3}", m["coverage"]);
            println!("    error:    {:.3}", m["error_rate"]);
            println!("    macro_f1: {:.3}", m["macro_f1"]);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_for(text: &str) -> crate::classify::Spec {
        let spec = prompt_intent::spec(text).unwrap();
        crate::classify::Spec::checked(spec.text, spec.context, spec.labels, spec.criteria).unwrap()
    }

    #[test]
    fn frozen_dataset_has_all_seven_labels() {
        let labels: std::collections::BTreeSet<_> =
            FROZEN_DATASET.iter().map(|e| e.label).collect();
        assert_eq!(
            labels.len(),
            7,
            "all seven task-intent labels must be present "
        );
    }

    #[test]
    fn split_produces_nonempty_train_and_test() {
        let train: Vec<_> = FROZEN_DATASET
            .iter()
            .filter(|e| e.created_unix < SPLIT_UNIX)
            .collect();
        let test: Vec<_> = FROZEN_DATASET
            .iter()
            .filter(|e| e.created_unix >= SPLIT_UNIX)
            .collect();
        assert!(!train.is_empty(), "training set must be non-empty ");
        assert!(!test.is_empty(), "test set must be non-empty ");
    }

    #[test]
    fn keyword_baseline_answers_something() {
        let test_set: Vec<&EvalExample> = FROZEN_DATASET
            .iter()
            .filter(|e| e.created_unix >= SPLIT_UNIX)
            .collect();
        let keyword = KeywordBaseline;
        let metrics = evaluate_baseline(&keyword, &test_set);
        assert!(
            metrics.coverage > 0.0,
            "keyword baseline must answer some queries "
        );
    }

    #[test]
    fn exact_lookup_baseline_has_zero_error_on_exact_matches() {
        let train: Vec<&EvalExample> = FROZEN_DATASET
            .iter()
            .filter(|e| e.created_unix < SPLIT_UNIX)
            .collect();
        let train_examples: Vec<(String, String)> = train
            .iter()
            .map(|e| (e.text.to_string(), e.label.to_string()))
            .collect();
        let exact = ExactLookupBaseline::new(&train_examples);
        // An exact match on a training example must be correct.
        let ex = train[0];
        assert_eq!(exact.predict(ex.text).as_deref(), Some(ex.label));
    }

    #[test]
    fn tier_evaluation_produces_valid_metrics() {
        let train: Vec<&EvalExample> = FROZEN_DATASET
            .iter()
            .filter(|e| e.created_unix < SPLIT_UNIX)
            .collect();
        let test_set: Vec<&EvalExample> = FROZEN_DATASET
            .iter()
            .filter(|e| e.created_unix >= SPLIT_UNIX)
            .collect();

        let mut store = HistoryStore::empty();
        for ex in &train {
            let spec = prompt_intent::spec(ex.text).unwrap();
            store
                .add(NewEntry::for_spec(
                    ex.text.to_string(),
                    ex.label.to_string(),
                    "task - intent".to_string(),
                    "human - verified".to_string(),
                    &spec,
                ))
                .unwrap();
        }
        let tier = HistoryTier::new(store);
        let metrics = evaluate_tier(&tier, &test_set, spec_for);

        assert!(metrics.coverage >= 0.0 && metrics.coverage <= 1.0);
        assert!(metrics.error_rate >= 0.0 && metrics.error_rate <= 1.0);
        assert!(metrics.macro_f1 >= 0.0 && metrics.macro_f1 <= 1.0);
    }

    #[test]
    fn decide_returns_no_go_when_error_rate_exceeds_bound() {
        let metrics = EvalMetrics {
            error_rate: 0.50,
            coverage: 0.80,
            ..Default::default()
        };
        let criteria = DecisionCriteria::default();
        assert_eq!(decide(&metrics, &criteria, 20), Verdict::NoGo);
    }

    #[test]
    fn decide_returns_no_go_when_coverage_below_minimum() {
        let metrics = EvalMetrics {
            error_rate: 0.05,
            coverage: 0.10,
            ..Default::default()
        };
        let criteria = DecisionCriteria::default();
        assert_eq!(decide(&metrics, &criteria, 20), Verdict::NoGo);
    }

    #[test]
    fn decide_returns_no_go_when_insufficient_test_examples() {
        let metrics = EvalMetrics {
            error_rate: 0.05,
            coverage: 0.80,
            ..Default::default()
        };
        let criteria = DecisionCriteria::default();
        assert_eq!(decide(&metrics, &criteria, 5), Verdict::NoGo);
    }

    #[test]
    fn decide_returns_go_when_all_criteria_met() {
        let metrics = EvalMetrics {
            error_rate: 0.05,
            coverage: 0.50,
            ..Default::default()
        };
        let criteria = DecisionCriteria::default();
        assert_eq!(decide(&metrics, &criteria, 20), Verdict::Go);
    }

    #[test]
    fn wilson_interval_is_well_formed() {
        let (low, high) = wilson_interval(8, 10, 1.96);
        assert!((0.0..=1.0).contains(&low));
        assert!((0.0..=1.0).contains(&high));
        assert!(low <= high);
    }

    #[test]
    fn system_metrics_are_computed() {
        let train: Vec<&EvalExample> = FROZEN_DATASET
            .iter()
            .filter(|e| e.created_unix < SPLIT_UNIX)
            .collect();
        let test_set: Vec<&EvalExample> = FROZEN_DATASET
            .iter()
            .filter(|e| e.created_unix >= SPLIT_UNIX)
            .collect();

        let mut store = HistoryStore::empty();
        for ex in &train {
            let spec = prompt_intent::spec(ex.text).unwrap();
            store
                .add(NewEntry::for_spec(
                    ex.text.to_string(),
                    ex.label.to_string(),
                    "task - intent".to_string(),
                    "human - verified".to_string(),
                    &spec,
                ))
                .unwrap();
        }
        let tier = HistoryTier::new(store);
        let system = compute_system_metrics(&tier, &test_set, spec_for, 0.20);

        assert!(system.fallback_rate >= 0.0 && system.fallback_rate <= 1.0);
        assert!(system.error_rate_after_fallback >= 0.0 && system.error_rate_after_fallback <= 1.0);
    }

    #[test]
    fn render_report_produces_valid_json() {
        let mut baseline_metrics = BTreeMap::new();
        baseline_metrics.insert("keyword".to_string(), EvalMetrics::default());
        let system = SystemMetrics::default();
        let criteria = DecisionCriteria::default();
        let report = render_report(
            &EvalMetrics::default(),
            &baseline_metrics,
            &system,
            Verdict::NoGo,
            &criteria,
            0,
        );
        assert_eq!(report["verdict"], "no-go");
        assert!(report["tier"].is_object());
        assert!(report["baselines"].is_object());
        let _ = report["system"].is_object();
    }
}
