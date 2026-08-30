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

/// Walk `root` (gitignore-aware), extract grams in parallel, write the shard.
pub fn build(root: &Path, extractor: &dyn GramExtractor) -> Result<BuildStats, IndexError> {
    let started = std::time::Instant::now();

    let mut paths: Vec<PathBuf> = Vec::new();
    // `hidden(false)`: dotfiles (`.github/`, `.claude/`, `.env.example`, …)
    // are real, searchable project content and are indexed. With hidden
    // entries no longer skipped wholesale, the `ignore` crate does NOT skip
    // `.git/` on its own, so it (and our own `.pixel/` sidecar) must be
    // pruned explicitly — object/pack data and daemon db/socket files must
    // never be indexed.
    for entry in ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            name != ".git" && name != SHARD_DIR
        })
        .build()
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().is_some_and(|t| t.is_file()) {
            let p = entry.into_path();
            // Never index our own shard directory.
            if p.components().any(|c| c.as_os_str() == SHARD_DIR) {
                continue;
            }
            paths.push(p);
        }
    }
    paths.sort();

    struct FileGrams {
        rel: String,
        bytes: u64,
        hashes: Vec<u64>,
    }

    let extracted: Vec<FileGrams> = paths
        .par_iter()
        .filter_map(|path| {
            let content = read_regular_bounded(path, MAX_FILE_BYTES).ok()?;
            // Binary sniff: NUL in the first 8KiB.
            if content[..content.len().min(8192)].contains(&0) {
                return None;
            }
            let rel = path
                .strip_prefix(root)
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
        .collect();

    let mut builder = ShardBuilder::new(&extractor.id());
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
}
