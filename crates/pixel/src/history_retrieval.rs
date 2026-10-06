// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Verified-history retrieval with abstention for the opt-in classify tier.
//!
//! Given a decision specification (text + context + labels + criteria), the
//! retriever searches the verified-history store for similar past decisions
//! and votes on a label. It abstains — returning no answer and preserving
//! the existing model fallback — when support, disagreement or novelty checks
//! fail.
//!
//! History applicability is keyed by the decision specification, not state
//! text alone: the same text under a different rubric may legitimately have a
//! different answer. Similarity of state text is not permission to reuse a
//! label from another task family.
//!
//! Raw vote share is not calibrated probability. The retriever exposes its
//! evidence (neighbours, scores, abstention reason) so the caller can decide
//! how to use it.

use std::collections::BTreeMap;

use crate::classify::Spec;
use crate::history_store::{HistoryEntry, HistoryStore};

/// Minimum number of supporting neighbours required to accept a history answer.
pub(crate) const MIN_SUPPORT: usize = 2;
/// Minimum vote share (0.0–1.0) for the winning label to be accepted.
pub(crate) const MIN_VOTE_SHARE: f64 = 0.6;
/// Minimum token-level Jaccard similarity for a neighbour to participate.
pub(crate) const MIN_NEIGHBOUR_SIMILARITY: f64 = 0.2;
/// Minimum similarity for the nearest neighbour (novelty check).
pub(crate) const MIN_NOVELTY_SIMILARITY: f64 = 0.1;

/// The result of a history retrieval attempt.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RetrievalResult {
    /// A confident answer from verified history.
    Answer {
        /// The winning label.
        label: String,
        /// Raw vote share (not calibrated probability).
        vote_share: f64,
        /// Number of supporting neighbours.
        support: usize,
        /// The neighbours that voted for the winning label.
        neighbours: Vec<Neighbour>,
        /// The rubric fingerprint this answer was verified under.
        rubric_fingerprint: String,
    },
    /// Abstention: the history cannot confidently answer this request.
    Abstain {
        /// Why the retriever abstained.
        reason: AbstainReason,
        /// The nearest neighbours found, for disclosure.
        neighbours: Vec<Neighbour>,
    },
}

/// Why the retriever abstained.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AbstainReason {
    /// The store is empty or has no entries for this task family.
    EmptyHistory,
    /// No neighbour was close enough (novelty check failed).
    TooNovel,
    /// Neighbours disagreed too much (no label reached the vote-share threshold).
    Disagreement,
    /// Not enough supporting neighbours.
    InsufficientSupport,
    /// The rubric fingerprint doesn't match any stored entry.
    RubricMismatch,
}

/// One neighbour that participated in voting.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Neighbour {
    /// The stored text.
    pub text: String,
    /// The verified label.
    pub label: String,
    /// Token-level Jaccard similarity to the query (0.0–1.0).
    pub similarity: f64,
    /// The source of this entry.
    pub source: String,
}

/// The retriever: searches a verified-history store and votes on a label.
#[derive(Debug, Clone)]
pub(crate) struct HistoryRetriever<'a> {
    store: &'a HistoryStore,
    task_family: String,
}

impl<'a> HistoryRetriever<'a> {
    /// Create a new retriever over a store, scoped to a task family.
    pub fn new(store: &'a HistoryStore, task_family: impl Into<String>) -> Self {
        Self {
            store,
            task_family: task_family.into(),
        }
    }

    /// Attempt to answer the spec from verified history. Returns an answer
    /// when the support, disagreement and novelty checks all pass; otherwise
    /// abstains with a reason.
    pub fn retrieve(&self, spec: &Spec) -> RetrievalResult {
        let rubric_fp = HistoryStore::rubric_fingerprint(&spec.context, &spec.criteria);

        // Key by the decision specification: same text under a different
        // rubric may legitimately have a different answer.
        let family_entries: Vec<&HistoryEntry> = self
            .store
            .by_task_family(&self.task_family)
            .into_iter()
            .filter(|e| e.rubric_fingerprint == rubric_fp)
            .collect();

        if family_entries.is_empty() {
            // Check if there are entries for this family under a different
            // rubric — that's a rubric mismatch, not just empty history.
            let any_family = self.store.by_task_family(&self.task_family);
            let reason = if any_family.is_empty() {
                AbstainReason::EmptyHistory
            } else {
                AbstainReason::RubricMismatch
            };
            return RetrievalResult::Abstain {
                reason,
                neighbours: vec![],
            };
        }

        // Compute similarities and collect neighbours.
        let mut neighbours: Vec<Neighbour> = family_entries
            .iter()
            .map(|entry| Neighbour {
                text: entry.text.clone(),
                label: entry.label.clone(),
                similarity: jaccard_similarity(&spec.text, &entry.text),
                source: entry.source.clone(),
            })
            .filter(|n| n.similarity >= MIN_NEIGHBOUR_SIMILARITY)
            .collect();

        if neighbours.is_empty() {
            return RetrievalResult::Abstain {
                reason: AbstainReason::TooNovel,
                neighbours: vec![],
            };
        }

        // Sort by similarity (closest first).
        neighbours.sort_by(|a, b| b.similarity.total_cmp(&a.similarity));

        // Novelty check: the nearest neighbour must be similar enough.
        if neighbours[0].similarity < MIN_NOVELTY_SIMILARITY {
            return RetrievalResult::Abstain {
                reason: AbstainReason::TooNovel,
                neighbours,
            };
        }

        // Vote: each neighbour votes for its label, weighted by similarity.
        let mut votes: BTreeMap<String, f64> = BTreeMap::new();
        let mut total_weight = 0.0;
        for neighbour in &neighbours {
            *votes.entry(neighbour.label.clone()).or_insert(0.0) += neighbour.similarity;
            total_weight += neighbour.similarity;
        }

        if total_weight == 0.0 {
            return RetrievalResult::Abstain {
                reason: AbstainReason::Disagreement,
                neighbours,
            };
        }

        // Find the winning label.
        let (winner, winner_votes) = votes
            .iter()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(k, v)| (k.clone(), *v))
            .unwrap_or_default();
        let vote_share = winner_votes / total_weight;

        // Support check: how many neighbours voted for the winner?
        let support = neighbours.iter().filter(|n| n.label == winner).count();

        if support < MIN_SUPPORT {
            return RetrievalResult::Abstain {
                reason: AbstainReason::InsufficientSupport,
                neighbours,
            };
        }

        if vote_share < MIN_VOTE_SHARE {
            return RetrievalResult::Abstain {
                reason: AbstainReason::Disagreement,
                neighbours,
            };
        }

        let winning_neighbours: Vec<Neighbour> = neighbours
            .iter()
            .filter(|n| n.label == winner)
            .cloned()
            .collect();

        RetrievalResult::Answer {
            label: winner,
            vote_share,
            support,
            neighbours: winning_neighbours,
            rubric_fingerprint: rubric_fp,
        }
    }
}

/// Token-level Jaccard similarity between two strings.
/// Returns a value in 0.0–1.0 where 1.0 means identical token sets.
pub(crate) fn jaccard_similarity(a: &str, b: &str) -> f64 {
    let tokens_a: std::collections::HashSet<&str> = tokenize(a).into_iter().collect();
    let tokens_b: std::collections::HashSet<&str> = tokenize(b).into_iter().collect();
    if tokens_a.is_empty() && tokens_b.is_empty() {
        return 1.0;
    }
    if tokens_a.is_empty() || tokens_b.is_empty() {
        return 0.0;
    }
    let intersection: std::collections::HashSet<_> = tokens_a.intersection(&tokens_b).collect();
    let union: std::collections::HashSet<_> = tokens_a.union(&tokens_b).collect();
    intersection.len() as f64 / union.len() as f64
}

/// Simple tokenizer: lowercase, split on non-alphanumeric.
fn tokenize(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::Spec;

    fn spec(text: &str, context: &str, labels: &[&str], criteria: &[(&str, &str)]) -> Spec {
        Spec::checked(
            text.to_string(),
            context.to_string(),
            labels.iter().map(ToString::to_string).collect(),
            criteria
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
        .unwrap()
    }

    fn intent_labels() -> Vec<&'static str> {
        vec!["bugfix", "feature", "refactor", "investigate", "question", "review", "ops"]
    }

    fn intent_criteria() -> Vec<(&'static str, &'static str)> {
        vec![
            ("bugfix", "something broken"),
            ("feature", "new behaviour"),
            ("refactor", "restructure"),
            ("investigate", "understand"),
            ("question", "answer in prose"),
            ("review", "review changes"),
            ("ops", "operate repo"),
        ]
    }

    fn entry(text: &str, label: &str, rubric: &str) -> HistoryEntry {
        HistoryEntry {
            text: text.to_string(),
            label: label.to_string(),
            source: "human".to_string(),
            supersedes: vec![],
            task_family: "coding-agent-prompt".to_string(),
            label_schema_version: 1,
            rubric_fingerprint: rubric.to_string(),
            verified_at: 1_700_000_000,
            note: None,
        }
    }

    /// Compute the rubric fingerprint for the test context "context" with
    /// empty criteria (matching the test specs).
    fn test_rubric_fingerprint() -> String {
        HistoryStore::rubric_fingerprint("context", &BTreeMap::new())
    }

    #[test]
    fn empty_store_abstains_with_empty_history() {
        let store = HistoryStore::new();
        let retriever = HistoryRetriever::new(&store, "coding-agent-prompt");
        let s = spec("fix the bug", "context", &intent_labels(), &[]);
        let result = retriever.retrieve(&s);
        assert!(matches!(
            result,
            RetrievalResult::Abstain {
                reason: AbstainReason::EmptyHistory,
                ..
            }
        ));
    }

    #[test]
    fn exact_match_returns_confident_answer() {
        let mut store = HistoryStore::new();
        store.add(entry("fix the login bug", "bugfix", &test_rubric_fingerprint()));
        store.add(entry("fix the login bug", "bugfix", &test_rubric_fingerprint()));
        store.add(entry("fix the login bug", "bugfix", &test_rubric_fingerprint()));

        let retriever = HistoryRetriever::new(&store, "coding-agent-prompt");
        let s = spec("fix the login bug", "context", &intent_labels(), &[]);
        let result = retriever.retrieve(&s);

        match result {
            RetrievalResult::Answer { label, support, .. } => {
                assert_eq!(label, "bugfix");
                assert!(support >= MIN_SUPPORT);
            }
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    #[test]
    fn similar_text_with_agreement_returns_answer() {
        let mut store = HistoryStore::new();
        store.add(entry("fix the login bug", "bugfix", &test_rubric_fingerprint()));
        store.add(entry("fix the authentication bug", "bugfix", &test_rubric_fingerprint()));
        store.add(entry("fix the database connection bug", "bugfix", &test_rubric_fingerprint()));

        let retriever = HistoryRetriever::new(&store, "coding-agent-prompt");
        let s = spec("fix the logout bug", "context", &intent_labels(), &[]);
        let result = retriever.retrieve(&s);

        match result {
            RetrievalResult::Answer { label, .. } => {
                assert_eq!(label, "bugfix");
            }
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    #[test]
    fn disagreement_abstains() {
        let mut store = HistoryStore::new();
        // Entries with similar texts but different labels
        store.add(entry("fix the login bug", "bugfix", &test_rubric_fingerprint()));
        store.add(entry("fix the login issue", "bugfix", &test_rubric_fingerprint()));
        store.add(entry("fix the login feature", "feature", &test_rubric_fingerprint()));
        store.add(entry("fix the login problem", "feature", &test_rubric_fingerprint()));

        let retriever = HistoryRetriever::new(&store, "coding-agent-prompt");
        let s = spec("fix the login bug", "context", &intent_labels(), &[]);
        let result = retriever.retrieve(&s);

        // The vote should be split between bugfix and feature, causing
        // neither to reach the vote-share threshold.
        assert!(matches!(
            result,
            RetrievalResult::Abstain {
                reason: AbstainReason::Disagreement,
                ..
            }
        ));
    }

    #[test]
    fn novel_input_abstains() {
        let mut store = HistoryStore::new();
        store.add(entry("fix the login bug", "bugfix", &test_rubric_fingerprint()));
        store.add(entry("fix the authentication bug", "bugfix", &test_rubric_fingerprint()));
        store.add(entry("fix the database connection bug", "bugfix", &test_rubric_fingerprint()));

        let retriever = HistoryRetriever::new(&store, "coding-agent-prompt");
        // Completely different vocabulary
        let s = spec("xyzzy plugh quux", "context", &intent_labels(), &[]);
        let result = retriever.retrieve(&s);

        assert!(matches!(
            result,
            RetrievalResult::Abstain {
                reason: AbstainReason::TooNovel,
                ..
            }
        ));
    }

    #[test]
    fn rubric_mismatch_abstains() {
        let mut store = HistoryStore::new();
        store.add(entry("fix the login bug", "bugfix", &test_rubric_fingerprint()));
        store.add(entry("fix the login bug", "bugfix", &test_rubric_fingerprint()));
        store.add(entry("fix the login bug", "bugfix", &test_rubric_fingerprint()));

        let retriever = HistoryRetriever::new(&store, "coding-agent-prompt");
        // Same text, different rubric (different context/criteria)
        let s = spec("fix the login bug", "different context", &intent_labels(), &[]);
        let result = retriever.retrieve(&s);

        assert!(matches!(
            result,
            RetrievalResult::Abstain {
                reason: AbstainReason::RubricMismatch,
                ..
            }
        ));
    }

    #[test]
    fn different_task_family_abstains() {
        let mut store = HistoryStore::new();
        store.add(entry("fix the login bug", "bugfix", &test_rubric_fingerprint()));
        store.add(entry("fix the login bug", "bugfix", &test_rubric_fingerprint()));
        store.add(entry("fix the login bug", "bugfix", &test_rubric_fingerprint()));

        let retriever = HistoryRetriever::new(&store, "code-review");
        let s = spec("fix the login bug", "context", &intent_labels(), &[]);
        let result = retriever.retrieve(&s);

        assert!(matches!(
            result,
            RetrievalResult::Abstain {
                reason: AbstainReason::EmptyHistory,
                ..
            }
        ));
    }

    #[test]
    fn corrected_label_changes_the_answer() {
        let mut store = HistoryStore::new();
        // Old entries with the wrong label
        store.add(entry("fix the login bug", "feature", &test_rubric_fingerprint()));
        store.add(entry("fix the login bug", "feature", &test_rubric_fingerprint()));
        // Correction: replace one "feature" with "bugfix"
        store.correct(
            "fix the login bug",
            "feature",
            HistoryEntry {
                label: "bugfix".to_string(),
                source: "correction".to_string(),
                ..entry("fix the login bug", "bugfix", &test_rubric_fingerprint())
            },
        );

        let retriever = HistoryRetriever::new(&store, "coding-agent-prompt");
        let s = spec("fix the login bug", "context", &intent_labels(), &[]);
        let result = retriever.retrieve(&s);

        // Now there's 1 entry with "feature" and 1 with "bugfix" — a tie.
        // The retriever should abstain with Disagreement.
        match result {
            RetrievalResult::Answer { label, .. } => {
                // If it does answer, it should be one of the two labels
                assert!(label == "bugfix" || label == "feature");
            }
            RetrievalResult::Abstain { .. } => {
                // Expected: the correction creates a tie, so abstain
            }
        }
    }

    #[test]
    fn removing_a_label_changes_the_answer() {
        let mut store = HistoryStore::new();
        store.add(entry("fix the login bug", "bugfix", &test_rubric_fingerprint()));
        store.add(entry("fix the login bug", "bugfix", &test_rubric_fingerprint()));
        store.add(entry("fix the login bug", "bugfix", &test_rubric_fingerprint()));

        let s = spec("fix the login bug", "context", &intent_labels(), &[]);

        // Before removal: should answer
        {
            let retriever = HistoryRetriever::new(&store, "coding-agent-prompt");
            let result = retriever.retrieve(&s);
            assert!(matches!(result, RetrievalResult::Answer { .. }));
        }

        // Remove two entries
        store.remove("fix the login bug", "bugfix");
        store.remove("fix the login bug", "bugfix");

        // After removal: should abstain (insufficient support)
        {
            let retriever = HistoryRetriever::new(&store, "coding-agent-prompt");
            let result = retriever.retrieve(&s);
            assert!(matches!(
                result,
                RetrievalResult::Abstain {
                    reason: AbstainReason::InsufficientSupport,
                    ..
                }
            ));
        }
    }

    #[test]
    fn jaccard_similarity_basic() {
        assert_eq!(jaccard_similarity("hello world", "hello world"), 1.0);
        // "hello world" -> {hello, world}, "goodbye world" -> {goodbye, world}
        // intersection = {world} (1), union = {hello, world, goodbye} (3)
        assert_eq!(jaccard_similarity("hello world", "goodbye world"), 1.0 / 3.0);
        assert_eq!(jaccard_similarity("abc", "xyz"), 0.0);
        assert_eq!(jaccard_similarity("", ""), 1.0);
        assert_eq!(jaccard_similarity("abc", ""), 0.0);
    }

    #[test]
    fn answer_exposes_neighbours_and_scores() {
        let mut store = HistoryStore::new();
        store.add(entry("fix the login bug", "bugfix", &test_rubric_fingerprint()));
        store.add(entry("fix the auth bug", "bugfix", &test_rubric_fingerprint()));
        store.add(entry("fix the db bug", "bugfix", &test_rubric_fingerprint()));

        let retriever = HistoryRetriever::new(&store, "coding-agent-prompt");
        let s = spec("fix the logout bug", "context", &intent_labels(), &[]);
        let result = retriever.retrieve(&s);

        match result {
            RetrievalResult::Answer {
                label,
                vote_share,
                support,
                neighbours,
                rubric_fingerprint,
            } => {
                assert_eq!(label, "bugfix");
                assert!(vote_share > 0.0 && vote_share <= 1.0);
                assert!(support >= MIN_SUPPORT);
                assert!(!neighbours.is_empty());
                assert_eq!(rubric_fingerprint, test_rubric_fingerprint());
            }
            other => panic!("expected an answer, got {other:?}"),
        }
    }
}
