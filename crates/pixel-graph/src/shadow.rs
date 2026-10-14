// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel graph-shadow`: this graph's own Rust edges against the ones
//! pixel-mvp's index gives through [`crate::mvp_adapter`], counted the way
//! the 2026-10-14 comparison counted them. Both sides stand on this graph's
//! symbols, so an edge is `(caller, callee, site line)`, and two edges of
//! one caller and callee whose lines are at most [`LINE_TOLERANCE`] apart
//! are one: on a call split over lines, this graph records the statement's
//! first line and pixel-mvp the method name's.
//!
//! [`shadow`] builds both into a scratch directory and writes nothing in
//! the repository.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::Serialize;

use crate::mvp_adapter::{self, RustRows};
use crate::store::{EdgeKind, EdgeRow, GraphStore, Tier};

pub mod ruby;

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

/// How many lines apart two edges of one caller and callee may sit and
/// still be one edge.
pub const LINE_TOLERANCE: u32 = 3;

/// The comparison, as `graph-shadow --json` prints it.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct Shadow {
    pub mvp_commit: &'static str,
    pub calls: Calls,
    pub references: Overlap,
    pub imports: Imports,
    /// `unresolved_calls` rows of Rust files: this graph's, pixel-mvp's.
    pub unresolved: Sides,
    /// Rows of pixel-mvp's index left out for want of a symbol here.
    pub dropped_no_caller: usize,
    pub dropped_no_target: usize,
    /// Edges from a caller in another language to a Rust symbol: this
    /// graph links them by name.
    pub into_rust_from_other_languages: usize,
    /// A few edges of each side alone, `caller -> callee (path:line)`.
    pub examples: Examples,
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct Sides {
    pub v1: usize,
    pub mvp: usize,
}

/// Two edge sets and what they share.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct Overlap {
    pub v1: usize,
    pub mvp: usize,
    pub both: usize,
    pub v1_only: usize,
    pub mvp_only: usize,
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct Calls {
    #[serde(flatten)]
    pub overlap: Overlap,
    pub v1_exact: usize,
    pub v1_probable: usize,
    /// This graph's edges pixel-mvp lacks, by what they are.
    pub v1_only_by: V1Only,
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct V1Only {
    pub exact: usize,
    pub probable: usize,
    /// The callee is a symbol of another language.
    pub into_other_language: usize,
    /// Receiver shape: a method call (`x.len()`), a path (`Path::new`), none.
    pub method_receiver: usize,
    pub path_receiver: usize,
    pub no_receiver: usize,
    /// What pixel-mvp holds at the same site: an unresolved row, an edge to
    /// another callee, nothing.
    pub mvp_abstains: usize,
    pub mvp_resolves_elsewhere: usize,
    pub mvp_has_no_site: usize,
}

/// Rust `imports` rows, by the file each side resolved them to.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct Imports {
    pub rows: usize,
    pub v1_resolved: usize,
    pub mvp_resolved: usize,
    pub same: usize,
    pub v1_only: usize,
    pub mvp_only: usize,
    pub different_file: usize,
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct Examples {
    pub v1_only: Vec<String>,
    pub mvp_only: Vec<String>,
}

/// Builds this graph of `root` and pixel-mvp's index in a scratch
/// directory, then compares their Rust edges ([`compare`]). Writes nothing
/// under `root`. Fails outside a git work tree (pixel-mvp lists files
/// through git).
pub fn shadow(root: &Path, examples: usize) -> Result<Shadow, BoxErr> {
    let scratch = tempfile::tempdir()?;
    let db = scratch.path().join("graph.db");
    let mvp = mvp_adapter::refresh(root, scratch.path())?;
    // This graph's own resolver, whatever the switch says.
    crate::build::build_graph_sources(
        root,
        &db,
        mvp_adapter::RustSource::V1,
        mvp_adapter::ruby_switch::RubySource::V1,
    )?;
    let store = GraphStore::open(&db)?;
    let rows = mvp_adapter::rust_rows(&store, &mvp)?;
    compare(&store, &rows, examples)
}

/// One `(caller, callee)` pair's site lines on each side, matched: equal
/// lines first, then each line of `ours` to the nearest free one of
/// `theirs` within [`LINE_TOLERANCE`]. Returns which of each side matched.
fn matched_lines(ours: &[u32], theirs: &[u32]) -> (Vec<bool>, Vec<bool>) {
    let mut ours_hit = vec![false; ours.len()];
    let mut theirs_hit = vec![false; theirs.len()];
    for (i, line) in ours.iter().enumerate() {
        if let Some(j) = (0..theirs.len()).find(|&j| !theirs_hit[j] && theirs[j] == *line) {
            ours_hit[i] = true;
            theirs_hit[j] = true;
        }
    }
    for (i, line) in ours.iter().enumerate() {
        if ours_hit[i] {
            continue;
        }
        let nearest = (0..theirs.len())
            .filter(|&j| !theirs_hit[j] && theirs[j].abs_diff(*line) <= LINE_TOLERANCE)
            .min_by_key(|&j| theirs[j].abs_diff(*line));
        if let Some(j) = nearest {
            ours_hit[i] = true;
            theirs_hit[j] = true;
        }
    }
    (ours_hit, theirs_hit)
}

/// One `(caller, callee)` pair's edges on each side.
type PairEdges<'a> = (Vec<&'a EdgeRow>, Vec<&'a EdgeRow>);

/// The edges of each side alone, matched pair by pair ([`matched_lines`]).
fn overlap<'a>(
    ours: &[&'a EdgeRow],
    theirs: &[&'a EdgeRow],
) -> (Overlap, Vec<&'a EdgeRow>, Vec<&'a EdgeRow>) {
    let mut pairs: BTreeMap<(i64, i64), PairEdges<'a>> = BTreeMap::new();
    for e in ours {
        pairs.entry((e.src_id, e.dst_id)).or_default().0.push(e);
    }
    for e in theirs {
        pairs.entry((e.src_id, e.dst_id)).or_default().1.push(e);
    }
    let mut out = Overlap {
        v1: ours.len(),
        mvp: theirs.len(),
        ..Overlap::default()
    };
    let (mut ours_alone, mut theirs_alone) = (Vec::new(), Vec::new());
    for (mut a, mut b) in pairs.into_values() {
        a.sort_by_key(|e| e.site_line);
        b.sort_by_key(|e| e.site_line);
        let lines = |edges: &[&EdgeRow]| edges.iter().map(|e| e.site_line).collect::<Vec<_>>();
        let (a_hit, b_hit) = matched_lines(&lines(&a), &lines(&b));
        out.both += a_hit.iter().filter(|&&hit| hit).count();
        ours_alone.extend(
            a.iter()
                .zip(&a_hit)
                .filter(|(_, hit)| !**hit)
                .map(|(e, _)| *e),
        );
        theirs_alone.extend(
            b.iter()
                .zip(&b_hit)
                .filter(|(_, hit)| !**hit)
                .map(|(e, _)| *e),
        );
    }
    out.v1_only = ours_alone.len();
    out.mvp_only = theirs_alone.len();
    (out, ours_alone, theirs_alone)
}

/// Compares `store`'s own edges from Rust callers with `rows`, pixel-mvp's
/// answer on the same symbols.
pub fn compare(store: &GraphStore, rows: &RustRows, examples: usize) -> Result<Shadow, BoxErr> {
    let conn = store.conn();
    // symbol id -> (file id, path, qualified name, lang)
    let mut symbols: HashMap<i64, (i64, String, String, String)> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT s.id, s.file_id, f.path, s.qualified, f.lang FROM symbols s
               JOIN files f ON f.id = s.file_id",
        )?;
        let found = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?),
            ))
        })?;
        for row in found {
            let (id, rest) = row?;
            symbols.insert(id, rest);
        }
    }
    let rust = |id: i64| symbols.get(&id).is_some_and(|s| s.3 == "rust");
    let ours: Vec<EdgeRow> = {
        let mut stmt = conn.prepare(
            "SELECT src_id, dst_id, kind, tier, site_line, receiver, callee FROM edges
              WHERE kind IN ('calls', 'references')",
        )?;
        let found = stmt.query_map([], |r| {
            Ok(EdgeRow {
                src_id: r.get(0)?,
                dst_id: r.get(1)?,
                kind: EdgeKind::parse(&r.get::<_, String>(2)?),
                tier: Tier::parse(&r.get::<_, String>(3)?),
                site_line: r.get(4)?,
                receiver: r.get(5)?,
                callee: r.get(6)?,
            })
        })?;
        found.collect::<Result<_, _>>()?
    };
    let mut out = Shadow {
        mvp_commit: mvp_adapter::MVP_COMMIT,
        dropped_no_caller: rows.dropped.no_caller,
        dropped_no_target: rows.dropped.no_target,
        ..Shadow::default()
    };
    out.into_rust_from_other_languages = ours
        .iter()
        .filter(|e| symbols.contains_key(&e.src_id) && !rust(e.src_id) && rust(e.dst_id))
        .count();

    let of_kind = |edges: &[EdgeRow], kind: EdgeKind, from_rust: bool| -> Vec<EdgeRow> {
        edges
            .iter()
            .filter(|e| e.kind == kind && (!from_rust || rust(e.src_id)))
            .cloned()
            .collect()
    };
    let our_calls = of_kind(&ours, EdgeKind::Calls, true);
    let our_calls: Vec<&EdgeRow> = our_calls.iter().collect();
    let their_calls: Vec<&EdgeRow> = rows.calls.iter().collect();
    let (overlap_calls, v1_alone, mvp_alone) = overlap(&our_calls, &their_calls);
    out.calls.overlap = overlap_calls;
    out.calls.v1_exact = our_calls.iter().filter(|e| e.tier == Tier::Exact).count();
    out.calls.v1_probable = our_calls.len() - out.calls.v1_exact;

    // What pixel-mvp holds at a site: (caller, line) of its edges, and
    // (file, line, name) of its unresolved rows.
    let mvp_sites: HashSet<(i64, u32)> =
        rows.calls.iter().map(|e| (e.src_id, e.site_line)).collect();
    let mvp_open: HashSet<(i64, u32, &str)> = rows
        .unresolved
        .iter()
        .map(|u| (u.file_id, u.site_line, u.name.as_str()))
        .collect();
    let by = &mut out.calls.v1_only_by;
    for e in &v1_alone {
        match e.tier {
            Tier::Exact => by.exact += 1,
            Tier::Probable => by.probable += 1,
        }
        if !rust(e.dst_id) {
            by.into_other_language += 1;
        }
        match e.receiver.as_deref().map(str::trim) {
            Some(r) if r.starts_with('.') => by.method_receiver += 1,
            Some(r) if !r.is_empty() => by.path_receiver += 1,
            _ => by.no_receiver += 1,
        }
        let file = symbols.get(&e.src_id).map_or(-1, |s| s.0);
        let name = e.callee.as_deref().unwrap_or_default();
        if mvp_open.contains(&(file, e.site_line, name)) {
            by.mvp_abstains += 1;
        } else if mvp_sites.contains(&(e.src_id, e.site_line)) {
            by.mvp_resolves_elsewhere += 1;
        } else {
            by.mvp_has_no_site += 1;
        }
    }
    let shown = |e: &EdgeRow| -> String {
        let name = |id: i64| symbols.get(&id).map_or("?", |s| s.2.as_str()).to_string();
        let path = symbols.get(&e.src_id).map_or("?", |s| s.1.as_str());
        format!(
            "{} -> {} ({path}:{})",
            name(e.src_id),
            name(e.dst_id),
            e.site_line
        )
    };
    out.examples.v1_only = v1_alone.iter().take(examples).map(|e| shown(e)).collect();
    out.examples.mvp_only = mvp_alone.iter().take(examples).map(|e| shown(e)).collect();

    let our_refs = of_kind(&ours, EdgeKind::References, true);
    let our_refs: Vec<&EdgeRow> = our_refs.iter().collect();
    let their_refs: Vec<&EdgeRow> = rows.references.iter().collect();
    out.references = overlap(&our_refs, &their_refs).0;

    let rust_files: HashSet<i64> = store
        .files()?
        .into_iter()
        .filter(|f| f.lang == "rust")
        .map(|f| f.id)
        .collect();
    out.unresolved.v1 = {
        let mut stmt = conn.prepare("SELECT file_id FROM unresolved_calls")?;
        let found = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        let mut n = 0;
        for file in found {
            n += usize::from(rust_files.contains(&file?));
        }
        n
    };
    out.unresolved.mvp = rows.unresolved.len();

    let v1_imports: HashMap<i64, Option<i64>> = {
        let mut stmt = conn.prepare("SELECT id, file_id, resolved_file_id FROM imports")?;
        let found = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        })?;
        let mut map = HashMap::new();
        for row in found {
            let (id, file, target) = row?;
            if rust_files.contains(&file) {
                map.insert(id, target);
            }
        }
        map
    };
    let imports = &mut out.imports;
    imports.rows = v1_imports.len();
    for (id, mvp) in &rows.imports {
        let v1 = v1_imports.get(id).copied().flatten();
        imports.v1_resolved += usize::from(v1.is_some());
        imports.mvp_resolved += usize::from(mvp.is_some());
        match (v1, mvp) {
            (Some(a), Some(b)) if a == *b => imports.same += 1,
            (Some(_), Some(_)) => imports.different_file += 1,
            (Some(_), None) => imports.v1_only += 1,
            (None, Some(_)) => imports.mvp_only += 1,
            (None, None) => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(src: i64, dst: i64, line: u32) -> EdgeRow {
        EdgeRow {
            src_id: src,
            dst_id: dst,
            kind: EdgeKind::Calls,
            tier: Tier::Exact,
            site_line: line,
            receiver: None,
            callee: None,
        }
    }

    #[test]
    fn lines_match_equal_first_then_nearest_within_the_tolerance() {
        // 10 = 10; 20 takes 22 (2 apart) rather than 23; 30 finds nothing
        // within 3 (34 is 4 apart).
        let (ours, theirs) = matched_lines(&[10, 20, 30], &[23, 10, 22, 34]);
        assert_eq!(ours, vec![true, true, false]);
        assert_eq!(theirs, vec![false, true, true, false]);
        // Exactly the tolerance still matches.
        assert_eq!(matched_lines(&[5], &[8]).0, vec![true]);
        // An equal line is matched first: 8 keeps 8, though 7 comes first
        // and 8 is its nearest; 7 then finds nothing within 3 (11 is 4).
        let (ours, theirs) = matched_lines(&[7, 8], &[8, 11]);
        assert_eq!(ours, vec![false, true]);
        assert_eq!(theirs, vec![true, false]);
    }

    #[test]
    fn overlap_counts_each_side_alone_per_caller_and_callee() {
        let ours = [edge(1, 2, 10), edge(1, 2, 40), edge(1, 3, 10)];
        let theirs = [edge(1, 2, 12), edge(1, 4, 10)];
        let ours: Vec<&EdgeRow> = ours.iter().collect();
        let theirs: Vec<&EdgeRow> = theirs.iter().collect();
        let (counts, v1_alone, mvp_alone) = overlap(&ours, &theirs);
        assert_eq!(
            counts,
            Overlap {
                v1: 3,
                mvp: 2,
                both: 1,
                v1_only: 2,
                mvp_only: 1
            }
        );
        let lines = |edges: &[&EdgeRow]| {
            edges
                .iter()
                .map(|e| (e.dst_id, e.site_line))
                .collect::<Vec<_>>()
        };
        assert_eq!(lines(&v1_alone), vec![(2, 40), (3, 10)]);
        assert_eq!(lines(&mvp_alone), vec![(4, 10)]);
    }
}
