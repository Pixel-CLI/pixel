// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Path tracing — BFS shortest path between two symbols over outgoing
//! `calls` + `has_method` edges; reports the deepest reachable node on failure.

use std::collections::{HashMap, VecDeque};

use serde::Serialize;

use crate::impact::{file_path_by_id, symbol_by_id};
use crate::store::{EdgeKind, GraphStore, StoreError};

#[derive(Debug, Clone, Serialize)]
pub struct TraceHop {
    pub uid: String,
    pub name: String,
    pub path: String,
    pub line: u32,
    /// Kind of the edge that led into this hop (None for the start node).
    pub edge_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraceResult {
    pub found: bool,
    pub hops: Vec<TraceHop>,
    pub furthest_reachable: Option<TraceHop>,
}

fn hop(
    store: &GraphStore,
    id: i64,
    edge_kind: Option<String>,
) -> Result<Option<TraceHop>, StoreError> {
    Ok(match symbol_by_id(store, id)? {
        Some(s) => Some(TraceHop {
            path: file_path_by_id(store, s.file_id)?,
            uid: s.uid,
            name: s.name,
            line: s.start_line,
            edge_kind,
        }),
        None => None,
    })
}

pub fn trace(
    store: &GraphStore,
    from_uid: &str,
    to_uid: &str,
    max_depth: u32,
) -> Result<TraceResult, StoreError> {
    let from = store
        .symbol_by_uid(from_uid)?
        .ok_or(StoreError::Sql(rusqlite::Error::QueryReturnedNoRows))?;
    let to = store
        .symbol_by_uid(to_uid)?
        .ok_or(StoreError::Sql(rusqlite::Error::QueryReturnedNoRows))?;

    // parent map: node -> (parent, edge_kind used)
    let mut parent: HashMap<i64, (i64, EdgeKind)> = HashMap::new();
    let mut depth_of: HashMap<i64, u32> = HashMap::new();
    depth_of.insert(from.id, 0);
    let mut queue: VecDeque<i64> = VecDeque::new();
    queue.push_back(from.id);
    let mut deepest = (from.id, 0u32);
    let mut found = from.id == to.id;

    'bfs: while let Some(id) = queue.pop_front() {
        let d = depth_of[&id];
        if d >= max_depth {
            continue;
        }
        for kind in [EdgeKind::Calls, EdgeKind::HasMethod] {
            for e in store.edges_from(id, Some(kind))? {
                if depth_of.contains_key(&e.dst_id) {
                    continue;
                }
                depth_of.insert(e.dst_id, d + 1);
                parent.insert(e.dst_id, (id, kind));
                if d + 1 > deepest.1 {
                    deepest = (e.dst_id, d + 1);
                }
                if e.dst_id == to.id {
                    found = true;
                    break 'bfs;
                }
                queue.push_back(e.dst_id);
            }
        }
    }

    if found {
        // reconstruct path
        let mut chain: Vec<(i64, Option<EdgeKind>)> = Vec::new();
        let mut cur = to.id;
        loop {
            match parent.get(&cur) {
                Some(&(p, k)) => {
                    chain.push((cur, Some(k)));
                    cur = p;
                }
                None => {
                    chain.push((cur, None));
                    break;
                }
            }
        }
        chain.reverse();
        let mut hops = Vec::with_capacity(chain.len());
        for (id, k) in chain {
            if let Some(h) = hop(store, id, k.map(|k| k.as_str().to_string()))? {
                hops.push(h);
            }
        }
        Ok(TraceResult {
            found: true,
            hops,
            furthest_reachable: None,
        })
    } else {
        let furthest = hop(store, deepest.0, None)?;
        Ok(TraceResult {
            found: false,
            hops: Vec::new(),
            furthest_reachable: furthest,
        })
    }
}
