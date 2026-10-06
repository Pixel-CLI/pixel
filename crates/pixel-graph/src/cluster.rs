// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Clustering — label propagation over an undirected symbol adjacency
//! (calls ∪ has_method ∪ symbol-level imports ∪ file-import projection),
//! persisted to `clusters`/`cluster_members`.

use std::collections::HashMap;

use rusqlite::params;
use serde::Serialize;

use crate::impact::split_ident_words;
use crate::store::{ExecCached, GraphStore, StoreError};

#[derive(Debug, Clone, Serialize)]
pub struct ClusterSummary {
    pub id: i64,
    pub label: String,
    pub cohesion: f64,
    pub keywords: String,
    pub symbol_count: u64,
}

struct Graph {
    ids: Vec<i64>,
    adj: Vec<Vec<usize>>,
}

fn build_graph(store: &GraphStore) -> Result<Graph, StoreError> {
    // all symbols
    let mut ids: Vec<i64> = Vec::new();
    {
        let mut stmt = store.conn().prepare("SELECT id FROM symbols ORDER BY id")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        for r in rows {
            ids.push(r?);
        }
    }
    let index: HashMap<i64, usize> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); ids.len()];
    let add = |a: i64, b: i64, adj: &mut Vec<Vec<usize>>| {
        if a == b {
            return;
        }
        if let (Some(&ia), Some(&ib)) = (index.get(&a), index.get(&b)) {
            adj[ia].push(ib);
            adj[ib].push(ia);
        }
    };

    // symbol-level edges: calls, has_method, imports
    {
        let mut stmt = store.conn().prepare(
            "SELECT src_id, dst_id FROM edges WHERE kind IN ('calls','has_method','imports')",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        for r in rows {
            let (a, b) = r?;
            add(a, b, &mut adj);
        }
    }

    // file-level imports projected to symbols via one representative symbol
    // per file (min symbol id) — keeps the projection O(imports). One link per
    // statement and target file: the paths of a Rust `use` that land in one
    // file (`use a::{b, c};`) are one import, as they were before being split.
    {
        let mut stmt = store.conn().prepare(
            "SELECT (SELECT MIN(s.id) FROM symbols s WHERE s.file_id = i.file_id),
                    (SELECT MIN(s.id) FROM symbols s WHERE s.file_id = i.resolved_file_id)
             FROM (SELECT DISTINCT file_id, spec, resolved_file_id FROM imports
                    WHERE resolved_file_id IS NOT NULL) i",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?))
        })?;
        for r in rows {
            if let (Some(a), Some(b)) = r? {
                add(a, b, &mut adj);
            }
        }
    }

    Ok(Graph { ids, adj })
}

/// Synchronous-ish label propagation, deterministic: nodes processed in id
/// order, most-frequent neighbor label wins, ties broken by minimum label.
fn propagate(g: &Graph, iterations: usize) -> Vec<usize> {
    let n = g.ids.len();
    let mut labels: Vec<usize> = (0..n).collect();
    for _ in 0..iterations {
        let mut changed = false;
        for v in 0..n {
            if g.adj[v].is_empty() {
                continue;
            }
            let mut freq: HashMap<usize, usize> = HashMap::new();
            for &u in &g.adj[v] {
                *freq.entry(labels[u]).or_insert(0) += 1;
            }
            let best = freq
                .iter()
                .map(|(&l, &c)| (c, std::cmp::Reverse(l)))
                .max()
                .map(|(_, std::cmp::Reverse(l))| l)
                .unwrap();
            if best != labels[v] {
                labels[v] = best;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    labels
}

fn dominant_dir(paths: &[String]) -> String {
    let mut freq: HashMap<&str, usize> = HashMap::new();
    for p in paths {
        let dir = match p.rfind('/') {
            Some(i) => &p[..i],
            None => "root",
        };
        *freq.entry(dir).or_insert(0) += 1;
    }
    freq.into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
        .map_or_else(|| "root".to_string(), |(d, _)| d.to_string())
}

fn top_keywords(names: &[String], k: usize) -> String {
    let mut freq: HashMap<String, usize> = HashMap::new();
    for n in names {
        for w in split_ident_words(n) {
            if w.len() < 2 {
                continue;
            }
            *freq.entry(w).or_insert(0) += 1;
        }
    }
    let mut v: Vec<(String, usize)> = freq.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.into_iter()
        .take(k)
        .map(|(w, _)| w)
        .collect::<Vec<_>>()
        .join(",")
}

/// Recompute clusters from scratch: clears `clusters`/`cluster_members`,
/// runs label propagation, persists groups of ≥3 (smaller merged into "misc").
pub fn compute(store: &mut GraphStore) -> Result<Vec<ClusterSummary>, StoreError> {
    store.conn().execute("DELETE FROM cluster_members", [])?;
    store.conn().execute("DELETE FROM clusters", [])?;

    let g = build_graph(store)?;
    if g.ids.is_empty() {
        return Ok(Vec::new());
    }
    let labels = propagate(&g, 10);

    // group members by final label
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for (v, &l) in labels.iter().enumerate() {
        groups.entry(l).or_default().push(v);
    }
    let mut real: Vec<Vec<usize>> = Vec::new();
    let mut misc: Vec<usize> = Vec::new();
    let mut keys: Vec<usize> = groups.keys().copied().collect();
    keys.sort_unstable();
    for k in keys {
        let members = groups.remove(&k).unwrap();
        if members.len() >= 3 {
            real.push(members);
        } else {
            misc.extend(members);
        }
    }
    // index of the misc bucket (merged sub-3 groups), if any
    let misc_group: Option<usize> = if misc.is_empty() {
        None
    } else {
        real.push(misc);
        Some(real.len() - 1)
    };

    // cohesion per group: internal / (internal + external) over adjacency
    let mut group_of: Vec<usize> = vec![usize::MAX; g.ids.len()];
    for (gi, members) in real.iter().enumerate() {
        for &v in members {
            group_of[v] = gi;
        }
    }

    let mut out = Vec::with_capacity(real.len());
    for (gi, members) in real.iter().enumerate() {
        let mut internal = 0u64;
        let mut external = 0u64;
        for &v in members {
            for &u in &g.adj[v] {
                if group_of[u] == gi {
                    internal += 1;
                } else {
                    external += 1;
                }
            }
        }
        // each internal edge counted twice
        internal /= 2;
        let cohesion = internal as f64 / std::cmp::max(1, internal + external) as f64;

        let mut paths = Vec::with_capacity(members.len());
        let mut names = Vec::with_capacity(members.len());
        for &v in members {
            let sid = g.ids[v];
            let (name, path): (String, String) = store.conn().query_row(
                "SELECT s.name, f.path FROM symbols s JOIN files f ON f.id = s.file_id
                 WHERE s.id = ?1",
                params![sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            names.push(name);
            paths.push(path);
        }
        let label = if misc_group == Some(gi) {
            "misc".to_string()
        } else {
            dominant_dir(&paths)
        };
        let keywords = top_keywords(&names, 5);

        store.conn().exec_cached(
            "INSERT INTO clusters (label, cohesion, keywords) VALUES (?1, ?2, ?3)",
            params![label, cohesion, keywords],
        )?;
        let cid = store.conn().last_insert_rowid();
        for &v in members {
            store.conn().exec_cached(
                "INSERT INTO cluster_members (cluster_id, symbol_id) VALUES (?1, ?2)",
                params![cid, g.ids[v]],
            )?;
        }
        out.push(ClusterSummary {
            id: cid,
            label,
            cohesion,
            keywords,
            symbol_count: members.len() as u64,
        });
    }
    Ok(out)
}

/// List at most `limit` persisted clusters. The returned total is the number
/// of persisted clusters before limiting.
pub fn list(
    store: &GraphStore,
    limit: usize,
    offset: usize,
) -> Result<(Vec<ClusterSummary>, usize), StoreError> {
    let total = store
        .conn()
        .query_row("SELECT COUNT(*) FROM clusters", [], |r| r.get::<_, i64>(0))?
        .max(0) as usize;
    let mut stmt = store.conn().prepare(
        "SELECT c.id, c.label, c.cohesion, c.keywords,
                (SELECT COUNT(*) FROM cluster_members m WHERE m.cluster_id = c.id)
         FROM clusters c ORDER BY c.id LIMIT ?1 OFFSET ?2",
    )?;
    let rows = stmt.query_map(
        params![
            limit.min(i64::MAX as usize) as i64,
            offset.min(i64::MAX as usize) as i64
        ],
        |r| {
            Ok(ClusterSummary {
                id: r.get(0)?,
                label: r.get(1)?,
                cohesion: r.get(2)?,
                keywords: r.get(3)?,
                symbol_count: r.get::<_, i64>(4)? as u64,
            })
        },
    )?;
    Ok((rows.collect::<std::result::Result<_, _>>()?, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::build_graph;

    /// `compute` must find the group of mutually calling functions and
    /// persist it: an empty answer on a connected graph is the bug the
    /// daemon's `clusters` op would silently pass on as "no clusters".
    #[test]
    fn compute_persists_a_cluster_for_a_connected_call_group() {
        let root = std::env::temp_dir().join(format!(
            "pixel-cluster-compute-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("ring.ts"),
            "export function alpha() { return beta() + delta() }\n\
             export function beta() { return gamma() }\n\
             export function gamma() { return alpha() }\n\
             export function delta() { return beta() }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let mut store = GraphStore::open(&db).unwrap();

        let summaries = compute(&mut store).unwrap();
        assert!(
            !summaries.is_empty(),
            "four mutually calling functions form a cluster"
        );
        assert_eq!(summaries.len(), 1, "{summaries:?}");
        assert_eq!(
            summaries[0].label, "root",
            "a ring of four is no misc bucket"
        );
        assert_eq!(summaries[0].symbol_count, 4);
        assert!(
            (summaries[0].cohesion - 1.0).abs() < 1e-9,
            "every edge stays inside the ring: {summaries:?}"
        );
        let members: u64 = summaries.iter().map(|s| s.symbol_count).sum();
        assert!(
            members >= 3,
            "cluster must hold the ring, got {summaries:?}"
        );
        let persisted: i64 = store
            .conn()
            .query_row("SELECT count(*) FROM cluster_members", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            persisted,
            i64::try_from(members).unwrap(),
            "summaries and table agree"
        );

        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn top_keywords_ranks_by_frequency_before_name() {
        let names = ["zeta_alpha", "zeta_beta", "zeta"].map(String::from);
        assert_eq!(top_keywords(&names, 1), "zeta");
        assert_eq!(top_keywords(&names, 3), "zeta,alpha,beta");
    }

    #[test]
    fn dominant_dir_is_the_most_common_parent_directory() {
        let paths = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(
            dominant_dir(&paths(&["src/a.rs", "src/b.rs", "lib/c.rs", "top.rs"])),
            "src"
        );
        assert_eq!(
            dominant_dir(&paths(&["a/b/c.rs", "a/b/d.rs", "a/e.rs"])),
            "a/b"
        );
        assert_eq!(dominant_dir(&paths(&["top.rs"])), "root");
        assert_eq!(dominant_dir(&[]), "root");
    }
}
