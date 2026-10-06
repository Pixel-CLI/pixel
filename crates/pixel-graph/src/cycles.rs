// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Bounded recursion-cycle enumeration with witnesses and explicit coverage.
//!
//! Finds strongly connected components (SCCs) in the call graph — sets of
//! symbols that can reach each other through `Calls` edges, i.e. potential
//! recursion cycles. The enumeration is bounded by four budgets (node cap,
//! edge cap, time cap, component cap) and reports explicit coverage so an
//! incomplete graph is never read as proof of safety.
//!
//! # The recursion relation
//!
//! Only `EdgeKind::Calls` edges participate. `HasMethod` (class→method
//! ownership) is deliberately excluded: ownership is not runtime invocation,
//! and including it would manufacture cycles that don't exist at runtime.
//! Unresolved calls are never edges — they live in `unresolved_calls` and
//! surface through the coverage report.
//!
//! # Honesty contract
//!
//! - `enumeration_exhausted = true` means every reachable node was visited
//!   and every admissible edge was followed; "no cycles" is a complete answer.
//! - `enumeration_exhausted = false` means a budget fired; "no cycles" is
//!   only a partial answer and the coverage says which budget stopped it.
//! - Every reported cycle carries a witness: a concrete closed path of
//!   stored edges, each re-read from the store so it is verifiable against
//!   the snapshot.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::impact::{file_path_by_id, symbol_by_id};
use crate::store::{EdgeKind, EdgeRow, GraphStore, StoreError, SymbolRow, Tier, fnv1a64};

/// The edge kind that forms the recursion relation: direct calls only.
/// `HasMethod` is excluded because ownership is not runtime invocation.
pub const RECURSION_EDGE_KIND: EdgeKind = EdgeKind::Calls;

/// Which stored edge tiers form the relation being enumerated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TierSelection {
    /// Only `Tier::Exact` edges (same-file scope or import-resolved).
    Exact,
    /// `Tier::Exact` and `Tier::Probable` (unique name in the
    /// import-connected component) edges.
    ExactAndProbable,
}

impl TierSelection {
    /// Parse the `--tiers` value; anything else is `None` (a usage error).
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "exact" => Some(TierSelection::Exact),
            "exact,probable" => Some(TierSelection::ExactAndProbable),
            _ => None,
        }
    }

    /// Whether an edge of tier `tier` belongs to the selected relation.
    pub fn admits(self, tier: Tier) -> bool {
        match self {
            TierSelection::Exact => tier == Tier::Exact,
            TierSelection::ExactAndProbable => true,
        }
    }

    /// The tiers this selection names, for the coverage report.
    pub fn tiers(self) -> &'static [Tier] {
        match self {
            TierSelection::Exact => &[Tier::Exact],
            TierSelection::ExactAndProbable => &[Tier::Exact, Tier::Probable],
        }
    }
}

/// The four budgets the enumeration runs under. Each one that fires is
/// reported by name in the coverage so a partial answer is never mistaken
/// for a complete one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Maximum number of nodes to visit.
    pub max_nodes: u32,
    /// Maximum number of edges to follow.
    pub max_edges: u32,
    /// Wall-clock budget for the enumeration itself.
    pub time_budget: Duration,
    /// Maximum number of components to report.
    pub max_components: u32,
}

/// Elapsed time since the enumeration started. Injected so tests drive the
/// clock deterministically; production uses [`WallClock`].
pub trait Clock {
    /// Time elapsed since the clock was created.
    fn elapsed(&mut self) -> Duration;
}

/// The production clock: `Instant::now()` at creation, real elapsed time.
#[derive(Debug)]
pub struct WallClock(Instant);

impl WallClock {
    /// Start the clock now.
    pub fn start() -> Self {
        WallClock(Instant::now())
    }
}

impl Clock for WallClock {
    #[cfg_attr(test, mutants::skip)] // one-line adapter over Instant; the fake clock tests the logic
    fn elapsed(&mut self) -> Duration {
        self.0.elapsed()
    }
}

/// What to enumerate: all cycles in the call graph over the edges `tiers`
/// admits, within `budget`.
#[derive(Debug, Clone, Copy)]
pub struct Request {
    pub tiers: TierSelection,
    pub budget: Budget,
}

/// Which budget parameter stopped the enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetParameter {
    MaxNodes,
    MaxEdges,
    TimeBudgetMs,
    MaxComponents,
}

/// How much of the graph the enumeration covered, and under what relation.
/// Every cap that fired is named here; none is ever silent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Coverage {
    /// True when every reachable node was visited and every admissible edge
    /// was followed; the precondition of a complete "no cycles" answer.
    pub enumeration_exhausted: bool,
    pub max_nodes: u32,
    pub max_edges: u32,
    pub time_budget_ms: u64,
    pub max_components: u32,
    /// Which budget stopped the enumeration, if any.
    pub stopped_by: Option<BudgetParameter>,
    /// Nodes visited by the enumeration.
    pub visited: u64,
    /// Edges followed by the enumeration.
    pub edges_followed: u64,
    /// Unresolved call sites: calls the resolver could not attach to an
    /// edge, so the stored relation may be missing edges that would form
    /// additional cycles.
    pub unresolved_same_name_sites: u64,
    pub tiers: Vec<Tier>,
}

/// A symbol as it appears in a witness: enough to re-open the definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SymbolRef {
    pub uid: String,
    pub path: String,
    /// `[start_line, end_line]` of the definition.
    pub lines: [u32; 2],
}

/// The call site that justifies an edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CallSite {
    pub path: String,
    pub line: u32,
}

/// The stored edge behind a witness hop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EdgeInfo {
    pub kind: EdgeKind,
    pub tier: Tier,
    pub site: CallSite,
    pub receiver: Option<String>,
}

/// One hop of a witness path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WitnessEdge {
    /// The caller.
    pub from: SymbolRef,
    /// The callee.
    pub to: SymbolRef,
    pub edge: EdgeInfo,
    /// 1-based position along the cycle.
    pub step: u32,
}

/// The evidence behind a reported cycle: a concrete closed path of stored
/// edges, each re-read from the store so it is verifiable against the
/// snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Witness {
    /// A non-empty closed edge list forming a cycle.
    Cycle {
        probable_edges: u32,
        edges: Vec<WitnessEdge>,
    },
    /// No witness; only valid when no cycles were found.
    None,
}

/// One strongly connected component: a set of symbols that can reach each
/// other through `Calls` edges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Component {
    /// Stable content-derived id: FNV-1a of sorted member uids. Two runs
    /// over the same graph produce the same id.
    pub id: String,
    /// Member symbol ids, sorted.
    pub members: Vec<i64>,
    /// A concrete closed cycle as evidence. For a size-1 component with a
    /// self-loop, the self-loop edge. For a multi-node component, a DFS
    /// from the smallest member uid.
    pub witness: Witness,
}

/// The result of a bounded cycle enumeration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Enumeration {
    pub components: Vec<Component>,
    pub coverage: Coverage,
}

/// Admissible outgoing edges of `id` under `tiers`, over
/// [`RECURSION_EDGE_KIND`], in a total order (neighbour id, then site line)
/// so the witness is byte-stable.
fn admissible_edges(
    store: &GraphStore,
    id: i64,
    tiers: TierSelection,
) -> Result<Vec<EdgeRow>, StoreError> {
    let mut edges = store.edges_from(id, Some(RECURSION_EDGE_KIND))?;
    edges.retain(|e| tiers.admits(e.tier));
    edges.sort_by_key(|e| (e.dst_id, e.site_line));
    Ok(edges)
}

fn symbol_ref(store: &GraphStore, sym: &SymbolRow) -> Result<SymbolRef, StoreError> {
    Ok(SymbolRef {
        uid: sym.uid.clone(),
        path: file_path_by_id(store, sym.file_id)?,
        lines: [sym.start_line, sym.end_line],
    })
}

fn required_symbol(store: &GraphStore, id: i64) -> Result<SymbolRow, StoreError> {
    symbol_by_id(store, id)?.ok_or(StoreError::Sql(rusqlite::Error::QueryReturnedNoRows))
}

/// Re-read `edge` from the store: the identical row must still exist.
fn reread_edge(store: &GraphStore, edge: &EdgeRow) -> Result<Option<EdgeRow>, StoreError> {
    Ok(store
        .edges_from(edge.src_id, Some(edge.kind))?
        .into_iter()
        .find(|e| e.dst_id == edge.dst_id && e.site_line == edge.site_line && e.tier == edge.tier))
}

/// Build the witness hop for a stored edge at `step`.
fn witness_edge(store: &GraphStore, edge: &EdgeRow, step: u32) -> Result<WitnessEdge, StoreError> {
    let caller = required_symbol(store, edge.src_id)?;
    let callee = required_symbol(store, edge.dst_id)?;
    Ok(WitnessEdge {
        from: symbol_ref(store, &caller)?,
        to: symbol_ref(store, &callee)?,
        edge: EdgeInfo {
            kind: edge.kind,
            tier: edge.tier,
            site: CallSite {
                path: file_path_by_id(store, caller.file_id)?,
                line: edge.site_line,
            },
            receiver: edge.receiver.clone(),
        },
        step,
    })
}

/// All callable symbol ids in the store, sorted.
fn callable_symbols(store: &GraphStore) -> Result<Vec<i64>, StoreError> {
    let mut stmt = store.conn().prepare(
        "SELECT id FROM symbols WHERE kind IN ('function', 'method', 'script') ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Unresolved call sites: calls the resolver could not attach to an edge.
fn unresolved_call_count(store: &GraphStore) -> Result<u64, StoreError> {
    let count: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM unresolved_calls", [], |r| r.get(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Compute a stable component id from member uids: FNV-1a of the sorted
/// uid list. Two runs over the same graph produce the same id.
fn component_id(member_uids: &[String]) -> String {
    let mut sorted = member_uids.to_vec();
    sorted.sort();
    let joined = sorted.join(",");
    format!("{:016x}", fnv1a64(&joined))
}

/// Find a concrete closed cycle within an SCC. For a size-1 component,
/// the self-loop edge. For a multi-node component, DFS from the smallest
/// member uid to find a path back to it.
///
/// Returns `None` when the time budget expires during witness construction,
/// signaling the caller to stop enumeration.
fn find_cycle_witness(
    store: &GraphStore,
    members: &[i64],
    tiers: TierSelection,
    clock: &mut dyn Clock,
    time_budget: Duration,
) -> Result<Option<Witness>, StoreError> {
    if members.len() == 1 {
        let id = members[0];
        let edges = admissible_edges(store, id, tiers)?;
        if let Some(self_loop) = edges.into_iter().find(|e| e.dst_id == id) {
            let stored = reread_edge(store, &self_loop)?
                .ok_or(StoreError::Sql(rusqlite::Error::QueryReturnedNoRows))?;
            let probable = if stored.tier == Tier::Probable { 1 } else { 0 };
            let edge = witness_edge(store, &stored, 1)?;
            return Ok(Some(Witness::Cycle {
                probable_edges: probable,
                edges: vec![edge],
            }));
        }
        // Size-1 SCC without self-loop: not a cycle (Tarjan wouldn't
        // produce this, but defensive).
        return Ok(Some(Witness::None));
    }

    // Multi-node SCC: DFS from the smallest member uid to find a path
    // back to it. The SCC property guarantees such a path exists.
    let start = *members.iter().min().expect("non-empty");
    let mut visited: HashSet<i64> = HashSet::new();
    let mut stack: Vec<(i64, Vec<EdgeRow>)> = vec![(start, Vec::new())];
    visited.insert(start);

    while let Some((node, path)) = stack.pop() {
        if clock.elapsed() > time_budget {
            return Ok(None);
        }
        let edges = admissible_edges(store, node, tiers)?;
        for edge in &edges {
            let next = edge.dst_id;
            if next == start && !path.is_empty() {
                // Found a cycle back to start.
                let mut cycle_edges = path.clone();
                cycle_edges.push(edge.clone());
                let mut witness_edges = Vec::with_capacity(cycle_edges.len());
                let mut probable = 0u32;
                for (i, e) in cycle_edges.iter().enumerate() {
                    if clock.elapsed() > time_budget {
                        return Ok(None);
                    }
                    let stored = reread_edge(store, e)?
                        .ok_or(StoreError::Sql(rusqlite::Error::QueryReturnedNoRows))?;
                    if stored.tier == Tier::Probable {
                        probable += 1;
                    }
                    let step = u32::try_from(i + 1).unwrap_or(u32::MAX);
                    witness_edges.push(witness_edge(store, &stored, step)?);
                }
                return Ok(Some(Witness::Cycle {
                    probable_edges: probable,
                    edges: witness_edges,
                }));
            }
            if !visited.contains(&next) && members.contains(&next) {
                visited.insert(next);
                let mut new_path = path.clone();
                new_path.push(edge.clone());
                stack.push((next, new_path));
            }
        }
    }

    // Should not happen for a true SCC, but defensive.
    Ok(Some(Witness::None))
}

/// Enumerate strongly connected components in the call graph, bounded by
/// `budget`. Only `Calls` edges participate; `HasMethod` ownership is
/// excluded because ownership is not runtime invocation.
///
/// # Errors
///
/// Any store read failure.
pub fn enumerate(
    store: &GraphStore,
    req: Request,
    clock: &mut dyn Clock,
) -> Result<Enumeration, StoreError> {
    let time_budget_ms = u64::try_from(req.budget.time_budget.as_millis()).unwrap_or(u64::MAX);
    let mut coverage = Coverage {
        enumeration_exhausted: false,
        max_nodes: req.budget.max_nodes,
        max_edges: req.budget.max_edges,
        time_budget_ms,
        max_components: req.budget.max_components,
        stopped_by: None,
        visited: 0,
        edges_followed: 0,
        unresolved_same_name_sites: 0,
        tiers: req.tiers.tiers().to_vec(),
    };

    let unresolved = unresolved_call_count(store)?;
    coverage.unresolved_same_name_sites = unresolved;

    let nodes = callable_symbols(store)?;
    let mut components: Vec<Component> = Vec::new();

    // Iterative Tarjan's SCC algorithm.
    // Each node gets an index (discovery order) and a lowlink (lowest
    // index reachable). When a node's lowlink equals its index, it's the
    // root of an SCC.
    let mut index: HashMap<i64, u32> = HashMap::new();
    let mut lowlink: HashMap<i64, u32> = HashMap::new();
    let mut on_stack: HashSet<i64> = HashSet::new();
    let mut stack: Vec<i64> = Vec::new();
    let mut idx: u32 = 0;

    // Check budgets before starting.
    if req.budget.max_nodes == 0 {
        coverage.stopped_by = Some(BudgetParameter::MaxNodes);
        return Ok(Enumeration {
            components,
            coverage,
        });
    }
    if req.budget.max_edges == 0 {
        coverage.stopped_by = Some(BudgetParameter::MaxEdges);
        return Ok(Enumeration {
            components,
            coverage,
        });
    }
    if req.budget.max_components == 0 {
        coverage.stopped_by = Some(BudgetParameter::MaxComponents);
        return Ok(Enumeration {
            components,
            coverage,
        });
    }

    'outer: for &root in &nodes {
        if index.contains_key(&root) {
            continue;
        }

        // Check time budget.
        if clock.elapsed() > req.budget.time_budget {
            coverage.stopped_by = Some(BudgetParameter::TimeBudgetMs);
            break 'outer;
        }

        // Check node budget.
        if coverage.visited >= u64::from(req.budget.max_nodes) {
            coverage.stopped_by = Some(BudgetParameter::MaxNodes);
            break 'outer;
        }

        // Iterative DFS from root. The frame caches the admissible edge
        // list so resumed frames do not repeat the query, filter, and sort.
        // `None` means "not yet fetched"; `Some(vec)` means "fetched".
        let mut call_stack: Vec<(i64, usize, Option<Vec<EdgeRow>>)> = vec![(root, 0, None)];
        index.insert(root, idx);
        lowlink.insert(root, idx);
        stack.push(root);
        on_stack.insert(root);
        idx += 1;
        coverage.visited += 1;

        while let Some((node, edge_ix, edges_opt)) = call_stack.pop() {
            // Check time budget.
            if clock.elapsed() > req.budget.time_budget {
                coverage.stopped_by = Some(BudgetParameter::TimeBudgetMs);
                break 'outer;
            }

            // On first visit, fetch and cache the admissible edges.
            let edges = match edges_opt {
                None => admissible_edges(store, node, req.tiers)?,
                Some(edges) => edges,
            };

            if edge_ix < edges.len() {
                // Push current state back with cached edges, then follow edge.
                call_stack.push((node, edge_ix + 1, Some(edges.clone())));

                // Check edge budget.
                if coverage.edges_followed >= u64::from(req.budget.max_edges) {
                    coverage.stopped_by = Some(BudgetParameter::MaxEdges);
                    break 'outer;
                }

                let edge = &edges[edge_ix];
                coverage.edges_followed += 1;
                let next = edge.dst_id;

                if let std::collections::hash_map::Entry::Vacant(e) = index.entry(next) {
                    // Check node budget before admitting a new node.
                    if coverage.visited >= u64::from(req.budget.max_nodes) {
                        coverage.stopped_by = Some(BudgetParameter::MaxNodes);
                        break 'outer;
                    }
                    // Tree edge: visit next.
                    e.insert(idx);
                    lowlink.insert(next, idx);
                    stack.push(next);
                    on_stack.insert(next);
                    idx += 1;
                    coverage.visited += 1;
                    call_stack.push((next, 0, None));
                } else if on_stack.contains(&next) {
                    // Back edge: update lowlink.
                    let next_idx = index[&next];
                    let node_low = lowlink[&node];
                    lowlink.insert(node, node_low.min(next_idx));
                }
            } else {
                // All edges processed: pop from Tarjan stack if root.
                let node_low = lowlink[&node];
                let node_idx = index[&node];
                if node_low == node_idx {
                    // Root of an SCC.
                    let mut scc: Vec<i64> = Vec::new();
                    while let Some(top) = stack.pop() {
                        on_stack.remove(&top);
                        scc.push(top);
                        if top == node {
                            break;
                        }
                    }
                    scc.sort_unstable();

                    // Only report actual cycles: size > 1, or size == 1
                    // with a self-loop.
                    let is_cycle = scc.len() > 1 || {
                        let id = scc[0];
                        admissible_edges(store, id, req.tiers)?
                            .into_iter()
                            .any(|e| e.dst_id == id)
                    };

                    if is_cycle {
                        // Check component budget.
                        if components.len()
                            >= usize::try_from(req.budget.max_components).unwrap_or(usize::MAX)
                        {
                            coverage.stopped_by = Some(BudgetParameter::MaxComponents);
                            break 'outer;
                        }

                        let member_uids: Vec<String> = {
                            let mut uids = Vec::with_capacity(scc.len());
                            for &id in &scc {
                                uids.push(required_symbol(store, id)?.uid);
                            }
                            uids
                        };
                        let id = component_id(&member_uids);
                        let witness = match find_cycle_witness(
                            store,
                            &scc,
                            req.tiers,
                            clock,
                            req.budget.time_budget,
                        )? {
                            Some(w) => w,
                            None => {
                                coverage.stopped_by = Some(BudgetParameter::TimeBudgetMs);
                                break 'outer;
                            }
                        };
                        components.push(Component {
                            id,
                            members: scc,
                            witness,
                        });
                    }
                }

                // Update parent's lowlink.
                if let Some(&(parent, _, _)) = call_stack.last() {
                    let parent_low = lowlink[&parent];
                    lowlink.insert(parent, parent_low.min(node_low));
                }
            }
        }
    }

    // Sort components by id for stable output.
    components.sort_by(|a, b| a.id.cmp(&b.id));

    if coverage.stopped_by.is_none() {
        coverage.enumeration_exhausted = true;
    }

    Ok(Enumeration {
        components,
        coverage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SymbolKind;

    /// A clock that returns a scripted sequence of elapsed values, then
    /// repeats the last one.
    struct ScriptedClock {
        script: Vec<Duration>,
        calls: usize,
    }

    impl ScriptedClock {
        fn frozen() -> Self {
            ScriptedClock {
                script: vec![Duration::ZERO],
                calls: 0,
            }
        }
        fn script(script: &[u64]) -> Self {
            ScriptedClock {
                script: script.iter().map(|ms| Duration::from_millis(*ms)).collect(),
                calls: 0,
            }
        }
    }

    impl Clock for ScriptedClock {
        fn elapsed(&mut self) -> Duration {
            let ix = self.calls.min(self.script.len() - 1);
            self.calls += 1;
            self.script[ix]
        }
    }

    /// A graph under construction: node `i` is symbol `f{i}` in file
    /// `src/f{i}.rs`, one file per node.
    struct Fixture {
        store: GraphStore,
        ids: Vec<i64>,
    }

    fn fixture(nodes: usize) -> Fixture {
        let mut store = GraphStore::open_in_memory().unwrap();
        let mut ids = Vec::with_capacity(nodes);
        for i in 0..nodes {
            let path = format!("src/f{i}.rs");
            let file_id = store.replace_file(&path, "0", "rust").unwrap();
            let start = u32::try_from(10 * i + 1).unwrap();
            let id = store
                .insert_symbol(
                    file_id,
                    &format!("{path}#f{i}#function"),
                    &format!("f{i}"),
                    &format!("f{i}"),
                    SymbolKind::Function,
                    start,
                    start + 5,
                    "fn",
                )
                .unwrap();
            ids.push(id);
        }
        Fixture { store, ids }
    }

    impl Fixture {
        fn call(&self, from: usize, to: usize, tier: Tier) {
            self.store
                .insert_edge(&EdgeRow {
                    src_id: self.ids[from],
                    dst_id: self.ids[to],
                    kind: EdgeKind::Calls,
                    tier,
                    site_line: u32::try_from(10 * from + 3).unwrap(),
                    receiver: None,
                    callee: None,
                })
                .unwrap();
        }
        fn exact(&self, from: usize, to: usize) {
            self.call(from, to, Tier::Exact);
        }
        fn probable(&self, from: usize, to: usize) {
            self.call(from, to, Tier::Probable);
        }
        fn eval_with(
            &self,
            tiers: TierSelection,
            budget: Budget,
            clock: &mut dyn Clock,
        ) -> Enumeration {
            enumerate(&self.store, Request { tiers, budget }, clock).unwrap()
        }
        fn eval(&self) -> Enumeration {
            self.eval_with(
                TierSelection::Exact,
                unbounded(),
                &mut ScriptedClock::frozen(),
            )
        }
    }

    fn unbounded() -> Budget {
        Budget {
            max_nodes: 10_000,
            max_edges: 100_000,
            time_budget: Duration::from_secs(60),
            max_components: 1_000,
        }
    }

    fn budget_with(max_nodes: u32, max_edges: u32, max_components: u32) -> Budget {
        Budget {
            max_nodes,
            max_edges,
            time_budget: Duration::from_secs(60),
            max_components,
        }
    }

    // --- self-cycle ------------------------------------------------------

    #[test]
    fn self_cycle_is_reported() {
        let fx = fixture(1);
        fx.exact(0, 0);
        let result = fx.eval();
        assert!(result.coverage.enumeration_exhausted);
        assert_eq!(result.components.len(), 1);
        let comp = &result.components[0];
        assert_eq!(comp.members, vec![fx.ids[0]]);
        match &comp.witness {
            Witness::Cycle {
                probable_edges,
                edges,
            } => {
                assert_eq!(*probable_edges, 0);
                assert_eq!(edges.len(), 1);
                assert_eq!(edges[0].from.uid, edges[0].to.uid);
            }
            _ => panic!("expected cycle witness"),
        }
    }

    #[test]
    fn self_cycle_with_probable_edge_is_reported() {
        let fx = fixture(1);
        fx.probable(0, 0);
        let result = fx.eval_with(
            TierSelection::ExactAndProbable,
            unbounded(),
            &mut ScriptedClock::frozen(),
        );
        assert!(result.coverage.enumeration_exhausted);
        assert_eq!(result.components.len(), 1);
        match &result.components[0].witness {
            Witness::Cycle { probable_edges, .. } => assert_eq!(*probable_edges, 1),
            _ => panic!("expected cycle witness"),
        }
    }

    // --- multi-node cycle -------------------------------------------------

    #[test]
    fn two_node_cycle_is_reported() {
        let fx = fixture(2);
        fx.exact(0, 1);
        fx.exact(1, 0);
        let result = fx.eval();
        assert!(result.coverage.enumeration_exhausted);
        assert_eq!(result.components.len(), 1);
        let comp = &result.components[0];
        assert_eq!(comp.members.len(), 2);
        match &comp.witness {
            Witness::Cycle {
                probable_edges,
                edges,
            } => {
                assert_eq!(*probable_edges, 0);
                assert_eq!(edges.len(), 2);
                // The witness forms a closed loop.
                assert_eq!(edges[0].from.uid, edges[1].to.uid);
                assert_eq!(edges[0].to.uid, edges[1].from.uid);
            }
            _ => panic!("expected cycle witness"),
        }
    }

    #[test]
    fn three_node_cycle_is_reported() {
        let fx = fixture(3);
        fx.exact(0, 1);
        fx.exact(1, 2);
        fx.exact(2, 0);
        let result = fx.eval();
        assert!(result.coverage.enumeration_exhausted);
        assert_eq!(result.components.len(), 1);
        let comp = &result.components[0];
        assert_eq!(comp.members.len(), 3);
        match &comp.witness {
            Witness::Cycle { edges, .. } => {
                assert_eq!(edges.len(), 3);
            }
            _ => panic!("expected cycle witness"),
        }
    }

    // --- acyclic graph ----------------------------------------------------

    #[test]
    fn acyclic_graph_has_no_cycles() {
        let fx = fixture(3);
        fx.exact(0, 1);
        fx.exact(1, 2);
        let result = fx.eval();
        assert!(result.coverage.enumeration_exhausted);
        assert_eq!(result.components.len(), 0);
    }

    #[test]
    fn empty_graph_has_no_cycles() {
        let fx = fixture(0);
        let result = fx.eval();
        assert!(result.coverage.enumeration_exhausted);
        assert_eq!(result.components.len(), 0);
    }

    // --- ownership is not recursion ---------------------------------------

    #[test]
    fn has_method_ownership_does_not_create_cycle() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let file_id = store.replace_file("src/lib.rs", "0", "rust").unwrap();
        let class_id = store
            .insert_symbol(
                file_id,
                "src/lib.rs#MyClass#class",
                "MyClass",
                "MyClass",
                SymbolKind::Class,
                1,
                10,
                "class",
            )
            .unwrap();
        let method_id = store
            .insert_symbol(
                file_id,
                "src/lib.rs#MyClass::my_method#method",
                "my_method",
                "MyClass::my_method",
                SymbolKind::Method,
                3,
                8,
                "method",
            )
            .unwrap();
        store
            .insert_edge(&EdgeRow {
                src_id: class_id,
                dst_id: method_id,
                kind: EdgeKind::HasMethod,
                tier: Tier::Exact,
                site_line: 3,
                receiver: None,
                callee: None,
            })
            .unwrap();
        let result = enumerate(
            &store,
            Request {
                tiers: TierSelection::Exact,
                budget: unbounded(),
            },
            &mut ScriptedClock::frozen(),
        )
        .unwrap();
        assert!(result.coverage.enumeration_exhausted);
        assert_eq!(result.components.len(), 0);
    }

    // --- tier selection ---------------------------------------------------

    #[test]
    fn probable_edges_excluded_under_exact_tiers() {
        let fx = fixture(2);
        fx.probable(0, 1);
        fx.probable(1, 0);
        let result = fx.eval_with(
            TierSelection::Exact,
            unbounded(),
            &mut ScriptedClock::frozen(),
        );
        assert!(result.coverage.enumeration_exhausted);
        assert_eq!(result.components.len(), 0);
    }

    #[test]
    fn probable_edges_included_under_exact_and_probable() {
        let fx = fixture(2);
        fx.probable(0, 1);
        fx.probable(1, 0);
        let result = fx.eval_with(
            TierSelection::ExactAndProbable,
            unbounded(),
            &mut ScriptedClock::frozen(),
        );
        assert!(result.coverage.enumeration_exhausted);
        assert_eq!(result.components.len(), 1);
    }

    // --- budget caps ------------------------------------------------------

    #[test]
    fn node_budget_stops_enumeration() {
        let fx = fixture(3);
        fx.exact(0, 1);
        fx.exact(1, 2);
        fx.exact(2, 0);
        let result = fx.eval_with(
            TierSelection::Exact,
            budget_with(1, 100, 100),
            &mut ScriptedClock::frozen(),
        );
        assert!(!result.coverage.enumeration_exhausted);
        assert_eq!(result.coverage.stopped_by, Some(BudgetParameter::MaxNodes));
    }

    #[test]
    fn edge_budget_stops_enumeration() {
        let fx = fixture(3);
        fx.exact(0, 1);
        fx.exact(1, 2);
        fx.exact(2, 0);
        let result = fx.eval_with(
            TierSelection::Exact,
            budget_with(100, 1, 100),
            &mut ScriptedClock::frozen(),
        );
        assert!(!result.coverage.enumeration_exhausted);
        assert_eq!(result.coverage.stopped_by, Some(BudgetParameter::MaxEdges));
    }

    #[test]
    fn time_budget_stops_enumeration() {
        let fx = fixture(3);
        fx.exact(0, 1);
        fx.exact(1, 2);
        fx.exact(2, 0);
        // Scripted clock that immediately exceeds the time budget.
        let result = fx.eval_with(
            TierSelection::Exact,
            Budget {
                max_nodes: 100,
                max_edges: 100,
                time_budget: Duration::from_millis(0),
                max_components: 100,
            },
            &mut ScriptedClock::script(&[1]),
        );
        assert!(!result.coverage.enumeration_exhausted);
        assert_eq!(
            result.coverage.stopped_by,
            Some(BudgetParameter::TimeBudgetMs)
        );
    }

    #[test]
    fn component_budget_stops_enumeration() {
        let fx = fixture(4);
        // Two separate 2-node cycles.
        fx.exact(0, 1);
        fx.exact(1, 0);
        fx.exact(2, 3);
        fx.exact(3, 2);
        let result = fx.eval_with(
            TierSelection::Exact,
            budget_with(100, 100, 1),
            &mut ScriptedClock::frozen(),
        );
        assert!(!result.coverage.enumeration_exhausted);
        assert_eq!(
            result.coverage.stopped_by,
            Some(BudgetParameter::MaxComponents)
        );
        assert_eq!(result.components.len(), 1);
    }

    #[test]
    fn zero_node_budget_stops_immediately() {
        let fx = fixture(1);
        fx.exact(0, 0);
        let result = fx.eval_with(
            TierSelection::Exact,
            budget_with(0, 100, 100),
            &mut ScriptedClock::frozen(),
        );
        assert!(!result.coverage.enumeration_exhausted);
        assert_eq!(result.coverage.stopped_by, Some(BudgetParameter::MaxNodes));
        assert_eq!(result.components.len(), 0);
    }

    #[test]
    fn node_budget_equal_to_node_count_is_exhaustive() {
        // When max_nodes equals the exact node count, the enumeration
        // should still be exhaustive — the budget check fires only when
        // trying to admit a node beyond the cap.
        let fx = fixture(3);
        fx.exact(0, 1);
        fx.exact(1, 2);
        fx.exact(2, 0);
        let result = fx.eval_with(
            TierSelection::Exact,
            budget_with(3, 100, 100),
            &mut ScriptedClock::frozen(),
        );
        assert!(result.coverage.enumeration_exhausted);
        assert_eq!(result.components.len(), 1);
    }

    // --- stable ids and ordering ------------------------------------------

    #[test]
    fn component_ids_are_stable_across_runs() {
        let fx = fixture(3);
        fx.exact(0, 1);
        fx.exact(1, 2);
        fx.exact(2, 0);
        let r1 = fx.eval();
        let r2 = fx.eval();
        assert_eq!(r1.components[0].id, r2.components[0].id);
    }

    #[test]
    fn components_sorted_by_id() {
        let fx = fixture(6);
        // Two separate cycles: {0,1} and {2,3,4,5}.
        fx.exact(0, 1);
        fx.exact(1, 0);
        fx.exact(2, 3);
        fx.exact(3, 4);
        fx.exact(4, 5);
        fx.exact(5, 2);
        let result = fx.eval();
        assert_eq!(result.components.len(), 2);
        assert!(result.components[0].id < result.components[1].id);
    }

    // --- witness verification ---------------------------------------------

    #[test]
    fn witness_edges_are_reread_from_store() {
        let fx = fixture(2);
        fx.exact(0, 1);
        fx.exact(1, 0);
        let result = fx.eval();
        let comp = &result.components[0];
        match &comp.witness {
            Witness::Cycle { edges, .. } => {
                for hop in edges {
                    // Each hop's edge must exist in the store.
                    let src = &hop.from.uid;
                    let dst = &hop.to.uid;
                    let src_id = fx
                        .ids
                        .iter()
                        .find(|&&id| {
                            fx.store
                                .symbol_by_uid(src)
                                .unwrap()
                                .map(|s| s.id == id)
                                .unwrap_or(false)
                        })
                        .unwrap();
                    let dst_id = fx
                        .ids
                        .iter()
                        .find(|&&id| {
                            fx.store
                                .symbol_by_uid(dst)
                                .unwrap()
                                .map(|s| s.id == id)
                                .unwrap_or(false)
                        })
                        .unwrap();
                    let found = fx
                        .store
                        .edges_from(*src_id, Some(EdgeKind::Calls))
                        .unwrap()
                        .into_iter()
                        .any(|e| e.dst_id == *dst_id && e.tier == Tier::Exact);
                    assert!(found, "witness edge {src} -> {dst} not found in store");
                }
            }
            _ => panic!("expected cycle witness"),
        }
    }

    // --- unresolved calls -------------------------------------------------

    #[test]
    fn unresolved_calls_reported_in_coverage() {
        let fx = fixture(2);
        fx.exact(0, 1);
        fx.exact(1, 0);
        // Insert an unresolved call.
        let file_id = fx
            .store
            .symbol_by_uid("src/f0.rs#f0#function")
            .unwrap()
            .unwrap()
            .file_id;
        fx.store
            .insert_unresolved_call(file_id, "f1", None, 5, None, "calls")
            .unwrap();
        let result = fx.eval();
        assert_eq!(result.coverage.unresolved_same_name_sites, 1);
    }

    // --- non-callable symbols excluded ------------------------------------

    #[test]
    fn non_callable_symbols_excluded_from_cycles() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let file_id = store.replace_file("src/lib.rs", "0", "rust").unwrap();
        // Two structs that call each other (unusual but possible in some languages).
        let s0 = store
            .insert_symbol(
                file_id,
                "src/lib.rs#S0#struct",
                "S0",
                "S0",
                SymbolKind::Struct,
                1,
                5,
                "struct",
            )
            .unwrap();
        let s1 = store
            .insert_symbol(
                file_id,
                "src/lib.rs#S1#struct",
                "S1",
                "S1",
                SymbolKind::Struct,
                7,
                11,
                "struct",
            )
            .unwrap();
        store
            .insert_edge(&EdgeRow {
                src_id: s0,
                dst_id: s1,
                kind: EdgeKind::Calls,
                tier: Tier::Exact,
                site_line: 3,
                receiver: None,
                callee: None,
            })
            .unwrap();
        store
            .insert_edge(&EdgeRow {
                src_id: s1,
                dst_id: s0,
                kind: EdgeKind::Calls,
                tier: Tier::Exact,
                site_line: 9,
                receiver: None,
                callee: None,
            })
            .unwrap();
        let result = enumerate(
            &store,
            Request {
                tiers: TierSelection::Exact,
                budget: unbounded(),
            },
            &mut ScriptedClock::frozen(),
        )
        .unwrap();
        assert!(result.coverage.enumeration_exhausted);
        // Structs are not callable, so no cycle is reported.
        assert_eq!(result.components.len(), 0);
    }

    // --- diamond shape ----------------------------------------------------

    #[test]
    fn diamond_with_back_edge_forms_cycle() {
        let fx = fixture(4);
        // 0 -> 1, 0 -> 2, 1 -> 3, 2 -> 3, 3 -> 0 (back edge forms cycle).
        fx.exact(0, 1);
        fx.exact(0, 2);
        fx.exact(1, 3);
        fx.exact(2, 3);
        fx.exact(3, 0);
        let result = fx.eval();
        assert!(result.coverage.enumeration_exhausted);
        assert_eq!(result.components.len(), 1);
        assert_eq!(result.components[0].members.len(), 4);
    }

    #[test]
    fn diamond_without_back_edge_is_acyclic() {
        let fx = fixture(4);
        // 0 -> 1, 0 -> 2, 1 -> 3, 2 -> 3 (no back edge).
        fx.exact(0, 1);
        fx.exact(0, 2);
        fx.exact(1, 3);
        fx.exact(2, 3);
        let result = fx.eval();
        assert!(result.coverage.enumeration_exhausted);
        assert_eq!(result.components.len(), 0);
    }

    // --- multiple separate cycles -----------------------------------------

    #[test]
    fn multiple_separate_cycles_all_reported() {
        let fx = fixture(6);
        // Cycle 1: {0, 1}
        fx.exact(0, 1);
        fx.exact(1, 0);
        // Cycle 2: {2, 3, 4}
        fx.exact(2, 3);
        fx.exact(3, 4);
        fx.exact(4, 2);
        // Cycle 3: {5} self-loop
        fx.exact(5, 5);
        let result = fx.eval();
        assert!(result.coverage.enumeration_exhausted);
        assert_eq!(result.components.len(), 3);
    }

    // --- coverage fields --------------------------------------------------

    #[test]
    fn coverage_reports_tiers_and_edge_kinds() {
        let fx = fixture(2);
        fx.exact(0, 1);
        fx.exact(1, 0);
        let result = fx.eval();
        assert_eq!(result.coverage.tiers, vec![Tier::Exact]);
    }

    #[test]
    fn coverage_visited_and_edges_followed() {
        let fx = fixture(3);
        fx.exact(0, 1);
        fx.exact(1, 2);
        fx.exact(2, 0);
        let result = fx.eval();
        assert!(result.coverage.visited >= 3);
        assert!(result.coverage.edges_followed >= 3);
    }
}
