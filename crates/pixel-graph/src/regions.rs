// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Symbol-level regions manifest for safe parallel edits.
//!
//! Pure analysis over the P0 symbols `scope-task` already returns: the daemon
//! gathers graph inputs (full symbol rows, edges between them, imports), and
//! this module computes regions, conflicts, merge-order layers, and shared
//! files — no I/O, no daemon dependency.
//!
//! Conflict detection over-approximates: when in doubt, two regions conflict.
//! A false conflict only costs serialization; a false "disjoint" costs a
//! broken merge, so it is the unacceptable failure mode.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::store::ImportRow;
use crate::{EdgeRow, FileRow, SymbolRow};

/// Input to the regions analysis: the P0 symbols and their graph context,
/// gathered by the daemon so unit tests need neither a real index nor a real
/// graph.
#[derive(Default)]
pub struct RegionsInput {
    /// P0 symbols with full data (start_line, end_line, etc.)
    pub symbols: Vec<SymbolRow>,
    /// Edges between the P0 symbols (both directions)
    pub edges: Vec<EdgeRow>,
    /// Import relationships between the files containing P0 symbols
    pub imports: Vec<ImportRow>,
    /// File metadata for files containing P0 symbols
    pub files: Vec<FileRow>,
}

/// A symbol-level region: a P0 symbol's line range plus a context reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Region {
    /// Region identifier (the symbol's uid)
    pub id: String,
    /// The symbol's uid (`path#qualified#kind`)
    pub symbol_uid: String,
    /// File path containing the symbol
    pub file_path: String,
    /// Symbol start line (1-indexed)
    pub start_line: u32,
    /// Symbol end line (1-indexed, inclusive)
    pub end_line: u32,
    /// Context reference for the harness to fetch more context
    pub context_ref: String,
}

/// A conflict between two regions, with a reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    /// First region id
    pub region_a: String,
    /// Second region id
    pub region_b: String,
    /// Human-readable reason for the conflict
    pub reason: String,
}

/// A merge-order layer: regions that can be merged in parallel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layer {
    /// Layer level (0 = first to merge)
    pub level: u32,
    /// Region ids in this layer
    pub region_ids: Vec<String>,
}

/// Epistemics envelope for the regions manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionsEnvelope {
    /// True when the analysis may have missed conflicts (lower bound)
    pub lower_bound: bool,
    /// Caps that fired during analysis
    pub caps: Vec<String>,
}

/// The complete regions manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionsManifest {
    /// Symbol-level regions
    pub regions: Vec<Region>,
    /// Conflict pairs with reasons
    pub conflicts: Vec<Conflict>,
    /// Merge-order layers from condensed call graph
    pub layers: Vec<Layer>,
    /// Files imported by more than one region
    pub shared_files: Vec<String>,
    /// Epistemics envelope
    pub envelope: RegionsEnvelope,
}

/// Compute the regions manifest from P0 symbols and their graph context.
///
/// Conflict detection over-approximates: when in doubt, two regions conflict.
/// A false conflict only costs serialization; a false "disjoint" costs a
/// broken merge.
pub fn compute_regions(input: &RegionsInput) -> RegionsManifest {
    let regions: Vec<Region> = input
        .symbols
        .iter()
        .map(|sym| Region {
            id: sym.uid.clone(),
            symbol_uid: sym.uid.clone(),
            file_path: file_path_for_symbol(input, sym),
            start_line: sym.start_line,
            end_line: sym.end_line,
            context_ref: format!("symbol:{}", sym.uid),
        })
        .collect();

    let conflicts = compute_conflicts(input, &regions);
    let layers = compute_layers(&regions, &conflicts);
    let shared_files = compute_shared_files(input, &regions);

    RegionsManifest {
        regions,
        conflicts,
        layers,
        shared_files,
        envelope: RegionsEnvelope {
            lower_bound: false,
            caps: Vec::new(),
        },
    }
}

/// Look up the file path for a symbol from the input's file list.
fn file_path_for_symbol(input: &RegionsInput, sym: &SymbolRow) -> String {
    input
        .files
        .iter()
        .find(|f| f.id == sym.file_id)
        .map(|f| f.path.clone())
        .unwrap_or_default()
}

/// Compute conflict pairs between regions.
///
/// Over-approximates: any doubt → conflict. The conservative direction is
/// the only safe one — a false conflict costs serialization, a false
/// "disjoint" costs a broken merge.
fn compute_conflicts(input: &RegionsInput, regions: &[Region]) -> Vec<Conflict> {
    let mut conflicts = Vec::new();

    // Build a map from symbol uid to region index for edge lookups.
    let uid_to_region: HashMap<&str, usize> = regions
        .iter()
        .enumerate()
        .map(|(i, r)| (r.symbol_uid.as_str(), i))
        .collect();

    // Build a map from file_id to file path for import lookups.
    let file_id_to_path: HashMap<i64, &str> = input
        .files
        .iter()
        .map(|f| (f.id, f.path.as_str()))
        .collect();

    // Build a map from file path to region indices (for same-file conflicts).
    let mut file_to_regions: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, r) in regions.iter().enumerate() {
        file_to_regions.entry(r.file_path.as_str()).or_default().push(i);
    }

    // 1. Same-file conflicts: two regions in the same file always conflict.
    for indices in file_to_regions.values() {
        if indices.len() > 1 {
            for a in 0..indices.len() {
                for b in (a + 1)..indices.len() {
                    conflicts.push(Conflict {
                        region_a: regions[indices[a]].id.clone(),
                        region_b: regions[indices[b]].id.clone(),
                        reason: "same_file".to_string(),
                    });
                }
            }
        }
    }

    // 2. Edge conflicts: if there's an edge between two regions' symbols
    // (in either direction), they conflict.
    for edge in &input.edges {
        // Find which regions these symbols belong to.
        let src_uid = symbol_uid_by_id(input, edge.src_id);
        let dst_uid = symbol_uid_by_id(input, edge.dst_id);
        if let (Some(src_uid), Some(dst_uid)) = (src_uid, dst_uid)
            && let (Some(&a), Some(&b)) = (
                uid_to_region.get(src_uid.as_str()),
                uid_to_region.get(dst_uid.as_str()),
            )
                && a != b {
                    conflicts.push(Conflict {
                        region_a: regions[a].id.clone(),
                        region_b: regions[b].id.clone(),
                        reason: "edge".to_string(),
                    });
                }
    }

    // 3. Import conflicts: if one region's file imports another's, they conflict.
    for imp in &input.imports {
        let from_path = file_id_to_path.get(&imp.file_id).copied();
        let to_path = imp
            .resolved_file_id
            .and_then(|id| file_id_to_path.get(&id).copied());
        if let (Some(from_path), Some(to_path)) = (from_path, to_path)
            && let (Some(from_indices), Some(to_indices)) = (
                file_to_regions.get(from_path),
                file_to_regions.get(to_path),
            ) {
                for &a in from_indices {
                    for &b in to_indices {
                        if a != b {
                            conflicts.push(Conflict {
                                region_a: regions[a].id.clone(),
                                region_b: regions[b].id.clone(),
                                reason: "import".to_string(),
                            });
                        }
                    }
                }
            }
    }

    // 4. Same-name conflicts: if two regions' symbols have the same name,
    // they conflict (conservative: same name = potential relationship).
    let mut name_to_regions: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, r) in regions.iter().enumerate() {
        // Extract the name from the symbol uid (path#qualified#kind → qualified)
        // We use the symbol's name field directly from the input.
        if let Some(sym) = input.symbols.iter().find(|s| s.uid == r.symbol_uid) {
            name_to_regions.entry(sym.name.as_str()).or_default().push(i);
        }
    }
    for indices in name_to_regions.values() {
        if indices.len() > 1 {
            for a in 0..indices.len() {
                for b in (a + 1)..indices.len() {
                    conflicts.push(Conflict {
                        region_a: regions[indices[a]].id.clone(),
                        region_b: regions[indices[b]].id.clone(),
                        reason: "same_name".to_string(),
                    });
                }
            }
        }
    }

    // Deduplicate conflicts (same pair, same reason).
    conflicts.sort_by(|a, b| {
        (&a.region_a, &a.region_b, &a.reason).cmp(&(&b.region_a, &b.region_b, &b.reason))
    });
    conflicts.dedup_by(|a, b| {
        a.region_a == b.region_a && a.region_b == b.region_b && a.reason == b.reason
    });

    conflicts
}

/// Look up a symbol's uid by its id from the input's symbol list.
fn symbol_uid_by_id(input: &RegionsInput, id: i64) -> Option<String> {
    input.symbols.iter().find(|s| s.id == id).map(|s| s.uid.clone())
}

/// Compute merge-order layers from the condensed call graph.
///
/// 1. Build a conflict graph (nodes = regions, edges = conflicts).
/// 2. Condense SCCs — regions in the same SCC must be merged together.
/// 3. Topological sort the condensed DAG.
/// 4. Each topological level is a layer.
fn compute_layers(regions: &[Region], conflicts: &[Conflict]) -> Vec<Layer> {
    if regions.is_empty() {
        return Vec::new();
    }

    let n = regions.len();
    let id_to_idx: HashMap<&str, usize> = regions
        .iter()
        .enumerate()
        .map(|(i, r)| (r.id.as_str(), i))
        .collect();

    // Build adjacency list for the conflict graph.
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for c in conflicts {
        if let (Some(&a), Some(&b)) = (id_to_idx.get(c.region_a.as_str()), id_to_idx.get(c.region_b.as_str())) {
            adj[a].push(b);
        }
    }

    // Tarjan's SCC algorithm (iterative).
    let sccs = tarjan_scc(n, &adj);

    // Build the condensed DAG: each SCC is a node, edges between SCCs.
    let scc_count = sccs.len();
    let mut node_to_scc: Vec<usize> = vec![0; n];
    for (scc_idx, scc) in sccs.iter().enumerate() {
        for &node in scc {
            node_to_scc[node] = scc_idx;
        }
    }

    // Build condensed adjacency (as a set to avoid duplicates).
    let mut condensed_adj: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); scc_count];
    let mut condensed_indeg: Vec<usize> = vec![0; scc_count];
    for u in 0..n {
        for &v in &adj[u] {
            let su = node_to_scc[u];
            let sv = node_to_scc[v];
            if su != sv && condensed_adj[su].insert(sv) {
                condensed_indeg[sv] += 1;
            }
        }
    }

    // Kahn's algorithm for topological sort, level by level.
    let mut current_level: Vec<usize> = (0..scc_count)
        .filter(|&i| condensed_indeg[i] == 0)
        .collect();
    let mut layers: Vec<Layer> = Vec::new();
    let mut level_num: u32 = 0;

    while !current_level.is_empty() {
        let mut next_level: Vec<usize> = Vec::new();
        let mut region_ids: Vec<String> = Vec::new();

        for &scc_idx in &current_level {
            for &node in &sccs[scc_idx] {
                region_ids.push(regions[node].id.clone());
            }
            for &next in &condensed_adj[scc_idx] {
                condensed_indeg[next] -= 1;
                if condensed_indeg[next] == 0 {
                    next_level.push(next);
                }
            }
        }

        region_ids.sort();
        layers.push(Layer {
            level: level_num,
            region_ids,
        });
        level_num += 1;
        current_level = next_level;
    }

    layers
}

/// Tarjan's SCC algorithm (iterative, to avoid stack overflow on large graphs).
fn tarjan_scc(n: usize, adj: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut index: Vec<Option<usize>> = vec![None; n];
    let mut lowlink: Vec<usize> = vec![0; n];
    let mut on_stack: Vec<bool> = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut sccs: Vec<Vec<usize>> = Vec::new();
    let mut counter: usize = 0;

    for start in 0..n {
        if index[start].is_some() {
            continue;
        }
        // Iterative DFS.
        let mut call_stack: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&(v, pi)) = call_stack.last() {
            if pi == 0 {
                // First time visiting this node.
                index[v] = Some(counter);
                lowlink[v] = counter;
                counter += 1;
                stack.push(v);
                on_stack[v] = true;
            }

            if pi < adj[v].len() {
                let w = adj[v][pi];
                call_stack.last_mut().unwrap().1 += 1;
                if index[w].is_none() {
                    call_stack.push((w, 0));
                } else if on_stack[w] {
                    lowlink[v] = lowlink[v].min(index[w].unwrap());
                }
            } else {
                // Done with this node.
                call_stack.pop();
                if let Some(&(parent, _)) = call_stack.last() {
                    lowlink[parent] = lowlink[parent].min(lowlink[v]);
                }
                if lowlink[v] == index[v].unwrap() {
                    // Start a new SCC.
                    let mut scc: Vec<usize> = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on_stack[w] = false;
                        scc.push(w);
                        if w == v {
                            break;
                        }
                    }
                    scc.sort_unstable();
                    sccs.push(scc);
                }
            }
        }
    }

    sccs
}

/// Compute shared files: files imported by more than one region.
fn compute_shared_files(input: &RegionsInput, regions: &[Region]) -> Vec<String> {
    // Build a map from file_id to region indices (which regions live in this file).
    let mut file_id_to_regions: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, r) in regions.iter().enumerate() {
        if let Some(f) = input.files.iter().find(|f| f.path == r.file_path) {
            file_id_to_regions.entry(f.id).or_default().push(i);
        }
    }

    // Build a map from file_id to file path.
    let file_id_to_path: HashMap<i64, &str> = input
        .files
        .iter()
        .map(|f| (f.id, f.path.as_str()))
        .collect();

    // For each imported file, collect the set of regions that import it.
    let mut imported_by: BTreeMap<i64, BTreeSet<usize>> = BTreeMap::new();
    for imp in &input.imports {
        if let Some(&to_path) = imp.resolved_file_id.and_then(|id| file_id_to_path.get(&id)) {
            // Find which regions live in the importing file (imp.file_id).
            if let Some(importers) = file_id_to_regions.get(&imp.file_id) {
                for &region_idx in importers {
                    imported_by
                        .entry(imp.resolved_file_id.unwrap())
                        .or_default()
                        .insert(region_idx);
                }
            }
            let _ = to_path;
        }
    }

    // Shared files: imported by more than one region.
    let mut shared: BTreeSet<String> = BTreeSet::new();
    for (resolved_id, importers) in &imported_by {
        if importers.len() > 1
            && let Some(&path) = file_id_to_path.get(resolved_id)
        {
            shared.insert(path.to_string());
        }
    }

    shared.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EdgeKind, SymbolKind, Tier};

    fn make_symbol(id: i64, uid: &str, file_id: i64, name: &str, start: u32, end: u32) -> SymbolRow {
        SymbolRow {
            id,
            uid: uid.to_string(),
            file_id,
            name: name.to_string(),
            qualified: name.to_string(),
            kind: SymbolKind::Function,
            start_line: start,
            end_line: end,
            sig: format!("fn {name}()"),
        }
    }

    fn make_file(id: i64, path: &str) -> FileRow {
        FileRow {
            id,
            path: path.to_string(),
            blob_oid: format!("oid{id}"),
            lang: "rust".to_string(),
        }
    }

    fn make_edge(src: i64, dst: i64) -> EdgeRow {
        EdgeRow {
            src_id: src,
            dst_id: dst,
            kind: EdgeKind::Calls,
            tier: Tier::Exact,
            site_line: 1,
            receiver: None,
            callee: None,
        }
    }

    fn make_import(id: i64, file_id: i64, resolved_file_id: i64) -> ImportRow {
        ImportRow {
            id,
            file_id,
            spec: format!("mod{id}"),
            path: format!("mod{id}"),
            bindings: Vec::new(),
            resolved_file_id: Some(resolved_file_id),
        }
    }

    #[test]
    fn empty_input_produces_empty_manifest() {
        let input = RegionsInput::default();
        let manifest = compute_regions(&input);
        assert!(manifest.regions.is_empty());
        assert!(manifest.conflicts.is_empty());
        assert!(manifest.layers.is_empty());
        assert!(manifest.shared_files.is_empty());
        assert!(!manifest.envelope.lower_bound);
    }

    #[test]
    fn single_region_no_conflicts() {
        let input = RegionsInput {
            symbols: vec![make_symbol(1, "a.rs#foo#Function", 1, "foo", 10, 20)],
            files: vec![make_file(1, "a.rs")],
            ..Default::default()
        };
        let manifest = compute_regions(&input);
        assert_eq!(manifest.regions.len(), 1);
        assert!(manifest.conflicts.is_empty());
        assert_eq!(manifest.layers.len(), 1);
        assert_eq!(manifest.layers[0].level, 0);
        assert_eq!(manifest.layers[0].region_ids.len(), 1);
    }

    #[test]
    fn same_file_conflict() {
        let input = RegionsInput {
            symbols: vec![
                make_symbol(1, "a.rs#foo#Function", 1, "foo", 10, 20),
                make_symbol(2, "a.rs#bar#Function", 1, "bar", 30, 40),
            ],
            files: vec![make_file(1, "a.rs")],
            ..Default::default()
        };
        let manifest = compute_regions(&input);
        assert_eq!(manifest.regions.len(), 2);
        assert_eq!(manifest.conflicts.len(), 1);
        assert_eq!(manifest.conflicts[0].reason, "same_file");
    }

    #[test]
    fn edge_conflict() {
        let input = RegionsInput {
            symbols: vec![
                make_symbol(1, "a.rs#foo#Function", 1, "foo", 10, 20),
                make_symbol(2, "b.rs#bar#Function", 2, "bar", 30, 40),
            ],
            files: vec![make_file(1, "a.rs"), make_file(2, "b.rs")],
            edges: vec![make_edge(1, 2)],
            ..Default::default()
        };
        let manifest = compute_regions(&input);
        assert_eq!(manifest.conflicts.len(), 1);
        assert_eq!(manifest.conflicts[0].reason, "edge");
    }

    #[test]
    fn import_conflict() {
        let input = RegionsInput {
            symbols: vec![
                make_symbol(1, "a.rs#foo#Function", 1, "foo", 10, 20),
                make_symbol(2, "b.rs#bar#Function", 2, "bar", 30, 40),
            ],
            files: vec![make_file(1, "a.rs"), make_file(2, "b.rs")],
            imports: vec![make_import(1, 1, 2)],
            ..Default::default()
        };
        let manifest = compute_regions(&input);
        assert_eq!(manifest.conflicts.len(), 1);
        assert_eq!(manifest.conflicts[0].reason, "import");
    }

    #[test]
    fn same_name_conflict() {
        let input = RegionsInput {
            symbols: vec![
                make_symbol(1, "a.rs#foo#Function", 1, "process", 10, 20),
                make_symbol(2, "b.rs#foo#Function", 2, "process", 30, 40),
            ],
            files: vec![make_file(1, "a.rs"), make_file(2, "b.rs")],
            ..Default::default()
        };
        let manifest = compute_regions(&input);
        assert_eq!(manifest.conflicts.len(), 1);
        assert_eq!(manifest.conflicts[0].reason, "same_name");
    }

    #[test]
    fn over_approximation_same_file_wins() {
        // Two regions in the same file with an edge: both reasons apply,
        // but the conflict is reported (deduplicated by reason).
        let input = RegionsInput {
            symbols: vec![
                make_symbol(1, "a.rs#foo#Function", 1, "foo", 10, 20),
                make_symbol(2, "a.rs#bar#Function", 1, "bar", 30, 40),
            ],
            files: vec![make_file(1, "a.rs")],
            edges: vec![make_edge(1, 2)],
            ..Default::default()
        };
        let manifest = compute_regions(&input);
        // Both same_file and edge conflicts should be present.
        let reasons: Vec<&str> = manifest.conflicts.iter().map(|c| c.reason.as_str()).collect();
        assert!(reasons.contains(&"same_file"));
        assert!(reasons.contains(&"edge"));
    }

    #[test]
    fn layers_ordered_by_conflict_dag() {
        // A → B → C: A and C can merge in parallel, B must wait.
        let input = RegionsInput {
            symbols: vec![
                make_symbol(1, "a.rs#foo#Function", 1, "foo", 10, 20),
                make_symbol(2, "b.rs#bar#Function", 2, "bar", 30, 40),
                make_symbol(3, "c.rs#baz#Function", 3, "baz", 50, 60),
            ],
            files: vec![make_file(1, "a.rs"), make_file(2, "b.rs"), make_file(3, "c.rs")],
            edges: vec![make_edge(1, 2), make_edge(2, 3)],
            ..Default::default()
        };
        let manifest = compute_regions(&input);
        assert_eq!(manifest.layers.len(), 3);
        assert_eq!(manifest.layers[0].level, 0);
        assert_eq!(manifest.layers[1].level, 1);
        assert_eq!(manifest.layers[2].level, 2);
    }

    #[test]
    fn scc_regions_merge_together() {
        // A ↔ B (cycle): both in the same SCC, same layer.
        let input = RegionsInput {
            symbols: vec![
                make_symbol(1, "a.rs#foo#Function", 1, "foo", 10, 20),
                make_symbol(2, "b.rs#bar#Function", 2, "bar", 30, 40),
            ],
            files: vec![make_file(1, "a.rs"), make_file(2, "b.rs")],
            edges: vec![make_edge(1, 2), make_edge(2, 1)],
            ..Default::default()
        };
        let manifest = compute_regions(&input);
        // Both regions should be in the same layer (same SCC).
        assert_eq!(manifest.layers.len(), 1);
        assert_eq!(manifest.layers[0].region_ids.len(), 2);
    }

    #[test]
    fn shared_files_detected() {
        // Two regions both import a third file.
        let input = RegionsInput {
            symbols: vec![
                make_symbol(1, "a.rs#foo#Function", 1, "foo", 10, 20),
                make_symbol(2, "b.rs#bar#Function", 2, "bar", 30, 40),
            ],
            files: vec![
                make_file(1, "a.rs"),
                make_file(2, "b.rs"),
                make_file(3, "common.rs"),
            ],
            imports: vec![make_import(1, 1, 3), make_import(2, 2, 3)],
            ..Default::default()
        };
        let manifest = compute_regions(&input);
        assert!(manifest.shared_files.contains(&"common.rs".to_string()));
    }

    #[test]
    fn no_shared_files_when_only_one_importer() {
        let input = RegionsInput {
            symbols: vec![
                make_symbol(1, "a.rs#foo#Function", 1, "foo", 10, 20),
                make_symbol(2, "b.rs#bar#Function", 2, "bar", 30, 40),
            ],
            files: vec![
                make_file(1, "a.rs"),
                make_file(2, "b.rs"),
                make_file(3, "common.rs"),
            ],
            imports: vec![make_import(1, 1, 3)],
            ..Default::default()
        };
        let manifest = compute_regions(&input);
        assert!(!manifest.shared_files.contains(&"common.rs".to_string()));
    }

    #[test]
    fn region_carries_line_range_and_context_ref() {
        let input = RegionsInput {
            symbols: vec![make_symbol(1, "a.rs#foo#Function", 1, "foo", 10, 20)],
            files: vec![make_file(1, "a.rs")],
            ..Default::default()
        };
        let manifest = compute_regions(&input);
        let r = &manifest.regions[0];
        assert_eq!(r.start_line, 10);
        assert_eq!(r.end_line, 20);
        assert_eq!(r.context_ref, "symbol:a.rs#foo#Function");
        assert_eq!(r.file_path, "a.rs");
    }

    #[test]
    fn conservative_direction_false_conflict_ok() {
        // Two regions with no graph relationship but same name: they conflict
        // (conservative). This is the over-approximation direction.
        let input = RegionsInput {
            symbols: vec![
                make_symbol(1, "a.rs#process#Function", 1, "process", 10, 20),
                make_symbol(2, "b.rs#process#Function", 2, "process", 30, 40),
            ],
            files: vec![make_file(1, "a.rs"), make_file(2, "b.rs")],
            ..Default::default()
        };
        let manifest = compute_regions(&input);
        // They conflict despite no edge/import — same name is enough.
        assert!(!manifest.conflicts.is_empty());
    }
}
