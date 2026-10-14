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

use std::path::Path;

use rusqlite::params;

use super::ruby::{RubyPlan, ruby_plan};
use super::{BoxErr, MVP_COMMIT, refresh};
use crate::store::{ExecCached, GraphStore, StoreError};

/// The switch.
pub const RUBY_SOURCE_ENV: &str = "PIXEL_GRAPH_SOURCE_RUBY";

/// Meta key: where the graph's Ruby edges come from. Absent when they are
/// this graph's resolver's alone, so a graph built with the switch off is
/// the graph built before the switch existed.
pub const RUBY_SOURCE_KEY: &str = "graph_source_ruby";

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
    format!("ruby=hybrid {MVP_COMMIT} {}", mvp_index::extractor())
}

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
/// out) or `added` (the merge's own row).
const TABLES: &str = "
CREATE TABLE IF NOT EXISTS ruby_hybrid_edges (
  op TEXT NOT NULL, src_id INTEGER NOT NULL, dst_id INTEGER NOT NULL, kind TEXT NOT NULL,
  tier TEXT NOT NULL, site_line INTEGER NOT NULL, receiver TEXT, callee TEXT);
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
    record_source(store, None)
}

/// Writes `plan` into `store` and records its undo.
pub fn apply(store: &GraphStore, plan: &RubyPlan) -> Result<(), StoreError> {
    let conn = store.conn();
    conn.execute_batch(TABLES)?;
    for id in &plan.remove_edges {
        conn.exec_cached(
            "INSERT INTO ruby_hybrid_edges
               SELECT 'removed', src_id, dst_id, kind, tier, site_line, receiver, callee
                 FROM edges WHERE id = ?1",
            params![id],
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
    for e in &plan.add_edges {
        store.insert_edge(e)?;
        conn.exec_cached(
            "INSERT INTO ruby_hybrid_edges VALUES ('added', ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
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

/// Undoes the last [`apply`]: the merge's rows go, this graph's come back,
/// and the side tables are emptied. A graph with no side tables is left
/// alone.
pub fn restore(store: &GraphStore) -> Result<(), StoreError> {
    if !has_tables(store)? {
        return Ok(());
    }
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
    Ok(())
}

/// The switch at work on a graph whose rows are this resolver's: plans the
/// merge with pixel-mvp's index (`mvp`) and writes it.
pub fn merge_into(store: &GraphStore, mvp: &mvp_store::Store) -> Result<(), BoxErr> {
    let plan = ruby_plan(store, mvp)?;
    apply(store, &plan)?;
    Ok(())
}

/// Refreshes pixel-mvp's index of `root` under `state`, then
/// [`merge_into`].
pub fn switch_ruby(root: &Path, store: &GraphStore, state: &Path) -> Result<(), BoxErr> {
    let mvp = refresh(root, state)?;
    merge_into(store, &mvp)
}
