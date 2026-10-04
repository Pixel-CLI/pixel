// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Git-anchored 3-layer index: base shard + delta shard + dirty overlay.
//!
//! Layering (freshest wins, by path):
//! 1. **base.shard** — all tracked files at a pinned commit OID.
//! 2. **delta.shard** — files changed between base OID and current HEAD;
//!    modified/deleted base paths are tombstoned via `state.json`.
//! 3. **overlay** — in-memory gram sets for working-tree-dirty files (from
//!    `git status --porcelain`, updated live by the watcher); overlay paths
//!    tombstone their base/delta entries.
//!
//! Outside a git repo the set degrades to a plain single-shard index.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pixel_git::BatchObject;
use rayon::prelude::*;

use crate::cache;
use crate::delta::{DeltaState, delta_shard_path};
use crate::gram::GramExtractor;
use crate::index::{MAX_FILE_BYTES, SHARD_DIR, SHARD_FILE, SearchStats, read_regular_bounded};
use crate::lock::{BuildLock, is_pixel_only_gitignore, is_pixel_only_gitignore_text};
use crate::overlay::Overlay;
use crate::plan::plan_pattern;
use crate::posting::{GramQuery, resolve_query};
use crate::shard::{Shard, ShardBuilder, ShardError};
use crate::verify::{MatchLine, Verifier, VerifyError};
use crate::{gitsync, index};

#[derive(Debug)]
pub enum IndexSetError {
    Shard(ShardError),
    Verify(VerifyError),
    Pattern(String),
    Io(std::io::Error),
    Index(index::IndexError),
}

impl From<ShardError> for IndexSetError {
    fn from(e: ShardError) -> Self {
        IndexSetError::Shard(e)
    }
}
impl From<VerifyError> for IndexSetError {
    fn from(e: VerifyError) -> Self {
        IndexSetError::Verify(e)
    }
}
impl From<std::io::Error> for IndexSetError {
    fn from(e: std::io::Error) -> Self {
        IndexSetError::Io(e)
    }
}
impl From<index::IndexError> for IndexSetError {
    fn from(e: index::IndexError) -> Self {
        IndexSetError::Index(e)
    }
}

impl std::fmt::Display for IndexSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexSetError::Shard(e) => write!(f, "{e}"),
            IndexSetError::Verify(e) => write!(f, "{e}"),
            IndexSetError::Pattern(e) => write!(f, "bad pattern: {e}"),
            IndexSetError::Io(e) => write!(f, "io error: {e}"),
            IndexSetError::Index(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for IndexSetError {}

#[derive(Debug, Clone)]
pub struct FreshnessStatus {
    pub commit_oid: Option<String>,
    pub base_files: u32,
    pub delta_files: u32,
    pub overlay_files: usize,
    pub tombstones: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    Indexed,
    Excluded,
}

pub struct IndexSet {
    root: PathBuf,
    extractor: Box<dyn GramExtractor>,
    base: Shard,
    delta: Option<Shard>,
    /// Paths superseded between base OID and HEAD (from state.json).
    delta_tombstones: HashSet<String>,
    overlay: Overlay,
    open_timings: OpenTimings,
    /// HEAD when the set was opened: the commit base ∪ delta covers. The
    /// overlay follows the working tree from there, HEAD moves included.
    head: Option<String>,
}

/// Whole milliseconds in `elapsed`, saturating rather than wrapping on a
/// duration no build reaches. The one spelling of a phase timing across the
/// index and the graph.
pub fn millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// Where the base layer of an open came from, cheapest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseSource {
    /// `.pixel/base.shard` was already valid.
    Reused,
    /// Linked in from the shard cache another worktree filled.
    SharedCache,
    /// Extracted from the blobs of HEAD's tree.
    BuiltFromGit,
    /// Extracted from a walk of a directory that is not a git repository.
    BuiltFromWalk,
}

impl BaseSource {
    pub fn as_str(self) -> &'static str {
        match self {
            BaseSource::Reused => "reused",
            BaseSource::SharedCache => "shared_cache",
            BaseSource::BuiltFromGit => "built_from_git",
            BaseSource::BuiltFromWalk => "built_from_walk",
        }
    }
}

/// How the delta layer (base commit to HEAD) of an open was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaSource {
    /// HEAD is the base commit, or the directory has no git: no delta.
    None,
    /// `state.json` already pinned a delta to this HEAD.
    Reused,
    /// Re-extracted from `git diff base..HEAD`.
    Rebuilt,
}

impl DeltaSource {
    pub fn as_str(self) -> &'static str {
        match self {
            DeltaSource::None => "none",
            DeltaSource::Reused => "reused",
            DeltaSource::Rebuilt => "rebuilt",
        }
    }
}

/// What opening the index cost, layer by layer, so a slow open names the
/// layer that was rebuilt instead of one opaque total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenTimings {
    pub base: BaseSource,
    pub base_ms: u64,
    pub delta: DeltaSource,
    pub delta_ms: u64,
    /// `git status` plus the re-extraction of every dirty file.
    pub overlay_ms: u64,
    pub overlay_files: usize,
}

/// Paths inside our own sidecar dir are never indexed or tombstoned.
fn is_internal(rel: &str) -> bool {
    rel == SHARD_DIR || rel.starts_with(&format!("{SHARD_DIR}/"))
}

fn is_ignore_control_path(rel: &str) -> bool {
    rel == ".git/info/exclude" || matches!(rel.rsplit('/').next(), Some(".gitignore" | ".ignore"))
}

/// Sidecar path for the plain-walk freshness signature.
fn plain_sig_path(gpx_dir: &Path) -> PathBuf {
    gpx_dir.join("base.sig")
}

/// Content signature for a non-Git directory. The same ignore, binary, size,
/// and symlink policy as the index builder keeps freshness tied to the bytes
/// that can actually appear in search results.
fn plain_signature(root: &Path) -> String {
    use std::hash::Hasher;
    // Must mirror `index::build`'s walk policy exactly — both go through
    // `policy_walk` — or freshness would disagree with what the shard
    // actually contains.
    let mut entries: Vec<(String, u64)> = crate::index::policy_walk(root)
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let rel = path.strip_prefix(root).ok()?.to_string_lossy().into_owned();
            if is_internal(&rel) {
                return None;
            }
            let meta = std::fs::symlink_metadata(path).ok()?;
            if !meta.file_type().is_file() || meta.len() > MAX_FILE_BYTES {
                return None;
            }
            let content = std::fs::read(path).ok()?;
            if content[..content.len().min(8192)].contains(&0) {
                return None;
            }
            Some((rel, xxhash_rust::xxh3::xxh3_64(&content)))
        })
        .collect();
    entries.sort();
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    for (rel, content_hash) in &entries {
        h.write(rel.as_bytes());
        h.write(&content_hash.to_le_bytes());
    }
    format!("{:016x}", h.finish())
}

/// Load the stored plain-walk signature, or `None` if absent/corrupt.
fn load_plain_sig(gpx_dir: &Path) -> Option<String> {
    let bytes = read_regular_bounded(&plain_sig_path(gpx_dir), 128).ok()?;
    let sig = String::from_utf8(bytes).ok()?;
    (!sig.is_empty()).then_some(sig)
}

fn save_plain_sig(gpx_dir: &Path, sig: &str) {
    if pixel_git::sidecar::private_dir(gpx_dir).is_ok() {
        let _ = pixel_git::nofollow::write_replace(
            &plain_sig_path(gpx_dir),
            sig.as_bytes(),
            pixel_git::nofollow::PRIVATE_MODE,
        );
    }
}

/// Which candidate paths a search may read: plain relative paths whose
/// directory, links resolved, lies inside the root. Shard paths are checked
/// when a shard opens, but a directory link committed beside them could
/// still lead a read outside; one resolution per directory, memoised for
/// the search.
struct Containment {
    root: PathBuf,
    canonical_root: Option<PathBuf>,
    dirs: std::collections::HashMap<PathBuf, bool>,
}

impl Containment {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            canonical_root: root.canonicalize().ok(),
            dirs: std::collections::HashMap::new(),
        }
    }

    fn admits(&mut self, rel: &str) -> bool {
        if !pixel_git::repo_path::is_normal_relative(rel) {
            return false;
        }
        let Some(parent) = Path::new(rel).parent() else {
            return false;
        };
        let (root, canonical_root) = (&self.root, &self.canonical_root);
        *self.dirs.entry(parent.to_path_buf()).or_insert_with(|| {
            canonical_root.as_ref().is_some_and(|canonical_root| {
                root.join(parent)
                    .canonicalize()
                    .is_ok_and(|dir| dir.starts_with(canonical_root))
            })
        })
    }
}

/// True iff `p` is a regular file (not a symlink, not a directory). Symlinks
/// are rejected even when they point at a regular file, so a tracked symlink
/// can never expose content outside the repository through the verifier.
fn is_regular_file(p: &Path) -> bool {
    std::fs::symlink_metadata(p)
        .is_ok_and(|metadata| metadata.file_type().is_file() && metadata.len() <= MAX_FILE_BYTES)
}

/// What reading one committed blob produced.
enum BlobExtraction {
    /// The blob's path and sorted, deduplicated gram hashes.
    Indexed(String, Vec<u64>),
    /// Not indexed by design: over the size cap, empty or binary.
    Skipped,
    /// Git could not report or produce the blob: the shard misses a file it
    /// should have, so it must not be shared.
    Unreadable,
}

/// Read + extract one blob straight from the git object store at `commit_oid`.
/// This is what makes the base/delta shards truly git-anchored: their bytes
/// come from the commit, not the working tree, so a dirty-then-reverted file
/// can never poison a shard labeled with that commit. Symlinks stored in git
/// are returned as their target text (a few bytes) and skipped as binary-ish
/// noise; they never traverse the filesystem.
fn extract_blob(
    root: &Path,
    commit_oid: &str,
    rel: &str,
    extractor: &dyn GramExtractor,
) -> BlobExtraction {
    let Some(size) = gitsync::blob_size(root, commit_oid, rel) else {
        return BlobExtraction::Unreadable;
    };
    if size > MAX_FILE_BYTES {
        return BlobExtraction::Skipped;
    }
    let Some(content) = gitsync::show_blob(root, commit_oid, rel) else {
        return BlobExtraction::Unreadable;
    };
    extract_content(rel, &content, extractor)
}

/// The grams of one committed blob's `content`, or why it is not indexed.
fn extract_content(rel: &str, content: &[u8], extractor: &dyn GramExtractor) -> BlobExtraction {
    if content.is_empty() || content[..content.len().min(8192)].contains(&0) {
        return BlobExtraction::Skipped;
    }
    // A `.gitignore` carrying *only* pixel's housekeeping `.pixel/` entry
    // (created from scratch by an earlier `ensure_pixel_gitignored`) is not real
    // tracked project content — keep it out of the file universe just like
    // `.pixel/` itself. Judged on the committed blob, so the shard stays a
    // function of the commit and can be shared across worktrees.
    if rel == ".gitignore" && is_pixel_only_gitignore_text(&String::from_utf8_lossy(content)) {
        return BlobExtraction::Skipped;
    }
    let mut hits = Vec::new();
    extractor.grams(content, &mut hits);
    let mut hashes: Vec<u64> = hits.iter().map(|h| h.hash).collect();
    hashes.sort_unstable();
    hashes.dedup();
    BlobExtraction::Indexed(rel.to_string(), hashes)
}

/// [`extract_blob`] for a run of paths, read through one `git cat-file
/// --batch` instead of two git processes per path. A path the batch's line
/// protocol cannot carry (a newline in its name) goes through
/// [`extract_blob`], and so does every path git never answered because the
/// batch failed part-way: a failed batch costs speed, never files.
fn extract_blobs(
    root: &Path,
    commit_oid: &str,
    rels: &[&String],
    extractor: &dyn GramExtractor,
) -> Vec<BlobExtraction> {
    extract_blobs_via(root, commit_oid, rels, extractor, |specs, visit| {
        gitsync::cat_file_blobs(root, specs, MAX_FILE_BYTES, visit)
    })
}

/// [`extract_blobs`] over any batch reader, so a batch that fails part-way
/// can be tested without breaking a real git.
fn extract_blobs_via<R>(
    root: &Path,
    commit_oid: &str,
    rels: &[&String],
    extractor: &dyn GramExtractor,
    read_batch: R,
) -> Vec<BlobExtraction>
where
    R: FnOnce(&[String], &mut dyn FnMut(usize, BatchObject<'_>)) -> Result<(), pixel_git::GitError>,
{
    let mut outcomes: Vec<Option<BlobExtraction>> = rels.iter().map(|_| None).collect();
    let specs: Vec<String> = rels
        .iter()
        .map(|rel| format!("{commit_oid}:{rel}"))
        .collect();
    let batch = read_batch(&specs, &mut |i, object| {
        outcomes[i] = Some(match object {
            BatchObject::Blob(content) => extract_content(rels[i], content, extractor),
            BatchObject::Oversized(_) => BlobExtraction::Skipped,
            BatchObject::Unsendable => extract_blob(root, commit_oid, rels[i], extractor),
            BatchObject::Missing => BlobExtraction::Unreadable,
        });
    });
    if let Err(e) = batch {
        let unanswered = outcomes.iter().filter(|outcome| outcome.is_none()).count();
        eprintln!(
            "pixel: warning: git cat-file --batch failed at {commit_oid} ({e}); reading the {unanswered} file(s) it did not answer one by one"
        );
    }
    outcomes
        .into_iter()
        .zip(rels)
        .map(|(outcome, rel)| {
            outcome.unwrap_or_else(|| extract_blob(root, commit_oid, rel, extractor))
        })
        .collect()
}

/// Fewest paths one `cat-file --batch` process is given: below it the
/// process costs more than the parallelism it buys, so a small delta reads
/// through a single git.
const MIN_PATHS_PER_BATCH: usize = 64;

/// Paths per batch when `paths` are spread over `threads` workers: one
/// batch per worker, never fewer than [`MIN_PATHS_PER_BATCH`] paths each.
fn batch_len(paths: usize, threads: usize) -> usize {
    paths.div_ceil(threads.max(1)).max(MIN_PATHS_PER_BATCH)
}

/// True unless `PIXEL_INDEX_NO_DEFAULT_IGNORES` asks to index the default
/// ignored directories (`node_modules`, `vendor`, …) too.
#[cfg_attr(test, mutants::skip)] // one-line adapter over the environment; `prunes_default_ignores` is tested
fn default_ignores_pruned() -> bool {
    prunes_default_ignores(
        std::env::var("PIXEL_INDEX_NO_DEFAULT_IGNORES")
            .ok()
            .as_deref(),
    )
}

/// Whether a `PIXEL_INDEX_NO_DEFAULT_IGNORES` value keeps the default
/// ignored directories out of the index: only `1` and `true` let them in.
fn prunes_default_ignores(value: Option<&str>) -> bool {
    !matches!(value, Some("1" | "true"))
}

/// The shared-cache key suffix for a base shard: the same commit indexed
/// with and without the default ignores yields different shards.
fn cache_variant(default_ignores_pruned: bool) -> &'static str {
    if default_ignores_pruned { "" } else { ".all" }
}

/// A shard built from git, and how many of its blobs git could not read.
struct BuiltShard {
    shard: Shard,
    unreadable: usize,
}

/// Build a git-anchored shard from an explicit repo-relative file list,
/// extracting every blob from the git object store at `commit_oid` (parallel).
fn build_shard_from(
    root: &Path,
    rel_paths: &[String],
    extractor: &dyn GramExtractor,
    commit_oid: &str,
    dest: &Path,
    prune_default: bool,
) -> Result<BuiltShard, IndexSetError> {
    let admitted: Vec<&String> = rel_paths
        .iter()
        .filter(|rel| !is_internal(rel))
        .filter(|rel| !prune_default || !rel.split('/').any(crate::index::is_ignored_dir_name))
        .collect();
    let outcomes: Vec<BlobExtraction> = admitted
        .par_chunks(batch_len(admitted.len(), rayon::current_num_threads()))
        .flat_map_iter(|chunk| extract_blobs(root, commit_oid, chunk, extractor))
        .collect();
    let mut unreadable = 0;
    let mut extracted: Vec<(String, Vec<u64>)> = Vec::with_capacity(outcomes.len());
    for outcome in outcomes {
        match outcome {
            BlobExtraction::Indexed(rel, hashes) => extracted.push((rel, hashes)),
            BlobExtraction::Skipped => {}
            BlobExtraction::Unreadable => unreadable += 1,
        }
    }
    extracted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut builder = ShardBuilder::new(&extractor.id());
    builder.set_commit_oid(commit_oid);
    for (rel, hashes) in extracted {
        builder.add_file(&rel, hashes);
    }
    builder.write(dest)?;
    Ok(BuiltShard {
        shard: Shard::open(dest)?,
        unreadable,
    })
}

/// Whether a git repository can delta `base` to HEAD: the shard is anchored
/// to a commit, and that commit is in this repository. A `.pixel/` restored
/// from another checkout can hold a base built at a commit this clone never
/// fetched; `git diff base..HEAD` fails on it, so the base is rebuilt.
fn delta_anchor_held(root: &Path, base: &Shard) -> bool {
    base.commit_oid()
        .is_some_and(|oid| gitsync::commit_exists(root, oid))
}

impl IndexSet {
    /// Open the index at `root/.pixel`, (re)building layers as needed.
    /// Concurrent callers building the same root are serialized via an
    /// exclusive `flock` on `.pixel/build.lock` — the first process builds,
    /// others wait and then load the already-built shard.
    pub fn open_or_build(
        root: &Path,
        extractor: Box<dyn GramExtractor>,
    ) -> Result<Self, IndexSetError> {
        Self::open_or_build_impl(root, extractor, false)
    }

    /// Same as `open_or_build` but bypasses the shared shard cache. Used by
    /// `pixel reindex` to force a rebuild — without this, a poisoned cache
    /// entry would be relinked back and the reindex would be a no-op.
    pub fn open_or_build_bypass_cache(
        root: &Path,
        extractor: Box<dyn GramExtractor>,
    ) -> Result<Self, IndexSetError> {
        Self::open_or_build_impl(root, extractor, true)
    }

    fn open_or_build_impl(
        root: &Path,
        extractor: Box<dyn GramExtractor>,
        bypass_cache: bool,
    ) -> Result<Self, IndexSetError> {
        let started = Instant::now();
        // A `.pixel/` that is a link or that git tracks came from the
        // repository: its shards, extractor id and anchor included, are not
        // pixel's to trust.
        pixel_git::sidecar::check(root)?;
        let gpx_dir = root.join(SHARD_DIR);
        let base_path = gpx_dir.join(SHARD_FILE);
        let head = gitsync::rev_parse_head(root);
        let mut source = BaseSource::Reused;

        // --- base layer (fast path: no lock if shard is valid) ---
        let mut base = match Shard::open(&base_path) {
            Ok(s) if s.extractor_id() == extractor.id() => Some(s),
            _ => None,
        };
        // A git repo demands a git-anchored base; a plain-walk base (no OID)
        // cannot be delta'd against and is rebuilt.
        if head.is_some() {
            base = base.filter(|s| delta_anchor_held(root, s));
        }
        // Non-Git repos: the base shard has no commit anchor, so it can go
        // stale when files are added/removed/edited. Invalidate it when the
        // directory signature has changed (or is missing on first open).
        if head.is_none()
            && base.as_ref().is_some_and(|s| s.commit_oid().is_none())
            && load_plain_sig(&gpx_dir).as_deref() != Some(&plain_signature(root))
        {
            base = None;
        }

        let base = match base {
            Some(s) => s,
            None => {
                // Build needed — acquire exclusive lock so concurrent
                // callers don't duplicate the work. After acquiring, re-check
                // whether the shard is now valid (another process may have
                // built it while we waited).
                let _lock = BuildLock::acquire(root)?;

                // Re-check after acquiring the lock.
                if let Ok(s) = Shard::open(&base_path)
                    && s.extractor_id() == extractor.id()
                {
                    let valid = if head.is_some() {
                        delta_anchor_held(root, &s)
                    } else {
                        s.commit_oid().is_none()
                            && load_plain_sig(&gpx_dir).as_deref() == Some(&plain_signature(root))
                    };
                    if valid {
                        let base = (BaseSource::Reused, millis(started.elapsed()));
                        return Self::finish_open(root, s, extractor, head, &gpx_dir, base);
                    }
                }

                // Invalidate stale delta state alongside a base rebuild.
                std::fs::remove_file(delta_shard_path(&gpx_dir)).ok();
                std::fs::remove_file(crate::delta::state_path(&gpx_dir)).ok();
                let extractor_id = extractor.id();
                let prune_default = default_ignores_pruned();
                let cache_key = format!("{extractor_id}{}", cache_variant(prune_default));
                // When bypassing the cache (reindex), also remove the cached
                // entry so the rebuild produces fresh bytes, not a relink.
                if bypass_cache && let Some(oid) = head.as_deref() {
                    cache::remove_cached(oid, &cache_key);
                }
                match &head {
                    Some(oid) => {
                        // Shared cache: a base shard for this commit, extractor
                        // and ignore variant may already exist (built by another
                        // worktree). Try to hardlink/copy it in before doing the
                        // expensive build.
                        let cached = !bypass_cache
                            && cache::try_link_from_cache(oid, &cache_key, &base_path);
                        let shard = if cached
                            && let Ok(s) = Shard::open(&base_path)
                            && s.extractor_id() == extractor_id
                            && s.commit_oid() == Some(oid.as_str())
                        {
                            source = BaseSource::SharedCache;
                            s
                        } else {
                            source = BaseSource::BuiltFromGit;
                            if cached {
                                // The entry did not open as this commit's shard:
                                // drop it so the rebuild below can replace it
                                // (publishing never overwrites an entry).
                                cache::remove_cached(oid, &cache_key);
                            }
                            // The commit's tree, not the worktree index: the base
                            // shard must be a pure function of (commit, extractor,
                            // ignore variant) to be shared across worktrees.
                            let tracked = gitsync::ls_tree_blobs(root, oid).map_err(|e| {
                                IndexSetError::Io(std::io::Error::other(format!(
                                    "git ls-tree {oid}: {e}"
                                )))
                            })?;
                            let built = build_shard_from(
                                root,
                                &tracked,
                                extractor.as_ref(),
                                oid,
                                &base_path,
                                prune_default,
                            )?;
                            // Publish only a complete shard: one missing a blob
                            // git failed to read would spread the gap to every
                            // worktree at this commit.
                            if built.unreadable == 0 {
                                cache::link_to_cache(&base_path, oid, &cache_key);
                            } else {
                                eprintln!(
                                    "pixel: warning: {} file(s) could not be read from git at {oid}; the index misses them and is not shared",
                                    built.unreadable
                                );
                            }
                            built.shard
                        };
                        DeltaState {
                            base_oid: oid.clone(),
                            delta_oid: None,
                            tombstones: Vec::new(),
                        }
                        .save(&gpx_dir)?;
                        shard
                    }
                    None => {
                        source = BaseSource::BuiltFromWalk;
                        index::build(root, extractor.as_ref())?;
                        let shard = Shard::open(&base_path)?;
                        save_plain_sig(&gpx_dir, &plain_signature(root));
                        shard
                    }
                }
            }
        };

        let base_timing = (source, millis(started.elapsed()));
        Self::finish_open(root, base, extractor, head, &gpx_dir, base_timing)
    }

    /// Complete the open after the base shard is resolved — build delta +
    /// overlay for git repos, assemble the `IndexSet`.
    fn finish_open(
        root: &Path,
        base: Shard,
        extractor: Box<dyn GramExtractor>,
        head: Option<String>,
        gpx_dir: &Path,
        (base_source, base_ms): (BaseSource, u64),
    ) -> Result<Self, IndexSetError> {
        let mut set = Self {
            root: root.to_path_buf(),
            extractor,
            base,
            delta: None,
            delta_tombstones: HashSet::new(),
            overlay: Overlay::new(),
            head: head.clone(),
            open_timings: OpenTimings {
                base: base_source,
                base_ms,
                delta: DeltaSource::None,
                delta_ms: 0,
                overlay_ms: 0,
                overlay_files: 0,
            },
        };

        // --- delta + overlay (git repos only) ---
        if let Some(head_oid) = head {
            let base_oid = set
                .base
                .commit_oid()
                .expect("git-anchored base has an oid")
                .to_string();
            let clock = Instant::now();
            if head_oid != base_oid {
                set.open_timings.delta = set.reconcile_delta(gpx_dir, &base_oid, &head_oid)?;
            }
            set.open_timings.delta_ms = millis(clock.elapsed());
            let clock = Instant::now();
            // Dirty working tree -> overlay.
            for (xy, path) in gitsync::status_porcelain(root) {
                if is_internal(&path) {
                    continue;
                }
                // A `.gitignore` carrying *only* pixel's housekeeping `.pixel/` entry
                // (created from scratch by an earlier `ensure_pixel_gitignored`) is not real
                // project content — keep it out of the file/search universe just
                // like `.pixel/` itself. A real user `.gitignore` with any other
                // ignore rule is fully indexed.
                if path == ".gitignore" && is_pixel_only_gitignore(root) {
                    continue;
                }
                if xy.contains('D') {
                    set.overlay.remove_file(&path);
                } else {
                    set.overlay
                        .refresh_file(root, &path, set.extractor.as_ref());
                }
                set.open_timings.overlay_files += 1;
            }
            set.open_timings.overlay_ms = millis(clock.elapsed());
        }
        Ok(set)
    }

    /// What the open that produced this set cost, layer by layer.
    pub fn open_timings(&self) -> OpenTimings {
        self.open_timings
    }

    /// HEAD when the set was opened (`None` outside a git repository): the
    /// commit base ∪ delta answers for, whatever HEAD has moved to since.
    pub fn opened_head(&self) -> Option<&str> {
        self.head.as_deref()
    }

    /// Every path the overlay answers for instead of base ∪ delta: the
    /// working-tree edits seen at open or refreshed since. Re-reading them
    /// is what notices an edit that was discarded with no event observed.
    pub fn overlay_paths(&self) -> BTreeSet<String> {
        self.overlay
            .files
            .keys()
            .chain(&self.overlay.tombstones)
            .cloned()
            .collect()
    }

    /// Build or reuse the delta layer covering `base_oid..head_oid`.
    fn reconcile_delta(
        &mut self,
        gpx_dir: &Path,
        base_oid: &str,
        head_oid: &str,
    ) -> Result<DeltaSource, IndexSetError> {
        let delta_path = delta_shard_path(gpx_dir);
        // Reuse a delta already pinned to this exact HEAD.
        if let Some(state) = DeltaState::load(gpx_dir)
            && state.base_oid == base_oid
            && state.delta_oid.as_deref() == Some(head_oid)
            && let Ok(s) = Shard::open(&delta_path)
            && s.extractor_id() == self.extractor.id()
        {
            self.delta_tombstones = state.tombstones.into_iter().collect();
            self.delta = Some(s);
            return Ok(DeltaSource::Reused);
        }
        // Cumulative diff base..HEAD — simple and correct however HEAD moved.
        // A failed diff is not "nothing changed": recording it as HEAD's
        // delta would serve the base commit's text as HEAD's.
        let diff =
            gitsync::diff_name_status_or_err(&self.root, base_oid, head_oid).map_err(|e| {
                IndexSetError::Io(std::io::Error::other(format!(
                    "git diff {base_oid}..{head_oid}: {e}"
                )))
            })?;
        let mut changed: Vec<String> = Vec::new();
        let mut tombstones: Vec<String> = Vec::new();
        for (status, path) in diff {
            if is_internal(&path) {
                continue;
            }
            match status {
                'A' => changed.push(path),
                'D' => tombstones.push(path),
                // M, T, and anything else: superseded in base + re-indexed.
                _ => {
                    tombstones.push(path.clone());
                    changed.push(path);
                }
            }
        }
        self.delta = if changed.is_empty() {
            std::fs::remove_file(&delta_path).ok();
            None
        } else {
            Some(
                build_shard_from(
                    &self.root,
                    &changed,
                    self.extractor.as_ref(),
                    head_oid,
                    &delta_path,
                    default_ignores_pruned(),
                )?
                .shard,
            )
        };
        DeltaState {
            base_oid: base_oid.to_string(),
            delta_oid: Some(head_oid.to_string()),
            tombstones: tombstones.clone(),
        }
        .save(gpx_dir)?;
        self.delta_tombstones = tombstones.into_iter().collect();
        Ok(DeltaSource::Rebuilt)
    }

    /// Re-extract one admitted file into the overlay, or tombstone an excluded path.
    pub fn refresh_file(&mut self, rel_path: &str) -> RefreshOutcome {
        self.refresh_files(&[(rel_path, false)])
            .into_iter()
            .find_map(|(path, outcome)| (path == rel_path).then_some(outcome))
            .unwrap_or(RefreshOutcome::Excluded)
    }

    /// Refresh a watcher batch from one ignore-policy snapshot.
    pub fn refresh_files(&mut self, files: &[(&str, bool)]) -> Vec<(String, RefreshOutcome)> {
        let policy_changed = files.iter().any(|(path, _)| is_ignore_control_path(path));
        let admitted = if policy_changed {
            index::policy_indexable_paths(&self.root)
        } else {
            index::policy_file_paths(&self.root)
        };
        let before: HashSet<String> = self.paths().into_iter().collect();
        let mut outcomes = BTreeMap::new();

        for &(path, removed) in files {
            if removed || !admitted.contains(path) {
                self.overlay.remove_file(path);
                outcomes.insert(path.to_string(), RefreshOutcome::Excluded);
            } else {
                self.overlay
                    .refresh_file(&self.root, path, self.extractor.as_ref());
                outcomes.insert(path.to_string(), RefreshOutcome::Indexed);
            }
        }

        if policy_changed {
            for path in before.difference(&admitted) {
                self.overlay.remove_file(path);
                outcomes.insert(path.clone(), RefreshOutcome::Excluded);
            }
            for path in admitted.difference(&before) {
                self.overlay
                    .refresh_file(&self.root, path, self.extractor.as_ref());
                outcomes.insert(path.clone(), RefreshOutcome::Indexed);
            }
        }

        outcomes.into_iter().collect()
    }

    /// Tombstone a deleted file everywhere.
    pub fn remove_file(&mut self, rel_path: &str) {
        self.overlay.remove_file(rel_path);
    }

    /// Merge-order query: base ∪ delta candidates − tombstones + overlay
    /// matches → verify every survivor with the real regex.
    ///
    /// `limit` caps the number of matches returned. When set, verification
    /// proceeds in path-sorted chunks and stops as soon as `limit` matches
    /// have been found (early stop), so a broad pattern over a huge tree
    /// cannot run unbounded. `stats.truncated` is set when more matches exist
    /// beyond the returned slice. `None` means no limit (verify everything in
    /// one parallel pass).
    pub fn search(
        &self,
        pattern: &str,
        limit: Option<usize>,
    ) -> Result<(Vec<MatchLine>, SearchStats), IndexSetError> {
        self.search_page(pattern, 0, limit)
    }

    /// Search a stable path/line-ordered page without retaining skipped
    /// matches. `offset` counts matching lines, not candidate files.
    pub fn search_page(
        &self,
        pattern: &str,
        offset: usize,
        limit: Option<usize>,
    ) -> Result<(Vec<MatchLine>, SearchStats), IndexSetError> {
        self.search_page_in(pattern, offset, limit, None)
    }

    /// `search_page` restricted to repo-relative path prefixes (component-wise,
    /// so `src/foo` never matches `src/foobar`). `None`/empty = whole repo.
    /// Filtering happens before verification and before the row limit, so
    /// pagination stays correct within the requested subtrees.
    pub fn search_page_in(
        &self,
        pattern: &str,
        offset: usize,
        limit: Option<usize>,
        path_prefixes: Option<&[String]>,
    ) -> Result<(Vec<MatchLine>, SearchStats), IndexSetError> {
        self.search_page_filtered(pattern, offset, limit, path_prefixes, None)
    }

    /// [`Self::search_page_in`] that also drops every candidate file `filter`
    /// does not keep (`-g`/`-t`), before verification: `offset`, `limit` and
    /// `truncated` then count kept matches only, so a page of a filtered
    /// search is as long as an unfiltered one and pages without gaps.
    pub fn search_page_filtered(
        &self,
        pattern: &str,
        offset: usize,
        limit: Option<usize>,
        path_prefixes: Option<&[String]>,
        filter: Option<&crate::path_filter::PathFilter>,
    ) -> Result<(Vec<MatchLine>, SearchStats), IndexSetError> {
        let started = std::time::Instant::now();
        let query = plan_pattern(pattern, self.extractor.as_ref())
            .map_err(|e| IndexSetError::Pattern(e.to_string()))?;
        let scanned_all = matches!(query, GramQuery::All);

        let mut paths: BTreeSet<String> = BTreeSet::new();
        // Base candidates, minus everything superseded by newer layers.
        for id in resolve_query(&query, self.base.file_count(), &|h| self.base.postings(h)) {
            if let Some(p) = self.base.path_of(id)
                && !self.delta_tombstones.contains(p)
                && !self.overlay.tombstones.contains(p)
            {
                paths.insert(p.to_string());
            }
        }
        // Delta candidates, minus dirty-overlay supersessions.
        if let Some(delta) = &self.delta {
            for id in resolve_query(&query, delta.file_count(), &|h| delta.postings(h)) {
                if let Some(p) = delta.path_of(id)
                    && !self.overlay.tombstones.contains(p)
                {
                    paths.insert(p.to_string());
                }
            }
        }
        // Overlay files whose in-memory gram set satisfies the plan.
        for p in self.overlay.matching_files(&query) {
            paths.insert(p.to_string());
        }

        let mut candidates: Vec<String> = paths.into_iter().collect();
        if let Some(prefixes) = path_prefixes
            && !prefixes.is_empty()
        {
            candidates.retain(|rel| {
                let rel_path = std::path::Path::new(rel);
                prefixes
                    .iter()
                    .any(|p| p.is_empty() || rel_path.starts_with(p))
            });
        }
        if let Some(filter) = filter {
            candidates.retain(|rel| filter.keeps(rel));
        }
        let mut contained = Containment::new(&self.root);
        candidates.retain(|rel| contained.admits(rel));
        let verifier = Verifier::new(pattern)?;

        // Limited verification searches sorted files until one match beyond
        // the requested limit is observed. That bounds retained matches and
        // makes `truncated` exact: remaining candidates alone are not proof
        // that another match exists.
        let mut matches: Vec<MatchLine> = Vec::new();
        let mut truncated = false;
        if let Some(limit) = limit {
            let probe_target = limit.saturating_add(1);
            let mut skip = offset;
            for rel in &candidates {
                let abs = self.root.join(rel);
                if !is_regular_file(&abs) {
                    continue;
                }
                let remaining = probe_target.saturating_sub(matches.len());
                if remaining == 0 {
                    break;
                }
                verifier.search_file_page(&abs, rel, &mut matches, &mut skip, Some(remaining))?;
                if matches.len() > limit {
                    truncated = true;
                    matches.truncate(limit);
                    break;
                }
            }
        } else {
            let results: Vec<Vec<MatchLine>> = candidates
                .par_iter()
                .filter_map(|rel| {
                    let abs = self.root.join(rel);
                    if !is_regular_file(&abs) {
                        return None;
                    }
                    let mut out = Vec::new();
                    verifier.search_file(&abs, rel, &mut out, None).ok()?;
                    (!out.is_empty()).then_some(out)
                })
                .collect();
            matches = results.into_iter().flatten().collect();
            matches.sort_by(|a, b| a.path.cmp(&b.path).then(a.line_number.cmp(&b.line_number)));
        }

        let stats = SearchStats {
            candidates: candidates.len(),
            scanned_all,
            matches: matches.len(),
            elapsed_us: started.elapsed().as_micros(),
            truncated,
        };
        Ok((matches, stats))
    }

    /// All live file paths across base ∪ delta − tombstones + overlay,
    /// sorted ascending — the canonical fresh file universe.
    pub fn paths(&self) -> Vec<String> {
        let mut paths: BTreeSet<String> = BTreeSet::new();
        for id in 0..self.base.file_count() {
            if let Some(p) = self.base.path_of(id)
                && !self.delta_tombstones.contains(p)
                && !self.overlay.tombstones.contains(p)
            {
                paths.insert(p.to_string());
            }
        }
        if let Some(delta) = &self.delta {
            for id in 0..delta.file_count() {
                if let Some(p) = delta.path_of(id)
                    && !self.overlay.tombstones.contains(p)
                {
                    paths.insert(p.to_string());
                }
            }
        }
        for p in self.overlay.files.keys() {
            paths.insert(p.clone());
        }
        paths.into_iter().collect()
    }

    pub fn status(&self) -> FreshnessStatus {
        FreshnessStatus {
            commit_oid: self.base.commit_oid().map(str::to_string),
            base_files: self.base.file_count(),
            delta_files: self.delta.as_ref().map_or(0, Shard::file_count),
            overlay_files: self.overlay.files.len(),
            tombstones: self.delta_tombstones.len() + self.overlay.tombstones.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gram::SparseGramExtractor;
    use crate::weights::Crc32Weigher;
    use std::path::PathBuf;
    use std::process::Command;

    /// The verifier reads only what `is_regular_file` admits: a symlink
    /// (content outside the repository), a directory or an oversized file
    /// must all be refused, and a plain file under the cap accepted.
    #[test]
    fn is_regular_file_admits_only_plain_files_under_the_size_cap() {
        let dir = std::env::temp_dir().join(format!("gpx-regular-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let plain = dir.join("plain.txt");
        std::fs::write(&plain, b"hello").unwrap();
        assert!(is_regular_file(&plain));
        assert!(!is_regular_file(&dir));
        assert!(!is_regular_file(&dir.join("missing.txt")));
        // A sparse file just past the cap: no 4 MiB write needed.
        let big = dir.join("big.bin");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert!(!is_regular_file(&big));
        #[cfg(unix)]
        {
            let link = dir.join("link.txt");
            std::os::unix::fs::symlink(&plain, &link).unwrap();
            assert!(!is_regular_file(&link));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn ex() -> Box<dyn GramExtractor> {
        Box::new(SparseGramExtractor::new(Crc32Weigher))
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gpx-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A private shard cache for one test. `open_or_build` reads and writes
    /// the cache under `$XDG_CACHE_HOME`, so a test that opens an index
    /// without it links from and publishes to the developer's real
    /// `~/.cache/pixel/shards` (one entry per fixture commit, never evicted
    /// by the suite). Holding the guard serialises the tests that set the
    /// variable (`CACHE_TEST_LOCK`), points it at a fresh directory and puts
    /// the previous value back on drop, a failed assertion included.
    struct IsolatedCache {
        home: PathBuf,
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl IsolatedCache {
        fn new(tag: &str) -> Self {
            let lock = crate::cache::CACHE_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let home = scratch(&format!("{tag}-cache-home"));
            let previous = std::env::var_os("XDG_CACHE_HOME");
            // SAFETY: every test that sets XDG_CACHE_HOME holds
            // CACHE_TEST_LOCK, taken above.
            unsafe {
                std::env::set_var("XDG_CACHE_HOME", &home);
            }
            Self {
                home,
                previous,
                _lock: lock,
            }
        }

        /// Where the cache entries of this test live.
        fn shards(&self) -> PathBuf {
            self.home.join("pixel").join("shards")
        }
    }

    impl Drop for IsolatedCache {
        fn drop(&mut self) {
            // SAFETY: the lock is still held; fields drop after this body.
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var("XDG_CACHE_HOME", value),
                    None => std::env::remove_var("XDG_CACHE_HOME"),
                }
            }
            std::fs::remove_dir_all(&self.home).ok();
        }
    }

    #[test]
    fn cache_variant_separates_indexes_built_with_and_without_default_ignores() {
        assert_eq!(cache_variant(true), "");
        assert_eq!(cache_variant(false), ".all");
        assert!(prunes_default_ignores(None));
        assert!(prunes_default_ignores(Some("0")));
        assert!(prunes_default_ignores(Some("yes")));
        assert!(!prunes_default_ignores(Some("1")));
        assert!(!prunes_default_ignores(Some("true")));
    }

    /// The size cap is inclusive: a blob of exactly `MAX_FILE_BYTES` is
    /// indexed, one byte more is skipped, and a real `.gitignore` is content.
    #[test]
    fn extract_blob_caps_size_inclusively_and_indexes_a_real_gitignore() {
        let dir = scratch("extract-blob-cap");
        git(&dir, &["init", "-q"]);
        let cap = usize::try_from(MAX_FILE_BYTES).unwrap();
        std::fs::write(dir.join("at_cap.txt"), "a".repeat(cap)).unwrap();
        std::fs::write(dir.join("over_cap.txt"), "a".repeat(cap + 1)).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "caps"]);
        let head = git_out(&dir, &["rev-parse", "HEAD"]);
        let extractor = ex();
        let outcome = |rel: &str| extract_blob(&dir, &head, rel, extractor.as_ref());
        assert!(matches!(outcome("at_cap.txt"), BlobExtraction::Indexed(..)));
        assert!(matches!(outcome("over_cap.txt"), BlobExtraction::Skipped));
        assert!(
            matches!(outcome(".gitignore"), BlobExtraction::Indexed(p, _) if p == ".gitignore")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A valid shard cached for this commit is linked in instead of rebuilt:
    /// the cached one here was built without `a.rs`, so a search that misses
    /// the needle proves the cache answered.
    #[test]
    fn a_valid_cached_shard_is_reused_without_a_rebuild() {
        let _cache = IsolatedCache::new("cache-reuse");
        let dir = scratch("cache-reuse");
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.rs"), "fn reusedNeedle() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);
        let head = git_out(&dir, &["rev-parse", "HEAD"]);
        let crafted = dir.join("crafted.shard");
        build_shard_from(&dir, &[], ex().as_ref(), &head, &crafted, true).unwrap();
        let entry = crate::cache::cached_shard_path(&head, &ex().id()).unwrap();
        std::fs::copy(&crafted, &entry).unwrap();

        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        assert!(
            set.search("reusedNeedle", None).unwrap().0.is_empty(),
            "the cached shard was used, not a rebuild"
        );
        drop(set);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Each committed blob is indexed, skipped by design, or unreadable, and
    /// only the last one makes a shard unfit to share.
    #[test]
    fn extract_blob_tells_skipped_from_unreadable() {
        let dir = scratch("extract-blob");
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("code.rs"), "fn needle() {}\n").unwrap();
        std::fs::write(dir.join("empty.txt"), "").unwrap();
        std::fs::write(dir.join("bin.dat"), [0u8, 1, 2, 0]).unwrap();
        std::fs::write(dir.join(".gitignore"), ".pixel/\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);
        // The worktree copy says otherwise: the commit decides.
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        let head = git_out(&dir, &["rev-parse", "HEAD"]);
        let extractor = ex();
        let outcome = |rel: &str| extract_blob(&dir, &head, rel, extractor.as_ref());
        assert!(
            matches!(outcome("code.rs"), BlobExtraction::Indexed(p, h) if p == "code.rs" && !h.is_empty())
        );
        assert!(matches!(outcome("empty.txt"), BlobExtraction::Skipped));
        assert!(matches!(outcome("bin.dat"), BlobExtraction::Skipped));
        assert!(matches!(outcome(".gitignore"), BlobExtraction::Skipped));
        assert!(matches!(outcome("absent.rs"), BlobExtraction::Unreadable));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_shard_from_counts_unreadable_blobs_and_prunes_default_ignores_on_request() {
        let dir = scratch("build-shard-from");
        git(&dir, &["init", "-q"]);
        std::fs::create_dir_all(dir.join("node_modules/dep")).unwrap();
        std::fs::write(dir.join("app.rs"), "fn app() {}\n").unwrap();
        std::fs::write(
            dir.join("node_modules/dep/index.js"),
            "module.exports = 1;\n",
        )
        .unwrap();
        git(&dir, &["add", "-f", "."]);
        git(&dir, &["commit", "-qm", "one"]);
        let head = git_out(&dir, &["rev-parse", "HEAD"]);
        let paths: Vec<String> = ["app.rs", "node_modules/dep/index.js", "ghost.rs"]
            .iter()
            .map(|p| (*p).to_string())
            .collect();
        let dest = dir.join("out.shard");
        let pruned = build_shard_from(&dir, &paths, ex().as_ref(), &head, &dest, true).unwrap();
        assert_eq!(pruned.unreadable, 1, "ghost.rs is not in the commit");
        assert_eq!(pruned.shard.file_count(), 1);
        let all = build_shard_from(&dir, &paths, ex().as_ref(), &head, &dest, false).unwrap();
        assert_eq!(all.unreadable, 1);
        assert_eq!(all.shard.file_count(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Reading through `cat-file --batch` keeps every blob under its own
    /// path across several batches, indexes a path the batch protocol
    /// cannot carry (a newline in its name, a trailing carriage return)
    /// through the per-file read,
    /// skips an over-cap blob and counts a path the commit lacks as
    /// unreadable.
    #[test]
    fn build_shard_from_keeps_every_blob_on_its_own_path_across_batches() {
        const FILES: usize = 150;
        let dir = scratch("build-shard-batches");
        git(&dir, &["init", "-q"]);
        let mut contents: Vec<(String, String)> = (0..FILES)
            .map(|i| {
                (
                    format!("f{i:03}.rs"),
                    format!("fn needle_{i:03}_{}() {{}}\n", i * 7),
                )
            })
            .collect();
        contents.push((
            "odd\nname.rs".to_string(),
            "fn newline_needle() {}\n".to_string(),
        ));
        // Git strips a trailing carriage return from a batch request, so
        // `f000.rs\r` must not be answered with `f000.rs`'s content.
        contents.push((
            "f000.rs\r".to_string(),
            "fn carriage_return_needle() {}\n".to_string(),
        ));
        for (rel, text) in &contents {
            std::fs::write(dir.join(rel), text).unwrap();
        }
        let cap = usize::try_from(MAX_FILE_BYTES).unwrap();
        std::fs::write(dir.join("big.txt"), "b".repeat(cap + 1)).unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "many"]);
        let head = git_out(&dir, &["rev-parse", "HEAD"]);
        let mut paths: Vec<String> = contents.iter().map(|(rel, _)| rel.clone()).collect();
        paths.push("big.txt".to_string());
        paths.push("ghost.rs".to_string());
        let dest = dir.join("out.shard");

        let built = build_shard_from(&dir, &paths, ex().as_ref(), &head, &dest, true).unwrap();

        assert_eq!(built.unreadable, 1, "ghost.rs is not in the commit");
        let mut expected: Vec<String> = contents.iter().map(|(rel, _)| rel.clone()).collect();
        expected.sort();
        assert_eq!(
            built.shard.files(),
            expected.as_slice(),
            "big.txt is over the cap"
        );
        for (file_id, rel) in built.shard.files().iter().enumerate() {
            let text = &contents.iter().find(|(r, _)| r == rel).unwrap().1;
            let mut hits = Vec::new();
            ex().grams(text.as_bytes(), &mut hits);
            let id = u32::try_from(file_id).unwrap();
            for hit in hits {
                assert!(
                    built.shard.postings(hit.hash).contains(&id),
                    "{rel:?} lacks a gram of its own content"
                );
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A batch that dies after its first answer loses no file: the answer
    /// it gave is kept, every path it never answered is read on its own,
    /// and only a path the commit really lacks stays unreadable.
    #[test]
    fn a_batch_failing_part_way_falls_back_to_one_read_per_path() {
        let dir = scratch("batch-part-way");
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.rs"), "fn first_needle() {}\n").unwrap();
        std::fs::write(dir.join("b.rs"), "fn second_needle() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "two"]);
        let head = git_out(&dir, &["rev-parse", "HEAD"]);
        let rels: Vec<String> = ["a.rs", "b.rs", "ghost.rs"]
            .iter()
            .map(|p| (*p).to_string())
            .collect();
        let refs: Vec<&String> = rels.iter().collect();
        let extractor = ex();
        let outcomes = extract_blobs_via(&dir, &head, &refs, extractor.as_ref(), |_, visit| {
            // Not the committed bytes: the grams must come from the answer.
            visit(0, BatchObject::Blob(b"fn batch_only_needle() {}\n"));
            Err(pixel_git::GitError::Timeout {
                args: vec!["cat-file".to_string()],
            })
        });
        let shape: Vec<String> = outcomes
            .iter()
            .map(|outcome| match outcome {
                BlobExtraction::Indexed(rel, hashes) if !hashes.is_empty() => {
                    format!("indexed {rel}")
                }
                BlobExtraction::Indexed(rel, _) => format!("empty {rel}"),
                BlobExtraction::Skipped => "skipped".to_string(),
                BlobExtraction::Unreadable => "unreadable".to_string(),
            })
            .collect();
        assert_eq!(shape, ["indexed a.rs", "indexed b.rs", "unreadable"]);
        let mut hits = Vec::new();
        extractor.grams(b"fn batch_only_needle() {}\n", &mut hits);
        let mut from_batch: Vec<u64> = hits.iter().map(|h| h.hash).collect();
        from_batch.sort_unstable();
        from_batch.dedup();
        assert!(
            matches!(&outcomes[0], BlobExtraction::Indexed(_, hashes) if *hashes == from_batch),
            "a.rs is indexed from the batch's answer, not re-read"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// One batch per worker, never so small that a git process costs more
    /// than it saves, and a zero thread count does not divide by zero.
    #[test]
    fn batch_len_spreads_paths_over_workers_with_a_floor() {
        assert_eq!(batch_len(19_001, 8), 2_376);
        assert_eq!(batch_len(700, 10), 70);
        assert_eq!(batch_len(3, 8), MIN_PATHS_PER_BATCH);
        assert_eq!(batch_len(0, 8), MIN_PATHS_PER_BATCH);
        assert_eq!(batch_len(1_000, 0), 1_000);
    }

    /// The shared cache holds only complete shards for their exact key: a
    /// corrupt entry is replaced by the rebuild it forced, and a commit
    /// whose tree names a blob git cannot read is indexed locally but never
    /// published.
    #[test]
    fn the_shared_cache_replaces_a_bad_entry_and_never_stores_an_incomplete_shard() {
        let _cache = IsolatedCache::new("cache-publish");
        let dir = scratch("cache-publish");
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.rs"), "fn cachedNeedle() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);
        let head = git_out(&dir, &["rev-parse", "HEAD"]);
        let key = ex().id();
        let entry = crate::cache::cached_shard_path(&head, &key).unwrap();
        std::fs::write(&entry, b"not a shard").unwrap();

        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        assert_eq!(set.search("cachedNeedle", None).unwrap().0.len(), 1);
        let cached = Shard::open(&entry).expect("the corrupt entry was replaced by the rebuild");
        assert_eq!(cached.commit_oid(), Some(head.as_str()));
        drop(set);

        // A second commit whose tree names a blob git does not have.
        let tree = git_out(&dir, &["write-tree"]);
        let _ = tree;
        git(
            &dir,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "100644,2222222222222222222222222222222222222222,ghost.rs",
            ],
        );
        let broken_tree = git_out(&dir, &["write-tree", "--missing-ok"]);
        let broken = git_out(
            &dir,
            &["commit-tree", &broken_tree, "-p", &head, "-m", "ghost"],
        );
        git(&dir, &["reset", "-q", "--soft", &broken]);
        std::fs::remove_dir_all(dir.join(SHARD_DIR)).ok();
        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        assert_eq!(set.search("cachedNeedle", None).unwrap().0.len(), 1);
        assert!(
            !crate::cache::cached_shard_path(&broken, &key)
                .unwrap()
                .exists(),
            "an incomplete shard is not shared"
        );
        drop(set);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn git_anchored_layers_end_to_end() {
        let _cache = IsolatedCache::new("git_anchored_layers_end_to_end");
        let dir = std::env::temp_dir().join(format!("gpx-indexset-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("alpha.rs"), "fn handleClick() {}\n").unwrap();
        std::fs::write(dir.join("beta.rs"), "fn openMenuWidget() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);

        // Base layer finds committed content.
        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        let st = set.status();
        assert!(st.commit_oid.is_some());
        assert_eq!(st.base_files, 2);
        let (m, _) = set.search("handleClick", None).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].path, "alpha.rs");

        // Commit a change + a new file -> reopened set builds a delta layer.
        std::fs::write(dir.join("alpha.rs"), "fn renamedEntryPoint() {}\n").unwrap();
        std::fs::write(dir.join("gamma.rs"), "fn freshDeltaSymbol() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "two"]);
        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        let st = set.status();
        assert_eq!(st.delta_files, 2, "alpha (modified) + gamma (added)");
        let (m, _) = set.search("freshDeltaSymbol", None).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].path, "gamma.rs");
        let (m, _) = set.search("handleClick", None).unwrap();
        assert!(m.is_empty(), "old base content is tombstoned by the delta");

        // Overlay: uncommitted edit is visible without a rebuild.
        let mut set = set;
        std::fs::write(dir.join("beta.rs"), "fn overlayOnlySymbol() {}\n").unwrap();
        set.refresh_file("beta.rs");
        let (m, _) = set.search("overlayOnlySymbol", None).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].path, "beta.rs");
        let (m, _) = set.search("openMenuWidget", None).unwrap();
        assert!(m.is_empty(), "overlay tombstones the stale base entry");

        // remove_file tombstones everywhere.
        std::fs::remove_file(dir.join("gamma.rs")).unwrap();
        set.remove_file("gamma.rs");
        let (m, _) = set.search("freshDeltaSymbol", None).unwrap();
        assert!(m.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refresh_file_excludes_gitignore_and_git_info_exclude_paths() {
        let _cache =
            IsolatedCache::new("refresh_file_excludes_gitignore_and_git_info_exclude_paths");
        let dir = std::env::temp_dir().join(format!(
            "gpx-indexset-refresh-ignore-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("visible.rs"), "fn visible_base() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);

        let mut set = IndexSet::open_or_build(&dir, ex()).unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored.env\n").unwrap();
        std::fs::write(dir.join("ignored.env"), "gitignoreSecretNeedle\n").unwrap();
        std::fs::write(dir.join(".git/info/exclude"), "info-excluded.json\n").unwrap();
        std::fs::write(dir.join("info-excluded.json"), "infoExcludeSecretNeedle\n").unwrap();

        assert_eq!(set.refresh_file("ignored.env"), RefreshOutcome::Excluded);
        assert_eq!(
            set.refresh_file("info-excluded.json"),
            RefreshOutcome::Excluded
        );
        assert!(set.search("SecretNeedle", None).unwrap().0.is_empty());
        assert_eq!(set.status().overlay_files, 0);
        assert!(!set.paths().iter().any(|path| path.contains("excluded")));

        std::fs::write(dir.join("ordinary.txt"), "ordinaryOverlayNeedle\n").unwrap();
        assert_eq!(set.refresh_file("ordinary.txt"), RefreshOutcome::Indexed);
        assert_eq!(
            set.search("ordinaryOverlayNeedle", None).unwrap().0.len(),
            1
        );

        std::fs::write(dir.join(".gitignore"), "ignored.env\nordinary.txt\n").unwrap();
        set.refresh_file(".gitignore");
        assert!(
            set.search("ordinaryOverlayNeedle", None)
                .unwrap()
                .0
                .is_empty()
        );
        std::fs::write(dir.join(".gitignore"), "ignored.env\n").unwrap();
        set.refresh_file(".gitignore");
        assert_eq!(
            set.search("ordinaryOverlayNeedle", None).unwrap().0.len(),
            1
        );

        std::fs::write(dir.join("info-live.json"), "infoTransitionNeedle\n").unwrap();
        set.refresh_file("info-live.json");
        std::fs::write(
            dir.join(".git/info/exclude"),
            "info-excluded.json\ninfo-live.json\n",
        )
        .unwrap();
        set.refresh_file(".git/info/exclude");
        assert!(
            set.search("infoTransitionNeedle", None)
                .unwrap()
                .0
                .is_empty()
        );
        std::fs::write(dir.join(".git/info/exclude"), "info-excluded.json\n").unwrap();
        set.refresh_file(".git/info/exclude");
        assert_eq!(set.search("infoTransitionNeedle", None).unwrap().0.len(), 1);

        std::fs::write(dir.join("ignore-live.log"), "ignoreTransitionNeedle\n").unwrap();
        set.refresh_file("ignore-live.log");
        std::fs::write(dir.join(".ignore"), "ignore-live.log\n").unwrap();
        set.refresh_file(".ignore");
        assert!(
            set.search("ignoreTransitionNeedle", None)
                .unwrap()
                .0
                .is_empty()
        );
        std::fs::write(dir.join(".ignore"), "").unwrap();
        set.refresh_file(".ignore");
        assert_eq!(
            set.search("ignoreTransitionNeedle", None).unwrap().0.len(),
            1
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn paths_merges_layers_and_honors_tombstones() {
        let _cache = IsolatedCache::new("paths_merges_layers_and_honors_tombstones");
        let dir = std::env::temp_dir().join(format!("gpx-indexset-paths-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("alpha.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(dir.join("beta.rs"), "fn beta() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);

        let mut set = IndexSet::open_or_build(&dir, ex()).unwrap();
        assert_eq!(
            set.paths(),
            vec!["alpha.rs".to_string(), "beta.rs".to_string()]
        );

        // Overlay add is included; overlay delete is tombstoned out.
        std::fs::write(dir.join("gamma.rs"), "fn gamma() {}\n").unwrap();
        set.refresh_file("gamma.rs");
        std::fs::remove_file(dir.join("beta.rs")).unwrap();
        set.remove_file("beta.rs");
        assert_eq!(
            set.paths(),
            vec!["alpha.rs".to_string(), "gamma.rs".to_string()]
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_git_plain_build() {
        let _cache = IsolatedCache::new("non_git_plain_build");
        let dir = std::env::temp_dir().join(format!("gpx-indexset-nogit-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("solo.txt"), "plainWalkNeedle here\n").unwrap();

        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        assert!(set.status().commit_oid.is_none());
        let (m, _) = set.search("plainWalkNeedle", None).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].path, "solo.txt");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The anchor check is for git repositories only: an unchanged plain
    /// directory reopens on the base it built, without rewriting it.
    #[test]
    fn an_unchanged_plain_directory_reopens_without_rebuilding_its_base() {
        let _cache = IsolatedCache::new("plain-reopen");
        let dir = scratch("plain-reopen");
        std::fs::write(dir.join("solo.txt"), "plainReopenNeedle\n").unwrap();
        let base = dir.join(SHARD_DIR).join(SHARD_FILE);
        drop(IndexSet::open_or_build(&dir, ex()).unwrap());
        let written = std::fs::metadata(&base).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));

        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        assert_eq!(set.search("plainReopenNeedle", None).unwrap().0.len(), 1);
        assert_eq!(
            std::fs::metadata(&base).unwrap().modified().unwrap(),
            written,
            "the base shard was rebuilt"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Phase 3 item 4: hidden files (dotfiles, `.github/`, `.claude/`) are
    /// real project content and must be indexed — while `.git/` and our own
    /// `.pixel/` sidecar must never be, even with the hidden filter off.
    #[test]
    fn hidden_files_are_indexed_but_git_dir_is_not() {
        let _cache = IsolatedCache::new("hidden_files_are_indexed_but_git_dir_is_not");
        let dir = std::env::temp_dir().join(format!(
            "gpx-indexset-hidden-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        std::fs::remove_dir_all(&dir).ok();

        // --- non-Git directory: the plain-walk build path ---
        std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
        std::fs::write(
            dir.join(".github/workflows/x.yml"),
            "jobs:\n  build:\n    run: hiddenWorkflowNeedle\n",
        )
        .unwrap();
        std::fs::write(dir.join(".dotfileNeedle.cfg"), "dotfileContentNeedle=1\n").unwrap();
        // A fake .git dir (not a valid repo, so the plain-walk path is used)
        // whose content must NEVER be indexed.
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/config"), "gitInternalNeedle = true\n").unwrap();
        std::fs::write(dir.join("visible.txt"), "plainVisibleNeedle\n").unwrap();

        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        let (m, _) = set.search("hiddenWorkflowNeedle", None).unwrap();
        assert_eq!(
            m.len(),
            1,
            "hidden .github workflow content must be searchable"
        );
        assert_eq!(m[0].path, ".github/workflows/x.yml");
        let (m, _) = set.search("dotfileContentNeedle", None).unwrap();
        assert_eq!(m.len(), 1, "dotfile content must be searchable");
        let (m, _) = set.search("gitInternalNeedle", None).unwrap();
        assert!(m.is_empty(), ".git/ content must never be indexed: {m:?}");
        assert!(
            set.paths()
                .iter()
                .all(|p| !p.starts_with(".git/") && !p.starts_with(".pixel/")),
            "no .git/ or .pixel/ path may appear in the file universe: {:?}",
            set.paths()
        );

        // Freshness must react to hidden-file edits too (plain signature
        // walks the same policy).
        std::fs::write(
            dir.join(".github/workflows/x.yml"),
            "jobs:\n  build:\n    run: editedHiddenNeedle\n",
        )
        .unwrap();
        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        let (m, _) = set.search("editedHiddenNeedle", None).unwrap();
        assert_eq!(
            m.len(),
            1,
            "hidden-file edits must invalidate the plain signature"
        );

        std::fs::remove_dir_all(&dir).ok();

        // --- git repo: tracked hidden files come through the git-anchored
        // base, and .git/ still never appears in the universe ---
        std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
        git(&dir, &["init", "-q"]);
        std::fs::write(
            dir.join(".github/workflows/x.yml"),
            "jobs:\n  test:\n    run: trackedHiddenNeedle\n",
        )
        .unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "hidden"]);
        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        let (m, _) = set.search("trackedHiddenNeedle", None).unwrap();
        assert_eq!(
            m.len(),
            1,
            "tracked hidden files must be searchable in a git repo"
        );
        assert!(
            set.paths().iter().all(|p| !p.starts_with(".git/")),
            "git repo universe must not contain .git/ paths"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Regression: non-Git directories must detect file changes on reopen
    /// and rebuild the base shard. Previously the base was reused without
    /// checking freshness, so new/edited files were invisible after reopening.
    #[test]
    fn non_git_reopen_detects_changes() {
        let _cache = IsolatedCache::new("non_git_reopen_detects_changes");
        let dir = std::env::temp_dir().join(format!(
            "gpx-indexset-nongit-stale-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        // Initial build with one file.
        std::fs::write(dir.join("a.txt"), "firstNeedle here\n").unwrap();
        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        let (m, _) = set.search("firstNeedle", None).unwrap();
        assert_eq!(m.len(), 1, "initial file must be indexed");

        // Add a new file and edit the existing one. Without reopening in the
        // same process, we simulate a cold reopen by dropping and rebuilding.
        std::fs::write(dir.join("b.txt"), "secondNeedle here\n").unwrap();
        std::fs::write(dir.join("a.txt"), "firstNeedle editedContent\n").unwrap();

        // Reopen: the signature check must detect the changes and rebuild.
        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        let (m, _) = set.search("secondNeedle", None).unwrap();
        assert_eq!(
            m.len(),
            1,
            "new file must be visible after reopen (non-Git staleness fix)"
        );
        assert_eq!(m[0].path, "b.txt");
        let (m, _) = set.search("editedContent", None).unwrap();
        assert_eq!(m.len(), 1, "edited content must be visible after reopen");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_git_reopen_detects_equal_size_edit_with_restored_mtime() {
        let _cache =
            IsolatedCache::new("non_git_reopen_detects_equal_size_edit_with_restored_mtime");
        let dir = std::env::temp_dir().join(format!(
            "gpx-indexset-nongit-content-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("a.txt");
        let reference = dir.join("mtime-reference");
        std::fs::write(&source, "oldSameSizeNeedle\n").unwrap();
        std::fs::write(&reference, "reference\n").unwrap();
        assert!(
            std::process::Command::new("touch")
                .args(["-r"])
                .arg(&source)
                .arg(&reference)
                .status()
                .unwrap()
                .success()
        );
        let _ = IndexSet::open_or_build(&dir, ex()).unwrap();
        std::fs::write(&source, "newSameSizeNeedle\n").unwrap();
        assert!(
            std::process::Command::new("touch")
                .args(["-r"])
                .arg(&reference)
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );

        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        let (new_matches, _) = set.search("newSameSizeNeedle", None).unwrap();
        let (old_matches, _) = set.search("oldSameSizeNeedle", None).unwrap();
        assert_eq!(new_matches.len(), 1);
        assert!(old_matches.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Regression: base shard must be git-anchored — its bytes come from the
    /// commit, not the working tree. The bug was: rebuild on a dirty working
    /// tree indexed dirty bytes but labeled them as HEAD, so restoring the
    /// file caused a false-negative (the committed content was no longer in
    /// the index). With the fix, rebuilding on a dirty tree still indexes the
    /// commit's bytes, so after restoring the file the committed content is
    /// searchable again.
    #[test]
    fn base_shard_uses_commit_bytes_not_working_tree() {
        let _cache = IsolatedCache::new("base_shard_uses_commit_bytes_not_working_tree");
        let dir = std::env::temp_dir().join(format!(
            "gpx-indexset-prov-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("src.rs"), "fn committedNeedle() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);

        // Build the base shard — it must contain the committed content.
        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        let (m, _) = set.search("committedNeedle", None).unwrap();
        assert_eq!(m.len(), 1, "committed content must be indexed at HEAD");

        // Dirty the working tree, then force a base rebuild by removing the
        // shard. The OLD behavior indexed the dirty bytes as HEAD; the fix
        // indexes the commit's bytes.
        std::fs::write(dir.join("src.rs"), "fn dirtyUnrelatedContent() {}\n").unwrap();
        std::fs::remove_file(dir.join(".pixel").join("base.shard")).unwrap();
        let _set = IndexSet::open_or_build(&dir, ex()).unwrap();

        // Restore the file to its committed content (git checkout).
        git(&dir, &["checkout", "--", "src.rs"]);
        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        let (m, _) = set.search("committedNeedle", None).unwrap();
        assert_eq!(
            m.len(),
            1,
            "base shard must be git-anchored: after restore, committed content must be searchable"
        );
        // The dirty content must NOT be in the base shard.
        let (m, _) = set.search("dirtyUnrelatedContent", None).unwrap();
        assert!(
            m.is_empty(),
            "dirty working-tree content must not appear in the base shard (it was never committed)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Regression: a tracked symlink pointing outside the repository must never
    /// expose its target's content through search. The symlink is rejected at
    /// extraction (so it is not indexed) and at verification (so even if a
    /// candidate path slipped through, the target is not read).
    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_blocked() {
        use std::os::unix::fs::symlink;
        let _cache = IsolatedCache::new("symlink_escape_is_blocked");
        let dir = std::env::temp_dir().join(format!(
            "gpx-indexset-symlink-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);

        // External file with a unique needle that must never be searchable
        // through the repo's index.
        let external = std::env::temp_dir().join(format!(
            "gpx-ext-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        std::fs::write(&external, "externalSymlinkTargetNeedle\n").unwrap();

        // A regular tracked file (so the repo has at least one real file).
        std::fs::write(dir.join("real.rs"), "fn realRepoNeedle() {}\n").unwrap();
        // A symlink tracked by git that points outside the repo.
        symlink(&external, dir.join("escape.txt")).unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "with symlink"]);

        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        // The regular file is searchable.
        let (m, _) = set.search("realRepoNeedle", None).unwrap();
        assert_eq!(m.len(), 1);
        // The external needle must NOT be searchable through the symlink.
        let (m, _) = set.search("externalSymlinkTargetNeedle", None).unwrap();
        assert!(
            m.is_empty(),
            "symlink escape: external target content must not be indexed or verified"
        );

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&external).ok();
    }

    #[test]
    fn oversized_worktree_file_is_not_indexed_or_verified() {
        let _cache = IsolatedCache::new("oversized_worktree_file_is_not_indexed_or_verified");
        let dir =
            std::env::temp_dir().join(format!("gpx-indexset-oversized-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("small.rs"), "fn boundedNeedle() {}\n").unwrap();
        let mut oversized = vec![b'x'; MAX_FILE_BYTES as usize + 1];
        let needle = b"oversizedNeedle";
        oversized[..needle.len()].copy_from_slice(needle);
        std::fs::write(dir.join("large.rs"), oversized).unwrap();

        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        let (small, _) = set.search("boundedNeedle", None).unwrap();
        let (large, _) = set.search("oversizedNeedle", None).unwrap();
        assert_eq!(small.len(), 1);
        assert!(large.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Regression: a search with a row limit returns at most `limit` matches
    /// and reports `truncated` when more matches exist beyond the slice.
    #[test]
    fn search_limit_truncates() {
        let _cache = IsolatedCache::new("search_limit_truncates");
        let dir = std::env::temp_dir().join(format!(
            "gpx-indexset-limit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        // Many files, each with the same needle, so a broad pattern hits all.
        for i in 0..20 {
            std::fs::write(
                dir.join(format!("f{i:02}.rs")),
                format!("fn sharedLimitNeedle{i:02}() {{}}\n"),
            )
            .unwrap();
        }
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "many"]);

        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        // Limit of 5: at most 5 matches returned, truncated=true.
        let (m, stats) = set.search("sharedLimitNeedle", Some(5)).unwrap();
        assert!(m.len() <= 5, "limit must cap returned matches");
        assert_eq!(stats.matches, m.len());
        assert!(stats.truncated, "truncated must be true when capped");
        // No limit: all 20 matches returned, truncated=false.
        let (m, stats) = set.search("sharedLimitNeedle", None).unwrap();
        assert_eq!(m.len(), 20);
        assert!(!stats.truncated);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A test holding the guard builds and publishes its shard in its own
    /// cache, and leaves `XDG_CACHE_HOME` as it found it.
    #[test]
    fn open_or_build_should_publish_into_the_test_cache_when_the_guard_is_held() {
        let cache = IsolatedCache::new("isolated-publish");
        let dir = scratch("isolated-publish");
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.rs"), "fn isolatedNeedle() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);
        let head = git_out(&dir, &["rev-parse", "HEAD"]);

        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        assert_eq!(set.search("isolatedNeedle", None).unwrap().0.len(), 1);
        drop(set);
        let published: Vec<_> = std::fs::read_dir(cache.shards())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            published,
            [format!("{head}.{}.v1.shard", ex().id())],
            "the shard is published in the test's cache"
        );

        let previous = cache.previous.clone();
        let home = cache.home.clone();
        drop(cache);
        assert!(!home.exists(), "the test cache is removed");
        // Every holder restores the variable before releasing the lock.
        let _lock = crate::cache::CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(std::env::var_os("XDG_CACHE_HOME"), previous);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Only a base anchored to a commit this repository holds can be the
    /// start of `git diff base..HEAD`.
    #[test]
    fn delta_anchor_held_requires_an_anchor_commit_the_repository_holds() {
        let dir = scratch("anchor-held");
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.rs"), "fn anchored() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);
        let head = git_out(&dir, &["rev-parse", "HEAD"]);
        let shard_at = |oid: &str| {
            let path = dir.join(format!("{oid}.shard"));
            build_shard_from(&dir, &[], ex().as_ref(), oid, &path, true)
                .unwrap()
                .shard
        };
        assert!(delta_anchor_held(&dir, &shard_at(&head)));
        assert!(!delta_anchor_held(&dir, &shard_at(&"0".repeat(40))));
        let unanchored = dir.join("plain.shard");
        ShardBuilder::new(&ex().id()).write(&unanchored).unwrap();
        assert!(!delta_anchor_held(&dir, &Shard::open(&unanchored).unwrap()));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `reconcile_delta` itself: a diff git cannot compute is an error, never
    /// an empty change list recorded as HEAD's delta.
    #[test]
    fn reconcile_delta_should_fail_on_a_diff_git_cannot_compute() {
        let _cache = IsolatedCache::new("failed-diff");
        let dir = scratch("failed-diff");
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.rs"), "fn diffed() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);
        let head = git_out(&dir, &["rev-parse", "HEAD"]);
        let mut set = IndexSet::open_or_build(&dir, ex()).unwrap();
        let gpx = dir.join(SHARD_DIR);

        let missing = "0".repeat(40);
        let err = set.reconcile_delta(&gpx, &missing, &head).unwrap_err();
        assert!(
            matches!(&err, IndexSetError::Io(e) if e.to_string().contains("git diff")),
            "{err:?}"
        );
        let state = DeltaState::load(&gpx).expect("the open saved a state");
        assert_eq!(state.delta_oid, None, "nothing recorded as HEAD's delta");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A restored `.pixel/` can carry a base built at a commit this clone
    /// never fetched (a CI cache filled on another pull request's merge
    /// ref). `git diff base..HEAD` then fails, and reading that failure as
    /// "nothing changed" served the other commit's text as HEAD's: the base
    /// must be rebuilt at HEAD instead.
    #[test]
    fn a_base_whose_commit_is_gone_should_be_rebuilt_not_diffed_as_empty() {
        let _cache = IsolatedCache::new("gone-base");
        let dir = scratch("gone-base");
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.rs"), "fn goneNeedle() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);
        drop(IndexSet::open_or_build(&dir, ex()).unwrap());

        std::fs::write(dir.join("a.rs"), "fn headNeedle() {}\n").unwrap();
        git(&dir, &["commit", "-qa", "--amend", "-m", "one, amended"]);
        git(&dir, &["reflog", "expire", "--expire=now", "--all"]);
        git(&dir, &["gc", "-q", "--prune=now"]);

        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        assert_eq!(set.search("headNeedle", None).unwrap().0.len(), 1);
        assert!(set.search("goneNeedle", None).unwrap().0.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The phase timings are read by people deciding where a slow open or
    /// build goes: a millisecond count that rounds up, wraps or truncates
    /// would send them to the wrong phase.
    #[test]
    fn millis_counts_whole_milliseconds_and_saturates_instead_of_wrapping() {
        assert_eq!(millis(Duration::ZERO), 0);
        assert_eq!(millis(Duration::from_micros(1_999)), 1);
        assert_eq!(millis(Duration::from_millis(154_321)), 154_321);
        assert_eq!(millis(Duration::MAX), u64::MAX);
    }

    /// These names are the JSON a CI log is grepped for (`base: reused`
    /// is the line that says a restored index was used).
    #[test]
    fn layer_sources_should_spell_their_json_names() {
        assert_eq!(BaseSource::Reused.as_str(), "reused");
        assert_eq!(BaseSource::SharedCache.as_str(), "shared_cache");
        assert_eq!(BaseSource::BuiltFromGit.as_str(), "built_from_git");
        assert_eq!(BaseSource::BuiltFromWalk.as_str(), "built_from_walk");
        assert_eq!(DeltaSource::None.as_str(), "none");
        assert_eq!(DeltaSource::Reused.as_str(), "reused");
        assert_eq!(DeltaSource::Rebuilt.as_str(), "rebuilt");
    }

    /// Each open reports which layer it rebuilt: the answer to "why did a
    /// restored index still cost minutes" is the layer that was not reused.
    #[test]
    fn open_timings_should_name_the_layer_each_open_rebuilt() {
        let _cache = IsolatedCache::new("open-timings");
        let dir = scratch("open-timings");
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.rs"), "fn first() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);
        let layers = |root: &Path| {
            let t = IndexSet::open_or_build(root, ex()).unwrap().open_timings();
            (t.base, t.delta, t.overlay_files)
        };

        assert_eq!(
            layers(&dir),
            (BaseSource::BuiltFromGit, DeltaSource::None, 0)
        );
        assert_eq!(layers(&dir), (BaseSource::Reused, DeltaSource::None, 0));

        let clone = scratch("open-timings-clone");
        std::fs::remove_dir_all(&clone).ok();
        let out = Command::new("git")
            .args(["clone", "-q"])
            .arg(&dir)
            .arg(&clone)
            .output()
            .unwrap();
        assert!(out.status.success(), "clone: {out:?}");
        assert_eq!(
            layers(&clone),
            (BaseSource::SharedCache, DeltaSource::None, 0),
            "same commit, another checkout"
        );

        std::fs::write(dir.join("a.rs"), "fn second() {}\n").unwrap();
        git(&dir, &["commit", "-qam", "two"]);
        assert_eq!(layers(&dir), (BaseSource::Reused, DeltaSource::Rebuilt, 0));
        assert_eq!(layers(&dir), (BaseSource::Reused, DeltaSource::Reused, 0));

        std::fs::write(dir.join("a.rs"), "fn dirty() {}\n").unwrap();
        std::fs::write(dir.join("b.rs"), "fn untracked() {}\n").unwrap();
        assert_eq!(layers(&dir), (BaseSource::Reused, DeltaSource::Reused, 2));

        let plain = scratch("open-timings-plain");
        std::fs::write(plain.join("c.rs"), "fn plain() {}\n").unwrap();
        assert_eq!(
            layers(&plain),
            (BaseSource::BuiltFromWalk, DeltaSource::None, 0)
        );
        for d in [dir, clone, plain] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// Every test that opens an index holds [`IsolatedCache`]: one that does
    /// not writes a shard per fixture commit into the developer's real cache.
    #[test]
    fn every_test_that_opens_an_index_should_hold_an_isolated_cache() {
        let source = include_str!("indexset.rs");
        let tests = &source[source.find("\nmod tests {").unwrap()..];
        let mut unguarded = Vec::new();
        let mut opening = 0;
        for chunk in tests.split("\n    #[test]\n    fn ").skip(1) {
            let name = &chunk[..chunk.find('(').unwrap()];
            let body = &chunk[..chunk.find("\n    }\n").unwrap()];
            if body.contains("IndexSet::open_or_build") {
                opening += 1;
                if !body.contains("IsolatedCache::new(") {
                    unguarded.push(name);
                }
            }
        }
        assert!(opening >= 13, "the scan found the tests: {opening}");
        assert!(unguarded.is_empty(), "no isolated cache: {unguarded:?}");
    }

    /// `-g`/`-t` drop files before paging: a page holds `limit` KEPT
    /// matches, `truncated` says whether more kept ones exist, and `offset`
    /// counts kept matches, so paging a filtered search neither skips nor
    /// repeats one. Filtering a page after the fact returned short or empty
    /// pages while later rows matched.
    #[test]
    fn a_filtered_page_counts_kept_matches_only() {
        let _cache = IsolatedCache::new("filtered-page");
        let dir = scratch("filtered-page");
        git(&dir, &["init", "-q"]);
        for name in ["a.md", "b.rs", "c.md", "d.rs", "e.md"] {
            std::fs::write(dir.join(name), "filterPageNeedle\n").unwrap();
        }
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "fixture"]);
        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        let md = crate::path_filter::PathFilter::new(&["*.md".to_string()], &[])
            .unwrap()
            .unwrap();
        let paths =
            |page: &[MatchLine]| -> Vec<String> { page.iter().map(|m| m.path.clone()).collect() };

        let (first, stats) = set
            .search_page_filtered("filterPageNeedle", 0, Some(2), None, Some(&md))
            .unwrap();
        assert_eq!(paths(&first), ["a.md", "c.md"]);
        assert!(stats.truncated, "e.md is still to come");
        let (second, stats) = set
            .search_page_filtered("filterPageNeedle", 2, Some(2), None, Some(&md))
            .unwrap();
        assert_eq!(paths(&second), ["e.md"]);
        assert!(!stats.truncated, "no kept match is left");

        let (all, _) = set
            .search_page_filtered("filterPageNeedle", 0, Some(10), None, None)
            .unwrap();
        assert_eq!(all.len(), 5, "without a filter every file matches");
        drop(set);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Gram hashes of `text`, as the shard stores them.
    fn hashes_of(text: &[u8]) -> Vec<u64> {
        let mut hits = Vec::new();
        ex().grams(text, &mut hits);
        hits.iter().map(|hit| hit.hash).collect()
    }

    /// A shard listing a path under a committed directory link must not
    /// lead a search outside the repository: the base is forged with the
    /// right extractor and anchor, as a hostile `.pixel/` could be.
    #[test]
    fn search_should_not_read_through_a_directory_link_a_forged_shard_names() {
        let _cache = IsolatedCache::new("containment");
        let dir = scratch("containment-repo");
        let outside = scratch("containment-outside");
        std::fs::write(outside.join("secret.txt"), "fn outsideNeedle() {}\n").unwrap();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.rs"), "fn insideNeedle() {}\n").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);
        drop(IndexSet::open_or_build(&dir, ex()).unwrap());

        let head = git_out(&dir, &["rev-parse", "HEAD"]);
        let mut forged = ShardBuilder::new(&ex().id());
        forged.set_commit_oid(&head);
        forged.add_file("a.rs", hashes_of(b"fn insideNeedle() {}\n"));
        forged.add_file("link/secret.txt", hashes_of(b"fn outsideNeedle() {}\n"));
        forged.write(&dir.join(SHARD_DIR).join(SHARD_FILE)).unwrap();

        let set = IndexSet::open_or_build(&dir, ex()).unwrap();
        assert_eq!(
            set.open_timings().base,
            BaseSource::Reused,
            "the forged base is used"
        );
        let (inside, _) = set.search("insideNeedle", None).unwrap();
        assert_eq!(inside.len(), 1);
        for limit in [None, Some(10)] {
            let (outside_hits, _) = set
                .search_page_filtered("outsideNeedle", 0, limit, None, None)
                .unwrap();
            assert!(
                outside_hits.is_empty(),
                "read through the link: {outside_hits:?}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn containment_should_admit_plain_paths_inside_the_root_only() {
        let dir = scratch("containment-unit");
        let outside = scratch("containment-unit-outside");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("out")).unwrap();
        std::os::unix::fs::symlink(dir.join("src"), dir.join("in")).unwrap();
        let mut contained = Containment::new(&dir);
        assert!(contained.admits("a.rs"));
        assert!(contained.admits("src/a.rs"));
        assert!(contained.admits("in/a.rs"), "a link that stays inside");
        assert!(!contained.admits("out/a.rs"));
        assert!(!contained.admits("../a.rs"));
        assert!(!contained.admits("/etc/passwd"));
        assert!(
            !contained.admits("missing/a.rs"),
            "an unresolvable directory"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn open_or_build_should_refuse_a_pixel_dir_the_repository_tracks() {
        let _cache = IsolatedCache::new("tracked-pixel");
        let dir = scratch("tracked-pixel");
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::create_dir_all(dir.join(SHARD_DIR)).unwrap();
        std::fs::write(dir.join(SHARD_DIR).join("state.json"), "{}").unwrap();
        git(&dir, &["add", "-f", "."]);
        git(&dir, &["commit", "-qm", "one"]);
        let Err(IndexSetError::Io(error)) = IndexSet::open_or_build(&dir, ex()) else {
            panic!("a tracked .pixel must be refused");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains(".pixel/state.json"), "{error}");
        assert!(
            !dir.join(SHARD_DIR).join(SHARD_FILE).exists(),
            "nothing was built into it"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Outside git the base is reused only while the stored walk signature
    /// matches: the signature must be written (owner-only) after a build.
    #[test]
    fn a_gitless_base_should_be_reused_until_the_tree_changes() {
        use std::os::unix::fs::PermissionsExt;
        let _cache = IsolatedCache::new("plain-sig");
        let dir = scratch("plain-sig");
        std::fs::write(dir.join("a.rs"), "fn plainNeedle() {}\n").unwrap();
        let first = IndexSet::open_or_build(&dir, ex()).unwrap();
        assert_eq!(first.open_timings().base, BaseSource::BuiltFromWalk);
        drop(first);
        let sig = plain_sig_path(&dir.join(SHARD_DIR));
        let mode = std::fs::metadata(&sig).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);
        let again = IndexSet::open_or_build(&dir, ex()).unwrap();
        assert_eq!(again.open_timings().base, BaseSource::Reused);
        drop(again);
        std::fs::write(dir.join("a.rs"), "fn changedNeedle() {}\n").unwrap();
        let rebuilt = IndexSet::open_or_build(&dir, ex()).unwrap();
        assert_eq!(rebuilt.open_timings().base, BaseSource::BuiltFromWalk);
        std::fs::remove_dir_all(&dir).ok();
    }
}
