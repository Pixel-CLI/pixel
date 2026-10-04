// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Process discovery — entry-point-seeded BFS traces over `calls` edges,
//! persisted to `processes`/`process_steps` and readable back as summaries.

use std::collections::{BTreeSet, HashSet, VecDeque};

use rusqlite::params;
use serde::Serialize;

use crate::impact::{file_path_by_id, split_ident_words, symbol_by_id};
use crate::store::{EdgeKind, ExecCached, GraphStore, StoreError, Tier};
use crate::trace::TraceHop;

#[derive(Debug, Clone, Serialize)]
pub struct ProcessSummary {
    pub id: i64,
    pub label: String,
    pub entry_uid: String,
    pub step_count: u32,
    pub steps: Vec<TraceHop>,
}

/// "handleClick" / "handle_click" → "Handle Click flow".
fn humanize(name: &str) -> String {
    let words = split_ident_words(name);
    if words.is_empty() {
        return format!("{name} flow");
    }
    let mut label = words
        .iter()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    label.push_str(" flow");
    label
}

/// Entry points: symbols with ≥1 outgoing `calls` edge and 0 incoming `calls`
/// edges, highest out-degree first. Test functions and common framework
/// boilerplate are excluded so discovered processes represent real execution
/// flows, not test harnesses.
fn entry_points(store: &GraphStore, limit: usize) -> Result<Vec<i64>, StoreError> {
    let mut stmt = store.conn().prepare(
        "SELECT s.id,
                (SELECT COUNT(*) FROM edges e WHERE e.src_id = s.id AND e.kind = 'calls') AS outd
         FROM symbols s
         JOIN files f ON f.id = s.file_id
         WHERE outd > 0
           AND NOT EXISTS (SELECT 1 FROM edges e2 WHERE e2.dst_id = s.id AND e2.kind = 'calls')
           AND NOT (
             -- Exclude test functions: name-based heuristic.
             s.name LIKE 'test_%' OR s.name LIKE 'test%' OR s.name LIKE '%_test'
             OR s.name LIKE '%Test' OR s.name LIKE '%_tests' OR s.name LIKE '%Tests'
             OR s.name LIKE 'it_%' OR s.name LIKE 'it%' OR s.name LIKE 'spec_%'
             OR s.name LIKE 'should_%' OR s.name LIKE 'expect_%'
           )
           AND f.path NOT LIKE 'tests/%'
           AND f.path NOT LIKE '%/tests/%'
           AND f.path NOT LIKE 'test/%'
           AND f.path NOT LIKE '%/test/%'
           AND f.path NOT LIKE '%/__tests__/%'
           AND f.path NOT LIKE 'benches/%'
           AND f.path NOT LIKE '%/benches/%'
           AND f.path NOT LIKE '%.test.ts'
           AND f.path NOT LIKE '%.test.tsx'
           AND f.path NOT LIKE '%.spec.ts'
           AND f.path NOT LIKE '%.spec.tsx'
           AND f.path NOT LIKE '%_test.go'
           AND f.path NOT LIKE 'test_%.py'
           AND f.path NOT LIKE '%/test_%.py'
         ORDER BY outd DESC, s.id ASC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// BFS trace from an entry point: branching capped per node, Exact-tier edges
/// preferred, visited-deduped. Returns visit-ordered (symbol_id, edge_kind).
fn bfs_trace(
    store: &GraphStore,
    entry: i64,
    max_depth: u32,
    max_branching: usize,
) -> Result<Vec<(i64, Option<EdgeKind>)>, StoreError> {
    let mut visited: HashSet<i64> = HashSet::new();
    visited.insert(entry);
    let mut steps: Vec<(i64, Option<EdgeKind>)> = vec![(entry, None)];
    let mut queue: VecDeque<(i64, u32)> = VecDeque::new();
    queue.push_back((entry, 0));
    while let Some((id, d)) = queue.pop_front() {
        if d >= max_depth {
            continue;
        }
        let mut edges = store.edges_from(id, Some(EdgeKind::Calls))?;
        // prefer exact tier first, then stable order
        edges.sort_by_key(|e| (matches!(e.tier, Tier::Probable), e.dst_id));
        let mut taken = 0usize;
        for e in edges {
            if taken >= max_branching {
                break;
            }
            if visited.insert(e.dst_id) {
                steps.push((e.dst_id, Some(EdgeKind::Calls)));
                queue.push_back((e.dst_id, d + 1));
                taken += 1;
            }
        }
    }
    Ok(steps)
}

fn hops_for(
    store: &GraphStore,
    steps: &[(i64, Option<EdgeKind>)],
) -> Result<Vec<TraceHop>, StoreError> {
    let mut out = Vec::with_capacity(steps.len());
    for &(id, kind) in steps {
        if let Some(s) = symbol_by_id(store, id)? {
            out.push(TraceHop {
                path: file_path_by_id(store, s.file_id)?,
                uid: s.uid,
                name: s.name,
                line: s.start_line,
                edge_kind: kind.map(|k| k.as_str().to_string()),
            });
        }
    }
    Ok(out)
}

/// Discover processes from scratch: clears `processes`/`process_steps`, seeds
/// BFS traces from entry points, dedupes near-identical traces (same symbol
/// set), persists and returns the summaries.
pub fn discover(
    store: &mut GraphStore,
    max_depth: u32,
    max_branching: usize,
    min_steps: usize,
    max_processes: usize,
) -> Result<Vec<ProcessSummary>, StoreError> {
    store.conn().execute("DELETE FROM process_steps", [])?;
    store.conn().execute("DELETE FROM processes", [])?;

    let entries = entry_points(store, max_processes.saturating_mul(4).max(16))?;
    let mut seen_sets: HashSet<BTreeSet<i64>> = HashSet::new();
    let mut summaries = Vec::new();

    for entry in entries {
        if summaries.len() >= max_processes {
            break;
        }
        let steps = bfs_trace(store, entry, max_depth, max_branching)?;
        if steps.len() < min_steps {
            continue;
        }
        let key: BTreeSet<i64> = steps.iter().map(|(id, _)| *id).collect();
        if !seen_sets.insert(key) {
            continue;
        }
        let entry_sym = match symbol_by_id(store, entry)? {
            Some(s) => s,
            None => continue,
        };
        let label = humanize(&entry_sym.name);
        store.conn().exec_cached(
            "INSERT INTO processes (label, entry_symbol_id, step_count) VALUES (?1, ?2, ?3)",
            params![label, entry, steps.len() as i64],
        )?;
        let pid = store.conn().last_insert_rowid();
        for (i, (sid, _)) in steps.iter().enumerate() {
            store.conn().exec_cached(
                "INSERT INTO process_steps (process_id, step, symbol_id) VALUES (?1, ?2, ?3)",
                params![pid, i as i64, sid],
            )?;
        }
        let hops = hops_for(store, &steps)?;
        summaries.push(ProcessSummary {
            id: pid,
            label,
            entry_uid: entry_sym.uid,
            step_count: steps.len() as u32,
            steps: hops,
        });
    }
    Ok(summaries)
}

/// List at most `process_limit` persisted processes and at most `step_limit`
/// ordered steps per process. The returned total is the number of persisted
/// processes before limiting.
pub fn list(
    store: &GraphStore,
    process_limit: usize,
    step_limit: usize,
    offset: usize,
) -> Result<(Vec<ProcessSummary>, usize), StoreError> {
    let total = store
        .conn()
        .query_row("SELECT COUNT(*) FROM processes", [], |r| r.get::<_, i64>(0))?
        .max(0) as usize;
    let mut stmt = store.conn().prepare(
        "SELECT id, label, entry_symbol_id, step_count
             FROM processes ORDER BY id LIMIT ?1 OFFSET ?2",
    )?;
    let rows = stmt.query_map(
        params![
            process_limit.min(i64::MAX as usize) as i64,
            offset.min(i64::MAX as usize) as i64
        ],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        },
    )?;
    let procs: Vec<(i64, String, i64, i64)> = rows.collect::<std::result::Result<_, _>>()?;

    let mut out = Vec::with_capacity(procs.len());
    for (pid, label, entry_id, step_count) in procs {
        let entry_uid = symbol_by_id(store, entry_id)?
            .map(|s| s.uid)
            .unwrap_or_default();
        let mut sstmt = store.conn().prepare(
            "SELECT symbol_id FROM process_steps
                 WHERE process_id = ?1 ORDER BY step LIMIT ?2",
        )?;
        let srows = sstmt.query_map(
            params![pid, step_limit.min(i64::MAX as usize) as i64],
            |r| r.get::<_, i64>(0),
        )?;
        let ids: Vec<i64> = srows.collect::<std::result::Result<_, _>>()?;
        let steps_pairs: Vec<(i64, Option<EdgeKind>)> = ids
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, if i == 0 { None } else { Some(EdgeKind::Calls) }))
            .collect();
        let steps = hops_for(store, &steps_pairs)?;
        out.push(ProcessSummary {
            id: pid,
            label,
            entry_uid,
            step_count: step_count as u32,
            steps,
        });
    }
    Ok((out, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{EdgeRow, SymbolKind};

    /// `run → load → parse` in one file: `run` is the only symbol nothing
    /// calls, so it seeds the one process `processes` answers with, and its
    /// steps follow the calls in order.
    #[test]
    fn discover_persists_the_call_chain_from_an_uncalled_entry() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let file = store.replace_file("src/app.rs", "oid", "rust").unwrap();
        let sym = |name: &str, line: u32| {
            store
                .insert_symbol(
                    file,
                    &format!("src/app.rs#{name}#function"),
                    name,
                    name,
                    SymbolKind::Function,
                    line,
                    line + 2,
                    "",
                )
                .unwrap()
        };
        let (run, load, parse) = (sym("run", 1), sym("load", 5), sym("parse", 9));
        for (src, dst) in [(run, load), (load, parse)] {
            store
                .insert_edge(&EdgeRow {
                    src_id: src,
                    dst_id: dst,
                    kind: EdgeKind::Calls,
                    tier: Tier::Exact,
                    site_line: 2,
                    receiver: None,
                    callee: None,
                })
                .unwrap();
        }

        let found = discover(&mut store, 6, 3, 3, 10).unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].entry_uid, "src/app.rs#run#function");
        assert_eq!(found[0].step_count, 3);
        let names: Vec<&str> = found[0].steps.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["run", "load", "parse"]);
        let (listed, total) = list(&store, 10, 10, 0).unwrap();
        assert_eq!(total, 1, "the process is persisted, not only returned");
        assert_eq!(listed[0].entry_uid, found[0].entry_uid);
        let stored: Vec<&str> = listed[0].steps.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(stored, ["run", "load", "parse"], "steps read back in order");
    }
}
