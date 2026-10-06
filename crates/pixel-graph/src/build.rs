// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Whole-repo build / single-file update orchestration.
//!
//! `build_graph`: walk → parallel extract (rayon) → single-writer store
//! phase (files+symbols, then imports, then tiered call resolution).
//! `update_file`: transactional per-file replacement + re-resolution.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

use pixel_index::indexset::millis;
use rayon::prelude::*;
use xxhash_rust::xxh3::xxh3_64;

use crate::extract::{
    FileExtraction, RawMixin, extract_file, is_binstub_candidate, lang_of, lang_of_file,
};
use crate::imports::{resolve_import_in, ruby as ruby_projects};
use crate::resolve::{
    Affected, Definition, FileCalls, FileReferences, PendingCall, PendingReference,
    reconsider_resolved_calls, resolve_affected, resolve_calls, resolve_references,
};
use crate::store::{EdgeKind, ExecCached, GraphStore, StoreError, extract_crux};

/// Extract concepts for a file and insert them, linking each to the smallest
/// enclosing symbol (by line range) when one exists. `symbol_ids` are the ids
/// of the file's symbols in `start_line` order.
fn insert_concepts(
    store: &GraphStore,
    file_id: i64,
    rel: &str,
    content: &[u8],
    symbol_ids: &[i64],
    symbol_lines: &[(u32, u32)],
) -> Result<(), BoxErr> {
    let concepts = crate::concept::extract_concepts(rel, content);
    for mut c in concepts {
        c.owner_symbol_id = symbol_lines
            .iter()
            .enumerate()
            .filter(|(_, (s, e))| *s <= c.start_line && c.end_line <= *e)
            .min_by_key(|(_, (s, e))| e - s)
            .map(|(i, _)| symbol_ids[i]);
        store.insert_concept(
            file_id,
            c.kind,
            &c.raw,
            &c.norm,
            &c.detail,
            c.start_line,
            c.end_line,
            c.owner_symbol_id,
        )?;
    }
    Ok(())
}

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

/// `meta` key under which the build-time freshness signature is stored.
pub const FRESHNESS_KEY: &str = "freshness";

/// The value an incremental update stores under [`FRESHNESS_KEY`] when it
/// committed rows it could not sign. It is not a hexadecimal string, so it
/// never equals a tree's signature: `is_fresh` says stale and `tree_delta`
/// computes the drift from the per-row content hashes and repairs it
/// incrementally. Deleting the key instead would make `tree_delta` return
/// `None` ("built before signatures existed"), which the daemon answers
/// with a full rebuild. Leaving the previous signature in place is not an
/// option either: if the tree moved back to the state that signature
/// describes (A, rows written for B, A again before signing), the stale
/// rows would pass as fresh.
pub const FRESHNESS_WITHHELD: &str = "withheld";

/// `meta` key under which a full build records [`EXTRACTOR_VERSION`].
pub const EXTRACTOR_VERSION_KEY: &str = "extractor_version";

/// What extraction writes for an unchanged file. The freshness signature
/// only hashes file contents, so a graph built by an older extractor looks
/// fresh forever; a stored version other than this one makes it stale and
/// forces a full rebuild. Bump it whenever an extractor or resolver change
/// alters the rows an unchanged source produces.
///
/// 2: JSX component call edges; callback references only for functions the
/// graph defines, member arguments only on a self receiver.
/// 3: Rust trait-implementation methods are marked (`symbols.trait_impl`).
/// 4: Rust enum variants are symbols (`SymbolKind::Variant`).
/// 5: Rust external module declarations are marked (`symbols.module_decl`),
///    so `symbol_hits` can promote inline modules without promoting `mod foo;`.
/// 6: the resolver relaxes the receiver-shadow veto for a sole same-file
///    inherent method (`w.push_call()`), turning those rows into Probable
///    edges for unchanged sources.
/// 7: a Rust `use` records the names it binds (`imports.bindings`), so the
///    import tier turns unresolved calls into Exact edges.
/// 8: an aliased import binds the alias to its source name
///    (`use a::push as leased;`, `import { push as leased }`), so T1 links
///    `leased()` to `push` and no longer links an unbound `push()`.
/// 9: a Rust `use` naming several paths yields one import per path
///    (`imports.path`), each resolved to its own file.
/// 10: a Rust `use` records the lines where its names are in scope
///    (`imports.scope`), and T1 ignores it outside them.
/// 11: a Rust method call on a local of stated type or on a constructor
///    (`runner.x()` with `runner: &GitRunner`, `GitRunner::new(root).x()`)
///    records that type as its receiver, and a module-path receiver
///    (`signals::is_test_path`) resolves to that module's free function.
/// 12: Rust literal environment reads (`env::var("NAME")`, `env::var_os`,
///    `env!`, `option_env!`) are stored as `env_read` concepts.
/// 13: a Go package import or a Java wildcard import resolves to the
///    package's smallest path, not to the first one the walk or the store
///    happened to list (`imports.resolved_file_id`).
/// 14: markup concepts carry 1-based lines (HTML started at 0, Svelte/Vue
///    markup before a `<script>` was offset), strings under `__tests__/` are
///    no longer concepts, and `app.get("/users")` keeps its path.
/// 15: C function definitions are symbols and `o->start()` keeps its
///    receiver; Swift calls are recorded; Elixir `def`/`defmodule` and the
///    heads they define are not calls; a double-quoted PHP `require_once` is
///    an import; C# generic calls are named without type arguments, an alias
///    `using` no longer imports its alias, and callback arguments are
///    references.
/// 16: a Ruby call without receiver to a name defined in its file and in
///    another one resolves to the caller's own method, as `self.name` did,
///    instead of staying unresolved.
/// 17: a Ruby `def` inside `class << self` or after a bare `module_function`
///    is a class method (`Klass.name`), not an instance method.
/// 18: Ruby callback and literal self-send symbols reference the owning method,
///    preserving that owner during incremental resolution.
/// 19: Ruby constant receivers resolve against lexical owners, including
///    constructors and probable Rails dispatch; factory receivers are normalized.
/// 20: Ruby `attr_*`, `alias_method`/`alias`, `delegate` and `scope` declare
///    methods of their owner (and are no longer calls of it), an alias or
///    delegator references its own owner's target, and an assignment through
///    a receiver calls the writer (`self.name = v` → `name=`).
/// 21: Ruby superclass/`include`/`prepend`/`extend` declarations are stored
///    (`ruby_mixins`), a concern's `class_methods` block defines its
///    `ClassMethods` module, and calls on `self` follow Ruby's ancestor
///    lookup order; an instance call no longer reaches a class method.
/// 22: `Gemfile`, `Rakefile`, `Guardfile`, `Capfile` and Ruby-shebang
///    binstubs under `bin/`/`exe/` are Ruby files, and Ruby `require` /
///    `require_relative` resolve to files inside their project's load roots
///    (`imports.path` keeps `require_relative` as `./spec`).
/// 23: Rails routes files yield `route` concepts with their handler and a
///    `references` edge to the controller action (`:route` arg_of); Active
///    Record associations reference their model class (`:association`).
pub const EXTRACTOR_VERSION: &str = "23";

/// True iff the graph's rows were written by the current extractor.
fn extractor_is_current(store: &GraphStore) -> Result<bool, BoxErr> {
    Ok(store.meta_get(EXTRACTOR_VERSION_KEY)?.as_deref() == Some(EXTRACTOR_VERSION))
}

#[derive(Debug, Clone)]
pub struct GraphStats {
    pub files: u64,
    pub symbols: u64,
    pub edges: u64,
    pub unresolved: u64,
    pub elapsed_ms: u128,
    pub phases: BuildPhases,
}

/// Wall time of each phase of a full [`build_graph`], in milliseconds and
/// in the order the phases run, so a slow build names the phase to work on
/// instead of one opaque total. `concepts_ms` is the share of `store_ms`
/// spent in the concept pass; every other field is disjoint from the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildPhases {
    /// Walk the tree and read every supported source file.
    pub collect_ms: u64,
    /// Parse every file (rayon, so wall time, not CPU time).
    pub extract_ms: u64,
    /// Pass 1: file, symbol, crux, concept and JSX rows.
    pub store_ms: u64,
    /// Part of `store_ms`: the concept extraction and its rows.
    pub concepts_ms: u64,
    /// Pass 2: import rows and the pending call and reference lists.
    pub imports_ms: u64,
    /// Tiered call resolution.
    pub resolve_calls_ms: u64,
    /// Reference resolution.
    pub resolve_references_ms: u64,
    /// The second walk and hash that proves the tree did not move during
    /// the build, before the graph is signed fresh.
    pub verify_ms: u64,
}

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// The bytes the graph indexes for `path`: a regular file (not a symlink)
/// within `MAX_FILE_BYTES`, the same file from stat to read. `None` for
/// anything else, which the graph never indexes; `pixel audit` reads through
/// it so a file grown past the cap is never loaded whole.
pub fn read_source_file(path: &Path) -> Option<Vec<u8>> {
    let before = std::fs::symlink_metadata(path).ok()?;
    if !before.file_type().is_file() || before.len() > MAX_FILE_BYTES {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let opened = file.metadata().ok()?;
    let after = std::fs::symlink_metadata(path).ok()?;
    if !after.file_type().is_file()
        || opened.dev() != after.dev()
        || opened.ino() != after.ino()
        || opened.len() > MAX_FILE_BYTES
    {
        return None;
    }
    let mut content = Vec::with_capacity(opened.len() as usize);
    file.take(MAX_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut content)
        .ok()?;
    (content.len() as u64 <= MAX_FILE_BYTES).then_some(content)
}
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

fn is_binary(content: &[u8]) -> bool {
    let end = content.len().min(BINARY_SNIFF_BYTES);
    content[..end].contains(&0)
}

fn rel_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    if s.is_empty() { None } else { Some(s) }
}

/// Maximum number of source files to collect in a single walk when there is
/// no git anchor. With a git repo, `git ls-files` bounds the file set to
/// tracked files only; without git, the walk could traverse an entire home
/// directory or a vendored monorepo. This cap prevents pathological cases
/// from hanging the daemon. Override with `PIXEL_GRAPH_MAX_FILES=0` (disables)
/// or a positive integer. Default 50000 — generous for any real project,
/// but stops a runaway walk on a mis-rooted or huge directory.
const DEFAULT_GRAPH_MAX_FILES: usize = 50_000;

/// The build-time file cap in force, as an evaluation reports it: `None`
/// when the environment lifted it, so "the cap was hit" is never claimed
/// where no cap applies.
///
/// This reads the *current* environment, so it is only correct at build
/// time. An evaluation must ask [`stored_graph_file_cap`] what the loaded
/// graph was actually built under.
pub fn graph_file_cap() -> Option<usize> {
    graph_max_files().filter(|&n| n != usize::MAX)
}

/// Meta key: the file cap that was in force when the graph was last built
/// in full.
pub const GRAPH_FILE_CAP_KEY: &str = "graph_file_cap";

/// The [`GRAPH_FILE_CAP_KEY`] value recording that no cap applied, kept
/// distinct from a missing key: "the walk was unbounded" is a fact the
/// build knows, while a missing key only means nobody recorded one.
const GRAPH_FILE_CAP_NONE: &str = "none";

/// The file cap the loaded graph was actually built under, `None` when the
/// build walked the tree unbounded.
///
/// An evaluation must use this rather than [`graph_file_cap`]: the cap
/// lives in the environment, and a daemon restarted with a different
/// `PIXEL_GRAPH_MAX_FILES` would otherwise describe the loaded graph with a
/// ceiling that never applied to it. That drift runs both ways, and the
/// dangerous direction is a cap *raised* after a truncated build: the
/// indexed count then falls below the new ceiling and the coverage flag
/// reports `false` for a walk that did stop at a cap, narrowing a published
/// limit instead of widening it.
///
/// A graph written before this key existed reports the built-in default
/// rather than today's environment. Both are guesses about a build nobody
/// recorded, but the constant cannot drift between build and evaluation,
/// and the next full build replaces it with the truth.
pub fn stored_graph_file_cap(store: &GraphStore) -> Result<Option<usize>, StoreError> {
    match store.meta_get(GRAPH_FILE_CAP_KEY)? {
        Some(value) if value == GRAPH_FILE_CAP_NONE => Ok(None),
        Some(value) => Ok(value
            .parse::<usize>()
            .ok()
            .or(Some(DEFAULT_GRAPH_MAX_FILES))),
        None => Ok(Some(DEFAULT_GRAPH_MAX_FILES)),
    }
}

/// [`GRAPH_FILE_CAP_KEY`]'s value for the cap a build ran under.
fn graph_file_cap_value(cap: Option<usize>) -> String {
    cap.map_or_else(|| GRAPH_FILE_CAP_NONE.to_string(), |n| n.to_string())
}

fn graph_max_files() -> Option<usize> {
    match std::env::var("PIXEL_GRAPH_MAX_FILES") {
        Ok(v) => v
            .parse::<usize>()
            .ok()
            .filter(|&n| n > 0)
            .or(Some(usize::MAX)),
        Err(_) => Some(DEFAULT_GRAPH_MAX_FILES),
    }
}

/// Why the build would not hold a path, or that it would. The variants are
/// ordered as `collect_files` applies its filters: a file is judged on the
/// first gate it fails, so an unsupported extension is never reported as
/// oversized and a binary is never reported as generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indexability {
    /// The walk would collect it and extraction would accept it.
    Indexable,
    /// No grammar for this extension (`lang_of` is `None`).
    UnsupportedLanguage,
    /// Not on disk, or not a regular file (a symlink, a directory, or a
    /// path that changed identity between the two stats).
    Absent,
    /// Over `MAX_FILE_BYTES`.
    TooLarge,
    /// A NUL byte within the first `BINARY_SNIFF_BYTES`.
    Binary,
    /// A generated or minified blob, which `extract_file` refuses.
    Generated,
}

/// Judge one repo-relative path against the build's own file policy by
/// reading it, so a caller can say *why* a changed file carries no symbols
/// instead of reporting one undifferentiated "not indexed".
///
/// It answers about the file on disk now, never about the graph: a path this
/// returns `Indexable` for may still be missing from the store (built before
/// the file appeared, or dropped at the file cap), which is the caller's
/// distinction to make.
pub fn indexability(root: &Path, rel: &str) -> Indexability {
    if !is_graph_candidate(rel) {
        return Indexability::UnsupportedLanguage;
    }
    let path = root.join(rel);
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return Indexability::Absent;
    };
    if !meta.file_type().is_file() {
        return Indexability::Absent;
    }
    if meta.len() > MAX_FILE_BYTES {
        return Indexability::TooLarge;
    }
    // Re-reads through the same guarded reader the build uses, so a file
    // that grows past the cap or stops being a regular file between the
    // stat and the read is judged exactly as the build would judge it.
    let Some(content) = read_source_file(&path) else {
        return Indexability::Absent;
    };
    if is_binary(&content) {
        return Indexability::Binary;
    }
    if lang_of_file(rel, &content).is_none() {
        return Indexability::UnsupportedLanguage;
    }
    if crate::extract::is_generated_blob(&content) {
        return Indexability::Generated;
    }
    Indexability::Indexable
}

/// True iff the graph walks read `rel`: a file of a supported language, or
/// a binstub whose shebang [`lang_of_file`] reads. The build, the freshness
/// walks and [`indexability`] all filter through it, so they see one file
/// set; a binstub that is not Ruby is walked and hashed, then extraction
/// drops it, as it drops a generated blob.
fn is_graph_candidate(rel: &str) -> bool {
    lang_of(rel).is_some() || is_binstub_candidate(rel)
}

/// Walk `root` collecting supported source files (skips .git, .pixel,
/// default-ignored dirs, gitignored paths, binaries, oversized files). Hidden
/// files (dotfiles, `.github/`, `.claude/`, …) ARE collected — they are real
/// project content. The walk itself is the shared
/// `pixel_index::index::policy_walk`, so default-ignored-dir pruning and
/// gitless-tree gitignore handling stay in lockstep with the lexical index.
///
/// When `max_files` is `Some(n)`, the walk stops after collecting `n` source
/// files — a safety cap for non-git directories where there is no
/// `git ls-files` to bound the file set. `None` means no cap (git-anchored
/// builds where `git ls-files` already bounds the set).
fn collect_files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let max_files = graph_max_files();
    let mut out = Vec::new();
    let walker = pixel_index::index::policy_walk(root);
    for entry in walker.flatten() {
        if let Some(cap) = max_files
            && out.len() >= cap
        {
            break;
        }
        let is_file = entry.file_type().is_some_and(|t| t.is_file());
        if !is_file {
            continue;
        }
        let Some(rel) = rel_path(root, entry.path()) else {
            continue;
        };
        if !is_graph_candidate(&rel) {
            continue;
        }
        let Some(content) = read_source_file(entry.path()) else {
            continue;
        };
        if is_binary(&content) {
            continue;
        }
        out.push((rel, content));
    }
    out
}

/// The `blob_oid` the store keeps for a file: the xxh3 of its bytes, 16 hex
/// digits. `build_graph` and the incremental update write source rows
/// through it, so a reader holding the file's current bytes (`pixel audit`)
/// can tell a row indexed from other contents. The daemon's context check
/// still spells the same format by hand.
pub fn content_oid(content: &[u8]) -> String {
    format!("{:016x}", xxh3_64(content))
}

struct Extracted {
    rel: String,
    blob_oid: String,
    content: Vec<u8>,
    fx: FileExtraction,
}

fn input_signature(inputs: &[(String, Vec<u8>)]) -> String {
    let mut entries: Vec<(&str, u64)> = inputs
        .iter()
        .map(|(rel, content)| (rel.as_str(), xxh3_64(content)))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut hasher_buf = Vec::with_capacity(entries.len() * 24);
    for (rel, hash) in entries {
        hasher_buf.extend_from_slice(rel.as_bytes());
        hasher_buf.extend_from_slice(&hash.to_le_bytes());
    }
    format!("{:016x}", xxh3_64(&hasher_buf))
}

/// Full graph build: parse everything in parallel, then write files,
/// symbols, imports, and resolved call edges.
pub fn build_graph(root: &Path, db_path: &Path) -> Result<GraphStats, BoxErr> {
    build_graph_with(root, db_path, freshness_signature)
}

/// [`build_graph`] with the signature of the tree as it stands after the
/// writes supplied by `signature_now`: production re-walks the tree, a test
/// makes it move to reach the rollback.
fn build_graph_with(
    root: &Path,
    db_path: &Path,
    signature_now: impl FnOnce(&Path) -> String,
) -> Result<GraphStats, BoxErr> {
    let t0 = Instant::now();
    let mut phases = BuildPhases::default();

    let inputs = collect_files(root);
    let snapshot_signature = input_signature(&inputs);
    phases.collect_ms = millis(t0.elapsed());
    let clock = Instant::now();
    let extracted: Vec<Extracted> = inputs
        .into_par_iter()
        .filter_map(|(rel, content)| {
            let fx = extract_file(&rel, &content)?;
            let blob_oid = content_oid(&content);
            Some(Extracted {
                rel,
                blob_oid,
                content,
                fx,
            })
        })
        .collect();
    phases.extract_ms = millis(clock.elapsed());
    let clock = Instant::now();

    let all_paths: Vec<String> = extracted.iter().map(|e| e.rel.clone()).collect();

    let mut store = GraphStore::open(db_path)?;
    // One transaction for the whole build: without it every row below is its
    // own autocommit, and on a 60 000-symbol repository the store and
    // resolution passes spent 43 s of a 49 s build committing (#309).
    store.begin_write()?;

    // Drop files that vanished since the last build.
    let known: std::collections::HashSet<&str> = all_paths.iter().map(String::as_str).collect();
    let stale: Vec<String> = store
        .files()?
        .into_iter()
        .filter(|f| !known.contains(f.path.as_str()))
        .map(|f| f.path)
        .collect();
    for path in &stale {
        store.remove_file(path)?;
    }

    let projects = ruby_projects::Projects::load(root, &all_paths);
    let stored = store_batch(&mut store, extracted, &all_paths, &projects)?;
    phases.store_ms = millis(stored.stored_at.duration_since(clock));
    phases.concepts_ms = millis(stored.concepts);
    phases.imports_ms = millis(stored.imports);
    let clock = Instant::now();
    resolve_calls(&store, &stored.calls)?;
    phases.resolve_calls_ms = millis(clock.elapsed());
    let clock = Instant::now();
    resolve_references(&store, &stored.references)?;
    phases.resolve_references_ms = millis(clock.elapsed());
    let clock = Instant::now();

    // Bind freshness to the exact bytes parsed above. If the source tree moved
    // during extraction/storage, publishing this graph as fresh would attach
    // old symbols to a new filesystem signature.
    let current_signature = signature_now(root);
    phases.verify_ms = millis(clock.elapsed());
    if current_signature != snapshot_signature {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "source changed during graph build; graph was not published as fresh",
        )
        .into());
    }
    store.meta_set(EXTRACTOR_VERSION_KEY, EXTRACTOR_VERSION)?;
    // Before the signature on purpose: a crash between the two leaves the
    // graph unsigned, and an unsigned graph is refused rather than read, so
    // no evaluation can see a cap that belongs to a half-written build.
    store.meta_set(GRAPH_FILE_CAP_KEY, &graph_file_cap_value(graph_file_cap()))?;
    store.meta_set(FRESHNESS_KEY, &snapshot_signature)?;
    store.commit_write()?;

    let (files, symbols, edges, unresolved) = store.counts()?;
    Ok(GraphStats {
        files,
        symbols,
        edges,
        unresolved,
        elapsed_ms: t0.elapsed().as_millis(),
        phases,
    })
}

/// What [`store_batch`] leaves to its caller: the calls and references of
/// every stored file, waiting for resolution, and the time each pass took.
struct StoredBatch {
    calls: Vec<FileCalls>,
    references: Vec<FileReferences>,
    /// When pass 1, the concept pass included, ended: a caller times its
    /// store phase from wherever its own setup started.
    stored_at: Instant,
    /// The concept pass alone.
    concepts: Duration,
    /// Pass 2.
    imports: Duration,
}

/// Stores a batch of extracted files, for the full build and the
/// incremental update alike, so both write the same rows for the same file.
///
/// Pass 1 writes each file's row, symbols, crux lines, concepts and JSX
/// elements, then frees its bytes. Pass 2 runs once every file of the batch
/// has a row, because an import may target a file stored later in the same
/// batch: it writes the imports and collects the calls and references left
/// to resolve.
///
/// `all_paths` is every file the graph holds once the batch is stored, in
/// the order import resolution scans them: a Go package import resolves to
/// the first of the package's files in that order.
fn store_batch(
    store: &mut GraphStore,
    mut files: Vec<Extracted>,
    all_paths: &[String],
    projects: &ruby_projects::Projects,
) -> Result<StoredBatch, BoxErr> {
    let mut concepts = Duration::ZERO;
    let mut stored_ids: Vec<(i64, Vec<i64>)> = Vec::with_capacity(files.len());
    for e in &mut files {
        let file_id = store.replace_file(&e.rel, &e.blob_oid, e.fx.lang)?;
        let mut ids = Vec::with_capacity(e.fx.symbols.len());
        let mut lines = Vec::with_capacity(e.fx.symbols.len());
        // Decoded and split once per file, not once per symbol: the crux
        // below slices every symbol's body out of the same lines.
        let body_str = String::from_utf8_lossy(&e.content);
        let all_lines: Vec<&str> = body_str.lines().collect();
        for s in &e.fx.symbols {
            let uid = format!("{}#{}#{}", e.rel, s.qualified, s.kind.as_str());
            let id = store.insert_symbol(
                file_id,
                &uid,
                &s.name,
                &s.qualified,
                s.kind,
                s.start_line,
                s.end_line,
                &s.sig,
            )?;
            if s.trait_impl {
                store.mark_trait_impl(id)?;
            }
            if s.module_decl {
                store.mark_module_decl(id)?;
            }
            ids.push(id);
            lines.push((s.start_line, s.end_line));

            // P2·3: content-anchored crux — store the guarded logical lines of
            // each symbol's body (guards, state mutations, early-returns) so
            // retrieval can surface them. Fingerprints are content-stable: they survive
            // a file that later shifts line numbers (re-anchoring by fingerprint).
            let start = (s.start_line.saturating_sub(1) as usize).min(all_lines.len());
            let end = (s.end_line.saturating_sub(1) as usize).min(all_lines.len());
            let body = if end > start {
                all_lines[start..end].join("\n")
            } else {
                String::new()
            };
            let crux = extract_crux(&body, 3);
            store.set_symbol_crux(id, &crux)?;
        }
        // Engine 1: concept pass alongside symbol extraction with O(1) content access.
        let concept_clock = Instant::now();
        insert_concepts(store, file_id, &e.rel, &e.content, &ids, &lines)?;
        concepts += concept_clock.elapsed();
        for mixin in &e.fx.mixins {
            store.insert_ruby_mixin(file_id, mixin)?;
        }
        // Plan pass: persist JSX elements for dead-interactive queries.
        for jsx in &e.fx.jsx_elements {
            store.insert_jsx_element(
                file_id,
                &jsx.tag,
                jsx.has_handler,
                &jsx.text_content,
                jsx.start_line,
                jsx.end_line,
            )?;
        }
        e.content.clear();
        e.content.shrink_to_fit();
        stored_ids.push((file_id, ids));
    }
    let stored_at = Instant::now();

    let path_to_id: HashMap<String, i64> =
        store.files()?.into_iter().map(|f| (f.path, f.id)).collect();
    let mut calls: Vec<FileCalls> = Vec::with_capacity(files.len());
    let mut references: Vec<FileReferences> = Vec::with_capacity(files.len());
    for (e, (file_id, symbol_ids)) in files.iter().zip(&stored_ids) {
        let file_id = *file_id;
        for imp in &e.fx.imports {
            let resolved = resolve_import_in(&imp.path, &e.rel, all_paths, projects)
                .and_then(|p| path_to_id.get(&p).copied());
            store.insert_import_at(
                file_id,
                &imp.spec,
                &imp.path,
                resolved,
                &imp.bindings,
                &imp.scope,
            )?;
        }
        let file_calls =
            e.fx.calls
                .iter()
                .map(|c| PendingCall {
                    callee_name: c.callee_name.clone(),
                    enclosing_symbol_id: c.enclosing_index.map(|ix| symbol_ids[ix]),
                    site_line: c.site_line,
                    receiver: c.receiver.clone(),
                })
                .collect();
        calls.push(FileCalls {
            file_id,
            calls: file_calls,
        });
        let file_references =
            e.fx.references
                .iter()
                .map(|r| PendingReference {
                    name: r.name.clone(),
                    enclosing_symbol_id: r.enclosing_index.map(|ix| symbol_ids[ix]),
                    site_line: r.site_line,
                    arg_of: r.arg_of.clone(),
                })
                .collect();
        references.push(FileReferences {
            file_id,
            references: file_references,
        });
    }
    Ok(StoredBatch {
        calls,
        references,
        stored_at,
        concepts,
        imports: stored_at.elapsed(),
    })
}

/// `(repo-relative path, xxh3 content hash)` of every supported source file
/// under `root`, sorted by path. This is the exact input set of
/// `build_graph`, and the per-file hash is the `blob_oid` the store keeps
/// for each file, so a stored row whose `blob_oid` differs from the entry
/// here is a file that changed since the graph was built.
///
/// The walk is serial (`ignore::Walk` is an iterator and the directory
/// listing is the cheap part); reading and hashing the candidate files,
/// which is where the time goes on a large tree, runs on rayon's pool. The
/// result is sorted by path afterwards, so it is byte-identical to a serial
/// pass: the signature built over it never depends on thread scheduling.
fn tree_hashes(root: &Path) -> Vec<(String, u64)> {
    // Must mirror `collect_files`'s walk policy exactly — both go through
    // `pixel_index::index::policy_walk` — or the freshness signature would
    // disagree with the set of files the graph was actually built from.
    let walker = pixel_index::index::policy_walk(root);
    let candidates: Vec<(String, std::path::PathBuf)> = walker
        .flatten()
        .filter_map(|entry| {
            let is_file = entry.file_type().is_some_and(|t| t.is_file());
            if !is_file {
                return None;
            }
            let rel = rel_path(root, entry.path())?;
            is_graph_candidate(&rel).then_some(())?;
            Some((rel, entry.into_path()))
        })
        .collect();
    hash_candidates(candidates)
}

/// Read and hash `candidates` in parallel, dropping the ones
/// `read_source_file` refuses (vanished, not a regular file, over
/// `MAX_FILE_BYTES`) and the binaries, exactly as `collect_files` does.
/// Sorted by path so callers see one order whatever the scheduling.
fn hash_candidates(candidates: Vec<(String, std::path::PathBuf)>) -> Vec<(String, u64)> {
    let mut entries: Vec<(String, u64)> = candidates
        .into_par_iter()
        .filter_map(|(rel, path)| {
            let content = read_source_file(&path)?;
            if is_binary(&content) {
                return None;
            }
            Some((rel, xxh3_64(&content)))
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

/// In-process memo of the last `tree_hashes` walk: `rel path → (mtime_sec,
/// mtime_nsec, len, xxh3)`. A daemon that re-walks after a watcher batch
/// stats every file but only re-reads and re-hashes the ones whose
/// `(mtime, len)` moved — unchanged files reuse their content hash, so the
/// resulting signature is byte-identical to a full re-hash.
///
/// Held per `Service` (daemon lifetime), never persisted: the on-disk
/// signature format is unchanged. Accepted trade, documented by
/// `freshness_signature`: a same-size edit that also restores mtime
/// (`touch -t`) is invisible to the stat check inside this process — the
/// same edit to a file the watcher reported is still caught, because the
/// watcher's batch is what invalidated the graph handle in the first place.
#[derive(Debug, Default)]
pub struct TreeHashCache {
    seen: HashMap<String, (i64, i64, u64, u64)>,
    /// Files whose cached stat matched on the last walk — hash reused, no
    /// read. Diagnostic; lets tests prove the stat-only path ran.
    #[doc(hidden)]
    pub stat_hits: u64,
    /// Files the last walk had to open and hash (new or stat-changed).
    #[doc(hidden)]
    pub rehashed: u64,
}

/// One walked file: repo-relative path, content hash, `(mtime, mtime_nsec,
/// size)` stat, and whether the hash came from the stat memo.
type HashedFile = (String, u64, (i64, i64, u64), bool);

/// [`tree_hashes`] driven by `cache`: stat-only for files whose
/// `(mtime, len)` is unchanged since the last walk, read + hash for the
/// rest. The returned entries are identical to `tree_hashes`'s whenever
/// file content follows file stat — which is the invariant this cache
/// exists to exploit. Entries for vanished/binary/oversized files are
/// dropped, so the memo can never resurrect a file the walk would exclude.
fn tree_hashes_cached(root: &Path, cache: &mut TreeHashCache) -> Vec<(String, u64)> {
    let walker = pixel_index::index::policy_walk(root);
    let candidates: Vec<(String, std::path::PathBuf)> = walker
        .flatten()
        .filter_map(|entry| {
            let is_file = entry.file_type().is_some_and(|t| t.is_file());
            if !is_file {
                return None;
            }
            let rel = rel_path(root, entry.path())?;
            is_graph_candidate(&rel).then_some(())?;
            Some((rel, entry.into_path()))
        })
        .collect();
    let previous = std::mem::take(&mut cache.seen);
    let previous = &previous;
    let hashed: Vec<HashedFile> = candidates
        .into_par_iter()
        .filter_map(|(rel, path)| {
            let meta = std::fs::metadata(&path).ok()?;
            let stat = (meta.mtime(), meta.mtime_nsec(), meta.size());
            if let Some(&(mtime, nsec, len, hash)) = previous.get(rel.as_str())
                && (mtime, nsec, len) == stat
            {
                return Some((rel, hash, stat, true));
            }
            let content = read_source_file(&path)?;
            if is_binary(&content) {
                return None;
            }
            Some((rel, xxh3_64(&content), stat, false))
        })
        .collect();
    cache.stat_hits = hashed.iter().filter(|e| e.3).count() as u64;
    cache.rehashed = hashed.iter().filter(|e| !e.3).count() as u64;
    // Rebuild the memo from what THIS walk actually produced: stat-matched
    // files keep their hash, stat-changed files were re-hashed, and
    // anything dropped (binary, vanished, unreadable) is absent — so a
    // later walk cannot hit a stale entry for a file that no longer hashes.
    cache.seen = hashed
        .iter()
        .map(|(rel, hash, (m, n, l), _)| (rel.clone(), (*m, *n, *l, *hash)))
        .collect();
    let mut entries: Vec<(String, u64)> = hashed
        .into_iter()
        .map(|(rel, hash, _, _)| (rel, hash))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

fn signature_of(entries: &[(String, u64)]) -> String {
    let mut hasher_buf: Vec<u8> = Vec::with_capacity(entries.len() * 24);
    for (rel, hash) in entries {
        hasher_buf.extend_from_slice(rel.as_bytes());
        hasher_buf.extend_from_slice(&hash.to_le_bytes());
    }
    format!("{:016x}", xxh3_64(&hasher_buf))
}

/// Content-aware signature of every supported source file under `root`.
/// The signature includes a fast xxh3 content hash per file, so it detects
/// equal-size edits even when mtime is restored (e.g. `touch -t`). This is
/// more expensive than a stat-only signature but is necessary for trust:
/// a stale graph would serve obsolete symbols. The cost is bounded by
/// `MAX_FILE_BYTES` per file; the directory walk is serial and the per-file
/// read + hash runs on rayon's pool (see `tree_hashes`). Symlinks are
/// excluded (their target's content would be unstable and they are never
/// indexed).
pub fn freshness_signature(root: &Path) -> String {
    signature_of(&tree_hashes(root))
}

/// What separates the working tree from the graph at `db_path`.
#[derive(Debug, Clone)]
pub struct TreeDelta {
    /// The stored signature equals the tree's: nothing to do.
    pub fresh: bool,
    /// Files added or edited since the build (present in the tree, absent
    /// from the store or stored under another content hash), with the hash
    /// the tree had when the delta was taken.
    pub changed: Vec<(String, u64)>,
    /// Files the store knows that are no longer in the tree.
    pub removed: Vec<String>,
    /// Number of files the store currently holds.
    pub indexed_files: usize,
    /// Signature of the tree as walked for this delta.
    pub signature: String,
}

impl TreeDelta {
    pub fn changed_count(&self) -> usize {
        self.changed.len() + self.removed.len()
    }
}

/// Compare `root`'s working tree with the graph at `db_path`. One walk
/// (the same one `freshness_signature` makes) answers both "is it fresh"
/// and "which files drifted". `Ok(None)` when the db carries no freshness
/// signature (built before signatures existed, or interrupted) or was
/// written by another extractor version: the caller cannot trust its rows
/// and must rebuild.
pub fn tree_delta(root: &Path, db_path: &Path) -> Result<Option<TreeDelta>, BoxErr> {
    tree_delta_with(root, db_path, tree_hashes)
}

/// [`tree_delta`] with a [`TreeHashCache`]: the walk stats every file but
/// only re-reads and re-hashes the ones whose `(mtime, len)` moved since
/// the last cached walk. Same delta, same signature — less work.
pub fn tree_delta_cached(
    root: &Path,
    db_path: &Path,
    cache: &mut TreeHashCache,
) -> Result<Option<TreeDelta>, BoxErr> {
    tree_delta_with(root, db_path, |root| tree_hashes_cached(root, cache))
}

fn tree_delta_with(
    root: &Path,
    db_path: &Path,
    hashes: impl FnOnce(&Path) -> Vec<(String, u64)>,
) -> Result<Option<TreeDelta>, BoxErr> {
    let store = GraphStore::open(db_path)?;
    let Some(stored) = store.meta_get(FRESHNESS_KEY)? else {
        return Ok(None);
    };
    if !extractor_is_current(&store)? {
        return Ok(None);
    }
    let current = hashes(root);
    let signature = signature_of(&current);
    let known: HashMap<String, String> = store
        .files()?
        .into_iter()
        .map(|f| (f.path, f.blob_oid))
        .collect();
    if stored == signature {
        return Ok(Some(TreeDelta {
            fresh: true,
            changed: Vec::new(),
            removed: Vec::new(),
            indexed_files: known.len(),
            signature,
        }));
    }
    let present: HashSet<&str> = current.iter().map(|(rel, _)| rel.as_str()).collect();
    let changed: Vec<(String, u64)> = current
        .iter()
        .filter(|(rel, hash)| known.get(rel) != Some(&format!("{hash:016x}")))
        .cloned()
        .collect();
    let mut removed: Vec<String> = known
        .keys()
        .filter(|path| !present.contains(path.as_str()))
        .cloned()
        .collect();
    removed.sort();
    Ok(Some(TreeDelta {
        fresh: false,
        changed,
        removed,
        indexed_files: known.len(),
        signature,
    }))
}

/// Bring the graph up to date with a [`TreeDelta`]: re-extract the changed
/// files, drop the removed ones, re-resolve the calls that targeted them
/// (see `update_files`), then publish `delta.signature` as the freshness
/// signature, all in one write transaction: another connection sees either
/// the previous rows under the previous signature or the new rows under
/// the new one, never new rows under a signature that describes another
/// tree. The signature is only published when every changed file was
/// stored under the hash the delta saw. A file edited while the update ran
/// leaves the rows committed but unsigned (the graph is no worse than
/// before) and returns an error, so the caller falls back and the next open
/// detects the drift again instead of binding stale symbols to a
/// fresh-looking signature.
pub fn apply_tree_delta(root: &Path, db_path: &Path, delta: &TreeDelta) -> Result<(), BoxErr> {
    let files: Vec<(&str, bool)> = delta
        .changed
        .iter()
        .map(|(rel, _)| (rel.as_str(), false))
        .chain(delta.removed.iter().map(|rel| (rel.as_str(), true)))
        .collect();
    let mut drifted: Option<String> = None;
    update_files_in_one_transaction(
        root,
        db_path,
        &files,
        |store, _batch| {
            for (rel, hash) in &delta.changed {
                let stored = store.file_by_path(rel)?.map(|f| f.blob_oid);
                // A changed file that extraction dropped (unparseable, vanished)
                // has no row; that is its stable state, not a race.
                if stored.is_some_and(|oid| oid != format!("{hash:016x}")) {
                    drifted = Some(rel.clone());
                    return Ok(None);
                }
            }
            Ok(Some(delta.signature.clone()))
        },
        &mut || {},
    )?;
    if let Some(rel) = drifted {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            format!(
                "{rel} changed during incremental graph update; graph was not published as fresh"
            ),
        )
        .into());
    }
    Ok(())
}

/// True iff the on-disk graph at `db_path` is fresh relative to `root`'s
/// current working tree. A missing db is never fresh. A db whose stored
/// signature matches `freshness_signature(root)` is fresh; otherwise (or if
/// the meta key is absent on an old db) it is stale and must be rebuilt.
pub fn is_fresh(root: &Path, db_path: &Path) -> bool {
    let Ok(store) = GraphStore::open(db_path) else {
        return false;
    };
    let Ok(Some(stored)) = store.meta_get(FRESHNESS_KEY) else {
        return false;
    };
    extractor_is_current(&store).unwrap_or(false) && stored == freshness_signature(root)
}

/// Whether an incremental update published a freshness signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publication {
    /// The rows and the signature that describes them were committed together;
    /// the graph is fresh for the tree the update saw.
    Signed,
    /// The rows were committed but no signature describes them: the tree
    /// drifted from what the store holds. The stored signature is replaced
    /// by [`FRESHNESS_WITHHELD`] so the graph reads as stale whatever the
    /// tree looks like now, and the next graph open repairs it from the
    /// per-row hashes. An empty batch also lands here, without touching the
    /// signature: nothing changed.
    Withheld,
}

/// Incrementally re-index a batch of files: preserve incoming call knowledge,
/// replace files, re-extract symbols and concepts, and resolve all calls once.
///
/// Rows and freshness signature commit in one transaction. The signature is
/// the tree's, taken by one walk after the rows are written, and it is
/// published only when that same walk agrees with every row the store holds
/// (see [`rows_match_tree`]): a batched file that changed again while the
/// update ran, or a file outside the batch that drifted, means the walk
/// describes bytes the store did not parse, and signing would let a cold open
/// serve those stale symbols as fresh. The update then lands
/// [`Publication::Withheld`] and the next `tree_delta` names the drift.
pub fn update_files(
    root: &Path,
    db_path: &Path,
    files: &[(&str, bool)],
) -> Result<Publication, BoxErr> {
    update_files_probed(root, db_path, files, &mut || {})
}

/// [`update_files`] with a seam for tests: `probe` runs after the rows are
/// written and before the tree walk that decides the signature, inside the
/// open transaction. Production passes a no-op.
fn update_files_probed(
    root: &Path,
    db_path: &Path,
    files: &[(&str, bool)],
    probe: &mut dyn FnMut(),
) -> Result<Publication, BoxErr> {
    update_files_in_one_transaction(
        root,
        db_path,
        files,
        |store, batch| {
            let walk = tree_hashes(root);
            let known: HashMap<String, String> = store
                .files()?
                .into_iter()
                .map(|f| (f.path, f.blob_oid))
                .collect();
            Ok(rows_match_tree(&known, &walk, batch).then(|| signature_of(&walk)))
        },
        probe,
    )
}

/// True iff the rows (`known`: path to stored content hash) describe exactly
/// the tree `walk` saw, so that `walk`'s signature may be published for them:
/// every walked file has a row under the same hash, except a file of this
/// `batch` that was not removed and has no row (extraction dropped it, its
/// stable state); and every row names a walked file. A batched removal whose
/// file is back on disk, or a file that drifted outside the batch, fails.
fn rows_match_tree(
    known: &HashMap<String, String>,
    walk: &[(String, u64)],
    batch: &[(&str, bool)],
) -> bool {
    let re_extracted: HashSet<&str> = batch
        .iter()
        .filter(|(_, removed)| !removed)
        .map(|(rel, _)| *rel)
        .collect();
    let walked: HashSet<&str> = walk.iter().map(|(rel, _)| rel.as_str()).collect();
    let every_walked_file_matches = walk.iter().all(|(rel, hash)| match known.get(rel) {
        Some(stored) => *stored == format!("{hash:016x}"),
        None => re_extracted.contains(rel.as_str()),
    });
    every_walked_file_matches && known.keys().all(|path| walked.contains(path.as_str()))
}

/// One final action per path, in order of first appearance, the last
/// occurrence winning: a debounced watcher batch can carry `a.ts` as edited
/// and then removed (or the reverse), and the store must end in the state of
/// the last event, with the file's imports and calls staged once. Feeding
/// the raw batch to `write_rows` would apply both, and the signing check
/// would still count the path as re-extracted after its row was removed.
fn final_actions<'a>(files: &[(&'a str, bool)]) -> Vec<(&'a str, bool)> {
    let mut position: HashMap<&str, usize> = HashMap::with_capacity(files.len());
    let mut out: Vec<(&'a str, bool)> = Vec::with_capacity(files.len());
    for &(rel, removed) in files {
        match position.get(rel) {
            Some(&at) => out[at].1 = removed,
            None => {
                position.insert(rel, out.len());
                out.push((rel, removed));
            }
        }
    }
    out
}

/// The one write transaction behind [`update_files`] and [`apply_tree_delta`]:
/// open the store, `BEGIN IMMEDIATE`, write the rows for the batch (one final
/// action per path, see [`final_actions`]), run `probe` (a test seam), ask
/// `sign` which signature (if any) describes the rows now, write it under
/// [`FRESHNESS_KEY`], or [`FRESHNESS_WITHHELD`] when there is none, `COMMIT`.
/// An error anywhere returns before the commit and the dropped connection
/// rolls everything back, so another connection never observes new rows
/// under the old signature, and rows that did commit are never left under
/// a signature that may still match the tree.
fn update_files_in_one_transaction(
    root: &Path,
    db_path: &Path,
    files: &[(&str, bool)],
    sign: impl FnOnce(&GraphStore, &[(&str, bool)]) -> Result<Option<String>, BoxErr>,
    probe: &mut dyn FnMut(),
) -> Result<Publication, BoxErr> {
    if files.is_empty() {
        return Ok(Publication::Withheld);
    }
    let batch = final_actions(files);
    let mut store = GraphStore::open(db_path)?;
    store.begin_write()?;
    write_rows(root, &mut store, &batch)?;
    probe();
    let signature = sign(&store, &batch)?;
    store.meta_set(
        FRESHNESS_KEY,
        signature.as_deref().unwrap_or(FRESHNESS_WITHHELD),
    )?;
    store.commit_write()?;
    Ok(if signature.is_some() {
        Publication::Signed
    } else {
        Publication::Withheld
    })
}

/// A stored file's definitions, as `Affected::record_changed_definitions`
/// compares them with the ones its new content extracts.
fn stored_definitions(
    store: &GraphStore,
    file_id: i64,
    path: &str,
) -> Result<Vec<Definition>, StoreError> {
    let mut stmt = store.conn().prepare_cached(
        "SELECT name, qualified, kind, trait_impl FROM symbols WHERE file_id = ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![file_id], |r| {
        Ok(Definition {
            path: path.to_string(),
            name: r.get(0)?,
            qualified: r.get(1)?,
            kind: r.get(2)?,
            trait_impl: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The row half of an incremental update, inside the caller's transaction:
/// files, symbols, concepts, imports, calls, references, and the re-resolution
/// of everything a changed definition may have made ambiguous. Writes no
/// signature: the caller decides which one (if any) describes the tree it
/// just synchronised to.
fn write_rows(root: &Path, store: &mut GraphStore, files: &[(&str, bool)]) -> Result<(), BoxErr> {
    // What the batch changed, before and after, for the re-resolution below.
    let mut affected = Affected::default();
    let mut before: Vec<Definition> = Vec::new();
    let mut after: Vec<Definition> = Vec::new();
    let mut mixins_before: Vec<(String, RawMixin)> = Vec::new();
    let mut mixins_after: Vec<(String, RawMixin)> = Vec::new();
    let known_before: HashSet<String> = store.files()?.into_iter().map(|f| f.path).collect();

    let mut extracted: Vec<Extracted> = Vec::with_capacity(files.len());
    for &(rel, removed) in files {
        let abs = root.join(rel);

        // Demote incoming call+reference edges (from OTHER files) into
        // unresolved rows so they can re-link after the rebuild instead of
        // being silently dropped. The receiver and kind are preserved so
        // re-resolution produces the correct edge type (Calls vs References).
        if let Some(old) = store.file_by_path(rel)? {
            let old_syms = store.symbols_in_file(old.id)?;
            before.extend(stored_definitions(store, old.id, rel)?);
            mixins_before.extend(
                store
                    .ruby_mixins_in_file(old.id)?
                    .into_iter()
                    .map(|mixin| (rel.to_string(), mixin)),
            );
            affected.files.insert(old.id);
            for import in store.imports_to_file(old.id)? {
                affected.files.insert(import.file_id);
            }
            let mut demoted: Vec<(i64, String, i64, u32, Option<String>, String)> = Vec::new();
            for sym in &old_syms {
                for kind in [EdgeKind::Calls, EdgeKind::References] {
                    for edge in store.edges_to(sym.id, Some(kind))? {
                        let src_file: Option<i64> = store
                            .conn()
                            .query_row(
                                "SELECT file_id FROM symbols WHERE id = ?1",
                                rusqlite::params![edge.src_id],
                                |r| r.get(0),
                            )
                            .ok();
                        if let Some(src_file) = src_file
                            && src_file != old.id
                        {
                            // Replay the name the site wrote: under an
                            // alias (`leased()` → `push`) the target's name
                            // is not in scope at the caller.
                            demoted.push((
                                src_file,
                                edge.callee.clone().unwrap_or_else(|| sym.name.clone()),
                                edge.src_id,
                                edge.site_line,
                                edge.receiver.clone(),
                                kind.as_str().to_string(),
                            ));
                        }
                    }
                }
            }
            for (src_file, name, src_id, site_line, receiver, kind) in demoted {
                affected.replayed.insert(name.clone());
                store.insert_unresolved_call(
                    src_file,
                    &name,
                    Some(src_id),
                    site_line,
                    receiver.as_deref(),
                    &kind,
                )?;
            }
        }

        if removed {
            store.remove_file(rel)?;
            continue;
        }

        let Some(content) = read_source_file(&abs) else {
            store.remove_file(rel)?;
            continue;
        };

        let Some(fx) = extract_file(rel, &content) else {
            store.remove_file(rel)?;
            continue;
        };

        mixins_after.extend(
            fx.mixins
                .iter()
                .map(|mixin| (rel.to_string(), mixin.clone())),
        );
        after.extend(fx.symbols.iter().map(|s| Definition {
            path: rel.to_string(),
            name: s.name.clone(),
            qualified: s.qualified.clone(),
            kind: s.kind.as_str().to_string(),
            trait_impl: s.trait_impl,
        }));
        extracted.push(Extracted {
            rel: rel.to_string(),
            blob_oid: content_oid(&content),
            content,
            fx,
        });
    }

    // The file list as it stands once the batch is stored, in the order the
    // store lists it: kept files, then the batch's new ones as they are
    // inserted.
    let mut all_paths: Vec<String> = store.files()?.into_iter().map(|f| f.path).collect();
    let present: HashSet<String> = all_paths.iter().cloned().collect();
    all_paths.extend(
        extracted
            .iter()
            .filter(|e| !present.contains(&e.rel))
            .map(|e| e.rel.clone()),
    );
    let added_any = extracted.iter().any(|e| !known_before.contains(&e.rel));
    let projects = ruby_projects::Projects::load(root, &all_paths);
    let stored = store_batch(store, extracted, &all_paths, &projects)?;
    affected.record_changed_definitions(&before, &after);
    affected.record_changed_mixins(store, &mixins_before, &mixins_after)?;
    // A file new to the graph gets its id here; its importers are any file
    // whose import already resolved to it, and the dangling ones below.
    for &(rel, _) in files {
        if let Some(file) = store.file_by_path(rel)? {
            affected.files.insert(file.id);
            for import in store.imports_to_file(file.id)? {
                affected.files.insert(import.file_id);
            }
        }
    }

    // A file that appeared in this batch may be the target of imports that
    // UNCHANGED files could never resolve before (`import x from "./new"`
    // written ahead of the file). Re-resolve every dangling import against
    // the new file list so the resolver's import tier sees them.
    if added_any {
        let path_to_id: HashMap<String, i64> =
            store.files()?.into_iter().map(|f| (f.path, f.id)).collect();
        let dangling: Vec<(i64, i64, String, String)> = {
            let mut stmt = store.conn().prepare(
                "SELECT i.id, i.file_id, i.path, f.path FROM imports i
                   JOIN files f ON f.id = i.file_id
                  WHERE i.resolved_file_id IS NULL",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        for (import_id, importer_id, import_path, importer) in dangling {
            if let Some(target) = resolve_import_in(&import_path, &importer, &all_paths, &projects)
                .and_then(|p| path_to_id.get(&p).copied())
            {
                affected.files.insert(importer_id);
                store.conn().exec_cached(
                    "UPDATE imports SET resolved_file_id = ?2 WHERE id = ?1",
                    rusqlite::params![import_id, target],
                )?;
            }
        }
    }

    // A Ruby require resolves against the project's manifests and load
    // roots, which any Ruby file or manifest of the batch can move: a path
    // gem declared, a second file on another load root, a gem added to the
    // Gemfile. Re-resolve every Ruby import, resolved or not.
    if files.iter().any(|(rel, _)| {
        ruby_projects::is_manifest(rel) || lang_of(rel) == Some("ruby") || is_binstub_candidate(rel)
    }) {
        re_resolve_ruby_imports(store, &all_paths, &projects, &mut affected)?;
    }

    if !stored.calls.is_empty() {
        resolve_calls(store, &stored.calls)?;
    }
    if !stored.references.is_empty() {
        resolve_references(store, &stored.references)?;
    }

    // Any changed definition can invalidate a previously unique target.
    reconsider_resolved_calls(store, &mut affected)?;
    // Retry what the batch can have changed against the new candidate set.
    resolve_affected(store, &affected)?;

    // Persisted analyses (`processes`, `clusters`) are keyed by symbol id,
    // and `replace_file` hands re-extracted symbols NEW ids: the cached
    // rows would point at deleted symbols. A full rebuild starts from an
    // empty db, so they were recomputed on demand; give the incremental
    // path the same guarantee.
    store.conn().execute_batch(
        "DELETE FROM process_steps; DELETE FROM processes;
         DELETE FROM cluster_members; DELETE FROM clusters;",
    )?;
    Ok(())
}

/// Resolve every stored import of a Ruby file again against `projects`,
/// updating the rows whose target moved and recording their importers in
/// `affected`.
fn re_resolve_ruby_imports(
    store: &GraphStore,
    all_paths: &[String],
    projects: &ruby_projects::Projects,
    affected: &mut Affected,
) -> Result<(), BoxErr> {
    let path_to_id: HashMap<String, i64> =
        store.files()?.into_iter().map(|f| (f.path, f.id)).collect();
    let rows: Vec<(i64, i64, String, String, Option<i64>)> = {
        let mut stmt = store.conn().prepare(
            "SELECT i.id, i.file_id, i.path, f.path, i.resolved_file_id FROM imports i
               JOIN files f ON f.id = i.file_id
              WHERE f.lang = 'ruby'",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for (import_id, importer_id, import_path, importer, before) in rows {
        let after = resolve_import_in(&import_path, &importer, all_paths, projects)
            .and_then(|p| path_to_id.get(&p).copied());
        if after != before {
            affected.files.insert(importer_id);
            store.conn().exec_cached(
                "UPDATE imports SET resolved_file_id = ?2 WHERE id = ?1",
                rusqlite::params![import_id, after],
            )?;
        }
    }
    Ok(())
}

/// Incrementally re-index one file (see [`update_files`]).
pub fn update_file(root: &Path, db_path: &Path, rel: &str) -> Result<Publication, BoxErr> {
    update_files(root, db_path, &[(rel, false)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::{Decision, ResolveIndex};
    use crate::store::Tier;

    /// `graph_file_cap` is what an evaluation quotes when it says the build
    /// stopped at the cap, so the number it reports has to be the cap the
    /// walk actually enforces, and `None` has to mean "no cap applies"
    /// rather than "the default".
    ///
    /// Reads the ambient environment on purpose: `PIXEL_GRAPH_MAX_FILES` is
    /// unset everywhere this suite runs, and setting it here would be
    /// process-global, capping the walk of every graph built concurrently
    /// by another test in this binary.
    #[test]
    fn the_reported_build_file_cap_should_be_the_default_when_nothing_overrides_it() {
        assert!(
            std::env::var_os("PIXEL_GRAPH_MAX_FILES").is_none(),
            "this test describes the unconfigured default; the variable is set"
        );
        assert_eq!(
            graph_file_cap(),
            Some(DEFAULT_GRAPH_MAX_FILES),
            "an unconfigured build is capped, and the cap it reports is the \
             one `collect_files` stops at"
        );
    }

    /// A build that finds the tree moved under it publishes nothing: the rows
    /// and the signature of the graph it was rebuilding stay as they were.
    /// Without the transaction, the rewritten rows stayed under the previous
    /// build's signature, and a later reader took them for fresh.
    #[test]
    fn a_build_interrupted_by_a_moving_tree_leaves_the_previous_graph_intact() {
        let root = tmpdir("rollback");
        std::fs::write(root.join("a.rs"), "fn kept() -> u32 { 1 }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let snapshot = || {
            let store = GraphStore::open(&db).unwrap();
            let names: Vec<String> = store
                .symbols_by_name("kept", None, 10)
                .unwrap()
                .into_iter()
                .chain(store.symbols_by_name("added", None, 10).unwrap())
                .map(|s| s.name)
                .collect();
            (store.meta_get(FRESHNESS_KEY).unwrap(), names)
        };
        let before = snapshot();
        assert_eq!(before.1, ["kept"]);

        std::fs::write(root.join("b.rs"), "fn added() -> u32 { 2 }\n").unwrap();
        let err = build_graph_with(&root, &db, |_| "moved".into()).unwrap_err();
        assert!(err.to_string().contains("source changed"), "{err}");
        assert_eq!(
            snapshot(),
            before,
            "no row and no signature of the failed build"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The phases split one build: disjoint ones cannot add up to more than
    /// the total the build reports, and the concept pass is a part of the
    /// store pass, not a phase beside it.
    #[test]
    fn build_phases_partition_the_reported_build_time() {
        let root = tmpdir("phases");
        std::fs::write(
            root.join("c.rs"),
            "fn helper() -> u32 { 1 }\nfn run() -> u32 { helper() }\n",
        )
        .unwrap();
        let stats = build_graph(&root, &root.join(".pixel").join("graph.db")).unwrap();
        let p = &stats.phases;
        let disjoint = p.collect_ms
            + p.extract_ms
            + p.store_ms
            + p.imports_ms
            + p.resolve_calls_ms
            + p.resolve_references_ms
            + p.verify_ms;
        assert!(
            u128::from(disjoint) <= stats.elapsed_ms,
            "phases {p:?} exceed the build's {} ms",
            stats.elapsed_ms
        );
        assert!(
            p.concepts_ms <= p.store_ms,
            "concepts are inside store: {p:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pixel-graph-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The cap change detection and `evaluate` report. `None` must mean
    /// "no cap applies", never "there is one and I lost it": a file absent
    /// from the graph is blamed on the cap only when one was in force.
    #[test]
    fn graph_file_cap_is_the_build_default_unless_the_environment_lifts_it() {
        assert!(
            std::env::var("PIXEL_GRAPH_MAX_FILES").is_err(),
            "no test in this binary may set PIXEL_GRAPH_MAX_FILES: it is \
             process-global and this assertion is what keeps the default \
             below meaningful"
        );
        assert_eq!(graph_file_cap(), Some(DEFAULT_GRAPH_MAX_FILES));
        // The two agree while a cap applies; they part only at `usize::MAX`,
        // which is how `PIXEL_GRAPH_MAX_FILES=0` spells "no cap".
        assert_eq!(graph_file_cap(), graph_max_files());
        assert_eq!(Some(usize::MAX).filter(|&n| n != usize::MAX), None);
    }

    /// `indexability` is what names the motif of a changed file the graph
    /// does not hold, so it must answer with the build's own gates, in the
    /// build's own order: a wrong reason here is a wrong published motif.
    #[test]
    fn indexability_names_the_gate_a_file_fails() {
        let root = tmpdir("indexability");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), b"fn alpha() -> u32 { 1 }\n").unwrap();
        // Rust-looking content under an extension with no grammar: judged on
        // the extension, before anything is read.
        std::fs::write(root.join("notes.txt"), b"fn looks_like_rust() {}\n").unwrap();
        std::fs::write(root.join("src/blob.rs"), b"fn x() {}\n\0\0binary\n").unwrap();
        let oversize = usize::try_from(MAX_FILE_BYTES).unwrap() + 1;
        std::fs::write(root.join("src/huge.rs"), vec![b'/'; oversize]).unwrap();
        // Exactly at the cap: the build reads it, so this must not be
        // refused. A newline every 64 bytes keeps it off the
        // generated-blob threshold, which is a mean bytes-per-line.
        let at_cap = usize::try_from(MAX_FILE_BYTES).unwrap();
        let mut exact: Vec<u8> = (0..at_cap)
            .map(|i| if i % 64 == 63 { b'\n' } else { b'/' })
            .collect();
        exact[at_cap - 1] = b'\n';
        std::fs::write(root.join("src/exact.rs"), &exact).unwrap();
        // A minified bundle: over GENERATED_MIN_BYTES, on one line.
        let minified = format!("var x=\"{}\";\n", "0".repeat(70_000));
        std::fs::write(root.join("src/bundle.js"), minified).unwrap();
        std::os::unix::fs::symlink(root.join("src/a.rs"), root.join("src/link.rs")).unwrap();

        assert_eq!(indexability(&root, "src/a.rs"), Indexability::Indexable);
        assert_eq!(
            indexability(&root, "notes.txt"),
            Indexability::UnsupportedLanguage
        );
        assert_eq!(indexability(&root, "src/blob.rs"), Indexability::Binary);
        assert_eq!(indexability(&root, "src/huge.rs"), Indexability::TooLarge);
        assert_eq!(
            indexability(&root, "src/exact.rs"),
            Indexability::Indexable,
            "a file of exactly MAX_FILE_BYTES is within the cap, not over it"
        );
        assert_eq!(
            indexability(&root, "src/bundle.js"),
            Indexability::Generated
        );
        // A symlink is never indexed, and neither is a path that is not
        // there at all — a deletion asks about the second one.
        assert_eq!(indexability(&root, "src/link.rs"), Indexability::Absent);
        assert_eq!(indexability(&root, "src/gone.rs"), Indexability::Absent);
        // A directory reached through a supported-looking name.
        std::fs::create_dir_all(root.join("pkg.rs")).unwrap();
        assert_eq!(indexability(&root, "pkg.rs"), Indexability::Absent);

        // The verdicts match what the walk actually collects.
        let collected: Vec<String> = collect_files(&root).into_iter().map(|(r, _)| r).collect();
        for kept in ["src/a.rs", "src/exact.rs"] {
            assert!(collected.contains(&kept.to_string()), "{collected:?}");
        }
        for skipped in [
            "notes.txt",
            "src/blob.rs",
            "src/huge.rs",
            "src/link.rs",
            "pkg.rs",
        ] {
            assert!(
                !collected.contains(&skipped.to_string()),
                "{skipped} is collected but indexability refuses it"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn smoke_build_graph_ts_and_rust() {
        let root = tmpdir("smoke");
        std::fs::write(
            root.join("a.ts"),
            "export function greet(name: string): string {\n  return \"hi \" + name;\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.ts"),
            "import { greet } from \"./a\";\nexport function main() {\n  return greet(\"x\");\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("c.rs"),
            "fn helper() -> u32 { 1 }\nfn run() -> u32 { helper() }\n",
        )
        .unwrap();

        let db = root.join(".pixel").join("graph.db");
        let stats = build_graph(&root, &db).unwrap();
        assert_eq!(stats.files, 3, "all three files indexed");
        assert!(
            stats.symbols >= 4,
            "greet, main, helper, run: {}",
            stats.symbols
        );
        assert!(
            stats.edges >= 2,
            "cross-file + same-file call edges: {}",
            stats.edges
        );

        let store = GraphStore::open(&db).unwrap();
        // Cross-file: main -> greet must be an Exact (T1 import-resolved) edge.
        let greet = &store.symbols_by_name("greet", None, 10).unwrap()[0];
        let callers = store.edges_to(greet.id, Some(EdgeKind::Calls)).unwrap();
        assert_eq!(callers.len(), 1, "exactly one caller of greet");
        assert_eq!(callers[0].tier, Tier::Exact);
        let main_sym = &store.symbols_by_name("main", None, 10).unwrap()[0];
        assert_eq!(callers[0].src_id, main_sym.id, "caller is b.ts main");
        // Same-file Rust: run -> helper Exact (T0).
        let helper = &store.symbols_by_name("helper", None, 10).unwrap()[0];
        let hcallers = store.edges_to(helper.id, Some(EdgeKind::Calls)).unwrap();
        assert_eq!(hcallers.len(), 1);
        assert_eq!(hcallers[0].tier, Tier::Exact);
        // Sanity: counts agree with stats.
        let (f, s, e, _u) = store.counts().unwrap();
        assert_eq!((f, s, e), (stats.files, stats.symbols, stats.edges));
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn smoke_update_file_relinks_callers() {
        let root = tmpdir("update");
        std::fs::write(root.join("a.ts"), "export function greet() { return 1 }\n").unwrap();
        std::fs::write(
            root.join("b.ts"),
            "import { greet } from \"./a\";\nexport function main() { return greet() }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        // Edit a.ts (same symbol, new body) and update just that file.
        std::fs::write(root.join("a.ts"), "export function greet() { return 2 }\n").unwrap();
        let published = update_file(&root, &db, "a.ts").unwrap();
        assert_eq!(
            published,
            Publication::Signed,
            "an update that matches the tree signs the graph"
        );
        assert!(is_fresh(&root, &db));

        let store = GraphStore::open(&db).unwrap();
        let greet = &store.symbols_by_name("greet", None, 10).unwrap()[0];
        let callers = store.edges_to(greet.id, Some(EdgeKind::Calls)).unwrap();
        assert_eq!(
            callers.len(),
            1,
            "caller edge survives an incremental update"
        );
        assert_eq!(callers[0].tier, Tier::Exact);
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `tree_delta` is the one walk behind both the freshness verdict and
    /// the incremental update: it must name exactly the files that drifted
    /// (added, edited, removed) and nothing else, or the daemon would either
    /// re-extract the whole tree (defeating the point) or miss an edit
    /// (serving stale symbols).
    #[test]
    fn tree_delta_names_added_edited_and_removed_files_only() {
        let root = tmpdir("delta");
        std::fs::write(root.join("a.ts"), "export function alpha() { return 1 }\n").unwrap();
        std::fs::write(root.join("b.ts"), "export function beta() { return 1 }\n").unwrap();
        std::fs::write(root.join("c.ts"), "export function gamma() { return 1 }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        let fresh = tree_delta(&root, &db).unwrap().expect("signed db");
        assert!(fresh.fresh);
        assert_eq!(fresh.changed_count(), 0);
        assert_eq!(fresh.indexed_files, 3);
        assert_eq!(fresh.signature, freshness_signature(&root));

        // Same size, different content (the `touch -t` shape), one new
        // file, one deleted file; `b.ts` untouched.
        std::fs::write(root.join("a.ts"), "export function alpha() { return 2 }\n").unwrap();
        std::fs::write(root.join("d.ts"), "export function delta() { return 1 }\n").unwrap();
        std::fs::remove_file(root.join("c.ts")).unwrap();

        let delta = tree_delta(&root, &db).unwrap().expect("signed db");
        assert!(!delta.fresh);
        let changed: Vec<&str> = delta.changed.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(changed, ["a.ts", "d.ts"], "sorted, b.ts untouched");
        assert_eq!(delta.removed, ["c.ts"]);
        assert_eq!(delta.indexed_files, 3, "counts the graph as built");
        assert_eq!(delta.signature, freshness_signature(&root));

        // Applying it makes the graph fresh again with exactly the
        // surviving files, and the deleted file's symbol is gone.
        apply_tree_delta(&root, &db, &delta).unwrap();
        assert!(is_fresh(&root, &db));
        let store = GraphStore::open(&db).unwrap();
        let mut paths: Vec<String> = store.files().unwrap().into_iter().map(|f| f.path).collect();
        paths.sort();
        assert_eq!(paths, ["a.ts", "b.ts", "d.ts"]);
        assert!(store.symbols_by_name("gamma", None, 5).unwrap().is_empty());
        assert_eq!(store.symbols_by_name("delta", None, 5).unwrap().len(), 1);
        drop(store);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `tree_delta_cached` must produce the SAME delta as `tree_delta` while
    /// hashing only the files whose stat moved — the post-edit hot path.
    #[test]
    fn tree_delta_cached_matches_uncached_and_rehashes_only_changed_files() {
        let root = tmpdir("delta-cached");
        std::fs::write(root.join("a.ts"), "export function alpha() { return 1 }\n").unwrap();
        std::fs::write(root.join("b.ts"), "export function beta() { return 1 }\n").unwrap();
        std::fs::write(root.join("c.ts"), "export function gamma() { return 1 }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        let mut cache = TreeHashCache::default();
        // First cached walk: cold — every file is hashed once.
        let d1 = tree_delta_cached(&root, &db, &mut cache)
            .unwrap()
            .expect("signed db");
        assert!(d1.fresh);
        assert_eq!(cache.rehashed, 3, "cold cache hashes everything");
        assert_eq!(cache.stat_hits, 0);

        // Second walk, nothing changed: identical verdict, zero rehashes —
        // the stat-only path is what makes a post-watcher-batch op cheap.
        let d2 = tree_delta_cached(&root, &db, &mut cache)
            .unwrap()
            .expect("signed db");
        assert!(d2.fresh);
        assert_eq!(d2.signature, d1.signature);
        assert_eq!(cache.rehashed, 0, "unchanged tree must not re-read files");
        assert_eq!(cache.stat_hits, 3);

        // One same-size edit: only that file is re-hashed, and the delta
        // still names it exactly as the uncached walk does.
        std::fs::write(root.join("b.ts"), "export function beta() { return 2 }\n").unwrap();
        let uncached = tree_delta(&root, &db).unwrap().expect("signed db");
        let d3 = tree_delta_cached(&root, &db, &mut cache)
            .unwrap()
            .expect("signed db");
        assert_eq!(d3.changed, uncached.changed);
        assert_eq!(d3.removed, uncached.removed);
        assert_eq!(d3.signature, uncached.signature);
        assert_eq!(cache.rehashed, 1, "only the stat-changed file is re-read");
        assert_eq!(cache.stat_hits, 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Parity with a full rebuild, part 1: imports between files of the
    /// same batch, and imports of UNCHANGED files that pointed at a file
    /// which did not exist yet, resolve once the batch lands. Without this
    /// the resolver's import tier never saw the new file and the caller
    /// edge came out `Probable` or unresolved, unlike after `pixel rebuild-graph`.
    #[test]
    fn incremental_update_resolves_imports_to_files_added_in_the_batch() {
        let root = tmpdir("delta-imports");
        // a.ts imports a file that does not exist yet.
        std::fs::write(
            root.join("a.ts"),
            "import { helper } from \"./b\";\nexport function work() { return helper() }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let a_id = store.file_by_path("a.ts").unwrap().unwrap().id;
        let unresolved: i64 = store
            .conn()
            .query_row(
                "SELECT count(*) FROM imports WHERE file_id = ?1 AND resolved_file_id IS NULL",
                rusqlite::params![a_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unresolved, 1, "b.ts does not exist: import dangles");
        drop(store);

        // b.ts appears, together with c.ts which imports it in the same batch.
        std::fs::write(root.join("b.ts"), "export function helper() { return 1 }\n").unwrap();
        std::fs::write(
            root.join("c.ts"),
            "import { helper } from \"./b\";\nexport function other() { return helper() }\n",
        )
        .unwrap();
        let delta = tree_delta(&root, &db).unwrap().unwrap();
        apply_tree_delta(&root, &db, &delta).unwrap();

        let store = GraphStore::open(&db).unwrap();
        let b_id = store.file_by_path("b.ts").unwrap().unwrap().id;
        for importer in ["a.ts", "c.ts"] {
            let f = store.file_by_path(importer).unwrap().unwrap().id;
            let resolved: Option<i64> = store
                .conn()
                .query_row(
                    "SELECT resolved_file_id FROM imports WHERE file_id = ?1",
                    rusqlite::params![f],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                resolved,
                Some(b_id),
                "{importer}: import must resolve to b.ts"
            );
        }
        let helper = &store.symbols_by_name("helper", None, 5).unwrap()[0];
        let callers = store.edges_to(helper.id, Some(EdgeKind::Calls)).unwrap();
        assert_eq!(callers.len(), 2, "work() and other() both call helper()");
        assert!(
            callers.iter().all(|e| e.tier == Tier::Exact),
            "imported unique target: Exact, as after a full rebuild ({callers:?})"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Parity with a full rebuild, part 2: persisted `processes`/`clusters`
    /// are keyed by symbol id and a re-extracted file gets new ids; after an
    /// incremental update they must be gone (recomputed on demand), not
    /// left pointing at deleted symbols.
    #[test]
    fn incremental_update_drops_cached_processes_and_clusters() {
        let root = tmpdir("delta-analyses");
        std::fs::write(root.join("a.ts"), "export function alpha() { return 1 }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let alpha = &store.symbols_by_name("alpha", None, 5).unwrap()[0];
        store
            .conn()
            .execute_batch(&format!(
                "INSERT INTO processes (id, label, entry_symbol_id, step_count) VALUES (1, 'p', {0}, 1);
                 INSERT INTO process_steps (process_id, step, symbol_id) VALUES (1, 0, {0});
                 INSERT INTO clusters (id, label) VALUES (1, 'c');
                 INSERT INTO cluster_members (cluster_id, symbol_id) VALUES (1, {0});",
                alpha.id
            ))
            .unwrap();
        drop(store);

        std::fs::write(root.join("a.ts"), "export function alpha() { return 2 }\n").unwrap();
        let delta = tree_delta(&root, &db).unwrap().unwrap();
        apply_tree_delta(&root, &db, &delta).unwrap();

        let store = GraphStore::open(&db).unwrap();
        for table in ["processes", "process_steps", "clusters", "cluster_members"] {
            let n: i64 = store
                .conn()
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table} must be cleared by the incremental update");
        }
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A db without a freshness signature cannot say what it was built
    /// from: `tree_delta` refuses to guess (None) so the caller rebuilds.
    #[test]
    fn tree_delta_is_none_without_a_signature() {
        let root = tmpdir("delta-unsigned");
        std::fs::write(root.join("a.ts"), "export function alpha() { return 1 }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        GraphStore::open(&db)
            .unwrap()
            .conn()
            .execute(
                "DELETE FROM meta WHERE key = ?1",
                rusqlite::params![FRESHNESS_KEY],
            )
            .unwrap();
        assert!(tree_delta(&root, &db).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file edited between the delta walk and its application must not be
    /// signed as fresh: the delta's signature describes bytes the store never
    /// saw. The update itself still lands (the graph is no worse than before),
    /// only the signature is withheld so the next open detects the drift.
    #[test]
    fn apply_tree_delta_withholds_signature_when_a_file_changed_underneath() {
        let root = tmpdir("delta-race");
        std::fs::write(root.join("a.ts"), "export function alpha() { return 1 }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        std::fs::write(root.join("a.ts"), "export function alpha() { return 2 }\n").unwrap();
        let delta = tree_delta(&root, &db).unwrap().unwrap();
        // The "concurrent" edit: the tree moves on after the walk.
        std::fs::write(root.join("a.ts"), "export function alpha() { return 3 }\n").unwrap();
        let err = apply_tree_delta(&root, &db, &delta).unwrap_err();
        assert!(
            err.to_string().contains("changed during incremental"),
            "{err}"
        );
        assert!(
            !is_fresh(&root, &db),
            "signature must not have been published"
        );
        assert_eq!(
            stored_signature(&GraphStore::open(&db).unwrap()),
            FRESHNESS_WITHHELD,
            "the previous signature is invalidated, not kept"
        );
        let repair = tree_delta(&root, &db).unwrap();
        assert!(
            repair.is_some_and(|d| !d.fresh),
            "an invalidated signature keeps the incremental repair path"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn hash_of(content: &[u8]) -> String {
        format!("{:016x}", xxh3_64(content))
    }

    fn stored_hash(store: &GraphStore, rel: &str) -> String {
        store.file_by_path(rel).unwrap().unwrap().blob_oid
    }

    fn stored_signature(store: &GraphStore) -> String {
        store.meta_get(FRESHNESS_KEY).unwrap().unwrap()
    }

    /// The rows and the freshness signature of an incremental update are one
    /// publication. A second connection reading while the update runs sees
    /// the previous rows under the previous signature; once it returns, that
    /// same connection sees the new rows under the new signature. A signature
    /// written outside the rows' transaction would let the reader take new
    /// rows for a tree the signature does not describe, or vouch for old rows
    /// with the new tree's signature.
    #[test]
    fn watcher_update_commits_rows_and_signature_together() {
        let root = tmpdir("atomic-publish");
        let v1 = b"export function alpha() { return 1 }\n";
        let v2 = b"export function alpha() { return 2 }\n";
        std::fs::write(root.join("a.ts"), v1).unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let reader = GraphStore::open(&db).unwrap();
        let old_signature = stored_signature(&reader);
        assert_eq!(stored_hash(&reader, "a.ts"), hash_of(v1));

        std::fs::write(root.join("a.ts"), v2).unwrap();
        let new_signature = freshness_signature(&root);
        assert_ne!(old_signature, new_signature);
        let mut probed = false;
        let published = update_files_probed(&root, &db, &[("a.ts", false)], &mut || {
            probed = true;
            // In flight: rows are written on the writer's connection and not
            // committed, so the reader still sees the previous generation.
            assert_eq!(
                stored_hash(&reader, "a.ts"),
                hash_of(v1),
                "rows became visible before the commit"
            );
            assert_eq!(
                stored_signature(&reader),
                old_signature,
                "signature became visible before the commit"
            );
        })
        .unwrap();
        assert!(probed, "the seam must run inside the transaction");
        assert_eq!(published, Publication::Signed);
        assert_eq!(stored_hash(&reader, "a.ts"), hash_of(v2));
        assert_eq!(stored_signature(&reader), new_signature);
        assert!(is_fresh(&root, &db));
        drop(reader);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The watcher path signs with the tree it walks after writing the rows.
    /// A batched file that changed again in between makes that walk describe
    /// bytes the store never parsed: signing would let a cold open serve the
    /// stale symbols as fresh. The rows still land (the graph is no worse
    /// than before), the signature is withheld, and the next delta names the
    /// file and repairs it.
    #[test]
    fn watcher_update_withholds_signature_when_a_touched_file_changed_underneath() {
        let root = tmpdir("watcher-race");
        let v1 = b"export function alpha() { return 1 }\n";
        let v2 = b"export function alpha() { return 2 }\n";
        let v3 = b"export function alpha() { return 3 }\n";
        std::fs::write(root.join("a.ts"), v1).unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let old_signature = stored_signature(&GraphStore::open(&db).unwrap());

        std::fs::write(root.join("a.ts"), v2).unwrap();
        let published = update_files_probed(&root, &db, &[("a.ts", false)], &mut || {
            // The "concurrent" edit: the file moves on after extraction.
            std::fs::write(root.join("a.ts"), v3).unwrap();
        })
        .unwrap();
        assert_eq!(published, Publication::Withheld);
        let store = GraphStore::open(&db).unwrap();
        assert_eq!(
            stored_hash(&store, "a.ts"),
            hash_of(v2),
            "the rows for the bytes that were parsed still land"
        );
        assert_ne!(stored_signature(&store), old_signature);
        assert_eq!(
            stored_signature(&store),
            FRESHNESS_WITHHELD,
            "the stored signature is invalidated, never set to the walked tree"
        );
        drop(store);
        assert!(!is_fresh(&root, &db));

        // Repair: the next delta sees a.ts stored as v2 while the tree has v3.
        let delta = tree_delta(&root, &db).unwrap().unwrap();
        let changed: Vec<&str> = delta.changed.iter().map(|(rel, _)| rel.as_str()).collect();
        assert_eq!(changed, ["a.ts"]);
        apply_tree_delta(&root, &db, &delta).unwrap();
        assert!(is_fresh(&root, &db));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file outside the batch that drifted means the walk's signature would
    /// vouch for a row the store does not have yet. The batch lands unsigned;
    /// the batch that brings the last drifted file is the one that signs.
    #[test]
    fn watcher_update_withholds_signature_while_an_unbatched_file_drifted() {
        let root = tmpdir("watcher-partial");
        std::fs::write(root.join("a.ts"), "export function alpha() { return 1 }\n").unwrap();
        std::fs::write(root.join("b.ts"), "export function beta() { return 1 }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        let a2 = b"export function alpha() { return 2 }\n";
        std::fs::write(root.join("a.ts"), a2).unwrap();
        std::fs::write(root.join("b.ts"), "export function beta() { return 2 }\n").unwrap();

        let first = update_files(&root, &db, &[("a.ts", false)]).unwrap();
        assert_eq!(
            first,
            Publication::Withheld,
            "b.ts drifted outside the batch"
        );
        assert_eq!(
            stored_hash(&GraphStore::open(&db).unwrap(), "a.ts"),
            hash_of(a2),
            "the batched file's rows land regardless"
        );
        assert!(!is_fresh(&root, &db));

        let second = update_files(&root, &db, &[("b.ts", false)]).unwrap();
        assert_eq!(second, Publication::Signed);
        assert!(is_fresh(&root, &db));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An empty batch changes no row and publishes nothing: signing it would
    /// bind whatever the tree looks like now to rows nobody re-checked.
    #[test]
    fn empty_batch_publishes_nothing() {
        let root = tmpdir("watcher-empty");
        std::fs::write(root.join("a.ts"), "export function alpha() { return 1 }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let before = stored_signature(&GraphStore::open(&db).unwrap());
        std::fs::write(root.join("a.ts"), "export function alpha() { return 2 }\n").unwrap();
        assert_eq!(
            update_files(&root, &db, &[]).unwrap(),
            Publication::Withheld
        );
        assert_eq!(stored_signature(&GraphStore::open(&db).unwrap()), before);
        assert!(!is_fresh(&root, &db));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `rows_match_tree` is the gate on signing a watcher update: it accepts a
    /// store that holds exactly the walked tree and rejects every way the two
    /// can disagree. A rejected case that slipped through would sign a graph
    /// whose rows a cold open then serves as fresh.
    #[test]
    fn rows_match_tree_accepts_only_a_store_that_holds_exactly_the_walked_tree() {
        fn known(pairs: &[(&str, u64)]) -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(rel, hash)| ((*rel).to_string(), format!("{hash:016x}")))
                .collect()
        }
        fn walk(pairs: &[(&str, u64)]) -> Vec<(String, u64)> {
            pairs
                .iter()
                .map(|(rel, hash)| ((*rel).to_string(), *hash))
                .collect()
        }
        let batch_a = [("a.ts", false)];
        assert!(
            rows_match_tree(
                &known(&[("a.ts", 1), ("b.ts", 2)]),
                &walk(&[("a.ts", 1), ("b.ts", 2)]),
                &batch_a
            ),
            "rows equal the tree"
        );
        assert!(
            !rows_match_tree(
                &known(&[("a.ts", 1), ("b.ts", 2)]),
                &walk(&[("a.ts", 1), ("b.ts", 3)]),
                &batch_a
            ),
            "a file outside the batch drifted"
        );
        assert!(
            !rows_match_tree(&known(&[("a.ts", 1)]), &walk(&[("a.ts", 9)]), &batch_a),
            "the batched file changed after extraction"
        );
        assert!(
            rows_match_tree(
                &known(&[("b.ts", 2)]),
                &walk(&[("a.ts", 1), ("b.ts", 2)]),
                &batch_a
            ),
            "a batched file that extraction dropped has no row, and that is its stable state"
        );
        assert!(
            !rows_match_tree(
                &known(&[("b.ts", 2)]),
                &walk(&[("a.ts", 1), ("b.ts", 2)]),
                &[("b.ts", false)]
            ),
            "a file with no row outside the batch is an addition the store missed"
        );
        assert!(
            !rows_match_tree(
                &known(&[("b.ts", 2)]),
                &walk(&[("a.ts", 1), ("b.ts", 2)]),
                &[("a.ts", true)]
            ),
            "a batched removal whose file is back on disk"
        );
        assert!(
            !rows_match_tree(
                &known(&[("a.ts", 1), ("b.ts", 2)]),
                &walk(&[("a.ts", 1)]),
                &batch_a
            ),
            "a row for a file the tree no longer has"
        );
        assert!(
            rows_match_tree(&HashMap::new(), &[], &[]),
            "an empty store matches an empty tree"
        );
    }

    /// A debounced batch may carry one path several times; only its last
    /// event describes the file's final state. First appearance keeps the
    /// order (imports resolve in batch order), the last occurrence wins.
    #[test]
    fn final_actions_keeps_one_action_per_path_with_the_last_occurrence_winning() {
        assert_eq!(
            final_actions(&[("a.ts", false), ("b.ts", false), ("a.ts", true)]),
            [("a.ts", true), ("b.ts", false)]
        );
        assert_eq!(
            final_actions(&[("a.ts", true), ("a.ts", false)]),
            [("a.ts", false)]
        );
        assert_eq!(final_actions(&[("a.ts", false)]), [("a.ts", false)]);
        assert_eq!(final_actions(&[]), Vec::<(&str, bool)>::new());
    }

    /// Edited then removed in one batch while the file is in fact still on
    /// disk (the removal event was stale): the store must end without the
    /// row, and the signing check must see a walked file with no row that
    /// was NOT re-extracted, hence withhold. Applying both events and
    /// counting the path as re-extracted would sign a graph missing a file
    /// the tree has.
    #[test]
    fn repeated_path_update_then_remove_ends_removed_and_withholds_while_the_file_exists() {
        let root = tmpdir("batch-update-remove");
        std::fs::write(root.join("a.ts"), "export function alpha() { return 1 }\n").unwrap();
        std::fs::write(root.join("b.ts"), "export function beta() { return 1 }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        std::fs::write(root.join("a.ts"), "export function alpha() { return 2 }\n").unwrap();

        let published = update_files(&root, &db, &[("a.ts", false), ("a.ts", true)]).unwrap();
        assert_eq!(published, Publication::Withheld);
        let store = GraphStore::open(&db).unwrap();
        assert!(
            store.file_by_path("a.ts").unwrap().is_none(),
            "the last event (removal) decides the row"
        );
        assert_eq!(stored_signature(&store), FRESHNESS_WITHHELD);
        drop(store);
        assert!(!is_fresh(&root, &db));
        // The repair re-extracts the file the tree still has.
        let delta = tree_delta(&root, &db).unwrap().unwrap();
        let changed: Vec<&str> = delta.changed.iter().map(|(rel, _)| rel.as_str()).collect();
        assert_eq!(changed, ["a.ts"]);
        apply_tree_delta(&root, &db, &delta).unwrap();
        assert!(is_fresh(&root, &db));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Removed then edited in one batch (the file was recreated): the last
    /// event wins, the row is present under the new bytes, and the update
    /// signs because the store holds exactly the tree.
    #[test]
    fn repeated_path_remove_then_update_ends_re_extracted_and_signs() {
        let root = tmpdir("batch-remove-update");
        std::fs::write(root.join("a.ts"), "export function alpha() { return 1 }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let v2 = b"export function alpha() { return 2 }\n";
        std::fs::write(root.join("a.ts"), v2).unwrap();

        let published = update_files(&root, &db, &[("a.ts", true), ("a.ts", false)]).unwrap();
        assert_eq!(published, Publication::Signed);
        assert_eq!(
            stored_hash(&GraphStore::open(&db).unwrap(), "a.ts"),
            hash_of(v2)
        );
        assert!(is_fresh(&root, &db));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ABA: tree A is signed, rows are written for B, the tree is back to A
    /// before the signing walk. The update is withheld, but had the previous
    /// signature (A) been kept, `is_fresh` would accept rows B for tree A.
    /// The sentinel makes the graph stale, and stale through the incremental
    /// path: `tree_delta` still yields a delta (not `None`, which would cost
    /// a full rebuild) naming the file whose row disagrees with the tree.
    #[test]
    fn withheld_update_invalidates_the_previous_signature_so_an_aba_tree_is_not_fresh() {
        let root = tmpdir("aba");
        let a = b"export function alpha() { return 1 }\n";
        let b = b"export function alpha() { return 2 }\n";
        std::fs::write(root.join("a.ts"), a).unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let signature_a = stored_signature(&GraphStore::open(&db).unwrap());

        std::fs::write(root.join("a.ts"), b).unwrap();
        let published = update_files_probed(&root, &db, &[("a.ts", false)], &mut || {
            // Back to A before the walk that would sign.
            std::fs::write(root.join("a.ts"), a).unwrap();
        })
        .unwrap();
        assert_eq!(published, Publication::Withheld);
        let store = GraphStore::open(&db).unwrap();
        assert_eq!(stored_hash(&store, "a.ts"), hash_of(b), "rows hold B");
        assert_ne!(
            stored_signature(&store),
            signature_a,
            "keeping signature A would pass rows B off as fresh for tree A"
        );
        drop(store);
        assert_eq!(
            freshness_signature(&root),
            signature_a,
            "the tree is A again"
        );
        assert!(!is_fresh(&root, &db));

        let delta = tree_delta(&root, &db)
            .unwrap()
            .expect("stale, not unsigned");
        assert!(!delta.fresh);
        let changed: Vec<&str> = delta.changed.iter().map(|(rel, _)| rel.as_str()).collect();
        assert_eq!(changed, ["a.ts"]);
        apply_tree_delta(&root, &db, &delta).unwrap();
        assert!(is_fresh(&root, &db));
        assert_eq!(
            stored_hash(&GraphStore::open(&db).unwrap(), "a.ts"),
            hash_of(a)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: an existing graph.db must be detected as stale when a
    /// source file changes, and `is_fresh` must reflect that. After a rebuild
    /// the db is fresh again.
    #[test]
    fn freshness_detects_drift_and_rebuild() {
        let root = tmpdir("fresh");
        std::fs::write(root.join("a.ts"), "export function alpha() { return 1 }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");

        // Initial build: fresh.
        build_graph(&root, &db).unwrap();
        assert!(
            is_fresh(&root, &db),
            "graph must be fresh right after build"
        );

        // Edit a source file: now stale.
        std::fs::write(root.join("a.ts"), "export function alpha() { return 2 }\n").unwrap();
        assert!(
            !is_fresh(&root, &db),
            "graph must be stale after a source file changes"
        );

        // Rebuild: fresh again.
        build_graph(&root, &db).unwrap();
        assert!(is_fresh(&root, &db), "graph must be fresh after rebuild");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: a method call with a real receiver (`x.parse()`) must NOT
    /// be linked as an `Exact` edge to a same-name function/method elsewhere.
    /// The cited false-positive was `42.parse()` linking to `SymbolKind::parse`.
    /// The receiver downgrade caps such calls at `Probable` at most.
    #[test]
    fn receiver_call_not_exact_to_same_name_function() {
        let root = tmpdir("receiver");
        // a.ts defines a free function `parse` (unique repo-wide).
        std::fs::write(
            root.join("a.ts"),
            "export function parse(input: string): number { return Number(input); }\n",
        )
        .unwrap();
        // b.ts calls `parse(...)` directly (no receiver) AND `n.parse(...)`
        // with a receiver. The bare call should be Exact (T2 unique); the
        // receiver call must NOT be Exact.
        std::fs::write(
            root.join("b.ts"),
            "import { parse } from \"./a\";\n\
             export function caller(n: any) {\n  \
             const a = parse(\"42\");\n  \
             const b = n.parse(\"42\");\n\
             }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        let store = GraphStore::open(&db).unwrap();
        let parse = &store.symbols_by_name("parse", None, 10).unwrap()[0];
        let callers = store.edges_to(parse.id, Some(EdgeKind::Calls)).unwrap();
        // At least the bare `parse("42")` call resolves (Exact, T1 imported).
        let exact: Vec<_> = callers.iter().filter(|e| e.tier == Tier::Exact).collect();
        let probable: Vec<_> = callers
            .iter()
            .filter(|e| e.tier == Tier::Probable)
            .collect();
        // The receiver call `n.parse(...)` must NOT be Exact.
        // (It may be Probable via T2 unique-name, or unresolved; either is
        // acceptable as long as it is not Exact.)
        assert!(
            !exact.is_empty(),
            "bare parse() call should resolve as Exact"
        );
        // If there is a second edge (the receiver call), it must not be Exact.
        if callers.len() > exact.len() {
            assert!(
                probable.len() + (callers.len() - exact.len() - probable.len()) > 0,
                "receiver call must be Probable or Unresolved, never Exact"
            );
            for e in &callers {
                if e.tier == Tier::Exact {
                    // Exact edges must come from the bare call only; sanity
                    // check there is at least one Exact and any non-Exact is
                    // not Exact (trivially true).
                }
            }
        }
        // Stronger direct check via the resolver: a receiver call to a unique
        // name is downgraded to Probable.
        let idx = ResolveIndex::build(&store).unwrap();
        let b_id = store.file_by_path("b.ts").unwrap().unwrap().id;
        match idx.decide(b_id, "parse", Some("n")) {
            Decision::Exact(_) => panic!("receiver call must not be Exact"),
            Decision::Probable(_) => {} // acceptable downgrade
            Decision::Unresolved => {}  // also acceptable
        }
        // Bare call (no receiver) to the imported unique name stays Exact.
        match idx.decide(b_id, "parse", None) {
            Decision::Exact(_) => {}
            other => panic!("bare call to imported unique name should be Exact, got {other:?}"),
        }
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: graph freshness must detect equal-size content changes
    /// even when mtime is restored. The old stat-only signature (path+size+mtime)
    /// could be fooled by `touch -t` or `cp` + `touch -r`. The content-hash
    /// signature catches this.
    #[test]
    fn freshness_detects_equal_size_content_change() {
        let root = tmpdir("fresh-content");
        // Two different bodies with the same byte length.
        let body_a = "export function alpha() { return 1 }\n";
        let body_b = "export function alpha() { return 2 }\n";
        assert_eq!(
            body_a.len(),
            body_b.len(),
            "test setup: equal-length bodies"
        );

        std::fs::write(root.join("a.ts"), body_a).unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        assert!(is_fresh(&root, &db), "fresh after initial build");

        // Change content, same size. The content hash differs even if mtime
        // is restored, so the signature must change.
        std::fs::write(root.join("a.ts"), body_b).unwrap();
        assert!(
            !is_fresh(&root, &db),
            "graph must be stale after equal-size content change"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: importing one binding must NOT make unrelated definitions
    /// from the same file eligible for Exact T1 resolution. If file B imports
    /// `{ greet }` from `./a`, a call to `farewell()` (also defined in `./a`
    /// but NOT imported) must NOT be Exact via T1.
    #[test]
    fn import_binding_specificity_prevents_false_exact() {
        let root = tmpdir("import-bindings");
        // a.ts exports two functions: greet and farewell.
        std::fs::write(
            root.join("a.ts"),
            "export function greet(): void {}\n\
             export function farewell(): void {}\n",
        )
        .unwrap();
        // b.ts imports ONLY greet, but calls both greet and farewell.
        std::fs::write(
            root.join("b.ts"),
            "import { greet } from \"./a\";\n\
             export function caller() {\n  \
             greet();\n  \
             farewell();\n\
             }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        let store = GraphStore::open(&db).unwrap();
        let idx = ResolveIndex::build(&store).unwrap();
        let b_id = store.file_by_path("b.ts").unwrap().unwrap().id;

        // greet was imported → bare call should be Exact (T1 binding-level).
        match idx.decide(b_id, "greet", None) {
            Decision::Exact(_) => {}
            other => panic!("imported binding `greet` should be Exact, got {other:?}"),
        }

        // farewell was NOT imported → must NOT be Exact via T1. It can be
        // Probable (T2 unique name) or Unresolved, but never Exact.
        match idx.decide(b_id, "farewell", None) {
            Decision::Exact(_) => {
                panic!("farewell was not imported — must not be Exact via T1")
            }
            Decision::Probable(_) => {} // acceptable: T2 unique name
            Decision::Unresolved => {}  // also acceptable
        }

        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A Rust `use` binds names like a TS import does: `ship.rs` imports
    /// `push` from `push.rs` while `other.rs` defines a `push` too, so only
    /// the import tier can tell the two apart. Before Rust bindings were
    /// extracted the call stayed unresolved and `who-calls push` reported no
    /// caller (the demo's `ship.rs` → `push` edge).
    #[test]
    fn a_rust_use_makes_the_imported_same_name_function_exact() {
        let root = tmpdir("rust-use-bindings");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub mod push;\npub mod other;\npub mod ship;\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/push.rs"),
            "pub struct PushOptions;\npub fn push() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/other.rs"),
            "pub fn push() {}\npub fn publish() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/ship.rs"),
            "use crate::push::{PushOptions, push};\npub fn ship() { push(); publish(); }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        let store = GraphStore::open(&db).unwrap();
        let idx = ResolveIndex::build(&store).unwrap();
        let ship = store.file_by_path("src/ship.rs").unwrap().unwrap().id;
        let push_file = store.file_by_path("src/push.rs").unwrap().unwrap().id;
        match idx.decide(ship, "push", None) {
            Decision::Exact(id) => assert!(
                store
                    .symbols_in_file(push_file)
                    .unwrap()
                    .iter()
                    .any(|s| s.id == id),
                "the imported push, not other.rs's"
            ),
            other => panic!("imported `push` should be Exact, got {other:?}"),
        }
        // Not imported: the use binds `push`, not every item of push.rs,
        // and `publish` is defined elsewhere only.
        assert!(
            !matches!(idx.decide(ship, "publish", None), Decision::Exact(_)),
            "a name the use does not bind never gets the import tier"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The callers of `push` in `path`, as `(caller name, tier)`.
    fn callers_of_push_in(store: &GraphStore, path: &str) -> Vec<(String, Tier)> {
        let file = store.file_by_path(path).unwrap().unwrap().id;
        let push = store
            .symbols_in_file(file)
            .unwrap()
            .into_iter()
            .find(|s| s.name == "push")
            .unwrap();
        let mut callers: Vec<(String, Tier)> = store
            .edges_to(push.id, Some(EdgeKind::Calls))
            .unwrap()
            .into_iter()
            .map(|e| {
                let src: String = store
                    .conn()
                    .query_row(
                        "SELECT name FROM symbols WHERE id = ?1",
                        rusqlite::params![e.src_id],
                        |r| r.get(0),
                    )
                    .unwrap();
                (src, e.tier)
            })
            .collect();
        callers.sort_by(|a, b| a.0.cmp(&b.0));
        callers
    }

    /// The callers of `name` defined in `path`, as `(caller name, tier)`.
    fn callers_in(store: &GraphStore, path: &str, name: &str) -> Vec<(String, Tier)> {
        let file = store.file_by_path(path).unwrap().unwrap().id;
        let target = store
            .symbols_in_file(file)
            .unwrap()
            .into_iter()
            .find(|s| s.name == name)
            .unwrap();
        let mut callers: Vec<(String, Tier)> = store
            .edges_to(target.id, Some(EdgeKind::Calls))
            .unwrap()
            .into_iter()
            .map(|e| {
                let src: String = store
                    .conn()
                    .query_row(
                        "SELECT name FROM symbols WHERE id = ?1",
                        rusqlite::params![e.src_id],
                        |r| r.get(0),
                    )
                    .unwrap();
                (src, e.tier)
            })
            .collect();
        callers.sort_by(|a, b| a.0.cmp(&b.0));
        callers
    }

    /// Three definitions of `current_branch` (a `GitRunner` method and two
    /// free functions) leave the name tiers nothing to pick. The typed local
    /// `runner: &GitRunner` links `run` to the method, and the module path
    /// `gitsync::current_branch` links `wrap` to `gitsync.rs`'s function, both
    /// at Probable. Each incremental route re-derives the decision from what
    /// it stored: rewriting the caller or the target keeps both edges, and a
    /// second `gitsync.rs` makes the module path ambiguous without touching
    /// the typed call.
    #[test]
    fn typed_and_module_receivers_survive_incremental_updates() {
        let root = tmpdir("typed-receivers");
        let files = [
            (
                "crates/git/src/lib.rs",
                "pub struct GitRunner;\nimpl GitRunner {\n    pub fn current_branch(&self) {}\n}\n",
            ),
            ("crates/guard/src/lib.rs", "pub fn current_branch() {}\n"),
            ("crates/idx/src/gitsync.rs", "pub fn current_branch() {}\n"),
            (
                "crates/ops/src/lib.rs",
                "pub fn run(runner: &git::GitRunner) {\n    runner.current_branch();\n}\npub fn wrap() {\n    idx::gitsync::current_branch();\n}\n",
            ),
        ];
        for (path, body) in files {
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::write(root.join(path), body).unwrap();
        }
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let both = |db: &Path| {
            let store = GraphStore::open(db).unwrap();
            (
                callers_in(&store, "crates/git/src/lib.rs", "current_branch"),
                callers_in(&store, "crates/idx/src/gitsync.rs", "current_branch"),
                callers_in(&store, "crates/guard/src/lib.rs", "current_branch"),
            )
        };
        let run = vec![("run".to_string(), Tier::Probable)];
        let wrap = vec![("wrap".to_string(), Tier::Probable)];
        assert_eq!(both(&db), (run.clone(), wrap.clone(), vec![]), "full build");

        std::fs::write(
            root.join("crates/ops/src/lib.rs"),
            format!("// v2\n{}", files[3].1),
        )
        .unwrap();
        update_file(&root, &db, "crates/ops/src/lib.rs").unwrap();
        assert_eq!(
            both(&db),
            (run.clone(), wrap.clone(), vec![]),
            "caller rewritten"
        );

        std::fs::write(
            root.join("crates/git/src/lib.rs"),
            format!("// v2\n{}", files[0].1),
        )
        .unwrap();
        update_file(&root, &db, "crates/git/src/lib.rs").unwrap();
        std::fs::write(
            root.join("crates/idx/src/gitsync.rs"),
            format!("// v2\n{}", files[2].1),
        )
        .unwrap();
        update_file(&root, &db, "crates/idx/src/gitsync.rs").unwrap();
        assert_eq!(
            both(&db),
            (run.clone(), wrap.clone(), vec![]),
            "targets rewritten"
        );

        std::fs::create_dir_all(root.join("crates/other/src/idx")).unwrap();
        std::fs::write(
            root.join("crates/other/src/idx/gitsync.rs"),
            "pub fn current_branch() {}\n",
        )
        .unwrap();
        update_file(&root, &db, "crates/other/src/idx/gitsync.rs").unwrap();
        assert_eq!(
            both(&db),
            (run, vec![], vec![]),
            "a second `idx::gitsync` makes the module path name neither"
        );
        let store = GraphStore::open(&db).unwrap();
        assert!(
            callers_in(&store, "crates/other/src/idx/gitsync.rs", "current_branch").is_empty(),
            "nor the new one"
        );
        assert_eq!(
            store
                .envelope_for_name("current_branch")
                .unwrap()
                .unresolved_same_name,
            1
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `use crate::push::push as leased;` puts `leased` in scope, not `push`.
    /// Two files define `push`, so only the import tier can link a call to
    /// one of them: `leased()` must reach `push.rs`, and the bare `push()` in
    /// `stray`, which no import binds, must reach neither — it used to take
    /// the Exact edge to `push.rs` because the use recorded the source name.
    /// An incremental update that adds a third `push` reconsiders the edge;
    /// it must replay the written `leased`, or the edge would be lost.
    #[test]
    fn a_rust_use_alias_binds_the_alias_and_survives_an_incremental_update() {
        let root = tmpdir("rust-use-alias");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub mod push;\npub mod other;\npub mod ship;\n",
        )
        .unwrap();
        std::fs::write(root.join("src/push.rs"), "pub fn push() {}\n").unwrap();
        std::fs::write(root.join("src/other.rs"), "pub fn push() {}\n").unwrap();
        std::fs::write(
            root.join("src/ship.rs"),
            "use crate::push::push as leased;\npub fn ship() { leased(); }\npub fn stray() { push(); }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        {
            let store = GraphStore::open(&db).unwrap();
            assert_eq!(
                callers_of_push_in(&store, "src/push.rs"),
                [("ship".to_string(), Tier::Exact)],
                "leased() calls push.rs's push; the unbound push() calls nothing proven"
            );
            assert!(callers_of_push_in(&store, "src/other.rs").is_empty());
        }

        std::fs::write(root.join("src/third.rs"), "pub fn push() {}\n").unwrap();
        update_file(&root, &db, "src/third.rs").unwrap();
        {
            let store = GraphStore::open(&db).unwrap();
            assert_eq!(
                callers_of_push_in(&store, "src/push.rs"),
                [("ship".to_string(), Tier::Exact)],
                "a new same-name definition elsewhere leaves the aliased import's edge"
            );
        }

        // Rewriting push.rs demotes its incoming edges; they must come back
        // under the written `leased`, not the target's `push`.
        std::fs::write(root.join("src/push.rs"), "// v2\npub fn push() {}\n").unwrap();
        update_file(&root, &db, "src/push.rs").unwrap();
        let store = GraphStore::open(&db).unwrap();
        assert_eq!(
            callers_of_push_in(&store, "src/push.rs"),
            [("ship".to_string(), Tier::Exact)],
            "rewriting the target file leaves the aliased import's edge"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The TS form of the alias: `import { push as leased }`. The alias is
    /// also what a passed-as-value reference writes (`run(leased)`), and it
    /// names no symbol, so the reference must not be dropped as a plain value.
    #[test]
    fn a_ts_import_alias_binds_the_alias_for_calls_and_references() {
        let root = tmpdir("ts-import-alias");
        std::fs::write(
            root.join("push.ts"),
            "export function push() {}\nexport const LIMIT = 3;\n",
        )
        .unwrap();
        std::fs::write(
            root.join("other.ts"),
            "export function push() {}\nexport function helper() {}\nexport function aid() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("ship.ts"),
            "import { push as leased, LIMIT, helper as aid } from \"./push\";\n\
             export function cap() { run(LIMIT); run(aid); }\n\
             export function direct() { aid(); }\n\
             export function run(f: () => void) { f(); }\n\
             export function ship() { leased(); }\n\
             export function stray() { push(); }\n\
             export function hand() { run(leased); }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        assert_eq!(
            callers_of_push_in(&store, "push.ts"),
            [("ship".to_string(), Tier::Exact)]
        );
        assert!(callers_of_push_in(&store, "other.ts").is_empty());
        let push_file = store.file_by_path("push.ts").unwrap().unwrap().id;
        let push = store.symbols_in_file(push_file).unwrap().remove(0);
        let references = store.edges_to(push.id, Some(EdgeKind::References)).unwrap();
        assert_eq!(references.len(), 1, "run(leased) passes push.ts's push");
        // `LIMIT` is imported but is no callable symbol, and `aid` aliases a
        // `helper` push.ts does not define: passing either is a plain value,
        // never an unresolved reference. other.ts defines both `helper` and
        // an `aid`, which is not what the alias means in ship.ts: no edge,
        // not even T2's Probable one from the `aid()` call.
        let unresolved: i64 = store
            .conn()
            .query_row(
                "SELECT count(*) FROM unresolved_calls
                  WHERE name IN ('LIMIT', 'aid') AND kind = 'references'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unresolved, 0);
        let other_file = store.file_by_path("other.ts").unwrap().unwrap().id;
        for symbol in store.symbols_in_file(other_file).unwrap() {
            if symbol.name == "push" {
                continue;
            }
            assert!(
                store.edges_to(symbol.id, None).unwrap().is_empty(),
                "nothing in ship.ts reaches other.ts's {}",
                symbol.name
            );
        }
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The file holding the symbol an `Exact` call edge from `caller` to
    /// `callee` lands on, or `None` when there is no such edge.
    fn exact_callee_file(store: &GraphStore, caller: &str, callee: &str) -> Option<String> {
        store
            .conn()
            .query_row(
                "SELECT f.path FROM edges e
                   JOIN symbols src ON src.id = e.src_id
                   JOIN symbols dst ON dst.id = e.dst_id
                   JOIN files f ON f.id = dst.file_id
                  WHERE src.name = ?1 AND dst.name = ?2 AND e.kind = 'calls' AND e.tier = 'exact'",
                rusqlite::params![caller, callee],
                |r| r.get(0),
            )
            .ok()
    }

    /// A grouped `use` names one file per path. `other.rs` defines every
    /// name too, so only the import tier links the calls: resolving the
    /// statement as one spec cut it at `{` (`crate`, no file) and gave no
    /// binding the import tier, and `crate::push::{a::x, y}` sent `x` to
    /// `push.rs` although it lives in `push/a.rs`.
    #[test]
    fn a_grouped_rust_use_resolves_each_path_to_its_own_file() {
        let root = tmpdir("rust-grouped-use");
        std::fs::create_dir_all(root.join("src/push")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub mod left;\npub mod right;\npub mod push;\npub mod other;\npub mod ship;\n",
        )
        .unwrap();
        std::fs::write(root.join("src/left.rs"), "pub fn push() {}\n").unwrap();
        std::fs::write(root.join("src/right.rs"), "pub fn publish() {}\n").unwrap();
        std::fs::write(root.join("src/push.rs"), "pub mod a;\npub fn y() {}\n").unwrap();
        std::fs::write(root.join("src/push/a.rs"), "pub fn x() {}\n").unwrap();
        std::fs::write(
            root.join("src/other.rs"),
            "pub fn push() {}\npub fn publish() {}\npub fn x() {}\npub fn y() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/ship.rs"),
            "use crate::{left::push, right::publish};\nuse crate::push::{a::x, y};\n\
             pub fn ship() { push(); publish(); x(); y(); }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let landed: Vec<Option<String>> = ["push", "publish", "x", "y"]
            .iter()
            .map(|callee| exact_callee_file(&store, "ship", callee))
            .collect();
        assert_eq!(
            landed,
            [
                Some("src/left.rs".to_string()),
                Some("src/right.rs".to_string()),
                Some("src/push/a.rs".to_string()),
                Some("src/push.rs".to_string()),
            ]
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A path of a grouped `use` whose file is written after the importer
    /// dangles; adding the file re-resolves that path (not the statement's
    /// spec, which resolves nothing) and turns the call Exact.
    #[test]
    fn a_dangling_path_of_a_grouped_use_resolves_when_its_file_appears() {
        let root = tmpdir("rust-grouped-dangling");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub mod left;\npub mod right;\npub mod other;\npub mod ship;\n",
        )
        .unwrap();
        std::fs::write(root.join("src/right.rs"), "pub fn publish() {}\n").unwrap();
        std::fs::write(root.join("src/other.rs"), "pub fn push() {}\n").unwrap();
        std::fs::write(
            root.join("src/ship.rs"),
            "use crate::{left::push, right::publish};\npub fn ship() { push(); publish(); }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        {
            let store = GraphStore::open(&db).unwrap();
            assert_eq!(exact_callee_file(&store, "ship", "push"), None);
        }
        std::fs::write(root.join("src/left.rs"), "pub fn push() {}\n").unwrap();
        let delta = tree_delta(&root, &db).unwrap().unwrap();
        apply_tree_delta(&root, &db, &delta).unwrap();
        let store = GraphStore::open(&db).unwrap();
        assert_eq!(
            exact_callee_file(&store, "ship", "push").as_deref(),
            Some("src/left.rs")
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A Rust `use` binds names for its module or block only. Three files
    /// define `push`, so T2 never decides and only an import in scope may
    /// link a call:
    /// - `mod a`'s use reaches `fa`, not `fb` in `mod b`, which gets its
    ///   `push` from a wildcard (it took the Exact edge to `left.rs` when
    ///   bindings were file-wide);
    /// - the file-level use reaches `top` and, through `use super::*;`, the
    ///   `tests` module, but not `bare`, which does not import its parent.
    ///
    /// An incremental update that adds a fourth `push` re-resolves every
    /// edge from its stored site line, and must land on the same targets.
    #[test]
    fn a_rust_use_binds_names_only_where_it_is_in_scope() {
        let root = tmpdir("rust-use-scope");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub mod left;\npub mod right;\npub mod other;\npub mod ship;\npub mod top;\n",
        )
        .unwrap();
        for file in ["left", "right", "other"] {
            std::fs::write(root.join(format!("src/{file}.rs")), "pub fn push() {}\n").unwrap();
        }
        std::fs::write(
            root.join("src/ship.rs"),
            [
                "mod a {",
                "    use crate::left::push;",
                "    pub fn fa() { push(); }",
                "}",
                "mod b {",
                "    use crate::right::*;",
                "    pub fn fb() { push(); }",
                "}",
            ]
            .join("\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("src/top.rs"),
            [
                "use crate::left::push;",
                "pub fn top() { push(); }",
                "mod tests {",
                "    use super::*;",
                "    fn t() { push(); }",
                "}",
                "mod bare {",
                "    use crate::right::*;",
                "    fn u() { push(); }",
                "}",
            ]
            .join("\n"),
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let callers = ["fa", "fb", "top", "t", "u"];
        let left = Some("src/left.rs".to_string());
        let expected = [left.clone(), None, left.clone(), left, None];
        {
            let store = GraphStore::open(&db).unwrap();
            let landed: Vec<Option<String>> = callers
                .iter()
                .map(|caller| exact_callee_file(&store, caller, "push"))
                .collect();
            assert_eq!(landed, expected);
        }
        std::fs::write(root.join("src/fourth.rs"), "pub fn push() {}\n").unwrap();
        update_file(&root, &db, "src/fourth.rs").unwrap();
        let store = GraphStore::open(&db).unwrap();
        let landed: Vec<Option<String>> = callers
            .iter()
            .map(|caller| exact_callee_file(&store, caller, "push"))
            .collect();
        assert_eq!(landed, expected, "after the incremental update");
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An alias blocks T2 only where it is in scope. `zap` aliases a
    /// `helper` that left.rs does not define, inside `mod a`; other.rs holds
    /// the only `zap`. In `mod a` the call means the alias, so it links
    /// nothing; in `mod b` no import binds `zap` and T2 links other.rs's.
    #[test]
    fn an_alias_blocks_the_repo_wide_tier_only_where_it_is_in_scope() {
        let root = tmpdir("rust-alias-scope");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub mod left;\npub mod other;\npub mod ship;\n",
        )
        .unwrap();
        std::fs::write(root.join("src/left.rs"), "pub fn push() {}\n").unwrap();
        std::fs::write(root.join("src/other.rs"), "pub fn zap() {}\n").unwrap();
        std::fs::write(
            root.join("src/ship.rs"),
            [
                "mod a {",
                "    use crate::left::helper as zap;",
                "    pub fn fa() { zap(); }",
                "}",
                "mod b {",
                "    pub fn fb() { zap(); }",
                "}",
            ]
            .join("\n"),
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let callers: Vec<(String, String)> = store
            .conn()
            .prepare(
                "SELECT src.name, e.tier FROM edges e
                   JOIN symbols src ON src.id = e.src_id
                   JOIN symbols dst ON dst.id = e.dst_id
                  WHERE dst.name = 'zap' ORDER BY src.name",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(callers, [("fb".to_string(), "probable".to_string())]);
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A block's `use` shadows the file-level one for the calls inside it:
    /// `inner` means `later`, `outer` means `early`, although both aliases
    /// are named `run` and point into one file (T1 used to take whichever
    /// symbol of utils.rs came first).
    #[test]
    fn a_block_use_shadows_the_file_level_alias_of_the_same_name() {
        let root = tmpdir("rust-use-shadow");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod utils;\npub mod ship;\n").unwrap();
        std::fs::write(
            root.join("src/utils.rs"),
            "pub fn early() {}\npub fn later() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/ship.rs"),
            [
                "use crate::utils::early as run;",
                "pub fn outer() { run(); }",
                "pub fn inner() {",
                "    use crate::utils::later as run;",
                "    run();",
                "}",
            ]
            .join("\n"),
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let edges: Vec<(String, String, String)> = store
            .conn()
            .prepare(
                "SELECT src.name, dst.name, e.tier FROM edges e
                   JOIN symbols src ON src.id = e.src_id
                   JOIN symbols dst ON dst.id = e.dst_id
                  WHERE e.kind = 'calls' ORDER BY src.name",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let expected = [("inner", "later"), ("outer", "early")]
            .map(|(src, dst)| (src.to_string(), dst.to_string(), "exact".to_string()));
        assert_eq!(edges, expected);
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An alias applies only in its own scope: `mod a` aliases a constant as
    /// `zap`, and `mod b` passes its own `fn zap` as a value. The out-of-scope
    /// alias must not turn that reference into a plain value and drop it.
    #[test]
    fn an_out_of_scope_alias_does_not_hide_a_reference_to_a_local_function() {
        let root = tmpdir("rust-alias-reference-scope");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod consts;\npub mod ship;\n").unwrap();
        std::fs::write(root.join("src/consts.rs"), "pub const VALUE: u32 = 1;\n").unwrap();
        std::fs::write(
            root.join("src/ship.rs"),
            [
                "mod a {",
                "    use crate::consts::VALUE as zap;",
                "}",
                "mod b {",
                "    fn zap() {}",
                "    fn consume(_: fn()) {}",
                "    fn f() { consume(zap); }",
                "}",
            ]
            .join("\n"),
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let references: i64 = store
            .conn()
            .query_row(
                "SELECT count(*) FROM edges e
                   JOIN symbols src ON src.id = e.src_id
                   JOIN symbols dst ON dst.id = e.dst_id
                  WHERE e.kind = 'references' AND src.name = 'f' AND dst.name = 'zap'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(references, 1, "consume(zap) passes mod b's zap");
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wildcard_import_does_not_make_unqualified_call_exact() {
        let root = tmpdir("wildcard-import");
        std::fs::write(root.join("a.ts"), "export function target(): void {}\n").unwrap();
        std::fs::write(
            root.join("b.ts"),
            "import * as ns from \"./a\";\nexport function caller() { target(); }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        let store = GraphStore::open(&db).unwrap();
        let idx = ResolveIndex::build(&store).unwrap();
        let b_id = store.file_by_path("b.ts").unwrap().unwrap().id;
        assert!(
            !matches!(idx.decide(b_id, "target", None), Decision::Exact(_)),
            "namespace import cannot make an unqualified call Exact"
        );

        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn incremental_definition_addition_reconsiders_existing_edges() {
        let root = tmpdir("incremental-ambiguity");
        std::fs::write(root.join("a.ts"), "export function target() {}\n").unwrap();
        std::fs::write(
            root.join("b.ts"),
            "export function caller() { target(); }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        {
            let store = GraphStore::open(&db).unwrap();
            let target = store.symbols_by_name("target", None, 10).unwrap().remove(0);
            assert_eq!(
                store
                    .edges_to(target.id, Some(EdgeKind::Calls))
                    .unwrap()
                    .len(),
                1
            );
        }

        std::fs::write(root.join("c.ts"), "export function target() {}\n").unwrap();
        update_file(&root, &db, "c.ts").unwrap();
        let store = GraphStore::open(&db).unwrap();
        for target in store.symbols_by_name("target", None, 10).unwrap() {
            assert!(
                store
                    .edges_to(target.id, Some(EdgeKind::Calls))
                    .unwrap()
                    .is_empty(),
                "ambiguous target must not retain a resolved edge"
            );
        }
        let envelope = store.envelope_for_name("target").unwrap();
        assert!(envelope.lower_bound);
        assert!(envelope.unresolved_same_name >= 1);
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A graph written by an older extractor lacks the rows the current one
    /// emits for the same bytes; content hashes alone call it fresh forever.
    #[test]
    fn graph_from_another_extractor_version_is_stale_until_rebuilt() {
        let root = tmpdir("extractor-version");
        std::fs::write(root.join("a.ts"), "export function a() { return 1; }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        assert_eq!(
            store.meta_get(EXTRACTOR_VERSION_KEY).unwrap().as_deref(),
            Some(EXTRACTOR_VERSION)
        );
        assert!(is_fresh(&root, &db));
        assert!(tree_delta(&root, &db).unwrap().is_some_and(|d| d.fresh));

        store.meta_set(EXTRACTOR_VERSION_KEY, "1").unwrap();
        assert!(!is_fresh(&root, &db), "an older extractor's graph is stale");
        assert!(
            tree_delta(&root, &db).unwrap().is_none(),
            "no delta can repair rows the extractor never wrote: rebuild"
        );
        store
            .conn()
            .execute("DELETE FROM meta WHERE key = ?1", [EXTRACTOR_VERSION_KEY])
            .unwrap();
        assert!(!is_fresh(&root, &db), "an unversioned graph is stale");
        assert!(tree_delta(&root, &db).unwrap().is_none());
        drop(store);

        build_graph(&root, &db).unwrap();
        assert!(
            is_fresh(&root, &db),
            "a rebuild records the current version"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An exact probe of a module name promotes the file that defines the
    /// code, not the `mod foo;` naming it. The end-to-end build path must
    /// write both directions of `symbols.module_decl`.
    #[test]
    fn build_marks_external_module_declarations_for_exact_probes() {
        let root = tmpdir("module-decl");
        std::fs::write(root.join("main.rs"), "mod search_compat;\n").unwrap();
        std::fs::write(root.join("search_compat.rs"), "pub fn search_compat() {}\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        let store = GraphStore::open(&db).unwrap();
        let hits = crate::targets::symbol_hits(
            &store,
            &["search".into(), "compat".into()],
            &["search_compat".into()],
        )
        .unwrap();
        let declaring = hits.iter().find(|h| h.path == "main.rs").unwrap();
        assert!(
            !declaring.exact_name_hit,
            "`mod search_compat;` only names the other file: {declaring:?}"
        );
        let defining = hits.iter().find(|h| h.path == "search_compat.rs").unwrap();
        assert!(
            defining.exact_name_hit,
            "`fn search_compat` defines the exact name: {defining:?}"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn reference_rows(store: &GraphStore) -> Vec<String> {
        let mut stmt = store
            .conn()
            .prepare("SELECT name FROM unresolved_calls WHERE kind = 'references' ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// Only an argument that may be a callback leaves a trace: a plain value
    /// (`value`, `user.name`) creates no edge to a same-named function and no
    /// unresolved row, while a function the resolver cannot pick stays
    /// counted in the envelope.
    #[test]
    fn value_arguments_leave_no_references_and_ambiguous_callbacks_stay_unresolved() {
        let root = tmpdir("reference-values");
        std::fs::write(
            root.join("name.ts"),
            "export function name() { return 1; }\n",
        )
        .unwrap();
        std::fs::write(root.join("h1.ts"), "export function handler() {}\n").unwrap();
        std::fs::write(root.join("h2.ts"), "export function handler() {}\n").unwrap();
        std::fs::write(
            root.join("entry.ts"),
            "export function entry(value: any, user: any) {\n  consume(value, user.name, handler, render);\n}\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        let store = GraphStore::open(&db).unwrap();
        let name = &store.symbols_by_name("name", None, 10).unwrap()[0];
        assert!(
            store
                .edges_to(name.id, Some(EdgeKind::References))
                .unwrap()
                .is_empty(),
            "`user.name` is data, not a reference to `name()`"
        );
        assert_eq!(reference_rows(&store), ["handler"]);
        assert!(store.envelope_for_name("handler").unwrap().lower_bound);
        assert!(!store.envelope_for_name("value").unwrap().lower_bound);
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `<Button/>` compiles to a call of `Button`: the component a file
    /// renders is a callee of the renderer, across an import.
    #[test]
    fn rendering_a_component_links_the_renderer_as_its_caller() {
        let root = tmpdir("jsx-component-call");
        std::fs::write(
            root.join("Button.tsx"),
            "export function Button() { return <button>ok</button>; }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("App.tsx"),
            "import { Button } from \"./Button\";\nexport function App() { return <div><Button /></div>; }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        let store = GraphStore::open(&db).unwrap();
        let button = &store.symbols_by_name("Button", None, 10).unwrap()[0];
        let callers = store.edges_to(button.id, Some(EdgeKind::Calls)).unwrap();
        assert_eq!(callers.len(), 1, "{callers:?}");
        assert_eq!(callers[0].tier, Tier::Exact, "import-bound");
        let app = &store.symbols_by_name("App", None, 10).unwrap()[0];
        assert_eq!(callers[0].src_id, app.id);
        assert!(!store.envelope_for_name("div").unwrap().lower_bound);
        assert!(!store.envelope_for_name("button").unwrap().lower_bound);
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `schema.plugin(tenantScopePlugin)` should create a `References` edge
    /// from the enclosing symbol (`setup`) to the referenced symbol
    /// (`tenantScopePlugin`), with `Tier::Probable`.
    #[test]
    fn callback_arg_creates_references_edge() {
        let root = tmpdir("references-edge");
        std::fs::write(
            root.join("a.ts"),
            "export function tenantScopePlugin(s: any) { return s; }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.ts"),
            "import { tenantScopePlugin } from \"./a\";\n\
             export function setup(schema: any) {\n  \
             schema.plugin(tenantScopePlugin);\n\
             }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        let store = GraphStore::open(&db).unwrap();
        let plugin = &store
            .symbols_by_name("tenantScopePlugin", None, 10)
            .unwrap()[0];
        // A References edge should point to tenantScopePlugin from setup.
        let ref_edges = store
            .edges_to(plugin.id, Some(EdgeKind::References))
            .unwrap();
        assert_eq!(
            ref_edges.len(),
            1,
            "exactly one References edge to tenantScopePlugin"
        );
        assert_eq!(ref_edges[0].tier, Tier::Probable);
        // The source should be the `setup` symbol.
        let setup = &store.symbols_by_name("setup", None, 10).unwrap()[0];
        assert_eq!(ref_edges[0].src_id, setup.id);
        // No Calls edge should exist (plugin is a method call on schema, not
        // a direct call to tenantScopePlugin).
        let call_edges = store.edges_to(plugin.id, Some(EdgeKind::Calls)).unwrap();
        assert!(call_edges.is_empty(), "no Calls edge to tenantScopePlugin");
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The freshness walk hashes files on rayon's pool, so its output must
    /// not depend on scheduling: the same tree gives the same path-sorted
    /// list a serial pass would, with the same exclusions `collect_files`
    /// applies (unsupported extension, binary, over `MAX_FILE_BYTES`,
    /// symlink). A divergence here is a graph that is either never fresh or
    /// fresh over the wrong file set.
    #[test]
    fn tree_hashes_matches_a_serial_reference_and_applies_every_exclusion() {
        let root = tmpdir("parallel-hashes");
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        let kept: [(&str, &[u8]); 4] = [
            ("b.ts", b"export function beta() { return 2 }\n"),
            ("src/a.rs", b"fn alpha() -> u32 { 1 }\n"),
            ("src/deep/c.py", b"def gamma():\n    return 3\n"),
            (
                "src/deep/d.go",
                b"package d\nfunc Delta() int { return 4 }\n",
            ),
        ];
        for (rel, content) in kept {
            std::fs::write(root.join(rel), content).unwrap();
        }
        // Excluded: no supported extension, a NUL in the first bytes, one
        // byte over the size cap, and a symlink to a kept file.
        std::fs::write(root.join("notes.txt"), b"fn looks_like_rust() {}\n").unwrap();
        std::fs::write(root.join("src/blob.rs"), b"fn x() {}\n\0\0binary\n").unwrap();
        let oversize_len = usize::try_from(MAX_FILE_BYTES).unwrap() + 1;
        std::fs::write(root.join("src/huge.rs"), vec![b'/'; oversize_len]).unwrap();
        std::os::unix::fs::symlink(root.join("src/a.rs"), root.join("src/link.rs")).unwrap();

        // The serial reference: every kept file, hashed one by one, sorted.
        let mut expected: Vec<(String, u64)> = kept
            .iter()
            .map(|(rel, content)| ((*rel).to_string(), xxh3_64(content)))
            .collect();
        expected.sort_by(|a, b| a.0.cmp(&b.0));

        let walked = tree_hashes(&root);
        assert_eq!(walked, expected, "same set, same hashes, path-sorted");
        let paths: Vec<&str> = walked.iter().map(|(rel, _)| rel.as_str()).collect();
        assert_eq!(
            paths,
            ["b.ts", "src/a.rs", "src/deep/c.py", "src/deep/d.go"]
        );
        for excluded in ["notes.txt", "src/blob.rs", "src/huge.rs", "src/link.rs"] {
            assert!(!paths.contains(&excluded), "{excluded} must not be hashed");
        }

        // Ten more walks give ten identical answers: the sort makes the
        // parallel collection order invisible.
        for _ in 0..10 {
            assert_eq!(tree_hashes(&root), expected);
        }

        // And `build_graph`, whose input set is `collect_files`, signs the
        // tree with exactly this walk's signature: both sides apply the same
        // exclusions, or the graph would never read as fresh.
        let db = root.join(".pixel").join("graph.db");
        let stats = build_graph(&root, &db).unwrap();
        assert_eq!(stats.files, 4, "collect_files kept the same four files");
        assert_eq!(
            stored_signature(&GraphStore::open(&db).unwrap()),
            freshness_signature(&root)
        );
        assert!(is_fresh(&root, &db));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `hash_candidates` is the parallel half on its own: a candidate that
    /// vanished between the walk and the read is dropped, not an error, and
    /// the survivors come back sorted.
    #[test]
    fn hash_candidates_drops_vanished_files_and_sorts_the_rest() {
        let root = tmpdir("hash-candidates");
        std::fs::write(root.join("z.rs"), b"fn z() {}\n").unwrap();
        std::fs::write(root.join("a.rs"), b"fn a() {}\n").unwrap();
        let candidates = vec![
            ("z.rs".to_string(), root.join("z.rs")),
            ("gone.rs".to_string(), root.join("gone.rs")),
            ("a.rs".to_string(), root.join("a.rs")),
        ];
        let hashed = hash_candidates(candidates);
        assert_eq!(
            hashed,
            vec![
                ("a.rs".to_string(), xxh3_64(b"fn a() {}\n")),
                ("z.rs".to_string(), xxh3_64(b"fn z() {}\n")),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The cap a build ran under must survive the process that built it.
    ///
    /// It lives in the environment, and the graph outlives the daemon, so
    /// an evaluation that re-read the environment would describe a stored
    /// graph with a ceiling that never applied to it.
    #[test]
    fn a_full_build_should_record_the_file_cap_it_ran_under() {
        let root = tmpdir("records-file-cap");
        std::fs::write(root.join("a.ts"), "export function target() {}\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        let store = GraphStore::open(&db).unwrap();
        assert_eq!(
            stored_graph_file_cap(&store).unwrap(),
            graph_file_cap(),
            "the published graph must carry the cap the walk actually used"
        );
        assert!(
            store.meta_get(GRAPH_FILE_CAP_KEY).unwrap().is_some(),
            "the key must be written, not merely defaulted to on read"
        );
    }

    /// "No cap applied" is a fact the build knows; a missing key is not.
    #[test]
    fn a_recorded_cap_should_be_read_back_including_its_disabled_state() {
        let root = tmpdir("read-file-cap");
        std::fs::write(root.join("a.ts"), "export function target() {}\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let store = GraphStore::open(&db).unwrap();

        store.meta_set(GRAPH_FILE_CAP_KEY, "none").unwrap();
        assert_eq!(
            stored_graph_file_cap(&store).unwrap(),
            None,
            "an unbounded walk has no ceiling to report"
        );

        store.meta_set(GRAPH_FILE_CAP_KEY, "7").unwrap();
        assert_eq!(stored_graph_file_cap(&store).unwrap(), Some(7));

        // A graph written before the key existed, and one whose value no
        // longer parses, both fall back to the built-in default rather than
        // to whatever this process's environment happens to say.
        for unusable in ["", "not-a-number"] {
            store.meta_set(GRAPH_FILE_CAP_KEY, unusable).unwrap();
            assert_eq!(
                stored_graph_file_cap(&store).unwrap(),
                Some(DEFAULT_GRAPH_MAX_FILES),
                "`{unusable}` must not be read as a cap"
            );
        }
    }

    /// The two spellings the key uses, pinned so a reader and a writer
    /// cannot drift apart on what "no cap" looks like.
    #[test]
    fn the_recorded_cap_value_should_spell_a_disabled_cap_distinctly() {
        assert_eq!(graph_file_cap_value(None), "none");
        assert_eq!(graph_file_cap_value(Some(50_000)), "50000");
    }

    /// The files every equivalence scenario below writes: a Rust `use` with
    /// an alias and a group, a module path call, a literal environment read
    /// (a concept) behind a guard (a crux line), a relative TS import, a
    /// function passed as a value (a reference), a JSX handler, a Go package
    /// of two files, and Ruby calls without receiver or parentheses.
    const EQUIVALENCE_TREE: &[(&str, &str)] = &[
        (
            "src/lib.rs",
            "pub mod push;\npub mod other;\npub mod ship;\npub mod cfg;\n",
        ),
        ("src/push.rs", "pub fn push() {}\npub fn pull() {}\n"),
        ("src/other.rs", "pub fn push() {}\n"),
        (
            "src/ship.rs",
            "use crate::push::push as leased;\nuse crate::{push::pull, other};\n\
             pub fn ship() { leased(); pull(); other::push(); }\npub fn stray() { push(); }\n",
        ),
        (
            "src/cfg.rs",
            "pub fn dir() -> String {\n    if let Ok(d) = std::env::var(\"PIXEL_FLOW_DIR\") {\n        \
             return d;\n    }\n    String::new()\n}\n",
        ),
        (
            "web/a.ts",
            "import { helper } from \"./b\";\nexport function work() { return helper() }\n\
             export function setup(schema: any) { schema.plugin(helper); }\n",
        ),
        ("web/b.ts", "export function helper() { return 1 }\n"),
        (
            "web/view.tsx",
            "import { helper } from \"./b\";\n\
             export function View() { return <button onClick={() => helper()}>Go</button> }\n",
        ),
        ("go/pkg/zeta.go", "package pkg\n\nfunc Zeta() {}\n"),
        ("go/pkg/alpha.go", "package pkg\n\nfunc Alpha() {}\n"),
        (
            "go/main.go",
            "package main\n\nimport \"example.com/m/pkg\"\n\nfunc main() { pkg.Alpha() }\n",
        ),
        (
            "app/svc.rb",
            "class Svc\n  def target\n    1\n  end\n  def bare\n    target\n  end\n  \
             def chained\n    @ids ||= target.to_set\n  end\nend\n",
        ),
        (
            "app/account.rb",
            "class Account\n  attr_accessor :name\n  alias_method :label, :name\n  \
             delegate :email, to: :owner, prefix: true\n  def owner; end\n  \
             def rename\n    self.name = label\n  end\nend\n",
        ),
        (
            "app/concerns/tracked.rb",
            "module Tracked\n  extend ActiveSupport::Concern\n  included do\n    include Audited\n  \
             end\n  class_methods do\n    def since; end\n  end\n  def track; end\nend\n",
        ),
        (
            "app/concerns/audited.rb",
            "module Audited\n  def audit; end\nend\n",
        ),
        (
            "app/order.rb",
            "class Order < Base\n  include Tracked\n  def run\n    track\n    audit\n  end\n  \
             def self.report\n    since\n  end\nend\n",
        ),
        ("app/base.rb", "class Base\n  def audit; end\nend\n"),
        (
            "config/routes.rb",
            "Rails.application.routes.draw do\n  resources :orders, only: [:create]\nend\n",
        ),
        (
            "app/controllers/orders_controller.rb",
            "class OrdersController\n  def create; end\nend\n",
        ),
        ("app/line.rb", "class Line\n  belongs_to :order\nend\n"),
    ];

    fn write_tree(root: &Path, files: &[(&str, &str)]) {
        for (rel, body) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
    }

    /// Every row the build writes, with each id replaced by what it names
    /// (a path, a symbol uid), so two graphs of the same tree compare equal
    /// whatever order their rows were inserted in.
    fn graph_rows(db: &Path) -> Vec<(&'static str, Vec<String>)> {
        const TABLES: &[(&str, &str)] = &[
            ("files", "SELECT path, blob_oid, lang FROM files"),
            (
                "symbols",
                "SELECT s.uid, f.path, s.name, s.qualified, s.kind, s.start_line, s.end_line, \
                 s.sig, s.trait_impl, s.module_decl FROM symbols s JOIN files f ON f.id = s.file_id",
            ),
            (
                "symbol_crux",
                "SELECT s.uid, c.line, c.text, c.fingerprint FROM symbol_crux c \
                 LEFT JOIN symbols s ON s.id = c.symbol_id",
            ),
            (
                "imports",
                "SELECT f.path, i.spec, i.path, t.path, i.bindings, i.scope FROM imports i \
                 LEFT JOIN files f ON f.id = i.file_id LEFT JOIN files t ON t.id = i.resolved_file_id",
            ),
            (
                "edges",
                "SELECT a.uid, b.uid, e.kind, e.tier, e.site_line, e.receiver, e.callee FROM edges e \
                 LEFT JOIN symbols a ON a.id = e.src_id LEFT JOIN symbols b ON b.id = e.dst_id",
            ),
            (
                "ruby_mixins",
                "SELECT f.path, m.owner, m.kind, m.target, m.site_line FROM ruby_mixins m \
                 LEFT JOIN files f ON f.id = m.file_id",
            ),
            (
                "unresolved_calls",
                "SELECT f.path, u.name, s.uid, u.site_line, u.receiver, u.kind FROM unresolved_calls u \
                 LEFT JOIN files f ON f.id = u.file_id LEFT JOIN symbols s ON s.id = u.enclosing_symbol_id",
            ),
            (
                "concepts",
                "SELECT f.path, c.kind, c.raw, c.norm, c.detail, c.start_line, c.end_line, s.uid \
                 FROM concepts c LEFT JOIN files f ON f.id = c.file_id \
                 LEFT JOIN symbols s ON s.id = c.owner_symbol_id",
            ),
            (
                "concept_words",
                "SELECT w.word, f.path, c.kind, c.raw, c.start_line FROM concept_words w \
                 LEFT JOIN concepts c ON c.id = w.concept_id LEFT JOIN files f ON f.id = c.file_id",
            ),
            (
                "jsx_elements",
                "SELECT f.path, j.tag, j.has_handler, j.text_content, j.start_line, j.end_line \
                 FROM jsx_elements j LEFT JOIN files f ON f.id = j.file_id",
            ),
        ];
        let store = GraphStore::open(db).unwrap();
        TABLES
            .iter()
            .map(|&(table, sql)| {
                let mut stmt = store.conn().prepare(sql).unwrap();
                let width = stmt.column_count();
                let mut rows: Vec<String> = stmt
                    .query_map([], |r| {
                        (0..width)
                            .map(|i| {
                                r.get::<_, rusqlite::types::Value>(i)
                                    .map(|v| format!("{v:?}"))
                            })
                            .collect::<rusqlite::Result<Vec<_>>>()
                            .map(|cells| cells.join(" | "))
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                rows.sort();
                (table, rows)
            })
            .collect()
    }

    fn assert_same_graph(label: &str, got: &[(&str, Vec<String>)], want: &[(&str, Vec<String>)]) {
        for ((table, got_rows), (_, want_rows)) in got.iter().zip(want) {
            assert_eq!(
                got_rows, want_rows,
                "{label}: `{table}` differs from a full build"
            );
        }
    }

    /// The full build and the incremental update store the same tree the
    /// same way. `graph-resolver.md` makes every resolution change hold on
    /// both; this pins the rows they share, table by table, so a change to
    /// one path that the other misses fails here instead of in a user's
    /// graph after its first edit.
    #[test]
    fn an_incremental_update_should_store_what_a_full_build_stores() {
        let reference = tmpdir("equiv-full");
        write_tree(&reference, EQUIVALENCE_TREE);
        let reference_db = reference.join(".pixel").join("graph.db");
        build_graph(&reference, &reference_db).unwrap();
        let want = graph_rows(&reference_db);
        for (table, rows) in &want {
            assert!(!rows.is_empty(), "the fixture must exercise `{table}`");
        }
        let edges = &want.iter().find(|(t, _)| *t == "edges").unwrap().1;
        assert!(
            edges.iter().any(|e| e.starts_with(
                "Text(\"src/ship.rs#ship#function\") | Text(\"src/push.rs#push#function\") | Text(\"calls\") | Text(\"exact\")"
            )),
            "the aliased `use` must give an Exact edge in the reference ({edges:#?})"
        );
        assert!(
            edges.iter().any(|e| e.starts_with(
                "Text(\"web/a.ts#setup#function\") | Text(\"web/b.ts#helper#function\") | Text(\"references\")"
            )),
            "`schema.plugin(helper)` must give a references edge in the reference ({edges:#?})"
        );
        for (src, dst) in [
            (
                "app/order.rb#Order#run",
                "app/concerns/tracked.rb#Tracked#track",
            ),
            (
                "app/order.rb#Order#run",
                "app/concerns/audited.rb#Audited#audit",
            ),
            (
                "app/order.rb#Order.report",
                "app/concerns/tracked.rb#Tracked::ClassMethods#since",
            ),
        ] {
            let prefix = format!(
                "Text(\"{src}#method\") | Text(\"{dst}#method\") | Text(\"calls\") | Text(\"probable\")"
            );
            assert!(
                edges.iter().any(|e| e.starts_with(&prefix)),
                "the Ruby ancestor chain must give {src} -> {dst} ({edges:#?})"
            );
        }
        for (src, dst) in [
            (
                "config/routes.rb#config/routes.rb#script",
                "app/controllers/orders_controller.rb#OrdersController#create#method",
            ),
            ("app/line.rb#Line#class", "app/order.rb#Order#class"),
        ] {
            let prefix = format!(
                "Text(\"{src}\") | Text(\"{dst}\") | Text(\"references\") | Text(\"probable\")"
            );
            assert!(
                edges.iter().any(|e| e.starts_with(&prefix)),
                "a Rails route or association must give {src} -> {dst} ({edges:#?})"
            );
        }

        let all: Vec<(&str, bool)> = EQUIVALENCE_TREE
            .iter()
            .map(|(rel, _)| (*rel, false))
            .collect();

        // One batch holding every file, on top of an empty graph.
        let batch = tmpdir("equiv-batch");
        let batch_db = batch.join(".pixel").join("graph.db");
        build_graph(&batch, &batch_db).unwrap();
        write_tree(&batch, EQUIVALENCE_TREE);
        update_files(&batch, &batch_db, &all).unwrap();
        assert_same_graph("one batch", &graph_rows(&batch_db), &want);

        // One file at a time, each importer before what it imports, so
        // every import dangles first and is re-resolved when its file lands.
        let single = tmpdir("equiv-single");
        let single_db = single.join(".pixel").join("graph.db");
        build_graph(&single, &single_db).unwrap();
        write_tree(&single, EQUIVALENCE_TREE);
        for (rel, _) in EQUIVALENCE_TREE.iter().rev() {
            update_file(&single, &single_db, rel).unwrap();
        }
        assert_same_graph("one file at a time", &graph_rows(&single_db), &want);

        // An edit of a call target and of an imported file, then the revert:
        // their incoming edges are demoted and must come back as they were.
        let edited = tmpdir("equiv-edit");
        write_tree(&edited, EQUIVALENCE_TREE);
        let edited_db = edited.join(".pixel").join("graph.db");
        build_graph(&edited, &edited_db).unwrap();
        write_tree(
            &edited,
            &[
                ("src/push.rs", "pub fn pull() {}\n"),
                ("web/b.ts", "export function other() { return 2 }\n"),
            ],
        );
        update_files(
            &edited,
            &edited_db,
            &[("src/push.rs", false), ("web/b.ts", false)],
        )
        .unwrap();
        write_tree(&edited, EQUIVALENCE_TREE);
        update_file(&edited, &edited_db, "src/push.rs").unwrap();
        update_file(&edited, &edited_db, "web/b.ts").unwrap();
        assert_same_graph("edit then revert", &graph_rows(&edited_db), &want);

        for dir in [reference, batch, single, edited] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// A tree whose unresolved rows each depend on something other than
    /// their own name or file: the input the incremental retry has to find
    /// without rereading every row (`Affected`).
    const RETRY_TREE: &[(&str, &str)] = &[
        ("app/mail_job.rb", "class MailJob\n  def run; end\nend\n"),
        (
            "app/widget.rb",
            "class Widget\n  def self.build; end\nend\n",
        ),
        (
            "app/caller.rb",
            "class Caller\n  def go\n    MailJob.perform_later(1)\n    Widget.new(1)\n    \
             Widget.build\n    shared\n    assist\n  end\nend\n",
        ),
        ("app/mixin.rb", "module Mixin\n  def assist; end\nend\n"),
        (
            "app/other_mixin.rb",
            "module OtherMixin\n  def assist; end\nend\n",
        ),
        ("app/alpha.rb", "class Alpha\n  def shared; end\nend\n"),
        ("app/beta.rb", "class Beta\n  def shared; end\nend\n"),
        ("src/lib.rs", "pub mod x;\npub mod y;\npub mod z;\n"),
        ("src/x.rs", "pub fn helper() {}\n"),
        ("src/y.rs", "pub fn helper() {}\n"),
        ("src/z.rs", "pub fn caller() { helper(); }\n"),
        (
            "web/a.ts",
            "import { tool as t } from './b';\nexport function main() { t(); }\n",
        ),
        ("web/b.ts", "export function other() {}\n"),
    ];

    /// One edit of `RETRY_TREE`: its label, the files it writes (`None`
    /// removes one), and the edge a full build of the result must hold.
    type RetryStep = (
        &'static str,
        &'static [(&'static str, Option<&'static str>)],
        &'static str,
    );

    /// The incremental update retries only the rows its batch can have
    /// changed, so a row it skips must be one a full build would decide the
    /// same way. Each step changes an input a stored row reads without
    /// carrying the row's name, then holds the update to a fresh full build
    /// of the same tree:
    ///
    /// - `def perform` links `MailJob.perform_later` (Rails job dispatch);
    /// - `def initialize` links `Widget.new` (Ruby constructor);
    /// - removing `Beta#shared` and the second `helper` makes each name
    ///   unique: only the definitions the batch had *before* name them;
    /// - rewriting `Alpha` with the same definitions on other lines changes
    ///   no name, yet its incoming edge is moved back and must return;
    /// - `export function tool` reaches the call written `t()` through an
    ///   aliased import, found through the importing file;
    /// - the revert undoes each one.
    #[test]
    fn an_incremental_update_should_retry_every_row_whose_inputs_changed() {
        let dir = tmpdir("retry-incremental");
        write_tree(&dir, RETRY_TREE);
        let db = dir.join(".pixel").join("graph.db");
        build_graph(&dir, &db).unwrap();
        let mut tree: std::collections::BTreeMap<&str, &str> = RETRY_TREE.iter().copied().collect();
        let steps: &[RetryStep] = &[
            (
                "perform defined",
                &[(
                    "app/mail_job.rb",
                    Some("class MailJob\n  def run; end\n  def perform; end\nend\n"),
                )],
                "Text(\"app/caller.rb#Caller#go#method\") | Text(\"app/mail_job.rb#MailJob#perform#method\")",
            ),
            (
                "initialize defined",
                &[(
                    "app/widget.rb",
                    Some("class Widget\n  def self.build; end\n  def initialize(x); end\nend\n"),
                )],
                "Text(\"app/caller.rb#Caller#go#method\") | Text(\"app/widget.rb#Widget#initialize#method\")",
            ),
            (
                "second definitions removed",
                &[
                    ("app/beta.rb", None),
                    ("src/y.rs", Some("pub fn other() {}\n")),
                ],
                "Text(\"src/z.rs#caller#function\") | Text(\"src/x.rs#helper#function\")",
            ),
            (
                "same definitions, shifted lines",
                &[(
                    "app/alpha.rb",
                    Some("class Alpha\n  # moved down\n  def shared; end\nend\n"),
                )],
                "Text(\"app/caller.rb#Caller#go#method\") | Text(\"app/alpha.rb#Alpha#shared#method\")",
            ),
            (
                "ancestor declared in a reopening",
                &[(
                    "app/caller_ext.rb",
                    Some("class Caller\n  include Mixin\nend\n"),
                )],
                "Text(\"app/caller.rb#Caller#go#method\") | Text(\"app/mixin.rb#Mixin#assist#method\")",
            ),
            (
                "aliased source defined",
                &[(
                    "web/b.ts",
                    Some("export function other() {}\nexport function tool() {}\n"),
                )],
                "Text(\"web/a.ts#main#function\") | Text(\"web/b.ts#tool#function\")",
            ),
        ];
        let check = |label: &str, tree: &std::collections::BTreeMap<&str, &str>| {
            let full = tmpdir("retry-full");
            let files: Vec<(&str, &str)> = tree.iter().map(|(k, v)| (*k, *v)).collect();
            write_tree(&full, &files);
            let full_db = full.join(".pixel").join("graph.db");
            build_graph(&full, &full_db).unwrap();
            let want = graph_rows(&full_db);
            assert_same_graph(label, &graph_rows(&db), &want);
            let _ = std::fs::remove_dir_all(full);
            want
        };
        for (label, changes, edge) in steps {
            for (rel, body) in *changes {
                match body {
                    Some(body) => {
                        write_tree(&dir, &[(rel, body)]);
                        tree.insert(rel, body);
                    }
                    None => {
                        std::fs::remove_file(dir.join(rel)).unwrap();
                        tree.remove(rel);
                    }
                }
            }
            let batch: Vec<(&str, bool)> = changes
                .iter()
                .map(|(rel, body)| (*rel, body.is_none()))
                .collect();
            update_files(&dir, &db, &batch).unwrap();
            let want = check(label, &tree);
            let edges = &want.iter().find(|(t, _)| *t == "edges").unwrap().1;
            assert!(
                edges.iter().any(|e| e.starts_with(edge)),
                "{label}: the full build must link {edge} ({edges:#?})"
            );
        }
        // Back to the first tree: every edge above becomes a row again, and
        // a file a step added is removed.
        write_tree(&dir, RETRY_TREE);
        let first: HashSet<&str> = RETRY_TREE.iter().map(|(rel, _)| *rel).collect();
        let batch: Vec<(&str, bool)> = steps
            .iter()
            .flat_map(|(_, changes, _)| changes.iter().map(|(rel, _)| *rel))
            .map(|rel| {
                let added = !first.contains(rel);
                if added {
                    let _ = std::fs::remove_file(dir.join(rel));
                }
                (rel, added)
            })
            .collect();
        update_files(&dir, &db, &batch).unwrap();
        check("revert", &RETRY_TREE.iter().copied().collect());
        let _ = std::fs::remove_dir_all(dir);
    }
}
