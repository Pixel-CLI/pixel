// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The on-disk vector store: `.pixel/code-vectors/`.
//!
//! Vectors are keyed by [`super::chunk::chunk_key`] and quantised to `i8` with
//! one scale per row. Segments are immutable files published by writing a
//! temporary name, syncing and renaming; the manifest that lists them is
//! replaced the same way, so a reader sees either the old set or the new one.
//! A `lock` file serialises writers with `flock`. When more than a quarter of
//! the stored rows are no longer reachable from the corpus, the store is
//! rewritten ([`VectorStore::compact`]).
//!
//! Segment layout, little endian:
//!
//! ```text
//! header, 32 bytes: magic "PIXVECS1" | version u32 | dim u32 | count u64 | reserved u64
//! rows, count x (24 + dim):  key u128 | scale f32 | reserved u32 | i8 x dim
//! ```

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use super::{Result, SemanticError};

/// The directory under `.pixel/` that holds the store.
pub const STORE_DIR: &str = "code-vectors";

const MAGIC: &[u8; 8] = b"PIXVECS1";
const VERSION: u32 = 1;
const HEADER: usize = 32;
const MANIFEST: &str = "manifest.json";

/// More unreachable rows than this share of the store trigger a rewrite.
pub const COMPACT_SHARE: (usize, usize) = (1, 4);

/// What the manifest records.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub model_id: String,
    pub revision: u32,
    pub dim: usize,
    pub chunker_version: u32,
    /// Segment file names, oldest first.
    pub segments: Vec<String>,
    /// Rows across all segments (a key repeated in two segments counts twice).
    pub rows: usize,
}

/// One stored vector, dequantised on demand.
#[derive(Debug, Clone, PartialEq)]
pub struct Quantised {
    scale: f32,
    values: Vec<i8>,
}

impl Quantised {
    /// Quantises `vector`: the largest magnitude maps to 127.
    pub fn from_f32(vector: &[f32]) -> Self {
        let max = vector.iter().fold(0.0_f32, |m, x| m.max(x.abs()));
        let scale = if max > 0.0 { max / 127.0 } else { 1.0 };
        let values = vector
            .iter()
            .map(|x| (x / scale).round().clamp(-127.0, 127.0) as i8)
            .collect();
        Self { scale, values }
    }

    pub fn to_f32(&self) -> Vec<f32> {
        self.values
            .iter()
            .map(|v| f32::from(*v) * self.scale)
            .collect()
    }

    /// The dot product with a query vector of the same width.
    pub fn dot(&self, query: &[f32]) -> f32 {
        self.values
            .iter()
            .zip(query)
            .map(|(v, q)| f32::from(*v) * q)
            .sum::<f32>()
            * self.scale
    }
}

/// Exclusive writer access; released when dropped.
#[derive(Debug)]
pub struct StoreLock {
    _file: File,
}

/// The store of one root.
#[derive(Debug)]
pub struct VectorStore {
    dir: PathBuf,
    pub manifest: Manifest,
}

impl VectorStore {
    /// Opens (and creates, mode 0700) the store under `dir`. A manifest that
    /// does not parse is an error: nothing is silently discarded.
    pub fn open(dir: &Path) -> Result<Self> {
        create_private(dir)?;
        let manifest = match fs::read(dir.join(MANIFEST)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| SemanticError(format!("corrupt vector manifest: {e}")))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Manifest::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            manifest,
        })
    }

    /// Takes the writer lock, waiting for another writer.
    pub fn lock(&self) -> Result<StoreLock> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("lock"))?;
        file.lock_exclusive()?;
        Ok(StoreLock { _file: file })
    }

    /// Whether the store may be read and extended by this model and cutting:
    /// an empty store takes any; a populated one refuses another model, another
    /// width, another revision or another chunker version.
    pub fn check_model(&self, model_id: &str, revision: u32, dim: usize) -> Result<()> {
        let m = &self.manifest;
        if m.segments.is_empty() && m.model_id.is_empty() {
            return Ok(());
        }
        if m.model_id != model_id || m.dim != dim {
            return Err(SemanticError(format!(
                "the vector store was built with {} ({}d) but the model is {model_id} ({dim}d)",
                m.model_id, m.dim
            )));
        }
        if m.revision != revision || m.chunker_version != super::chunk::CHUNKER_VERSION {
            return Err(SemanticError(format!(
                "the vector store is at revision {}/{} but the model is at {revision}/{}",
                m.revision,
                m.chunker_version,
                super::chunk::CHUNKER_VERSION
            )));
        }
        Ok(())
    }

    /// Empties the store and binds it to this model, for a rebuild.
    pub fn reset(&mut self, model_id: &str, revision: u32, dim: usize) -> Result<()> {
        for name in std::mem::take(&mut self.manifest.segments) {
            let _ = fs::remove_file(self.dir.join(name));
        }
        self.manifest = Manifest {
            model_id: model_id.to_owned(),
            revision,
            dim,
            chunker_version: super::chunk::CHUNKER_VERSION,
            segments: Vec::new(),
            rows: 0,
        };
        self.write_manifest()
    }

    /// Every stored vector by key. A key in two segments keeps the later one.
    pub fn load(&self) -> Result<HashMap<u128, Quantised>> {
        let mut all = HashMap::with_capacity(self.manifest.rows);
        for name in &self.manifest.segments {
            read_segment(&self.dir.join(name), self.manifest.dim, &mut all)?;
        }
        Ok(all)
    }

    /// Writes `rows` as one new segment. Requires the model to match.
    pub fn append(
        &mut self,
        model_id: &str,
        revision: u32,
        dim: usize,
        rows: &[(u128, Vec<f32>)],
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        if self.manifest.model_id.is_empty() {
            self.manifest.model_id = model_id.to_owned();
            self.manifest.revision = revision;
            self.manifest.dim = dim;
            self.manifest.chunker_version = super::chunk::CHUNKER_VERSION;
        }
        self.check_model(model_id, revision, dim)?;
        if rows.iter().any(|(_, v)| v.len() != dim) {
            return Err(SemanticError("a vector of the wrong width".into()));
        }
        let name = format!("seg-{:06}.vec", self.next_sequence());
        write_segment(&self.dir, &name, dim, rows)?;
        self.manifest.segments.push(name);
        self.manifest.rows += rows.len();
        self.write_manifest()
    }

    /// Rewrites the store keeping only `keep`, when unreachable rows exceed
    /// [`COMPACT_SHARE`]. True when it rewrote.
    pub fn compact(&mut self, keep: &HashSet<u128>) -> Result<bool> {
        let all = self.load()?;
        let unreachable = self
            .manifest
            .rows
            .saturating_sub(all.keys().filter(|k| keep.contains(k)).count());
        if unreachable * COMPACT_SHARE.1 <= self.manifest.rows * COMPACT_SHARE.0 {
            return Ok(false);
        }
        let dim = self.manifest.dim;
        let mut kept: Vec<(u128, Vec<f32>)> = all
            .into_iter()
            .filter(|(k, _)| keep.contains(k))
            .map(|(k, q)| (k, q.to_f32()))
            .collect();
        kept.sort_by_key(|(k, _)| *k);
        let old = std::mem::take(&mut self.manifest.segments);
        self.manifest.rows = 0;
        if !kept.is_empty() {
            let name = format!("seg-{:06}.vec", self.next_sequence_after(&old));
            write_segment(&self.dir, &name, dim, &kept)?;
            self.manifest.segments.push(name);
            self.manifest.rows = kept.len();
        }
        self.write_manifest()?;
        for name in old {
            let _ = fs::remove_file(self.dir.join(name));
        }
        Ok(true)
    }

    fn next_sequence(&self) -> u64 {
        self.next_sequence_after(&self.manifest.segments)
    }

    fn next_sequence_after(&self, names: &[String]) -> u64 {
        names
            .iter()
            .filter_map(|n| {
                n.strip_prefix("seg-")?
                    .strip_suffix(".vec")?
                    .parse::<u64>()
                    .ok()
            })
            .max()
            .unwrap_or(0)
            + 1
    }

    fn write_manifest(&self) -> Result<()> {
        let tmp = self.dir.join(format!("{MANIFEST}.tmp"));
        {
            let mut file = File::create(&tmp)?;
            file.write_all(&serde_json::to_vec_pretty(&self.manifest)?)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, self.dir.join(MANIFEST))?;
        Ok(())
    }
}

fn create_private(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if !dir.is_dir() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    Ok(())
}

fn write_segment(dir: &Path, name: &str, dim: usize, rows: &[(u128, Vec<f32>)]) -> Result<()> {
    let mut bytes = Vec::with_capacity(HEADER + rows.len() * (24 + dim));
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&(dim as u32).to_le_bytes());
    bytes.extend_from_slice(&(rows.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&0_u64.to_le_bytes());
    for (key, vector) in rows {
        let q = Quantised::from_f32(vector);
        bytes.extend_from_slice(&key.to_le_bytes());
        bytes.extend_from_slice(&q.scale.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend(q.values.iter().map(|v| *v as u8));
    }
    let tmp = dir.join(format!("{name}.tmp"));
    {
        let mut file = File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, dir.join(name))?;
    Ok(())
}

fn read_segment(path: &Path, dim: usize, into: &mut HashMap<u128, Quantised>) -> Result<()> {
    let bytes = fs::read(path).map_err(|e| SemanticError(format!("{}: {e}", path.display())))?;
    let bad = |why: &str| SemanticError(format!("{}: {why}", path.display()));
    if bytes.len() < HEADER || &bytes[..8] != MAGIC {
        return Err(bad("not a vector segment"));
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().expect("4 bytes"));
    let width = u32::from_le_bytes(bytes[12..16].try_into().expect("4 bytes")) as usize;
    let count = u64::from_le_bytes(bytes[16..24].try_into().expect("8 bytes")) as usize;
    if version != VERSION || width != dim {
        return Err(bad("segment version or width does not match the manifest"));
    }
    let row = 24 + dim;
    if bytes.len() != HEADER + count * row {
        return Err(bad("segment length does not match its row count"));
    }
    for chunk in bytes[HEADER..].chunks_exact(row) {
        let key = u128::from_le_bytes(chunk[..16].try_into().expect("16 bytes"));
        let scale = f32::from_le_bytes(chunk[16..20].try_into().expect("4 bytes"));
        let values = chunk[24..].iter().map(|b| *b as i8).collect();
        into.insert(key, Quantised { scale, values });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec_of(seed: f32, dim: usize) -> Vec<f32> {
        (0..dim).map(|i| ((i as f32 + 1.0) * seed).sin()).collect()
    }

    #[test]
    fn quantised_should_round_trip_within_one_step() {
        let v = vec_of(0.7, 32);
        let q = Quantised::from_f32(&v);
        let max = v.iter().fold(0.0_f32, |m, x| m.max(x.abs()));
        for (a, b) in v.iter().zip(q.to_f32()) {
            assert!((a - b).abs() <= max / 127.0, "{a} vs {b}");
        }
        assert!((q.dot(&v) - v.iter().map(|x| x * x).sum::<f32>()).abs() < 0.2);
        assert_eq!(Quantised::from_f32(&[0.0, 0.0]).to_f32(), vec![0.0, 0.0]);
    }

    #[test]
    fn append_then_reopen_should_return_every_row() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(STORE_DIR);
        let mut store = VectorStore::open(&dir).unwrap();
        store
            .append("m", 1, 16, &[(1, vec_of(0.1, 16)), (2, vec_of(0.2, 16))])
            .unwrap();
        store.append("m", 1, 16, &[(3, vec_of(0.3, 16))]).unwrap();
        let reopened = VectorStore::open(&dir).unwrap();
        assert_eq!(
            reopened.manifest.segments,
            ["seg-000001.vec", "seg-000002.vec"]
        );
        assert_eq!(reopened.manifest.rows, 3);
        let all = reopened.load().unwrap();
        let mut keys: Vec<u128> = all.keys().copied().collect();
        keys.sort_unstable();
        assert_eq!(keys, [1, 2, 3]);
        let expect = Quantised::from_f32(&vec_of(0.2, 16));
        assert_eq!(all[&2], expect);
    }

    #[test]
    fn check_model_should_refuse_another_model_width_or_revision() {
        let root = tempfile::tempdir().unwrap();
        let mut store = VectorStore::open(&root.path().join(STORE_DIR)).unwrap();
        store.check_model("anything", 9, 3).unwrap();
        store.append("m", 1, 8, &[(1, vec_of(0.1, 8))]).unwrap();
        store.check_model("m", 1, 8).unwrap();
        assert!(
            store
                .check_model("n", 1, 8)
                .unwrap_err()
                .0
                .contains("built with m")
        );
        assert!(store.check_model("m", 1, 9).is_err());
        assert!(
            store
                .check_model("m", 2, 8)
                .unwrap_err()
                .0
                .contains("revision")
        );
        assert!(store.append("n", 1, 8, &[(2, vec_of(0.2, 8))]).is_err());
        assert!(
            store.append("m", 1, 8, &[(2, vec_of(0.2, 4))]).is_err(),
            "wrong width"
        );
        assert_eq!(
            store.manifest.segments.len(),
            1,
            "a refused append writes nothing"
        );
    }

    #[test]
    fn compact_should_rewrite_only_past_a_quarter_unreachable() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(STORE_DIR);
        let mut store = VectorStore::open(&dir).unwrap();
        let rows: Vec<(u128, Vec<f32>)> = (1..=8).map(|k| (k, vec_of(k as f32, 8))).collect();
        store.append("m", 1, 8, &rows).unwrap();
        // Two of eight unreachable: exactly a quarter, not more.
        let keep: HashSet<u128> = (1..=6).collect();
        assert!(!store.compact(&keep).unwrap());
        assert_eq!(store.manifest.rows, 8);
        // Three of eight: past a quarter.
        let keep: HashSet<u128> = (1..=5).collect();
        assert!(store.compact(&keep).unwrap());
        assert_eq!(store.manifest.rows, 5);
        assert_eq!(store.manifest.segments, ["seg-000002.vec"]);
        assert!(!dir.join("seg-000001.vec").exists());
        let mut keys: Vec<u128> = VectorStore::open(&dir)
            .unwrap()
            .load()
            .unwrap()
            .into_keys()
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, [1, 2, 3, 4, 5]);
        // Nothing reachable: the store empties.
        assert!(store.compact(&HashSet::new()).unwrap());
        assert!(store.manifest.segments.is_empty());
    }

    #[test]
    fn load_should_refuse_a_truncated_segment_and_a_bad_manifest() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(STORE_DIR);
        let mut store = VectorStore::open(&dir).unwrap();
        store.append("m", 1, 8, &[(1, vec_of(0.1, 8))]).unwrap();
        let seg = dir.join("seg-000001.vec");
        let bytes = fs::read(&seg).unwrap();
        fs::write(&seg, &bytes[..bytes.len() - 1]).unwrap();
        assert!(store.load().unwrap_err().0.contains("row count"));
        fs::write(
            &seg,
            b"not a segment at all, but long enough to pass the header length check",
        )
        .unwrap();
        assert!(store.load().unwrap_err().0.contains("not a vector segment"));
        fs::write(dir.join(MANIFEST), b"{").unwrap();
        assert!(VectorStore::open(&dir).unwrap_err().0.contains("corrupt"));
    }

    #[test]
    fn reset_should_drop_the_segments_and_rebind_the_model() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(STORE_DIR);
        let mut store = VectorStore::open(&dir).unwrap();
        store.append("m", 1, 8, &[(1, vec_of(0.1, 8))]).unwrap();
        store.reset("n", 4, 12).unwrap();
        assert!(store.manifest.segments.is_empty());
        assert_eq!(
            (
                store.manifest.model_id.as_str(),
                store.manifest.revision,
                store.manifest.dim
            ),
            ("n", 4, 12)
        );
        assert!(!dir.join("seg-000001.vec").exists());
        store.check_model("n", 4, 12).unwrap();
    }

    #[test]
    fn the_store_directory_should_be_private_and_the_lock_exclusive_to_writers() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(STORE_DIR);
        let store = VectorStore::open(&dir).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let _held = store.lock().unwrap();
        let other = OpenOptions::new()
            .write(true)
            .open(dir.join("lock"))
            .unwrap();
        assert!(
            other.try_lock_exclusive().is_err(),
            "a second writer must wait"
        );
    }
}
