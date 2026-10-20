// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel graph-shadow --lang ruby`: what the Ruby hybrid
//! ([`crate::mvp_adapter::ruby`]) would do to this graph's Ruby rows, built
//! in a scratch directory. Unlike Rust, Ruby is merged, not replaced, so
//! the report counts the merge: the edges both graphs hold, this graph's
//! edges kept where pixel-mvp abstains, the ones pixel-mvp's proof
//! replaces, and pixel-mvp's targets added, for calls, routes, model
//! associations and other references.

use rustc_hash::FxHashMap as HashMap;
use std::path::Path;

use serde::Serialize;

use crate::mvp_adapter::{self, ruby};
use crate::store::{EdgeRow, GraphStore};

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

/// The merge, as `graph-shadow --lang ruby --json` prints it.
#[derive(Debug, Default, Serialize)]
pub struct RubyShadow {
    pub mvp_commit: &'static str,
    pub sites: ruby::Sites,
    pub calls: ruby::Edges,
    pub routes: ruby::Edges,
    pub associations: ruby::Edges,
    pub references: ruby::Edges,
    /// `unresolved_calls` rows of Ruby files, before and after the merge.
    pub unresolved: Unresolved,
    /// pixel-mvp's resolved sites left out: no target here, no caller here.
    pub dropped_no_target: usize,
    pub dropped_no_caller: usize,
    /// A few edges the merge removes and adds, `caller -> callee (path:line)`.
    pub examples: Examples,
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct Unresolved {
    pub v1: usize,
    pub hybrid: usize,
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct Examples {
    pub v1_replaced: Vec<String>,
    pub mvp_added: Vec<String>,
}

/// Builds this graph of `root` with its own resolver and pixel-mvp's index
/// in a scratch directory, then reports the merge ([`compare`]). Writes
/// nothing under `root`. Fails outside a git work tree.
pub fn shadow(root: &Path, examples: usize) -> Result<RubyShadow, BoxErr> {
    let scratch = tempfile::tempdir()?;
    let db = scratch.path().join("graph.db");
    let mvp = mvp_adapter::refresh(root, scratch.path())?;
    crate::build::build_graph_sources(
        root,
        &db,
        mvp_adapter::RustSource::V1,
        mvp_adapter::ruby_switch::RubySource::V1,
    )?;
    let store = GraphStore::open(&db)?;
    let plan = ruby::ruby_plan(&store, &mvp)?;
    compare(&store, &plan, examples)
}

/// The report of `plan` against `store`, the graph it was computed on.
pub fn compare(
    store: &GraphStore,
    plan: &ruby::RubyPlan,
    examples: usize,
) -> Result<RubyShadow, BoxErr> {
    let conn = store.conn();
    let mut names: HashMap<i64, (String, String)> = HashMap::default();
    {
        let mut stmt = conn.prepare(
            "SELECT s.id, s.qualified, f.path FROM symbols s JOIN files f ON f.id = s.file_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, (r.get(1)?, r.get(2)?))))?;
        for row in rows {
            let (id, pair) = row?;
            names.insert(id, pair);
        }
    }
    let shown = |e: &EdgeRow| -> String {
        let name = |id: i64| names.get(&id).map_or("?", |n| n.0.as_str()).to_string();
        let path = names.get(&e.src_id).map_or("?", |n| n.1.as_str());
        format!(
            "{} -> {} ({path}:{})",
            name(e.src_id),
            name(e.dst_id),
            e.site_line
        )
    };
    let removed: Vec<EdgeRow> = {
        let mut stmt = conn.prepare("SELECT src_id, dst_id, site_line FROM edges WHERE id = ?1")?;
        let mut out = Vec::new();
        for id in &plan.remove_edges {
            out.push(stmt.query_row([id], |r| {
                Ok(EdgeRow {
                    src_id: r.get(0)?,
                    dst_id: r.get(1)?,
                    kind: crate::store::EdgeKind::Calls,
                    tier: crate::store::Tier::Exact,
                    site_line: r.get(2)?,
                    receiver: None,
                    callee: None,
                })
            })?);
        }
        out
    };
    // An edge raised to `exact` is removed and added back: neither a
    // replacement nor an addition.
    let at = |e: &EdgeRow| (e.src_id, e.dst_id, e.site_line);
    let raised: rustc_hash::FxHashSet<(i64, i64, u32)> = {
        let added: rustc_hash::FxHashSet<_> = plan.add_edges.iter().map(at).collect();
        removed
            .iter()
            .map(at)
            .filter(|k| added.contains(k))
            .collect()
    };
    let v1_unresolved: i64 = conn.query_row(
        "SELECT count(*) FROM unresolved_calls u JOIN files f ON f.id = u.file_id
          WHERE f.lang = 'ruby'",
        [],
        |r| r.get(0),
    )?;
    let v1_unresolved = usize::try_from(v1_unresolved)?;
    Ok(RubyShadow {
        mvp_commit: mvp_adapter::MVP_COMMIT,
        sites: plan.sites,
        calls: plan.calls,
        routes: plan.routes,
        associations: plan.associations,
        references: plan.references,
        unresolved: Unresolved {
            v1: v1_unresolved,
            hybrid: v1_unresolved - plan.remove_unresolved.len() + plan.add_unresolved.len(),
        },
        dropped_no_target: plan.no_target,
        dropped_no_caller: plan.no_caller,
        examples: Examples {
            v1_replaced: removed
                .iter()
                .filter(|e| !raised.contains(&at(e)))
                .take(examples)
                .map(&shown)
                .collect(),
            mvp_added: plan
                .add_edges
                .iter()
                .filter(|e| !raised.contains(&at(e)))
                .take(examples)
                .map(&shown)
                .collect(),
        },
    })
}
