// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Typed outcomes of the `meaning` request: natural-language retrieval over
//! code chunks whose embeddings stay resident in the daemon.
//!
//! A hit is a lead, never a verdict: embedding similarity ranks chunks, it
//! does not separate related code from unrelated code, so the result names
//! that in `caps` (which the envelope mirrors as `epistemics` and warnings).
//! When the vectors are not ready the answer is `Unavailable` with the
//! reason, never a slow or empty `Ready`: the caller falls back.

use serde::{Deserialize, Serialize};

/// One ranked lead: the best chunk of a file for the question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeaningHit {
    /// Repository-relative path.
    pub path: String,
    /// Inclusive 1-based line range of the chunk.
    pub start_line: u32,
    pub end_line: u32,
    /// The symbol the chunk belongs to, when it belongs to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// The fused ranking score that orders the hits (semantic rank plus
    /// lexical rank); comparable within one answer, not across questions.
    pub score: f64,
    /// The head of the chunk on one line, 160 characters at most.
    pub snippet: String,
}

/// What the resident vectors cover and cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeaningPool {
    /// The embedding model the vectors came from.
    pub model: String,
    pub dims: usize,
    /// Files and chunks held in memory.
    pub files: usize,
    pub chunks: usize,
    /// Eligible files under the root, whether held or not.
    pub eligible_files: usize,
    /// The publication generation the vectors were built for.
    pub generation: u64,
    /// Milliseconds since they were built.
    pub age_ms: u64,
    /// Approximate memory held, in bytes.
    pub resident_bytes: u64,
}

/// Why the resident vectors cannot answer now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeaningUnavailableReason {
    /// Nothing built and nothing building.
    Cold,
    /// The first build is running.
    Warming,
    /// Built for an older generation, and no rebuild is running yet.
    Stale,
    /// Built for an older generation; the rebuild is running.
    Refreshing,
    /// The embedding model is not on disk, and this request never downloads.
    ModelMissing,
    /// The model failed to load, or the build failed; see `detail`.
    Failed,
}

impl MeaningUnavailableReason {
    /// The reason as the wire spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Warming => "warming",
            Self::Stale => "stale",
            Self::Refreshing => "refreshing",
            Self::ModelMissing => "model_missing",
            Self::Failed => "failed",
        }
    }
}

/// The answer to a `meaning` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MeaningResult {
    Ready {
        /// Best first, at most the request's `limit` (capped).
        hits: Vec<MeaningHit>,
        pool: MeaningPool,
        /// Every cap that applied, in words.
        caps: Vec<String>,
    },
    Unavailable {
        reason: MeaningUnavailableReason,
        /// What went wrong, for [`MeaningUnavailableReason::Failed`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        caps: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pool() -> MeaningPool {
        MeaningPool {
            model: "minishlab/potion-code-16M-v2".into(),
            dims: 256,
            files: 12,
            chunks: 340,
            eligible_files: 12,
            generation: 7,
            age_ms: 1500,
            resident_bytes: 524_288,
        }
    }

    #[test]
    fn ready_serializes_hits_pool_and_caps_flat_under_the_status_tag() {
        let ready = MeaningResult::Ready {
            hits: vec![
                MeaningHit {
                    path: "src/billing.rs".into(),
                    start_line: 12,
                    end_line: 21,
                    symbol: Some("refund_payment".into()),
                    score: 0.0325,
                    snippet: "/// Refund a payment".into(),
                },
                MeaningHit {
                    path: "README.md".into(),
                    start_line: 1,
                    end_line: 3,
                    symbol: None,
                    score: 0.0262,
                    snippet: "# Billing".into(),
                },
            ],
            pool: pool(),
            caps: vec!["semantic leads are unverified".into()],
        };
        let value = serde_json::to_value(&ready).unwrap();
        assert_eq!(
            value,
            json!({
                "status": "ready",
                "hits": [
                    {
                        "path": "src/billing.rs",
                        "start_line": 12,
                        "end_line": 21,
                        "symbol": "refund_payment",
                        "score": 0.0325,
                        "snippet": "/// Refund a payment"
                    },
                    {
                        "path": "README.md",
                        "start_line": 1,
                        "end_line": 3,
                        "score": 0.0262,
                        "snippet": "# Billing"
                    }
                ],
                "pool": {
                    "model": "minishlab/potion-code-16M-v2",
                    "dims": 256,
                    "files": 12,
                    "chunks": 340,
                    "eligible_files": 12,
                    "generation": 7,
                    "age_ms": 1500,
                    "resident_bytes": 524_288
                },
                "caps": ["semantic leads are unverified"]
            })
        );
        assert_eq!(
            serde_json::from_value::<MeaningResult>(value).unwrap(),
            ready
        );
    }

    #[test]
    fn unavailable_names_its_reason_and_keeps_detail_only_when_given() {
        for (reason, name) in [
            (MeaningUnavailableReason::Cold, "cold"),
            (MeaningUnavailableReason::Warming, "warming"),
            (MeaningUnavailableReason::Stale, "stale"),
            (MeaningUnavailableReason::Refreshing, "refreshing"),
            (MeaningUnavailableReason::ModelMissing, "model_missing"),
            (MeaningUnavailableReason::Failed, "failed"),
        ] {
            assert_eq!(reason.as_str(), name);
            let unavailable = MeaningResult::Unavailable {
                reason,
                detail: None,
                caps: vec!["no semantic leads".into()],
            };
            let value = serde_json::to_value(&unavailable).unwrap();
            assert_eq!(
                value,
                json!({
                    "status": "unavailable",
                    "reason": name,
                    "caps": ["no semantic leads"]
                })
            );
            assert_eq!(
                serde_json::from_value::<MeaningResult>(value).unwrap(),
                unavailable
            );
        }
        let failed = MeaningResult::Unavailable {
            reason: MeaningUnavailableReason::Failed,
            detail: Some("model load: corrupt".into()),
            caps: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(&failed).unwrap(),
            json!({
                "status": "unavailable",
                "reason": "failed",
                "detail": "model load: corrupt",
                "caps": []
            })
        );
    }
}
