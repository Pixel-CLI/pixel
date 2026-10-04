// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Bounded reachability evaluation with witnesses for `pixel evaluate`.
//!
//! One proposition in ("some source reaches some target along the selected
//! call relation"), one of three statuses out, and the evidence that
//! produced it. The evaluator is pure over a [`GraphStore`]: no snapshot
//! check, no symbol-name resolution, no rendering. Those belong to the
//! daemon op and the CLI that wrap it.
//!
//! The decision rule, with `W` = a witness path was actually obtained and
//! re-read from the store, `X` = the traversal exhausted the reachable
//! region (visited set closed under the selected relation, no frontier node
//! dropped by the depth cap or the time budget):
//!
//! ```text
//! Established        ⇔ W
//! AbsentInSnapshot   ⇔ ¬W ∧ X
//! Unknown            otherwise (a budget dropped a frontier node)
//! ```
//!
//! Existence of a path is not enough for `Established`: discovery is
//! required, so a path beyond the depth cap yields `Unknown`, never a false
//! positive. `AbsentInSnapshot` speaks about the stored relation only
//! (edge kinds `calls` + `has_method`, the selected tiers); the caller adds
//! the snapshot identity and the extraction limits that bound that claim.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use rusqlite::params;
use serde::Serialize;

use crate::impact::{file_path_by_id, symbol_by_id};
use crate::store::{EdgeKind, EdgeRow, GraphStore, StoreError, SymbolRow, Tier};

/// The edge kinds every evaluation walks: direct calls and the class→method
/// ownership edge that lets a call on a receiver reach the method body.
pub const EDGE_KINDS: [EdgeKind; 2] = [EdgeKind::Calls, EdgeKind::HasMethod];

/// Which way the traversal walks the call relation from the sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Traversal {
    /// Follow outgoing edges: "what do the sources call, transitively".
    Callees,
    /// Follow incoming edges: "who calls the sources, transitively".
    Callers,
}

/// Which stored edge tiers form the relation being evaluated. `--tiers`
/// selects a relation, never a confidence threshold: there is no automatic
/// widening from `Exact` to `ExactAndProbable`.
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

/// The two caps a traversal runs under. Each one that fires is reported
/// by name in the `Unknown` reason so the caller can raise exactly it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Maximum traversal depth; a node at this depth is not expanded.
    pub max_depth: u32,
    /// Wall-clock budget for the traversal itself, measured by the
    /// injected [`Clock`].
    pub time_budget: Duration,
}

/// Elapsed time since the evaluation started. Injected so tests drive the
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

/// What to evaluate: `sources` reach `targets` along `traversal`, over the
/// edges `tiers` admits, within `budget`.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// Symbol ids the traversal starts from (multi-source).
    pub sources: &'a [i64],
    /// Symbol ids that end the traversal when reached.
    pub targets: &'a [i64],
    pub traversal: Traversal,
    pub tiers: TierSelection,
    pub budget: Budget,
}

/// Which budget parameter stopped the traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetParameter {
    MaxDepth,
    TimeBudgetMs,
}

/// Why this layer could not conclude. The only reason it produces itself is
/// an exhausted budget; resolution and snapshot reasons come from callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum UnknownReason {
    /// A depth or time cap dropped at least one frontier node.
    TraversalBudgetExhausted {
        parameter: BudgetParameter,
        /// The cap's value as configured (depth, or milliseconds).
        current: u64,
    },
}

/// The conclusion of an evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    /// A witness path was obtained and re-read from the store.
    Established,
    /// No witness, and the reachable region was exhausted: absent in the
    /// stored relation (and only there).
    AbsentInSnapshot,
    /// The traversal could not conclude.
    Unknown { reason: UnknownReason },
}

/// How much of the reachable region the traversal covered, and under what
/// relation. Every cap that fired is named here; none is ever silent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Coverage {
    /// True when the visited set is closed under the relation and no
    /// frontier node was dropped; the precondition of `AbsentInSnapshot`.
    pub traversal_exhausted: bool,
    pub depth_cap: u32,
    /// True only when a node at the depth cap still had an admissible,
    /// unvisited neighbour. A leaf at the cap does not set it.
    pub depth_cap_dropped_frontier: bool,
    pub time_budget_ms: u64,
    pub time_budget_hit: bool,
    /// Nodes dequeued (expanded or refused expansion) by the traversal.
    pub visited: u64,
    /// Unresolved call sites whose callee text equals a source or target
    /// name: same-name calls the resolver could not attach to an edge, so
    /// the stored relation may be missing edges into or out of them.
    pub unresolved_same_name_sites: u64,
    pub tiers: Vec<Tier>,
    pub edge_kinds: Vec<EdgeKind>,
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

/// One import statement linking the caller's file to the callee's file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportPremise {
    pub from_path: String,
    pub spec: String,
    pub to_path: String,
}

/// What the store holds in support of a `Probable` edge.
///
/// A `Probable` edge was resolved by name uniqueness inside an
/// import-connected component; the store does not keep the component id,
/// so the premises listed here are the import rows that connect the
/// caller's file to the callee's file, when any exist. `available` is
/// false when none do: the edge is still in the store, but its resolution
/// cannot be re-derived from the witness alone. Nothing is fabricated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Premises {
    pub available: bool,
    pub imports: Vec<ImportPremise>,
}

/// Fixed marker: `from` is always the caller and `to` the callee, whatever
/// the traversal direction was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CallDirection {
    #[serde(rename = "caller→callee")]
    CallerToCallee,
}

/// One hop of a witness path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WitnessEdge {
    /// The caller.
    pub from: SymbolRef,
    /// The callee.
    pub to: SymbolRef,
    pub edge: EdgeInfo,
    pub call_direction: CallDirection,
    /// 1-based position along the traversal from the source that reached
    /// the target. Under `Callers` the traversal walks callee→caller, so
    /// step 1's `to` is the source.
    pub traversal_step: u32,
    /// Empty for `Exact` edges; see [`Premises`] for `Probable` ones.
    pub premises: Option<Premises>,
}

/// The evidence behind a status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Witness {
    /// A non-empty edge list from a source to a target.
    Path {
        probable_edges: u32,
        edges: Vec<WitnessEdge>,
    },
    /// A source is itself a target: the zero-length path, with the symbol.
    Identity { symbol: SymbolRef },
    /// No witness; only with `AbsentInSnapshot` or `Unknown`.
    None,
}

/// Status, coverage and witness of one evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Evaluation {
    pub status: Status,
    pub traversal: Traversal,
    pub coverage: Coverage,
    pub witness: Witness,
}

/// Whether a node at `depth` may still be expanded under `max_depth`.
fn depth_allows_expansion(depth: u32, max_depth: u32) -> bool {
    depth < max_depth
}

/// Whether `elapsed` has gone past `budget`. Exactly at the budget is not
/// over it.
fn over_time(elapsed: Duration, budget: Duration) -> bool {
    elapsed > budget
}

/// The neighbour an edge leads to under `traversal`.
fn neighbour(edge: &EdgeRow, traversal: Traversal) -> i64 {
    match traversal {
        Traversal::Callees => edge.dst_id,
        Traversal::Callers => edge.src_id,
    }
}

/// Admissible edges of `id` under `traversal` and `tiers`, over
/// [`EDGE_KINDS`], in a total order (edge kind, then neighbour id, then site
/// line) so the witness is byte-stable.
fn admissible_edges(
    store: &GraphStore,
    id: i64,
    traversal: Traversal,
    tiers: TierSelection,
) -> Result<Vec<EdgeRow>, StoreError> {
    let mut out = Vec::new();
    for kind in EDGE_KINDS {
        let edges = match traversal {
            Traversal::Callees => store.edges_from(id, Some(kind))?,
            Traversal::Callers => store.edges_to(id, Some(kind))?,
        };
        out.extend(edges.into_iter().filter(|e| tiers.admits(e.tier)));
    }
    out.sort_by_key(|e| (e.kind.as_str(), neighbour(e, traversal), e.site_line));
    Ok(out)
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

/// Import rows of `from_file` whose spec resolved to `to_file`.
fn import_premises(
    store: &GraphStore,
    from_file: i64,
    to_file: i64,
) -> Result<Vec<ImportPremise>, StoreError> {
    let from_path = file_path_by_id(store, from_file)?;
    let to_path = file_path_by_id(store, to_file)?;
    let mut stmt = store.conn().prepare(
        "SELECT DISTINCT spec FROM imports WHERE file_id = ?1 AND resolved_file_id = ?2 ORDER BY spec",
    )?;
    let specs = stmt.query_map(params![from_file, to_file], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for spec in specs {
        out.push(ImportPremise {
            from_path: from_path.clone(),
            spec: spec?,
            to_path: to_path.clone(),
        });
    }
    Ok(out)
}

/// Unresolved call sites named like any of `names` (deduplicated).
fn unresolved_same_name(store: &GraphStore, names: &[&str]) -> Result<u64, StoreError> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut total = 0u64;
    for name in names {
        if seen.insert(*name) {
            total += store.envelope_for_name(name)?.unresolved_same_name;
        }
    }
    Ok(total)
}

/// Build the witness hop for a stored edge at `step`.
fn witness_edge(store: &GraphStore, edge: &EdgeRow, step: u32) -> Result<WitnessEdge, StoreError> {
    let caller = required_symbol(store, edge.src_id)?;
    let callee = required_symbol(store, edge.dst_id)?;
    let premises = match edge.tier {
        Tier::Exact => None,
        Tier::Probable => {
            let imports = import_premises(store, caller.file_id, callee.file_id)?;
            Some(Premises {
                available: !imports.is_empty(),
                imports,
            })
        }
    };
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
        call_direction: CallDirection::CallerToCallee,
        traversal_step: step,
        premises,
    })
}

/// Evaluate `req` against `store`, timing the traversal with `clock`.
///
/// Sources and targets are symbol ids that must exist in the store; the
/// caller resolved them (a missing id is a `StoreError`, not an `Unknown`:
/// resolution failures have their own reasons upstream).
///
/// # Errors
///
/// Any store read failure, and a source or target id with no symbol row.
pub fn evaluate(
    store: &GraphStore,
    req: Request<'_>,
    clock: &mut dyn Clock,
) -> Result<Evaluation, StoreError> {
    let mut sources: Vec<i64> = req.sources.to_vec();
    sources.sort_unstable();
    sources.dedup();
    let target_set: HashSet<i64> = req.targets.iter().copied().collect();

    let mut names: Vec<String> = Vec::new();
    for id in sources.iter().chain(req.targets.iter()) {
        names.push(required_symbol(store, *id)?.name);
    }
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let unresolved = unresolved_same_name(store, &name_refs)?;

    let time_budget_ms = u64::try_from(req.budget.time_budget.as_millis()).unwrap_or(u64::MAX);
    let mut coverage = Coverage {
        traversal_exhausted: false,
        depth_cap: req.budget.max_depth,
        depth_cap_dropped_frontier: false,
        time_budget_ms,
        time_budget_hit: false,
        visited: 0,
        unresolved_same_name_sites: unresolved,
        tiers: req.tiers.tiers().to_vec(),
        edge_kinds: EDGE_KINDS.to_vec(),
    };

    // Zero-length path: a source is a target. The smallest id keeps the
    // answer byte-stable across runs.
    if let Some(id) = sources.iter().copied().find(|s| target_set.contains(s)) {
        let sym = required_symbol(store, id)?;
        coverage.traversal_exhausted = true;
        return Ok(Evaluation {
            status: Status::Established,
            traversal: req.traversal,
            coverage,
            witness: Witness::Identity {
                symbol: symbol_ref(store, &sym)?,
            },
        });
    }

    // parent[node] = (previous node, edge that reached it)
    let mut parent: HashMap<i64, (i64, EdgeRow)> = HashMap::new();
    let mut depth_of: HashMap<i64, u32> = HashMap::new();
    let mut queue: VecDeque<i64> = VecDeque::new();
    for &s in &sources {
        depth_of.insert(s, 0);
        queue.push_back(s);
    }
    let mut reached: Option<i64> = None;

    'bfs: while let Some(id) = queue.pop_front() {
        if over_time(clock.elapsed(), req.budget.time_budget) {
            // The dequeued node (and anything behind it) is dropped.
            coverage.time_budget_hit = true;
            break 'bfs;
        }
        coverage.visited += 1;
        let depth = depth_of[&id];
        let edges = admissible_edges(store, id, req.traversal, req.tiers)?;
        if !depth_allows_expansion(depth, req.budget.max_depth) {
            if edges
                .iter()
                .any(|e| !depth_of.contains_key(&neighbour(e, req.traversal)))
            {
                coverage.depth_cap_dropped_frontier = true;
            }
            continue;
        }
        for e in edges {
            let next = neighbour(&e, req.traversal);
            if depth_of.contains_key(&next) {
                continue;
            }
            depth_of.insert(next, depth + 1);
            parent.insert(next, (id, e));
            if target_set.contains(&next) {
                reached = Some(next);
                break 'bfs;
            }
            queue.push_back(next);
        }
    }

    if let Some(target) = reached {
        // Walk back to the source, then forward to number the steps.
        let mut chain: Vec<EdgeRow> = Vec::new();
        let mut cur = target;
        while let Some((prev, edge)) = parent.get(&cur) {
            chain.push(edge.clone());
            cur = *prev;
        }
        chain.reverse();
        let mut edges = Vec::with_capacity(chain.len());
        let mut probable_edges = 0u32;
        for (i, edge) in chain.iter().enumerate() {
            let stored = reread_edge(store, edge)?
                .ok_or(StoreError::Sql(rusqlite::Error::QueryReturnedNoRows))?;
            if stored.tier == Tier::Probable {
                probable_edges += 1;
            }
            let step = u32::try_from(i + 1).unwrap_or(u32::MAX);
            edges.push(witness_edge(store, &stored, step)?);
        }
        return Ok(Evaluation {
            status: Status::Established,
            traversal: req.traversal,
            coverage,
            witness: Witness::Path {
                probable_edges,
                edges,
            },
        });
    }

    let exhausted = !coverage.depth_cap_dropped_frontier && !coverage.time_budget_hit;
    coverage.traversal_exhausted = exhausted;
    let status = if exhausted {
        Status::AbsentInSnapshot
    } else if coverage.time_budget_hit {
        Status::Unknown {
            reason: UnknownReason::TraversalBudgetExhausted {
                parameter: BudgetParameter::TimeBudgetMs,
                current: time_budget_ms,
            },
        }
    } else {
        Status::Unknown {
            reason: UnknownReason::TraversalBudgetExhausted {
                parameter: BudgetParameter::MaxDepth,
                current: u64::from(req.budget.max_depth),
            },
        }
    };
    Ok(Evaluation {
        status,
        traversal: req.traversal,
        coverage,
        witness: Witness::None,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::time::Duration;

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
    /// `src/f{i}.rs`, one file per node so `Probable` premises can be
    /// asserted per file pair.
    struct Fixture {
        store: GraphStore,
        ids: Vec<i64>,
        files: Vec<i64>,
    }

    fn fixture(nodes: usize) -> Fixture {
        let mut store = GraphStore::open_in_memory().unwrap();
        let mut ids = Vec::with_capacity(nodes);
        let mut files = Vec::with_capacity(nodes);
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
            files.push(file_id);
        }
        Fixture { store, ids, files }
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
        fn eval_with(
            &self,
            sources: &[usize],
            targets: &[usize],
            traversal: Traversal,
            tiers: TierSelection,
            budget: Budget,
            clock: &mut dyn Clock,
        ) -> Evaluation {
            let s: Vec<i64> = sources.iter().map(|i| self.ids[*i]).collect();
            let t: Vec<i64> = targets.iter().map(|i| self.ids[*i]).collect();
            evaluate(
                &self.store,
                Request {
                    sources: &s,
                    targets: &t,
                    traversal,
                    tiers,
                    budget,
                },
                clock,
            )
            .unwrap()
        }
        fn eval(&self, sources: &[usize], targets: &[usize], traversal: Traversal) -> Evaluation {
            self.eval_with(
                sources,
                targets,
                traversal,
                TierSelection::Exact,
                unbounded(),
                &mut ScriptedClock::frozen(),
            )
        }
    }

    fn unbounded() -> Budget {
        Budget {
            max_depth: 64,
            time_budget: Duration::from_secs(60),
        }
    }

    fn depth(max_depth: u32) -> Budget {
        Budget {
            max_depth,
            time_budget: Duration::from_secs(60),
        }
    }

    fn unknown_depth(current: u64) -> Status {
        Status::Unknown {
            reason: UnknownReason::TraversalBudgetExhausted {
                parameter: BudgetParameter::MaxDepth,
                current,
            },
        }
    }

    // --- the independent oracle -------------------------------------------

    /// Naive transitive closure over an explicit edge list: `reach[a][b]`
    /// is true when `b` is reachable from `a` in one or more hops. Shares
    /// nothing with the evaluator.
    fn closure(nodes: usize, edges: &[(usize, usize)]) -> Vec<Vec<bool>> {
        let mut reach = vec![vec![false; nodes]; nodes];
        for &(a, b) in edges {
            reach[a][b] = true;
        }
        for k in 0..nodes {
            for i in 0..nodes {
                for j in 0..nodes {
                    if reach[i][k] && reach[k][j] {
                        reach[i][j] = true;
                    }
                }
            }
        }
        reach
    }

    /// Does any source reach the target, in the oracle's terms? Identity
    /// counts (zero-length path).
    fn oracle_reaches(
        reach: &[Vec<bool>],
        sources: &[usize],
        target: usize,
        traversal: Traversal,
    ) -> bool {
        sources.iter().any(|&s| {
            s == target
                || match traversal {
                    Traversal::Callees => reach[s][target],
                    Traversal::Callers => reach[target][s],
                }
        })
    }

    /// Every ordered pair of nodes (self-loops included when asked), in a
    /// fixed order, so a graph is a number in base `base`: digit 0 = no
    /// edge, 1 = exact, 2 = probable (base 3 only).
    fn edge_slots(nodes: usize, self_loops: bool) -> Vec<(usize, usize)> {
        let mut slots = Vec::new();
        for a in 0..nodes {
            for b in 0..nodes {
                if self_loops || a != b {
                    slots.push((a, b));
                }
            }
        }
        slots
    }

    fn graph_from_code(
        nodes: usize,
        self_loops: bool,
        base: u64,
        mut code: u64,
    ) -> (Fixture, Vec<(usize, usize, Tier)>) {
        let fx = fixture(nodes);
        let mut edges = Vec::new();
        for (a, b) in edge_slots(nodes, self_loops) {
            let digit = code % base;
            code /= base;
            let tier = match digit {
                0 => continue,
                1 => Tier::Exact,
                _ => Tier::Probable,
            };
            fx.call(a, b, tier);
            edges.push((a, b, tier));
        }
        (fx, edges)
    }

    fn check_against_oracle(nodes: usize, self_loops: bool, base: u64, code: u64) {
        let (fx, edges) = graph_from_code(nodes, self_loops, base, code);
        let source_sets: Vec<Vec<usize>> = {
            let mut sets: Vec<Vec<usize>> = (0..nodes).map(|s| vec![s]).collect();
            if nodes >= 2 {
                sets.push(vec![0, 1]);
            }
            sets
        };
        for tiers in [TierSelection::Exact, TierSelection::ExactAndProbable] {
            let admitted: Vec<(usize, usize)> = edges
                .iter()
                .filter(|(_, _, t)| tiers.admits(*t))
                .map(|(a, b, _)| (*a, *b))
                .collect();
            let reach = closure(nodes, &admitted);
            for traversal in [Traversal::Callees, Traversal::Callers] {
                for sources in &source_sets {
                    for target in 0..nodes {
                        let ev = fx.eval_with(
                            sources,
                            &[target],
                            traversal,
                            tiers,
                            unbounded(),
                            &mut ScriptedClock::frozen(),
                        );
                        let expected = oracle_reaches(&reach, sources, target, traversal);
                        let got = match ev.status {
                            Status::Established => true,
                            Status::AbsentInSnapshot => false,
                            Status::Unknown { .. } => {
                                panic!("unbounded evaluation must conclude: {ev:?}")
                            }
                        };
                        assert_eq!(
                            got, expected,
                            "graph {code} nodes {nodes} {traversal:?} {tiers:?} {sources:?}->{target}: {ev:?}"
                        );
                        // A negative under an unbounded budget is exhaustive.
                        if !expected {
                            assert!(ev.coverage.traversal_exhausted, "{ev:?}");
                            assert_eq!(ev.witness, Witness::None);
                        }
                        // A positive carries a witness whose ends are the
                        // source and the target, in call order.
                        if expected {
                            match &ev.witness {
                                Witness::Identity { symbol } => {
                                    assert!(sources.contains(&target));
                                    assert_eq!(
                                        symbol.uid,
                                        format!("src/f{target}.rs#f{target}#function")
                                    );
                                }
                                Witness::Path {
                                    edges,
                                    probable_edges,
                                } => {
                                    assert!(!edges.is_empty(), "path witness must not be empty");
                                    let (first_end, last_end) = match traversal {
                                        Traversal::Callees => {
                                            (&edges[0].from, &edges[edges.len() - 1].to)
                                        }
                                        Traversal::Callers => {
                                            (&edges[0].to, &edges[edges.len() - 1].from)
                                        }
                                    };
                                    let src_uids: HashSet<String> = sources
                                        .iter()
                                        .map(|s| format!("src/f{s}.rs#f{s}#function"))
                                        .collect();
                                    assert!(src_uids.contains(&first_end.uid), "{ev:?}");
                                    assert_eq!(
                                        last_end.uid,
                                        format!("src/f{target}.rs#f{target}#function")
                                    );
                                    let probable = u32::try_from(
                                        edges
                                            .iter()
                                            .filter(|e| e.edge.tier == Tier::Probable)
                                            .count(),
                                    )
                                    .unwrap();
                                    assert_eq!(*probable_edges, probable);
                                    if tiers == TierSelection::Exact {
                                        assert_eq!(
                                            probable, 0,
                                            "exact relation admits no probable edge"
                                        );
                                    }
                                    for (i, e) in edges.iter().enumerate() {
                                        assert_eq!(e.traversal_step, u32::try_from(i + 1).unwrap());
                                        assert_eq!(e.call_direction, CallDirection::CallerToCallee);
                                    }
                                }
                                Witness::None => panic!("established without witness: {ev:?}"),
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn evaluator_should_agree_with_transitive_closure_on_every_three_node_exact_graph() {
        // 9 slots (self-loops included), edge present or absent: 512 graphs,
        // every one exhaustively checked for both traversals.
        for code in 0..2u64.pow(9) {
            check_against_oracle(3, true, 2, code);
        }
    }

    #[test]
    fn evaluator_should_agree_with_transitive_closure_on_sampled_four_node_tiered_graphs() {
        // 12 slots (no self-loops), 3 states each → 3^12 graphs; a fixed LCG
        // samples them so the run is reproducible and stays far under the
        // mutants timeout.
        let mut x: u64 = 0x9E37_79B9;
        for _ in 0..80 {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            check_against_oracle(4, false, 3, (x >> 20) % 3u64.pow(12));
        }
    }

    // --- abstention fixtures ------------------------------------------------

    #[test]
    fn depth_cap_should_yield_unknown_when_a_frontier_node_is_dropped() {
        // 0→1→2→3, target 4 unreachable; with max_depth 2 node 2 still has
        // the unvisited neighbour 3, so the cap dropped a frontier node.
        let fx = fixture(5);
        fx.exact(0, 1);
        fx.exact(1, 2);
        fx.exact(2, 3);
        let ev = fx.eval_with(
            &[0],
            &[4],
            Traversal::Callees,
            TierSelection::Exact,
            depth(2),
            &mut ScriptedClock::frozen(),
        );
        assert_eq!(ev.status, unknown_depth(2));
        assert!(ev.coverage.depth_cap_dropped_frontier);
        assert!(!ev.coverage.traversal_exhausted);
        assert!(!ev.coverage.time_budget_hit);
        assert_eq!(ev.witness, Witness::None);
    }

    #[test]
    fn depth_cap_should_leave_traversal_exhaustive_when_the_node_at_the_cap_is_a_leaf() {
        // 0→1→2, target 3 unreachable; node 2 sits at max_depth 2 but has
        // no edges: nothing was dropped, the negative is exhaustive.
        let fx = fixture(4);
        fx.exact(0, 1);
        fx.exact(1, 2);
        let ev = fx.eval_with(
            &[0],
            &[3],
            Traversal::Callees,
            TierSelection::Exact,
            depth(2),
            &mut ScriptedClock::frozen(),
        );
        assert_eq!(ev.status, Status::AbsentInSnapshot);
        assert!(!ev.coverage.depth_cap_dropped_frontier);
        assert!(ev.coverage.traversal_exhausted);
    }

    #[test]
    fn depth_cap_should_leave_traversal_exhaustive_when_the_node_at_the_cap_only_points_back() {
        // 0→1→2→0: node 2 at depth 2 only points to the visited 0.
        let fx = fixture(4);
        fx.exact(0, 1);
        fx.exact(1, 2);
        fx.exact(2, 0);
        let ev = fx.eval_with(
            &[0],
            &[3],
            Traversal::Callees,
            TierSelection::Exact,
            depth(2),
            &mut ScriptedClock::frozen(),
        );
        assert_eq!(ev.status, Status::AbsentInSnapshot);
        assert!(!ev.coverage.depth_cap_dropped_frontier);
    }

    #[test]
    fn witness_should_be_established_when_the_target_sits_exactly_at_the_depth_cap() {
        // 0→1→2, target 2, max_depth 2: reached as the neighbour of node 1
        // (depth 1), so discovered within the cap.
        let fx = fixture(3);
        fx.exact(0, 1);
        fx.exact(1, 2);
        let ev = fx.eval_with(
            &[0],
            &[2],
            Traversal::Callees,
            TierSelection::Exact,
            depth(2),
            &mut ScriptedClock::frozen(),
        );
        assert_eq!(ev.status, Status::Established);
        match ev.witness {
            Witness::Path { edges, .. } => assert_eq!(edges.len(), 2),
            other => panic!("expected a path witness, got {other:?}"),
        }
    }

    #[test]
    fn target_one_past_the_depth_cap_should_be_unknown_not_absent() {
        // 0→1→2→3, target 3, max_depth 2: node 2 cannot be expanded and has
        // the unvisited neighbour 3. The path exists but was not discovered.
        let fx = fixture(4);
        fx.exact(0, 1);
        fx.exact(1, 2);
        fx.exact(2, 3);
        let ev = fx.eval_with(
            &[0],
            &[3],
            Traversal::Callees,
            TierSelection::Exact,
            depth(2),
            &mut ScriptedClock::frozen(),
        );
        assert_eq!(ev.status, unknown_depth(2));
    }

    #[test]
    fn time_budget_should_yield_unknown_when_it_expires_mid_traversal() {
        // 0→1→2→3, target 3. The clock reads 0 ms on the first dequeue and
        // 51 ms on the second, past a 50 ms budget: node 1 is dropped.
        let fx = fixture(4);
        fx.exact(0, 1);
        fx.exact(1, 2);
        fx.exact(2, 3);
        let ev = fx.eval_with(
            &[0],
            &[3],
            Traversal::Callees,
            TierSelection::Exact,
            Budget {
                max_depth: 64,
                time_budget: Duration::from_millis(50),
            },
            &mut ScriptedClock::script(&[0, 51]),
        );
        assert_eq!(
            ev.status,
            Status::Unknown {
                reason: UnknownReason::TraversalBudgetExhausted {
                    parameter: BudgetParameter::TimeBudgetMs,
                    current: 50,
                },
            }
        );
        assert!(ev.coverage.time_budget_hit);
        assert!(!ev.coverage.traversal_exhausted);
        assert_eq!(ev.coverage.visited, 1, "only the first node was expanded");
    }

    #[test]
    fn time_budget_should_not_fire_when_elapsed_equals_the_budget() {
        // Exactly at the budget is not over it: the traversal completes.
        let fx = fixture(3);
        fx.exact(0, 1);
        fx.exact(1, 2);
        let ev = fx.eval_with(
            &[0],
            &[2],
            Traversal::Callees,
            TierSelection::Exact,
            Budget {
                max_depth: 64,
                time_budget: Duration::from_millis(50),
            },
            &mut ScriptedClock::script(&[50]),
        );
        assert_eq!(ev.status, Status::Established);
        assert!(!ev.coverage.time_budget_hit);
    }

    #[test]
    fn over_time_should_be_strict() {
        let b = Duration::from_millis(50);
        assert!(!over_time(Duration::from_millis(49), b));
        assert!(!over_time(Duration::from_millis(50), b));
        assert!(over_time(Duration::from_millis(51), b));
    }

    #[test]
    fn depth_allows_expansion_should_stop_at_the_cap() {
        assert!(depth_allows_expansion(0, 1));
        assert!(depth_allows_expansion(1, 2));
        assert!(!depth_allows_expansion(2, 2));
        assert!(!depth_allows_expansion(3, 2));
    }

    // --- tiers ----------------------------------------------------------------

    #[test]
    fn probable_only_path_should_be_absent_under_exact_and_established_when_widened() {
        // 0 ─probable→ 1 ─exact→ 2, target 2.
        let fx = fixture(3);
        fx.call(0, 1, Tier::Probable);
        fx.exact(1, 2);
        let strict = fx.eval_with(
            &[0],
            &[2],
            Traversal::Callees,
            TierSelection::Exact,
            unbounded(),
            &mut ScriptedClock::frozen(),
        );
        assert_eq!(strict.status, Status::AbsentInSnapshot);
        assert!(strict.coverage.traversal_exhausted);
        assert_eq!(strict.coverage.tiers, vec![Tier::Exact]);

        let wide = fx.eval_with(
            &[0],
            &[2],
            Traversal::Callees,
            TierSelection::ExactAndProbable,
            unbounded(),
            &mut ScriptedClock::frozen(),
        );
        assert_eq!(wide.status, Status::Established);
        assert_eq!(wide.coverage.tiers, vec![Tier::Exact, Tier::Probable]);
        match &wide.witness {
            Witness::Path {
                probable_edges,
                edges,
            } => {
                assert_eq!(*probable_edges, 1);
                assert_eq!(edges[0].edge.tier, Tier::Probable);
                assert_eq!(edges[1].edge.tier, Tier::Exact);
            }
            other => panic!("expected a path witness, got {other:?}"),
        }
    }

    #[test]
    fn tier_selection_should_admit_only_the_named_tiers() {
        assert!(TierSelection::Exact.admits(Tier::Exact));
        assert!(!TierSelection::Exact.admits(Tier::Probable));
        assert!(TierSelection::ExactAndProbable.admits(Tier::Exact));
        assert!(TierSelection::ExactAndProbable.admits(Tier::Probable));
    }

    // --- witness shape --------------------------------------------------------

    #[test]
    fn identity_witness_should_carry_the_symbol_when_a_source_is_a_target() {
        let fx = fixture(2);
        fx.exact(0, 1);
        let ev = fx.eval(&[1, 0], &[0], Traversal::Callees);
        assert_eq!(ev.status, Status::Established);
        assert_eq!(
            ev.witness,
            Witness::Identity {
                symbol: SymbolRef {
                    uid: "src/f0.rs#f0#function".to_string(),
                    path: "src/f0.rs".to_string(),
                    lines: [1, 6],
                },
            }
        );
        assert!(ev.coverage.traversal_exhausted);
    }

    #[test]
    fn identity_none_and_path_witnesses_should_be_distinct_values() {
        let fx = fixture(3);
        fx.exact(0, 1);
        let identity = fx.eval(&[0], &[0], Traversal::Callees).witness;
        let path = fx.eval(&[0], &[1], Traversal::Callees).witness;
        let none = fx.eval(&[0], &[2], Traversal::Callees).witness;
        assert!(matches!(identity, Witness::Identity { .. }));
        assert!(matches!(path, Witness::Path { ref edges, .. } if !edges.is_empty()));
        assert_eq!(none, Witness::None);
        assert_ne!(identity, none);
        assert_ne!(path, none);
        assert_ne!(identity, path);
    }

    #[test]
    fn witness_edges_should_match_the_stored_edge_site_and_tier() {
        // Under Callers the traversal walks 2 → 1 → 0 but each hop still
        // names the caller as `from`.
        let fx = fixture(3);
        fx.exact(0, 1);
        fx.call(1, 2, Tier::Probable);
        let ev = fx.eval_with(
            &[2],
            &[0],
            Traversal::Callers,
            TierSelection::ExactAndProbable,
            unbounded(),
            &mut ScriptedClock::frozen(),
        );
        assert_eq!(ev.status, Status::Established);
        let Witness::Path { edges, .. } = &ev.witness else {
            panic!("expected a path witness, got {:?}", ev.witness);
        };
        assert_eq!(edges.len(), 2);
        // step 1: hop out of the source 2, i.e. the stored edge 1→2
        assert_eq!(edges[0].traversal_step, 1);
        assert_eq!(edges[0].from.uid, "src/f1.rs#f1#function");
        assert_eq!(edges[0].to.uid, "src/f2.rs#f2#function");
        assert_eq!(edges[0].edge.tier, Tier::Probable);
        assert_eq!(edges[0].edge.kind, EdgeKind::Calls);
        assert_eq!(
            edges[0].edge.site,
            CallSite {
                path: "src/f1.rs".to_string(),
                line: 13,
            }
        );
        // step 2: the stored edge 0→1
        assert_eq!(edges[1].traversal_step, 2);
        assert_eq!(edges[1].from.uid, "src/f0.rs#f0#function");
        assert_eq!(edges[1].to.uid, "src/f1.rs#f1#function");
        assert_eq!(edges[1].edge.tier, Tier::Exact);
        assert_eq!(edges[1].edge.site.line, 3);
        assert_eq!(edges[1].premises, None, "exact edges carry no premises");
        for e in edges {
            let stored = fx
                .store
                .edges_from(
                    fx.store.symbol_by_uid(&e.from.uid).unwrap().unwrap().id,
                    Some(e.edge.kind),
                )
                .unwrap();
            assert!(
                stored
                    .iter()
                    .any(|s| s.site_line == e.edge.site.line && s.tier == e.edge.tier),
                "witness edge not found in store: {e:?}"
            );
        }
    }

    #[test]
    fn probable_edge_should_list_import_premises_when_the_store_holds_them() {
        let fx = fixture(2);
        fx.call(0, 1, Tier::Probable);
        fx.store
            .insert_import(
                fx.files[0],
                "crate::f1",
                Some(fx.files[1]),
                &[crate::extract::ImportBinding::named("f1")],
            )
            .unwrap();
        let ev = fx.eval_with(
            &[0],
            &[1],
            Traversal::Callees,
            TierSelection::ExactAndProbable,
            unbounded(),
            &mut ScriptedClock::frozen(),
        );
        let Witness::Path { edges, .. } = &ev.witness else {
            panic!("expected a path witness, got {:?}", ev.witness);
        };
        assert_eq!(
            edges[0].premises,
            Some(Premises {
                available: true,
                imports: vec![ImportPremise {
                    from_path: "src/f0.rs".to_string(),
                    spec: "crate::f1".to_string(),
                    to_path: "src/f1.rs".to_string(),
                }],
            })
        );
    }

    #[test]
    fn probable_edge_should_say_premises_unavailable_when_no_import_links_the_files() {
        let fx = fixture(2);
        fx.call(0, 1, Tier::Probable);
        let ev = fx.eval_with(
            &[0],
            &[1],
            Traversal::Callees,
            TierSelection::ExactAndProbable,
            unbounded(),
            &mut ScriptedClock::frozen(),
        );
        let Witness::Path { edges, .. } = &ev.witness else {
            panic!("expected a path witness, got {:?}", ev.witness);
        };
        assert_eq!(
            edges[0].premises,
            Some(Premises {
                available: false,
                imports: vec![],
            })
        );
    }

    #[test]
    fn has_method_edges_should_be_walked_like_calls() {
        let fx = fixture(2);
        fx.store
            .insert_edge(&EdgeRow {
                src_id: fx.ids[0],
                dst_id: fx.ids[1],
                kind: EdgeKind::HasMethod,
                tier: Tier::Exact,
                site_line: 1,
                receiver: None,
                callee: None,
            })
            .unwrap();
        let ev = fx.eval(&[0], &[1], Traversal::Callees);
        assert_eq!(ev.status, Status::Established);
        let Witness::Path { edges, .. } = &ev.witness else {
            panic!("expected a path witness, got {:?}", ev.witness);
        };
        assert_eq!(edges[0].edge.kind, EdgeKind::HasMethod);
        assert_eq!(
            ev.coverage.edge_kinds,
            vec![EdgeKind::Calls, EdgeKind::HasMethod]
        );
    }

    #[test]
    fn references_edges_should_not_form_a_path() {
        // `References` ("may be invoked") is deliberately outside the
        // relation: a passed-argument reference is not a call.
        let fx = fixture(2);
        fx.store
            .insert_edge(&EdgeRow {
                src_id: fx.ids[0],
                dst_id: fx.ids[1],
                kind: EdgeKind::References,
                tier: Tier::Exact,
                site_line: 1,
                receiver: None,
                callee: None,
            })
            .unwrap();
        let ev = fx.eval(&[0], &[1], Traversal::Callees);
        assert_eq!(ev.status, Status::AbsentInSnapshot);
    }

    // --- coverage ---------------------------------------------------------------

    #[test]
    fn coverage_should_count_unresolved_sites_named_like_a_source_or_target() {
        let fx = fixture(2);
        fx.store
            .insert_unresolved_call(fx.files[0], "f1", Some(fx.ids[0]), 4, None, "calls")
            .unwrap();
        fx.store
            .insert_unresolved_call(fx.files[0], "f1", Some(fx.ids[0]), 5, None, "calls")
            .unwrap();
        fx.store
            .insert_unresolved_call(fx.files[0], "other", Some(fx.ids[0]), 6, None, "calls")
            .unwrap();
        let ev = fx.eval(&[0], &[1], Traversal::Callees);
        assert_eq!(ev.status, Status::AbsentInSnapshot);
        assert_eq!(ev.coverage.unresolved_same_name_sites, 2);
        assert_eq!(ev.coverage.depth_cap, 64);
        assert_eq!(ev.coverage.time_budget_ms, 60_000);
    }

    #[test]
    fn coverage_should_count_a_name_shared_by_a_source_and_a_target_once() {
        // Source 0 is also the target: its name `f0` appears twice in the
        // names list, and its three unresolved sites must be counted once.
        let fx = fixture(2);
        for line in [4, 5, 6] {
            fx.store
                .insert_unresolved_call(fx.files[1], "f0", Some(fx.ids[1]), line, None, "calls")
                .unwrap();
        }
        let ev = fx.eval(&[0], &[0], Traversal::Callees);
        assert_eq!(ev.status, Status::Established);
        assert_eq!(
            ev.coverage.unresolved_same_name_sites, 3,
            "a name shared by a source and a target is counted once, not per occurrence"
        );
    }

    #[test]
    fn evaluate_should_error_when_a_source_or_target_id_has_no_symbol() {
        let fx = fixture(1);
        let missing = fx.ids[0] + 1000;
        let err = evaluate(
            &fx.store,
            Request {
                sources: &[fx.ids[0]],
                targets: &[missing],
                traversal: Traversal::Callees,
                tiers: TierSelection::Exact,
                budget: unbounded(),
            },
            &mut ScriptedClock::frozen(),
        );
        assert!(
            err.is_err(),
            "a missing id is a store error, not an Unknown"
        );
    }

    #[test]
    fn multi_source_should_establish_from_whichever_source_reaches() {
        // 0 is isolated, 1→2. Sources {0, 1}, target 2.
        let fx = fixture(3);
        fx.exact(1, 2);
        let ev = fx.eval(&[0, 1], &[2], Traversal::Callees);
        assert_eq!(ev.status, Status::Established);
        let Witness::Path { edges, .. } = &ev.witness else {
            panic!("expected a path witness, got {:?}", ev.witness);
        };
        assert_eq!(edges[0].from.uid, "src/f1.rs#f1#function");
    }

    #[test]
    fn callers_traversal_should_not_answer_the_callees_question() {
        // 0→1. "Does 0 reach 1 along callers?" is false; along callees true.
        let fx = fixture(2);
        fx.exact(0, 1);
        assert_eq!(
            fx.eval(&[0], &[1], Traversal::Callers).status,
            Status::AbsentInSnapshot
        );
        assert_eq!(
            fx.eval(&[0], &[1], Traversal::Callees).status,
            Status::Established
        );
        assert_eq!(
            fx.eval(&[1], &[0], Traversal::Callers).status,
            Status::Established
        );
    }

    #[test]
    fn evaluation_should_serialize_status_and_witness_with_tags() {
        let fx = fixture(2);
        fx.exact(0, 1);
        let ev = fx.eval(&[0], &[1], Traversal::Callees);
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["status"]["status"], "established");
        assert_eq!(json["witness"]["kind"], "path");
        assert_eq!(
            json["witness"]["edges"][0]["call_direction"],
            "caller→callee"
        );
        assert_eq!(json["traversal"], "callees");
        let unknown = fx.eval_with(
            &[0],
            &[1],
            Traversal::Callees,
            TierSelection::Exact,
            depth(0),
            &mut ScriptedClock::frozen(),
        );
        let json = serde_json::to_value(&unknown).unwrap();
        assert_eq!(json["status"]["status"], "unknown");
        assert_eq!(
            json["status"]["reason"]["code"],
            "traversal_budget_exhausted"
        );
        assert_eq!(json["status"]["reason"]["parameter"], "max_depth");
        assert_eq!(json["witness"]["kind"], "none");
    }
}
