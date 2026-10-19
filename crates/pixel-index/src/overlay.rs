// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! In-memory dirty overlay: gram sets for files newer than any shard.
//!
//! Populated at open from `git status --porcelain` and updated live by the
//! daemon watcher via `refresh_file` / `remove_file`. A path present in
//! `tombstones` suppresses base/delta candidates for that path; a path in
//! `files` is additionally matched against the query's gram plan in memory.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::gram::GramExtractor;
use crate::index::{MAX_FILE_BYTES, read_regular_bounded};
use crate::posting::GramQuery;

pub const MAX_OVERLAY_FILES: usize = 1000;
pub const MAX_TOMBSTONES: usize = 5000;

#[derive(Default)]
pub struct Overlay {
    /// rel path -> deduped gram hash set (from the working-tree content).
    pub files: HashMap<String, HashSet<u64>>,
    /// rel paths whose base/delta entries are stale (modified or deleted).
    pub tombstones: HashSet<String>,
}

impl Overlay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Re-extract one file from disk. Missing, binary, or symlinked files are
    /// tombstoned without an overlay gram set (they can never match). Symlinks
    /// are rejected so a tracked symlink can never expose out-of-repo content.
    pub fn refresh_file(&mut self, root: &Path, rel_path: &str, extractor: &dyn GramExtractor) {
        self.insert_extracted(rel_path, Self::extract(root, rel_path, extractor));
    }

    /// The gram set [`Overlay::refresh_file`] would store for `rel_path`, read
    /// without touching the overlay: `None` for a missing, binary or
    /// symlinked file. The disk read and the extraction are the slow half of
    /// a refresh, so a caller can do them while readers still use the overlay.
    pub fn extract(
        root: &Path,
        rel_path: &str,
        extractor: &dyn GramExtractor,
    ) -> Option<HashSet<u64>> {
        let content = read_regular_bounded(&root.join(rel_path), MAX_FILE_BYTES).ok()?;
        // Binary sniff: NUL in the first 8KiB (same policy as the builder).
        if content[..content.len().min(8192)].contains(&0) {
            return None;
        }
        let mut hits = Vec::new();
        extractor.grams(&content, &mut hits);
        Some(hits.iter().map(|h| h.hash).collect())
    }

    /// Store what [`Overlay::extract`] read for `rel_path`: tombstone its
    /// base/delta entries, and keep its grams unless there were none.
    pub fn insert_extracted(&mut self, rel_path: &str, grams: Option<HashSet<u64>>) {
        if self.tombstones.len() >= MAX_TOMBSTONES {
            self.tombstones.clear();
        }
        self.tombstones.insert(rel_path.to_string());
        let Some(hashes) = grams else {
            self.files.remove(rel_path);
            return;
        };
        if self.files.len() >= MAX_OVERLAY_FILES
            && !self.files.contains_key(rel_path)
            && let Some(old_key) = self.files.keys().next().cloned()
        {
            self.files.remove(&old_key);
        }
        self.files.insert(rel_path.to_string(), hashes);
    }

    /// Tombstone a deleted file everywhere.
    pub fn remove_file(&mut self, rel_path: &str) {
        self.files.remove(rel_path);
        self.tombstones.insert(rel_path.to_string());
    }

    /// Overlay files whose gram set satisfies the query plan.
    pub fn matching_files(&self, query: &GramQuery) -> Vec<&str> {
        self.files
            .iter()
            .filter(|(_, set)| eval(query, set))
            .map(|(path, _)| path.as_str())
            .collect()
    }
}

fn eval(query: &GramQuery, set: &HashSet<u64>) -> bool {
    match query {
        GramQuery::Literal(h) => set.contains(h),
        GramQuery::And(children) => children.iter().all(|c| eval(c, set)),
        GramQuery::Or(children) => children.iter().any(|c| eval(c, set)),
        GramQuery::All => true,
    }
}
