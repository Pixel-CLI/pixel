// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Fact gathering for the structural check kinds (`diff-in-scope`,
//! `graph-resolves`, `tests-touched`).
//!
//! The kinds themselves are pure functions in `pixel_task::structural`; this
//! module owns the repository side — the working-tree diff, the scoped
//! targets manifest and the graph store — and gathers only the facts the
//! selected checks need. A missing fact (no manifest, no graph) stays `None`
//! in the context: the check then reports `Unavailable`, never a silent pass.

use std::path::Path;

use pixel_graph::build::update_files;
use pixel_graph::store::GraphStore;
use pixel_task::{CallEdge, CheckKind, StructuralContext, TestPathConventions};

/// Working-tree diff paths: tracked changes against HEAD plus untracked
/// files, sorted and deduped.
pub(crate) fn diff_paths(root: &Path) -> Result<Vec<String>, String> {
    let git = pixel_git::GitRunner::new(root);
    let mut paths: Vec<String> = Vec::new();
    for args in [
        vec!["diff", "--name-only", "HEAD"],
        vec!["ls-files", "--others", "--exclude-standard"],
    ] {
        let out = git
            .run(&args)
            .map_err(|error| format!("git {args:?}: {error}"))?;
        paths.extend(
            String::from_utf8_lossy(&out)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string),
        );
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Union of every active task's scoped manifest paths; `None` when no
/// targets manifest exists (the scope pipeline has not run).
pub(crate) fn manifest_paths(root: &Path) -> Option<Vec<String>> {
    let path = root
        .join(pixel_index::index::SHARD_DIR)
        .join("targets.json");
    let text = std::fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let mut paths: Vec<String> = Vec::new();
    if let Some(tasks) = value.get("tasks").and_then(serde_json::Value::as_array) {
        for task in tasks {
            collect_target_paths(task.get("targets"), &mut paths);
        }
    }
    // Legacy single-task shape: a top-level `files` array.
    collect_target_paths(value.get("files"), &mut paths);
    paths.sort();
    paths.dedup();
    Some(paths)
}

fn collect_target_paths(value: Option<&serde_json::Value>, out: &mut Vec<String>) {
    let Some(targets) = value.and_then(serde_json::Value::as_array) else {
        return;
    };
    for target in targets {
        match target {
            serde_json::Value::String(path) => out.push(path.clone()),
            serde_json::Value::Object(_) => {
                if let Some(path) = target.get("path").and_then(serde_json::Value::as_str) {
                    out.push(path.to_string());
                }
            }
            _ => {}
        }
    }
}

/// Resolved `calls` edges touching `changed` files, as endpoint-labelled
/// edges. Re-extraction only rewrites edges that touch a changed file, so
/// this is exactly the set of edges that could have become unresolved.
fn snapshot_edges(store: &GraphStore, changed: &[String]) -> Result<Vec<CallEdge>, String> {
    if changed.is_empty() {
        return Ok(Vec::new());
    }
    let mut placeholders = String::new();
    for (index, _) in changed.iter().enumerate() {
        if index > 0 {
            placeholders.push(',');
        }
        placeholders.push('?');
    }
    let sql = format!(
        "SELECT e.site_line, ff.path, sf.name, df.path, dt.name
         FROM edges e
         JOIN symbols sf ON sf.id = e.src_id
         JOIN symbols dt ON dt.id = e.dst_id
         JOIN files ff ON ff.id = sf.file_id
         JOIN files df ON df.id = dt.file_id
         WHERE e.kind = 'calls'
           AND (ff.path IN ({placeholders}) OR df.path IN ({placeholders}))"
    );
    let mut stmt = store
        .conn()
        .prepare(&sql)
        .map_err(|error| format!("prepare edge snapshot: {error}"))?;
    let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(changed.len() * 2);
    for path in changed {
        params.push(path);
    }
    for path in changed {
        params.push(path);
    }
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |row| {
            let site_line: u32 = row.get(0)?;
            let src_path: String = row.get(1)?;
            let src_name: String = row.get(2)?;
            let dst_path: String = row.get(3)?;
            let dst_name: String = row.get(4)?;
            Ok(CallEdge {
                src: format!("{src_path}::{src_name}"),
                dst: format!("{dst_path}::{dst_name}"),
                site_line,
            })
        })
        .map_err(|error| format!("edge snapshot query: {error}"))?;
    let mut edges = Vec::new();
    for row in rows {
        edges.push(row.map_err(|error| format!("edge snapshot row: {error}"))?);
    }
    edges.sort_by(|a, b| {
        a.src
            .cmp(&b.src)
            .then(a.dst.cmp(&b.dst))
            .then(a.site_line.cmp(&b.site_line))
    });
    Ok(edges)
}

/// The before/after call-edge snapshots a `graph-resolves` check compares.
type GraphFacts = (Vec<CallEdge>, Vec<CallEdge>);

/// Graph facts for `graph-resolves`: the edges touching the changed files
/// before and after the changed files are re-extracted. `Ok(None)` when no
/// graph exists — the check cannot attest anything then. A failure to open,
/// snapshot or re-extract the graph is an `Err`, so it is reported as a
/// gather failure rather than silently read as "no graph".
///
/// The after snapshot is taken from a scratch copy of the graph database
/// so that the shared graph is never mutated during fact gathering.
fn graph_facts(root: &Path, changed: &[String]) -> Result<Option<GraphFacts>, String> {
    let db = root
        .join(pixel_index::index::SHARD_DIR)
        .join(pixel_daemon::api::GRAPH_DB_FILE);
    if !db.exists() {
        return Ok(None);
    }
    let store = GraphStore::open(&db).map_err(|error| format!("graph open: {error}"))?;
    let before = snapshot_edges(&store, changed)?;
    // Work on a scratch copy so update_files never mutates the shared graph.
    let scratch = std::env::temp_dir().join("pixel-graph-scratch.sqlite");
    std::fs::copy(&db, &scratch).map_err(|error| format!("scratch copy: {error}"))?;
    let files: Vec<(&str, bool)> = changed.iter().map(|p| (p.as_str(), false)).collect();
    update_files(root, &scratch, &files).map_err(|error| format!("graph update: {error}"))?;
    let after = snapshot_edges(
        &GraphStore::open(&scratch).map_err(|error| format!("scratch open: {error}"))?,
        changed,
    )?;
    // Clean up the scratch copy.
    let _ = std::fs::remove_file(&scratch);
    Ok(Some((before, after)))
}

/// Gather the structural facts the selected checks need. Only the kinds
/// present in `checks` are gathered: the graph is re-extracted only for a
/// `graph-resolves` check.
pub(crate) fn gather(
    root: &Path,
    checks: &[pixel_task::Check],
) -> Result<StructuralContext, String> {
    let needs_diff = checks
        .iter()
        .any(|check| matches!(check.kind, CheckKind::DiffInScope | CheckKind::TestsTouched));
    let needs_manifest = checks
        .iter()
        .any(|check| matches!(check.kind, CheckKind::DiffInScope));
    let needs_graph = checks
        .iter()
        .any(|check| matches!(check.kind, CheckKind::GraphResolves));
    let diff_paths = if needs_diff || needs_graph {
        diff_paths(root)?
    } else {
        Vec::new()
    };
    let manifest_paths = if needs_manifest {
        manifest_paths(root)
    } else {
        None
    };
    let (graph_before, graph_after) = if needs_graph {
        match graph_facts(root, &diff_paths)? {
            Some((before, after)) => (Some(before), Some(after)),
            None => (None, None),
        }
    } else {
        (None, None)
    };
    Ok(StructuralContext {
        diff_paths,
        manifest_paths,
        graph_before,
        graph_after,
        test_conventions: TestPathConventions::new(),
    })
}
