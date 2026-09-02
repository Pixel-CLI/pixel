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

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::delta::{DeltaState, delta_shard_path};
use crate::gram::GramExtractor;
use crate::index::{MAX_FILE_BYTES, SHARD_DIR, SHARD_FILE, SearchStats, read_regular_bounded};
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

pub struct IndexSet {
    root: PathBuf,
    extractor: Box<dyn GramExtractor>,
    base: Shard,
    delta: Option<Shard>,
    /// Paths superseded between base OID and HEAD (from state.json).
    delta_tombstones: HashSet<String>,
    overlay: Overlay,
}

/// Paths inside our own sidecar dir are never indexed or tombstoned.
fn is_internal(rel: &str) -> bool {
    rel == SHARD_DIR || rel.starts_with(&format!("{SHARD_DIR}/"))
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
    let _ = std::fs::create_dir_all(gpx_dir);
    let _ = std::fs::write(plain_sig_path(gpx_dir), sig);
}

/// True iff `p` is a regular file (not a symlink, not a directory). Symlinks
/// are rejected even when they point at a regular file, so a tracked symlink
/// can never expose content outside the repository through the verifier.
fn is_regular_file(p: &Path) -> bool {
    std::fs::symlink_metadata(p)
        .map(|metadata| metadata.file_type().is_file() && metadata.len() <= MAX_FILE_BYTES)
        .unwrap_or(false)
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
) -> Option<(String, Vec<u64>)> {
    if gitsync::blob_size(root, commit_oid, rel)? > MAX_FILE_BYTES {
        return None;
    }
    let content = gitsync::show_blob(root, commit_oid, rel)?;
    if content.is_empty() {
        return None;
    }
    if content[..content.len().min(8192)].contains(&0) {
        return None;
    }
    let mut hits = Vec::new();
    extractor.grams(&content, &mut hits);
    let mut hashes: Vec<u64> = hits.iter().map(|h| h.hash).collect();
    hashes.sort_unstable();
    hashes.dedup();
    Some((rel.to_string(), hashes))
}

/// Build a git-anchored shard from an explicit repo-relative file list,
/// extracting every blob from the git object store at `commit_oid` (parallel).
fn build_shard_from(
    root: &Path,
    rel_paths: &[String],
    extractor: &dyn GramExtractor,
    commit_oid: &str,
    dest: &Path,
) -> Result<Shard, IndexSetError> {
    let mut extracted: Vec<(String, Vec<u64>)> = rel_paths
        .par_iter()
        .filter(|rel| !is_internal(rel))
        .filter_map(|rel| extract_blob(root, commit_oid, rel, extractor))
        .collect();
    extracted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut builder = ShardBuilder::new(&extractor.id());
    builder.set_commit_oid(commit_oid);
    for (rel, hashes) in extracted {
        builder.add_file(&rel, hashes);
    }
    builder.write(dest)?;
    Ok(Shard::open(dest)?)
}

impl IndexSet {
    /// Open the index at `root/.pixel`, (re)building layers as needed.
    pub fn open_or_build(
        root: &Path,
        extractor: Box<dyn GramExtractor>,
    ) -> Result<Self, IndexSetError> {
        let gpx_dir = root.join(SHARD_DIR);
        let base_path = gpx_dir.join(SHARD_FILE);
        let head = gitsync::rev_parse_head(root);

        // --- base layer ---
        let mut base = match Shard::open(&base_path) {
            Ok(s) if s.extractor_id() == extractor.id() => Some(s),
            _ => None,
        };
        // A git repo demands a git-anchored base; a plain-walk base (no OID)
        // cannot be delta'd against and is rebuilt.
        if head.is_some() && base.as_ref().is_some_and(|s| s.commit_oid().is_none()) {
            base = None;
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
                // Invalidate stale delta state alongside a base rebuild.
                std::fs::remove_file(delta_shard_path(&gpx_dir)).ok();
                std::fs::remove_file(crate::delta::state_path(&gpx_dir)).ok();
                match &head {
                    Some(oid) => {
                        let tracked = gitsync::ls_files(root);
                        let shard =
                            build_shard_from(root, &tracked, extractor.as_ref(), oid, &base_path)?;
                        DeltaState {
                            base_oid: oid.clone(),
                            delta_oid: None,
                            tombstones: Vec::new(),
                        }
                        .save(&gpx_dir)?;
                        shard
                    }
                    None => {
                        index::build(root, extractor.as_ref())?;
                        let shard = Shard::open(&base_path)?;
                        save_plain_sig(&gpx_dir, &plain_signature(root));
                        shard
                    }
                }
            }
        };

        let mut set = Self {
            root: root.to_path_buf(),
            extractor,
            base,
            delta: None,
            delta_tombstones: HashSet::new(),
            overlay: Overlay::new(),
        };

        // --- delta + overlay (git repos only) ---
        if let Some(head_oid) = head {
            let base_oid = set
                .base
                .commit_oid()
                .expect("git-anchored base has an oid")
                .to_string();
            if head_oid != base_oid {
                set.reconcile_delta(&gpx_dir, &base_oid, &head_oid)?;
            }
            // Dirty working tree -> overlay.
            for (xy, path) in gitsync::status_porcelain(root) {
                if is_internal(&path) {
                    continue;
                }
                if xy.contains('D') {
                    set.overlay.remove_file(&path);
                } else {
                    set.overlay
                        .refresh_file(root, &path, set.extractor.as_ref());
                }
            }
        }
        Ok(set)
    }

    /// Build or reuse the delta layer covering `base_oid..head_oid`.
    fn reconcile_delta(
        &mut self,
        gpx_dir: &Path,
        base_oid: &str,
        head_oid: &str,
    ) -> Result<(), IndexSetError> {
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
            return Ok(());
        }
        // Cumulative diff base..HEAD — simple and correct however HEAD moved.
        let diff = gitsync::diff_name_status(&self.root, base_oid, head_oid);
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
            Some(build_shard_from(
                &self.root,
                &changed,
                self.extractor.as_ref(),
                head_oid,
                &delta_path,
            )?)
        };
        DeltaState {
            base_oid: base_oid.to_string(),
            delta_oid: Some(head_oid.to_string()),
            tombstones: tombstones.clone(),
        }
        .save(gpx_dir)?;
        self.delta_tombstones = tombstones.into_iter().collect();
        Ok(())
    }

    /// Re-extract one file from disk into the in-memory overlay (tombstoning
    /// its base/delta entry). Called by the daemon watcher on file change.
    pub fn refresh_file(&mut self, rel_path: &str) {
        let root = self.root.clone();
        self.overlay
            .refresh_file(&root, rel_path, self.extractor.as_ref());
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
    use std::process::Command;

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
        assert!(out.status.success(), "git {args:?}: {:?}", out);
    }

    fn ex() -> Box<dyn GramExtractor> {
        Box::new(SparseGramExtractor::new(Crc32Weigher))
    }

    #[test]
    fn git_anchored_layers_end_to_end() {
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
    fn paths_merges_layers_and_honors_tombstones() {
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

    /// Phase 3 item 4: hidden files (dotfiles, `.github/`, `.claude/`) are
    /// real project content and must be indexed — while `.git/` and our own
    /// `.pixel/` sidecar must never be, even with the hidden filter off.
    #[test]
    fn hidden_files_are_indexed_but_git_dir_is_not() {
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
        assert_eq!(m.len(), 1, "hidden .github workflow content must be searchable");
        assert_eq!(m[0].path, ".github/workflows/x.yml");
        let (m, _) = set.search("dotfileContentNeedle", None).unwrap();
        assert_eq!(m.len(), 1, "dotfile content must be searchable");
        let (m, _) = set.search("gitInternalNeedle", None).unwrap();
        assert!(m.is_empty(), ".git/ content must never be indexed: {m:?}");
        assert!(
            set.paths().iter().all(|p| !p.starts_with(".git/") && !p.starts_with(".pixel/")),
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
        assert_eq!(m.len(), 1, "hidden-file edits must invalidate the plain signature");

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
        assert_eq!(m.len(), 1, "tracked hidden files must be searchable in a git repo");
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
}
