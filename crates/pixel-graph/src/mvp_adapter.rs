// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! pixel-mvp's index in front of this graph, for Rust (ADR-0035 of the
//! pixel-mvp research repository; `crates/mvp-index/VENDORED.md`).
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
//!   `lower_bound` of `impact` and `uses` still counts it;
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

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use mvp_model::{Graph, Lang};

use crate::store::{EdgeKind, EdgeRow, ExecCached, GraphStore, StoreError, Tier};

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

/// The pixel-mvp commit the `mvp-*` crates were copied from. A test holds it
/// equal to the one `crates/mvp-index/VENDORED.md` names, so a sync that
/// forgets it fails.
pub const MVP_COMMIT: &str = "4b23d4c";

/// Meta key: where the graph's Rust edges come from. Absent when they are
/// this graph's own resolver's, so a graph built with the switch off is
/// the graph built before the switch existed.
pub const GRAPH_SOURCE_KEY: &str = "graph_source";

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
/// pixel-mvp's: the vendored commit and its extractor versions, so that a
/// sync that changes either makes every such graph stale.
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
pub(crate) fn record_source(store: &GraphStore, source: Option<&str>) -> Result<(), StoreError> {
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

/// Replaces the Rust rows of `store` with `rows`: every `calls` and
/// `references` edge from a Rust symbol, every `unresolved_calls` row of a
/// Rust file, and the target of every Rust import.
pub fn apply(store: &GraphStore, rows: &RustRows) -> Result<(), StoreError> {
    let conn = store.conn();
    conn.exec_cached(
        "DELETE FROM edges WHERE kind IN ('calls', 'references') AND src_id IN
           (SELECT s.id FROM symbols s JOIN files f ON f.id = s.file_id WHERE f.lang = 'rust')",
        [],
    )?;
    conn.exec_cached(
        "DELETE FROM unresolved_calls WHERE file_id IN (SELECT id FROM files WHERE lang = 'rust')",
        [],
    )?;
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
/// answer ([`apply`]).
pub fn switch_rust(root: &Path, store: &GraphStore, state: &Path) -> Result<(), BoxErr> {
    let mvp = refresh_crate(root, state)?;
    let rows = rust_rows(store, &mvp)?;
    apply(store, &rows)?;
    Ok(())
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
    let tree = mvp_index::scan(&root)?;
    writer.retried(|writer| -> Result<(), BoxErr> {
        let indexed = writer.indexed()?;
        let plan = mvp_index::plan(
            &tree,
            &indexed.files,
            indexed.extractor.as_deref(),
            &indexed.stale,
        );
        let extracted = mvp_index::extract(&tree.root, &plan.extract, None);
        let delta = mvp_index::link(&tree, &plan, extracted, |drop| writer.kept(drop))?;
        writer.publish(&delta)?;
        Ok(())
    })?;
    drop(writer);
    Ok(mvp_store::Store::open(state)?)
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
    let files: HashMap<String, i64> = store
        .files()?
        .into_iter()
        .filter(|f| f.lang == "rust")
        .map(|f| (f.path, f.id))
        .collect();
    let ours: Vec<Sym<i64>> = {
        let mut stmt = store.conn().prepare(
            "SELECT s.id, f.path, s.qualified, s.kind, s.start_line FROM symbols s
               JOIN files f ON f.id = s.file_id WHERE f.lang = 'rust'",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Sym {
                key: r.get::<_, i64>(0)?,
                path: r.get(1)?,
                name: r.get(2)?,
                kind: common_kind(&r.get::<_, String>(3)?).to_string(),
                line: r.get(4)?,
            })
        })?;
        rows.collect::<Result<_, _>>()?
    };
    let mut theirs: Vec<Sym<String>> = Vec::new();
    for file in mvp.files()?.into_iter().filter(|f| f.lang == Lang::Rust) {
        for symbol in mvp.symbols(&file.path)? {
            theirs.push(Sym {
                kind: common_kind(symbol.kind.as_str()).to_string(),
                key: symbol.uid,
                path: symbol.file,
                name: symbol.name,
                line: symbol.line,
            });
        }
    }
    let paired = pair(&ours, &theirs);
    // A name some symbol of the graph carries: an unresolved reference to
    // anything else is a plain value, which this graph never records.
    let named: HashSet<String> = {
        let mut stmt = store.conn().prepare("SELECT DISTINCT name FROM symbols")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<_, _>>()?
    };
    let all_files: HashMap<String, i64> =
        store.files()?.into_iter().map(|f| (f.path, f.id)).collect();

    let mut out = RustRows::default();
    // (importing file id, path as written) -> the files it resolved to.
    let mut imported: HashMap<(i64, String), HashSet<Option<i64>>> = HashMap::new();
    for edge in mvp.all_edges()? {
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
            // sites; a reference to a name no symbol carries is a value.
            let in_graph = src.is_some() || is_file_symbol(&edge.src);
            if in_graph && (kind == EdgeKind::Calls || named.contains(&callee)) {
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

    let mut stmt = store.conn().prepare(
        "SELECT i.id, i.file_id, i.path FROM imports i
           JOIN files f ON f.id = i.file_id WHERE f.lang = 'rust' ORDER BY i.id",
    )?;
    let rows = stmt.query_map([], |r| {
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
    Ok(out)
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
    fn the_commit_is_the_one_vendored_md_names() {
        let vendored = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../mvp-index/VENDORED.md"),
        )
        .unwrap();
        assert!(
            vendored.contains(&format!("(`{MVP_COMMIT}`)")),
            "MVP_COMMIT is {MVP_COMMIT}, VENDORED.md names another commit"
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
