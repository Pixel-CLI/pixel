// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Verified-history retrieval tier for `pixel classify` (issue #624).
//!
//! An opt-in tier that sits *before* the model: when the caller asks for a
//! decision whose rubric (context + labels + criteria) matches a stored,
//! human-verified example, the tier answers from that history and the model
//! is never consulted. When the history cannot support a confident answer the
//! tier abstains and the existing fallback policy (the model) runs unchanged.
//!
//! The store is deliberately separate from the machine-wide recall corpus:
//! a bounded, project-isolated JSONL file under `<root>/.pixel/`. Entries
//! record their source, correction lineage, task family, label-schema version
//! and rubric fingerprint. Retrieval is keyed by the rubric fingerprint, never
//! by state text alone — the same text under a different rubric may
//! legitimately have a different answer.
//!
//! Model predictions can never enter the store: the only write path is
//! [`HistoryStore::add`], which requires a human/independently-verified
//! `source`, and the tier's verdicts are never written back. Superseded
//! corrections are marked and carry no voting weight, so an erroneous cache
//! cannot accumulate support.
//!
//! Raw vote share is not a calibrated probability; the verdict discloses it
//! as raw evidence.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::classify::Spec;

/// The store's file name, under the repository's `.pixel/` directory.
pub(crate) const STORE_FILENAME: &str = "classify-history.jsonl";
/// Retention bound: the store keeps at most this many active entries, evicting
/// the oldest by creation time (FIFO) when an add crosses the bound.
pub(crate) const MAX_ENTRIES: usize = 500;
/// The label schema this tier reads. Entries written under another version are
/// ignored, so a relabelled vocabulary cannot reuse stale answers.
pub(crate) const LABEL_SCHEMA_VERSION: u32 = 1;
/// Neighbours below this character-trigram Jaccard similarity are not
/// evidence: the input is too unlike anything verified.
pub(crate) const MIN_SIMILARITY: f64 = 0.30;
/// At least this many neighbours must clear [`MIN_SIMILARITY`] before a
/// paraphrase may be answered from voting.
pub(crate) const MIN_NEIGHBOURS: usize = 2;
/// The winning label's share of the similarity-weighted vote must reach this
/// or the neighbours disagree and the tier abstains.
pub(crate) const MIN_VOTE_SHARE: f64 = 0.60;
/// How many neighbours a verdict discloses.
pub(crate) const DISCLOSED_NEIGHBOURS: usize = 5;

/// One correction in an entry's lineage: what it was corrected from, and the
/// source of that correction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Correction {
    pub label: String,
    pub source: String,
    pub at_unix: u64,
}

/// One verified example. `superseded` entries are corrections that a later
/// [`HistoryStore::correct`] replaced: they stay in the file for lineage but
/// carry no voting weight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HistoryEntry {
    pub id: String,
    pub text: String,
    pub label: String,
    pub task_family: String,
    pub rubric_fingerprint: String,
    pub label_schema_version: u32,
    pub source: String,
    pub corrections: Vec<Correction>,
    pub superseded: bool,
    pub created_unix: u64,
}

/// A new entry to store. The rubric fingerprint is computed from the decision
/// specification the example was verified under (see [`NewEntry::for_spec`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewEntry {
    pub text: String,
    pub label: String,
    pub task_family: String,
    pub rubric_fingerprint: String,
    pub source: String,
}

impl NewEntry {
    /// Bind the entry to the rubric of `spec` — the same fingerprint the tier
    /// computes when it later evaluates that spec.
    pub fn for_spec(
        text: String,
        label: String,
        task_family: String,
        source: String,
        spec: &Spec,
    ) -> Self {
        Self {
            rubric_fingerprint: rubric_fingerprint(&spec.context, &spec.labels, &spec.criteria),
            text,
            label,
            task_family,
            source,
        }
    }
}

/// The bounded, project-isolated store. One JSONL file per repository under
/// `.pixel/`; never the machine-wide recall corpus.
pub(crate) struct HistoryStore {
    path: PathBuf,
    entries: Vec<HistoryEntry>,
}

impl HistoryStore {
    /// Open (or create) the store at `path`. A missing file is an empty store,
    /// not an error: an empty history preserves current behavior.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let entries = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            parse_entries(&text)?
        } else {
            Vec::new()
        };
        Ok(Self { path, entries })
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// An empty store with no backing file — the tier's fallback when the
    /// real store cannot be opened. It abstains (Empty) and never saves.
    pub fn empty() -> Self {
        Self {
            path: PathBuf::new(),
            entries: Vec::new(),
        }
    }

    /// The store's file name inside a repository's `.pixel/` directory.
    pub fn repo_path(root: &Path) -> PathBuf {
        root.join(".pixel").join(STORE_FILENAME)
    }

    /// Add a verified example. `source` is required provenance: there is no
    /// write path that accepts a model prediction, so predictions cannot
    /// self-certify into gold labels.
    pub fn add(&mut self, new: NewEntry) -> Result<HistoryEntry, String> {
        if new.text.trim().is_empty() {
            return Err("history entry needs non-empty text".into());
        }
        if new.label.trim().is_empty() {
            return Err("history entry needs a non-empty verified label".into());
        }
        if new.source.trim().is_empty() {
            return Err(
                "history entry needs a source: only human/independently verified labels are stored"
                    .into(),
            );
        }
        let created_unix = crate::task_runtime::now_unix();
        let id = self.next_id(created_unix);
        let entry = HistoryEntry {
            id,
            text: new.text,
            label: new.label,
            task_family: new.task_family,
            rubric_fingerprint: new.rubric_fingerprint,
            label_schema_version: LABEL_SCHEMA_VERSION,
            source: new.source,
            corrections: Vec::new(),
            superseded: false,
            created_unix,
        };
        self.entries.push(entry.clone());
        self.retain();
        self.save()?;
        Ok(entry)
    }

    /// Delete an entry by id. A missing id is an error, not a silent no-op.
    pub fn remove(&mut self, id: &str) -> Result<(), String> {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        if self.entries.len() == before {
            return Err(format!("no history entry with id {id}"));
        }
        self.save()
    }

    /// Correct an entry's label: the old entry is marked superseded (it keeps
    /// its lineage but loses all voting weight) and a new entry carries the
    /// correction. Explicit replacement, not a 2x human weight that could be
    /// overpowered by an arbitrarily large erroneous cache.
    pub fn correct(&mut self, id: &str, label: &str, source: &str) -> Result<HistoryEntry, String> {
        if label.trim().is_empty() {
            return Err("correction needs a non-empty verified label".into());
        }
        if source.trim().is_empty() {
            return Err("correction needs a source: only verified corrections are stored".into());
        }
        let Some(index) = self.entries.iter().position(|e| e.id == id) else {
            return Err(format!("no history entry with id {id}"));
        };
        let old = self.entries[index].clone();
        self.entries[index].superseded = true;
        let created_unix = crate::task_runtime::now_unix();
        let entry = HistoryEntry {
            id: self.next_id(created_unix),
            text: old.text,
            label: label.to_string(),
            task_family: old.task_family,
            rubric_fingerprint: old.rubric_fingerprint,
            label_schema_version: LABEL_SCHEMA_VERSION,
            source: source.to_string(),
            corrections: old
                .corrections
                .into_iter()
                .chain(std::iter::once(Correction {
                    label: old.label,
                    source: old.source,
                    at_unix: old.created_unix,
                }))
                .collect(),
            superseded: false,
            created_unix,
        };
        self.entries.push(entry.clone());
        self.retain();
        self.save()?;
        Ok(entry)
    }

    /// Every entry, including superseded ones (for lineage reporting).
    pub fn all(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// The active (non-superseded) entries — the only ones that vote.
    pub fn active(&self) -> Vec<&HistoryEntry> {
        self.entries.iter().filter(|e| !e.superseded).collect()
    }

    /// Remove every entry from the store.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Retention: keep at most [`MAX_ENTRIES`] active entries, evicting the
    /// oldest by creation time. Superseded entries stay for lineage (they carry
    /// no voting weight) and are bounded by the same limit, oldest first.
    fn retain(&mut self) {
        self.retain_bound(false);
        self.retain_bound(true);
    }

    fn retain_bound(&mut self, superseded: bool) {
        let mut order: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.superseded == superseded)
            .map(|(i, _)| i)
            .collect();
        if order.len() <= MAX_ENTRIES {
            return;
        }
        order.sort_by_key(|&i| (self.entries[i].created_unix, self.entries[i].id.clone()));
        let excess = order.len() - MAX_ENTRIES;
        let drop: BTreeSet<usize> = order.into_iter().take(excess).collect();
        let mut index = 0;
        self.entries.retain(|_| {
            let keep = !drop.contains(&index);
            index += 1;
            keep
        });
    }

    fn next_id(&self, created_unix: u64) -> String {
        let mut n = self.entries.len() as u64 + 1;
        loop {
            let id = format!("{created_unix:012}-{n:04}");
            if !self.entries.iter().any(|e| e.id == id) {
                return id;
            }
            n += 1;
        }
    }

    pub(crate) fn save(&self) -> Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let mut text = String::new();
        for entry in &self.entries {
            let line = serde_json::to_string(entry)
                .map_err(|e| format!("cannot encode history entry: {e}"))?;
            text.push_str(&line);
            text.push('\n');
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| format!("cannot store {}: {e}", self.path.display()))?;
        Ok(())
    }
}

fn parse_entries(text: &str) -> Result<Vec<HistoryEntry>, String> {
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: HistoryEntry =
            serde_json::from_str(line).map_err(|e| format!("history line {}: {e}", index + 1))?;
        entries.push(entry);
    }
    Ok(entries)
}

/// The rubric fingerprint: a hash of the decision specification (context,
/// labels, criteria) — everything except the state text. Two specs with the
/// same text but different rubrics fingerprint differently, so a label
/// verified under one rubric can never be reused under another.
pub(crate) fn rubric_fingerprint(
    context: &str,
    labels: &[String],
    criteria: &BTreeMap<String, String>,
) -> String {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(context.as_bytes());
    hasher.update(b"\n");
    for label in labels {
        hasher.update(label.as_bytes());
        hasher.update(b"\n");
    }
    for (label, criterion) in criteria {
        hasher.update(label.as_bytes());
        hasher.update(b"=");
        hasher.update(criterion.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:016x}", hasher.digest())
}

/// Character trigrams of normalized text — the lexical evidence for
/// neighbourhood. Normalization lowercases and collapses whitespace so
/// "Fix the login bug" and "fix  the login bug" are the same query.
pub(crate) fn char_trigrams(text: &str) -> BTreeSet<String> {
    let normalized: String = text
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let chars: Vec<char> = normalized.chars().collect();
    let mut set = BTreeSet::new();
    if normalized.is_empty() {
        return set;
    }
    if chars.len() < 3 {
        set.insert(normalized);
        return set;
    }
    for window in chars.windows(3) {
        set.insert(window.iter().collect::<String>());
    }
    set
}

/// Jaccard similarity over two sets: `|A ∩ B| / |A ∪ B|`.
pub(crate) fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

/// The lowercase alphanumeric word tokens of text — the lexical evidence for
/// paraphrase, which shares words rather than exact phrasing.
pub(crate) fn token_set(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Similarity between two texts: the stronger of word-overlap (catches
/// paraphrases) and character-trigram (catches morphological variants)
/// Jaccard. Both are bounded in [0, 1].
pub(crate) fn text_similarity(a: &str, b: &str) -> f64 {
    let tokens = jaccard(&token_set(a), &token_set(b));
    let trigrams = jaccard(&char_trigrams(a), &char_trigrams(b));
    tokens.max(trigrams)
}

/// One neighbour that voted for the verdict.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Neighbour {
    pub id: String,
    pub text: String,
    pub label: String,
    pub similarity: f64,
}

/// A confident history answer. `confidence` is the raw similarity-weighted
/// vote share — evidence, not a calibrated probability.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HistoryVerdict {
    pub label: String,
    pub confidence: f64,
    pub neighbours: Vec<Neighbour>,
    pub vote_shares: BTreeMap<String, f64>,
    pub source: String,
    pub version: u32,
    pub basis: String,
}

/// Why the tier could not answer. Every reason preserves the existing
/// fallback policy: the model runs as it did before the tier existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AbstainReason {
    /// The store holds no entry for this rubric.
    Empty,
    /// The spec's rubric fingerprint matches no stored entry.
    RubricMismatch,
    /// The spec's label schema version is not the one this tier reads.
    SchemaMismatch,
    /// No stored example is close enough: the input is unfamiliar.
    Novelty,
    /// Fewer than [`MIN_NEIGHBOURS`] neighbours clear [`MIN_SIMILARITY`].
    InsufficientSupport,
    /// The neighbours disagree: the winning vote share is below
    /// [`MIN_VOTE_SHARE`].
    Disagreement,
}

impl AbstainReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::RubricMismatch => "rubric-mismatch",
            Self::SchemaMismatch => "schema-mismatch",
            Self::Novelty => "novelty",
            Self::InsufficientSupport => "insufficient-support",
            Self::Disagreement => "disagreement",
        }
    }
}

/// The tier's answer: a confident verdict, or a typed abstention.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HistoryDecision {
    Accept(HistoryVerdict),
    Abstain(AbstainReason),
}

/// The retrieval tier over a store. Pure local computation: it opens no daemon
/// and makes no network call, so a warm-only hook path that consults it keeps
/// its no-start/no-network guarantees.
pub(crate) struct HistoryTier {
    store: HistoryStore,
    min_neighbours: usize,
    min_similarity: f64,
    min_vote_share: f64,
}

impl HistoryTier {
    pub fn new(store: HistoryStore) -> Self {
        Self {
            store,
            min_neighbours: MIN_NEIGHBOURS,
            min_similarity: MIN_SIMILARITY,
            min_vote_share: MIN_VOTE_SHARE,
        }
    }

    /// The number of entries in the backing store (active + superseded).
    pub fn store_len(&self) -> usize {
        self.store.len()
    }

    /// Evaluate `spec` against the stored history. The rubric fingerprint keys
    /// applicability; the state text only ranks neighbours.
    pub fn evaluate(&self, spec: &Spec) -> HistoryDecision {
        let fingerprint = rubric_fingerprint(&spec.context, &spec.labels, &spec.criteria);
        if self.store.is_empty() {
            return HistoryDecision::Abstain(AbstainReason::Empty);
        }
        let candidates: Vec<&HistoryEntry> = self
            .store
            .active()
            .into_iter()
            .filter(|e| e.rubric_fingerprint == fingerprint)
            .collect();
        if candidates.is_empty() {
            return HistoryDecision::Abstain(AbstainReason::RubricMismatch);
        }
        if candidates
            .iter()
            .any(|e| e.label_schema_version != LABEL_SCHEMA_VERSION)
        {
            return HistoryDecision::Abstain(AbstainReason::SchemaMismatch);
        }

        let mut neighbours: Vec<Neighbour> = candidates
            .into_iter()
            .map(|e| Neighbour {
                id: e.id.clone(),
                text: e.text.clone(),
                label: e.label.clone(),
                similarity: text_similarity(&spec.text, &e.text),
            })
            .collect();
        neighbours.sort_by(|a, b| b.similarity.total_cmp(&a.similarity));

        // An exact verified match is the strongest evidence: answer it
        // directly, even without a second neighbour.
        let best = &neighbours[0];
        if best.similarity >= 1.0 {
            return HistoryDecision::Accept(self.verdict(
                &neighbours[..1],
                BTreeMap::from([(best.label.clone(), 1.0)]),
            ));
        }

        // Novelty: nothing close enough has been verified.
        if best.similarity < self.min_similarity {
            return HistoryDecision::Abstain(AbstainReason::Novelty);
        }
        let supported: Vec<&Neighbour> = neighbours
            .iter()
            .filter(|n| n.similarity >= self.min_similarity)
            .collect();
        // Support: a single neighbour is not a verified pattern.
        if supported.len() < self.min_neighbours {
            return HistoryDecision::Abstain(AbstainReason::InsufficientSupport);
        }

        let mut votes: BTreeMap<String, f64> = BTreeMap::new();
        for n in &supported {
            *votes.entry(n.label.clone()).or_insert(0.0) += n.similarity;
        }
        let total: f64 = votes.values().sum();
        let top_votes = votes
            .values()
            .max_by(|a, b| a.total_cmp(b))
            .copied()
            .unwrap_or(0.0);
        let share = if total > 0.0 { top_votes / total } else { 0.0 };
        // Disagreement: the verified neighbours do not agree.
        if share < self.min_vote_share {
            return HistoryDecision::Abstain(AbstainReason::Disagreement);
        }

        let shares: BTreeMap<String, f64> =
            votes.iter().map(|(l, v)| (l.clone(), *v / total)).collect();
        let disclosed: Vec<Neighbour> = supported
            .iter()
            .take(DISCLOSED_NEIGHBOURS)
            .map(|n| (*n).clone())
            .collect();
        HistoryDecision::Accept(self.verdict(&disclosed, shares))
    }

    fn verdict(
        &self,
        neighbours: &[Neighbour],
        vote_shares: BTreeMap<String, f64>,
    ) -> HistoryVerdict {
        let (label, confidence) = vote_shares
            .iter()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map_or((String::new(), 0.0), |(l, v)| (l.clone(), *v));
        let mut sources: BTreeSet<&str> = BTreeSet::new();
        for n in neighbours {
            if let Some(entry) = self.store.active().into_iter().find(|e| e.id == n.id) {
                sources.insert(entry.source.as_str());
            }
        }
        HistoryVerdict {
            label,
            confidence,
            neighbours: neighbours.to_vec(),
            vote_shares,
            source: sources.into_iter().collect::<Vec<_>>().join(", "),
            version: LABEL_SCHEMA_VERSION,
            basis: "verified-history retrieval, deterministic, raw vote share over verified neighbours (not a calibrated probability)".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::REMOTE_BASIS;

    fn store() -> HistoryStore {
        HistoryStore::open(std::env::temp_dir().join(format!(
            "pixel-history-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )))
        .unwrap()
    }

    fn spec(text: &str) -> Spec {
        Spec::checked(
            text.to_string(),
            "the rubric".to_string(),
            vec!["yes".to_string(), "no".to_string()],
            BTreeMap::from([("yes".to_string(), "affirm".to_string())]),
        )
        .unwrap()
    }

    fn entry(text: &str, label: &str, spec_: &Spec) -> NewEntry {
        NewEntry::for_spec(
            text.to_string(),
            label.to_string(),
            "task-intent".to_string(),
            "human-verified".to_string(),
            spec_,
        )
    }

    #[test]
    fn empty_store_abstains_empty_and_preserves_fallback() {
        let tier = HistoryTier::new(store());
        match tier.evaluate(&spec("anything")) {
            HistoryDecision::Abstain(AbstainReason::Empty) => {}
            other => panic!("expected Empty abstention, got {other:?}"),
        }
    }

    #[test]
    fn exact_match_accepts_with_full_confidence_and_no_model() {
        let mut store = store();
        let s = spec("fix the login bug");
        store.add(entry("fix the login bug", "yes", &s)).unwrap();
        let tier = HistoryTier::new(store);
        match tier.evaluate(&spec("fix the login bug")) {
            HistoryDecision::Accept(v) => {
                assert_eq!(v.label, "yes");
                assert_eq!(v.confidence, 1.0);
                assert_eq!(v.neighbours.len(), 1);
                assert_eq!(v.source, "human-verified");
                assert_eq!(v.version, LABEL_SCHEMA_VERSION);
                assert!(v.basis.contains("not a calibrated probability"));
            }
            other @ HistoryDecision::Abstain(_) => panic!("expected accept, got {other:?}"),
        }
    }

    #[test]
    fn paraphrase_votes_from_neighbours() {
        let mut store = store();
        let s = spec("fix the login bug");
        store.add(entry("the login is broken", "yes", &s)).unwrap();
        store.add(entry("login broken", "yes", &s)).unwrap();
        store.add(entry("add a dark mode", "no", &s)).unwrap();
        let tier = HistoryTier::new(store);
        match tier.evaluate(&spec("login is broken")) {
            HistoryDecision::Accept(v) => {
                assert_eq!(v.label, "yes");
                assert!(v.confidence >= MIN_VOTE_SHARE, "{}", v.confidence);
                assert!(v.neighbours.len() >= 2);
            }
            other @ HistoryDecision::Abstain(_) => panic!("expected accept, got {other:?}"),
        }
    }

    #[test]
    fn unfamiliar_input_abstains_novelty() {
        let mut store = store();
        let s = spec("fix the login bug");
        store.add(entry("fix the login bug", "yes", &s)).unwrap();
        store.add(entry("the login is broken", "yes", &s)).unwrap();
        let tier = HistoryTier::new(store);
        match tier.evaluate(&spec("xyzzy frobnicate the widget")) {
            HistoryDecision::Abstain(AbstainReason::Novelty) => {}
            other => panic!("expected novelty abstention, got {other:?}"),
        }
    }

    #[test]
    fn single_neighbour_abstains_insufficient_support() {
        let mut store = store();
        let s = spec("fix the login bug");
        store.add(entry("fix the login bug", "yes", &s)).unwrap();
        let tier = HistoryTier::new(store);
        // A near-exact but not exact match: one neighbour, not two.
        match tier.evaluate(&spec("fix the login bug today")) {
            HistoryDecision::Abstain(AbstainReason::InsufficientSupport) => {}
            other => panic!("expected insufficient-support abstention, got {other:?}"),
        }
    }

    #[test]
    fn split_neighbours_abstain_disagreement() {
        let mut store = store();
        let s = spec("fix the login bug");
        store.add(entry("add dark mode", "no", &s)).unwrap();
        store.add(entry("fix login bug", "yes", &s)).unwrap();
        let tier = HistoryTier::new(store);
        // Two close neighbours, one per label: the vote splits and cannot
        // reach MIN_VOTE_SHARE.
        match tier.evaluate(&spec("add dark fix login")) {
            HistoryDecision::Abstain(AbstainReason::Disagreement) => {}
            other => panic!("expected disagreement abstention, got {other:?}"),
        }
    }

    #[test]
    fn same_text_under_a_different_rubric_cannot_reuse_the_label() {
        let mut store = store();
        let s = spec("fix the login bug");
        store.add(entry("fix the login bug", "yes", &s)).unwrap();
        let other = Spec::checked(
            "fix the login bug".to_string(),
            "a different rubric".to_string(),
            vec!["yes".to_string(), "no".to_string()],
            BTreeMap::new(),
        )
        .unwrap();
        let tier = HistoryTier::new(store);
        match tier.evaluate(&other) {
            HistoryDecision::Abstain(AbstainReason::RubricMismatch) => {}
            other => {
                panic!("expected rubric-mismatch abstention under a new rubric, got {other:?}")
            }
        }
    }

    #[test]
    fn changed_labels_cannot_reuse_a_stale_confident_answer() {
        let mut store = store();
        let s = spec("fix the login bug");
        store.add(entry("fix the login bug", "yes", &s)).unwrap();
        // A relabelled vocabulary: same text, different label set.
        let relabelled = Spec::checked(
            "fix the login bug".to_string(),
            "the rubric".to_string(),
            vec!["affirm".to_string(), "deny".to_string()],
            BTreeMap::new(),
        )
        .unwrap();
        let tier = HistoryTier::new(store);
        match tier.evaluate(&relabelled) {
            HistoryDecision::Abstain(AbstainReason::RubricMismatch) => {}
            other => panic!("expected abstention under relabelled vocabulary, got {other:?}"),
        }
    }

    #[test]
    fn removing_an_entry_changes_the_decision() {
        let mut store = store();
        let s = spec("fix the login bug");
        let e = store.add(entry("fix the login bug", "yes", &s)).unwrap();
        let tier = HistoryTier::new(HistoryStore::open(store.path()).unwrap());
        assert!(matches!(
            tier.evaluate(&spec("fix the login bug")),
            HistoryDecision::Accept(_)
        ));
        let mut store = HistoryStore::open(store.path()).unwrap();
        store.remove(&e.id).unwrap();
        let tier = HistoryTier::new(store);
        assert!(matches!(
            tier.evaluate(&spec("fix the login bug")),
            HistoryDecision::Abstain(AbstainReason::Empty)
        ));
    }

    #[test]
    fn correcting_an_entry_supersedes_the_old_label_and_changes_the_decision() {
        let mut store = store();
        let s = spec("fix the login bug");
        let e = store.add(entry("fix the login bug", "yes", &s)).unwrap();
        store.correct(&e.id, "no", "human-verified").unwrap();
        let tier = HistoryTier::new(HistoryStore::open(store.path()).unwrap());
        match tier.evaluate(&spec("fix the login bug")) {
            HistoryDecision::Accept(v) => {
                assert_eq!(v.label, "no", "the correction must win");
                assert_eq!(v.confidence, 1.0);
            }
            other @ HistoryDecision::Abstain(_) => {
                panic!("expected corrected accept, got {other:?}")
            }
        }
        // The superseded entry carries no voting weight: with only the old
        // label stored, the same query would have answered "yes".
        let all = store.all();
        assert_eq!(all.len(), 2);
        assert!(all[0].superseded);
        assert!(!all[1].superseded);
        assert_eq!(all[1].corrections.len(), 1);
        assert_eq!(all[1].corrections[0].label, "yes");
    }

    #[test]
    fn superseded_entries_do_not_accumulate_voting_weight() {
        let mut store = store();
        let s = spec("fix the login bug");
        // Three entries for "yes", then two corrections to "no".
        let e1 = store.add(entry("fix the login bug", "yes", &s)).unwrap();
        let e2 = store.add(entry("the login is broken", "yes", &s)).unwrap();
        store.add(entry("login fails", "yes", &s)).unwrap();
        store.correct(&e1.id, "no", "human-verified").unwrap();
        store.correct(&e2.id, "no", "human-verified").unwrap();
        let tier = HistoryTier::new(HistoryStore::open(store.path()).unwrap());
        match tier.evaluate(&spec("fix the login bug")) {
            HistoryDecision::Accept(v) => {
                assert_eq!(
                    v.label, "no",
                    "corrections must outweigh the superseded cache"
                );
            }
            other @ HistoryDecision::Abstain(_) => {
                panic!("expected corrected accept, got {other:?}")
            }
        }
    }

    #[test]
    fn add_requires_a_source_so_model_predictions_cannot_self_certify() {
        let mut store = store();
        let s = spec("fix the login bug");
        let no_source = NewEntry::for_spec(
            "fix the login bug".to_string(),
            "yes".to_string(),
            "task-intent".to_string(),
            "   ".to_string(),
            &s,
        );
        let error = store.add(no_source).unwrap_err();
        assert!(error.contains("source"), "{error}");
    }

    #[test]
    fn evaluating_never_writes_back_to_the_store() {
        let mut store = store();
        let s = spec("fix the login bug");
        store.add(entry("fix the login bug", "yes", &s)).unwrap();
        let before = store.len();
        let tier = HistoryTier::new(HistoryStore::open(store.path()).unwrap());
        let _ = tier.evaluate(&spec("fix the login bug"));
        let store = HistoryStore::open(store.path()).unwrap();
        assert_eq!(
            store.len(),
            before,
            "a verdict must not self-certify into the store"
        );
    }

    #[test]
    fn store_is_bounded_by_max_entries_with_fifo_retention() {
        let mut store = store();
        let s = spec("fix the login bug");
        let mut first_id = String::new();
        for i in 0..(MAX_ENTRIES + 10) {
            let e = store
                .add(entry(&format!("task number {i}"), "yes", &s))
                .unwrap();
            if i == 0 {
                first_id = e.id;
            }
        }
        assert!(store.len() <= MAX_ENTRIES, "{}", store.len());
        assert!(
            store.active().iter().all(|e| e.id != first_id),
            "the oldest entry must be evicted"
        );
    }

    #[test]
    fn store_persists_across_reopen() {
        let path = std::env::temp_dir().join(format!(
            "pixel-history-persist-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let s = spec("fix the login bug");
        {
            let mut store = HistoryStore::open(&path).unwrap();
            store.add(entry("fix the login bug", "yes", &s)).unwrap();
        }
        let store = HistoryStore::open(&path).unwrap();
        assert_eq!(store.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_of_a_missing_id_is_an_error() {
        let mut store = store();
        assert!(store.remove("nope").is_err());
    }

    #[test]
    fn rubric_fingerprint_changes_with_labels_and_criteria_not_text() {
        let labels = vec!["yes".to_string(), "no".to_string()];
        let criteria = BTreeMap::from([("yes".to_string(), "affirm".to_string())]);
        let a = rubric_fingerprint("ctx", &labels, &criteria);
        let b = rubric_fingerprint("ctx", &labels, &criteria);
        assert_eq!(a, b, "the fingerprint is deterministic");
        let other_labels = vec!["affirm".to_string(), "deny".to_string()];
        let c = rubric_fingerprint("ctx", &other_labels, &criteria);
        assert_ne!(a, c, "relabeling changes the fingerprint");
        let other_ctx = rubric_fingerprint("different", &labels, &criteria);
        assert_ne!(a, other_ctx, "a different rubric changes the fingerprint");
    }

    #[test]
    fn jaccard_is_symmetric_and_bounded() {
        let a = char_trigrams("fix the login bug");
        let b = char_trigrams("fix the login bug");
        let c = char_trigrams("add a dark mode");
        assert_eq!(jaccard(&a, &b), 1.0);
        assert!(jaccard(&a, &c) < 0.2, "{}", jaccard(&a, &c));
        assert_eq!(jaccard(&a, &c), jaccard(&c, &a));
        let empty = BTreeSet::new();
        assert_eq!(jaccard(&empty, &empty), 1.0);
        assert_eq!(jaccard(&a, &empty), 0.0);
    }

    #[test]
    fn remote_basis_is_untouched() {
        // The tier's basis is its own; the model's basis string is unchanged.
        assert!(REMOTE_BASIS.contains("remote LLM"));
    }
}
