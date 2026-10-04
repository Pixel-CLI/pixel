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
            if batch.is_empty() {
                break;
            }
            let mut builder = ShardBuilder::new(&extractor.id());
            let mut max_id = after;
            let mut hits = Vec::new();
            for (id, text) in &batch {
                hits.clear();
                extractor.grams(text.as_bytes(), &mut hits);
                let hashes: Vec<u64> = hits.iter().map(|h| h.hash).collect();
                builder.add_file(&id.to_string(), hashes);
                max_id = (*id).max(max_id);
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

/// Exclusive lock file with stale-lock stealing (a crashed indexer must
/// not wedge the corpus forever).
struct SegmentLock {
    path: PathBuf,
}

impl SegmentLock {
    fn acquire(dir: &Path) -> Result<Self, String> {
        let path = dir.join(".index.lock");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
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
