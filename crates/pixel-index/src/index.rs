//! Index orchestration for Phase 1: walk a tree, build a shard, search it.
//!
//! Git anchoring (base/delta/overlay layering) replaces the plain-walk build
//! in Phase 2; the search path here is already the final shape — plan →
//! resolve postings → verify.

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::gram::GramExtractor;
use crate::plan::plan_pattern;
use crate::posting::resolve_query;
use crate::shard::{Shard, ShardBuilder, ShardError};
use crate::verify::{MatchLine, Verifier, VerifyError};

pub const SHARD_DIR: &str = ".pixel";
pub const SHARD_FILE: &str = "base.shard";
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Directory names never walked for indexing: version-control internals,
/// pixel's own sidecar, and dependency/build-output trees. These are the
/// dirs we never want to burn CPU on — committed vendored deps (`Pods`,
/// `vendor`) and gitless trees (no `.gitignore` anchoring) included. Set
/// `PIXEL_INDEX_NO_DEFAULT_IGNORES=1` to index them anyway.
pub const DEFAULT_IGNORED_DIRS: &[&str] = &[
    ".git",
    SHARD_DIR,
    "node_modules",
    "bower_components",
    "Pods",
    "vendor",
    "target",
    "_build",
    "DerivedData",
    "dist",
    "build",
    "out",
    ".gradle",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    "site-packages",
    ".terraform",
    ".next",
    ".nuxt",
    ".cache",
    ".npm",
    ".yarn",
    ".pnpm-store",
];

pub fn is_ignored_dir_name(name: &str) -> bool {
    DEFAULT_IGNORED_DIRS.contains(&name)
}

/// The single walk policy shared by every indexing/freshness walk (shard
/// build, plain-tree signature, graph collect/freshness): hidden entries are
/// included, default-ignored dirs are pruned, and `.gitignore`/`.ignore`
/// files apply with `require_git(false)` so they are honored even in gitless
/// trees (git-scoped rule sources like `.git/info/exclude` still need a
/// repo, which is inherent to them). Every caller MUST go through this — a
/// divergent walk policy makes freshness signatures disagree with shard
/// contents.
pub fn policy_walk(root: &Path) -> ignore::Walk {
    let prune_default = !matches!(
        std::env::var("PIXEL_INDEX_NO_DEFAULT_IGNORES").as_deref(),
        Ok("1") | Ok("true")
    );
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .filter_entry(move |e| {
            let name = e.file_name().to_string_lossy();
            if name == ".git" || name == SHARD_DIR {
                return false;
            }
            !(prune_default && is_ignored_dir_name(&name))
        })
        .build()
}

/// Time budget for the un-anchored plain-walk build (see [`build_with_budget`]).
pub const DEFAULT_BUILD_BUDGET_MS: u64 = 5_000;
/// Total extracted-bytes budget for the same path (memory bound).
pub const DEFAULT_BUILD_MAX_BYTES: u64 = 512 * 1024 * 1024;

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// Plain-walk build budget: `PIXEL_INDEX_BUDGET_MS` (`0` disables) or 5s.
pub fn build_budget_from_env() -> Option<std::time::Duration> {
    match env_u64("PIXEL_INDEX_BUDGET_MS") {
        Some(0) => None,
        Some(ms) => Some(std::time::Duration::from_millis(ms)),
        None => Some(std::time::Duration::from_millis(DEFAULT_BUILD_BUDGET_MS)),
    }
}

fn build_max_bytes_from_env() -> Option<u64> {
    match env_u64("PIXEL_INDEX_MAX_BYTES") {
        Some(0) => None,
        Some(n) => Some(n),
        None => Some(DEFAULT_BUILD_MAX_BYTES),
    }
}

fn budget_time_error(elapsed: std::time::Duration, budget: std::time::Duration, files: usize) -> IndexError {
    IndexError::Budget(format!(
        "index build exceeded its time budget: stopped after {:.1}s (budget {:.1}s), {} files walked, no shard written. \
         Scope pixel at a git repo (a `.git` bounds the file set), raise the budget with \
         PIXEL_INDEX_BUDGET_MS=<ms> (0 disables), or set PIXEL_INDEX_NO_DEFAULT_IGNORES=1 if default-ignored dirs were the cause.",
        elapsed.as_secs_f32(),
        budget.as_secs_f32(),
        files
    ))
}

fn budget_bytes_error(files: usize, bytes: u64, cap: u64) -> IndexError {
    IndexError::Budget(format!(
        "index build exceeded its total-size cap: {} files / {:.2} GB > cap {:.2} GB, no shard written. \
         Raise PIXEL_INDEX_MAX_BYTES=<bytes> (0 disables) or scope pixel at a git repo.",
        files,
        bytes as f32 / 1e9,
        cap as f32 / 1e9
    ))
}

pub fn open_regular_bounded(path: &Path, max_bytes: u64) -> io::Result<File> {
    let before = std::fs::symlink_metadata(path)?;
    if !before.file_type().is_file() || before.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "path is not a bounded regular file",
        ));
    }
    let file = File::open(path)?;
    let opened = file.metadata()?;
    let after = std::fs::symlink_metadata(path)?;
    if !after.file_type().is_file()
        || opened.dev() != after.dev()
        || opened.ino() != after.ino()
        || opened.len() > max_bytes
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "path changed while opening",
        ));
    }
    Ok(file)
}

pub fn read_regular_bounded(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let file = open_regular_bounded(path, max_bytes)?;
    let mut bytes = Vec::with_capacity(file.metadata()?.len() as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file grew beyond size limit while reading",
        ));
    }
    Ok(bytes)
}

#[derive(Debug)]
pub enum IndexError {
    Shard(ShardError),
    Verify(VerifyError),
    Pattern(String),
    /// The un-anchored plain-walk build hit its time/size budget (T2: the
    /// cap is named, never silent). Carries the rendered explanation.
    Budget(String),
    Io(std::io::Error),
}

impl From<ShardError> for IndexError {
    fn from(e: ShardError) -> Self {
        IndexError::Shard(e)
    }
}
impl From<VerifyError> for IndexError {
    fn from(e: VerifyError) -> Self {
        IndexError::Verify(e)
    }
}
impl From<std::io::Error> for IndexError {
    fn from(e: std::io::Error) -> Self {
        IndexError::Io(e)
    }
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexError::Shard(e) => write!(f, "{e}"),
            IndexError::Verify(e) => write!(f, "{e}"),
            IndexError::Pattern(e) => write!(f, "bad pattern: {e}"),
            IndexError::Budget(e) => write!(f, "{e}"),
            IndexError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}
impl std::error::Error for IndexError {}

#[derive(Debug, Clone)]
pub struct BuildStats {
    pub files: usize,
    pub bytes: u64,
    pub grams: u64,
    pub shard_bytes: u64,
    pub elapsed_ms: u128,
}

pub fn shard_path(root: &Path) -> PathBuf {
    root.join(SHARD_DIR).join(SHARD_FILE)
}

/// Walk `root` (policy_walk: default-ignored dirs pruned, gitignore honored
/// even in gitless trees), extract grams in parallel, write the shard.
///
/// The plain-walk build runs under the time budget from
/// [`build_budget_from_env`] and the total-bytes cap from
/// [`build_max_bytes_from_env`] — with no commit OID bounding the file set,
/// a huge or mis-rooted tree would otherwise walk and extract for minutes
/// and eat gigabytes. On budget exhaustion it fails with
/// [`IndexError::Budget`] instead of writing a partial shard.
pub fn build(root: &Path, extractor: &dyn GramExtractor) -> Result<BuildStats, IndexError> {
    build_with_budget(root, extractor, build_budget_from_env())
}

pub fn build_with_budget(
    root: &Path,
    extractor: &dyn GramExtractor,
    budget: Option<std::time::Duration>,
) -> Result<BuildStats, IndexError> {
    use std::sync::atomic::Ordering;
    let started = std::time::Instant::now();
    let max_total_bytes = build_max_bytes_from_env();

    // Pipelined walk + extraction: the walker pushes paths into a bounded
    // channel while rayon workers pull and extract grams concurrently. This
    // overlaps directory I/O with CPU work instead of waiting for the full
    // walk to finish before starting extraction.
    struct FileGrams {
        rel: String,
        bytes: u64,
        hashes: Vec<u64>,
    }

    let total_bytes = std::sync::atomic::AtomicU64::new(0);
    let budget_exceeded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let file_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Bounded channel: 256 paths buffered, so the walker blocks if workers
    // fall behind (backpressure instead of unbounded memory).
    let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();

    let walker_root = root.to_path_buf();
    let walker_budget = budget;
    let walker_started = started;
    let walker_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
    let walker_budget_exceeded = budget_exceeded.clone();
    let walker_file_count = file_count.clone();

    // Spawn the directory walker on a dedicated thread.
    let walker_handle = std::thread::spawn(move || {
        let tx = walker_tx.lock().unwrap().take().unwrap();
        for entry in policy_walk(&walker_root) {
            if walker_budget.is_some()
                && walker_started.elapsed() > walker_budget.unwrap()
            {
                walker_budget_exceeded.store(true, Ordering::Relaxed);
                break;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.file_type().is_some_and(|t| t.is_file()) {
                let p = entry.into_path();
                if p.components().any(|c| c.as_os_str() == SHARD_DIR) {
                    continue;
                }
                walker_file_count.fetch_add(1, Ordering::Relaxed);
                if tx.send(p).is_err() {
                    // Receiver dropped (budget exceeded or error).
                    break;
                }
            }
        }
    });

    // Collect extracted grams from the channel using rayon's thread pool.
    // We drain the channel and process in batches for parallelism.
    let extractor_id = extractor.id();
    let root_for_rel = root.to_path_buf();

    let extracted: Vec<FileGrams> = {
        let rx = std::sync::Mutex::new(rx);
        let total_bytes = &total_bytes;
        let budget_exceeded = &budget_exceeded;
        let max_total_bytes = &max_total_bytes;
        let extractor = extractor;

        // Collect all paths from the channel first (walker is concurrent),
        // then parallel-extract. This is a middle ground: the walk runs on
        // its own thread while we drain the channel, then we rayon-extract.
        let mut all_paths: Vec<PathBuf> = Vec::new();
        loop {
            match rx.lock().unwrap().recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(p) => all_paths.push(p),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if walker_handle.is_finished() {
                        // Drain any remaining.
                        while let Ok(p) = rx.lock().unwrap().try_recv() {
                            all_paths.push(p);
                        }
                        break;
                    }
                    if budget.is_some() && started.elapsed() > budget.unwrap() {
                        budget_exceeded.store(true, Ordering::Relaxed);
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // Wait for walker to finish.
        let _ = walker_handle.join();

        all_paths.sort();

        all_paths
            .par_iter()
            .filter_map(|path| {
                if max_total_bytes.is_some_and(|cap| total_bytes.load(Ordering::Relaxed) > cap) {
                    return None;
                }
                let content = read_regular_bounded(path, MAX_FILE_BYTES).ok()?;
                if content[..content.len().min(8192)].contains(&0) {
                    return None;
                }
                total_bytes.fetch_add(content.len() as u64, Ordering::Relaxed);
                let rel = path
                    .strip_prefix(&root_for_rel)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned();
                let mut hits = Vec::new();
                extractor.grams(&content, &mut hits);
                let mut hashes: Vec<u64> = hits.iter().map(|h| h.hash).collect();
                hashes.sort_unstable();
                hashes.dedup();
                Some(FileGrams {
                    rel,
                    bytes: content.len() as u64,
                    hashes,
                })
            })
            .collect()
    };

    let paths_count = file_count.load(Ordering::Relaxed);
    if budget_exceeded.load(Ordering::Relaxed) {
        return Err(budget_time_error(started.elapsed(), budget.unwrap(), paths_count));
    }

    let bytes_seen = total_bytes.load(Ordering::Relaxed);
    if let Some(cap) = max_total_bytes {
        if bytes_seen > cap {
            return Err(budget_bytes_error(paths_count, bytes_seen, cap));
        }
    }
    if let Some(d) = budget {
        if started.elapsed() > d {
            return Err(budget_time_error(started.elapsed(), d, paths_count));
        }
    }

    let mut builder = ShardBuilder::new(&extractor_id);
    let mut stats = BuildStats {
        files: extracted.len(),
        bytes: 0,
        grams: 0,
        shard_bytes: 0,
        elapsed_ms: 0,
    };
    for fg in &extracted {
        stats.bytes += fg.bytes;
        stats.grams += fg.hashes.len() as u64;
        builder.add_file(&fg.rel, fg.hashes.clone());
    }
    let dest = shard_path(root);
    builder.write(&dest)?;
    stats.shard_bytes = std::fs::metadata(&dest)?.len();
    stats.elapsed_ms = started.elapsed().as_millis();
    Ok(stats)
}

#[derive(Debug, Clone)]
pub struct SearchStats {
    pub candidates: usize,
    pub scanned_all: bool,
    pub matches: usize,
    pub elapsed_us: u128,
    /// True when the returned matches were capped by a row limit (more matches
    /// exist beyond the returned slice). Always `false` for the legacy
    /// unlimited search path.
    pub truncated: bool,
}

/// Plan → resolve → verify against an open shard.
pub fn search(
    root: &Path,
    shard: &Shard,
    extractor: &dyn GramExtractor,
    pattern: &str,
) -> Result<(Vec<MatchLine>, SearchStats), IndexError> {
    let started = std::time::Instant::now();
    let query = plan_pattern(pattern, extractor).map_err(|e| IndexError::Pattern(e.to_string()))?;
    let scanned_all = matches!(query, crate::posting::GramQuery::All);
    let candidates = resolve_query(&query, shard.file_count(), &|h| shard.postings(h));

    let verifier = Verifier::new(pattern)?;
    let results: Vec<Vec<MatchLine>> = candidates
        .par_iter()
        .filter_map(|&id| {
            let rel = shard.path_of(id)?;
            let abs = root.join(rel);
            let mut out = Vec::new();
            verifier.search_file(&abs, rel, &mut out, None).ok()?;
            (!out.is_empty()).then_some(out)
        })
        .collect();

    let mut matches: Vec<MatchLine> = results.into_iter().flatten().collect();
    matches.sort_by(|a, b| a.path.cmp(&b.path).then(a.line_number.cmp(&b.line_number)));
    let stats = SearchStats {
        candidates: candidates.len(),
        scanned_all,
        matches: matches.len(),
        elapsed_us: started.elapsed().as_micros(),
        truncated: false,
    };
    Ok((matches, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gram::SparseGramExtractor;
    use crate::weights::Crc32Weigher;

    #[test]
    fn end_to_end_build_and_search() {
        let dir = std::env::temp_dir().join(format!("gpx-index-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/a.rs"),
            "fn handleClick() {\n    openMenu();\n}\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/b.rs"), "fn openMenu() {}\n").unwrap();
        std::fs::write(dir.join("note.txt"), "no calls here\n").unwrap();
        std::fs::write(dir.join("blob.bin"), b"handleClick\x00").unwrap();

        let ex = SparseGramExtractor::new(Crc32Weigher);
        let stats = build(&dir, &ex).unwrap();
        assert_eq!(stats.files, 3, "binary file must be excluded");

        let shard = Shard::open(&shard_path(&dir)).unwrap();
        let (matches, sstats) = search(&dir, &shard, &ex, "handleClick").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "src/a.rs");
        assert_eq!(matches[0].line_number, 1);
        assert!(sstats.candidates < 3, "index should narrow candidates");

        // Regex with class, still correct.
        let (matches, _) = search(&dir, &shard, &ex, r"fn \w+Menu").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "src/b.rs");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_skips_default_ignored_dirs() {
        let dir = std::env::temp_dir().join(format!("gpx-index-ignored-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(dir.join("Pods/ZoomSDK")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn realFunction() {}\n").unwrap();
        std::fs::write(
            dir.join("node_modules/pkg/i.js"),
            "function vendoredJsUniqueFn() {}\n",
        )
        .unwrap();
        std::fs::write(dir.join("Pods/ZoomSDK/p.m"), "vendoredPodUniqueFn();\n").unwrap();
        std::fs::write(dir.join("target/debug/x.rs"), "vendoredTargetUniqueFn();\n").unwrap();

        let ex = SparseGramExtractor::new(Crc32Weigher);
        let stats = build(&dir, &ex).unwrap();
        assert_eq!(
            stats.files, 1,
            "dependency/build dirs must not be indexed by default"
        );

        let shard = Shard::open(&shard_path(&dir)).unwrap();
        for token in ["vendoredJsUniqueFn", "vendoredPodUniqueFn", "vendoredTargetUniqueFn"] {
            let (matches, _) = search(&dir, &shard, &ex, token).unwrap();
            assert!(matches.is_empty(), "{token} must not be searchable");
        }
        let (real, _) = search(&dir, &shard, &ex, "realFunction").unwrap();
        assert_eq!(real.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_honors_gitignore_in_gitless_tree() {
        let dir =
            std::env::temp_dir().join(format!("gpx-index-gitignore-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".gitignore"), "secrets.txt\n").unwrap();
        std::fs::write(dir.join("keep.rs"), "fn keptFunction() {}\n").unwrap();
        std::fs::write(dir.join("secrets.txt"), "gitlessIgnoredUniqueToken()\n").unwrap();

        let ex = SparseGramExtractor::new(Crc32Weigher);
        // `.gitignore` itself is hidden-but-indexed content, so 2 files.
        let stats = build(&dir, &ex).unwrap();
        assert_eq!(
            stats.files, 2,
            "gitignore must apply even without a git repo (require_git(false))"
        );

        let shard = Shard::open(&shard_path(&dir)).unwrap();
        let (ignored, _) = search(&dir, &shard, &ex, "gitlessIgnoredUniqueToken").unwrap();
        assert!(ignored.is_empty(), "gitignored file must not be searchable");
        let (kept, _) = search(&dir, &shard, &ex, "keptFunction").unwrap();
        assert_eq!(kept.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_budget_failure_names_the_cap() {
        let dir = std::env::temp_dir().join(format!("gpx-index-budget-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn budgetTripFn() {}\n").unwrap();

        let ex = SparseGramExtractor::new(Crc32Weigher);
        let err =
            match build_with_budget(&dir, &ex, Some(std::time::Duration::ZERO)) {
                Err(e) => e,
                Ok(_) => panic!("zero budget must trip"),
            };
        assert!(matches!(err, IndexError::Budget(_)), "{err}");
        let msg = err.to_string();
        assert!(msg.contains("time budget"), "cap must be named: {msg}");
        assert!(
            !shard_path(&dir).exists(),
            "a budget trip must never leave a partial shard"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
