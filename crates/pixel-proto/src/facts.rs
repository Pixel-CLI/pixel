// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Typed outcomes for deterministic repository fact requests.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Deterministic inputs that identify a `targets_facts` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetsFactsInputs {
    pub task: String,
    pub limit: usize,
    pub index_commit_oid: Option<String>,
    pub index_base_files: u32,
    pub index_delta_files: u32,
    pub index_overlay_files: usize,
    pub index_tombstones: usize,
    pub graph_generation: u64,
    /// Hash of the repository files represented by the fresh graph.
    pub graph_signature: String,
    pub algorithm_version: u32,
    pub activity_reranking: bool,
    pub semantic_fallback: bool,
}

/// Typed availability result for a deterministic task-target fact request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TargetsFactsResult {
    Available {
        inputs: TargetsFactsInputs,
        facts: Value,
    },
    Unavailable {
        reason: TargetsFactsUnavailableReason,
    },
}

/// Why an existing fact snapshot cannot be served without a rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetsFactsUnavailableReason {
    /// No running daemon has a published index available for a read-only request.
    DaemonUnavailable,
    IndexUnavailable,
    IndexStale,
    GraphMissing,
    GraphStale,
    PublicationUnhealthy,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn targets_facts_result_serializes_typed_availability_and_declared_inputs() {
        let available = TargetsFactsResult::Available {
            inputs: TargetsFactsInputs {
                task: "fix login flow".into(),
                limit: 8,
                index_commit_oid: Some("abc123".into()),
                index_base_files: 3,
                index_delta_files: 1,
                index_overlay_files: 2,
                index_tombstones: 0,
                graph_generation: 4,
                graph_signature: "content-hash".into(),
                algorithm_version: 1,
                activity_reranking: false,
                semantic_fallback: false,
            },
            facts: json!({"targets": [{"path": "src/login.rs"}]}),
        };
        assert_eq!(
            serde_json::to_value(available).unwrap(),
            json!({
                "status": "available",
                "inputs": {
                    "task": "fix login flow",
                    "limit": 8,
                    "index_commit_oid": "abc123",
                    "index_base_files": 3,
                    "index_delta_files": 1,
                    "index_overlay_files": 2,
                    "index_tombstones": 0,
                    "graph_generation": 4,
                    "graph_signature": "content-hash",
                    "algorithm_version": 1,
                    "activity_reranking": false,
                    "semantic_fallback": false
                },
                "facts": {"targets": [{"path": "src/login.rs"}]}
            })
        );
        assert_eq!(
            serde_json::to_value(TargetsFactsResult::Unavailable {
                reason: TargetsFactsUnavailableReason::GraphStale,
            })
            .unwrap(),
            json!({"status": "unavailable", "reason": "graph_stale"})
        );
        assert_eq!(
            serde_json::to_value(TargetsFactsResult::Unavailable {
                reason: TargetsFactsUnavailableReason::DaemonUnavailable,
            })
            .unwrap(),
            json!({"status": "unavailable", "reason": "daemon_unavailable"})
        );
    }
}
