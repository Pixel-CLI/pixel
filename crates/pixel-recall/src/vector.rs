// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! i8-quantized vector segments with exact brute-force KNN.
//!
//! Same publish discipline as the text shards (magic/version header,
//! tmp + fsync + rename, immutable segments). At corpus scale (≤ a few
//! million chunks) an exact scan with metadata pre-filtering beats an ANN
//! graph: no recall loss, trivial filtering, no extra dependency.
//!
//! Layout (little-endian):
//! ```text
//! header (fixed 96 bytes):
//!   magic  b"GPXVECS1"                8
//!   version u32                       4
//!   dim u32                           4
//!   count u64                         8
//!   model_id [u8; 64] (zero-padded)  64
//!   reserved 8
//! rows (count × (16 + dim)):
//!   chunk_id u64 | scale f32 | reserved u32 | i8[dim]
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use serde::{Deserialize, Serialize};

pub const MAGIC: &[u8; 8] = b"GPXVECS1";
pub const VERSION: u32 = 1;
const HEADER_LEN: usize = 96;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct VectorMeta {
    pub model_id: String,
    /// `crate::embed::embedder_revision` of the model when the vectors were
    /// written: the same model id can embed differently across library
    /// releases, and vectors of two revisions must not be mixed. Absent in
    /// stores written before it existed, which reads as revision 0.
    #[serde(default)]
    pub revision: u32,
    pub dim: usize,
    pub segments: Vec<String>,
    /// Highest chunk_id stored, for append bookkeeping.
    pub last_chunk_id: i64,
}

pub struct VectorStore {
    dir: PathBuf,
    pub meta: VectorMeta,
}

pub struct OpenSegment {
    mmap: Mmap,
    dim: usize,
    count: usize,
}

impl VectorStore {
    pub fn open(dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|e| format!("vectors dir: {e}"))?;
        let meta_path = dir.join("meta.json");
        let meta = match fs::read(&meta_path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| format!("corrupt vector meta: {e}"))?
            }
            Err(_) => VectorMeta::default(),
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            meta,
        })
    }

    /// Refuse to serve or extend a store built with a different model.
    pub fn check_model(&self, model_id: &str, dim: usize) -> Result<(), String> {
        if self.meta.model_id.is_empty() {
            return Ok(());
        }
        if self.meta.model_id != model_id || self.meta.dim != dim {
            return Err(format!(
                "vector store was built with model '{}' ({}d) but the active model is '{}' ({}d) — run `pixel recall embed --rebuild`",
                self.meta.model_id, self.meta.dim, model_id, dim
            ));
        }
        Ok(())
    }

    /// True when the stored vectors were written by an older (or newer)
    /// revision of the model the store is built with: they no longer match
    /// what the model embeds today, so the store must be re-embedded.
    pub fn stale_revision(&self) -> bool {
        !self.meta.model_id.is_empty()
            && self.meta.revision != crate::embed::embedder_revision(&self.meta.model_id)
    }

    /// Drop every segment but keep the model, now at its current revision:
    /// the store stays bound to the model it was built with while the
    /// corpus is embedded again.
    pub fn reset_for_reembed(&mut self) -> Result<(), String> {
        for seg in &self.meta.segments {
            let _ = fs::remove_file(self.dir.join(seg));
        }
        self.meta.segments.clear();
        self.meta.last_chunk_id = 0;
        self.meta.revision = crate::embed::embedder_revision(&self.meta.model_id);
        self.write_meta()
    }

    fn write_meta(&self) -> Result<(), String> {
        let tmp = self.dir.join("meta.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&self.meta).unwrap())
            .map_err(|e| e.to_string())?;
        fs::rename(&tmp, self.dir.join("meta.json")).map_err(|e| e.to_string())
    }

    /// Write one immutable segment of (chunk_id, f32 vector) rows.
    pub fn append_segment(
        &mut self,
        model_id: &str,
        dim: usize,
        rows: &[(i64, Vec<f32>)],
    ) -> Result<(), String> {
        if rows.is_empty() {
            return Ok(());
        }
        self.check_model(model_id, dim)?;
        let seq = self.meta.segments.len() as u64 + 1;
        let name = format!("seg-{seq:06}.vec");
        let tmp = self.dir.join(format!("{name}.tmp"));
        {
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)
                .map_err(|e| e.to_string())?;
            let mut w = BufWriter::new(file);
            let mut header = [0u8; HEADER_LEN];
            header[0..8].copy_from_slice(MAGIC);
            header[8..12].copy_from_slice(&VERSION.to_le_bytes());
            header[12..16].copy_from_slice(&(dim as u32).to_le_bytes());
            header[16..24].copy_from_slice(&(rows.len() as u64).to_le_bytes());
            let mid = model_id.as_bytes();
            header[24..24 + mid.len().min(64)].copy_from_slice(&mid[..mid.len().min(64)]);
            w.write_all(&header).map_err(|e| e.to_string())?;
            let mut quantized = vec![0i8; dim];
            for (chunk_id, vec) in rows {
                if vec.len() != dim {
                    return Err("vector dim mismatch".to_string());
                }
                let max_abs = vec.iter().fold(0f32, |m, v| m.max(v.abs()));
                let scale = if max_abs > 0.0 { max_abs / 127.0 } else { 1.0 };
                for (q, v) in quantized.iter_mut().zip(vec) {
                    *q = (v / scale).round().clamp(-127.0, 127.0) as i8;
                }
                w.write_all(&chunk_id.to_le_bytes())
                    .map_err(|e| e.to_string())?;
                w.write_all(&scale.to_le_bytes())
                    .map_err(|e| e.to_string())?;
                w.write_all(&0u32.to_le_bytes())
                    .map_err(|e| e.to_string())?;
                // SAFETY: `quantized` is a Vec<i8> of exactly `dim` elements; i8 and u8
                // share size and alignment, and the slice does not outlive `quantized`.
                let bytes: &[u8] =
                    unsafe { std::slice::from_raw_parts(quantized.as_ptr().cast::<u8>(), dim) };
                w.write_all(bytes).map_err(|e| e.to_string())?;
                self.meta.last_chunk_id = self.meta.last_chunk_id.max(*chunk_id);
            }
            w.flush().map_err(|e| e.to_string())?;
            w.get_ref().sync_all().map_err(|e| e.to_string())?;
        }
        fs::rename(&tmp, self.dir.join(&name)).map_err(|e| e.to_string())?;
        if self.meta.segments.is_empty() {
            self.meta.revision = crate::embed::embedder_revision(model_id);
        }
        self.meta.model_id = model_id.to_string();
        self.meta.dim = dim;
        self.meta.segments.push(name);
        self.write_meta()
    }

    /// Drop every segment (model change / rebuild).
    pub fn clear(&mut self) -> Result<(), String> {
        for seg in &self.meta.segments {
            let _ = fs::remove_file(self.dir.join(seg));
        }
        self.meta = VectorMeta::default();
        self.write_meta()
    }

    pub fn open_segments(&self) -> Vec<OpenSegment> {
        self.meta
            .segments
            .iter()
            .filter_map(|name| OpenSegment::open(&self.dir.join(name)).ok())
            .collect()
    }

    /// Exact KNN over every segment. `allowed` (when present) is the set of
    /// chunk ids that pass the metadata filters — exact pre-filtering.
    pub fn knn(
        &self,
        query: &[f32],
        k: usize,
        allowed: Option<&std::collections::HashSet<i64>>,
    ) -> Vec<(i64, f32)> {
        let mut best: Vec<(i64, f32)> = Vec::with_capacity(k + 1);
        for seg in self.open_segments() {
            seg.scan(query, |chunk_id, score| {
                if let Some(set) = allowed
                    && !set.contains(&chunk_id)
                {
                    return;
                }
                if best.len() < k {
                    best.push((chunk_id, score));
                    if best.len() == k {
                        best.sort_by(|a, b| b.1.total_cmp(&a.1));
                    }
                } else if score > best[k - 1].1 {
                    best[k - 1] = (chunk_id, score);
                    let mut i = k - 1;
                    while i > 0 && best[i].1 > best[i - 1].1 {
                        best.swap(i, i - 1);
                        i -= 1;
                    }
                }
            });
        }
        best.sort_by(|a, b| b.1.total_cmp(&a.1));
        best
    }
}

impl OpenSegment {
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        // SAFETY: segments are written to a temp path and renamed into place
        // (`append` above) and never modified afterwards, so the mapping cannot
        // change under us; the header and every offset are validated below.
        let mmap = unsafe { Mmap::map(&file).map_err(|e| e.to_string())? };
        if mmap.len() < HEADER_LEN || &mmap[0..8] != MAGIC {
            return Err("bad vector segment header".to_string());
        }
        let version = u32::from_le_bytes(mmap[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(format!("vector segment version {version}"));
        }
        let dim = u32::from_le_bytes(mmap[12..16].try_into().unwrap()) as usize;
        let count = u64::from_le_bytes(mmap[16..24].try_into().unwrap()) as usize;
        let row = 16 + dim;
        if HEADER_LEN + count * row > mmap.len() {
            return Err("truncated vector segment".to_string());
        }
        Ok(Self { mmap, dim, count })
    }

    /// Visit every row with its cosine-proportional score (dot product of
    /// the normalized query against the quantized vector).
    fn scan(&self, query: &[f32], mut visit: impl FnMut(i64, f32)) {
        if query.len() != self.dim {
            return;
        }
        let row_len = 16 + self.dim;
        for i in 0..self.count {
            let off = HEADER_LEN + i * row_len;
            let chunk_id = i64::from_le_bytes(self.mmap[off..off + 8].try_into().unwrap());
            let scale = f32::from_le_bytes(self.mmap[off + 8..off + 12].try_into().unwrap());
            let vec_bytes = &self.mmap[off + 16..off + 16 + self.dim];
            let mut dot = 0f32;
            for (q, b) in query.iter().zip(vec_bytes) {
                dot += q * (*b as i8 as f32);
            }
            visit(chunk_id, dot * scale);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_knn() {
        let dir = std::env::temp_dir().join(format!("gpx-vec-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = VectorStore::open(&dir).unwrap();
        let rows = vec![
            (1i64, vec![1.0, 0.0, 0.0]),
            (2, vec![0.0, 1.0, 0.0]),
            (3, vec![0.7, 0.7, 0.0]),
        ];
        store.append_segment("test-model", 3, &rows).unwrap();
        let hits = store.knn(&[1.0, 0.0, 0.0], 2, None);
        assert_eq!(hits[0].0, 1);
        assert_eq!(hits[1].0, 3);
        // Filtering excludes the best hit.
        let allowed: std::collections::HashSet<i64> = [2, 3].into_iter().collect();
        let hits = store.knn(&[1.0, 0.0, 0.0], 2, Some(&allowed));
        assert_eq!(hits[0].0, 3);
        // Model mismatch is a loud error.
        assert!(store.check_model("other-model", 3).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A store is stale only when bound to a model whose revision moved
    /// since it was written; a reset keeps the model and takes the current
    /// revision.
    #[test]
    fn stale_revision_and_reset_for_reembed_track_the_model_revision() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = VectorStore::open(dir.path()).unwrap();
        assert!(!store.stale_revision(), "an empty store is never stale");
        let potion = crate::embed::POTION_REPO;
        store
            .append_segment(potion, 3, &[(7i64, vec![1.0, 0.0, 0.0])])
            .unwrap();
        assert_eq!(store.meta.revision, crate::embed::MODEL2VEC_REVISION);
        assert!(!store.stale_revision());
        let segment = dir.path().join(&store.meta.segments[0]);
        assert!(segment.exists());

        store.meta.revision = 0;
        assert!(store.stale_revision());
        store.reset_for_reembed().unwrap();
        assert!(store.meta.segments.is_empty());
        assert!(!segment.exists(), "old segment files removed");
        assert_eq!(store.meta.last_chunk_id, 0);
        assert_eq!(store.meta.model_id, potion);
        assert_eq!(store.meta.revision, crate::embed::MODEL2VEC_REVISION);
        let reread = VectorStore::open(dir.path()).unwrap();
        assert!(!reread.stale_revision(), "the reset is on disk");

        let mut other = VectorStore::open(&dir.path().join("other")).unwrap();
        other
            .append_segment(crate::embed::E5_MODEL_ID, 3, &[(1i64, vec![0.0, 1.0, 0.0])])
            .unwrap();
        assert_eq!(other.meta.revision, 0);
        assert!(
            !other.stale_revision(),
            "a model at revision 0 written at 0"
        );
    }
}
