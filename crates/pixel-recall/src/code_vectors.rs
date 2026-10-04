//! Content-addressed store of the chunk vectors `pixel search-meaning` embeds.
//!
//! Lives under `<root>/.pixel/code-vectors/` of an indexed repository (the
//! caller decides when: [`crate::code_search::vector_cache_for`]) so a warm
//! question pays the model only for the chunks whose text changed. A vector is
//! found by the xxh3-128 hash of its chunk text, seeded by a [`Namespace`]
//! naming the model, its embedder revision and the chunker version, and it is
//! stored as the embedder returned it: `f32`, never quantized, so a cached
//! question ranks bit for bit like an uncached one.
//!
//! Layout:
//!
//! ```text
//! lock                     flock(2): shared while reading, exclusive around every read-modify-write
//! manifest.json            the one list of live segments, replaced by tmp + fsync + rename
//! seg-<xxh3-128 hex>.vec   immutable segments, named by the hash of their bytes
//! ```
//!
//! Segment (little-endian): `b"PXCVEC01"` | dim `u32` | rows `u64` |
//! rows × (key `u128` | dim × `f32`).
//!
//! A reader never sees a partial write: segments and the manifest appear by
//! rename only, and compaction deletes a segment under the exclusive lock that
//! readers' shared lock excludes. A segment the manifest names but that is
//! missing, truncated or altered is an error the caller reports, never a
//! silent skip; the next write rebuilds the store from the vectors in hand.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Directory of the store under `.pixel/`.
pub const DIR: &str = "code-vectors";
const MANIFEST: &str = "manifest.json";
const LOCK: &str = "lock";
const MAGIC: &[u8; 8] = b"PXCVEC01";
/// Magic (8) + dim (4) + rows (8).
const HEADER_LEN: usize = 20;
const MANIFEST_VERSION: u32 = 1;

/// Key of one chunk's vector.
pub(crate) type ChunkKey = u128;

/// What a stored vector depends on besides its chunk text: the model, the
/// revision of the library that runs it, and the chunker that cut the text.
/// A change of any of them changes every key and every segment's namespace,
/// so no vector of another namespace is ever served.
pub(crate) struct Namespace {
    name: String,
    seed: u64,
}

impl Namespace {
    pub(crate) fn new(model_id: &str, revision: u32, chunker: u32) -> Self {
        let name = format!("{model_id}@r{revision}+c{chunker}");
        let seed = xxhash_rust::xxh3::xxh3_64(name.as_bytes());
        Self { name, seed }
    }

    /// The key of `text`'s vector: xxh3-128 of its bytes, seeded by the
    /// namespace.
    pub(crate) fn key(&self, text: &str) -> ChunkKey {
        xxhash_rust::xxh3::xxh3_128_with_seed(text.as_bytes(), self.seed)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SegmentEntry {
    file: String,
    namespace: String,
    dim: usize,
    rows: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    segments: Vec<SegmentEntry>,
}

impl Manifest {
    fn new(segments: Vec<SegmentEntry>) -> Self {
        Self {
            version: MANIFEST_VERSION,
            segments,
        }
    }

    /// Rows every segment holds, whatever its namespace.
    fn rows(&self) -> usize {
        self.segments.iter().map(|segment| segment.rows).sum()
    }
}

/// What [`Store::load`] found for one question.
#[derive(Debug, Default)]
pub(crate) struct Loaded {
    /// The wanted keys the store holds, with their vectors.
    pub vectors: HashMap<ChunkKey, Vec<f32>>,
    /// Rows the manifest names across every namespace: live, unreachable and
    /// duplicate alike.
    pub stored_rows: usize,
    /// Why part of the store could not be read; empty when all of it was.
    pub errors: Vec<String>,
}

/// Handle on one store directory; cheap, holds no open file.
#[derive(Debug, Clone)]
pub(crate) struct Store {
    dir: PathBuf,
}

/// Whether the store holds enough unreachable rows to be rewritten with the
/// live ones only: more than a quarter of the live rows (unreachable above a
/// fifth of the file). Below that, an edit only appends its changed chunks.
fn needs_compaction(stored_rows: usize, live_rows: usize) -> bool {
    stored_rows.saturating_sub(live_rows) > live_rows / 4
}

impl Store {
    pub(crate) fn at(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    /// The vectors of `wanted` stored under `namespace` at `dim`, read under
    /// the shared lock. A store never written is empty without an error;
    /// every unreadable part of one is named in `errors`, and the vectors of
    /// the healthy segments are still returned.
    pub(crate) fn load(
        &self,
        namespace: &Namespace,
        dim: usize,
        wanted: &HashSet<ChunkKey>,
    ) -> Loaded {
        let mut loaded = Loaded::default();
        if !self.dir.join(MANIFEST).exists() {
            return loaded;
        }
        let _lock = match self.lock(false) {
            Ok(lock) => lock,
            Err(error) => {
                loaded.errors.push(error);
                return loaded;
            }
        };
        let manifest = match self.read_manifest() {
            Ok(Some(manifest)) => manifest,
            Ok(None) => return loaded,
            Err(error) => {
                loaded.errors.push(error);
                return loaded;
            }
        };
        loaded.stored_rows = manifest.rows();
        for entry in &manifest.segments {
            if entry.namespace != namespace.name || entry.dim != dim {
                continue;
            }
            if let Err(error) = read_segment(&self.dir, entry, wanted, &mut loaded.vectors) {
                loaded.errors.push(error);
            }
        }
        loaded
    }

    /// Record a question's vectors: `fresh` (just embedded) is appended as
    /// one segment, or the whole store is rewritten with `live` (every
    /// vector of the question's universe) when `rebuild` asks for it (its
    /// load failed) or unreachable rows passed [`needs_compaction`], counted
    /// from the `stored_rows` the load saw. Nothing is written when there is
    /// nothing new, nothing to repair and nothing to collect. Returns whether
    /// the store was written.
    ///
    /// # Errors
    ///
    /// Any I/O failure; the store is left as it was, since the manifest is
    /// replaced last.
    pub(crate) fn save(
        &self,
        namespace: &Namespace,
        dim: usize,
        fresh: &[ChunkKey],
        live: &HashMap<ChunkKey, Vec<f32>>,
        stored_rows: usize,
        rebuild: bool,
    ) -> Result<bool, String> {
        if fresh.is_empty() && !rebuild && !needs_compaction(stored_rows, live.len()) {
            return Ok(false);
        }
        self.commit_with(namespace, dim, fresh, live, rebuild, &mut || {})?;
        Ok(true)
    }

    /// [`Store::save`]'s write, under the exclusive lock, with `between`
    /// called after the manifest was read and before it is replaced (the
    /// window a lost update would need).
    fn commit_with(
        &self,
        namespace: &Namespace,
        dim: usize,
        fresh: &[ChunkKey],
        live: &HashMap<ChunkKey, Vec<f32>>,
        rebuild: bool,
        between: &mut dyn FnMut(),
    ) -> Result<(), String> {
        // Owner-only, and never created through a link committed under
        // `.pixel/`.
        pixel_git::sidecar::private_dir(&self.dir)
            .map_err(|e| format!("code-vector store {}: {e}", self.dir.display()))?;
        let _lock = self.lock(true)?;
        // An unreadable manifest is rebuilt from the vectors in hand, like
        // a store whose load failed.
        let current = if rebuild {
            None
        } else {
            self.read_manifest()
                .ok()
                .map(|manifest| manifest.unwrap_or_else(|| Manifest::new(Vec::new())))
        };
        between();
        let next = match current {
            Some(mut manifest) if !needs_compaction(manifest.rows() + fresh.len(), live.len()) => {
                if let Some(entry) = self.write_segment(namespace, dim, fresh, live)?
                    && !manifest.segments.contains(&entry)
                {
                    manifest.segments.push(entry);
                }
                manifest
            }
            _ => {
                let mut keys: Vec<ChunkKey> = live.keys().copied().collect();
                keys.sort_unstable();
                Manifest::new(
                    self.write_segment(namespace, dim, &keys, live)?
                        .into_iter()
                        .collect(),
                )
            }
        };
        let bytes = serde_json::to_vec(&next).expect("a manifest always serializes");
        write_atomically(&self.dir, MANIFEST, &bytes)?;
        self.remove_unlisted(&next);
        Ok(())
    }

    /// The lock file, locked shared or `exclusive`; released when dropped.
    fn lock(&self, exclusive: bool) -> Result<File, String> {
        let path = self.dir.join(LOCK);
        let file = pixel_git::nofollow::open_lock(&path)
            .map_err(|e| format!("code-vector lock {}: {e}", path.display()))?;
        if exclusive {
            file.lock()
        } else {
            file.lock_shared()
        }
        .map_err(|e| format!("code-vector lock {}: {e}", path.display()))?;
        Ok(file)
    }

    /// The manifest, `None` when the store has none yet.
    fn read_manifest(&self) -> Result<Option<Manifest>, String> {
        let path = self.dir.join(MANIFEST);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("code-vector manifest {}: {error}", path.display())),
        };
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|e| format!("code-vector manifest {} is corrupt: {e}", path.display()))?;
        if manifest.version != MANIFEST_VERSION {
            return Err(format!(
                "code-vector manifest {} has version {}, this pixel reads {MANIFEST_VERSION}",
                path.display(),
                manifest.version
            ));
        }
        Ok(Some(manifest))
    }

    /// Write `keys`' vectors as one immutable segment; `None` for no key.
    fn write_segment(
        &self,
        namespace: &Namespace,
        dim: usize,
        keys: &[ChunkKey],
        vectors: &HashMap<ChunkKey, Vec<f32>>,
    ) -> Result<Option<SegmentEntry>, String> {
        if keys.is_empty() {
            return Ok(None);
        }
        let mut bytes = Vec::with_capacity(HEADER_LEN + keys.len() * row_len(dim));
        bytes.extend_from_slice(&segment_header(dim, keys.len()));
        for key in keys {
            let vector = &vectors[key];
            assert_eq!(vector.len(), dim, "a stored vector has the store's dim");
            bytes.extend_from_slice(&key.to_le_bytes());
            for value in vector {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        let file = segment_file_name(&bytes);
        write_atomically(&self.dir, &file, &bytes)?;
        Ok(Some(SegmentEntry {
            file,
            namespace: namespace.name.clone(),
            dim,
            rows: keys.len(),
        }))
    }

    /// Delete the segments `manifest` does not name and the temporary files
    /// of an interrupted write. Called under the exclusive lock, which every
    /// writer holds while it creates files, so nothing in flight is removed.
    /// A file that cannot be removed only costs disk until the next write.
    fn remove_unlisted(&self, manifest: &Manifest) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let listed: HashSet<&str> = manifest
            .segments
            .iter()
            .map(|segment| segment.file.as_str())
            .collect();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let segment = name
                .strip_prefix("seg-")
                .and_then(|rest| rest.strip_suffix(".vec"))
                .is_some();
            if (segment && !listed.contains(name.as_ref())) || name.ends_with(".tmp") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Every key of every segment file present, listed or not.
    #[cfg(test)]
    pub(crate) fn keys_on_disk(&self) -> Vec<ChunkKey> {
        let mut keys = Vec::new();
        for entry in fs::read_dir(&self.dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("seg-") {
                continue;
            }
            let bytes = fs::read(entry.path()).unwrap();
            let dim = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
            for row in bytes[HEADER_LEN..].chunks_exact(row_len(dim)) {
                keys.push(u128::from_le_bytes(row[..16].try_into().unwrap()));
            }
        }
        keys.sort_unstable();
        keys
    }
}

fn row_len(dim: usize) -> usize {
    16 + 4 * dim
}

fn segment_header(dim: usize, rows: usize) -> [u8; HEADER_LEN] {
    let mut header = [0u8; HEADER_LEN];
    header[..8].copy_from_slice(MAGIC);
    header[8..12].copy_from_slice(&u32::try_from(dim).unwrap_or(u32::MAX).to_le_bytes());
    header[12..].copy_from_slice(&(rows as u64).to_le_bytes());
    header
}

fn segment_file_name(bytes: &[u8]) -> String {
    format!("seg-{:032x}.vec", xxhash_rust::xxh3::xxh3_128(bytes))
}

/// Add the vectors of `wanted` keys held by the segment `entry` names to
/// `out`, after checking the file is exactly the one the manifest names: its
/// header announces the entry's dim and rows, and its bytes hash to its name
/// (so a truncated or altered file is refused, and the length follows).
fn read_segment(
    dir: &Path,
    entry: &SegmentEntry,
    wanted: &HashSet<ChunkKey>,
    out: &mut HashMap<ChunkKey, Vec<f32>>,
) -> Result<(), String> {
    let bytes = fs::read(dir.join(&entry.file)).map_err(|e| {
        format!(
            "code-vector segment {} named by the manifest is unreadable: {e}",
            entry.file
        )
    })?;
    if !bytes.starts_with(&segment_header(entry.dim, entry.rows))
        || segment_file_name(&bytes) != entry.file
    {
        return Err(format!(
            "code-vector segment {} does not match the manifest (truncated or altered)",
            entry.file
        ));
    }
    for row in bytes[HEADER_LEN..].chunks_exact(row_len(entry.dim)) {
        let key = u128::from_le_bytes(row[..16].try_into().expect("a row opens on a 16-byte key"));
        if wanted.contains(&key) {
            let vector = row[16..]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|value| f32::from_le_bytes(*value))
                .collect();
            out.insert(key, vector);
        }
    }
    Ok(())
}

/// Write `bytes` to `dir/name` so no reader ever sees a partial file: a
/// temporary file in the same directory, fsynced, then renamed over the name.
fn write_atomically(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let target = dir.join(name);
    // A leftover from a crashed run, or a link committed at the name, is
    // removed, then the name is created fresh: never written through.
    let _ = fs::remove_file(&tmp);
    let written = pixel_git::nofollow::create_new(&tmp, pixel_git::nofollow::PRIVATE_MODE)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        });
    if let Err(error) = written.and_then(|()| fs::rename(&tmp, &target)) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("code-vector store {}: {error}", target.display()));
    }
    // The rename is durable once the directory is; best effort where the
    // platform refuses to fsync a directory.
    if let Ok(directory) = File::open(dir) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn namespace() -> Namespace {
        Namespace::new("model", 1, 1)
    }

    fn live(keys: &[ChunkKey]) -> HashMap<ChunkKey, Vec<f32>> {
        keys.iter()
            .map(|key| (*key, vec![*key as f32, 0.5]))
            .collect()
    }

    fn all(keys: &[ChunkKey]) -> HashSet<ChunkKey> {
        keys.iter().copied().collect()
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(&dir.path().join(DIR));
        (dir, store)
    }

    fn manifest(store: &Store) -> Manifest {
        store.read_manifest().unwrap().unwrap()
    }

    /// The compaction boundary: unreachable rows exactly at a quarter of
    /// the live rows are kept, one more triggers the rewrite.
    #[test]
    fn needs_compaction_should_start_strictly_above_a_quarter_of_the_live_rows() {
        assert!(!needs_compaction(4, 4), "nothing unreachable");
        assert!(!needs_compaction(5, 4), "exactly a quarter: kept");
        assert!(needs_compaction(6, 4), "above a quarter: rewritten");
        assert!(!needs_compaction(100, 80), "20 of 80 is a quarter");
        assert!(needs_compaction(101, 80));
        assert!(!needs_compaction(3, 4), "fewer stored than live");
    }

    /// Model, revision and chunker each move every key and the namespace a
    /// segment is filed under: a vector of the old one is never found.
    #[test]
    fn namespace_should_change_every_key_when_model_revision_or_chunker_changes() {
        let base = Namespace::new("model", 1, 1);
        for other in [
            Namespace::new("other", 1, 1),
            Namespace::new("model", 2, 1),
            Namespace::new("model", 1, 2),
        ] {
            assert_ne!(other.name, base.name);
            assert_ne!(other.key("fn a() {}"), base.key("fn a() {}"));
        }
        assert_ne!(base.key("fn a() {}"), base.key("fn b() {}"));
        assert_eq!(base.key("fn a() {}"), namespace().key("fn a() {}"));
    }

    #[test]
    fn save_then_load_should_return_the_exact_vectors_of_the_wanted_keys_only() {
        let (_dir, store) = store();
        let loaded = store.load(&namespace(), 2, &all(&[1]));
        assert!(loaded.vectors.is_empty() && loaded.errors.is_empty());
        assert!(!store.dir.exists(), "a load writes nothing");
        let vectors = HashMap::from([(1, vec![0.1f32, -2.5e-8]), (2, vec![3.0, 4.0])]);
        assert!(
            store
                .save(&namespace(), 2, &[1, 2], &vectors, 0, false)
                .unwrap()
        );
        let loaded = store.load(&namespace(), 2, &all(&[1, 7]));
        assert_eq!(loaded.vectors, HashMap::from([(1, vec![0.1f32, -2.5e-8])]));
        assert_eq!(loaded.stored_rows, 2);
        assert!(loaded.errors.is_empty());
        // Another namespace or dimension sees none of it.
        for (other, dim) in [(Namespace::new("model", 2, 1), 2), (namespace(), 3)] {
            let loaded = store.load(&other, dim, &all(&[1, 2]));
            assert!(loaded.vectors.is_empty(), "{}", other.name);
            assert_eq!(loaded.stored_rows, 2);
        }
    }

    #[test]
    fn save_should_write_nothing_when_nothing_is_new_nor_unreachable() {
        let (_dir, store) = store();
        let vectors = live(&[1, 2, 3, 4]);
        assert!(
            store
                .save(&namespace(), 2, &[1, 2, 3, 4], &vectors, 0, false)
                .unwrap()
        );
        let before = fs::read(store.dir.join(MANIFEST)).unwrap();
        let loaded = store.load(&namespace(), 2, &all(&[1, 2, 3, 4]));
        assert!(
            !store
                .save(&namespace(), 2, &[], &vectors, loaded.stored_rows, false)
                .unwrap()
        );
        // One unreachable row of four live: at the threshold, nothing written.
        assert!(
            !store
                .save(&namespace(), 2, &[], &vectors, 5, false)
                .unwrap()
        );
        assert_eq!(fs::read(store.dir.join(MANIFEST)).unwrap(), before);
        // Two: rewritten with the live rows.
        assert!(
            store
                .save(&namespace(), 2, &[], &vectors, 6, false)
                .unwrap()
        );
        // A failed load asks for a rebuild even with nothing new.
        assert!(store.save(&namespace(), 2, &[], &vectors, 4, true).unwrap());
    }

    /// Appending keeps earlier segments; passing the threshold rewrites the
    /// store with the live rows only and deletes the old segment files.
    #[test]
    fn commit_should_append_until_unreachable_rows_pass_the_threshold_then_compact() {
        let (_dir, store) = store();
        let ns = namespace();
        store
            .commit_with(
                &ns,
                2,
                &[1, 2, 3, 4],
                &live(&[1, 2, 3, 4]),
                false,
                &mut || {},
            )
            .unwrap();
        // 4 stored + 1 fresh against 4 live: one unreachable, appended.
        store
            .commit_with(&ns, 2, &[5], &live(&[2, 3, 4, 5]), false, &mut || {})
            .unwrap();
        assert_eq!(manifest(&store).segments.len(), 2);
        assert_eq!(store.keys_on_disk(), [1, 2, 3, 4, 5]);
        // 5 stored + 1 fresh against 4 live: two unreachable, compacted.
        store
            .commit_with(&ns, 2, &[6], &live(&[3, 4, 5, 6]), false, &mut || {})
            .unwrap();
        let compacted = manifest(&store);
        assert_eq!(compacted.segments.len(), 1);
        assert_eq!(compacted.rows(), 4);
        assert_eq!(store.keys_on_disk(), [3, 4, 5, 6], "old segments deleted");
        let loaded = store.load(&ns, 2, &all(&[3, 4, 5, 6]));
        assert_eq!(loaded.vectors, live(&[3, 4, 5, 6]));
    }

    /// Two writers racing on the same store: the second one runs its whole
    /// write while the first sits between reading and replacing the
    /// manifest. The lock makes it wait, so the store ends with both sets;
    /// without the lock the first writer's stale manifest drops the second
    /// writer's segment.
    #[test]
    fn concurrent_writers_should_leave_a_store_holding_both_sets() {
        let (_dir, store) = store();
        let store = &store;
        let union = &live(&[1, 2, 3, 11, 12, 13]);
        let (read_tx, read_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        std::thread::scope(|scope| {
            let first = scope.spawn(move || {
                store.commit_with(&namespace(), 2, &[1, 2, 3], union, false, &mut || {
                    read_tx.send(()).unwrap();
                    // Long enough for an unlocked second writer to finish.
                    let _ = done_rx.recv_timeout(std::time::Duration::from_secs(1));
                })
            });
            read_rx.recv().unwrap();
            store
                .commit_with(&namespace(), 2, &[11, 12, 13], union, false, &mut || {})
                .unwrap();
            let _ = done_tx.send(());
            first.join().unwrap().unwrap();
        });
        let loaded = store.load(&namespace(), 2, &all(&[1, 2, 3, 11, 12, 13]));
        assert_eq!(&loaded.vectors, union);
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert_eq!(manifest(store).rows(), 6);
    }

    /// A segment the manifest names is missing, truncated, altered, or
    /// described by a wrong entry: each is an error naming the file, the
    /// healthy segments still answer, and the next save rebuilds the store.
    #[test]
    fn damaged_segments_should_be_errors_and_rebuilt_never_skipped() {
        /// Damages the segment `file` and returns the name the error must carry.
        type Damage = fn(&Store, &str) -> String;
        let damages: [(&str, Damage); 4] = [
            ("missing", |store, file| {
                fs::remove_file(store.dir.join(file)).unwrap();
                file.to_string()
            }),
            ("truncated", |store, file| {
                let path = store.dir.join(file);
                let bytes = fs::read(&path).unwrap();
                fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
                file.to_string()
            }),
            ("altered", |store, file| {
                let path = store.dir.join(file);
                let mut bytes = fs::read(&path).unwrap();
                *bytes.last_mut().unwrap() ^= 1;
                fs::write(&path, bytes).unwrap();
                file.to_string()
            }),
            ("wrong entry", |store, file| {
                // An intact file of another shape and the same byte length:
                // 1 row × (16 + 4·20) = 2 rows × (16 + 4·8). Only the
                // header tells it from the two rows the entry announces.
                let other = HashMap::from([(4, vec![4.0; 20])]);
                let written = store
                    .write_segment(&namespace(), 20, &[4], &other)
                    .unwrap()
                    .unwrap();
                let mut manifest = manifest(store);
                let entry = manifest
                    .segments
                    .iter_mut()
                    .find(|entry| entry.file == file)
                    .unwrap();
                assert_eq!((entry.rows, entry.dim), (2, 8));
                entry.file.clone_from(&written.file);
                let bytes = serde_json::to_vec(&manifest).unwrap();
                write_atomically(&store.dir, MANIFEST, &bytes).unwrap();
                written.file
            }),
        ];
        for (label, damage) in damages {
            let (_dir, store) = store();
            let ns = namespace();
            let vectors: HashMap<ChunkKey, Vec<f32>> =
                [1, 2, 3].map(|key| (key, vec![key as f32; 8])).into();
            store
                .commit_with(&ns, 8, &[1], &vectors, false, &mut || {})
                .unwrap();
            store
                .commit_with(&ns, 8, &[2, 3], &vectors, false, &mut || {})
                .unwrap();
            let listed = manifest(&store).segments[1].file.clone();
            let damaged = damage(&store, &listed);
            let loaded = store.load(&ns, 8, &all(&[1, 2, 3]));
            assert_eq!(loaded.errors.len(), 1, "{label}: {:?}", loaded.errors);
            assert!(
                loaded.errors[0].contains(&damaged),
                "{label}: {:?}",
                loaded.errors
            );
            assert_eq!(
                loaded.vectors.keys().copied().collect::<Vec<_>>(),
                [1],
                "{label}: the healthy segment still answers"
            );
            let rebuild = !loaded.errors.is_empty();
            assert!(
                store
                    .save(&ns, 8, &[], &vectors, loaded.stored_rows, rebuild)
                    .unwrap(),
                "{label}"
            );
            let repaired = store.load(&ns, 8, &all(&[1, 2, 3]));
            assert!(repaired.errors.is_empty(), "{label}: {:?}", repaired.errors);
            assert_eq!(repaired.vectors, vectors, "{label}");
            assert_eq!(store.keys_on_disk(), [1, 2, 3], "{label}");
        }
    }

    /// A manifest that exists but cannot be read is an error, not an empty
    /// store.
    #[test]
    fn manifest_that_cannot_be_read_should_be_an_error_not_an_empty_store() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir.join(MANIFEST)).unwrap();
        let loaded = store.load(&namespace(), 2, &all(&[1]));
        assert_eq!(loaded.errors.len(), 1, "{:?}", loaded.errors);
        assert!(loaded.errors[0].contains("manifest"), "{:?}", loaded.errors);
    }

    #[test]
    fn unreadable_manifest_should_be_an_error_and_rebuilt() {
        for body in ["{not json", r#"{"version":99,"segments":[]}"#] {
            let (_dir, store) = store();
            store
                .commit_with(&namespace(), 2, &[1], &live(&[1]), false, &mut || {})
                .unwrap();
            fs::write(store.dir.join(MANIFEST), body).unwrap();
            let loaded = store.load(&namespace(), 2, &all(&[1]));
            assert_eq!(loaded.errors.len(), 1, "{body}");
            assert!(loaded.errors[0].contains("manifest"), "{:?}", loaded.errors);
            assert!(loaded.vectors.is_empty());
            // A writer that finds it unreadable rebuilds it too.
            store
                .commit_with(&namespace(), 2, &[2], &live(&[1, 2]), false, &mut || {})
                .unwrap();
            let loaded = store.load(&namespace(), 2, &all(&[1, 2]));
            assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
            assert_eq!(loaded.vectors, live(&[1, 2]));
        }
    }

    /// Files a crashed writer left behind (a temporary file, a segment it
    /// renamed but never listed) are invisible to readers and removed by the
    /// next write.
    #[test]
    fn leftovers_of_an_interrupted_write_should_be_ignored_then_removed() {
        let (_dir, store) = store();
        store
            .commit_with(&namespace(), 2, &[1], &live(&[1, 2]), false, &mut || {})
            .unwrap();
        fs::write(store.dir.join(".manifest.json.1.tmp"), "{partial").unwrap();
        let mut orphan = segment_header(2, 1).to_vec();
        orphan.extend_from_slice(&9u128.to_le_bytes());
        orphan.extend_from_slice(&[0; 8]);
        fs::write(store.dir.join(segment_file_name(&orphan)), &orphan).unwrap();
        let loaded = store.load(&namespace(), 2, &all(&[1, 9]));
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert_eq!(loaded.vectors, live(&[1]));
        assert_eq!(store.keys_on_disk(), [1, 9]);
        store
            .commit_with(&namespace(), 2, &[2], &live(&[1, 2]), false, &mut || {})
            .unwrap();
        assert_eq!(store.keys_on_disk(), [1, 2]);
        let names: Vec<String> = fs::read_dir(&store.dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(names.is_empty(), "{names:?}");
    }
}
