// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Append-only lexical segments over turn text.
//!
//! Each segment is a standard GPXSHARD (pixel-index) whose "path" strings
//! carry turn rowids — that one trick makes the file-granular shard format
//! turn-granular with zero format changes. This is load-bearing: if core
//! ever validates or normalizes paths, this module must adapt.
//!
//! Segments are immutable and never rewritten by ingest; re-ingested
//! sessions leave stale rowids behind in old segments, which die at the
//! SQL-fetch step (the row no longer exists) or at regex verification.
//! Turns newer than `last_turn_id` are searched unindexed (freshness
//! overlay), so search is correct even with a stale segment set.

use std::fs;
use std::path::{Path, PathBuf};

use pixel_index::TrigramExtractor;
use pixel_index::gram::GramExtractor;
use pixel_index::shard::{Shard, ShardBuilder};
use serde::{Deserialize, Serialize};

use crate::store::RecallStore;

const MANIFEST: &str = "manifest.json";
/// Flush a segment once this many turns are pending.
const SEGMENT_TARGET: usize = 65_536;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub generation: u64,
    /// Highest turn rowid covered by any segment.
    pub last_turn_id: i64,
    pub segments: Vec<SegmentEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentEntry {
    pub file: String,
    pub doc_count: u32,
}

pub struct SegmentSet {
    dir: PathBuf,
    pub manifest: Manifest,
}

#[derive(Debug, Default)]
pub struct IndexReport {
    pub turns_indexed: usize,
    pub segments_written: usize,
    pub elapsed_ms: u128,
}

impl SegmentSet {
    pub fn open(dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|e| format!("segments dir: {e}"))?;
        let manifest_path = dir.join(MANIFEST);
        let manifest = match fs::read(&manifest_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("corrupt segments manifest: {e}"))?,
            Err(_) => Manifest::default(),
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            manifest,
        })
    }

    fn write_manifest(&self) -> Result<(), String> {
        let tmp = self.dir.join("manifest.json.tmp");
        let bytes = serde_json::to_vec_pretty(&self.manifest).map_err(|e| e.to_string())?;
        fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
        fs::rename(&tmp, self.dir.join(MANIFEST)).map_err(|e| e.to_string())
    }

    /// Index every turn newer than the manifest's high-water mark.
    ///
    /// Guarded by an exclusive lock file: concurrent `recall index` runs
    /// would otherwise race on segment names and clobber each other's
    /// shards, silently losing postings below the high-water mark.
    pub fn index_new(&mut self, store: &RecallStore) -> Result<IndexReport, String> {
        let _lock = SegmentLock::acquire(&self.dir)?;
        // Another process may have advanced the manifest while we waited.
        let manifest_path = self.dir.join(MANIFEST);
        if let Ok(bytes) = fs::read(&manifest_path)
            && let Ok(fresh) = serde_json::from_slice::<Manifest>(&bytes)
        {
            self.manifest = fresh;
        }
        let started = std::time::Instant::now();
        let extractor = TrigramExtractor;
        let mut report = IndexReport::default();
        let mut after = self.manifest.last_turn_id;
        loop {
            let batch = store
                .turns_for_indexing(after, SEGMENT_TARGET)
                .map_err(|e| e.to_string())?;
            // Stop on a batch that does not move past `after` (empty, or a
            // store answering the same rows again): the loop always ends.
            let max_id = batch.iter().map(|(id, _)| *id).max().unwrap_or(after);
            if max_id <= after {
                break;
            }
            let mut builder = ShardBuilder::new(&extractor.id());
            let mut hits = Vec::new();
            for (id, text) in &batch {
                hits.clear();
                extractor.grams(text.as_bytes(), &mut hits);
                let hashes: Vec<u64> = hits.iter().map(|h| h.hash).collect();
                builder.add_file(&id.to_string(), hashes);
            }
            let seq = self.manifest.generation + self.manifest.segments.len() as u64 + 1;
            let name = format!("seg-{seq:06}.gpxshard");
            builder
                .write(&self.dir.join(&name))
                .map_err(|e| e.to_string())?;
            self.manifest.segments.push(SegmentEntry {
                file: name,
                doc_count: batch.len() as u32,
            });
            self.manifest.last_turn_id = max_id;
            self.write_manifest()?;
            report.turns_indexed += batch.len();
            report.segments_written += 1;
            after = max_id;
        }
        report.elapsed_ms = started.elapsed().as_millis();
        Ok(report)
    }

    /// Force a full rebuild: drop every segment and re-index from turn 0.
    pub fn rebuild(&mut self, store: &RecallStore) -> Result<IndexReport, String> {
        let _lock = SegmentLock::acquire(&self.dir)?;
        for entry in &self.manifest.segments {
            let _ = fs::remove_file(self.dir.join(&entry.file));
        }
        self.manifest = Manifest {
            generation: self.manifest.generation + 1,
            ..Default::default()
        };
        self.write_manifest()?;
        drop(_lock);
        self.index_new(store)
    }

    /// Open every segment shard for querying.
    pub fn open_shards(&self) -> Vec<Shard> {
        self.manifest
            .segments
            .iter()
            .filter_map(|e| Shard::open(&self.dir.join(&e.file)).ok())
            .collect()
    }
}

/// How long an indexer waits for another one's lock before giving up.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// Exclusive lock file with stale-lock stealing (a crashed indexer must
/// not wedge the corpus forever).
struct SegmentLock {
    path: PathBuf,
}

impl SegmentLock {
    fn acquire(dir: &Path) -> Result<Self, String> {
        Self::acquire_within(dir, LOCK_WAIT)
    }

    /// [`Self::acquire`] waiting at most `wait` for another holder.
    fn acquire_within(dir: &Path, wait: std::time::Duration) -> Result<Self, String> {
        let path = dir.join(".index.lock");
        let deadline = std::time::Instant::now() + wait;
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Steal locks older than 10 minutes (crashed holder).
                    if let Ok(meta) = fs::metadata(&path)
                        && let Ok(modified) = meta.modified()
                        && modified.elapsed().unwrap_or_default().as_secs() > 600
                    {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if std::time::Instant::now() > deadline {
                        return Err(
                            "segment index lock held for over 10 minutes — another indexer is running (or remove segments/.index.lock)"
                                .to_string(),
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                Err(e) => return Err(format!("segment lock: {e}")),
            }
        }
    }
}

impl Drop for SegmentLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Role;
    use crate::testutil::add_session;
    use std::time::Duration;

    fn store_with_turns(dir: &Path) -> RecallStore {
        let mut store = RecallStore::open(&dir.join("recall.db")).unwrap();
        add_session(
            &mut store,
            "claude",
            "aaaa1111",
            &[(Role::User, "one needle"), (Role::Assistant, "two needle")],
        );
        add_session(&mut store, "codex", "bbbb2222", &[(Role::User, "three")]);
        store
    }

    #[test]
    fn index_new_should_report_its_turns_persist_the_manifest_and_release_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_turns(tmp.path());
        let seg_dir = tmp.path().join("segments");
        let mut set = SegmentSet::open(&seg_dir).unwrap();
        let report = set.index_new(&store).unwrap();
        assert_eq!(report.turns_indexed, 3);
        assert_eq!(report.segments_written, 1);
        assert!(!seg_dir.join(".index.lock").exists());
        let reopened = SegmentSet::open(&seg_dir).unwrap();
        assert_eq!(reopened.manifest.last_turn_id, 3);
        assert_eq!(reopened.manifest.segments.len(), 1);
        assert_eq!(set.index_new(&store).unwrap().turns_indexed, 0);
    }

    #[test]
    fn rebuild_should_reindex_everything_under_a_new_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_turns(tmp.path());
        let seg_dir = tmp.path().join("segments");
        let mut set = SegmentSet::open(&seg_dir).unwrap();
        set.index_new(&store).unwrap();
        let report = set.rebuild(&store).unwrap();
        assert_eq!(report.turns_indexed, 3);
        assert_eq!(report.segments_written, 1);
        assert_eq!(set.manifest.generation, 1);
        assert_eq!(set.manifest.segments[0].file, "seg-000002.gpxshard");
        assert!(!seg_dir.join("seg-000001.gpxshard").exists());
        let reopened = SegmentSet::open(&seg_dir).unwrap();
        assert_eq!(reopened.manifest.generation, 1);
    }

    /// A held lock is waited for, not failed on: the second indexer gets
    /// it once the first lets go.
    #[test]
    fn acquire_should_wait_for_a_held_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let first = SegmentLock::acquire_within(tmp.path(), Duration::from_secs(5)).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(first);
        });
        let second = SegmentLock::acquire_within(tmp.path(), Duration::from_secs(5));
        releaser.join().unwrap();
        assert!(second.is_ok(), "{:?}", second.err());
    }

    #[test]
    fn acquire_should_fail_at_once_on_an_error_other_than_a_held_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        let err = SegmentLock::acquire_within(&missing, Duration::from_secs(1))
            .err()
            .unwrap();
        assert!(err.starts_with("segment lock: "), "{err}");
    }

    #[test]
    fn acquire_should_give_up_after_its_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let _held = SegmentLock::acquire_within(tmp.path(), Duration::from_secs(1)).unwrap();
        let err = SegmentLock::acquire_within(tmp.path(), Duration::ZERO)
            .err()
            .unwrap();
        assert!(err.contains("lock held"), "{err}");
    }
}
