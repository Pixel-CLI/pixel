// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Symbol-level regions manifest for safe parallel edits — the pure analysis
//! behind `pixel scope-task --regions` (issue #814).
//!
//! The daemon gathers the P0 symbols `scope-task` already returns plus the
//! call and import edges between them; this module computes the manifest
//! purely: regions (symbol line ranges + context reference), conservative
//! conflict pairs with reasons, merge-order layers from the condensed call
//! graph, and declared shared files.
//!
//! **Conservative direction:** when in doubt, two regions conflict. A false
//! conflict only costs serialization; a false "disjoint" costs a broken
//! merge. Every conflict pair carries its reason (the witness); every claim
//! that could not be proven rides the epistemics envelope as a named cap.
//!
//! No I/O, no daemon, no index — pure functions over caller-supplied
//! [`RegionsInputs`], mirroring the crate's pure-core pattern.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::Serialize;

/// One P0 symbol `scope-task` returned, with its full line range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegionInput {
    /// Stable id `path#qualified#kind` — also the `pixel pack-context` key.
    pub uid: String,
    pub name: String,
    pub kind: String,
    /// Repo-relative file the symbol lives in.
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// Call edge between two region symbols: `caller` calls `callee`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CallEdge {
    pub caller: String,
    pub callee: String,
}

/// Import edge between two region files: `importer` imports `imported`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportEdge {
    pub importer: String,
    pub imported: String,
}

/// Everything the analysis needs, gathered by the daemon from the graph
/// store so unit tests need neither a real index nor a real graph.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RegionsInputs {
    pub regions: Vec<RegionInput>,
    /// Call edges whose both endpoints are region symbols, deduped.
    pub call_edges: Vec<CallEdge>,
    /// Import edges whose both endpoints are region files, deduped.
    pub import_edges: Vec<ImportEdge>,
    /// False when the code graph could not be opened: no structural claim is
    /// provable, so every pair of regions conflicts (the conservative floor).
    pub graph_available: bool,
    /// Same-name unresolved call sites from the targets envelope: callers that
    /// may exist beyond the P0 set, so the region set is a lower bound.
    pub unresolved_same_name: u64,
    /// Caps the CALLER fired while gathering these inputs. Any entry forces
    /// `lower_bound` on the report envelope.
    pub caps: Vec<String>,
}

/// Why two regions cannot be edited in parallel. The reason is the witness:
/// it names the structural fact (or the missing fact) behind the conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    /// Both regions live in one file: parallel edits risk line shifts and a
    /// textual merge collision.
    SameFile,
    /// One region's symbol calls the other's (either direction): editing a
    /// caller and its callee in parallel can break the call.
    CallEdge,
    /// One region's file imports the other's (either direction): the
    /// dependency's API may need to change for the importer's edit.
    ImportAdjacency,
    /// The two regions define symbols with the same name: a caller may mean
    /// either, and an edit may need to be mirrored.
    SameName,
    /// The code graph was unavailable, so disjointness cannot be proven for
    /// ANY pair — the conservative floor.
    GraphUnavailable,
}

impl ConflictReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConflictReason::SameFile => "same file",
            ConflictReason::CallEdge => "call edge",
            ConflictReason::ImportAdjacency => "import adjacency",
            ConflictReason::SameName => "same name",
            ConflictReason::GraphUnavailable => "graph unavailable",
        }
    }
}

impl Serialize for ConflictReason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Reason priority when several rules fire for one pair: the most structural
/// (least disputable) witness names the pair.
const REASON_PRIORITY: &[ConflictReason] = &[
    ConflictReason::SameFile,
    ConflictReason::CallEdge,
    ConflictReason::ImportAdjacency,
    ConflictReason::SameName,
    ConflictReason::GraphUnavailable,
];

/// One editable territory: a symbol's line range plus the context reference
/// an agent resolves before editing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Region {
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    /// The `pixel pack-context <uid>` key — the region's context reference.
    pub context_ref: String,
}

/// A pair of regions that must NOT be edited in parallel, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Conflict {
    pub a: String,
    pub b: String,
    pub reason: ConflictReason,
}

/// One merge-order layer: regions whose callees (within the region set) are
/// all in earlier layers. Merge layer 0 first so every caller merges against
/// the already-merged callee it calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Layer {
    pub layer: usize,
    pub regions: Vec<String>,
}

/// A file imported by more than one region's file, with the importers as the
/// witness. Declared so the harness can treat the file as an integration
/// point; the file is not itself a region.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SharedFile {
    pub file: String,
    pub imported_by: Vec<String>,
}

/// The regions manifest body (everything except the envelope metadata the
/// caller attaches).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegionsReport {
    pub regions: Vec<Region>,
    pub conflicts: Vec<Conflict>,
    pub layers: Vec<Layer>,
    pub shared_files: Vec<SharedFile>,
    pub lower_bound: bool,
    pub caps: Vec<String>,
}

/// True when the line range is missing or inverted: the territory is
/// uncertain, which the envelope must say (the same-file conflict already
/// serializes the file).
fn range_unknown(start_line: u32, end_line: u32) -> bool {
    end_line == 0 || end_line < start_line
}

/// Compute the regions manifest. Pure and deterministic: regions sort by
/// (file, start_line, uid), conflicts by (a, b) with `a < b`, layers ascend
/// with regions sorted by uid inside a layer, shared files sort by path.
pub fn compute_regions(inputs: RegionsInputs) -> RegionsReport {
    let mut regions: Vec<Region> = inputs
        .regions
        .iter()
        .map(|r| Region {
            uid: r.uid.clone(),
            name: r.name.clone(),
            kind: r.kind.clone(),
            file: r.file.clone(),
            start_line: r.start_line,
            end_line: r.end_line,
            context_ref: r.uid.clone(),
        })
        .collect();
    regions.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.uid.cmp(&b.uid))
    });
    // Dedup on uid defensively: a uid is unique in the graph, but a caller
    // handing us a duplicate must not produce two regions with one identity.
    regions.dedup_by(|a, b| a.uid == b.uid);

    let mut conflicts = detect_conflicts(&regions, &inputs);
    conflicts.sort_by(|a, b| a.a.cmp(&b.a).then(a.b.cmp(&b.b)));
    conflicts.dedup_by(|a, b| a.a == b.a && a.b == b.b);

    let layers = assign_layers(&regions, &inputs);
    let shared_files = detect_shared_files(&inputs);

    // Epistemics: the manifest is a lower bound when the graph was missing,
    // when the caller's caps fired, when same-name call sites are unresolved,
    // or when any region's line range is unknown. Every such fact is named.
    let mut caps = inputs.caps.clone();
    // The graph-unavailable cap is only meaningful when there are regions:
    // with no regions there is no pair whose disjointness is unprovable.
    if !inputs.graph_available && !regions.is_empty() {
        caps.push(
            "code graph unavailable — no structural claim is provable; every pair of regions \
             conflicts"
                .to_string(),
        );
    }
    if inputs.unresolved_same_name > 0 {
        caps.push(format!(
            "{} unresolved call site(s) share a region symbol name; regions beyond the P0 set \
             may exist",
            inputs.unresolved_same_name
        ));
    }
    let unknown_ranges = regions
        .iter()
        .filter(|r| range_unknown(r.start_line, r.end_line))
        .count();
    if unknown_ranges > 0 {
        caps.push(format!(
            "{unknown_ranges} region(s) have an unknown line range; their territory is \
             uncertain beyond the same-file conflict"
        ));
    }
    let lower_bound = !caps.is_empty();

    RegionsReport {
        regions,
        conflicts,
        layers,
        shared_files,
        lower_bound,
        caps,
    }
}

/// Conservative conflict detection. A pair conflicts when ANY rule fires;
/// the reason is the highest-priority witness. With no graph, every pair
/// conflicts — disjointness is unprovable, and a false "disjoint" is the
/// unacceptable failure mode.
fn detect_conflicts(regions: &[Region], inputs: &RegionsInputs) -> Vec<Conflict> {
    let mut out = Vec::new();
    for (i, a) in regions.iter().enumerate() {
        for b in &regions[i + 1..] {
            let reason = pair_reason(a, b, inputs);
            if let Some(reason) = reason {
                out.push(Conflict {
                    a: a.uid.clone(),
                    b: b.uid.clone(),
                    reason,
                });
            }
        }
    }
    out
}

/// The conflict reason for one pair, or `None` when the regions are
/// provably disjoint (different files, no call edge, no import adjacency,
/// different names, graph available).
fn pair_reason(a: &Region, b: &Region, inputs: &RegionsInputs) -> Option<ConflictReason> {
    if !inputs.graph_available {
        return Some(ConflictReason::GraphUnavailable);
    }
    for reason in REASON_PRIORITY {
        let fires = match reason {
            ConflictReason::SameFile => a.file == b.file,
            ConflictReason::CallEdge => inputs.call_edges.iter().any(|e| {
                (e.caller == a.uid && e.callee == b.uid) || (e.caller == b.uid && e.callee == a.uid)
            }),
            ConflictReason::ImportAdjacency => inputs.import_edges.iter().any(|e| {
                (e.importer == a.file && e.imported == b.file)
                    || (e.importer == b.file && e.imported == a.file)
            }),
            ConflictReason::SameName => a.name == b.name,
            ConflictReason::GraphUnavailable => false,
        };
        if fires {
            return Some(*reason);
        }
    }
    None
}

/// Merge-order layers from the condensed call graph: strongly-connected
/// components (mutual recursion) condense to one DAG node, and a component's
/// layer is the longest path to a sink — callees merge before their callers.
/// With no graph there is no ordering evidence: every region shares layer 0
/// (and conflicts anyway).
fn assign_layers(regions: &[Region], inputs: &RegionsInputs) -> Vec<Layer> {
    if regions.is_empty() {
        return Vec::new();
    }
    if !inputs.graph_available {
        return vec![Layer {
            layer: 0,
            regions: regions.iter().map(|r| r.uid.clone()).collect(),
        }];
    }

    let scc_of = strongly_connected_components(regions, inputs);
    // Component id per region uid; components numbered 0..k in SCC order.
    let mut comp_of: HashMap<&str, usize> = HashMap::new();
    let mut members: Vec<Vec<&str>> = Vec::new();
    for (uid, comp) in &scc_of {
        while members.len() <= *comp {
            members.push(Vec::new());
        }
        members[*comp].push(uid.as_str());
        comp_of.insert(uid.as_str(), *comp);
    }
    let k = members.len();

    // Condensed DAG edges: caller component -> callee component.
    let mut dag: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); k];
    for e in &inputs.call_edges {
        let (Some(&ca), Some(&cb)) = (
            comp_of.get(e.caller.as_str()),
            comp_of.get(e.callee.as_str()),
        ) else {
            continue;
        };
        if ca != cb {
            dag[ca].insert(cb);
        }
    }

    // Layer = longest path to a sink: layer(c) = 0 when c calls no other
    // component, else 1 + max(layer of callees). The DAG is acyclic by
    // construction, so the memo recursion terminates.
    let mut memo: Vec<Option<usize>> = vec![None; k];
    for c in 0..k {
        layer_of(c, &dag, &mut memo);
    }

    // Group regions by layer, regions sorted by uid inside a layer.
    let mut by_layer: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (uid, comp) in &scc_of {
        by_layer
            .entry(memo[*comp].unwrap())
            .or_default()
            .push(uid.clone());
    }
    by_layer
        .into_iter()
        .map(|(layer, mut regions)| {
            regions.sort();
            Layer { layer, regions }
        })
        .collect()
}

/// Tarjan's SCC over the region call graph. Iterative: the region set is
/// small (P0-capped), but an explicit stack keeps a pathological input from
/// overflowing the call stack. Returns uid -> component number.
fn strongly_connected_components(
    regions: &[Region],
    inputs: &RegionsInputs,
) -> BTreeMap<String, usize> {
    let uids: Vec<&str> = regions.iter().map(|r| r.uid.as_str()).collect();
    let mut index_of: HashMap<&str, i64> = HashMap::new();
    let mut low: HashMap<&str, i64> = HashMap::new();
    let mut on_stack: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut next_index: i64 = 0;
    let mut comp_of: BTreeMap<String, usize> = BTreeMap::new();
    let mut component: usize = 0;

    // Adjacency: uid -> sorted callee uids (region symbols only).
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in &inputs.call_edges {
        adj.entry(e.caller.as_str())
            .or_default()
            .push(e.callee.as_str());
    }
    for list in adj.values_mut() {
        list.sort_unstable();
        list.dedup();
    }

    // Explicit Tarjan: each frame is (node, next child position). A child
    // frame is pushed only for a tree edge (first visit); a back edge to an
    // on-stack ancestor folds into the lowlink inline; a cross edge to a
    // completed component is ignored.
    for root in &uids {
        if index_of.contains_key(root) {
            continue;
        }
        let mut work: Vec<(&str, usize)> = vec![(root, 0)];
        while let Some((node, pos)) = work.pop() {
            if pos == 0 {
                index_of.insert(node, next_index);
                low.insert(node, next_index);
                next_index += 1;
                stack.push(node);
                on_stack.insert(node);
            }
            let children: Vec<&str> = adj.get(node).cloned().unwrap_or_default();
            if pos < children.len() {
                let child = children[pos];
                work.push((node, pos + 1));
                if !index_of.contains_key(child) {
                    work.push((child, 0));
                } else if on_stack.contains(child) {
                    let child_index = index_of[child];
                    low.insert(node, low[&node].min(child_index));
                }
            } else {
                // All children processed: pop the SCC rooted here.
                if low[&node] == index_of[&node] {
                    while let Some(top) = stack.pop() {
                        on_stack.remove(top);
                        comp_of.insert(top.to_string(), component);
                        if top == node {
                            break;
                        }
                    }
                    component += 1;
                }
                // Propagate the lowlink to the parent frame (tree edge).
                if let Some(&(parent, _)) = work.last() {
                    low.insert(parent, low[&parent].min(low[&node]));
                }
            }
        }
    }
    comp_of
}

/// Layer of component `c`: 0 at a sink, else 1 + max(callee layers).
fn layer_of(c: usize, dag: &[BTreeSet<usize>], memo: &mut [Option<usize>]) -> usize {
    if let Some(layer) = memo[c] {
        return layer;
    }
    // The DAG is acyclic; mark in-progress with 0 to terminate on a
    // defensive cycle (which cannot exist after condensation).
    memo[c] = Some(0);
    let layer = dag[c]
        .iter()
        .map(|&callee| layer_of(callee, dag, memo))
        .max()
        .map_or(0, |max| max + 1);
    memo[c] = Some(layer);
    layer
}

/// Files imported by more than one region's file, with the importers as the
/// witness. Sorted by file path; importers sorted ascending.
fn detect_shared_files(inputs: &RegionsInputs) -> Vec<SharedFile> {
    let mut importers: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for e in &inputs.import_edges {
        importers
            .entry(e.imported.as_str())
            .or_default()
            .insert(e.importer.as_str());
    }
    importers
        .into_iter()
        .filter(|(_, set)| set.len() >= 2)
        .map(|(file, set)| SharedFile {
            file: file.to_string(),
            imported_by: set.into_iter().map(str::to_string).collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests;
