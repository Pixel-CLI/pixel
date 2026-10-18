// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! pixel-mvp's index in front of this graph, for Rust (ADR-0035 of the
//! pixel-mvp research repository; `crates/mvp-index/ORIGIN.md`).
//!
//! The adapter keeps this graph's symbols and answers, in this graph's
//! rows, what pixel-mvp's rules decide at each Rust site:
//!
//! - a call pixel-mvp resolves is an `exact` `calls` edge: its rules link a
//!   site only when the language proves the target (C-10 there), so the
//!   name-only guesses this graph makes as `probable` have no counterpart;
//! - a reference it resolves is a `probable` `references` edge, the tier
//!   every reference has here (`plan`'s dead-code check relies on it);
//! - a site it leaves unresolved is an `unresolved_calls` row, so the
//!   `lower_bound` of `impact` and `uses` still counts it, when some symbol
//!   carries its name (any other can stand for no symbol of this graph);
//! - a Rust import takes the file pixel-mvp resolved it to, or none.
//!
//! Symbols are paired as the 2026-10-14 comparison paired them: path,
//! qualified name and kind, then path and qualified name, then path, last
//! segment and line. A row with no symbol of this graph at one end is
//! dropped and counted: today that is Rust test code, which this graph does
//! not extract (`rust_is_test_container`).
//!
//! [`rust_rows`] only reads this graph. The switch writes the rows in:
//! unless `PIXEL_GRAPH_SOURCE_RUST=v1` ([`RustSource`]), the full build and
//! every incremental update replace the Rust rows with pixel-mvp's
//! ([`switch_rust`]), the graph records where its Rust edges come from
//! ([`GRAPH_SOURCE_KEY`]), and the resolver no longer offers a Rust symbol
//! to a caller of another language ([`rust_is_walled`]): a name-only match
//! across languages is what the comparison found wrong (1 116 Rust calls
//! into Python and TypeScript on pixel's repository, 1 302 the other way).
//!
//! Ruby is merged, not replaced: [`ruby`] keeps this graph's edge wherever
//! pixel-mvp abstains (ADR-0037 there).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use mvp_model::{Graph, Lang};

use crate::store::{
    DELETE_FILE_UNRESOLVED_CALLS, EdgeKind, EdgeRow, ExecCached, GraphStore, StoreError, Tier,
};

pub mod ruby;
pub mod ruby_switch;

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

/// The pixel-mvp commit the `mvp-*` crates were copied from. A test holds it
/// equal to `crates/mvp-index/ORIGIN.md`. It stays fixed after the handover;
/// extraction changes bump the language versions, storage changes the schema.
pub const MVP_COMMIT: &str = "e91e8da";

/// Meta key: where the graph's Rust edges come from. Absent when they are
/// this graph's own resolver's, so a graph built with the switch off is
/// the graph built before the switch existed.
pub const GRAPH_SOURCE_KEY: &str = "graph_source";

/// Meta key: the generation of pixel-mvp's index the graph's Rust rows were
/// last written from ([`merge_into`], [`merge_again`]). Any full build
/// forgets it ([`record_source`]); absent, or naming another generation
/// than the one a refresh started from, every Rust file is written again.
pub const RUST_GENERATION_KEY: &str = "rust_mvp_generation";

/// The switch: `v1` takes Rust calls, references and imports from this
/// graph's resolver; unset, or anything else, from pixel-mvp's index.
pub const RUST_SOURCE_ENV: &str = "PIXEL_GRAPH_SOURCE_RUST";

/// The [`GRAPH_SOURCE_KEY`] value of a graph the switch asked to take from
/// pixel-mvp where pixel-mvp could not index it ([`crate_root`]): its Rust
/// edges are this graph's resolver's. Accepted as current while the switch
/// is on, so such a tree is not rebuilt on every read; the next full build
/// tries pixel-mvp again.
pub const MVP_UNAVAILABLE: &str = "rust=v1, pixel-mvp unavailable";

/// Where a graph takes its Rust edges from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RustSource {
    /// This graph's own resolver.
    V1,
    /// pixel-mvp's index, through this adapter.
    Mvp,
}

impl RustSource {
    /// [`RUST_SOURCE_ENV`] as the process sees it ([`RustSource::parse`]).
    pub fn from_env() -> Self {
        Self::parse(std::env::var(RUST_SOURCE_ENV).ok().as_deref())
    }

    /// `v1` is [`RustSource::V1`]; anything else, or nothing, is the
    /// default, [`RustSource::Mvp`].
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("v1") => RustSource::V1,
            _ => RustSource::Mvp,
        }
    }
}

/// The [`GRAPH_SOURCE_KEY`] value of a graph whose Rust edges are
/// from the imported index: the fixed origin and current extractor versions.
/// An extraction change bumps its version and makes existing graphs stale.
pub fn mvp_source() -> String {
    format!("rust=mvp {MVP_COMMIT} {}", mvp_index::extractor())
}

/// Whether a graph that recorded `stored` under [`GRAPH_SOURCE_KEY`] was
/// built from what `wanted` asks for.
pub fn source_is_current(stored: Option<&str>, wanted: RustSource) -> bool {
    match wanted {
        RustSource::V1 => stored.is_none(),
        RustSource::Mvp => {
            stored == Some(MVP_UNAVAILABLE) || stored.is_some_and(|s| s == mvp_source())
        }
    }
}

/// Records where the graph's Rust edges come from: `None` removes the key.
/// The generation its Rust rows were written from goes with it: the build
/// that records a source writes them again ([`merge_into`]) or not at all.
pub(crate) fn record_source(store: &GraphStore, source: Option<&str>) -> Result<(), StoreError> {
    store.conn().exec_cached(
        "DELETE FROM meta WHERE key = ?1",
        rusqlite::params![RUST_GENERATION_KEY],
    )?;
    match source {
        Some(value) => store.meta_set(GRAPH_SOURCE_KEY, value),
        None => {
            store.conn().exec_cached(
                "DELETE FROM meta WHERE key = ?1",
                rusqlite::params![GRAPH_SOURCE_KEY],
            )?;
            Ok(())
        }
    }
}

/// Whether the graph's Rust edges are pixel-mvp's: then this graph's
/// resolver decides no Rust site, and offers no Rust symbol to a caller of
/// another language.
pub fn rust_is_walled(store: &GraphStore) -> Result<bool, StoreError> {
    Ok(store
        .meta_get(GRAPH_SOURCE_KEY)?
        .is_some_and(|value| value.starts_with("rust=mvp ")))
}

/// Which Rust files a read or a write of pixel-mvp's rows covers.
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    /// Every Rust file of the graph.
    All,
    /// The Rust files of the graph at these paths; any other path is
    /// ignored.
    Files(&'a BTreeSet<String>),
}

/// Replaces the Rust rows of `store` with `rows`: every `calls` and
/// `references` edge from a Rust symbol, every `unresolved_calls` row of a
/// Rust file, and the target of every Rust import.
pub fn apply(store: &GraphStore, rows: &RustRows) -> Result<(), StoreError> {
    apply_in(store, rows, Scope::All)
}

/// [`apply`] over the Rust files of `scope` only: their edges and
/// `unresolved_calls` rows are deleted, then `rows` written, which
/// [`rust_rows_in`] read over the same scope.
pub fn apply_in(store: &GraphStore, rows: &RustRows, scope: Scope<'_>) -> Result<(), StoreError> {
    let conn = store.conn();
    match scope {
        Scope::All => {
            conn.exec_cached(
                "DELETE FROM edges WHERE kind IN ('calls', 'references') AND src_id IN
                   (SELECT s.id FROM symbols s JOIN files f ON f.id = s.file_id
                     WHERE f.lang = 'rust')",
                [],
            )?;
            conn.exec_cached(
                "DELETE FROM unresolved_calls
                  WHERE file_id IN (SELECT id FROM files WHERE lang = 'rust')",
                [],
            )?;
            conn.exec_cached(
                "UPDATE symbols SET unnamed_calls = 0
                  WHERE file_id IN (SELECT id FROM files WHERE lang = 'rust')",
                [],
            )?;
        }
        Scope::Files(paths) => {
            for file_id in rust_file_ids(store, paths)?.into_values() {
                conn.exec_cached(
                    "DELETE FROM edges WHERE kind IN ('calls', 'references') AND src_id IN
                       (SELECT id FROM symbols WHERE file_id = ?1)",
                    rusqlite::params![file_id],
                )?;
                conn.exec_cached(DELETE_FILE_UNRESOLVED_CALLS, rusqlite::params![file_id])?;
                conn.exec_cached(
                    "UPDATE symbols SET unnamed_calls = 0 WHERE file_id = ?1",
                    rusqlite::params![file_id],
                )?;
            }
        }
    }
    for (&symbol, &n) in &rows.unnamed {
        store.add_unnamed_calls(symbol, n)?;
    }
    for edge in rows.calls.iter().chain(&rows.references) {
        store.insert_edge(edge)?;
    }
    for u in &rows.unresolved {
        store.insert_unresolved_call(
            u.file_id,
            &u.name,
            u.enclosing,
            u.site_line,
            u.receiver.as_deref(),
            u.kind,
        )?;
    }
    for (import, target) in &rows.imports {
        conn.exec_cached(
            "UPDATE imports SET resolved_file_id = ?2 WHERE id = ?1",
            rusqlite::params![import, target],
        )?;
    }
    Ok(())
}

/// Whether the switch can take `root`'s Rust edges from pixel-mvp: `root`
/// holds a `Cargo.toml`. pixel-mvp's Rust rules resolve a module path from
/// a Cargo crate root, so in a tree without one (a bare `src/` of `.rs`
/// files, or no Rust at all) they prove nearly nothing, and pixel-mvp's
/// index is not built there. A Rust crate kept below the root (`rust/`,
/// `backend/`) is left to this graph's resolver too.
pub fn crate_root(root: &Path) -> bool {
    root.join("Cargo.toml").is_file()
}

/// [`refresh`] where [`crate_root`] holds, an error elsewhere.
pub fn refresh_crate(root: &Path, state: &Path) -> Result<mvp_store::Store, BoxErr> {
    if !crate_root(root) {
        return Err(format!(
            "{} has no Cargo.toml: pixel-mvp resolves Rust paths from a crate root",
            root.display()
        )
        .into());
    }
    refresh(root, state)
}

/// The switch at work: refreshes pixel-mvp's index of `root` under `state`
/// ([`refresh_crate`]) and replaces the Rust rows of `store` with its
/// answer ([`merge_into`]).
pub fn switch_rust(root: &Path, store: &GraphStore, state: &Path) -> Result<(), BoxErr> {
    let mvp = refresh_crate(root, state)?;
    merge_into(store, &mvp)
}

/// Replaces every Rust row of `store` with what pixel-mvp's index (`mvp`)
/// decides ([`apply`]), and records the generation they were written from
/// ([`RUST_GENERATION_KEY`]).
pub fn merge_into(store: &GraphStore, mvp: &mvp_store::Store) -> Result<(), BoxErr> {
    apply(store, &rust_rows(store, mvp)?)?;
    store.meta_set(RUST_GENERATION_KEY, &mvp.provenance()?.generation)?;
    Ok(())
}

/// The Rust rows written again after an update whose files are `batch`,
/// once the resolver's passes moved the rows of the files `moved` and the
/// refresh of pixel-mvp's index (`mvp`) changed `changes`; `names_before`
/// counts the names the batch's files defined before it ([`names_in`]).
///
/// A file's rows read pixel-mvp's sites of the file, the pairs of the
/// file's symbols and those of the symbols its sites resolve to, the files
/// its imports resolve to, and, for an unresolved reference, whether any
/// symbol carries its name. So a file is written again when any of these
/// may have moved: it is in `batch` or `moved`, rewritten or redirected by
/// the refresh, or resolves a site into a file of `batch` or rewritten
/// (whose symbols this graph numbers anew or pixel-mvp read again). Every
/// other file keeps its rows, without a statement. Every file is written
/// again ([`merge_into`]) when the rows were not written from the
/// generation the refresh started from, or when the batch made a name
/// defined nowhere else appear or disappear: an unresolved reference to it
/// in any file is recorded or dropped.
pub fn merge_again(
    store: &GraphStore,
    mvp: &mvp_store::Store,
    changes: &Changes,
    batch: &[&str],
    moved: &BTreeSet<String>,
    names_before: &HashMap<String, i64>,
) -> Result<(), BoxErr> {
    let written_from = store.meta_get(RUST_GENERATION_KEY)?;
    if changes.before.is_none()
        || written_from != changes.before
        || defined_names_moved(
            store,
            names_before,
            &names_in(store, batch.iter().copied())?,
        )?
    {
        return merge_into(store, mvp);
    }
    let mut again: BTreeSet<String> = moved.clone();
    again.extend(batch.iter().map(|path| (*path).to_string()));
    again.extend(changes.redirected.iter().cloned());
    let mut defined: BTreeSet<String> = changes.rewritten.clone();
    defined.extend(batch.iter().map(|path| (*path).to_string()));
    for path in &defined {
        again.extend(mvp.files_reaching(path)?);
    }
    again.extend(defined);
    let scope = Scope::Files(&again);
    apply_in(store, &rust_rows_in(store, mvp, scope)?, scope)?;
    store.meta_set(RUST_GENERATION_KEY, &mvp.provenance()?.generation)?;
    Ok(())
}

/// How many symbols of the files at `paths` carry each name.
pub fn names_in<'a>(
    store: &GraphStore,
    paths: impl IntoIterator<Item = &'a str>,
) -> Result<HashMap<String, i64>, StoreError> {
    let mut stmt = store.conn().prepare_cached(
        "SELECT s.name, count(*) FROM symbols s JOIN files f ON f.id = s.file_id
          WHERE f.path = ?1 GROUP BY s.name",
    )?;
    let mut out: HashMap<String, i64> = HashMap::new();
    for path in paths {
        let rows = stmt.query_map([path], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (name, n) = row?;
            *out.entry(name).or_default() += n;
        }
    }
    Ok(out)
}

/// Whether a name the batch's files defined (`before`) or define (`after`),
/// on one side only, is defined by no other symbol of `store`: then it went
/// from defined somewhere to defined nowhere, or the other way.
fn defined_names_moved(
    store: &GraphStore,
    before: &HashMap<String, i64>,
    after: &HashMap<String, i64>,
) -> Result<bool, StoreError> {
    let mut count = store
        .conn()
        .prepare_cached("SELECT count(*) FROM symbols WHERE name = ?1")?;
    for name in before.keys().chain(after.keys()) {
        let in_batch = after.get(name).copied().unwrap_or_default();
        if before.contains_key(name) == (in_batch > 0) {
            continue;
        }
        let total: i64 = count.query_row([name], |r| r.get(0))?;
        if total == in_batch {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The id of each Rust file of `store` at one of `paths`, by path.
fn rust_file_ids(
    store: &GraphStore,
    paths: &BTreeSet<String>,
) -> Result<HashMap<String, i64>, StoreError> {
    let mut stmt = store
        .conn()
        .prepare_cached("SELECT id FROM files WHERE path = ?1 AND lang = 'rust'")?;
    let mut out = HashMap::new();
    for path in paths {
        let rows = stmt.query_map([path], |r| r.get::<_, i64>(0))?;
        for id in rows {
            out.insert(path.clone(), id?);
        }
    }
    Ok(out)
}

/// How long a refresh waits for another process writing pixel-mvp's index
/// (pixel-mvp's own reader waits as long): past it the index is taken as
/// stuck and the refresh fails.
const WAIT: Duration = Duration::from_secs(60);

/// What pixel-mvp's index decides at the Rust sites of a graph, in the
/// graph's own rows ([`rust_rows`]).
#[derive(Debug, Default)]
pub struct RustRows {
    /// `calls` edges, all `exact`.
    pub calls: Vec<EdgeRow>,
    /// `references` edges, all `probable`.
    pub references: Vec<EdgeRow>,
    /// Sites pixel-mvp left unresolved, as `unresolved_calls` rows.
    pub unresolved: Vec<Unresolved>,
    /// Every Rust `imports` row's id, with the file pixel-mvp resolved it to.
    pub imports: Vec<(i64, Option<i64>)>,
    /// Per enclosing symbol, the unresolved calls to names no symbol
    /// carries: counted (`symbols.unnamed_calls`), not stored per site.
    pub unnamed: HashMap<i64, i64>,
    /// What had no counterpart in the graph and was left out.
    pub dropped: Dropped,
}

/// One `unresolved_calls` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    pub file_id: i64,
    pub name: String,
    pub enclosing: Option<i64>,
    pub site_line: u32,
    pub receiver: Option<String>,
    /// `"calls"` or `"references"`.
    pub kind: &'static str,
}

/// Rows of pixel-mvp's index the graph has no place for.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Dropped {
    /// Resolved sites whose enclosing symbol the graph does not have (test
    /// code), or that sit at a file's top level.
    pub no_caller: usize,
    /// Resolved sites whose target the graph does not have (a test helper,
    /// a type alias, a macro).
    pub no_target: usize,
}

/// Brings pixel-mvp's index of the work tree around `repo` up to date and
/// opens it. The index lives in `state/.pixel-mvp/` (`state` is created if
/// missing); a repeated refresh re-reads only the files that changed. Fails
/// unless `repo` is the top level of a git work tree: pixel-mvp lists files
/// through git, and its paths are relative to that top level, which must be
/// this graph's root for the two to name the same files.
///
/// The repository's `.pixel/` is checked first (`pixel_git::sidecar::check`):
/// one that is a link or that git tracks is refused, as the other stores
/// refuse it, before anything under it is read.
pub fn refresh(repo: &Path, state: &Path) -> Result<mvp_store::Store, BoxErr> {
    refresh_changes(repo, state).map(|(store, _)| store)
}

/// What one refresh of pixel-mvp's index changed, for a reader that
/// follows it file by file (`ruby_switch::merge_again`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// The generation the refresh started from (`Provenance::generation`
    /// of the index before it); `None` before the first one.
    pub before: Option<String>,
    /// The paths whose rows the refresh dropped or wrote again: what each
    /// defines may have moved.
    pub rewritten: BTreeSet<String>,
    /// The paths holding a kept site whose target the refresh moved.
    pub redirected: BTreeSet<String>,
}

/// [`refresh`], with what the refresh changed.
pub fn refresh_changes(repo: &Path, state: &Path) -> Result<(mvp_store::Store, Changes), BoxErr> {
    pixel_git::sidecar::check(repo)?;
    let root = mvp_index::root(repo)?;
    if root.canonicalize()? != repo.canonicalize()? {
        return Err(format!(
            "{} is inside the git work tree {}, not its top level",
            repo.display(),
            root.display()
        )
        .into());
    }
    std::fs::create_dir_all(state)?;
    let mut writer = mvp_store::Writer::open(state, Some(WAIT))?;
    settle(&root, state);
    // The stat the last generation kept lets the scan reuse its own hash for
    // every file whose metadata did not move (ADR-0022 §2); without it the
    // scan read and hashed every tracked file on every watcher batch.
    let known = writer.known()?;
    let tree = mvp_index::scan_discovered_in(
        mvp_index::discover(&root)?,
        &|path| known.get(path).copied(),
        state,
    )?;
    let changes = writer.retried(|writer| -> Result<Changes, BoxErr> {
        let indexed = writer.indexed()?;
        // The id the published generation carries: its files, extractor
        // and stale list (`link::generation_id`).
        let before = indexed.extractor.as_deref().map(|extractor| {
            mvp_index::link::generation_id(&indexed.files, extractor, &indexed.stale)
        });
        let plan = mvp_index::plan(
            &tree,
            &indexed.files,
            indexed.extractor.as_deref(),
            &indexed.stale,
        );
        let extracted = mvp_index::extract(&tree.root, &plan.extract, None);
        let delta = mvp_index::link(&tree, &plan, extracted, |drop| writer.kept(drop))?;
        writer.publish(&delta)?;
        let mut rewritten: BTreeSet<String> = delta.drop.iter().cloned().collect();
        rewritten.extend(delta.add.iter().map(|rows| rows.file.path.clone()));
        let redirected = delta
            .redirect
            .iter()
            .map(|(edge, _)| uid_file(&edge.src))
            .collect();
        Ok(Changes {
            before,
            rewritten,
            redirected,
        })
    })?;
    drop(writer);
    Ok((mvp_store::Store::open(state)?, changes))
}

/// Makes or refreshes again pixel-mvp's copy of git's index (ADR-0039 of the
/// pixel-mvp research repository) for the version of git's index there is
/// now, under `state` (`state/.pixel-mvp/git-index.*`, beside the index it
/// gives `Writer::open`), so that the scan reads its stat cache instead of
/// git's: on a copied or freshly cloned checkout, git's own is stale and
/// every file is hashed again. Run with the writer's lock held, as
/// pixel-mvp's own `refresh` does. git's index is never written; a copy
/// that cannot be made costs only git's stale stat cache.
fn settle(root: &Path, state: &Path) {
    use mvp_index::git_index;
    let Ok(repo) = mvp_index::discover(root) else {
        return;
    };
    let Some(copy) = repo
        .index()
        .and_then(|source| git_index::Copy::of(state, source))
    else {
        return;
    };
    let Some(bytes) = copy.bytes() else {
        return;
    };
    let Some(name) = copy.path().file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let _ = mvp_store::place_git_index(
        state,
        name,
        &bytes,
        |file| git_index::refresh(repo.root(), file),
        || copy.holds(),
        |other| other.starts_with(git_index::PREFIX),
    );
}

/// [`refresh_crate`], with what the refresh changed.
pub fn refresh_crate_changes(
    root: &Path,
    state: &Path,
) -> Result<(mvp_store::Store, Changes), BoxErr> {
    if !crate_root(root) {
        return Err(format!(
            "{} has no Cargo.toml: pixel-mvp resolves Rust paths from a crate root",
            root.display()
        )
        .into());
    }
    refresh_changes(root, state)
}

/// One symbol, as the pairing reads it from either graph.
#[derive(Debug, Clone)]
struct Sym<K> {
    key: K,
    path: String,
    /// The qualified name: `Store::len`.
    name: String,
    /// The kind, in the vocabulary both graphs share ([`common_kind`]).
    kind: String,
    line: u32,
}

/// A kind as both graphs spell it: pixel-mvp's `static` is a `const` here,
/// and this graph's `script` is pixel-mvp's `file`.
fn common_kind(kind: &str) -> &str {
    match kind {
        "static" => "const",
        "script" => "file",
        other => other,
    }
}

/// The last segment of a qualified name (`Store::len` gives `len`).
fn leaf(name: &str) -> &str {
    mvp_model::leaf(name)
}

/// Pairs each symbol of this graph (`ours`) with one of pixel-mvp's
/// (`theirs`), returning pixel-mvp's key to ours. Three passes, each only
/// over what the previous left unpaired:
///
/// 1. path, qualified name and kind: equal counts pair in line order,
///    otherwise each of ours takes the nearest line still free;
/// 2. path and qualified name, when each side has exactly one;
/// 3. path, last segment and line, when pixel-mvp has exactly one.
fn pair<A: Clone + Eq + std::hash::Hash, B: Clone + Eq + std::hash::Hash>(
    ours: &[Sym<A>],
    theirs: &[Sym<B>],
) -> HashMap<B, A> {
    let mut paired: HashMap<B, A> = HashMap::new();
    let mut ours_done: HashSet<A> = HashSet::new();
    let mut by_full: HashMap<(&str, &str, &str), Vec<&Sym<B>>> = HashMap::new();
    for t in theirs {
        by_full
            .entry((&t.path, &t.name, &t.kind))
            .or_default()
            .push(t);
    }
    let mut ours_full: HashMap<(&str, &str, &str), Vec<&Sym<A>>> = HashMap::new();
    for o in ours {
        ours_full
            .entry((&o.path, &o.name, &o.kind))
            .or_default()
            .push(o);
    }
    for (key, mut mine) in ours_full {
        let Some(mut candidates) = by_full.get(&key).cloned() else {
            continue;
        };
        mine.sort_by_key(|o| o.line);
        candidates.sort_by_key(|t| t.line);
        if mine.len() == candidates.len() {
            for (o, t) in mine.iter().zip(&candidates) {
                paired.insert(t.key.clone(), o.key.clone());
                ours_done.insert(o.key.clone());
            }
            continue;
        }
        for o in mine {
            let nearest = candidates
                .iter()
                .enumerate()
                .min_by_key(|(_, t)| t.line.abs_diff(o.line))
                .map(|(i, _)| i);
            if let Some(i) = nearest {
                let t = candidates.remove(i);
                paired.insert(t.key.clone(), o.key.clone());
                ours_done.insert(o.key.clone());
            }
        }
    }
    let free = |paired: &HashMap<B, A>| -> Vec<&Sym<B>> {
        theirs
            .iter()
            .filter(|t| !paired.contains_key(&t.key))
            .collect()
    };
    let mut theirs_named: HashMap<(&str, &str), Vec<&Sym<B>>> = HashMap::new();
    for t in free(&paired) {
        theirs_named.entry((&t.path, &t.name)).or_default().push(t);
    }
    let mut ours_named: HashMap<(&str, &str), Vec<&Sym<A>>> = HashMap::new();
    for o in ours.iter().filter(|o| !ours_done.contains(&o.key)) {
        ours_named.entry((&o.path, &o.name)).or_default().push(o);
    }
    for (key, mine) in &ours_named {
        if let ([o], Some([t])) = (mine.as_slice(), theirs_named.get(key).map(Vec::as_slice)) {
            paired.insert(t.key.clone(), o.key.clone());
            ours_done.insert(o.key.clone());
        }
    }
    let mut theirs_at: HashMap<(&str, &str, u32), Vec<&Sym<B>>> = HashMap::new();
    for t in free(&paired) {
        theirs_at
            .entry((&t.path, leaf(&t.name), t.line))
            .or_default()
            .push(t);
    }
    for o in ours.iter().filter(|o| !ours_done.contains(&o.key)) {
        if let Some([t]) = theirs_at
            .get(&(o.path.as_str(), leaf(&o.name), o.line))
            .map(Vec::as_slice)
        {
            paired.insert(t.key.clone(), o.key.clone());
        }
    }
    paired
}

/// The receiver and the name of a site as pixel-mvp writes its target
/// (`x.as_os_str().len`, `crate::store::open`), in this graph's spelling: a
/// method call's receiver starts with `.` (`.x.as_os_str()`), a path's is
/// the path (`crate::store`), a bare name has none. The cut is the last `.`
/// or `::` outside brackets.
fn split_target(target: &str) -> (Option<String>, String) {
    let mut depth = 0usize;
    let mut cut: Option<(usize, usize, bool)> = None;
    let bytes = target.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' | b'[' | b'<' | b'{' => depth += 1,
            b')' | b']' | b'>' | b'}' => depth = depth.saturating_sub(1),
            b'.' if depth == 0 => cut = Some((i, i + 1, true)),
            b':' if depth == 0 && bytes.get(i + 1) == Some(&b':') => {
                cut = Some((i, i + 2, false));
            }
            _ => {}
        }
    }
    match cut {
        Some((end, start, method)) if start < target.len() && end > 0 => {
            let receiver = &target[..end];
            let receiver = if method {
                format!(".{receiver}")
            } else {
                receiver.to_string()
            };
            (Some(receiver), target[start..].to_string())
        }
        _ => (None, target.to_string()),
    }
}

/// The path a pixel-mvp uid names: what precedes its first `#`, with `%23`
/// and `%25` read back (`mvp_model::uid_path`).
fn uid_file(uid: &str) -> String {
    let path = uid.split('#').next().unwrap_or(uid);
    path.replace("%23", "#").replace("%25", "%")
}

/// What pixel-mvp's index (`mvp`) decides at the Rust sites of `store`, in
/// `store`'s rows. Reads both, writes neither.
pub fn rust_rows(store: &GraphStore, mvp: &mvp_store::Store) -> Result<RustRows, BoxErr> {
    rust_rows_in(store, mvp, Scope::All)
}

/// [`rust_rows`] for the sites of the Rust files of `scope`: pixel-mvp's
/// edges of those files, paired over their paths and their targets' only
/// (each pass of the pairing matches within one path), and their imports.
pub fn rust_rows_in(
    store: &GraphStore,
    mvp: &mvp_store::Store,
    scope: Scope<'_>,
) -> Result<RustRows, BoxErr> {
    let files: HashMap<String, i64> = match scope {
        Scope::All => store
            .files()?
            .into_iter()
            .filter(|f| f.lang == "rust")
            .map(|f| (f.path, f.id))
            .collect(),
        Scope::Files(paths) => rust_file_ids(store, paths)?,
    };
    let (edges, ours, theirs) = match scope {
        Scope::All => (mvp.all_edges()?, rust_symbols(store, None)?, {
            let mut theirs = Vec::new();
            for file in mvp.files()?.into_iter().filter(|f| f.lang == Lang::Rust) {
                theirs.extend(mvp_symbols(mvp, &file.path)?);
            }
            theirs
        }),
        Scope::Files(_) => {
            let mut sited: Vec<&String> = files.keys().collect();
            sited.sort();
            let mut edges = Vec::new();
            for path in sited {
                edges.extend(mvp.edges_of(path)?);
            }
            let mut paths: BTreeSet<String> = files.keys().cloned().collect();
            paths.extend(edges.iter().filter_map(|e| e.dst.as_deref().map(uid_file)));
            let (mut ours, mut theirs) = (Vec::new(), Vec::new());
            for path in &paths {
                ours.extend(rust_symbols(store, Some(path))?);
                theirs.extend(mvp_symbols(mvp, path)?);
            }
            (edges, ours, theirs)
        }
    };
    let paired = pair(&ours, &theirs);
    // A name some symbol of the graph carries: an unresolved reference to
    // anything else is a plain value, which this graph never records.
    let mut named = Named::of(store, scope)?;
    let all_files: HashMap<String, i64> =
        store.files()?.into_iter().map(|f| (f.path, f.id)).collect();

    let mut out = RustRows::default();
    // (importing file id, path as written) -> the files it resolved to.
    let mut imported: HashMap<(i64, String), HashSet<Option<i64>>> = HashMap::new();
    for edge in edges {
        let path = uid_file(&edge.src);
        let Some(&file_id) = files.get(&path) else {
            continue;
        };
        let kind = match edge.kind {
            mvp_model::EdgeKind::Call => EdgeKind::Calls,
            mvp_model::EdgeKind::Reference => EdgeKind::References,
            mvp_model::EdgeKind::Import => {
                let target = edge
                    .dst
                    .as_deref()
                    .and_then(|dst| all_files.get(&uid_file(dst)));
                imported
                    .entry((file_id, edge.target.clone()))
                    .or_default()
                    .insert(target.copied());
                continue;
            }
            mvp_model::EdgeKind::Route | mvp_model::EdgeKind::Association => continue,
        };
        let src = paired.get(&edge.src).copied();
        let (receiver, callee) = split_target(&edge.target);
        let callee = leaf(&callee).to_string();
        let Some(dst_uid) = edge.dst.as_deref() else {
            // A site at a file's top level hangs on no symbol, as here; one
            // in code this graph has no symbol for (a test) is none of its
            // sites; a site whose name no symbol carries (`std`'s methods,
            // macros, other crates, a reference to a value) can stand for no
            // symbol of the graph, so no `impact` or `uses` count reads it.
            // When a batch makes such a name defined, `merge_again` writes
            // every file again, which records it then.
            let in_graph = src.is_some() || is_file_symbol(&edge.src);
            let carried = named.carried(store, &callee)?;
            if let (Some(symbol), EdgeKind::Calls, false) = (src, kind, carried) {
                *out.unnamed.entry(symbol).or_default() += 1;
            }
            if in_graph && carried {
                out.unresolved.push(Unresolved {
                    file_id,
                    name: callee,
                    enclosing: src,
                    site_line: edge.line,
                    receiver,
                    kind: unresolved_kind(kind),
                });
            }
            continue;
        };
        let Some(src_id) = src else {
            out.dropped.no_caller += 1;
            continue;
        };
        let Some(&dst_id) = paired.get(dst_uid) else {
            out.dropped.no_target += 1;
            continue;
        };
        let row = EdgeRow {
            src_id,
            dst_id,
            kind,
            tier: if kind == EdgeKind::Calls {
                Tier::Exact
            } else {
                Tier::Probable
            },
            site_line: edge.line,
            receiver: if kind == EdgeKind::Calls {
                receiver
            } else {
                None
            },
            callee: Some(callee),
        };
        if kind == EdgeKind::Calls {
            out.calls.push(row);
        } else {
            out.references.push(row);
        }
    }

    let mut stmt = store.conn().prepare_cached(
        "SELECT i.id, i.file_id, i.path FROM imports i
           JOIN files f ON f.id = i.file_id
          WHERE f.lang = 'rust' AND (?1 IS NULL OR f.id = ?1) ORDER BY i.id",
    )?;
    let ids: Vec<Option<i64>> = match scope {
        Scope::All => vec![None],
        Scope::Files(_) => {
            let mut ids: Vec<i64> = files.values().copied().collect();
            ids.sort_unstable();
            ids.into_iter().map(Some).collect()
        }
    };
    for id in ids {
        let rows = stmt.query_map([id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (id, file_id, path) = row?;
            // Two `use` of one path that pixel-mvp resolves apart (in two
            // scopes) give no single answer for the row: none.
            let target = match imported.get(&(file_id, path)) {
                Some(targets) if targets.len() == 1 => targets.iter().next().copied().flatten(),
                _ => None,
            };
            out.imports.push((id, target));
        }
    }
    Ok(out)
}

/// The Rust symbols of `store`, of the file at `path` when one is given, as
/// the pairing reads them.
fn rust_symbols(store: &GraphStore, path: Option<&str>) -> Result<Vec<Sym<i64>>, StoreError> {
    let mut stmt = store.conn().prepare_cached(
        "SELECT s.id, f.path, s.qualified, s.kind, s.start_line FROM symbols s
           JOIN files f ON f.id = s.file_id
          WHERE f.lang = 'rust' AND (?1 IS NULL OR f.path = ?1) ORDER BY s.id",
    )?;
    let rows = stmt.query_map([path], |r| {
        Ok(Sym {
            key: r.get::<_, i64>(0)?,
            path: r.get(1)?,
            name: r.get(2)?,
            kind: common_kind(&r.get::<_, String>(3)?).to_string(),
            line: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// pixel-mvp's symbols of the file at `path`, as the pairing reads them.
fn mvp_symbols(mvp: &mvp_store::Store, path: &str) -> Result<Vec<Sym<String>>, BoxErr> {
    Ok(mvp
        .symbols(path)?
        .into_iter()
        .map(|symbol| Sym {
            kind: common_kind(symbol.kind.as_str()).to_string(),
            key: symbol.uid,
            path: symbol.file,
            name: symbol.name,
            line: symbol.line,
        })
        .collect())
}

/// Whether some symbol of the graph carries a name: every name read at once
/// for every Rust file, one indexed lookup per name met for a few.
enum Named {
    All(HashSet<String>),
    Lookup(HashMap<String, bool>),
}

impl Named {
    fn of(store: &GraphStore, scope: Scope<'_>) -> Result<Self, StoreError> {
        Ok(match scope {
            Scope::All => {
                let mut stmt = store.conn().prepare("SELECT DISTINCT name FROM symbols")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                Named::All(rows.collect::<Result<_, _>>()?)
            }
            Scope::Files(_) => Named::Lookup(HashMap::new()),
        })
    }

    fn carried(&mut self, store: &GraphStore, name: &str) -> Result<bool, StoreError> {
        match self {
            Named::All(names) => Ok(names.contains(name)),
            Named::Lookup(seen) => {
                if let Some(&carried) = seen.get(name) {
                    return Ok(carried);
                }
                let carried: bool = store.conn().query_row(
                    "SELECT EXISTS (SELECT 1 FROM symbols WHERE name = ?1)",
                    [name],
                    |r| r.get(0),
                )?;
                seen.insert(name.to_string(), carried);
                Ok(carried)
            }
        }
    }
}

/// Whether a pixel-mvp uid is a file's own symbol (`path#path#file`): the
/// enclosing symbol of a site at a file's top level.
fn is_file_symbol(uid: &str) -> bool {
    uid.ends_with("#file")
}

fn unresolved_kind(kind: EdgeKind) -> &'static str {
    if kind == EdgeKind::References {
        "references"
    } else {
        "calls"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym<K>(key: K, path: &str, name: &str, kind: &str, line: u32) -> Sym<K> {
        Sym {
            key,
            path: path.to_string(),
            name: name.to_string(),
            kind: common_kind(kind).to_string(),
            line,
        }
    }

    #[test]
    fn the_origin_commit_matches_the_provenance_record() {
        let vendored = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../mvp-index/ORIGIN.md"),
        )
        .unwrap();
        assert!(
            vendored.contains(&format!("(`{MVP_COMMIT}`)")),
            "MVP_COMMIT is {MVP_COMMIT}, ORIGIN.md names another commit"
        );
    }

    #[test]
    fn a_site_target_splits_into_this_graphs_receiver_and_name() {
        let split = |t: &str| split_target(t);
        assert_eq!(split("open"), (None, "open".to_string()));
        assert_eq!(
            split("crate::store::open"),
            (Some("crate::store".to_string()), "open".to_string())
        );
        assert_eq!(
            split("p.join"),
            (Some(".p".to_string()), "join".to_string())
        );
        assert_eq!(
            split("x.as_os_str().len"),
            (Some(".x.as_os_str()".to_string()), "len".to_string())
        );
        // A separator inside brackets is not the cut.
        assert_eq!(
            split("f(a.b, c::d).g"),
            (Some(".f(a.b, c::d)".to_string()), "g".to_string())
        );
        assert_eq!(
            split("Vec::<u8>::new"),
            (Some("Vec::<u8>".to_string()), "new".to_string())
        );
        // A trailing or leading separator leaves the target whole.
        assert_eq!(split("x."), (None, "x.".to_string()));
        assert_eq!(split(".x"), (None, ".x".to_string()));
    }

    #[test]
    fn a_uid_names_its_file_with_the_escapes_read_back() {
        assert_eq!(uid_file("src/a.rs#f#function"), "src/a.rs");
        assert_eq!(uid_file("src/%23x%25.rs#f#function"), "src/#x%.rs");
        assert!(is_file_symbol("src/a.rs#src/a.rs#file"));
        assert!(!is_file_symbol("src/a.rs#f#function"));
    }

    #[test]
    fn kinds_are_compared_in_the_vocabulary_both_graphs_share() {
        assert_eq!(common_kind("static"), "const");
        assert_eq!(common_kind("script"), "file");
        assert_eq!(common_kind("method"), "method");
        assert_eq!(unresolved_kind(EdgeKind::References), "references");
        assert_eq!(unresolved_kind(EdgeKind::Calls), "calls");
    }

    #[test]
    fn symbols_pair_by_name_and_kind_then_name_then_line() {
        let ours = vec![
            sym(1, "a.rs", "S::f", "method", 3),
            // Kind differs from pixel-mvp's: paired by name alone.
            sym(2, "a.rs", "LIMIT", "const", 9),
            // Qualified apart, same last segment and line.
            sym(3, "a.rs", "inner::g", "function", 20),
            // Nothing on the other side.
            sym(4, "a.rs", "lonely", "function", 30),
        ];
        let theirs = vec![
            sym("m-f", "a.rs", "S::f", "method", 3),
            sym("m-limit", "a.rs", "LIMIT", "static", 9),
            sym("m-g", "a.rs", "g", "function", 20),
            sym("m-test", "a.rs", "tests::t", "function", 40),
        ];
        let paired = pair(&ours, &theirs);
        let expected: HashMap<&str, i64> = [("m-f", 1), ("m-limit", 2), ("m-g", 3)].into();
        assert_eq!(paired, expected);
    }

    #[test]
    fn repeated_names_pair_by_nearest_line() {
        // Two `impl` blocks each define `new`; pixel-mvp holds a third.
        let ours = vec![
            sym(1, "a.rs", "S::new", "method", 10),
            sym(2, "a.rs", "S::new", "method", 50),
        ];
        let theirs = vec![
            sym("first", "a.rs", "S::new", "method", 11),
            sym("test", "a.rs", "S::new", "method", 90),
            sym("second", "a.rs", "S::new", "method", 49),
        ];
        let paired = pair(&ours, &theirs);
        let expected: HashMap<&str, i64> = [("first", 1), ("second", 2)].into();
        assert_eq!(paired, expected);
        // Equal counts pair in line order.
        let theirs = vec![
            sym("b", "a.rs", "S::new", "method", 52),
            sym("a", "a.rs", "S::new", "method", 9),
        ];
        let expected: HashMap<&str, i64> = [("a", 1), ("b", 2)].into();
        assert_eq!(pair(&ours, &theirs), expected);
    }

    #[test]
    fn a_name_two_symbols_share_is_not_paired_by_name_alone() {
        let ours = vec![
            sym(1, "a.rs", "f", "function", 1),
            sym(2, "a.rs", "f", "function", 5),
        ];
        let theirs = vec![sym("m", "a.rs", "f", "macro", 3)];
        assert!(pair(&ours, &theirs).is_empty());
    }
}
