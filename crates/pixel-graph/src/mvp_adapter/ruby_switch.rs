// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The Ruby source switch, `PIXEL_GRAPH_SOURCE_RUBY` ([`RubySource`]):
//! `hybrid` writes the merge of [`super::ruby`] into the graph, anything
//! else leaves this graph's own Ruby rows, byte for byte.
//!
//! The merge is written over this graph's own rows, and an incremental
//! update needs those rows back: a site pixel-mvp resolved at the last
//! build and abstains on now must get this graph's own edge again, not the
//! one pixel-mvp no longer proves. So [`apply`] records what it removes and
//! adds in two side tables, and [`restore`] undoes it, before the update's
//! own passes run on the graph this resolver alone would have written; the
//! merge is then planned again on the result. Both tables exist only in a
//! hybrid graph, and a full build drops them ([`forget`]).
//!
//! An update merges again only the files whose merge may have moved
//! ([`merge_again`]): the merge is decided file by file, so a file whose
//! rows did not move on either side, and none of whose sites resolves
//! into a file whose definitions did, gets back the rows it had. On
//! antiwork/gumroad that is one plan of a few files instead of one of
//! about 970 000 sites.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::collections::BTreeSet;
use std::path::Path;

use mvp_model::Graph;
use rusqlite::params;

use super::ruby::{RubyPlan, Scope, ruby_plan, ruby_plan_in};
use super::{BoxErr, Changes, MVP_COMMIT, refresh};
use crate::mvp_index;
use crate::store::{EdgeKind, EdgeRow, ExecCached, GraphStore, StoreError, Tier};

/// The switch.
pub const RUBY_SOURCE_ENV: &str = "PIXEL_GRAPH_SOURCE_RUBY";

/// Meta key: where the graph's Ruby edges come from. Absent when they are
/// this graph's resolver's alone, so a graph built with the switch off is
/// the graph built before the switch existed.
pub const RUBY_SOURCE_KEY: &str = "graph_source_ruby";

/// Meta key of a merged graph: the generation of pixel-mvp's index the
/// merge was planned against (`Provenance::generation`). An update merges
/// again file by file only from that generation; from any other (the
/// index refreshed by a run whose graph update did not commit), it
/// merges every file again.
pub const RUBY_GENERATION_KEY: &str = "ruby_hybrid_generation";

/// The [`RUBY_SOURCE_KEY`] value of a graph the switch asked to merge where
/// pixel-mvp could not index it (the root is not the top level of a git
/// work tree): its Ruby rows are this resolver's. Current while the switch
/// is on; the next full build tries again.
pub const RUBY_MVP_UNAVAILABLE: &str = "ruby=v1, pixel-mvp unavailable";

/// Where a graph takes its Ruby edges from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RubySource {
    /// This graph's own resolver.
    V1,
    /// pixel-mvp's target where it proves a site, this graph's edge
    /// elsewhere ([`super::ruby`]).
    Hybrid,
}

impl RubySource {
    /// [`RUBY_SOURCE_ENV`] as the process sees it ([`RubySource::parse`]).
    pub fn from_env() -> Self {
        Self::parse(std::env::var(RUBY_SOURCE_ENV).ok().as_deref())
    }

    /// `hybrid` is [`RubySource::Hybrid`]; anything else, or nothing, is
    /// [`RubySource::V1`].
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("hybrid") => RubySource::Hybrid,
            _ => RubySource::V1,
        }
    }
}

/// The [`RUBY_SOURCE_KEY`] value of a merged graph: the vendored commit and
/// its extractor versions, so a sync that changes either makes the graph
/// stale.
pub fn hybrid_source() -> String {
    format!(
        "ruby=hybrid {MVP_COMMIT} {} {UNDO_FORMAT}",
        mvp_index::extractor()
    )
}

/// The layout of the side tables, in [`hybrid_source`]: a graph whose
/// side tables another layout wrote is rebuilt, not updated.
const UNDO_FORMAT: &str = "undo.2";

/// Whether a graph that recorded `stored` under [`RUBY_SOURCE_KEY`] was
/// built from what `wanted` asks for.
pub fn source_is_current(stored: Option<&str>, wanted: RubySource) -> bool {
    match wanted {
        RubySource::V1 => stored.is_none(),
        RubySource::Hybrid => {
            stored == Some(RUBY_MVP_UNAVAILABLE) || stored.is_some_and(|s| s == hybrid_source())
        }
    }
}

/// Whether the graph's Ruby rows are the merge (its side tables hold the
/// undo of the last [`apply`]).
pub fn is_hybrid(store: &GraphStore) -> Result<bool, StoreError> {
    Ok(store
        .meta_get(RUBY_SOURCE_KEY)?
        .is_some_and(|value| value.starts_with("ruby=hybrid ")))
}

/// Records where the graph's Ruby edges come from: `None` removes the key.
pub(crate) fn record_source(store: &GraphStore, source: Option<&str>) -> Result<(), StoreError> {
    match source {
        Some(value) => store.meta_set(RUBY_SOURCE_KEY, value),
        None => {
            store
                .conn()
                .exec_cached("DELETE FROM meta WHERE key = ?1", params![RUBY_SOURCE_KEY])?;
            Ok(())
        }
    }
}

/// The side tables: `op` is `removed` (this graph's row the merge took
/// out) or `added` (the merge's own row); `file_id` is the file of the
/// row's site.
const TABLES: &str = "
CREATE TABLE IF NOT EXISTS ruby_hybrid_edges (
  op TEXT NOT NULL, src_id INTEGER NOT NULL, dst_id INTEGER NOT NULL, kind TEXT NOT NULL,
  tier TEXT NOT NULL, site_line INTEGER NOT NULL, receiver TEXT, callee TEXT,
  file_id INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS ruby_hybrid_unresolved (
  op TEXT NOT NULL, file_id INTEGER NOT NULL, name TEXT NOT NULL, enclosing_symbol_id INTEGER,
  site_line INTEGER NOT NULL, receiver TEXT, kind TEXT NOT NULL);";

fn has_tables(store: &GraphStore) -> Result<bool, StoreError> {
    let n: i64 = store.conn().query_row(
        "SELECT count(*) FROM sqlite_master WHERE name = 'ruby_hybrid_edges'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Drops the side tables and the source key: what a full build does before
/// it writes, so no undo outlives the rows it describes.
pub(crate) fn forget(store: &GraphStore) -> Result<(), StoreError> {
    store.conn().execute_batch(
        "DROP TABLE IF EXISTS ruby_hybrid_edges; DROP TABLE IF EXISTS ruby_hybrid_unresolved;",
    )?;
    store.conn().exec_cached(
        "DELETE FROM meta WHERE key = ?1",
        params![RUBY_GENERATION_KEY],
    )?;
    record_source(store, None)
}

/// Writes `plan` into `store` and records its undo.
pub fn apply(store: &GraphStore, plan: &RubyPlan) -> Result<(), StoreError> {
    let conn = store.conn();
    conn.execute_batch(TABLES)?;
    for (id, file) in plan.remove_edges.iter().zip(&plan.remove_edge_files) {
        conn.exec_cached(
            "INSERT INTO ruby_hybrid_edges
               SELECT 'removed', src_id, dst_id, kind, tier, site_line, receiver, callee, ?2
                 FROM edges WHERE id = ?1",
            params![id, file],
        )?;
        conn.exec_cached("DELETE FROM edges WHERE id = ?1", params![id])?;
    }
    for id in &plan.remove_unresolved {
        conn.exec_cached(
            "INSERT INTO ruby_hybrid_unresolved
               SELECT 'removed', file_id, name, enclosing_symbol_id, site_line, receiver, kind
                 FROM unresolved_calls WHERE id = ?1",
            params![id],
        )?;
        conn.exec_cached("DELETE FROM unresolved_calls WHERE id = ?1", params![id])?;
    }
    for (e, file) in plan.add_edges.iter().zip(&plan.add_edge_files) {
        store.insert_edge(e)?;
        conn.exec_cached(
            "INSERT INTO ruby_hybrid_edges VALUES ('added', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                e.src_id,
                e.dst_id,
                e.kind.as_str(),
                e.tier.as_str(),
                e.site_line,
                e.receiver,
                e.callee,
                file
            ],
        )?;
    }
    for u in &plan.add_unresolved {
        store.insert_unresolved_call(
            u.file_id,
            &u.name,
            u.enclosing,
            u.site_line,
            u.receiver.as_deref(),
            u.kind,
        )?;
        conn.exec_cached(
            "INSERT INTO ruby_hybrid_unresolved VALUES ('added', ?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                u.file_id,
                u.name,
                u.enclosing,
                u.site_line,
                u.receiver,
                u.kind
            ],
        )?;
    }
    Ok(())
}

/// One row the last [`apply`] recorded, with the path of the file whose
/// site it is.
#[derive(Debug, Clone)]
struct Recorded<R> {
    added: bool,
    /// The site's file, and its path: `None` when the graph holds no
    /// such file.
    file_id: i64,
    path: Option<String>,
    row: R,
}

/// What [`restore`] undid: the rows the last [`apply`] recorded, so that
/// [`merge_again`] can give a file whose merge did not move the rows it
/// had, without planning it again.
#[derive(Debug, Default)]
pub struct Undo {
    edges: Vec<Recorded<EdgeRow>>,
    unresolved: Vec<Recorded<super::Unresolved>>,
}

/// The rows the side tables hold, each with its file's path.
fn recorded(store: &GraphStore) -> Result<Undo, StoreError> {
    let conn = store.conn();
    let mut undo = Undo::default();
    let mut stmt = conn.prepare(
        "SELECT h.op, f.path, h.src_id, h.dst_id, h.kind, h.tier, h.site_line, h.receiver, h.callee,
                h.file_id
           FROM ruby_hybrid_edges h LEFT JOIN files f ON f.id = h.file_id ORDER BY h.rowid",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Recorded {
            added: r.get::<_, String>(0)? == "added",
            path: r.get(1)?,
            file_id: r.get(9)?,
            row: EdgeRow {
                src_id: r.get(2)?,
                dst_id: r.get(3)?,
                kind: EdgeKind::parse(&r.get::<_, String>(4)?),
                tier: Tier::parse(&r.get::<_, String>(5)?),
                site_line: r.get(6)?,
                receiver: r.get(7)?,
                callee: r.get(8)?,
            },
        })
    })?;
    for row in rows {
        undo.edges.push(row?);
    }
    let mut stmt = conn.prepare(
        "SELECT h.op, f.path, h.file_id, h.name, h.enclosing_symbol_id, h.site_line, h.receiver,
                h.kind
           FROM ruby_hybrid_unresolved h LEFT JOIN files f ON f.id = h.file_id ORDER BY h.rowid",
    )?;
    let rows = stmt.query_map([], |r| {
        let kind: String = r.get(7)?;
        Ok(Recorded {
            added: r.get::<_, String>(0)? == "added",
            path: r.get(1)?,
            file_id: r.get(2)?,
            row: super::Unresolved {
                file_id: r.get(2)?,
                name: r.get(3)?,
                enclosing: r.get(4)?,
                site_line: r.get(5)?,
                receiver: r.get(6)?,
                kind: if kind == "references" {
                    "references"
                } else {
                    "calls"
                },
            },
        })
    })?;
    for row in rows {
        undo.unresolved.push(row?);
    }
    Ok(undo)
}

/// Undoes the last [`apply`]: the merge's rows go, this graph's come back,
/// and the side tables are emptied. A graph with no side tables is left
/// alone. Returns what it undid.
pub fn restore(store: &GraphStore) -> Result<Undo, StoreError> {
    if !has_tables(store)? {
        return Ok(Undo::default());
    }
    let undo = recorded(store)?;
    let conn = store.conn();
    // One row per recorded row, the newest first: the merge's rows were
    // inserted after every row of this graph's.
    let added = |table: &str| -> Result<Vec<i64>, StoreError> {
        let mut stmt = conn.prepare(&format!("SELECT rowid FROM {table} WHERE op = 'added'"))?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    };
    for at in added("ruby_hybrid_edges")? {
        conn.exec_cached(
            "DELETE FROM edges WHERE id = (SELECT e.id FROM edges e, ruby_hybrid_edges h
               WHERE h.rowid = ?1 AND e.src_id = h.src_id AND e.dst_id = h.dst_id
                 AND e.kind = h.kind AND e.tier = h.tier AND e.site_line = h.site_line
                 AND e.receiver IS h.receiver AND e.callee IS h.callee
               ORDER BY e.id DESC LIMIT 1)",
            params![at],
        )?;
    }
    for at in added("ruby_hybrid_unresolved")? {
        conn.exec_cached(
            "DELETE FROM unresolved_calls WHERE id = (SELECT u.id FROM unresolved_calls u,
                    ruby_hybrid_unresolved h
               WHERE h.rowid = ?1 AND u.file_id = h.file_id AND u.name = h.name
                 AND u.enclosing_symbol_id IS h.enclosing_symbol_id
                 AND u.site_line = h.site_line AND u.receiver IS h.receiver AND u.kind = h.kind
               ORDER BY u.id DESC LIMIT 1)",
            params![at],
        )?;
    }
    conn.execute_batch(
        "INSERT INTO edges (src_id, dst_id, kind, tier, site_line, receiver, callee)
           SELECT src_id, dst_id, kind, tier, site_line, receiver, callee
             FROM ruby_hybrid_edges WHERE op = 'removed';
         INSERT INTO unresolved_calls (file_id, name, enclosing_symbol_id, site_line, receiver, kind)
           SELECT file_id, name, enclosing_symbol_id, site_line, receiver, kind
             FROM ruby_hybrid_unresolved WHERE op = 'removed';
         DELETE FROM ruby_hybrid_edges;
         DELETE FROM ruby_hybrid_unresolved;",
    )?;
    Ok(undo)
}

/// What this resolver's rows of each Ruby file hold, by file id: their
/// count and the sum of a 128-bit hash of each row's content, a multiset
/// hash that two reads of the same rows give alike whatever their ids and
/// order ([`moved`]).
pub type Prints = HashMap<i64, (u64, u128)>;

/// Hashes one row of `width` columns into `prints` under its file
/// (column 0).
fn print_row(
    row: &rusqlite::Row<'_>,
    width: usize,
    prints: &mut Prints,
    buf: &mut Vec<u8>,
) -> rusqlite::Result<()> {
    use rusqlite::types::ValueRef;
    buf.clear();
    for i in 1..width {
        match row.get_ref(i)? {
            ValueRef::Null => buf.push(0),
            ValueRef::Integer(n) => {
                buf.push(1);
                buf.extend_from_slice(&n.to_le_bytes());
            }
            ValueRef::Real(x) => {
                buf.push(2);
                buf.extend_from_slice(&x.to_le_bytes());
            }
            ValueRef::Text(t) | ValueRef::Blob(t) => {
                buf.push(3);
                buf.extend_from_slice(&(t.len() as u64).to_le_bytes());
                buf.extend_from_slice(t);
            }
        }
    }
    let file: i64 = row.get(0)?;
    let entry = prints.entry(file).or_insert((0, 0));
    entry.0 += 1;
    entry.1 = entry.1.wrapping_add(xxhash_rust::xxh3::xxh3_128(buf));
    Ok(())
}

/// The [`Prints`] of every Ruby file's `edges` (by the file of their
/// source symbol) and `unresolved_calls` rows: what the merge reads of
/// this resolver.
pub fn prints(store: &GraphStore) -> Result<Prints, StoreError> {
    let conn = store.conn();
    let mut prints = Prints::default();
    let mut buf = Vec::new();
    for sql in [
        "SELECT s.file_id, 'e', e.src_id, e.dst_id, e.kind, e.tier, e.site_line, e.receiver, e.callee
           FROM edges e JOIN symbols s ON s.id = e.src_id JOIN files f ON f.id = s.file_id
          WHERE f.lang = 'ruby'",
        "SELECT u.file_id, 'u', u.name, u.enclosing_symbol_id, u.site_line, u.receiver, u.kind
           FROM unresolved_calls u JOIN files f ON f.id = u.file_id WHERE f.lang = 'ruby'",
    ] {
        let mut stmt = conn.prepare(sql)?;
        let width = stmt.column_count();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            print_row(row, width, &mut prints, &mut buf)?;
        }
    }
    Ok(prints)
}

/// The paths of the Ruby files whose rows moved from `before` to the
/// graph as it stands. A file whose rows went and came back the same (an
/// edge into a file of the update demoted, then resolved again to the
/// same target) did not move: the merge reads rows, not their ids.
pub fn moved(store: &GraphStore, before: &Prints) -> Result<BTreeSet<String>, StoreError> {
    let after = prints(store)?;
    let files: HashMap<i64, String> = store.files()?.into_iter().map(|f| (f.id, f.path)).collect();
    let mut moved = BTreeSet::new();
    for (file, print) in &after {
        if before.get(file) != Some(print)
            && let Some(path) = files.get(file)
        {
            moved.insert(path.clone());
        }
    }
    for file in before.keys().filter(|file| !after.contains_key(file)) {
        if let Some(path) = files.get(file) {
            moved.insert(path.clone());
        }
    }
    Ok(moved)
}

/// Gives back the rows `undo` recorded for the files `keep` accepts: this
/// graph's rows the merge had taken out go again (one row of the same
/// content each), the merge's own come back, and both are recorded as
/// [`apply`] records them.
fn reapply(store: &GraphStore, undo: &Undo, keep: impl Fn(&str) -> bool) -> Result<(), StoreError> {
    let conn = store.conn();
    conn.execute_batch(TABLES)?;
    let kept = |path: &Option<String>| path.as_deref().is_some_and(&keep);
    for r in undo.edges.iter().filter(|r| kept(&r.path)) {
        let e = &r.row;
        if r.added {
            store.insert_edge(e)?;
        } else {
            conn.exec_cached(
                "DELETE FROM edges WHERE id = (SELECT id FROM edges
                   WHERE src_id = ?1 AND dst_id = ?2 AND kind = ?3 AND tier = ?4
                     AND site_line = ?5 AND receiver IS ?6 AND callee IS ?7
                   ORDER BY id DESC LIMIT 1)",
                params![
                    e.src_id,
                    e.dst_id,
                    e.kind.as_str(),
                    e.tier.as_str(),
                    e.site_line,
                    e.receiver,
                    e.callee
                ],
            )?;
        }
        conn.exec_cached(
            "INSERT INTO ruby_hybrid_edges VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                if r.added { "added" } else { "removed" },
                e.src_id,
                e.dst_id,
                e.kind.as_str(),
                e.tier.as_str(),
                e.site_line,
                e.receiver,
                e.callee,
                r.file_id
            ],
        )?;
    }
    for r in undo.unresolved.iter().filter(|r| kept(&r.path)) {
        let u = &r.row;
        if r.added {
            store.insert_unresolved_call(
                u.file_id,
                &u.name,
                u.enclosing,
                u.site_line,
                u.receiver.as_deref(),
                u.kind,
            )?;
        } else {
            conn.exec_cached(
                "DELETE FROM unresolved_calls WHERE id = (SELECT id FROM unresolved_calls
                   WHERE file_id = ?1 AND name = ?2 AND enclosing_symbol_id IS ?3
                     AND site_line = ?4 AND receiver IS ?5 AND kind = ?6
                   ORDER BY id DESC LIMIT 1)",
                params![
                    u.file_id,
                    u.name,
                    u.enclosing,
                    u.site_line,
                    u.receiver,
                    u.kind
                ],
            )?;
        }
        conn.exec_cached(
            "INSERT INTO ruby_hybrid_unresolved VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                if r.added { "added" } else { "removed" },
                u.file_id,
                u.name,
                u.enclosing,
                u.site_line,
                u.receiver,
                u.kind
            ],
        )?;
    }
    Ok(())
}

/// The switch at work on a graph whose rows are this resolver's: plans the
/// merge with pixel-mvp's index (`mvp`) and writes it.
pub fn merge_into(store: &GraphStore, mvp: &mvp_store::Store) -> Result<(), BoxErr> {
    let plan = ruby_plan(store, mvp)?;
    apply(store, &plan)?;
    store.meta_set(RUBY_GENERATION_KEY, &mvp.provenance()?.generation)?;
    Ok(())
}

/// The merge again after an update, on a graph [`restore`]d to this
/// resolver's rows (what it undid is `undo`) whose passes then moved the
/// rows of the files `moved` ([`moved`]), the update's own files being
/// `batch`; `changes` is what the refresh of pixel-mvp's index (`mvp`)
/// changed.
///
/// A file's merge reads this resolver's rows of the file, pixel-mvp's
/// sites of the file, the pairs of the file's symbols and those of the
/// symbols its sites resolve to. So it is planned again when any of
/// these may have moved: the file is in `batch`, in `moved`, rewritten or
/// redirected by the refresh, or resolves a site into a file of `batch`
/// or rewritten (whose symbols this graph numbers anew or pixel-mvp read
/// again). Every other file gets back the rows `undo` recorded for it.
/// When the index is not the generation the graph was merged against, or
/// a recorded row's file is unknown, every file is merged again.
pub fn merge_again(
    store: &GraphStore,
    mvp: &mvp_store::Store,
    changes: &Changes,
    undo: &Undo,
    moved: &BTreeSet<String>,
    batch: &[&str],
) -> Result<(), BoxErr> {
    let merged_from = store.meta_get(RUBY_GENERATION_KEY)?;
    let unplaced = undo.edges.iter().any(|r| r.path.is_none())
        || undo.unresolved.iter().any(|r| r.path.is_none());
    if changes.before.is_none() || merged_from != changes.before || unplaced {
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
    reapply(store, undo, |path| !again.contains(path))?;
    let ruby: HashMap<String, i64> = store
        .files()?
        .into_iter()
        .filter(|f| f.lang == "ruby")
        .map(|f| (f.path, f.id))
        .collect();
    let files: HashSet<i64> = again.iter().filter_map(|p| ruby.get(p).copied()).collect();
    let plan = ruby_plan_in(store, mvp, Scope::Files(&files))?;
    apply(store, &plan)?;
    store.meta_set(RUBY_GENERATION_KEY, &mvp.provenance()?.generation)?;
    Ok(())
}

/// Refreshes pixel-mvp's index of `root` under `state`, then
/// [`merge_into`].
pub fn switch_ruby(root: &Path, store: &GraphStore, state: &Path) -> Result<(), BoxErr> {
    let mvp = refresh(root, state)?;
    merge_into(store, &mvp)
}

#[cfg(test)]
mod audited_contract_tests {
    use super::*;

    fn store() -> GraphStore {
        let store = GraphStore::open_in_memory().unwrap();
        store.conn().execute_batch("INSERT INTO files(id,path,blob_oid,lang) VALUES(1,'a.rb','oid','ruby');
          INSERT INTO symbols(id,uid,file_id,name,qualified,kind,start_line,end_line) VALUES
          (1,'a.rb#a',1,'a','A#a','method',1,5),(2,'a.rb#b',1,'b','A#b','method',6,8);
          INSERT INTO edges(id,src_id,dst_id,kind,tier,site_line,receiver,callee) VALUES(1,1,2,'calls','probable',2,'self','b');").unwrap();
        store
            .insert_unresolved_call(1, "missing", Some(1), 3, None, "calls")
            .unwrap();
        store
    }

    #[test]
    fn recorded_merge_should_restore_and_reapply_both_edge_and_unresolved_row_kinds() {
        let store = store();
        let original = prints(&store).unwrap();
        let plan = RubyPlan {
            remove_edges: vec![1],
            remove_edge_files: vec![1],
            remove_unresolved: vec![1],
            add_edges: vec![EdgeRow {
                src_id: 1,
                dst_id: 2,
                kind: EdgeKind::References,
                tier: Tier::Exact,
                site_line: 4,
                receiver: None,
                callee: Some("b".into()),
            }],
            add_edge_files: vec![1],
            add_unresolved: vec![super::super::Unresolved {
                file_id: 1,
                name: "unknown".into(),
                enclosing: Some(1),
                site_line: 5,
                receiver: Some("other".into()),
                kind: "references",
            }],
            ..RubyPlan::default()
        };
        apply(&store, &plan).unwrap();
        let merged = prints(&store).unwrap();
        assert_ne!(merged, original);
        let undo = recorded(&store).unwrap();
        assert_eq!(
            undo.edges
                .iter()
                .map(|r| (r.added, r.path.as_deref()))
                .collect::<Vec<_>>(),
            [(false, Some("a.rb")), (true, Some("a.rb"))]
        );
        assert_eq!(
            undo.unresolved
                .iter()
                .map(|r| (r.added, r.row.kind))
                .collect::<Vec<_>>(),
            [(false, "calls"), (true, "references")]
        );
        let undo = restore(&store).unwrap();
        assert_eq!(prints(&store).unwrap(), original);
        reapply(&store, &undo, |_| true).unwrap();
        assert_eq!(prints(&store).unwrap(), merged);
        assert_eq!(
            moved(&store, &original).unwrap(),
            BTreeSet::from(["a.rb".into()])
        );
        assert_eq!(moved(&store, &merged).unwrap(), BTreeSet::new());
    }

    #[test]
    fn row_fingerprints_should_count_duplicate_rows_without_collapsing_them() {
        let store = store();
        let before = prints(&store).unwrap();
        assert_eq!(before[&1].0, 2);
        store.conn().execute_batch("INSERT INTO edges(src_id,dst_id,kind,tier,site_line,receiver,callee) SELECT src_id,dst_id,kind,tier,site_line,receiver,callee FROM edges").unwrap();
        let after = prints(&store).unwrap();
        assert_eq!(after[&1].0, 3);
        assert_eq!(
            moved(&store, &before).unwrap(),
            BTreeSet::from(["a.rb".into()])
        );
    }
}
