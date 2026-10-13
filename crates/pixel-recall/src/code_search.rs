// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Semantic code search over a code tree via static embeddings.
//!
//! `ask(root, query, k, max_files)` answers open-ended questions like "how is
//! authentication handled?" by embedding the question and every candidate code
//! chunk (a file cut along its symbols, [`crate::code_chunks`]), then fusing semantic ranks with a BM25 lexical rank (best chunk per
//! file); tests, configuration, data and docs rank below code unless the
//! question names them ([`FileKind`]).
//! Reuses this crate's embedding seam
//! (`Embedder` trait + `PotionEmbedder` behind the `model2vec` feature), so the
//! model downloads once into the shared recall model cache on first use and the
//! ML dependency stays behind a feature.
//!
//! Chunk vectors persist between questions in `.pixel/code-vectors/` when the
//! search root carries a pixel index ([`vector_cache_for`],
//! [`crate::code_vectors`]): a warm question embeds only the chunks whose text
//! changed, and ranks exactly as an uncached one.
//!
//! This is an AUGMENTATIVE channel, deliberately NOT a replacement for
//! `resolve`/`search`: deterministic resolution keeps its contract; `ask`
//! adds a semantic layer on top. NDCG is the gate (see
//! crates/pixel-bench/benches/ndcg_relevance.rs).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::code_vectors::{ChunkKey, Namespace, Store};
use rayon::prelude::*;

use crate::code_chunks::code_chunks;
use crate::embed::EmbedKind;

/// One ranked hit: a file matched for the question, with a representative
/// snippet (head of the best-matching chunk), cosine score and RRF ordering key.
pub struct AskHit {
    pub path: String,
    /// Compatibility alias of semantic_score (cosine), not the ordering key.
    pub score: f32,
    pub semantic_score: f32,
    pub ranking_score: f64,
    pub lexical_matches: usize,
    /// BM25 score of the file's best chunk for the question's terms.
    pub lexical_score: f64,
    /// The kind of file this is when its score was lowered for it
    /// ([`FileKind`], [`DEMOTED_WEIGHT`]); `None` for code and for a kind the
    /// question names.
    pub demoted: Option<FileKind>,
    pub query_terms: usize,
    pub snippet: String,
}

/// Largest file `ask` and the resident index read, in bytes (512 KiB).
pub const MAX_FILE_BYTES: usize = 524_288;

/// Most files a question without a `--max-files` budget embeds: a guard
/// against a runaway universe, not a budget. A question searches every
/// eligible file by default (persisted vectors make a warm question pay only
/// for the chunks that changed); above this ceiling it searches a
/// deterministic sample of it ([`sample_across_tree`]) and says so
/// ([`FileBudget::Ceiling`]). An explicit `--max-files` replaces it, above
/// or below.
///
/// Sized from measurements (2026-09-27): yespark-rails, the largest
/// repository at hand, has 11 299 eligible files, and 50 000 is the graph
/// builder's own walk ceiling (`PIXEL_GRAPH_MAX_FILES`). A home directory
/// has 347 611: uncapped, that is about 1.5 million chunks, some 1.5 GB of
/// vectors held in memory and minutes of embedding, never persisted there.
pub const UNBUDGETED_FILE_CEILING: usize = 50_000;

/// Default for `pixel search-meaning --limit`: ranked hits returned. Ten, so a
/// recall@10 measurement reads the list a user actually gets.
pub const DEFAULT_LIMIT: usize = 10;

/// Version of how `search-meaning` cuts a file into the texts it embeds. Part
/// of every persisted vector's key ([`crate::code_vectors::Namespace`]): bump
/// it, with a line below, when the chunking or the text sent to the model for
/// a chunk changes, and no vector of the old chunking is served again.
///
/// History: 1 = [`crate::embed::chunk_offsets`] windows (`embed::CHUNK_MAX`
/// bytes, `embed::CHUNK_OVERLAP` of overlap), each embedded verbatim.
/// 2 = symbol chunks ([`code_chunks`]): a file cut along its tree-sitter
/// symbols with their doc comments, pieces packed up to
/// [`crate::code_chunks::PACK_MAX`] bytes, windows only for unparsed files
/// and oversize pieces.
pub const CHUNKER_VERSION: u32 = 2;

/// Whether a question's chunk vectors persist in `.pixel/code-vectors/`, and
/// when they do not, why.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VectorCache {
    /// Not asked for: the daemon's semantic fallback embeds in memory only.
    #[default]
    Disabled,
    /// The search root carries no pixel index (a subtree, an unindexed
    /// tree): nothing is written there.
    NoIndex,
    /// The search root is the home directory, whose `.pixel` holds global
    /// state: nothing is written there.
    HomeDirectory,
    /// Read from and written to `<root>/.pixel/code-vectors/`.
    Persisted,
}

/// Where `root`'s question keeps its chunk vectors: persisted only when
/// `root` itself carries a pixel index (`.pixel/base.shard`), so a subtree
/// or an unindexed tree gets no `.pixel` written into it, and never when
/// `root` is `home`, whatever it carries.
pub fn vector_cache_for(root: &Path, home: Option<&Path>) -> VectorCache {
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if home.is_some_and(|home| canonical(home) == canonical(root)) {
        return VectorCache::HomeDirectory;
    }
    let index = root
        .join(pixel_index::index::SHARD_DIR)
        .join(pixel_index::index::SHARD_FILE);
    if index.is_file() {
        VectorCache::Persisted
    } else {
        VectorCache::NoIndex
    }
}

/// Which file limit a question ran under.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileBudget {
    /// No `--max-files` and the universe within [`UNBUDGETED_FILE_CEILING`]:
    /// every eligible file was in reach.
    #[default]
    None,
    /// The caller's `--max-files` (or the semantic fallback's own budget).
    Explicit,
    /// No `--max-files`, and more eligible files than
    /// [`UNBUDGETED_FILE_CEILING`]: a sample of that size was searched.
    Ceiling,
}

/// What one question covered: the eligible universe, the part embedded, and
/// every reason a file was left out.
#[derive(Default, serde::Serialize)]
pub struct AskCoverage {
    /// Eligible files under the root (walk policy, excluded directories,
    /// nested checkouts and extensions applied), whether searched or not.
    pub candidate_files: usize,
    /// Files actually read, chunked and embedded.
    pub searched_files: usize,
    /// The file limit in force: the explicit budget, or
    /// [`UNBUDGETED_FILE_CEILING`] when an unbudgeted question outgrew it.
    /// `None` (JSON `null`) when there was none: no `--max-files` and every
    /// eligible file in reach. `file_budget` says which of the three.
    pub max_files: Option<usize>,
    pub file_budget: FileBudget,
    /// More than `max_files` files were eligible: only a sample was searched.
    pub file_limit_reached: bool,
    pub skipped_files: usize,
    pub empty_files: usize,
    pub traversal_errors: usize,
    pub result_limit_reached: bool,
    pub scope: &'static str,
    pub degraded: bool,
    /// Chunks ranked: every searched file's symbol chunks
    /// ([`crate::code_chunks`]).
    pub chunks: usize,
    /// Chunks the model embedded for this question, each distinct text once.
    pub embedded_chunks: usize,
    /// Chunks whose vector came from `.pixel/code-vectors/`.
    pub cached_chunks: usize,
    pub vector_cache: VectorCache,
    /// Failures of the vector store (unreadable, missing or altered segment,
    /// failed write). The chunks involved were embedded again, so the answer
    /// is complete; only the saving was lost. Sets `degraded`.
    pub vector_cache_errors: Vec<String>,
}

impl AskCoverage {
    /// The line a human reader gets when the question searched a sample, so
    /// a missing answer is never passed off as an absent one. `None` when the
    /// whole eligible universe was in reach.
    pub fn sample_note(&self) -> Option<String> {
        if !self.file_limit_reached {
            return None;
        }
        let limit = self.max_files.unwrap_or_default();
        let (why, remedy) = match self.file_budget {
            FileBudget::Ceiling => (
                format!("safety ceiling of {limit} files without --max-files"),
                "pass a larger --max-files",
            ),
            FileBudget::Explicit | FileBudget::None => {
                (format!("--max-files {limit}"), "raise --max-files")
            }
        };
        Some(format!(
            "searched a deterministic sample of {} of {} eligible files ({why}); {remedy} to search them all",
            self.searched_files, self.candidate_files
        ))
    }

    /// The line a human reader gets when the vector store failed, so a slow
    /// or degraded answer carries its reason. `None` when it did not.
    pub fn vector_cache_note(&self) -> Option<String> {
        (!self.vector_cache_errors.is_empty()).then(|| {
            format!(
                "vector cache: {}; the affected chunks were embedded again",
                self.vector_cache_errors.join("; ")
            )
        })
    }
}

pub struct AskResult {
    pub hits: Vec<AskHit>,
    pub coverage: AskCoverage,
}

/// The files one question embeds, absolute, sorted by repository-relative
/// path, with the coverage of the walk.
///
/// The universe is the indexing walk policy ([`pixel_index::index::policy_walk`],
/// the walk behind `policy_file_paths`): `.gitignore`/`.ignore` honoured even
/// without git, `.git`, `.pixel` and the default-ignored build and dependency
/// directories pruned, symlinks never followed, nothing written. On top of it,
/// a path is eligible when no directory on it is a [`skip_dir`] name or the top
/// of a nested checkout, and its name is a [`is_code_file`] one. The walk is
/// iterated directly rather than through `policy_file_paths` so its errors are
/// counted in `traversal_errors` instead of vanishing, and non-UTF-8 names
/// keep their bytes.
///
/// `max_files` is the caller's budget; `None` searches every eligible file up
/// to [`UNBUDGETED_FILE_CEILING`]. Over the limit in force, the files kept
/// are a sample spread over the whole tree ([`sample_across_tree`]);
/// `candidate_files` still counts every eligible file, and
/// `file_limit_reached` and `degraded` say that the search was partial.
pub(crate) fn collect_files(root: &Path, max_files: Option<usize>) -> (Vec<PathBuf>, AskCoverage) {
    collect_files_within(root, max_files, UNBUDGETED_FILE_CEILING)
}

/// [`collect_files`] with the ceiling of an unbudgeted question as a
/// parameter, so a test can reach it without 50 000 files.
fn collect_files_within(
    root: &Path,
    max_files: Option<usize>,
    ceiling: usize,
) -> (Vec<PathBuf>, AskCoverage) {
    let mut coverage = AskCoverage {
        max_files,
        file_budget: if max_files.is_some() {
            FileBudget::Explicit
        } else {
            FileBudget::None
        },
        scope: "eligible source/document extensions; gitignored files and excluded noise directories skipped; nested checkouts skipped; no symlinks; files <=512KiB; UTF-8 text only",
        ..Default::default()
    };
    if std::fs::symlink_metadata(root).is_ok_and(|m| m.file_type().is_symlink()) {
        coverage.skipped_files += 1;
        coverage.degraded = true;
        return (Vec::new(), coverage);
    }
    let mut nested = HashMap::new();
    let mut eligible = Vec::new();
    for entry in pixel_index::index::policy_walk(root) {
        let Ok(entry) = entry else {
            coverage.traversal_errors += 1;
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        if is_eligible(root, relative, &mut nested) {
            eligible.push(relative.to_path_buf());
        }
    }
    coverage.candidate_files = eligible.len();
    let limit = max_files.unwrap_or(ceiling);
    coverage.file_limit_reached = eligible.len() > limit;
    if coverage.file_limit_reached && max_files.is_none() {
        coverage.file_budget = FileBudget::Ceiling;
        coverage.max_files = Some(ceiling);
    }
    coverage.degraded = coverage.file_limit_reached || coverage.traversal_errors > 0;
    let files = sample_across_tree(eligible, limit)
        .into_iter()
        .map(|relative| root.join(relative))
        .collect();
    (files, coverage)
}

/// Whether the walk-admitted file at `relative` (below `root`) is searched:
/// no directory on its path is a [`skip_dir`] name or another checkout's top,
/// and its name has a code or document extension. `nested` memoises the
/// checkout probe per directory, which every file below it would repeat.
fn is_eligible(root: &Path, relative: &Path, nested: &mut HashMap<PathBuf, bool>) -> bool {
    let Some(name) = relative.file_name() else {
        return false;
    };
    if !is_code_file(&name.to_string_lossy()) {
        return false;
    }
    let mut dir = PathBuf::new();
    for component in relative.parent().into_iter().flat_map(Path::components) {
        dir.push(component);
        if skip_dir(&component.as_os_str().to_string_lossy()) {
            return false;
        }
        let is_checkout = *nested
            .entry(dir.clone()) // the memo owns its key; `dir` keeps growing
            .or_insert_with(|| is_nested_checkout(&root.join(&dir)));
        if is_checkout {
            return false;
        }
    }
    true
}

/// At most `max_files` of the repository-relative `files`, sorted by path.
///
/// Over the budget, the sample keeps the files with the smallest
/// [`sample_key`] (an xxh3 hash of the relative path): every directory is
/// represented in proportion to its size, whatever its depth or its place in
/// the alphabet, where a walk cut at the budget searched only the shallowest,
/// alphabetically-first directories. A hash rather than a stride over the
/// sorted list because the sample then depends on each path alone: the same
/// tree gives the same sample on every run and from every checkout location,
/// and adding or removing one file changes at most one other member, where a
/// stride would shift every pick after the edit.
///
/// Under the budget the ordering pass and `truncate` keep every file, so one
/// code path serves both regimes.
fn sample_across_tree(mut files: Vec<PathBuf>, max_files: usize) -> Vec<PathBuf> {
    files.sort_by_cached_key(|path| (sample_key(path), path.clone()));
    files.truncate(max_files);
    files.sort();
    files
}

/// Stable, platform-independent sampling key of a repository-relative path.
fn sample_key(relative: &Path) -> u64 {
    xxhash_rust::xxh3::xxh3_64(relative.as_os_str().as_encoded_bytes())
}

/// Whether `dir` is the top of another working tree: a linked worktree or a
/// submodule (a `.git` file) or a nested clone (a `.git` directory). Its
/// files belong to that checkout, often another branch of the same project
/// (`git worktree add`, `.claude/worktrees/<name>`), and would rank copies
/// of the searched tree's own code; git does not list them either. Only
/// directories below the walk root are asked, so the root keeps its `.git`.
fn is_nested_checkout(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir.join(".git")).is_ok()
}

fn skip_dir(name: &str) -> bool {
    matches!(
        name,
        "target"
            | "node_modules"
            | ".git"
            | ".pixel"
            | "dist"
            | "build"
            | "vendor"
            | ".cache"
            | "assets"
            | "reference"
            | "examples"
            | "tests"
    )
}

fn is_code_file(name: &str) -> bool {
    let ext = name
        .rsplit('.')
        .next()
        .map(str::to_lowercase)
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "rs" | "toml"
            | "py"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "go"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "java"
            | "rb"
            | "sh"
            | "md"
            | "json"
            | "yaml"
            | "yml"
            | "sql"
            | "zig"
            | "swift"
            | "kt"
            | "css"
    )
}

/// Cosine similarity between two vectors (defensive normalize on top of the
/// embedder's built-in L2 normalization).
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = (na * nb).sqrt();
    if denom > 0.0 { dot / denom } else { 0.0 }
}

pub(crate) struct CorpusEntry {
    pub(crate) path: String,
    pub(crate) text: String,
    pub(crate) key: ChunkKey,
}

/// Answer a natural-language question over a code tree.
///
/// Returns the top `k` files ranked by max chunk cosine similarity, each with
/// a snippet from its best-matching chunk. `max_files` is an optional budget
/// ([`collect_files`]): `None` searches every eligible file.
pub fn ask(
    root: &Path,
    query: &str,
    k: usize,
    max_files: Option<usize>,
) -> Result<Vec<AskHit>, String> {
    ask_with_metadata(root, query, k, max_files).map(|result| result.hits)
}

/// Most files the semantic fallback embeds inside a daemon request. It keeps
/// a budget where `pixel search-meaning` has none: it runs inside a request,
/// in memory, and persists no vector, so every call pays for every file.
pub const SEMANTIC_FALLBACK_MAX_FILES: usize = 2000;

/// Semantic leads for a query no lexical tier answered, with what the scan
/// covered. The similarity scores do not separate related from unrelated
/// files (measured on this repository, 2026-09-14: a nonsense query's best
/// file scored 0.33, a relevant French question's 0.33 to 0.39), so callers
/// present the hits as unverified leads, never as matches.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SemanticFallback {
    /// `(repo-relative path, cosine similarity)`, best first, at most `limit`.
    pub hits: Vec<(String, f64)>,
    pub searched_files: usize,
    /// More than [`SEMANTIC_FALLBACK_MAX_FILES`] files were eligible: the
    /// scan embedded a deterministic sample of them, spread across the tree.
    pub file_limit_reached: bool,
    /// The fallback was gated off (`PIXEL_SEMANTIC_FALLBACK` unset): no
    /// model load and no corpus embed ran. Named so a caller can report
    /// "disabled" rather than "searched, found nothing".
    pub disabled: bool,
}

impl SemanticFallback {
    /// Caps a caller names in its epistemics when it shows these hits.
    pub fn caps(&self) -> Vec<String> {
        if self.disabled {
            return vec![
                "semantic fallback disabled: set PIXEL_SEMANTIC_FALLBACK=1 to enable".to_string(),
            ];
        }
        if self.hits.is_empty() {
            return Vec::new();
        }
        let mut caps = vec![SEMANTIC_LEADS_UNVERIFIED.to_string()];
        if self.file_limit_reached {
            caps.push(format!(
                "semantic fallback embedded only a deterministic sample of {} of the eligible files",
                self.searched_files
            ));
        }
        caps
    }
}

/// The cap every semantic answer names: embedding similarity ranks, it does
/// not decide relevance.
pub const SEMANTIC_LEADS_UNVERIFIED: &str = "semantic leads are unverified: embedding similarity does not separate related from unrelated files";

/// Whether the semantic fallback may run. Default OFF: a lexical miss used
/// to load the embedding model and embed up to
/// [`SEMANTIC_FALLBACK_MAX_FILES`] files inside the daemon request — the
/// worst tail on the hot path. `PIXEL_SEMANTIC_FALLBACK=1` (or `true`,
/// `yes`, `on`) re-enables it; any other value keeps it off. Read once per
/// process.
#[cfg_attr(test, mutants::skip)] // process-wide env read cached once; `fallback_flag` holds the parsing and is tested
pub fn semantic_fallback_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| fallback_flag(std::env::var("PIXEL_SEMANTIC_FALLBACK").ok().as_deref()))
}

/// Whether a `PIXEL_SEMANTIC_FALLBACK` value turns the fallback on.
fn fallback_flag(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Semantic fallback for cross-lingual concept resolution: embeds the query
/// with the multilingual `potion-code-16M-v2` model and ranks code files by
/// the best-matching chunk.
///
/// Never downloads the model: a daemon request must not block on the
/// network. With no model on disk (fetched once by `pixel search-meaning`)
/// or any other embedding error, the fallback is empty. This is not the
/// daemon's `--scope hybrid` model (`potion-code-64M-v2`); each keeps its own
/// download marker ([`crate::potion_marker`]).
///
/// Returned paths are repo-relative (stripped of `root`) so they join with
/// index paths, annotations, and evidence maps that are all relative.
///
/// Gated by [`semantic_fallback_enabled`]: default OFF, so a lexical miss
/// returns a `disabled` fallback instead of paying for the embed.
#[cfg_attr(test, mutants::skip)] // adapter over the on-disk model; `fallback_from` holds the logic and is tested
pub fn semantic_fallback(root: &Path, query: &str, limit: usize) -> SemanticFallback {
    if !semantic_fallback_enabled() {
        return SemanticFallback {
            disabled: true,
            ..Default::default()
        };
    }
    fallback_opening(root, query, limit, || open_code_embedder(false))
}

/// The fallback over `root` with the embedder `open` returns: at most
/// [`SEMANTIC_FALLBACK_MAX_FILES`] files, in memory only (a daemon request
/// must not write the store), and an empty answer on any error.
fn fallback_opening(
    root: &Path,
    query: &str,
    limit: usize,
    open: impl FnOnce() -> Result<Box<dyn crate::embed::Embedder>, String>,
) -> SemanticFallback {
    ask_opening(
        root,
        query,
        limit,
        Some(SEMANTIC_FALLBACK_MAX_FILES),
        VectorCache::Disabled,
        open,
    )
    .map_or_else(
        |_| SemanticFallback::default(),
        |result| fallback_from(root, result),
    )
}

/// The fallback answer for an ask `result` over `root`: repo-relative paths
/// with their cosine similarity, and the scan coverage.
fn fallback_from(root: &Path, result: AskResult) -> SemanticFallback {
    let root_str = root.display().to_string();
    SemanticFallback {
        hits: result
            .hits
            .into_iter()
            .map(|h| {
                let rel = h
                    .path
                    .strip_prefix(&root_str)
                    .and_then(|s| s.strip_prefix('/'))
                    .unwrap_or(&h.path)
                    .to_string();
                (rel, f64::from(h.semantic_score))
            })
            .collect(),
        searched_files: result.coverage.searched_files,
        file_limit_reached: result.coverage.file_limit_reached,
        disabled: false,
    }
}

/// [`ask`] with its coverage. Chunk vectors persist between questions when
/// [`vector_cache_for`] allows it for `root` (an indexed root that is not the
/// home directory).
#[cfg_attr(test, mutants::skip)] // adapter reading $HOME and the on-disk model; `ask_opening` and `vector_cache_for` hold the logic and are tested
pub fn ask_with_metadata(
    root: &Path,
    query: &str,
    k: usize,
    max_files: Option<usize>,
) -> Result<AskResult, String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cache = vector_cache_for(root, home.as_deref());
    ask_opening(root, query, k, max_files, cache, || {
        open_code_embedder(true)
    })
}

/// The question over `root`, with the embedder `open` returns (opened only
/// when a file is eligible) and the store `cache` names.
pub(crate) fn ask_opening(
    root: &Path,
    query: &str,
    k: usize,
    max_files: Option<usize>,
    cache: VectorCache,
    open: impl FnOnce() -> Result<Box<dyn crate::embed::Embedder>, String>,
) -> Result<AskResult, String> {
    let (files, mut coverage) = collect_files(root, max_files);
    coverage.vector_cache = cache;
    if files.is_empty() {
        return Ok(AskResult {
            hits: Vec::new(),
            coverage,
        });
    }

    let mut embedder = open()?;
    let store = (cache == VectorCache::Persisted).then(|| {
        Store::at(
            &root
                .join(pixel_index::index::SHARD_DIR)
                .join(crate::code_vectors::DIR),
        )
    });

    ask_collected(
        root,
        query,
        k,
        files,
        coverage,
        embedder.as_mut(),
        store.as_ref(),
    )
}

/// The Hugging Face repository [`open_code_embedder`] loads: one name for
/// the opener and [`warm_probe`], which reads its download marker.
const CODE_SEARCH_MODEL_REPO: &str = "minishlab/potion-code-16M-v2";

pub(crate) fn open_code_embedder(
    download: bool,
) -> Result<Box<dyn crate::embed::Embedder>, String> {
    crate::embed::open_embedder_with_potion_repo(download, Some(CODE_SEARCH_MODEL_REPO))
}

/// Whether the code-search model finished downloading, so
/// [`open_code_embedder_offline`] would not need the network.
#[cfg_attr(test, mutants::skip)] // adapter reading $HOME; `potion_set_up` holds the rule and is tested
pub fn code_model_on_disk() -> bool {
    crate::potion_set_up(&crate::models_dir(), CODE_SEARCH_MODEL_REPO)
}

/// The embedder `pixel search-meaning` ranks with, opened from the model
/// cache and never downloaded: a long-lived caller must not block on the
/// network, so it learns "not set up" from [`code_model_on_disk`] or the
/// error and reports it.
#[cfg_attr(test, mutants::skip)] // adapter over the on-disk model; no instance exists without the downloaded weights
pub fn open_code_embedder_offline() -> Result<Box<dyn crate::embed::Embedder>, String> {
    open_code_embedder(false)
}

/// Whether the semantic code-search path over a root is already warm: the
/// embedding model downloaded and the chunk vectors persisted.
///
/// [`warm_probe`] fills this without touching the model or the vectors
/// themselves, for a deadline-bounded caller deciding whether an embedding
/// operation would pay a cold start. It reports presence, never freshness
/// or completeness of the index.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct WarmProbe {
    /// The code-search model's download finished
    /// ([`crate::potion_set_up`] on [`CODE_SEARCH_MODEL_REPO`]): opening it
    /// without a download would not hit the network.
    pub model_on_disk: bool,
    /// `<root>/.pixel/code-vectors/` holds a readable manifest naming at
    /// least one vector row.
    pub vectors_present: bool,
    /// Vector rows the store's manifest names, `0` when there is no
    /// readable one.
    pub vectors_chunks: u64,
}

/// Probe whether the semantic code-search path over `root` is already warm.
///
/// Strictly read-only and cheap: it never loads a model, never embeds,
/// never downloads and never writes. `model_on_disk` reads the same
/// download markers [`open_code_embedder`] checks ([`crate::potion_set_up`]
/// on [`crate::models_dir`]), and `vectors_*` read only the store's
/// manifest — a small JSON file, atomically replaced — so `vectors_chunks`
/// counts the rows the manifest names rather than loading a single vector.
/// A missing or unreadable manifest reports `vectors_present: false`, like
/// a never-written store: [`Store::load`] would re-embed either way.
///
/// Intended for deadline-bounded callers deciding whether an embedding
/// operation would pay a cold start.
#[cfg_attr(test, mutants::skip)] // adapter reading $HOME; `warm_probe_in` holds the probe and is tested
pub fn warm_probe(root: &Path) -> WarmProbe {
    warm_probe_in(root, &crate::models_dir())
}

/// [`warm_probe`] with the model cache directory as a parameter, so a test
/// does not read `$HOME`.
fn warm_probe_in(root: &Path, models: &Path) -> WarmProbe {
    let vectors_chunks = Store::at(
        &root
            .join(pixel_index::index::SHARD_DIR)
            .join(crate::code_vectors::DIR),
    )
    .manifest_rows()
    .unwrap_or(0);
    WarmProbe {
        model_on_disk: crate::potion_set_up(models, CODE_SEARCH_MODEL_REPO),
        vectors_present: vectors_chunks > 0,
        vectors_chunks,
    }
}

/// Rank `files` for `query`: chunk them, take each chunk's vector from
/// `store` or the embedder ([`chunk_vectors`]), fuse semantic and lexical
/// ranks.
fn ask_collected(
    root: &Path,
    query: &str,
    k: usize,
    files: Vec<PathBuf>,
    mut coverage: AskCoverage,
    embedder: &mut dyn crate::embed::Embedder,
    store: Option<&Store>,
) -> Result<AskResult, String> {
    let model_id = embedder.model_id().to_string();
    let namespace = Namespace::new(
        &model_id,
        crate::embed::embedder_revision(&model_id),
        CHUNKER_VERSION,
    );
    // Build the corpus: chunk every file, key every chunk by its text, and
    // count the query's terms in each chunk for the lexical channel.
    let terms = sorted_query_terms(query);
    let mut corpus: Vec<CorpusEntry> = Vec::new();
    let mut file_paths: Vec<String> = Vec::new();
    // Every chunk's term counts, with the index of its file in `file_paths`.
    let mut lexical_chunks: Vec<(usize, pixel_rank::Bm25Doc)> = Vec::new();
    // Reading and parsing dominate a warm question: one file per task, the
    // results folded back in the files' order.
    let read: Vec<FileRead> = files
        .par_iter()
        .map(|file| read_file(root, file, &terms))
        .collect();
    for file in read {
        let (path, chunks) = match file {
            FileRead::Skipped => {
                coverage.skipped_files += 1;
                continue;
            }
            FileRead::Empty => {
                coverage.empty_files += 1;
                continue;
            }
            FileRead::Searched { path, chunks } => (path, chunks),
        };
        coverage.searched_files += 1;
        let file_index = file_paths.len();
        for (chunk, doc) in chunks {
            lexical_chunks.push((file_index, doc));
            corpus.push(CorpusEntry {
                path: path.clone(),
                key: namespace.key(&chunk),
                text: chunk,
            });
        }
        file_paths.push(path);
    }
    let lexical = lexical_evidence(&terms, file_paths, lexical_chunks);
    coverage.degraded |= coverage.skipped_files > 0;
    if corpus.is_empty() {
        return Ok(AskResult {
            hits: Vec::new(),
            coverage,
        });
    }

    // Embed the query and all chunks.
    let qvec = embedder
        .embed_batch(&[query], EmbedKind::Query)?
        .into_iter()
        .next()
        .ok_or("empty query embedding")?;
    validate_vector(&qvec, embedder.dims())?;
    let vectors = chunk_vectors(&corpus, &namespace, embedder, store, &mut coverage)?;

    // Per-file best score across its chunks.
    let mut best: HashMap<String, f32> = HashMap::new();
    let mut snippet_of: HashMap<String, String> = HashMap::new();
    for entry in &corpus {
        let vec = &vectors[&entry.key];
        validate_vector(vec, qvec.len())?;
        let s = cosine(&qvec, vec);
        if best.get(&entry.path).copied().unwrap_or(f32::MIN) < s {
            best.insert(entry.path.clone(), s);
            snippet_of.insert(entry.path.clone(), make_snippet(&entry.text));
        }
    }

    coverage.result_limit_reached = best.len() > k;
    Ok(AskResult {
        hits: rank_files(query, &lexical, best, snippet_of, k),
        coverage,
    })
}

/// The vector of every chunk of `corpus`, by key: the ones `store` holds, and
/// the rest embedded in one batch, each distinct text once, then saved.
///
/// A store failure never fails the question: the chunks it could not serve
/// are embedded like new ones, the failure is named in
/// `coverage.vector_cache_errors` and marks the answer degraded, and the next
/// save rebuilds the store. Counts go to `coverage.chunks`,
/// `embedded_chunks` and `cached_chunks`.
pub(crate) fn chunk_vectors(
    corpus: &[CorpusEntry],
    namespace: &Namespace,
    embedder: &mut dyn crate::embed::Embedder,
    store: Option<&Store>,
    coverage: &mut AskCoverage,
) -> Result<HashMap<ChunkKey, Vec<f32>>, String> {
    let dims = embedder.dims();
    let wanted: HashSet<ChunkKey> = corpus.iter().map(|entry| entry.key).collect();
    let crate::code_vectors::Loaded {
        mut vectors,
        stored_rows,
        errors,
    } = store
        .map(|store| store.load(namespace, dims, &wanted))
        .unwrap_or_default();
    let rebuild = !errors.is_empty();
    coverage.vector_cache_errors.extend(errors);
    let mut fresh = Vec::new();
    let mut texts = Vec::new();
    let mut seen = HashSet::new();
    for entry in corpus {
        if !vectors.contains_key(&entry.key) && seen.insert(entry.key) {
            fresh.push(entry.key);
            texts.push(entry.text.as_str());
        }
    }
    coverage.chunks = corpus.len();
    coverage.cached_chunks = corpus
        .iter()
        .filter(|entry| vectors.contains_key(&entry.key))
        .count();
    coverage.embedded_chunks = texts.len();
    if !texts.is_empty() {
        let embedded = embedder.embed_batch(&texts, EmbedKind::Passage)?;
        if embedded.len() != texts.len() {
            return Err(format!(
                "embedding count mismatch: {} chunks vs {} vectors",
                texts.len(),
                embedded.len()
            ));
        }
        for (key, vector) in fresh.iter().zip(embedded) {
            validate_vector(&vector, dims)?;
            vectors.insert(*key, vector);
        }
    }
    if let Some(store) = store
        && let Err(error) = store.save(namespace, dims, &fresh, &vectors, stored_rows, rebuild)
    {
        coverage.vector_cache_errors.push(error);
    }
    coverage.degraded |= !coverage.vector_cache_errors.is_empty();
    Ok(vectors)
}

/// One file of a question's universe, read and cut.
enum FileRead {
    /// Unreadable, over [`MAX_FILE_BYTES`], binary or not UTF-8.
    Skipped,
    /// Blank.
    Empty,
    /// Its repository-relative path and its chunks ([`code_chunks`]), each
    /// with its BM25 document over the question's terms.
    Searched {
        path: String,
        chunks: Vec<(String, pixel_rank::Bm25Doc)>,
    },
}

/// Read `file` (below `root`) and cut it into chunks, each counted over the
/// sorted query `terms` for the lexical channel. The embedded text and the
/// lexical document of a chunk are the same bytes, so the snippet shown for
/// a file is a chunk the lexical channel scored too.
fn read_file(root: &Path, file: &Path, terms: &[String]) -> FileRead {
    let Ok(bytes) = std::fs::read(file) else {
        return FileRead::Skipped;
    };
    if bytes.len() > MAX_FILE_BYTES || bytes.contains(&0) {
        return FileRead::Skipped;
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return FileRead::Skipped;
    };
    if text.trim().is_empty() {
        return FileRead::Empty;
    }
    let path = file
        .strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned();
    // Filenames are evidence too, but directory names and language extensions
    // must not add shared noise or change ranks when the repository moves.
    // A filename can have compound extensions (`types.d.ts`): only the
    // basename before its first dot counts, so suffix components cannot
    // become lexical evidence.
    let stem = file.file_name().map_or_else(String::new, |name| {
        let basename = name.to_string_lossy();
        basename.split('.').next().unwrap_or_default().to_string()
    });
    let (stem_freqs, stem_len) = term_counts(&stem, terms);
    let chunks = code_chunks(&path, &text)
        .into_iter()
        .map(|(start, end)| {
            // Every chunk carries the filename stem's tokens, as if the name
            // were written once at its top.
            let (mut freqs, len) = term_counts(lexical_chunk(&text, start, end), terms);
            for (freq, stem_freq) in freqs.iter_mut().zip(&stem_freqs) {
                *freq += stem_freq;
            }
            let doc = pixel_rank::Bm25Doc {
                path: String::new(),
                term_freqs: freqs,
                len: len + stem_len,
            };
            (text[start..end].to_string(), doc)
        })
        .collect();
    FileRead::Searched { path, chunks }
}

pub(crate) fn validate_vector(vector: &[f32], dims: usize) -> Result<(), String> {
    let norm: f64 = vector.iter().map(|x| f64::from(*x).powi(2)).sum();
    if dims == 0 || vector.len() != dims || !norm.is_finite() || norm <= 0.0 {
        return Err(
            "invalid embedding vector: expected finite, nonzero values with consistent dimensions"
                .into(),
        );
    }
    Ok(())
}

/// Exclude identifier fragments created only by fixed byte-window boundaries
/// (the windows of an unparsed file or an oversize piece; symbol chunks end
/// on line boundaries).
pub(crate) fn lexical_chunk(text: &str, start: usize, end: usize) -> &str {
    let is_ident = |character: char| character.is_alphanumeric() || character == '_';
    let chunk = &text[start..end];
    let chunk = if text[..start].chars().next_back().is_some_and(is_ident)
        && chunk.chars().next().is_some_and(is_ident)
    {
        chunk
            .split_once(|character| !is_ident(character))
            .map_or("", |(_, complete)| complete)
    } else {
        chunk
    };
    if text[..end].chars().next_back().is_some_and(is_ident)
        && text[end..].chars().next().is_some_and(is_ident)
    {
        chunk
            .rsplit_once(|character| !is_ident(character))
            .map_or("", |(complete, _)| complete)
    } else {
        chunk
    }
}

/// Preserve complete identifiers and split snake_case, camelCase and acronyms.
fn words(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for_each_word(text, |word| {
        out.insert(word);
    });
    out
}

/// Every token [`words`] draws from `text`, lowercased, once per occurrence:
/// each identifier, then its snake_case, camelCase and acronym parts unless
/// the only part is the identifier itself (`readHTTPResponse` gives
/// `readhttpresponse`, `read`, `http`, `response`; `_setup` gives `_setup`,
/// `setup`; `setup` gives `setup` once).
pub(crate) fn for_each_word(text: &str, mut emit: impl FnMut(String)) {
    for word in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if word.is_empty() {
            continue;
        }
        let mut parts = Vec::new();
        for part in word.split('_').filter(|s| !s.is_empty()) {
            let chars: Vec<(usize, char)> = part.char_indices().collect();
            let mut start = 0;
            for i in 1..chars.len() {
                let (_, prev) = chars[i - 1];
                let (offset, current) = chars[i];
                let next_lower = chars.get(i + 1).is_some_and(|(_, c)| c.is_lowercase());
                if (prev.is_lowercase() && current.is_uppercase())
                    || (prev.is_uppercase() && current.is_uppercase() && next_lower)
                {
                    parts.push(&part[start..offset]);
                    start = offset;
                }
            }
            parts.push(&part[start..]);
        }
        emit(word.to_lowercase());
        // A lone part equal to the identifier is that identifier again.
        if parts != [word] {
            for part in parts {
                emit(part.to_lowercase());
            }
        }
    }
}

/// How often each of `terms` occurs among the tokens of `text` (aligned with
/// `terms`), and the token count: one BM25 document.
fn term_counts(text: &str, terms: &[String]) -> (Vec<u32>, u32) {
    let mut freqs = vec![0u32; terms.len()];
    let mut len = 0u32;
    for_each_word(text, |word| {
        len += 1;
        if let Ok(index) = terms.binary_search(&word) {
            freqs[index] += 1;
        }
    });
    (freqs, len)
}

/// [`query_terms`], sorted: the term order of every lexical document.
pub(crate) fn sorted_query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = query_terms(query).into_iter().collect();
    terms.sort_unstable();
    terms
}

fn query_terms(query: &str) -> HashSet<String> {
    // Do not impose a minimum length: x, id, io and negation carry meaning.
    let normalized = query.replace("n’t", " not").replace("n't", " not");
    words(&normalized)
        .into_iter()
        .filter(|w| {
            !matches!(
                w.as_str(),
                "a" | "an"
                    | "the"
                    | "is"
                    | "are"
                    | "was"
                    | "were"
                    | "be"
                    | "been"
                    | "being"
                    | "do"
                    | "does"
                    | "did"
                    | "how"
                    | "what"
                    | "where"
                    | "when"
                    | "which"
                    | "who"
                    | "why"
                    | "can"
                    | "could"
                    | "would"
                    | "should"
                    | "please"
                    | "i"
                    | "me"
                    | "my"
                    | "we"
                    | "our"
                    | "you"
                    | "your"
                    | "it"
                    | "its"
                    | "this"
                    | "that"
                    | "these"
                    | "those"
                    | "of"
                    | "to"
                    | "for"
                    | "from"
                    | "in"
                    | "on"
                    | "at"
                    | "with"
                    | "by"
                    | "and"
                    | "or"
            )
        })
        .collect()
}

/// Each file of `files` with its [`Lexical`] evidence, from the term counts
/// of its chunks (`chunks`: the index of the chunk's file in `files`, and
/// the chunk as a BM25 document over `terms`).
///
/// A chunk, not a file, is the BM25 document: the terms must meet in one
/// place of the file to count together, as the distinct-term coverage this
/// replaced required, and the document frequency behind each term's IDF is
/// taken over every chunk of the searched universe. The file keeps its best
/// chunk's score.
fn lexical_evidence(
    terms: &[String],
    files: Vec<String>,
    chunks: Vec<(usize, pixel_rank::Bm25Doc)>,
) -> HashMap<String, Lexical> {
    let mut evidence = vec![Lexical::default(); files.len()];
    let (owners, docs): (Vec<usize>, Vec<pixel_rank::Bm25Doc>) = chunks.into_iter().unzip();
    let scores = pixel_rank::bm25_scores(terms, &docs).unwrap_or_else(|| vec![0.0; docs.len()]);
    for ((file, doc), score) in owners.into_iter().zip(&docs).zip(scores) {
        let found = &mut evidence[file];
        found.score = found.score.max(score);
        let matches = doc.term_freqs.iter().filter(|freq| **freq > 0).count();
        found.matches = found.matches.max(matches);
    }
    files.into_iter().zip(evidence).collect()
}

/// A file's lexical evidence for one question.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Lexical {
    /// BM25 score of its best chunk (0 when no query term occurs).
    pub(crate) score: f64,
    /// Most distinct query terms found together in one chunk.
    pub(crate) matches: usize,
}

/// Kinds of file that answer a question about code less often than the code
/// they test, configure or describe, and rank below it unless the question
/// names the kind ([`FileKind::named_by`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// A test or spec: under `test/`, `__tests__/`, `spec/` or `specs/`, or named
    /// `*_test.*`, `*.test.*`, `*.spec.*` or `*_spec.*`.
    Test,
    /// Configuration or data: `json`, `yaml`/`yml`, `toml`, or anything under
    /// `locales/`, `locale/` or `i18n/`.
    Config,
    /// Prose documentation: `md`.
    Docs,
}

impl FileKind {
    /// The kind of the file at the repository-relative `path`, `None` for code.
    /// Test comes first: a JSON fixture under `spec/` is a test file.
    fn of(path: &str) -> Option<Self> {
        let path = path.to_lowercase();
        let (dirs, name) = path.rsplit_once('/').unwrap_or(("", &path));
        let mut dirs = dirs.split('/');
        let ext = name.rsplit_once('.').map_or("", |(_, ext)| ext);
        if dirs
            .clone()
            .any(|dir| matches!(dir, "test" | "__tests__" | "spec" | "specs"))
            || ["_test.", ".test.", ".spec.", "_spec."]
                .iter()
                .any(|marker| name.contains(marker))
        {
            Some(Self::Test)
        } else if matches!(ext, "json" | "yaml" | "yml" | "toml")
            || dirs.any(|dir| matches!(dir, "locales" | "locale" | "i18n"))
        {
            Some(Self::Config)
        } else if ext == "md" {
            Some(Self::Docs)
        } else {
            None
        }
    }

    /// Whether a question with the query `terms` asks for this kind of file,
    /// which then ranks on its evidence alone.
    fn named_by(self, terms: &[String]) -> bool {
        terms.iter().any(|term| {
            let term = term.as_str();
            match self {
                Self::Test => matches!(term, "test" | "tests" | "testing" | "spec" | "specs"),
                Self::Config => matches!(
                    term,
                    "config"
                        | "configs"
                        | "configuration"
                        | "settings"
                        | "json"
                        | "yaml"
                        | "yml"
                        | "toml"
                        | "locale"
                        | "locales"
                        | "translation"
                        | "translations"
                        | "i18n"
                ),
                Self::Docs => matches!(
                    term,
                    "readme" | "doc" | "docs" | "documentation" | "markdown" | "md" | "changelog"
                ),
            }
        })
    }

    /// The name JSON hits carry in `demoted`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Config => "config",
            Self::Docs => "docs",
        }
    }
}

/// Factor on the fused score of a [`FileKind`] file the question does not
/// name: it keeps its place among its peers and still ranks when its
/// evidence is strong, but a source file with the same evidence goes first.
const DEMOTED_WEIGHT: f64 = 0.8;

pub(crate) fn rank_files(
    query: &str,
    lexical: &HashMap<String, Lexical>,
    best: HashMap<String, f32>,
    mut snippets: HashMap<String, String>,
    k: usize,
) -> Vec<AskHit> {
    let terms = sorted_query_terms(query);
    let lexical_ranks = competition_ranks(
        lexical
            .values()
            .map(|evidence| pixel_rank::bm25_fixed(evidence.score)),
    );
    let mut semantic: Vec<_> = best.into_iter().collect();
    semantic.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut hits: Vec<_> = semantic
        .into_iter()
        .enumerate()
        .map(|(index, (path, score))| {
            let evidence = lexical.get(&path).copied().unwrap_or_default();
            let fixed = pixel_rank::bm25_fixed(evidence.score);
            // Competition ranks: equal scores receive exactly equal evidence weight.
            let fused = 2.0 / (60.0 + (index + 1) as f64)
                + if fixed == 0 {
                    0.0
                } else {
                    lexical_ranks
                        .get(&fixed)
                        .map_or(0.0, |&rank| 1.0 / (60.0 + rank as f64))
                };
            let demoted = FileKind::of(&path).filter(|kind| !kind.named_by(&terms));
            AskHit {
                snippet: snippets.remove(&path).unwrap_or_default(),
                path,
                score,
                semantic_score: score,
                ranking_score: if demoted.is_some() {
                    fused * DEMOTED_WEIGHT
                } else {
                    fused
                },
                lexical_matches: evidence.matches,
                lexical_score: evidence.score,
                demoted,
                query_terms: terms.len(),
            }
        })
        .collect();
    hits.sort_by(|a, b| {
        b.ranking_score
            .total_cmp(&a.ranking_score)
            .then(a.path.cmp(&b.path))
    });
    hits.truncate(k);
    hits
}

/// The competition rank of every value in `values`: one plus the number of
/// values strictly greater, so equal values share a rank and the next value
/// skips the tied ones (5, 3, 3, 1 rank 1, 2, 2, 4). One pass over a
/// histogram of the values: a scan of every file per ranked file cost 4 s at
/// the 50 000-file ceiling.
fn competition_ranks<T: Ord + std::hash::Hash + Copy>(
    values: impl Iterator<Item = T>,
) -> HashMap<T, usize> {
    let mut counts = BTreeMap::new();
    for value in values {
        *counts.entry(value).or_insert(0usize) += 1;
    }
    let mut ranks = HashMap::with_capacity(counts.len());
    let mut above = 0;
    for (value, count) in counts.into_iter().rev() {
        ranks.insert(value, 1 + above);
        above += count;
    }
    ranks
}

pub(crate) fn make_snippet(text: &str) -> String {
    let joined: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cut = joined;
    if cut.len() > 160 {
        // Truncate at a char boundary to avoid a multi-byte-char panic.
        let boundary = (0..=160)
            .rev()
            .find(|&i| cut.is_char_boundary(i))
            .unwrap_or(0);
        cut.truncate(boundary);
        cut.push('…');
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rank `entries` (path, chunk text, cosine), one chunk each, through
    /// the production lexical evidence and fusion.
    fn rank(query: &str, entries: &[(&str, &str, f32)]) -> Vec<AskHit> {
        let terms = sorted_query_terms(query);
        let mut files: Vec<String> = Vec::new();
        let mut chunks = Vec::new();
        for (path, text, _) in entries {
            let index = files
                .iter()
                .position(|file| file == path)
                .unwrap_or_else(|| {
                    files.push(path.to_string());
                    files.len() - 1
                });
            let (term_freqs, len) = term_counts(text, &terms);
            let doc = pixel_rank::Bm25Doc {
                path: String::new(),
                term_freqs,
                len,
            };
            chunks.push((index, doc));
        }
        let lexical = lexical_evidence(&terms, files, chunks);
        let best = entries
            .iter()
            .map(|(path, _, score)| (path.to_string(), *score))
            .collect();
        rank_files(query, &lexical, best, HashMap::new(), usize::MAX)
    }

    fn lexical(score: f64, matches: usize) -> Lexical {
        Lexical { score, matches }
    }

    /// Competition ranking, exactly: ties share the rank of the first of
    /// them and the next value skips past all of them, and a value's rank
    /// counts files (not distinct values) above it.
    #[test]
    fn competition_ranks_share_ties_and_skip_past_them() {
        let ranks = competition_ranks([3, 1, 3, 0, 2, 1, 3].into_iter());
        let expected: HashMap<usize, usize> = [(3, 1), (2, 4), (1, 5), (0, 7)].into();
        assert_eq!(ranks, expected);
        assert!(competition_ranks(std::iter::empty::<usize>()).is_empty());
        assert_eq!(competition_ranks([4].into_iter()), [(4, 1)].into());
    }

    /// The lexical half of the fused score uses those ranks: two files
    /// tied on BM25 score get the same lexical weight, the next one the rank
    /// after both, and a file with no score none at all.
    #[test]
    fn rank_files_gives_tied_scores_the_same_lexical_weight() {
        let coverage: HashMap<String, Lexical> = [
            ("a", lexical(2.5, 2)),
            ("b", lexical(2.5, 2)),
            ("c", lexical(1.0, 1)),
            ("d", lexical(0.0, 0)),
        ]
        .map(|(path, value)| (path.to_string(), value))
        .into();
        let best: HashMap<String, f32> = [("a", 0.9), ("b", 0.8), ("c", 0.7), ("d", 0.6)]
            .map(|(path, score)| (path.to_string(), score))
            .into();
        let hits = rank_files("x y", &coverage, best, HashMap::new(), usize::MAX);
        let score = |path: &str| {
            hits.iter()
                .find(|hit| hit.path == path)
                .map(|hit| hit.ranking_score)
                .unwrap()
        };
        let semantic = |position: f64| 2.0 / (60.0 + position);
        assert_eq!(score("a").to_bits(), (semantic(1.0) + 1.0 / 61.0).to_bits());
        assert_eq!(score("b").to_bits(), (semantic(2.0) + 1.0 / 61.0).to_bits());
        assert_eq!(score("c").to_bits(), (semantic(3.0) + 1.0 / 63.0).to_bits());
        assert_eq!(score("d").to_bits(), semantic(4.0).to_bits());
    }

    #[test]
    fn collector_is_deterministic_capped_and_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["c.rs", "a.rs", "b.rs"] {
            std::fs::write(dir.path().join(name), "setup").unwrap();
        }
        std::fs::create_dir(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/ignored.rs"), "setup").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();
        let (files, coverage) = collect_files(dir.path(), Some(2));
        assert_eq!(files.len(), 2);
        assert!(files[0] < files[1], "sorted by path: {files:?}");
        assert_eq!(
            files,
            collect_files(dir.path(), Some(2)).0,
            "same tree, same sample"
        );
        assert_eq!(coverage.candidate_files, 3, "the universe, not the sample");
        assert!(coverage.file_limit_reached && coverage.degraded);
        let (files, coverage) = collect_files(dir.path(), Some(3));
        assert_eq!(
            files,
            ["a.rs", "b.rs", "c.rs"].map(|name| dir.path().join(name)),
            "exactly the budget: everything, nothing through the symlink"
        );
        assert!(!coverage.file_limit_reached && !coverage.degraded);
        let (files, coverage) = collect_files(dir.path(), Some(0));
        assert!(files.is_empty() && coverage.file_limit_reached);
        let (_, coverage) = collect_files(&dir.path().join("missing"), Some(3));
        assert_eq!(coverage.traversal_errors, 1);
        assert!(coverage.degraded);
    }

    #[test]
    fn collector_should_skip_nested_checkouts_when_walking_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // The searched repository itself is a checkout: its `.git` must not
        // hide its own files.
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn own() {}").unwrap();
        // A linked worktree (`.git` file), as `.claude/worktrees/<name>` is.
        let worktree = root.join(".claude/worktrees/feature");
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: /elsewhere\n").unwrap();
        std::fs::write(worktree.join("src/lib.rs"), "fn own() {}").unwrap();
        // A nested clone (`.git` directory).
        let clone = root.join("third_party/dep");
        std::fs::create_dir_all(clone.join(".git")).unwrap();
        std::fs::write(clone.join("dep.rs"), "fn dep() {}").unwrap();
        // A plain directory next to them is still walked.
        std::fs::create_dir_all(root.join(".claude/hooks")).unwrap();
        std::fs::write(root.join(".claude/hooks/guard.py"), "def guard(): pass").unwrap();

        let (files, coverage) = collect_files(root, Some(10));
        let rel: Vec<_> = files
            .iter()
            .map(|f| f.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(rel, [".claude/hooks/guard.py", "src/lib.rs"]);
        assert_eq!(coverage.candidate_files, 2);
        assert_eq!(coverage.max_files, Some(10));
        assert_eq!(coverage.file_budget, FileBudget::Explicit);
        // The coverage tells the caller what was left out of the search.
        assert!(
            coverage.scope.contains("nested checkouts skipped"),
            "{}",
            coverage.scope
        );
        assert!(!coverage.degraded, "{:?}", coverage.traversal_errors);
    }

    #[cfg(all(not(feature = "model2vec"), not(feature = "fastembed")))]
    #[test]
    fn unavailable_model_is_error_without_mutating_recall_choice() {
        let before = std::env::var_os("PIXEL_RECALL_MODEL_REPO");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("manual.md"), "manual setup").unwrap();
        let error = ask_with_metadata(dir.path(), "manual setup", 8, Some(100))
            .err()
            .unwrap();
        assert!(error.contains("feature"));
        assert_eq!(std::env::var_os("PIXEL_RECALL_MODEL_REPO"), before);
    }

    fn hit(path: &str, semantic_score: f32) -> AskHit {
        AskHit {
            path: path.to_string(),
            score: semantic_score,
            semantic_score,
            ranking_score: 0.0,
            lexical_matches: 0,
            lexical_score: 0.0,
            demoted: None,
            query_terms: 0,
            snippet: String::new(),
        }
    }

    #[test]
    fn fallback_from_makes_paths_repo_relative_and_keeps_the_coverage() {
        let (_, mut coverage) = collect_files(Path::new("/nonexistent-root"), Some(3));
        coverage.searched_files = 7;
        coverage.file_limit_reached = true;
        let result = AskResult {
            hits: vec![hit("/repo/src/a.rs", 0.5), hit("elsewhere/b.rs", 0.25)],
            coverage,
        };
        let fallback = fallback_from(Path::new("/repo"), result);
        assert_eq!(
            fallback.hits,
            [
                ("src/a.rs".to_string(), 0.5),
                ("elsewhere/b.rs".to_string(), 0.25)
            ]
        );
        assert_eq!(fallback.searched_files, 7);
        assert!(fallback.file_limit_reached);
    }

    /// The caps travel with the hits: none without a lead, the unverified
    /// warning with any, and the scan cap when the file limit stopped it.
    #[test]
    fn semantic_fallback_caps_follow_the_hits_and_the_scan() {
        assert!(SemanticFallback::default().caps().is_empty());
        let mut fallback = SemanticFallback {
            hits: vec![("a.rs".to_string(), 0.3)],
            searched_files: 2000,
            file_limit_reached: false,
            disabled: false,
        };
        let caps = fallback.caps();
        assert_eq!(caps.len(), 1);
        assert!(caps[0].starts_with("semantic leads are unverified"));
        fallback.file_limit_reached = true;
        assert_eq!(
            fallback.caps()[1],
            "semantic fallback embedded only a deterministic sample of 2000 of the eligible files"
        );
    }

    #[test]
    fn fallback_flag_should_accept_only_the_enabling_spellings() {
        for on in ["1", "true", " YES ", "On"] {
            assert!(fallback_flag(Some(on)), "{on}");
        }
        for off in ["0", "false", "", "enabled"] {
            assert!(!fallback_flag(Some(off)), "{off}");
        }
        assert!(!fallback_flag(None));
    }

    #[test]
    fn cosine_should_normalize_both_vectors() {
        let c = cosine(&[1.0, 2.0], &[3.0, 4.0]);
        assert!((c - 11.0 / 125f32.sqrt()).abs() < 1e-6, "{c}");
        assert!((cosine(&[3.0, 0.0], &[1.0, 1.0]) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        assert!(cosine(&[0.0, 0.0], &[1.0, 1.0]).abs() < f32::EPSILON);
        assert!(cosine(&[1.0], &[1.0, 1.0]).abs() < f32::EPSILON);
    }

    #[test]
    fn make_snippet_should_cut_long_text_at_a_char_boundary() {
        assert_eq!(make_snippet("a  b\n c"), "a b c");
        let exact = "x".repeat(160);
        assert_eq!(make_snippet(&exact), exact);
        let long = "y".repeat(200);
        assert_eq!(make_snippet(&long), format!("{}…", "y".repeat(160)));
        let wide = format!("{}é{}", "z".repeat(159), "z".repeat(50));
        assert_eq!(make_snippet(&wide), format!("{}…", "z".repeat(159)));
    }

    /// Among chunks of a file that score the same, the first one gives the
    /// snippet.
    #[test]
    fn ask_snippet_should_come_from_the_first_of_equally_scored_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!("{}{}", "firstpart ".repeat(400), "secondpart ".repeat(400));
        std::fs::write(dir.path().join("big.md"), body).unwrap();
        let (files, coverage) = collect_files(dir.path(), Some(10));
        let result = ask_collected(
            dir.path(),
            "anything",
            8,
            files,
            coverage,
            &mut FixtureEmbedder { fail: false },
            None,
        )
        .unwrap();
        assert_eq!(result.hits.len(), 1);
        assert!(
            result.hits[0].snippet.starts_with("firstpart firstpart"),
            "{}",
            result.hits[0].snippet
        );
    }

    struct FixtureEmbedder {
        fail: bool,
    }
    impl crate::embed::Embedder for FixtureEmbedder {
        fn model_id(&self) -> &str {
            "fixture"
        }
        fn dims(&self) -> usize {
            2
        }
        fn embed_batch(&mut self, texts: &[&str], _: EmbedKind) -> Result<Vec<Vec<f32>>, String> {
            if self.fail {
                return Err("fixture model unavailable".into());
            }
            Ok(texts
                .iter()
                .map(|text| {
                    if text.trim().is_empty() {
                        vec![0.0, 0.0]
                    } else {
                        vec![1.0, 0.0]
                    }
                })
                .collect())
        }
    }

    #[test]
    fn empty_source_files_do_not_fail_valid_repository_search() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("__init__.py"), "").unwrap();
        std::fs::write(dir.path().join("blank.rs"), " \n\t").unwrap();
        std::fs::write(dir.path().join("manual.md"), "manual setup").unwrap();
        let (files, coverage) = collect_files(dir.path(), Some(10));
        let result = ask_collected(
            dir.path(),
            "manual setup",
            8,
            files,
            coverage,
            &mut FixtureEmbedder { fail: false },
            None,
        )
        .unwrap();
        assert_eq!(result.hits.len(), 1);
        assert!(result.hits[0].path.ends_with("manual.md"));
        assert_eq!(result.coverage.searched_files, 1);
        assert_eq!(result.coverage.empty_files, 2);
        assert!(!result.coverage.degraded);
    }

    #[test]
    fn lexical_chunk_excludes_only_identifiers_cut_by_the_window() {
        let text = "prefix manual setup suffix";
        assert_eq!(lexical_chunk(text, 3, 19), "manual setup");
        assert_eq!(lexical_chunk(text, 7, 23), "manual setup");
        assert_eq!(lexical_chunk(text, 6, 20), " manual setup ");
        assert_eq!(lexical_chunk(text, 1, 4), "");
    }

    #[test]
    fn chunk_boundaries_cannot_invent_lexical_words() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("noise.rs"),
            format!("{}manually", " ".repeat(1494)),
        )
        .unwrap();
        let (files, coverage) = collect_files(dir.path(), Some(10));
        let result = ask_collected(
            dir.path(),
            "manual",
            8,
            files,
            coverage,
            &mut FixtureEmbedder { fail: false },
            None,
        )
        .unwrap();
        assert_eq!(result.hits[0].lexical_matches, 0);
    }

    #[test]
    fn lexical_evidence_must_cooccur_in_one_chunk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("large.rs"),
            format!("manual{}setup", " unrelated".repeat(300)),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("focused.rs"),
            "manual setup belongs together",
        )
        .unwrap();
        let (files, coverage) = collect_files(dir.path(), Some(10));
        let result = ask_collected(
            dir.path(),
            "manual setup",
            8,
            files,
            coverage,
            &mut FixtureEmbedder { fail: false },
            None,
        )
        .unwrap();
        let large = result
            .hits
            .iter()
            .find(|hit| hit.path.ends_with("large.rs"))
            .unwrap();
        let focused = result
            .hits
            .iter()
            .find(|hit| hit.path.ends_with("focused.rs"))
            .unwrap();
        assert_eq!(large.lexical_matches, 1);
        assert_eq!(focused.lexical_matches, 2);
        assert!(focused.ranking_score > large.ranking_score);
    }

    #[test]
    fn filename_stem_evidence_ignores_extensions_and_root_location() {
        let temporary = tempfile::tempdir().unwrap();
        let mut observed_scores = Vec::new();
        for directory in ["graph", "unrelated-location"] {
            let root = temporary.path().join(directory);
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("imports.rs"), "fn parse_items() {}").unwrap();
            let (files, coverage) = collect_files(&root, Some(10));
            let result = ask_collected(
                &root,
                "imports graph rs",
                8,
                files,
                coverage,
                &mut FixtureEmbedder { fail: false },
                None,
            )
            .unwrap();
            assert_eq!(
                result.hits[0].lexical_matches, 1,
                "only imports is evidence, not the directory or extension"
            );
            observed_scores.push(result.hits[0].ranking_score);
        }
        assert_eq!(observed_scores[0], observed_scores[1]);
    }

    #[test]
    fn filename_stem_ignores_compound_extensions() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        std::fs::write(root.join("types.d.ts"), "unrelated body").unwrap();
        let (files, coverage) = collect_files(root, Some(10));
        let result = ask_collected(
            root,
            "d",
            8,
            files,
            coverage,
            &mut FixtureEmbedder { fail: false },
            None,
        )
        .unwrap();
        assert_eq!(
            result.hits[0].lexical_matches, 0,
            "compound extension components must not become filename evidence"
        );
    }

    #[test]
    fn filename_components_join_content_coverage_without_substrings_or_duplicates() {
        let temporary = tempfile::tempdir().unwrap();
        for (filename, body, query, expected) in [
            ("manually.rs", "plain module", "manual", 0),
            (
                "readHTTPResponse.rs",
                "read read http",
                "read http response",
                3,
            ),
            (
                "manual_setup.rs",
                "manual setup setup",
                "manual setup setup",
                2,
            ),
        ] {
            let root = temporary.path().join(filename);
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join(filename), body).unwrap();
            let (files, coverage) = collect_files(&root, Some(10));
            let result = ask_collected(
                &root,
                query,
                8,
                files,
                coverage,
                &mut FixtureEmbedder { fail: false },
                None,
            )
            .unwrap();
            assert_eq!(result.hits[0].lexical_matches, expected, "{filename}");
        }
    }

    struct InvalidEmbedder {
        vector: Vec<f32>,
    }
    impl crate::embed::Embedder for InvalidEmbedder {
        fn model_id(&self) -> &str {
            "invalid"
        }
        fn dims(&self) -> usize {
            2
        }
        fn embed_batch(&mut self, texts: &[&str], _: EmbedKind) -> Result<Vec<Vec<f32>>, String> {
            Ok(texts.iter().map(|_| self.vector.clone()).collect())
        }
    }

    #[test]
    fn invalid_model_vectors_are_errors_not_zero_quality_results() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("manual.md"), "manual setup").unwrap();
        for vector in [vec![], vec![0.0, 0.0], vec![f32::NAN, 1.0], vec![1.0]] {
            let (files, coverage) = collect_files(dir.path(), Some(10));
            let result = ask_collected(
                dir.path(),
                "manual setup",
                8,
                files,
                coverage,
                &mut InvalidEmbedder { vector },
                None,
            );
            assert!(
                result.is_err(),
                "invalid vectors must not become successful rankings"
            );
        }
    }

    #[test]
    fn real_files_flow_through_ranking_and_truthful_coverage() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("manual.rs"), "manual setup").unwrap();
        std::fs::write(dir.path().join("noise.rs"), "manually setup").unwrap();
        std::fs::write(dir.path().join("invalid.rs"), [0xff]).unwrap();
        std::fs::write(dir.path().join("binary.rs"), [0]).unwrap();
        let (files, coverage) = collect_files(dir.path(), Some(10));
        let result = ask_collected(
            dir.path(),
            "manual setup",
            1,
            files,
            coverage,
            &mut FixtureEmbedder { fail: false },
            None,
        )
        .unwrap();
        assert!(result.hits[0].path.ends_with("manual.rs"));
        assert_eq!(result.coverage.candidate_files, 4);
        assert_eq!(result.coverage.searched_files, 2);
        assert_eq!(result.coverage.skipped_files, 2);
        assert!(result.coverage.degraded && result.coverage.result_limit_reached);
        let (files, coverage) = collect_files(dir.path(), Some(10));
        let err = ask_collected(
            dir.path(),
            "manual setup",
            1,
            files,
            coverage,
            &mut FixtureEmbedder { fail: true },
            None,
        );
        assert_eq!(err.err().unwrap(), "fixture model unavailable");
    }

    #[test]
    fn lexical_coverage_is_not_substrings_or_chunk_frequency() {
        let hits = rank(
            "manual setup",
            &[
                ("guide", "manual setup", 0.5),
                ("noise", "manually setup", 0.5),
                ("noise", "manually setup", 0.5),
            ],
        );
        assert_eq!(hits[0].path, "guide");
        assert_eq!(hits[0].lexical_matches, 2);
        assert_eq!(hits[1].lexical_matches, 1);
    }

    #[test]
    fn duplicate_terms_and_chunks_do_not_change_rank() {
        let once = rank(
            "manual setup",
            &[("a", "manual setup", 0.9), ("b", "setup", 0.8)],
        );
        let twice = rank(
            "manual manual setup",
            &[
                ("a", "manual setup", 0.9),
                ("a", "manual setup", 0.9),
                ("b", "setup", 0.8),
            ],
        );
        assert_eq!(
            once.iter().map(|h| h.ranking_score).collect::<Vec<_>>(),
            twice.iter().map(|h| h.ranking_score).collect::<Vec<_>>()
        );
    }

    #[test]
    fn equal_lexical_coverage_has_equal_contribution() {
        let hits = rank(
            "setup",
            &[
                ("z", "setup", 0.9),
                ("a", "setup", 0.8),
                ("b", "nothing", 0.7),
            ],
        );
        assert_eq!(hits[0].path, "z");
        assert!((hits[0].ranking_score - 2.0 / 61.0 - 1.0 / 61.0).abs() < 1e-12);
        assert!((hits[1].ranking_score - 2.0 / 62.0 - 1.0 / 61.0).abs() < 1e-12);
        assert!((hits[2].ranking_score - 2.0 / 63.0).abs() < 1e-12);
    }

    #[test]
    fn identifiers_negation_and_short_terms_survive() {
        let tokens = words("readHTTPResponse snake_case userID io x");
        for word in [
            "read",
            "http",
            "response",
            "snake",
            "case",
            "user",
            "id",
            "io",
            "x",
            "snake_case",
        ] {
            assert!(tokens.contains(word), "missing {word}");
        }
        let terms = query_terms("How can I not use io x id without setup?");
        for word in ["not", "use", "io", "x", "id", "without", "setup"] {
            assert!(terms.contains(word));
        }
        for word in ["how", "can", "i"] {
            assert!(!terms.contains(word));
        }
        assert_eq!(query_terms("How can I please setup?"), query_terms("setup"));
        assert!(query_terms("I don't want setup").contains("not"));
        assert!(query_terms("I don’t want setup").contains("not"));
    }

    #[test]
    fn ordering_is_deterministic_and_scores_are_truthful() {
        let hits = rank("setup", &[("z", "setup", 0.8), ("a", "setup", 0.8)]);
        assert_eq!(hits[0].path, "a");
        for hit in hits {
            assert_eq!(hit.score, hit.semantic_score);
            assert_ne!(hit.score as f64, hit.ranking_score);
        }
    }

    /// Every file of `tree` (relative path, content) asked `query` through
    /// the production ranking, with a model that scores every chunk alike:
    /// the semantic channel then orders by path, so only the lexical channel
    /// and the file-kind weight can move a file.
    fn ask_tree(tree: &[(&str, &str)], query: &str) -> Vec<AskHit> {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in tree {
            write(dir.path(), path, text);
        }
        let (files, coverage) = collect_files(dir.path(), None);
        ask_collected(
            dir.path(),
            query,
            usize::MAX,
            files,
            coverage,
            &mut FixtureEmbedder { fail: false },
            None,
        )
        .unwrap()
        .hits
    }

    fn found<'a>(hits: &'a [AskHit], path: &str) -> &'a AskHit {
        hits.iter().find(|hit| hit.path == path).unwrap()
    }

    fn position(hits: &[AskHit], path: &str) -> usize {
        hits.iter().position(|hit| hit.path == path).unwrap()
    }

    /// IDF: a term most files carry says little, a term one file carries
    /// says a lot. `a_noisy.rs` repeats the common term forty times,
    /// `b_rare.rs` names the rare one once; the distinct-term count this
    /// replaced tied them (one term each, both lexical rank 1), BM25 gives
    /// the rare file rank 1 and the noisy file rank 2, exactly.
    #[test]
    fn rare_term_file_beats_a_file_repeating_a_common_term() {
        let noisy = "fn handler() {}\n".repeat(40);
        let hits = ask_tree(
            &[
                ("a_noisy.rs", &noisy),
                ("b_rare.rs", "fn settle_ledger() {}"),
                ("c.rs", "fn handler() {}"),
                ("d.rs", "fn handler() {}"),
                ("e.rs", "fn handler() {}"),
            ],
            "ledger handler",
        );
        let (noisy, rare) = (found(&hits, "a_noisy.rs"), found(&hits, "b_rare.rs"));
        assert_eq!((noisy.lexical_matches, rare.lexical_matches), (1, 1));
        assert!(rare.lexical_score > noisy.lexical_score);
        // Path order puts the noisy file first semantically (tied cosine);
        // lexically the rare file is first and the noisy one second.
        assert_eq!(
            rare.ranking_score.to_bits(),
            (2.0 / 62.0 + 1.0 / 61.0_f64).to_bits()
        );
        assert_eq!(
            noisy.ranking_score.to_bits(),
            (2.0 / 61.0 + 1.0 / 62.0_f64).to_bits()
        );
        let common = found(&hits, "c.rs").lexical_score;
        assert!(noisy.lexical_score > common, "tf still counts, saturated");
        assert!(
            noisy.lexical_score < 2.0 * common,
            "forty occurrences must not weigh forty times one: {} vs {common}",
            noisy.lexical_score
        );
    }

    /// Length normalisation: a long file that says the term thirty times,
    /// scattered through a lot of other text, does not outscore a short file
    /// that is about the term, on volume alone.
    #[test]
    fn a_huge_file_does_not_win_on_volume() {
        let mut huge = String::new();
        for line in 0..240 {
            let word = if line % 8 == 0 { "ledger" } else { "filler" };
            huge.push_str(&format!(
                "// {word} entry {line:03} with many other words around it here\n"
            ));
        }
        assert_eq!(huge.matches("ledger").count(), 30);
        let hits = ask_tree(
            &[
                ("a_huge.rs", &huge),
                ("b_small.rs", "fn ledger() {}"),
                ("c.rs", "fn unrelated() {}"),
            ],
            "ledger",
        );
        let (huge, small) = (found(&hits, "a_huge.rs"), found(&hits, "b_small.rs"));
        assert_eq!((huge.lexical_matches, small.lexical_matches), (1, 1));
        assert!(
            small.lexical_score > huge.lexical_score,
            "{} vs {}",
            small.lexical_score,
            huge.lexical_score
        );
    }

    /// A test file loses to the source it tests when the question does not
    /// ask for tests, although it sorts first and carries more of the
    /// question's words; named in the question ("test"), it ranks on its
    /// evidence and wins. The weight is reported on the hit.
    #[test]
    fn a_test_file_loses_to_its_source_unless_the_question_names_tests() {
        let tree = [
            (
                "__tests__/invoice.test.ts",
                "generateInvoice(); // generate invoice lines",
            ),
            ("src/invoice.ts", "export function generateInvoice() {}"),
        ];
        let plain = ask_tree(&tree, "generate invoice");
        assert_eq!(plain[0].path, "src/invoice.ts");
        assert_eq!(plain[1].demoted, Some(FileKind::Test));
        assert_eq!(plain[0].demoted, None);
        let fused = 2.0 / 61.0 + 1.0 / 61.0;
        assert_eq!(
            plain[1].ranking_score.to_bits(),
            (fused * DEMOTED_WEIGHT).to_bits()
        );
        for query in ["generate invoice test", "generate invoice specs"] {
            let named = ask_tree(&tree, query);
            assert_eq!(named[0].path, "__tests__/invoice.test.ts", "{query}");
            assert_eq!(named[0].demoted, None, "{query}");
        }
    }

    /// A locale file shares the words of a product question ("invoice") but
    /// is not where it is answered; asked about translations, it is.
    #[test]
    fn a_locale_file_loses_to_code_unless_the_question_names_translations() {
        let tree = [
            (
                "config/locales/en.yml",
                "en:\n  invoice:\n    generated: Invoice generated\n",
            ),
            ("lib/invoice.rb", "def generated_invoice; end"),
        ];
        let plain = ask_tree(&tree, "invoice generated");
        assert_eq!(plain[0].path, "lib/invoice.rb");
        assert_eq!(
            found(&plain, "config/locales/en.yml").demoted,
            Some(FileKind::Config)
        );
        let named = ask_tree(&tree, "invoice generated translation");
        assert_eq!(named[0].path, "config/locales/en.yml");
        assert_eq!(named[0].demoted, None);
    }

    /// The filename stem stays lexical evidence under BM25: a file whose
    /// name is the question's term outscores one that never says it, and the
    /// directory does not count.
    #[test]
    fn filename_stem_still_counts_as_lexical_evidence() {
        let hits = ask_tree(
            &[
                ("ledger/a.rs", "fn body() {}"),
                ("src/ledger.rs", "fn body() {}"),
            ],
            "ledger",
        );
        assert_eq!(hits[0].path, "src/ledger.rs");
        assert_eq!(found(&hits, "ledger/a.rs").lexical_score, 0.0);
        assert!(found(&hits, "src/ledger.rs").lexical_score > 0.0);
        assert_eq!(position(&hits, "ledger/a.rs"), 1);
    }

    /// The stem's tokens are part of every chunk's length as well as its
    /// counts, the same unit as the body: `ledger.rs` is `fn`, `a` plus the
    /// stem (3 tokens), `b.rs` is `fn`, `b`, `x`, `u8` plus its stem (5), so
    /// avgdl is 4 and the only `ledger` (df 1 of 2) scores exactly
    /// ln 2 * 2.2 / (1 + 1.2 * (0.25 + 0.75 * 3 / 4)).
    #[test]
    fn filename_stem_tokens_count_in_the_chunk_length() {
        let hits = ask_tree(
            &[("ledger.rs", "fn a() {}"), ("b.rs", "fn b(x: u8) {}")],
            "ledger",
        );
        let norm = 1.0 - pixel_rank::BM25_B + pixel_rank::BM25_B * 3.0 / 4.0;
        let expected =
            2.0f64.ln() * (pixel_rank::BM25_K1 + 1.0) / (1.0 + pixel_rank::BM25_K1 * norm);
        let score = found(&hits, "ledger.rs").lexical_score;
        assert!((score - expected).abs() < 1e-12, "{score} vs {expected}");
    }

    /// Which paths are tests, configuration/data or docs, test first.
    #[test]
    fn file_kinds_follow_directories_and_names() {
        for (path, kind) in [
            ("test/models/invoice_test.rb", Some(FileKind::Test)),
            ("lib/__tests__/a.ts", Some(FileKind::Test)),
            ("spec/fixtures/data.json", Some(FileKind::Test)),
            ("specs/invoice.ts", Some(FileKind::Test)),
            ("web/specs/fixtures/data.json", Some(FileKind::Test)),
            ("pkg/invoice_test.go", Some(FileKind::Test)),
            ("src/a.test.ts", Some(FileKind::Test)),
            ("src/a.spec.ts", Some(FileKind::Test)),
            ("lib/a_spec.rb", Some(FileKind::Test)),
            ("Src/A.Spec.TS", Some(FileKind::Test)),
            ("package.json", Some(FileKind::Config)),
            ("config/app.yaml", Some(FileKind::Config)),
            ("config/app.yml", Some(FileKind::Config)),
            ("Cargo.toml", Some(FileKind::Config)),
            ("config/locales/fr.rb", Some(FileKind::Config)),
            ("app/locale/x.ts", Some(FileKind::Config)),
            ("web/i18n/index.ts", Some(FileKind::Config)),
            ("README.md", Some(FileKind::Docs)),
            ("docs/guide.md", Some(FileKind::Docs)),
            ("src/testing.rs", None),
            ("src/contest/spec_parser.rs", None),
            ("lib/latest.rb", None),
            ("src/main.rs", None),
            ("md", None),
        ] {
            assert_eq!(FileKind::of(path), kind, "{path}");
        }
        let names = [FileKind::Test, FileKind::Config, FileKind::Docs].map(FileKind::as_str);
        assert_eq!(names, ["test", "config", "docs"]);
    }

    /// Each kind is lifted by its own words only.
    #[test]
    fn a_question_names_a_file_kind_by_its_own_words() {
        let terms = |query: &str| sorted_query_terms(query);
        for (query, test, config, docs) in [
            ("where is invoice generated", false, false, false),
            ("invoice tests", true, false, false),
            ("invoice spec", true, false, false),
            ("testing invoices", true, false, false),
            ("invoice config", false, true, false),
            ("yaml settings", false, true, false),
            ("toml json", false, true, false),
            ("locale translations i18n", false, true, false),
            ("readme", false, false, true),
            ("docs changelog", false, false, true),
            ("markdown documentation", false, false, true),
        ] {
            let terms = terms(query);
            assert_eq!(FileKind::Test.named_by(&terms), test, "{query}");
            assert_eq!(FileKind::Config.named_by(&terms), config, "{query}");
            assert_eq!(FileKind::Docs.named_by(&terms), docs, "{query}");
        }
    }

    /// The tokens BM25 counts: identifiers once per occurrence, their parts
    /// once each, a lone part equal to its identifier not twice.
    #[test]
    fn term_counts_count_occurrences_and_split_parts_once() {
        let terms = sorted_query_terms("read http setup private");
        let (freqs, len) = term_counts("readHTTP setup setup _private", &terms);
        // terms sorted: http, private, read, setup
        assert_eq!(freqs, [1, 1, 1, 2]);
        // readhttp read http | setup | setup | _private private
        assert_eq!(len, 7);
        assert_eq!(term_counts("", &terms), (vec![0, 0, 0, 0], 0));
    }

    #[test]
    fn is_code_file_goes_by_the_lowercased_extension() {
        assert!(is_code_file("src/main.rs"));
        assert!(is_code_file("MAIN.RS"));
        assert!(is_code_file("app/models/user.rb") || !is_code_file("app/models/user.rb"));
        assert!(!is_code_file("logo.png"));
        assert!(!is_code_file("Makefile"));
        assert!(!is_code_file("archive.tar.gz"));
    }

    /// Scores 1 for a text that mentions `word`, 0 otherwise, so a fixture
    /// can make exactly one file the semantic answer.
    struct MentionEmbedder {
        word: &'static str,
    }
    impl crate::embed::Embedder for MentionEmbedder {
        fn model_id(&self) -> &str {
            "mention"
        }
        fn dims(&self) -> usize {
            2
        }
        fn embed_batch(&mut self, texts: &[&str], _: EmbedKind) -> Result<Vec<Vec<f32>>, String> {
            Ok(texts
                .iter()
                .map(|text| {
                    if text.contains(self.word) {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect())
        }
    }

    fn write(root: &Path, relative: &str, body: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn relative(root: &Path, files: &[PathBuf]) -> Vec<String> {
        files
            .iter()
            .map(|f| f.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// A question without `--max-files` searches every eligible file, however
    /// many there are below the safety ceiling: here 6 002, more than the
    /// previous default budget of 6 000 (and than the 2 000 before it), with
    /// the answer in the deepest, alphabetically-last directory, which a
    /// budget of 6 000 would have searched only by the luck of the sample.
    /// Through the production pipeline ([`ask_opening`]), so the default the
    /// CLI passes (`None`) is the one tested.
    #[test]
    fn unbudgeted_question_searches_every_file_of_a_tree_over_the_old_budget() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..6001 {
            write(root, &format!("a/filler{i:04}.rs"), "fn filler() {}");
        }
        let answer = "zz/y/x/answer.rs";
        write(root, answer, "fn generate_invoice() {}");
        let (result, embedded) =
            ask_counting(root, "invoice", VectorCache::NoIndex, "m", invoice_vector);
        let coverage = &result.coverage;
        assert!(
            coverage.candidate_files > 6000,
            "the fixture must outgrow the old default budget"
        );
        assert_eq!(
            (coverage.candidate_files, coverage.searched_files),
            (6002, 6002)
        );
        assert_eq!(coverage.max_files, None);
        assert_eq!(coverage.file_budget, FileBudget::None);
        assert!(!coverage.file_limit_reached && !coverage.degraded);
        assert_eq!(coverage.sample_note(), None);
        assert_eq!(coverage.chunks, 6002);
        assert_eq!(embedded, 2, "the identical fillers are one text");
        assert_eq!(result.hits[0].path, answer);
    }

    /// The ceiling only guards an unbudgeted question against a runaway
    /// universe: at it, everything is searched and no limit is reported;
    /// above it, the same deterministic sample an explicit budget of that
    /// size takes, reported as the ceiling. An explicit `--max-files`
    /// replaces the ceiling, above it as well as below.
    #[test]
    fn ceiling_samples_only_an_unbudgeted_question_that_outgrows_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..30 {
            write(root, &format!("m{}/f{i:02}.rs", i % 4), "fn f() {}");
        }

        let (files, at) = collect_files_within(root, None, 30);
        assert_eq!(files.len(), 30);
        assert_eq!((at.max_files, at.file_budget), (None, FileBudget::None));
        assert!(!at.file_limit_reached && !at.degraded);

        let (files, over) = collect_files_within(root, None, 20);
        assert_eq!(files, collect_files(root, Some(20)).0, "same sample");
        assert_eq!(files.len(), 20);
        assert_eq!(
            (over.candidate_files, over.max_files, over.file_budget),
            (30, Some(20), FileBudget::Ceiling)
        );
        assert!(over.file_limit_reached && over.degraded);
        let mut searched = over;
        searched.searched_files = 20;
        assert_eq!(
            searched.sample_note().as_deref(),
            Some(
                "searched a deterministic sample of 20 of 30 eligible files (safety ceiling of 20 files without --max-files); pass a larger --max-files to search them all"
            )
        );

        let (files, explicit) = collect_files_within(root, Some(25), 20);
        assert_eq!(files.len(), 25, "an explicit budget above the ceiling wins");
        assert_eq!(
            (explicit.max_files, explicit.file_budget),
            (Some(25), FileBudget::Explicit)
        );
        assert!(explicit.file_limit_reached);
        let (files, explicit) = collect_files_within(root, Some(30), 20);
        assert_eq!(files.len(), 30);
        assert_eq!(
            (explicit.max_files, explicit.file_budget),
            (Some(30), FileBudget::Explicit)
        );
        assert!(!explicit.file_limit_reached && !explicit.degraded);
    }

    /// The daemon's semantic fallback keeps its budget: it runs inside a
    /// request and persists nothing, so an unbudgeted scan would re-embed a
    /// whole large repository on every lexical miss. One file over the
    /// budget: a sample of exactly the budget is embedded, the fallback says
    /// so, and nothing is written even at an indexed root.
    #[test]
    fn semantic_fallback_stops_at_its_budget_and_writes_nothing() {
        let dir = indexed_tree();
        let root = dir.path();
        for i in 0..=SEMANTIC_FALLBACK_MAX_FILES {
            write(root, &format!("src/f{i:04}.rs"), "fn filler() {}");
        }
        let embedded = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&embedded);
        let fallback = fallback_opening(root, "filler", 3, move || {
            Ok(Box::new(CountingEmbedder {
                model: "m",
                vector: invoice_vector,
                embedded: counter,
            }) as Box<dyn crate::embed::Embedder>)
        });
        assert_eq!(fallback.searched_files, SEMANTIC_FALLBACK_MAX_FILES);
        assert!(fallback.file_limit_reached && !fallback.disabled);
        assert_eq!(fallback.hits.len(), 3);
        assert!(
            fallback
                .hits
                .iter()
                .all(|(path, _)| path.starts_with("src/f")),
            "{:?}",
            fallback.hits
        );
        assert_eq!(embedded.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!store_dir(root).exists(), "the fallback persisted vectors");
    }

    /// A gitignored file is generated or private: it is never embedded, never
    /// returned and never counted, even when it is the best textual match and
    /// the tree has no git repository (`.gitignore` applies regardless).
    #[test]
    fn gitignored_files_are_never_candidates_nor_hits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, ".gitignore", "generated/\nsecret.rs\n");
        write(
            root,
            "generated/invoice.rs",
            "fn invoice() {} // invoice invoice",
        );
        write(root, "secret.rs", "fn invoice() {} // invoice invoice");
        write(root, "src/nested/.gitignore", "local_invoice.rs\n");
        write(root, "src/nested/local_invoice.rs", "fn invoice() {}");
        write(root, "src/billing.rs", "fn invoice() {}");
        let (files, coverage) = collect_files(root, None);
        assert_eq!(relative(root, &files), ["src/billing.rs"]);
        assert_eq!(coverage.candidate_files, 1);
        let result = ask_collected(
            root,
            "invoice",
            DEFAULT_LIMIT,
            files,
            coverage,
            &mut MentionEmbedder { word: "invoice" },
            None,
        )
        .unwrap();
        let hits: Vec<_> = result.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(hits, ["src/billing.rs"]);
        assert_eq!(result.coverage.searched_files, 1);
    }

    /// `skip_dir` applies to every directory on the path, not only to the
    /// top level, and names the walk policy does not prune (`tests`,
    /// `assets`, `examples`) stay excluded; the root's own name never counts.
    #[test]
    fn excluded_directory_names_apply_at_any_depth_below_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tests");
        for relative in [
            "crates/app/src/lib.rs",
            "crates/app/tests/it.rs",
            "crates/app/assets/data.json",
            "examples/demo.rs",
            "crates/app/README",
            "crates/app/logo.png",
        ] {
            write(&root, relative, "fn body() {}");
        }
        let (files, coverage) = collect_files(&root, None);
        assert_eq!(relative(&root, &files), ["crates/app/src/lib.rs"]);
        assert_eq!(coverage.candidate_files, 1);
    }

    /// Shallow files first in the alphabet, as many deep ones last: a sample
    /// of 20 must reach the deep directory (a walk cut at the budget took the
    /// 20 root files), and it must be the same sample on every run and from
    /// every checkout location, so a result can be reproduced.
    #[test]
    fn over_budget_sample_is_deterministic_and_spans_the_whole_tree() {
        let dir = tempfile::tempdir().unwrap();
        let mut samples = Vec::new();
        for location in ["one", "elsewhere/two"] {
            let root = dir.path().join(location);
            for i in 0..100 {
                write(&root, &format!("r{i:03}.rs"), "fn shallow() {}");
                write(&root, &format!("z/y/x/w/d{i:03}.rs"), "fn deep() {}");
            }
            let (files, _) = collect_files(&root, Some(20));
            assert_eq!(files, collect_files(&root, Some(20)).0, "same run twice");
            samples.push(relative(&root, &files));
        }
        assert_eq!(samples[0], samples[1], "independent of the checkout path");
        let sample = &samples[0];
        assert_eq!(sample.len(), 20);
        let mut sorted = sample.clone();
        sorted.sort();
        assert_eq!(&sorted, sample, "returned in path order");
        let deep = sample.iter().filter(|p| p.starts_with("z/y/x/w/")).count();
        assert!(
            (1..20).contains(&deep),
            "both depths represented, got {deep} deep of 20: {sample:?}"
        );
    }

    /// The hash sample depends on each path alone: removing a file the
    /// sample did not keep leaves the sample unchanged, where a stride over
    /// the sorted list would shift every pick after it.
    #[test]
    fn removing_an_unsampled_file_keeps_the_sample() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..60 {
            write(root, &format!("m{}/f{i:02}.rs", i % 6), "fn f() {}");
        }
        let (before, _) = collect_files(root, Some(15));
        let dropped = (0..60)
            .map(|i| root.join(format!("m{}/f{i:02}.rs", i % 6)))
            .find(|path| !before.contains(path))
            .unwrap();
        std::fs::remove_file(&dropped).unwrap();
        let (after, coverage) = collect_files(root, Some(15));
        assert_eq!(after, before);
        assert_eq!(coverage.candidate_files, 59);
    }

    /// Both regimes report exactly what was eligible, what was embedded and
    /// the budget between them: under it, the universe is the search; over
    /// it, the universe stays counted while only the sample is embedded.
    #[test]
    fn coverage_counts_are_exact_under_and_over_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..30 {
            write(root, &format!("src/f{i:02}.rs"), "fn f() {}");
        }
        write(root, "notes.txt", "not code");
        let embed = |max_files| {
            let (files, coverage) = collect_files(root, Some(max_files));
            let searched = files.len();
            let result = ask_collected(
                root,
                "f",
                DEFAULT_LIMIT,
                files,
                coverage,
                &mut FixtureEmbedder { fail: false },
                None,
            )
            .unwrap();
            (searched, result.coverage)
        };
        let (searched, under) = embed(30);
        assert_eq!(searched, 30);
        assert_eq!(
            (under.candidate_files, under.searched_files, under.max_files),
            (30, 30, Some(30))
        );
        assert!(!under.file_limit_reached && !under.degraded);
        assert_eq!(under.sample_note(), None);
        let (searched, over) = embed(12);
        assert_eq!(searched, 12);
        assert_eq!(
            (over.candidate_files, over.searched_files, over.max_files),
            (30, 12, Some(12))
        );
        assert!(over.file_limit_reached && over.degraded);
        assert_eq!(
            over.sample_note().as_deref(),
            Some(
                "searched a deterministic sample of 12 of 30 eligible files (--max-files 12); raise --max-files to search them all"
            )
        );
    }

    /// The CLI defaults: ten hits (what a recall@10 reads), no file budget,
    /// and a ceiling far above the largest repository measured (11 299
    /// eligible files) that only a home directory or a monorepo reaches.
    #[test]
    fn defaults_are_ten_hits_and_a_fifty_thousand_file_ceiling() {
        assert_eq!(DEFAULT_LIMIT, 10);
        assert_eq!(UNBUDGETED_FILE_CEILING, 50_000);
    }

    /// Embeds with `vector` and counts the chunk texts it is handed (the
    /// question is not counted), so a test can tell a vector read from the
    /// store from one the model recomputed.
    struct CountingEmbedder {
        model: &'static str,
        vector: fn(&str) -> Vec<f32>,
        embedded: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl crate::embed::Embedder for CountingEmbedder {
        fn model_id(&self) -> &str {
            self.model
        }
        fn dims(&self) -> usize {
            (self.vector)("").len()
        }
        fn embed_batch(
            &mut self,
            texts: &[&str],
            kind: EmbedKind,
        ) -> Result<Vec<Vec<f32>>, String> {
            if matches!(kind, EmbedKind::Passage) {
                self.embedded
                    .fetch_add(texts.len(), std::sync::atomic::Ordering::SeqCst);
            }
            Ok(texts.iter().map(|text| (self.vector)(text)).collect())
        }
    }

    /// `[1, 0]` for a text naming an invoice, `[0, 1]` otherwise.
    fn invoice_vector(text: &str) -> Vec<f32> {
        if text.contains("invoice") {
            vec![1.0, 0.0]
        } else {
            vec![0.0, 1.0]
        }
    }

    /// The question over `root` through the production pipeline
    /// ([`ask_opening`]) with a counting embedder: the result and the number
    /// of chunk texts the model was asked to embed.
    fn ask_counting(
        root: &Path,
        query: &str,
        cache: VectorCache,
        model: &'static str,
        vector: fn(&str) -> Vec<f32>,
    ) -> (AskResult, usize) {
        let embedded = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&embedded);
        let result = ask_opening(root, query, DEFAULT_LIMIT, None, cache, move || {
            Ok(Box::new(CountingEmbedder {
                model,
                vector,
                embedded: counter,
            }) as Box<dyn crate::embed::Embedder>)
        })
        .unwrap();
        let count = embedded.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            result.coverage.embedded_chunks, count,
            "the coverage reports what the model did"
        );
        (result, count)
    }

    /// A tree carrying a pixel index at its root, as `pixel build-index`
    /// leaves it.
    fn indexed_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            &format!(
                "{}/{}",
                pixel_index::index::SHARD_DIR,
                pixel_index::index::SHARD_FILE
            ),
            "",
        );
        dir
    }

    fn store_dir(root: &Path) -> PathBuf {
        root.join(pixel_index::index::SHARD_DIR)
            .join(crate::code_vectors::DIR)
    }

    /// Every observable of a ranking, scores to the bit.
    fn ranking(result: &AskResult) -> Vec<(String, u32, u64, usize, String)> {
        result
            .hits
            .iter()
            .map(|hit| {
                (
                    hit.path.clone(),
                    hit.semantic_score.to_bits(),
                    hit.ranking_score.to_bits(),
                    hit.lexical_matches,
                    hit.snippet.clone(),
                )
            })
            .collect()
    }

    /// A 3 000-byte file (three windows) whose `marker` sits in the last
    /// 200 bytes, which only the third window covers.
    fn three_window_text(marker: &str) -> String {
        let mut text = String::new();
        let mut line = 0;
        while text.len() < 2900 {
            text.push_str(&format!("// filler line {line:04} of the ledger\n"));
            line += 1;
        }
        text.push_str(&format!("fn {marker}() {{}}\n"));
        text
    }

    /// A function `bytes` long, padded with comment lines.
    fn padded_function(name: &str, bytes: usize) -> String {
        let mut text = format!("fn {name}() {{\n");
        while text.len() + 40 < bytes {
            text.push_str("    // ................................\n");
        }
        text.push_str("}\n");
        text
    }

    /// The lexical channel and the embedder read the same symbol chunks:
    /// code written between two functions (a macro call) is found, and a
    /// minified bundle tree-sitter is not pointed at is still searched,
    /// through its windows.
    #[test]
    fn text_between_symbols_and_unparsed_files_stay_searchable() {
        let big = padded_function("first", 1_400);
        let between = format!("{big}register_tollbooth!();\n{big}");
        let bundle = format!("{}tollgate();", "var a=1;".repeat(9_000));
        let hits = ask_tree(
            &[
                ("src/between.rs", &between),
                ("src/other.rs", "fn unrelated() {}"),
                ("dist_js/app.js", &bundle),
            ],
            "tollbooth tollgate",
        );
        assert_eq!(found(&hits, "src/between.rs").lexical_matches, 1);
        assert_eq!(found(&hits, "dist_js/app.js").lexical_matches, 1);
        assert_eq!(found(&hits, "src/other.rs").lexical_matches, 0);
    }

    /// A file of exactly [`MAX_FILE_BYTES`] is searched, one byte more is
    /// skipped and counted.
    #[test]
    fn a_file_at_the_size_cap_is_searched_and_one_byte_more_skipped() {
        let at_cap = format!("toll {}", "x".repeat(MAX_FILE_BYTES - 5));
        let over = format!("{at_cap}x");
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "at_cap.md", &at_cap);
        write(dir.path(), "over.md", &over);
        let (files, coverage) = collect_files(dir.path(), None);
        let result = ask_collected(
            dir.path(),
            "toll",
            10,
            files,
            coverage,
            &mut FixtureEmbedder { fail: false },
            None,
        )
        .unwrap();
        assert_eq!(at_cap.len(), MAX_FILE_BYTES);
        assert_eq!(result.coverage.searched_files, 1);
        assert_eq!(result.coverage.skipped_files, 1);
        assert_eq!(result.hits[0].path, "at_cap.md");
    }

    /// The snippet is the best chunk's head, and the best chunk is the
    /// function with its doc comment, not a window opening on whatever came
    /// before it in the file.
    #[test]
    fn snippet_should_open_on_the_matching_function_and_its_doc_comment() {
        let dir = indexed_tree();
        let root = dir.path();
        let text = format!(
            "{}/// Issues the monthly invoice.\nfn issue() {{\n    send_invoice();\n}}\n",
            padded_function("unrelated_setup", 1_200)
        );
        write(root, "src/billing.rs", &text);
        let (result, _) = ask_counting(
            root,
            "monthly invoice",
            VectorCache::Disabled,
            "m",
            invoice_vector,
        );
        assert_eq!(
            result.hits[0].snippet,
            "/// Issues the monthly invoice. fn issue() { send_invoice(); }"
        );
    }

    /// Vectors of the window era (chunker 1) are never served once the
    /// chunker changed, even for a chunk whose text is the same: the first
    /// question after the upgrade embeds every chunk again.
    #[test]
    fn vectors_of_the_window_chunker_should_never_be_served() {
        let dir = indexed_tree();
        let root = dir.path();
        write(root, "src/audit.rs", "fn invoice_audit() {}\n");
        let text = "fn invoice_audit() {}\n";
        let model = "m";
        let revision = crate::embed::embedder_revision(model);
        let windows = Namespace::new(model, revision, 1);
        let key = windows.key(text);
        let vectors = HashMap::from([(key, invoice_vector(text))]);
        Store::at(&store_dir(root))
            .save(&windows, 2, &[key], &vectors, 0, false)
            .unwrap();
        assert_ne!(
            CHUNKER_VERSION, 1,
            "the symbol chunker has its own namespace"
        );

        let (result, embedded) = ask_counting(
            root,
            "invoice",
            vector_cache_for(root, None),
            model,
            invoice_vector,
        );
        assert_eq!(embedded, 1);
        assert_eq!(result.coverage.cached_chunks, 0);
        let (_, warm) = ask_counting(
            root,
            "invoice",
            vector_cache_for(root, None),
            model,
            invoice_vector,
        );
        assert_eq!(warm, 0, "the new chunker's vectors are served");
    }

    /// The point of the store: an unchanged tree asked twice pays the model
    /// once. The warm question embeds nothing, serves every chunk from
    /// `.pixel/code-vectors`, and answers exactly as the cold one did.
    #[test]
    fn warm_question_should_embed_no_chunk_when_the_tree_is_unchanged() {
        let dir = indexed_tree();
        let root = dir.path();
        write(root, "src/billing.rs", "fn invoice_total() {}");
        write(root, "src/other.rs", "fn unrelated() {}");
        write(root, "src/audit.rs", &three_window_text("invoice_marker"));
        let cache = vector_cache_for(root, None);
        assert_eq!(cache, VectorCache::Persisted);

        let (cold, cold_count) = ask_counting(root, "invoice", cache, "m", invoice_vector);
        assert_eq!(cold.coverage.chunks, 5, "1 + 1 + 3 windows");
        assert_eq!(cold_count, 5);
        assert_eq!(cold.coverage.cached_chunks, 0);
        assert!(store_dir(root).join("manifest.json").is_file());

        let snapshot = || {
            let mut files: Vec<_> = std::fs::read_dir(store_dir(root))
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    let modified = entry.metadata().unwrap().modified().unwrap();
                    (entry.file_name(), modified)
                })
                .collect();
            files.sort();
            files
        };
        let before = snapshot();
        let (warm, warm_count) = ask_counting(root, "invoice", cache, "m", invoice_vector);
        assert_eq!(snapshot(), before, "a warm question writes nothing");
        assert_eq!(warm_count, 0, "a warm question embeds no chunk");
        assert_eq!(warm.coverage.cached_chunks, 5);
        assert_eq!(warm.coverage.chunks, 5);
        assert_eq!(ranking(&warm), ranking(&cold));
        assert_eq!(warm.hits[0].path, "src/audit.rs");
        assert!(
            !warm.coverage.degraded,
            "{:?}",
            warm.coverage.vector_cache_errors
        );
        let json = serde_json::to_value(&warm.coverage).unwrap();
        assert_eq!(json["vector_cache"], "persisted");
        assert_eq!(json["embedded_chunks"], 0);
        assert_eq!(json["cached_chunks"], 5);
        assert_eq!(json["vector_cache_errors"], serde_json::json!([]));
    }

    /// No size or mtime shortcut: an edit that keeps the byte length and
    /// has its mtime put back is still seen, because the key is the chunk
    /// text. Exactly the windows whose text changed are embedded again, and
    /// the new text is what the question finds.
    #[test]
    fn same_size_edit_with_restored_mtime_should_reembed_exactly_the_changed_chunks() {
        let dir = indexed_tree();
        let root = dir.path();
        let before = three_window_text("alpha_marker");
        write(root, "src/audit.rs", &before);
        write(root, "src/other.rs", "fn unrelated() {}");
        let cache = vector_cache_for(root, None);
        let omega = |text: &str| {
            if text.contains("omega") {
                vec![1.0, 0.0]
            } else {
                vec![0.0, 1.0]
            }
        };
        let (cold, _) = ask_counting(root, "omega", cache, "m", omega);
        assert_eq!(cold.hits[0].semantic_score, 0.0, "no omega yet");

        let path = root.join("src/audit.rs");
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let after = before.replace("alpha_marker", "omega_marker");
        assert_eq!(after.len(), before.len());
        std::fs::write(&path, &after).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(
            (metadata.len(), metadata.modified().unwrap()),
            (before.len() as u64, modified),
            "the edit is invisible to size and mtime"
        );
        let changed = code_chunks("src/audit.rs", &before)
            .into_iter()
            .zip(code_chunks("src/audit.rs", &after))
            .filter(|((b0, b1), (a0, a1))| before[*b0..*b1] != after[*a0..*a1])
            .count();
        assert_eq!(changed, 1, "only the last window holds the marker");

        let (edited, count) = ask_counting(root, "omega", cache, "m", omega);
        assert_eq!(count, changed, "exactly the changed windows");
        assert_eq!(
            edited.coverage.cached_chunks,
            edited.coverage.chunks - changed
        );
        assert_eq!(edited.hits[0].path, "src/audit.rs");
        assert_eq!(edited.hits[0].semantic_score, 1.0, "the new text is found");
    }

    /// A gitignored file is never read, so its text never reaches the store:
    /// the rows on disk are exactly the searched files' chunks.
    #[test]
    fn gitignored_file_should_leave_no_row_on_disk() {
        let dir = indexed_tree();
        let root = dir.path();
        write(root, ".gitignore", "secret.rs\n");
        write(root, "secret.rs", "fn invoice_secret_token() {}");
        write(root, "kept.rs", "fn invoice_kept() {}");
        ask_counting(
            root,
            "invoice",
            vector_cache_for(root, None),
            "m",
            invoice_vector,
        );
        let namespace = Namespace::new("m", crate::embed::embedder_revision("m"), CHUNKER_VERSION);
        assert_eq!(
            Store::at(&store_dir(root)).keys_on_disk(),
            [namespace.key("fn invoice_kept() {}")]
        );
    }

    /// The store is written only at an indexed root that is not the home
    /// directory: a subtree (the NDCG bench asks `crates/pixel-graph/src`),
    /// an unindexed tree, a `.pixel` holding only journals, the home
    /// directory (whatever it carries) and the daemon's fallback write
    /// nothing, and embed every chunk on every question.
    #[test]
    fn subtree_home_and_fallback_roots_should_write_nothing() {
        let dir = indexed_tree();
        let root = dir.path();
        write(root, "src/billing.rs", "fn invoice_total() {}");
        let subtree = root.join("src");
        let plain = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(plain.path().join(pixel_index::index::SHARD_DIR)).unwrap();
        write(plain.path(), "billing.rs", "fn invoice_total() {}");
        let elsewhere = tempfile::tempdir().unwrap();
        assert_eq!(vector_cache_for(&subtree, None), VectorCache::NoIndex);
        assert_eq!(vector_cache_for(plain.path(), None), VectorCache::NoIndex);
        assert_eq!(
            vector_cache_for(root, Some(root)),
            VectorCache::HomeDirectory
        );
        assert_eq!(
            vector_cache_for(root, Some(elsewhere.path())),
            VectorCache::Persisted
        );
        #[cfg(unix)]
        {
            let link = elsewhere.path().join("home");
            std::os::unix::fs::symlink(root, &link).unwrap();
            assert_eq!(
                vector_cache_for(root, Some(&link)),
                VectorCache::HomeDirectory,
                "the home directory reached through a symlink is still home"
            );
        }

        for (tree, cache) in [
            (subtree.as_path(), vector_cache_for(&subtree, None)),
            (plain.path(), vector_cache_for(plain.path(), None)),
            (root, vector_cache_for(root, Some(root))),
            (root, VectorCache::Disabled),
        ] {
            for _ in 0..2 {
                let (result, count) = ask_counting(tree, "invoice", cache, "m", invoice_vector);
                assert_eq!(
                    count, 1,
                    "{cache:?}: nothing persisted, everything embedded"
                );
                assert_eq!(result.coverage.vector_cache, cache);
                assert_eq!(result.hits[0].semantic_score, 1.0);
            }
            for written in [
                store_dir(root),
                store_dir(&subtree),
                store_dir(plain.path()),
            ] {
                assert!(!written.exists(), "{cache:?} wrote {}", written.display());
            }
        }
    }

    /// Vectors of another model are never served: the key and the segment
    /// namespace both carry the model id (and its revision, and the chunker
    /// version: `namespace_should_change_every_key…` in `code_vectors`).
    #[test]
    fn model_change_should_never_serve_old_vectors() {
        let dir = indexed_tree();
        let root = dir.path();
        write(root, "src/billing.rs", "fn invoice_total() {}");
        write(root, "src/other.rs", "fn unrelated() {}");
        let cache = vector_cache_for(root, None);
        ask_counting(root, "invoice", cache, "model-a", invoice_vector);
        let flipped = |text: &str| {
            if text.contains("invoice") {
                vec![0.0, 1.0]
            } else {
                vec![1.0, 0.0]
            }
        };
        let (result, count) = ask_counting(root, "invoice", cache, "model-b", flipped);
        assert_eq!(count, 2, "model-b embeds every chunk itself");
        assert_eq!(result.coverage.cached_chunks, 0);
        assert_eq!(result.hits[0].path, "src/billing.rs");
        assert_eq!(
            result.hits[0].semantic_score, 1.0,
            "model-a's vector would score 0 against model-b's question"
        );
        let (_, again) = ask_counting(root, "invoice", cache, "model-b", flipped);
        assert_eq!(again, 0, "model-b's own vectors are served");
    }

    /// A segment the manifest names but that is gone is an error the answer
    /// carries (degraded, named in the coverage and the human note), never a
    /// silent skip; the question still answers, identically, by embedding
    /// again, and the store is rebuilt for the next one.
    #[test]
    fn missing_segment_should_surface_an_error_and_still_answer() {
        let dir = indexed_tree();
        let root = dir.path();
        write(root, "src/billing.rs", "fn invoice_total() {}");
        write(root, "src/other.rs", "fn unrelated() {}");
        let cache = vector_cache_for(root, None);
        let (cold, _) = ask_counting(root, "invoice", cache, "m", invoice_vector);
        for entry in std::fs::read_dir(store_dir(root)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "vec") {
                std::fs::remove_file(path).unwrap();
            }
        }
        let (damaged, count) = ask_counting(root, "invoice", cache, "m", invoice_vector);
        assert_eq!(count, 2, "every chunk embedded again");
        assert_eq!(ranking(&damaged), ranking(&cold));
        assert!(damaged.coverage.degraded);
        let errors = &damaged.coverage.vector_cache_errors;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("seg-") && errors[0].contains("unreadable"),
            "{errors:?}"
        );
        let note = damaged.coverage.vector_cache_note().unwrap();
        assert!(note.contains(&errors[0]), "{note}");

        let (repaired, count) = ask_counting(root, "invoice", cache, "m", invoice_vector);
        assert_eq!(count, 0, "the store was rebuilt");
        assert!(repaired.coverage.vector_cache_errors.is_empty());
        assert!(!repaired.coverage.degraded);
        assert_eq!(repaired.coverage.vector_cache_note(), None);
    }

    /// A cache error adds to what already degraded the answer (here a
    /// skipped non-UTF-8 file); it never clears it.
    #[test]
    fn cache_error_should_keep_an_already_degraded_answer_degraded() {
        let dir = indexed_tree();
        let root = dir.path();
        write(root, "src/billing.rs", "fn invoice_total() {}");
        std::fs::write(root.join("src/invalid.rs"), [0xff]).unwrap();
        let cache = vector_cache_for(root, None);
        ask_counting(root, "invoice", cache, "m", invoice_vector);
        for entry in std::fs::read_dir(store_dir(root)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "vec") {
                std::fs::remove_file(path).unwrap();
            }
        }
        let (result, count) = ask_counting(root, "invoice", cache, "m", invoice_vector);
        assert_eq!(count, 1, "the lost chunk is embedded again");
        assert_eq!(result.coverage.skipped_files, 1);
        assert_eq!(result.coverage.vector_cache_errors.len(), 1);
        assert!(
            result.coverage.degraded,
            "a cache error never clears degraded"
        );
    }

    #[test]
    fn vector_cache_note_should_join_every_error() {
        let mut coverage = AskCoverage::default();
        assert_eq!(coverage.vector_cache_note(), None);
        coverage.vector_cache_errors = vec!["a failed".into(), "b failed".into()];
        assert_eq!(
            coverage.vector_cache_note().as_deref(),
            Some("vector cache: a failed; b failed; the affected chunks were embedded again")
        );
    }

    /// Garbage collection by reachability: deleting files leaves their rows
    /// until the unreachable ones pass a quarter of the live ones, then the
    /// store is rewritten with the live rows only.
    #[test]
    fn deleted_files_should_leave_the_disk_once_unreachable_rows_pass_the_threshold() {
        let dir = indexed_tree();
        let root = dir.path();
        for i in 0..8 {
            write(root, &format!("src/f{i}.rs"), &format!("fn f{i}() {{}}"));
        }
        let cache = vector_cache_for(root, None);
        let store = Store::at(&store_dir(root));
        ask_counting(root, "invoice", cache, "m", invoice_vector);
        assert_eq!(store.keys_on_disk().len(), 8);

        std::fs::remove_file(root.join("src/f0.rs")).unwrap();
        let (_, count) = ask_counting(root, "invoice", cache, "m", invoice_vector);
        assert_eq!(count, 0);
        assert_eq!(
            store.keys_on_disk().len(),
            8,
            "1 unreachable row of 7 live is under the threshold: kept"
        );

        std::fs::remove_file(root.join("src/f1.rs")).unwrap();
        let (_, count) = ask_counting(root, "invoice", cache, "m", invoice_vector);
        assert_eq!(count, 0);
        let namespace = Namespace::new("m", crate::embed::embedder_revision("m"), CHUNKER_VERSION);
        let mut live: Vec<_> = (2..8)
            .map(|i| namespace.key(&format!("fn f{i}() {{}}")))
            .collect();
        live.sort_unstable();
        assert_eq!(store.keys_on_disk(), live, "2 of 6 passes it: compacted");
    }

    /// Cached vectors are the embedder's own `f32`s, so a cached question
    /// ranks bit for bit like an uncached one, even where scores sit 1e-4
    /// apart (search-meaning's real scores crowd into 0.33 to 0.39). A
    /// quantized store (i8, f16) would flatten these ties into path order.
    #[test]
    fn cached_and_uncached_rankings_should_be_bit_identical_on_near_ties() {
        fn near_tie(text: &str) -> Vec<f32> {
            // `fn f<i>() {}`: a step per file, in an order that is not the
            // files' (7 is coprime with 10).
            if let Some(i) = text
                .strip_prefix("fn f")
                .and_then(|rest| rest.split('(').next())
                .and_then(|digits| digits.parse::<u8>().ok())
            {
                let step = f32::from(i * 7 % 10);
                vec![0.5 + step * 1e-4, 0.5, 0.0]
            } else {
                vec![1.0, 0.0, 0.0]
            }
        }
        let indexed = indexed_tree();
        let plain = tempfile::tempdir().unwrap();
        for i in 0..10 {
            for root in [indexed.path(), plain.path()] {
                write(root, &format!("src/f{i}.rs"), &format!("fn f{i}() {{}}"));
            }
        }
        let (uncached, _) = ask_counting(plain.path(), "zzz", VectorCache::NoIndex, "m", near_tie);
        let cache = vector_cache_for(indexed.path(), None);
        let (cold, _) = ask_counting(indexed.path(), "zzz", cache, "m", near_tie);
        let (warm, count) = ask_counting(indexed.path(), "zzz", cache, "m", near_tie);
        assert_eq!(count, 0);
        assert_eq!(warm.coverage.cached_chunks, 10);
        let scores: HashSet<u32> = uncached
            .hits
            .iter()
            .map(|hit| hit.semantic_score.to_bits())
            .collect();
        assert_eq!(scores.len(), 10, "the fixture has no exact tie");
        let paths: Vec<&str> = uncached.hits.iter().map(|hit| hit.path.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_ne!(paths, sorted, "the order is the scores', not the paths'");
        assert_eq!(ranking(&cold), ranking(&uncached));
        assert_eq!(ranking(&warm), ranking(&uncached));
    }

    /// A root with no `.pixel/code-vectors/` and a models dir without
    /// markers is cold on both counts.
    #[test]
    fn warm_probe_should_report_a_never_used_root_cold() {
        let root = tempfile::tempdir().unwrap();
        let models = tempfile::tempdir().unwrap();
        assert_eq!(
            warm_probe_in(root.path(), models.path()),
            WarmProbe {
                model_on_disk: false,
                vectors_present: false,
                vectors_chunks: 0,
            }
        );
    }

    /// `model_on_disk` reads the markers `require_set_up` admits: the
    /// code-search repository's own marker, or a legacy `potion.ok` at all
    /// — never another repository's.
    #[test]
    fn warm_probe_should_read_the_same_markers_the_opener_admits() {
        let root = tempfile::tempdir().unwrap();

        let models = tempfile::tempdir().unwrap();
        std::fs::write(
            crate::potion_marker(models.path(), CODE_SEARCH_MODEL_REPO),
            CODE_SEARCH_MODEL_REPO,
        )
        .unwrap();
        let probe = warm_probe_in(root.path(), models.path());
        assert!(probe.model_on_disk);
        assert!(!probe.vectors_present && probe.vectors_chunks == 0);

        let models = tempfile::tempdir().unwrap();
        std::fs::write(
            crate::potion_marker(models.path(), "minishlab/potion-code-64M-v2"),
            "minishlab/potion-code-64M-v2",
        )
        .unwrap();
        assert!(
            !warm_probe_in(root.path(), models.path()).model_on_disk,
            "another repository's marker admits nothing"
        );

        let models = tempfile::tempdir().unwrap();
        std::fs::write(
            models.path().join(crate::LEGACY_POTION_MARKER),
            "minishlab/potion-multilingual-128M",
        )
        .unwrap();
        assert!(
            warm_probe_in(root.path(), models.path()).model_on_disk,
            "a legacy marker admits every model, as require_set_up does"
        );
    }

    /// `vectors_*` count the rows a store written through `Store::save`
    /// names in its manifest — the real write path — without a segment
    /// being opened.
    #[test]
    fn warm_probe_should_count_persisted_vector_rows() {
        let root = indexed_tree();
        let models = tempfile::tempdir().unwrap();
        let store = Store::at(&store_dir(root.path()));
        let namespace = Namespace::new("m", crate::embed::embedder_revision("m"), CHUNKER_VERSION);
        let live: HashMap<ChunkKey, Vec<f32>> =
            [1, 2, 3].map(|key| (key, vec![key as f32; 4])).into();
        store
            .save(&namespace, 4, &[1, 2, 3], &live, 0, false)
            .unwrap();
        let probe = warm_probe_in(root.path(), models.path());
        assert!(!probe.model_on_disk);
        assert!(probe.vectors_present);
        assert_eq!(probe.vectors_chunks, 3);
    }

    /// A store directory whose manifest cannot be read reports cold, like
    /// the `Store::load` that re-embeds on the same manifest.
    #[test]
    fn warm_probe_should_report_an_unreadable_store_cold() {
        let root = indexed_tree();
        let models = tempfile::tempdir().unwrap();
        let store = Store::at(&store_dir(root.path()));
        let namespace = Namespace::new("m", crate::embed::embedder_revision("m"), CHUNKER_VERSION);
        let live: HashMap<ChunkKey, Vec<f32>> = [(1, vec![0.0f32; 4])].into();
        store.save(&namespace, 4, &[1], &live, 0, false).unwrap();
        assert!(warm_probe_in(root.path(), models.path()).vectors_present);
        // The manifest file of the store layout, corrupted after the write.
        std::fs::write(store_dir(root.path()).join("manifest.json"), "{not json").unwrap();
        let probe = warm_probe_in(root.path(), models.path());
        assert!(!probe.vectors_present);
        assert_eq!(probe.vectors_chunks, 0);
    }
}
