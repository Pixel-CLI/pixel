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

/// What a repository's own vocabulary says about the words of a task prompt.
///
/// The deterministic inputs of an on-topic decision, carried as
/// `facts.relevance` on a `targets_facts` result from algorithm version 2 on;
/// a result without the block predates it, which is not the same as a prompt
/// the repository knows nothing about (that one has `keywords` with all-zero
/// counts). `pixel_daemon::relevance::relevance_on` computes the same block
/// in process.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Relevance {
    /// Indexed files the counts below are drawn from: the IDF denominator.
    #[serde(default)]
    pub files_considered: usize,
    /// Whether the code graph answered. Without it every `symbol_files` is 0
    /// because symbols were not read, not because none matched.
    #[serde(default)]
    pub graph: bool,
    /// One row per task keyword, in task order, matched or not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<KeywordEvidence>,
    /// The files the task's rarest words meet in, heaviest first: the best
    /// few by weight and the best structural ones (see [`CoFile::weight`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cofiles: Vec<CoFile>,
    /// Every cap that bounded the block, in the words the response envelope
    /// uses; a reader without that envelope still sees them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caps: Vec<String>,
}

/// How widely one task keyword occurs in the repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeywordEvidence {
    pub keyword: String,
    /// Files with a word-bounded content match. A lower bound when
    /// `truncated`: the probe stops at its match cap, in path order.
    #[serde(default)]
    pub content_files: usize,
    /// The content probe hit its match cap, so `content_files` undercounts.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Files defining a symbol whose name has this word.
    #[serde(default)]
    pub symbol_files: usize,
    /// Files with this word in a directory or file name.
    #[serde(default)]
    pub filename_files: usize,
    /// The synonym whose counts stand in for a keyword the repository does
    /// not contain as typed (a French word, through the thesaurus).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_expansion: Option<String>,
    /// A general word (`does`, `handle`, `quel`) that says nothing about a
    /// repository: the counts are kept, the weight is 0.
    #[serde(default, skip_serializing_if = "is_false")]
    pub common: bool,
}

/// A file several task keywords meet in, with one line to start from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CoFile {
    pub path: String,
    /// The task keywords this file matches, in task order.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// The sum, over `keywords`, of how selective each is in this repository
    /// (`pixel_daemon::relevance::keyword_weight`): a word in most files, or
    /// one whose probe truncated, weighs 0; a rare one up to the cap. Rounded
    /// to three decimals.
    #[serde(default)]
    pub weight: f64,
    /// A keyword of positive weight matched the file's own name or one of its
    /// symbols, not only its text. A ubiquitous word in a path (`src`) is not
    /// structure.
    #[serde(default)]
    pub structural: bool,
    /// The keywords (a subset of `keywords`) that made it `structural`: the
    /// ones of positive weight that matched the file's name or a symbol.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub structural_keywords: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The `skip_serializing_if` predicate for a flag whose absence means false;
/// serde hands the field by reference.
fn is_false(value: &bool) -> bool {
    !*value
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

    fn sample_relevance() -> Relevance {
        Relevance {
            files_considered: 1161,
            graph: true,
            keywords: vec![
                KeywordEvidence {
                    keyword: "install".into(),
                    content_files: 412,
                    truncated: true,
                    symbol_files: 18,
                    filename_files: 9,
                    via_expansion: None,
                    common: false,
                },
                KeywordEvidence {
                    keyword: "connexion".into(),
                    content_files: 3,
                    truncated: false,
                    symbol_files: 0,
                    filename_files: 1,
                    via_expansion: Some("login".into()),
                    common: false,
                },
                KeywordEvidence {
                    keyword: "handle".into(),
                    common: true,
                    ..KeywordEvidence::default()
                },
            ],
            cofiles: vec![
                CoFile {
                    path: "crates/pixel-install/src/claude.rs".into(),
                    keywords: vec!["install".into(), "claude".into()],
                    weight: 6.25,
                    structural: true,
                    structural_keywords: vec!["claude".into()],
                    line: Some(120),
                    text: Some("fn merge_settings() {}".into()),
                },
                CoFile {
                    path: "docs/manual-setup.md".into(),
                    keywords: vec!["install".into()],
                    weight: 0.5,
                    structural: false,
                    structural_keywords: Vec::new(),
                    line: None,
                    text: None,
                },
            ],
            caps: vec!["task keywords truncated at 12".into()],
        }
    }

    #[test]
    fn relevance_should_serialize_only_the_parts_that_carry_information() {
        assert_eq!(
            serde_json::to_value(sample_relevance()).unwrap(),
            json!({
                "files_considered": 1161,
                "graph": true,
                "keywords": [
                    {
                        "keyword": "install",
                        "content_files": 412,
                        "truncated": true,
                        "symbol_files": 18,
                        "filename_files": 9
                    },
                    {
                        "keyword": "connexion",
                        "content_files": 3,
                        "symbol_files": 0,
                        "filename_files": 1,
                        "via_expansion": "login"
                    },
                    {
                        "keyword": "handle",
                        "content_files": 0,
                        "symbol_files": 0,
                        "filename_files": 0,
                        "common": true
                    }
                ],
                "cofiles": [
                    {
                        "path": "crates/pixel-install/src/claude.rs",
                        "keywords": ["install", "claude"],
                        "weight": 6.25,
                        "structural": true,
                        "structural_keywords": ["claude"],
                        "line": 120,
                        "text": "fn merge_settings() {}"
                    },
                    {
                        "path": "docs/manual-setup.md",
                        "keywords": ["install"],
                        "weight": 0.5,
                        "structural": false
                    }
                ],
                "caps": ["task keywords truncated at 12"]
            })
        );
        assert_eq!(
            serde_json::to_value(Relevance::default()).unwrap(),
            json!({"files_considered": 0, "graph": false}),
            "an empty block is the two counters, never null lists"
        );
    }

    #[test]
    fn relevance_should_survive_a_round_trip_through_json() {
        let original = sample_relevance();
        let text = serde_json::to_string(&original).unwrap();
        assert_eq!(serde_json::from_str::<Relevance>(&text).unwrap(), original);
    }

    #[test]
    fn relevance_should_parse_when_a_writer_left_optional_fields_out() {
        // A block written before `caps`, `cofiles` or a flag existed, and the
        // empty object, both read as "nothing recorded" rather than failing.
        assert_eq!(
            serde_json::from_value::<Relevance>(json!({})).unwrap(),
            Relevance::default()
        );
        assert_eq!(
            serde_json::from_value::<Relevance>(json!({
                "files_considered": 7,
                "keywords": [{"keyword": "login"}]
            }))
            .unwrap(),
            Relevance {
                files_considered: 7,
                keywords: vec![KeywordEvidence {
                    keyword: "login".into(),
                    ..KeywordEvidence::default()
                }],
                ..Relevance::default()
            }
        );
        assert_eq!(
            serde_json::from_value::<CoFile>(json!({"path": "a.rs"})).unwrap(),
            CoFile {
                path: "a.rs".into(),
                ..CoFile::default()
            }
        );
    }

    #[test]
    fn targets_facts_from_an_older_daemon_should_parse_without_a_relevance_block() {
        let older = json!({
            "status": "available",
            "inputs": {
                "task": "fix login flow",
                "limit": 8,
                "index_commit_oid": null,
                "index_base_files": 3,
                "index_delta_files": 0,
                "index_overlay_files": 0,
                "index_tombstones": 0,
                "graph_generation": 1,
                "graph_signature": "sig",
                "algorithm_version": 1,
                "activity_reranking": false,
                "semantic_fallback": false
            },
            "facts": {"targets": []}
        });
        let TargetsFactsResult::Available { inputs, facts } =
            serde_json::from_value(older).unwrap()
        else {
            panic!("an available result parses as available");
        };
        assert_eq!(inputs.algorithm_version, 1);
        assert!(
            facts.get("relevance").is_none(),
            "version 1 facts carry no relevance block"
        );
    }
}
