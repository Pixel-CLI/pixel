// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Impact analysis — BFS over `calls` edges, depth-bucketed blast radius with
//! an epistemic envelope so "0 callers" is distinguishable from "resolver gave up".

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::str::FromStr;

use rusqlite::params;
use serde::Serialize;

use crate::store::{EdgeKind, Envelope, GraphStore, StoreError, SymbolRow, Tier};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Upstream,
    Downstream,
}

impl Direction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Direction::Upstream => "upstream",
            Direction::Downstream => "downstream",
        }
    }
}

impl FromStr for Direction {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "upstream" | "up" | "callers" => Ok(Direction::Upstream),
            "downstream" | "down" | "callees" => Ok(Direction::Downstream),
            other => Err(format!(
                "unknown direction: {other} (expected upstream|downstream)"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactItem {
    pub uid: String,
    pub name: String,
    pub path: String,
    pub line: u32,
    pub tier: String,
    pub processes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactReport {
    pub target: String,
    pub direction: String,
    /// "LOW" | "MEDIUM" | "HIGH" | "CRITICAL" | "UNKNOWN"
    pub risk: String,
    /// Present only when `risk` is `UNKNOWN`: why the verdict is withheld.
    /// Omitted on every ranked verdict, so an absent field means "no caveat",
    /// never "a caveat was dropped".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_note: Option<String>,
    pub summary: String,
    pub d1_will_break: Vec<ImpactItem>,
    pub d2_likely_affected: Vec<ImpactItem>,
    pub d3_may_need_tests: Vec<ImpactItem>,
    pub counts_by_depth: [u64; 3],
    pub affected_files: u64,
    pub affected_processes: Vec<String>,
    pub envelope: Envelope,
    /// Symbols that reference the target via a `References` edge (passed as
    /// an argument to a call — a callback / plugin / handler registration).
    /// Weaker evidence than `Calls`: "may be invoked", not "will break".
    /// Populated for the upstream direction; empty otherwise.
    pub referenced_by: Vec<ImpactItem>,
    /// Distinct symbols referencing the target via `References` edges, counted
    /// before the `MAX_REFERENCED_BY` cut: `referenced_by` lists at most the
    /// cap, this is the honest total.
    pub referenced_by_total: u64,
    /// True when `referenced_by_total` exceeded `MAX_REFERENCED_BY`, so
    /// `referenced_by` holds only the first `MAX_REFERENCED_BY` entries.
    pub referenced_by_truncated: bool,
    /// True when any list in this report was cut short: a per-depth bucket at
    /// `limit_per_depth` or `referenced_by` at `MAX_REFERENCED_BY`.
    /// `counts_by_depth` and `referenced_by_total` stay exact either way.
    pub truncated: bool,
    /// Named caps that fired, one per cut list, so a consumer can mirror them
    /// as warnings instead of passing off a sampled list as complete.
    pub caps: Vec<String>,
}

/// Fetch a symbol row by rowid via ad-hoc SQL (store exposes uid/name lookups only).
pub(crate) fn symbol_by_id(store: &GraphStore, id: i64) -> Result<Option<SymbolRow>, StoreError> {
    use rusqlite::OptionalExtension;
    let row = store
        .conn()
        .query_row(
            "SELECT id, uid, file_id, name, qualified, kind, start_line, end_line, sig
             FROM symbols WHERE id = ?1",
            params![id],
            |r| {
                Ok(SymbolRow {
                    id: r.get(0)?,
                    uid: r.get(1)?,
                    file_id: r.get(2)?,
                    name: r.get(3)?,
                    qualified: r.get(4)?,
                    kind: crate::store::SymbolKind::parse(&r.get::<_, String>(5)?),
                    start_line: r.get(6)?,
                    end_line: r.get(7)?,
                    sig: r.get(8)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

pub(crate) fn file_path_by_id(store: &GraphStore, file_id: i64) -> Result<String, StoreError> {
    use rusqlite::OptionalExtension;
    Ok(store
        .conn()
        .query_row(
            "SELECT path FROM files WHERE id = ?1",
            params![file_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_default())
}

/// Distinct process labels a symbol participates in.
pub(crate) fn processes_for_symbol(
    store: &GraphStore,
    symbol_id: i64,
) -> Result<Vec<String>, StoreError> {
    let mut stmt = store.conn().prepare(
        "SELECT DISTINCT p.label FROM processes p
         JOIN process_steps ps ON ps.process_id = p.id
         WHERE ps.symbol_id = ?1 ORDER BY p.label",
    )?;
    let rows = stmt.query_map(params![symbol_id], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn risk_label(mut level: u8, lower_bound: bool) -> String {
    if lower_bound && level < 3 {
        level += 1;
    }
    match level {
        0 => "LOW",
        1 => "MEDIUM",
        2 => "HIGH",
        _ => "CRITICAL",
    }
    .to_string()
}

/// Risk reported when an upstream walk resolved no caller at all. Not a rung
/// below `LOW`: `LOW` is a claim about how much a change disturbs, and an
/// empty caller set is equally consistent with "unused" and with "reached
/// only through a reference class the index does not record", so no rank is
/// defensible.
const RISK_UNKNOWN: &str = "UNKNOWN";

/// The `risk_note` that accompanies `RISK_UNKNOWN`: both readings of an empty
/// caller set, and the check that separates them.
const NO_CALLER_NOTE: &str = "no caller was found: the symbol is either unused or reached only through a reference class the index does not record (dynamic dispatch, a callback passed as an argument, property access) — confirm with a text search before treating it as safe to change";

/// Cap on `referenced_by` items, like the main BFS buckets: an unbounded
/// list is noise.
const MAX_REFERENCED_BY: usize = 20;

pub fn impact(
    store: &GraphStore,
    uid: &str,
    direction: Direction,
    max_depth: u32,
    limit_per_depth: usize,
) -> Result<ImpactReport, StoreError> {
    let target = store
        .symbol_by_uid(uid)?
        .ok_or_else(|| StoreError::Sql(rusqlite::Error::QueryReturnedNoRows))?;
    let max_depth = max_depth.clamp(1, 3);

    let mut visited: HashSet<i64> = HashSet::new();
    visited.insert(target.id);
    // (symbol_id, depth, tier of the edge that reached it)
    let mut queue: VecDeque<(i64, u32, Tier)> = VecDeque::new();
    queue.push_back((target.id, 0, Tier::Exact));

    let mut buckets: [Vec<ImpactItem>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut counts: [u64; 3] = [0, 0, 0];
    // A bucket refused an item at `limit_per_depth`. The count stays exact;
    // only the listed subset is cut, and the cut is named below.
    let mut bucket_truncated = [false; 3];
    let mut files: HashSet<i64> = HashSet::new();
    let mut proc_set: BTreeSet<String> = BTreeSet::new();

    while let Some((id, depth, _)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        let edges = match direction {
            Direction::Upstream => store.edges_to(id, Some(EdgeKind::Calls))?,
            Direction::Downstream => store.edges_from(id, Some(EdgeKind::Calls))?,
        };
        for e in edges {
            let nbr = match direction {
                Direction::Upstream => e.src_id,
                Direction::Downstream => e.dst_id,
            };
            if !visited.insert(nbr) {
                continue;
            }
            let d = depth + 1;
            let bucket = (d - 1) as usize;
            counts[bucket] += 1;
            if let Some(sym) = symbol_by_id(store, nbr)? {
                files.insert(sym.file_id);
                let procs = processes_for_symbol(store, sym.id)?;
                for p in &procs {
                    proc_set.insert(p.clone());
                }
                if buckets[bucket].len() < limit_per_depth {
                    buckets[bucket].push(ImpactItem {
                        uid: sym.uid,
                        name: sym.name,
                        path: file_path_by_id(store, sym.file_id)?,
                        line: sym.start_line,
                        tier: e.tier.as_str().to_string(),
                        processes: procs,
                    });
                } else {
                    bucket_truncated[bucket] = true;
                }
            }
            queue.push_back((nbr, d, e.tier));
        }
    }

    let envelope = store.envelope_for_name(&target.name)?;
    let affected_processes: Vec<String> = proc_set.into_iter().collect();

    // References edges: symbols passed as arguments to calls (callbacks /
    // plugins / handlers). Weaker than Calls — "may be invoked", not "will
    // break" — so they surface in `referenced_by` rather than the main BFS
    // buckets. Only meaningful for the upstream direction (who passes me
    // as a callback).
    let mut referenced_by: Vec<ImpactItem> = Vec::new();
    let mut referenced_by_total: u64 = 0;
    let mut referenced_by_truncated = false;
    if matches!(direction, Direction::Upstream) {
        let ref_edges = store.edges_to(target.id, Some(EdgeKind::References))?;
        // Dedupe by src_id — the same referrer at N call sites shouldn't
        // produce N identical items; the total counts each referrer once.
        let mut seen_src: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for e in ref_edges {
            if !seen_src.insert(e.src_id) {
                continue;
            }
            referenced_by_total += 1;
            if referenced_by.len() >= MAX_REFERENCED_BY {
                referenced_by_truncated = true;
                continue;
            }
            if let Some(sym) = symbol_by_id(store, e.src_id)? {
                // Don't count reference-side files/processes toward risk —
                // References are weaker evidence than direct calls.
                referenced_by.push(ImpactItem {
                    uid: sym.uid,
                    name: sym.name,
                    path: file_path_by_id(store, sym.file_id)?,
                    line: sym.start_line,
                    tier: e.tier.as_str().to_string(),
                    processes: Vec::new(),
                });
            }
        }
    }

    let mut caps: Vec<String> = Vec::new();
    for (depth, cut) in bucket_truncated.iter().enumerate() {
        if *cut {
            caps.push(format!(
                "depth-{} list truncated at {limit_per_depth} of {} symbols",
                depth + 1,
                counts[depth]
            ));
        }
    }
    if referenced_by_truncated {
        caps.push(format!(
            "referenced_by truncated at {MAX_REFERENCED_BY} of {referenced_by_total} referencing symbols"
        ));
    }
    let truncated = !caps.is_empty();

    let d1 = counts[0];
    let nproc = affected_processes.len();
    let base = if d1 > 50 || nproc > 20 {
        3
    } else if d1 > 15 || nproc > 8 {
        2
    } else if d1 > 3 {
        1
    } else {
        0
    };
    // An upstream walk that resolved no caller cannot rank the change: an
    // empty caller set is as consistent with "unused" as with "reached only
    // through a reference class the index does not record", so ranking it
    // `LOW` would turn "found nothing" into "safe to change". Upstream only:
    // an empty downstream walk answers what the symbol calls, and zero there
    // is a fact about the code rather than a claim about risk.
    let no_caller = matches!(direction, Direction::Upstream) && d1 == 0;
    let (risk, risk_note) = if no_caller {
        (RISK_UNKNOWN.to_string(), Some(NO_CALLER_NOTE.to_string()))
    } else {
        (risk_label(base, envelope.lower_bound), None)
    };
    let ref_suffix = if referenced_by_total != 0 {
        format!(
            "; referenced as callback in {referenced_by_total} site{}",
            if referenced_by_total == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    let summary = format!(
        "{}: {} at depth 1, {} at depth 2, {} at depth 3 ({}) across {} files, {} processes; risk {}{}{}",
        target.name,
        counts[0],
        counts[1],
        counts[2],
        direction.as_str(),
        files.len(),
        nproc,
        risk,
        if envelope.lower_bound {
            format!(
                " (lower bound: {} unresolved same-name call sites)",
                envelope.unresolved_same_name
            )
        } else {
            String::new()
        },
        ref_suffix
    );

    let [d1_will_break, d2_likely_affected, d3_may_need_tests] = buckets;
    Ok(ImpactReport {
        target: target.uid,
        direction: direction.as_str().to_string(),
        risk,
        risk_note,
        summary,
        d1_will_break,
        d2_likely_affected,
        d3_may_need_tests,
        counts_by_depth: counts,
        affected_files: files.len() as u64,
        affected_processes,
        envelope,
        referenced_by,
        referenced_by_total,
        referenced_by_truncated,
        truncated,
        caps,
    })
}

/// Word-frequency helper shared by cluster/process labeling: split camelCase +
/// snake_case identifiers into lowercase words.
pub fn split_ident_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    for chunk in name.split(['_', '-', '.', ':', '#']) {
        if chunk.is_empty() {
            continue;
        }
        let mut cur = String::new();
        let chars: Vec<char> = chunk.chars().collect();
        for (i, &c) in chars.iter().enumerate() {
            let boundary = c.is_uppercase()
                && i > 0
                && (chars[i - 1].is_lowercase()
                    || (i + 1 < chars.len() && chars[i + 1].is_lowercase()));
            if boundary && !cur.is_empty() {
                words.push(cur.to_lowercase());
                cur = String::new();
            }
            cur.push(c);
        }
        if !cur.is_empty() {
            words.push(cur.to_lowercase());
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{EdgeRow, GraphStore, SymbolKind, Tier};
    use crate::trace;

    fn sym(store: &GraphStore, file_id: i64, name: &str, start: u32, end: u32) -> i64 {
        store
            .insert_symbol(
                file_id,
                &format!("src/a.ts#{name}#function"),
                name,
                name,
                SymbolKind::Function,
                start,
                end,
                "",
            )
            .unwrap()
    }

    fn call(store: &GraphStore, src: i64, dst: i64) {
        store
            .insert_edge(&EdgeRow {
                src_id: src,
                dst_id: dst,
                kind: EdgeKind::Calls,
                tier: Tier::Exact,
                site_line: 1,
                receiver: None,
                callee: None,
            })
            .unwrap();
    }

    fn reference(store: &GraphStore, src: i64, dst: i64) {
        store
            .insert_edge(&EdgeRow {
                src_id: src,
                dst_id: dst,
                kind: EdgeKind::References,
                tier: Tier::Probable,
                site_line: 1,
                receiver: Some("on".to_string()),
                callee: None,
            })
            .unwrap();
    }

    #[test]
    fn impact_buckets_envelope_and_trace() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fid = store.replace_file("src/a.ts", "oid", "ts").unwrap();
        let a = sym(&store, fid, "alpha", 1, 5);
        let b = sym(&store, fid, "beta", 10, 15);
        let c = sym(&store, fid, "gamma", 20, 25);
        // c -> b -> a
        call(&store, b, a);
        call(&store, c, b);
        // unresolved same-name call site for "alpha" → lower bound
        store
            .insert_unresolved_call(fid, "alpha", None, 42, None, "calls")
            .unwrap();

        let report = impact(
            &store,
            "src/a.ts#alpha#function",
            Direction::Upstream,
            3,
            100,
        )
        .unwrap();
        assert_eq!(report.counts_by_depth, [1, 1, 0]);
        assert_eq!(report.d1_will_break.len(), 1);
        assert_eq!(report.d1_will_break[0].name, "beta");
        assert_eq!(report.d2_likely_affected.len(), 1);
        assert_eq!(report.d2_likely_affected[0].name, "gamma");
        assert!(report.envelope.lower_bound);
        assert_eq!(report.envelope.unresolved_same_name, 1);
        // LOW bumped one level by lower-bound envelope
        assert_eq!(report.risk, "MEDIUM");
        // Nothing was cut: no cap, no truncation marker, no callback suffix.
        assert_eq!(report.referenced_by_total, 0);
        assert!(!report.referenced_by_truncated);
        assert!(!report.truncated);
        assert!(report.caps.is_empty());
        assert!(!report.summary.contains("referenced as callback"));

        // trace c -> a found through b
        let t = trace::trace(
            &store,
            "src/a.ts#gamma#function",
            "src/a.ts#alpha#function",
            10,
        )
        .unwrap();
        assert!(t.found);
        assert_eq!(t.hops.len(), 3);
        assert_eq!(t.hops[0].name, "gamma");
        assert_eq!(t.hops[2].name, "alpha");
    }

    /// A symbol with 0 `Calls` callers but 1 `References` edge should show
    /// `referenced_by` with 1 item in an upstream impact report.
    #[test]
    fn referenced_by_surfaces_callback_edges() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fid = store.replace_file("src/a.ts", "oid", "ts").unwrap();
        let handler = sym(&store, fid, "handler", 1, 5);
        let setup = sym(&store, fid, "setup", 10, 15);
        // No Calls edge to handler; only a References edge from setup.
        store
            .insert_edge(&EdgeRow {
                src_id: setup,
                dst_id: handler,
                kind: EdgeKind::References,
                tier: Tier::Probable,
                site_line: 12,
                receiver: Some("on".to_string()),
                callee: None,
            })
            .unwrap();

        let report = impact(
            &store,
            "src/a.ts#handler#function",
            Direction::Upstream,
            3,
            100,
        )
        .unwrap();
        // No Calls callers.
        assert_eq!(report.counts_by_depth, [0, 0, 0]);
        assert!(report.d1_will_break.is_empty());
        // But one referenced_by entry, and the total is that same count.
        // No Calls callers, so the walk resolved nothing: the verdict is
        // withheld rather than ranked, and the note says which check
        // separates "unused" from "unreachable by the index". A callback
        // registration is not a caller, so `referenced_by` does not turn the
        // empty caller set back into a ranked answer.
        assert_eq!(report.risk, "UNKNOWN");
        let note = report.risk_note.as_ref().expect("UNKNOWN carries a note");
        assert!(note.contains("no caller was found"), "{note}");
        assert!(note.contains("text search"), "{note}");
        assert_eq!(report.referenced_by.len(), 1);
        assert_eq!(report.referenced_by[0].name, "setup");
        assert_eq!(report.referenced_by[0].tier, "probable");
        assert_eq!(report.referenced_by_total, 1);
        assert!(!report.referenced_by_truncated);
        assert!(!report.truncated);
        assert!(report.caps.is_empty());
        // Summary mentions the callback reference, singular.
        assert!(report.summary.ends_with("referenced as callback in 1 site"));
    }

    /// 25 referencing symbols against a 20-item cap: the summary and
    /// `referenced_by_total` report the real count, and the cut is named so
    /// the answer is never passed off as complete.
    #[test]
    fn referenced_by_reports_the_total_when_cut() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fid = store.replace_file("src/a.ts", "oid", "ts").unwrap();
        let handler = sym(&store, fid, "handler", 1, 5);
        for i in 0..25u32 {
            let name = format!("caller{i}");
            let src = sym(&store, fid, &name, 10 + i, 12 + i);
            reference(&store, src, handler);
        }

        let report = impact(
            &store,
            "src/a.ts#handler#function",
            Direction::Upstream,
            3,
            100,
        )
        .unwrap();
        assert_eq!(report.referenced_by_total, 25);
        assert_eq!(report.referenced_by.len(), 20);
        assert!(report.referenced_by_truncated);
        assert!(report.truncated);
        assert_eq!(
            report.caps,
            vec!["referenced_by truncated at 20 of 25 referencing symbols"]
        );
        assert!(
            report
                .summary
                .contains("referenced as callback in 25 sites")
        );
    }

    /// Exactly `MAX_REFERENCED_BY` referencing symbols: every one is listed,
    /// so nothing was cut and the answer carries no cap or truncation marker.
    #[test]
    fn referenced_by_at_the_cap_is_not_truncated() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fid = store.replace_file("src/a.ts", "oid", "ts").unwrap();
        let handler = sym(&store, fid, "handler", 1, 5);
        for i in 0..MAX_REFERENCED_BY {
            let name = format!("caller{i}");
            let src = sym(&store, fid, &name, 10 + i as u32, 12 + i as u32);
            reference(&store, src, handler);
        }

        let report = impact(
            &store,
            "src/a.ts#handler#function",
            Direction::Upstream,
            3,
            100,
        )
        .unwrap();
        assert_eq!(report.referenced_by_total, MAX_REFERENCED_BY as u64);
        assert_eq!(report.referenced_by.len(), MAX_REFERENCED_BY);
        assert!(!report.referenced_by_truncated);
        assert!(!report.truncated);
        assert!(report.caps.is_empty());
        assert!(
            report
                .summary
                .contains(&format!("in {MAX_REFERENCED_BY} sites"))
        );
    }

    /// A per-depth bucket cut at `limit_per_depth` is named, and the exact
    /// count stays in `counts_by_depth`; a bucket that exactly fills the
    /// limit is not a cut.
    #[test]
    fn depth_bucket_cap_is_named_only_when_it_cuts() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fid = store.replace_file("src/a.ts", "oid", "ts").unwrap();
        let a = sym(&store, fid, "alpha", 1, 5);
        for i in 0..4u32 {
            let name = format!("caller{i}");
            let src = sym(&store, fid, &name, 10 + i, 12 + i);
            call(&store, src, a);
        }

        let cut = impact(&store, "src/a.ts#alpha#function", Direction::Upstream, 3, 3).unwrap();
        assert_eq!(cut.counts_by_depth, [4, 0, 0]);
        assert_eq!(cut.d1_will_break.len(), 3);
        assert!(cut.truncated);
        assert_eq!(cut.caps, vec!["depth-1 list truncated at 3 of 4 symbols"]);

        let exact = impact(&store, "src/a.ts#alpha#function", Direction::Upstream, 3, 4).unwrap();
        assert_eq!(exact.counts_by_depth, [4, 0, 0]);
        assert_eq!(exact.d1_will_break.len(), 4);
        assert!(!exact.truncated);
        assert!(exact.caps.is_empty());
    }

    /// An upstream walk that resolved no caller ranks nothing: the answer is
    /// `UNKNOWN` plus the note naming both readings of an empty caller set,
    /// never a `LOW` that reads as "safe to change".
    #[test]
    fn upstream_walk_without_a_caller_reports_unknown() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fid = store.replace_file("src/a.ts", "oid", "ts").unwrap();
        sym(&store, fid, "leaf", 1, 5);

        let report = impact(
            &store,
            "src/a.ts#leaf#function",
            Direction::Upstream,
            3,
            100,
        )
        .unwrap();
        assert_eq!(report.counts_by_depth, [0, 0, 0]);
        assert_eq!(report.risk, "UNKNOWN");
        // The wire contract agents read: the verdict and the note that
        // explains it, both in the serialized form.
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["risk"], "UNKNOWN");
        let note = json["risk_note"].as_str().expect("UNKNOWN carries a note");
        assert!(note.contains("no caller was found"), "{note}");
        assert!(note.contains("text search"), "{note}");
        assert!(
            report.summary.contains("risk UNKNOWN"),
            "{}",
            report.summary
        );
    }

    /// Downstream is unaffected: an empty callee set answers what the symbol
    /// calls, which is a fact about the code and not a claim about safety.
    #[test]
    fn downstream_walk_without_a_callee_stays_ranked() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fid = store.replace_file("src/a.ts", "oid", "ts").unwrap();
        sym(&store, fid, "leaf", 1, 5);

        let report = impact(
            &store,
            "src/a.ts#leaf#function",
            Direction::Downstream,
            3,
            100,
        )
        .unwrap();
        assert_eq!(report.risk, "LOW");
        // Absent, not null: an absent field can only mean "no caveat".
        assert!(report.risk_note.is_none());
        let json = serde_json::to_value(&report).unwrap();
        assert!(json.get("risk_note").is_none(), "{json}");
    }

    /// One resolved caller is enough to rank again, and a ranked verdict
    /// carries no note.
    #[test]
    fn a_single_caller_keeps_the_ranked_verdict() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fid = store.replace_file("src/a.ts", "oid", "ts").unwrap();
        let a = sym(&store, fid, "alpha", 1, 5);
        let b = sym(&store, fid, "beta", 10, 15);
        call(&store, b, a);

        let report = impact(
            &store,
            "src/a.ts#alpha#function",
            Direction::Upstream,
            3,
            100,
        )
        .unwrap();
        assert_eq!(report.counts_by_depth, [1, 0, 0]);
        assert_eq!(report.risk, "LOW");
        assert!(report.risk_note.is_none());
    }
}
