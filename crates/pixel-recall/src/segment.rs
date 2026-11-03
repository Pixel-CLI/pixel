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
/// Merge the trailing run of under-full segments once it is longer than this:
/// the daemon indexes every 5 s sweep, and a search opens every segment.
const MAX_SMALL_SEGMENTS: usize = 16;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub generation: u64,
    /// Highest turn rowid covered by any segment.
    pub last_turn_id: i64,
    pub segments: Vec<SegmentEntry>,
    /// Files a merge replaced. Deleted by the next merge, not by this one, so
    /// a reader that read the previous manifest can still open them.
    #[serde(default)]
    pub retired: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentEntry {
    pub file: String,
    pub doc_count: u32,
    /// Highest turn rowid in this segment (0 in a manifest written before
    /// merging existed).
    #[serde(default)]
    pub last_turn_id: i64,
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
            let mut seq = self.manifest.generation + self.manifest.segments.len() as u64 + 1;
            // A name a merge retired is still on disk for readers: skip it.
            while self.dir.join(format!("seg-{seq:06}.gpxshard")).exists() {
                seq += 1;
            }
            let name = format!("seg-{seq:06}.gpxshard");
            self.write_segment(&name, &batch)?;
            self.manifest.segments.push(SegmentEntry {
                file: name,
                doc_count: batch.len() as u32,
                last_turn_id: max_id,
            });
            self.manifest.last_turn_id = max_id;
            self.write_manifest()?;
            report.turns_indexed += batch.len();
            report.segments_written += 1;
            after = max_id;
        }
        self.merge_small_segments(store)?;
        report.elapsed_ms = started.elapsed().as_millis();
        Ok(report)
    }

    fn write_segment(&self, name: &str, batch: &[(i64, String)]) -> Result<(), String> {
        let extractor = TrigramExtractor;
        let mut builder = ShardBuilder::new(&extractor.id());
        let mut hits = Vec::new();
        for (id, text) in batch {
            hits.clear();
            extractor.grams(text.as_bytes(), &mut hits);
            let hashes: Vec<u64> = hits.iter().map(|h| h.hash).collect();
            builder.add_file(&id.to_string(), hashes);
        }
        builder
            .write(&self.dir.join(name))
            .map_err(|e| e.to_string())
    }

    /// Rebuild the trailing run of under-full segments from the store as
    /// full segments once the run exceeds `MAX_SMALL_SEGMENTS`. The new files
    /// are written first, the manifest switches to them by atomic rename, and
    /// the replaced files stay on disk until the next merge. Caller holds the
    /// index lock.
    fn merge_small_segments(&mut self, store: &RecallStore) -> Result<(), String> {
        let segs = &self.manifest.segments;
        let mut first = segs.len();
        while first > 0 && (segs[first - 1].doc_count as usize) < SEGMENT_TARGET {
            first -= 1;
        }
        if segs.len() - first <= MAX_SMALL_SEGMENTS {
            return Ok(());
        }
        // A manifest from before `last_turn_id` cannot say where the run
        // starts: merge everything from turn 0 then (once, entries now carry it).
        let mut after = match first.checked_sub(1).map(|i| segs[i].last_turn_id) {
            Some(id) if id > 0 => id,
            _ => {
                first = 0;
                0
            }
        };
        let end = self.manifest.last_turn_id;
        let replaced: Vec<String> = segs[first..].iter().map(|e| e.file.clone()).collect();
        let mut merged = Vec::new();
        let mut written = Vec::new();
        let generation = self.manifest.generation + 1;
        while after < end {
            let mut batch = store
                .turns_for_indexing(after, SEGMENT_TARGET)
                .map_err(|e| e.to_string())?;
            batch.retain(|(id, _)| *id <= end);
            let Some(max_id) = batch.iter().map(|(id, _)| *id).max() else {
                break;
            };
            let name = format!("seg-m{generation}-{}.gpxshard", merged.len());
            if let Err(e) = self.write_segment(&name, &batch) {
                for f in &written {
                    let _ = fs::remove_file(self.dir.join(f));
                }
                return Err(e);
            }
            written.push(name.clone());
            merged.push(SegmentEntry {
                file: name,
                doc_count: batch.len() as u32,
                last_turn_id: max_id,
            });
            after = max_id;
        }
        for f in std::mem::take(&mut self.manifest.retired) {
            let _ = fs::remove_file(self.dir.join(f));
        }
        self.manifest.segments.truncate(first);
        self.manifest.segments.extend(merged);
        self.manifest.retired = replaced;
        self.manifest.generation = generation;
        self.write_manifest()
    }

    /// Force a full rebuild: drop every segment and re-index from turn 0.
    pub fn rebuild(&mut self, store: &RecallStore) -> Result<IndexReport, String> {
        let _lock = SegmentLock::acquire(&self.dir)?;
        for entry in &self.manifest.segments {
            let _ = fs::remove_file(self.dir.join(&entry.file));
        }
        for f in &self.manifest.retired {
            let _ = fs::remove_file(self.dir.join(f));
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

    /// A sweep indexes a handful of turns each time: past `MAX_SMALL_SEGMENTS`
    /// they merge, search answers the same before and after, and every file
    /// the manifest names exists.
    #[test]
    fn index_new_should_merge_small_segments_without_changing_search() {
        use crate::search::{SearchFilters, search};
        let tmp = tempfile::tempdir().unwrap();
        let mut store = RecallStore::open(&tmp.path().join("recall.db")).unwrap();
        let seg_dir = tmp.path().join("segments");
        let mut set = SegmentSet::open(&seg_dir).unwrap();
        let mut merged_at = None;
        for n in 1..=20_i64 {
            add_session(
                &mut store,
                "claude",
                &format!("sess{n:04}"),
                &[(Role::User, "a needle in a haystack")],
            );
            let before = set.manifest.segments.len();
            set.index_new(&store).unwrap();
            if set.manifest.segments.len() <= before && merged_at.is_none() {
                merged_at = Some(n);
            }
            let found = search(
                &store,
                &set,
                "needle",
                false,
                &SearchFilters::default(),
                0,
                100,
            )
            .unwrap();
            let mut ids: Vec<i64> = found.hits.iter().map(|h| h.turn_id).collect();
            ids.sort_unstable();
            assert_eq!(ids, (1..=n).collect::<Vec<_>>(), "after batch {n}");
        }
        assert_eq!(merged_at, Some(MAX_SMALL_SEGMENTS as i64 + 1));
        assert!(
            set.manifest.segments.len() <= MAX_SMALL_SEGMENTS,
            "{:?}",
            set.manifest
        );
        let on_disk = SegmentSet::open(&seg_dir).unwrap();
        assert_eq!(on_disk.open_shards().len(), on_disk.manifest.segments.len());
        for f in &on_disk.manifest.retired {
            assert!(
                seg_dir.join(f).exists(),
                "retired {f} must outlive one merge"
            );
        }
    }

    /// A manifest written before segments carried `last_turn_id` merges
    /// everything from turn 0 and loses nothing.
    #[test]
    fn merge_should_cover_a_legacy_manifest_from_turn_zero() {
        use crate::search::{SearchFilters, search};
        let tmp = tempfile::tempdir().unwrap();
        let mut store = RecallStore::open(&tmp.path().join("recall.db")).unwrap();
        let seg_dir = tmp.path().join("segments");
        let mut set = SegmentSet::open(&seg_dir).unwrap();
        for n in 1..=MAX_SMALL_SEGMENTS {
            add_session(
                &mut store,
                "claude",
                &format!("s{n}"),
                &[(Role::User, "needle")],
            );
            set.index_new(&store).unwrap();
        }
        for e in &mut set.manifest.segments {
            e.last_turn_id = 0;
        }
        set.write_manifest().unwrap();
        add_session(&mut store, "claude", "last", &[(Role::User, "needle")]);
        set.index_new(&store).unwrap();
        assert_eq!(set.manifest.segments.len(), 1);
        let found = search(
            &store,
            &set,
            "needle",
            false,
            &SearchFilters::default(),
            0,
            100,
        )
        .unwrap();
        assert_eq!(found.hits.len(), MAX_SMALL_SEGMENTS + 1);
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
