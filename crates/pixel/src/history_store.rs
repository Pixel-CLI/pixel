// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Verified-history store for the opt-in classify history tier.
//!
//! A bounded, project-local store of human/independently verified task-intent
//! classifications. Each entry records its source, correction lineage, task
//! family, label-schema version and rubric fingerprint so that retrieval can
//! key on the decision specification — not state text alone — and refuse to
//! reuse a label verified under a different rubric.
//!
//! The store is deliberately separate from the machine-wide recall corpus:
//! it lives under the project's `.pixel/` directory, is bounded by a
//! configurable entry cap, and supports retention, deletion and project
//! isolation. Model predictions are never written back as gold labels; only
//! explicit human/independent corrections enter the store.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The current label-schema version. Bump when the intent vocabulary or
/// criteria change in a way that invalidates old entries.
pub(crate) const LABEL_SCHEMA_VERSION: u32 = 1;

/// Default maximum number of entries in the verified-history store.
pub(crate) const DEFAULT_MAX_ENTRIES: usize = 1_000;

/// One verified classification entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HistoryEntry {
    /// The state text that was classified.
    pub text: String,
    /// The verified label (must be one of the intent labels).
    pub label: String,
    /// How this label was verified: "human", "independent", or "correction".
    pub source: String,
    /// Correction lineage: IDs of entries this one corrects/supersedes.
    pub supersedes: Vec<String>,
    /// Task family for partitioning (e.g. "coding-agent-prompt").
    pub task_family: String,
    /// Label-schema version at the time of verification.
    pub label_schema_version: u32,
    /// Rubric fingerprint: hash of the criteria + context used.
    pub rubric_fingerprint: String,
    /// Unix timestamp of when this entry was verified.
    pub verified_at: u64,
    /// Optional note about the verification.
    pub note: Option<String>,
}

/// The verified-history store: a bounded, project-local collection of
/// verified classifications.
#[derive(Debug, Clone, Default)]
pub(crate) struct HistoryStore {
    entries: Vec<HistoryEntry>,
    max_entries: usize,
}

impl HistoryStore {
    /// Create a new empty store with the default cap.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }

    /// Create a new empty store with a custom cap.
    pub fn with_cap(max_entries: usize) -> Self {
        Self {
            entries: Vec::new(),
            max_entries,
        }
    }

    /// The number of entries currently stored.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// All entries, in insertion order.
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Add a verified entry. When the store is at capacity, the oldest
    /// entry is evicted (FIFO). Returns the evicted entry if any.
    pub fn add(&mut self, entry: HistoryEntry) -> Option<HistoryEntry> {
        let evicted = if self.entries.len() >= self.max_entries {
            Some(self.entries.remove(0))
        } else {
            None
        };
        self.entries.push(entry);
        evicted
    }

    /// Remove an entry by its text + label pair (the natural key).
    /// Returns the removed entry if found.
    pub fn remove(&mut self, text: &str, label: &str) -> Option<HistoryEntry> {
        let pos = self
            .entries
            .iter()
            .position(|e| e.text == text && e.label == label)?;
        Some(self.entries.remove(pos))
    }

    /// Correct a label: remove the old entry and add a corrected one with
    /// the correction lineage recorded. Returns the old entry if found.
    pub fn correct(
        &mut self,
        text: &str,
        old_label: &str,
        new_entry: HistoryEntry,
    ) -> Option<HistoryEntry> {
        let old = self.remove(text, old_label)?;
        let mut entry = new_entry;
        entry.supersedes.push(format!("{}:{}", old.text, old.label));
        self.add(entry);
        Some(old)
    }

    /// Entries matching a task family.
    pub fn by_task_family(&self, family: &str) -> Vec<&HistoryEntry> {
        self.entries
            .iter()
            .filter(|e| e.task_family == family)
            .collect()
    }

    /// Entries matching a rubric fingerprint.
    pub fn by_rubric(&self, fingerprint: &str) -> Vec<&HistoryEntry> {
        self.entries
            .iter()
            .filter(|e| e.rubric_fingerprint == fingerprint)
            .collect()
    }

    /// Load a store from a JSON file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let data = std::fs::read_to_string(path)
            .map_err(|e| format!("read history store {}: {e}", path.display()))?;
        let entries: Vec<HistoryEntry> = serde_json::from_str(&data)
            .map_err(|e| format!("parse history store {}: {e}", path.display()))?;
        let mut store = Self::new();
        for entry in entries {
            store.add(entry);
        }
        Ok(store)
    }

    /// Save the store to a JSON file.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create history store dir {}: {e}", parent.display()))?;
        }
        let data = serde_json::to_string_pretty(&self.entries)
            .map_err(|e| format!("serialize history store: {e}"))?;
        std::fs::write(path, data)
            .map_err(|e| format!("write history store {}: {e}", path.display()))?;
        Ok(())
    }

    /// The store's file name within a project's `.pixel/` directory.
    pub fn store_filename() -> &'static str {
        "classify_history.json"
    }

    /// The store's path for a given project root.
    pub fn store_path(project_root: &Path) -> PathBuf {
        project_root
            .join(".pixel")
            .join(Self::store_filename())
    }

    /// Compute a rubric fingerprint from the context and criteria.
    /// This is a simple hash that changes when the rubric changes.
    pub fn rubric_fingerprint(context: &str, criteria: &BTreeMap<String, String>) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        context.hash(&mut hasher);
        for (k, v) in criteria {
            k.hash(&mut hasher);
            v.hash(&mut hasher);
        }
        format!("{:016x}", hasher.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(text: &str, label: &str) -> HistoryEntry {
        HistoryEntry {
            text: text.to_string(),
            label: label.to_string(),
            source: "human".to_string(),
            supersedes: vec![],
            task_family: "coding-agent-prompt".to_string(),
            label_schema_version: LABEL_SCHEMA_VERSION,
            rubric_fingerprint: "abc123".to_string(),
            verified_at: 1_700_000_000,
            note: None,
        }
    }

    #[test]
    fn empty_store_is_empty() {
        let store = HistoryStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn add_and_remove_entries() {
        let mut store = HistoryStore::new();
        store.add(entry("fix the bug", "bugfix"));
        store.add(entry("add a flag", "feature"));
        assert_eq!(store.len(), 2);

        let removed = store.remove("fix the bug", "bugfix");
        assert!(removed.is_some());
        assert_eq!(store.len(), 1);

        let not_found = store.remove("nonexistent", "bugfix");
        assert!(not_found.is_none());
    }

    #[test]
    fn correct_replaces_old_entry_with_lineage() {
        let mut store = HistoryStore::new();
        store.add(entry("fix the bug", "bugfix"));
        let corrected = HistoryEntry {
            label: "investigate".to_string(),
            source: "correction".to_string(),
            ..entry("fix the bug", "investigate")
        };
        let old = store.correct("fix the bug", "bugfix", corrected);
        assert!(old.is_some());
        assert_eq!(store.len(), 1);
        let entry = &store.entries()[0];
        assert_eq!(entry.label, "investigate");
        assert_eq!(entry.source, "correction");
        assert_eq!(entry.supersedes.len(), 1);
    }

    #[test]
    fn capacity_evicts_oldest() {
        let mut store = HistoryStore::with_cap(2);
        store.add(entry("first", "bugfix"));
        store.add(entry("second", "feature"));
        let evicted = store.add(entry("third", "refactor"));
        assert!(evicted.is_some());
        assert_eq!(evicted.unwrap().text, "first");
        assert_eq!(store.len(), 2);
        assert_eq!(store.entries()[0].text, "second");
    }

    #[test]
    fn filter_by_task_family_and_rubric() {
        let mut store = HistoryStore::new();
        store.add(entry("fix the bug", "bugfix"));
        let mut other = entry("review this", "review");
        other.task_family = "code-review".to_string();
        store.add(other);

        assert_eq!(store.by_task_family("coding-agent-prompt").len(), 1);
        assert_eq!(store.by_task_family("code-review").len(), 1);
        assert_eq!(store.by_rubric("abc123").len(), 2);
    }

    #[test]
    fn rubric_fingerprint_changes_with_criteria() {
        let mut criteria1 = BTreeMap::new();
        criteria1.insert("bugfix".to_string(), "something broken".to_string());
        let mut criteria2 = BTreeMap::new();
        criteria2.insert("bugfix".to_string(), "something broken".to_string());
        criteria2.insert("feature".to_string(), "new behaviour".to_string());

        let fp1 = HistoryStore::rubric_fingerprint("context", &criteria1);
        let fp2 = HistoryStore::rubric_fingerprint("context", &criteria2);
        let fp3 = HistoryStore::rubric_fingerprint("different", &criteria1);

        assert_ne!(fp1, fp2, "different criteria must yield different fingerprints");
        assert_ne!(fp1, fp3, "different context must yield different fingerprints");
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("pixel-history-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.json");

        let mut store = HistoryStore::new();
        store.add(entry("fix the bug", "bugfix"));
        store.add(entry("add a flag", "feature"));
        store.save(&path).unwrap();

        let loaded = HistoryStore::load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.entries()[0].text, "fix the bug");
        assert_eq!(loaded.entries()[1].label, "feature");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn store_path_is_project_local() {
        let root = Path::new("/some/project");
        let path = HistoryStore::store_path(root);
        assert!(path.ends_with(".pixel/classify_history.json"));
    }
}
