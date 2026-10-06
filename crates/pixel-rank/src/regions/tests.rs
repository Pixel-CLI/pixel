// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use super::*;

fn region(uid: &str, name: &str, file: &str, start: u32, end: u32) -> RegionInput {
    RegionInput {
        uid: uid.to_string(),
        name: name.to_string(),
        kind: "function".to_string(),
        file: file.to_string(),
        start_line: start,
        end_line: end,
    }
}

fn mk_inputs(
    regions: Vec<RegionInput>,
    call_edges: Vec<(&str, &str)>,
    import_edges: Vec<(&str, &str)>,
) -> RegionsInputs {
    RegionsInputs {
        regions,
        call_edges: call_edges
            .into_iter()
            .map(|(caller, callee)| CallEdge {
                caller: caller.to_string(),
                callee: callee.to_string(),
            })
            .collect(),
        import_edges: import_edges
            .into_iter()
            .map(|(importer, imported)| ImportEdge {
                importer: importer.to_string(),
                imported: imported.to_string(),
            })
            .collect(),
        graph_available: true,
        unresolved_same_name: 0,
        caps: Vec::new(),
    }
}

fn conflict_reasons(report: &RegionsReport) -> Vec<(&str, &str, &'static str)> {
    report
        .conflicts
        .iter()
        .map(|c| (c.a.as_str(), c.b.as_str(), c.reason.as_str()))
        .collect()
}

// ---------------------------------------------------------------------------
// conflict detection — the conservative direction
// ---------------------------------------------------------------------------

#[test]
fn two_regions_in_one_file_conflict() {
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("a.rs#beta#function", "beta", "a.rs", 12, 20),
        ],
        vec![],
        vec![],
    );
    let report = compute_regions(inputs);
    assert_eq!(
        conflict_reasons(&report),
        vec![("a.rs#alpha#function", "a.rs#beta#function", "same file")],
        "two symbols in one file must never be editable in parallel"
    );
    assert!(!report.lower_bound);
}

#[test]
fn call_edge_conflicts_in_both_directions() {
    // a calls b.
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
        ],
        vec![("a.rs#alpha#function", "b.rs#beta#function")],
        vec![],
    );
    let report = compute_regions(inputs);
    assert_eq!(
        conflict_reasons(&report),
        vec![("a.rs#alpha#function", "b.rs#beta#function", "call edge")],
        "a caller and its callee must not be edited in parallel"
    );

    // b calls a: the direction must not matter.
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
        ],
        vec![("b.rs#beta#function", "a.rs#alpha#function")],
        vec![],
    );
    let report = compute_regions(inputs);
    assert_eq!(
        conflict_reasons(&report),
        vec![("a.rs#alpha#function", "b.rs#beta#function", "call edge")],
        "conflict is symmetric: callee-then-caller is the same pair"
    );
}

#[test]
fn import_adjacency_conflicts_in_both_directions() {
    // a.rs imports b.rs.
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
        ],
        vec![],
        vec![("a.rs", "b.rs")],
    );
    let report = compute_regions(inputs);
    assert_eq!(
        conflict_reasons(&report),
        vec![(
            "a.rs#alpha#function",
            "b.rs#beta#function",
            "import adjacency"
        )],
        "an importer and its dependency must not be edited in parallel"
    );

    // b.rs imports a.rs: symmetric.
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
        ],
        vec![],
        vec![("b.rs", "a.rs")],
    );
    let report = compute_regions(inputs);
    assert_eq!(
        conflict_reasons(&report),
        vec![(
            "a.rs#alpha#function",
            "b.rs#beta#function",
            "import adjacency"
        )],
        "import adjacency is symmetric"
    );
}

#[test]
fn same_name_symbols_conflict_even_in_different_files() {
    // Two `flow` symbols in different files: a caller may mean either, and
    // an edit may need to be mirrored. Doubt → conflict.
    let inputs = mk_inputs(
        vec![
            region("a.rs#flow#function", "flow", "a.rs", 1, 9),
            region("b.rs#flow#function", "flow", "b.rs", 1, 9),
        ],
        vec![],
        vec![],
    );
    let report = compute_regions(inputs);
    assert_eq!(
        conflict_reasons(&report),
        vec![("a.rs#flow#function", "b.rs#flow#function", "same name")],
        "same-name regions conflict: the unresolved-call envelope exists \
         precisely because same-name symbols cannot be told apart"
    );
}

#[test]
fn graph_unavailable_conflicts_every_pair() {
    // The conservative floor: with no graph, disjointness is unprovable for
    // ANY pair. A false "disjoint" costs a broken merge.
    let mut inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
            region("c.rs#gamma#function", "gamma", "c.rs", 1, 9),
        ],
        vec![],
        vec![],
    );
    inputs.graph_available = false;
    let report = compute_regions(inputs);
    assert_eq!(
        conflict_reasons(&report),
        vec![
            (
                "a.rs#alpha#function",
                "b.rs#beta#function",
                "graph unavailable"
            ),
            (
                "a.rs#alpha#function",
                "c.rs#gamma#function",
                "graph unavailable"
            ),
            (
                "b.rs#beta#function",
                "c.rs#gamma#function",
                "graph unavailable"
            ),
        ],
        "no graph means no pair may be called disjoint"
    );
    assert!(report.lower_bound);
    assert!(report.caps.iter().any(|c| c.contains("graph unavailable")));
}

#[test]
fn provably_disjoint_regions_do_not_conflict() {
    // The ONLY "disjoint" claim the manifest may make: different files, no
    // call edge, no import adjacency, different names, graph available.
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
        ],
        vec![],
        vec![],
    );
    let report = compute_regions(inputs);
    assert!(
        report.conflicts.is_empty(),
        "disjoint regions must not carry a false conflict: {:?}",
        report.conflicts
    );
    assert!(!report.lower_bound);
}

#[test]
fn reason_priority_picks_the_most_structural_witness() {
    // Same file AND a call edge AND same name: the reason is the most
    // structural witness (same file), deterministically.
    let inputs = mk_inputs(
        vec![
            region("a.rs#flow#function", "flow", "a.rs", 1, 9),
            region("a.rs#flow_helper#function", "flow", "a.rs", 12, 20),
        ],
        vec![("a.rs#flow#function", "a.rs#flow_helper#function")],
        vec![],
    );
    let report = compute_regions(inputs);
    assert_eq!(
        conflict_reasons(&report),
        vec![(
            "a.rs#flow#function",
            "a.rs#flow_helper#function",
            "same file"
        )]
    );
}

#[test]
fn caller_callee_and_transitive_independence() {
    // a calls b; c is independent of both. Only (a,b) conflicts.
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
            region("c.rs#gamma#function", "gamma", "c.rs", 1, 9),
        ],
        vec![("a.rs#alpha#function", "b.rs#beta#function")],
        vec![],
    );
    let report = compute_regions(inputs);
    assert_eq!(
        conflict_reasons(&report),
        vec![("a.rs#alpha#function", "b.rs#beta#function", "call edge")],
        "transitive independence is not a conflict: c shares no edge with a or b"
    );
}

// ---------------------------------------------------------------------------
// merge-order layers
// ---------------------------------------------------------------------------

#[test]
fn callees_merge_before_callers() {
    // a calls b: b is layer 0, a is layer 1.
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
        ],
        vec![("a.rs#alpha#function", "b.rs#beta#function")],
        vec![],
    );
    let report = compute_regions(inputs);
    let layers: Vec<(usize, Vec<&str>)> = report
        .layers
        .iter()
        .map(|l| (l.layer, l.regions.iter().map(String::as_str).collect()))
        .collect();
    assert_eq!(
        layers,
        vec![
            (0, vec!["b.rs#beta#function"]),
            (1, vec!["a.rs#alpha#function"])
        ],
        "a caller merges after the callee it calls"
    );
}

#[test]
fn chain_layers_count_call_depth() {
    // a → b → c: c is 0, b is 1, a is 2.
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
            region("c.rs#gamma#function", "gamma", "c.rs", 1, 9),
        ],
        vec![
            ("a.rs#alpha#function", "b.rs#beta#function"),
            ("b.rs#beta#function", "c.rs#gamma#function"),
        ],
        vec![],
    );
    let report = compute_regions(inputs);
    let layers: Vec<(usize, Vec<&str>)> = report
        .layers
        .iter()
        .map(|l| (l.layer, l.regions.iter().map(String::as_str).collect()))
        .collect();
    assert_eq!(
        layers,
        vec![
            (0, vec!["c.rs#gamma#function"]),
            (1, vec!["b.rs#beta#function"]),
            (2, vec!["a.rs#alpha#function"]),
        ]
    );
}

#[test]
fn mutual_recursion_shares_one_layer() {
    // a ↔ b (one SCC) must merge together: same layer, and they conflict.
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
        ],
        vec![
            ("a.rs#alpha#function", "b.rs#beta#function"),
            ("b.rs#beta#function", "a.rs#alpha#function"),
        ],
        vec![],
    );
    let report = compute_regions(inputs);
    assert_eq!(report.layers.len(), 1, "an SCC condenses to one layer");
    assert_eq!(report.layers[0].layer, 0);
    assert_eq!(report.layers[0].regions.len(), 2);
    assert_eq!(
        conflict_reasons(&report),
        vec![("a.rs#alpha#function", "b.rs#beta#function", "call edge")],
        "mutual recursion conflicts: the cycle is not parallelizable"
    );
}

#[test]
fn diamond_layers() {
    // a → b, a → c, b → d, c → d: d is 0, b and c are 1, a is 2.
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
            region("c.rs#gamma#function", "gamma", "c.rs", 1, 9),
            region("d.rs#delta#function", "delta", "d.rs", 1, 9),
        ],
        vec![
            ("a.rs#alpha#function", "b.rs#beta#function"),
            ("a.rs#alpha#function", "c.rs#gamma#function"),
            ("b.rs#beta#function", "d.rs#delta#function"),
            ("c.rs#gamma#function", "d.rs#delta#function"),
        ],
        vec![],
    );
    let report = compute_regions(inputs);
    let layers: Vec<(usize, Vec<&str>)> = report
        .layers
        .iter()
        .map(|l| (l.layer, l.regions.iter().map(String::as_str).collect()))
        .collect();
    assert_eq!(
        layers,
        vec![
            (0, vec!["d.rs#delta#function"]),
            (1, vec!["b.rs#beta#function", "c.rs#gamma#function"]),
            (2, vec!["a.rs#alpha#function"]),
        ],
        "layer is the longest path to a sink"
    );
}

#[test]
fn no_call_edges_puts_every_region_in_layer_zero() {
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
        ],
        vec![],
        vec![],
    );
    let report = compute_regions(inputs);
    assert_eq!(report.layers.len(), 1);
    assert_eq!(report.layers[0].layer, 0);
    assert_eq!(report.layers[0].regions.len(), 2);
}

#[test]
fn graph_unavailable_collapses_layers_to_zero() {
    let mut inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
        ],
        vec![("a.rs#alpha#function", "b.rs#beta#function")],
        vec![],
    );
    inputs.graph_available = false;
    let report = compute_regions(inputs);
    assert_eq!(
        report.layers,
        vec![Layer {
            layer: 0,
            regions: vec!["a.rs#alpha#function".into(), "b.rs#beta#function".into()],
        }],
        "no graph means no ordering evidence: one layer, and every pair conflicts"
    );
}

// ---------------------------------------------------------------------------
// shared files
// ---------------------------------------------------------------------------

#[test]
fn file_imported_by_two_regions_is_shared_with_witness() {
    // a.rs and b.rs both import types.rs: types.rs is shared, and the
    // importers are the witness.
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
        ],
        vec![],
        vec![("a.rs", "types.rs"), ("b.rs", "types.rs")],
    );
    let report = compute_regions(inputs);
    assert_eq!(
        report.shared_files,
        vec![SharedFile {
            file: "types.rs".into(),
            imported_by: vec!["a.rs".into(), "b.rs".into()],
        }]
    );
}

#[test]
fn file_imported_by_one_region_is_not_shared() {
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
        ],
        vec![],
        vec![("a.rs", "types.rs")],
    );
    let report = compute_regions(inputs);
    assert!(
        report.shared_files.is_empty(),
        "a single importer does not make a file shared"
    );
}

#[test]
fn shared_file_witness_lists_every_importer() {
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
            region("c.rs#gamma#function", "gamma", "c.rs", 1, 9),
        ],
        vec![],
        vec![
            ("a.rs", "types.rs"),
            ("c.rs", "types.rs"),
            ("b.rs", "util.rs"),
        ],
    );
    let report = compute_regions(inputs);
    assert_eq!(report.shared_files.len(), 1);
    assert_eq!(report.shared_files[0].file, "types.rs");
    assert_eq!(report.shared_files[0].imported_by, vec!["a.rs", "c.rs"]);
}

// ---------------------------------------------------------------------------
// epistemics
// ---------------------------------------------------------------------------

#[test]
fn unresolved_same_name_calls_force_lower_bound() {
    let mut inputs = mk_inputs(
        vec![region("a.rs#flow#function", "flow", "a.rs", 1, 9)],
        vec![],
        vec![],
    );
    inputs.unresolved_same_name = 3;
    let report = compute_regions(inputs);
    assert!(report.lower_bound);
    assert!(
        report
            .caps
            .iter()
            .any(|c| c.contains("3 unresolved call site(s)")),
        "the cap must name the count: {:?}",
        report.caps
    );
}

#[test]
fn unknown_line_range_forces_lower_bound_and_names_the_count() {
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 4, 0),
        ],
        vec![],
        vec![],
    );
    let report = compute_regions(inputs);
    assert!(report.lower_bound);
    assert!(
        report
            .caps
            .iter()
            .any(|c| c.contains("1 region(s) have an unknown line range")),
        "the cap must name the count: {:?}",
        report.caps
    );
}

#[test]
fn range_unknown_covers_inverted_and_single_line_ranges() {
    // Inverted range (end < start, non-zero end): unknown.
    let inputs = mk_inputs(
        vec![region("a.rs#alpha#function", "alpha", "a.rs", 5, 3)],
        vec![],
        vec![],
    );
    let report = compute_regions(inputs);
    assert!(report.lower_bound, "inverted range must be unknown");

    // Single-line range (start == end): known.
    let inputs = mk_inputs(
        vec![region("a.rs#alpha#function", "alpha", "a.rs", 3, 3)],
        vec![],
        vec![],
    );
    let report = compute_regions(inputs);
    assert!(!report.lower_bound, "single-line range must be known");
}

#[test]
fn caller_caps_ride_the_envelope() {
    let mut inputs = mk_inputs(
        vec![region("a.rs#alpha#function", "alpha", "a.rs", 1, 9)],
        vec![],
        vec![],
    );
    inputs.caps = vec!["content probe truncated at 500 matches".to_string()];
    let report = compute_regions(inputs);
    assert!(report.lower_bound);
    assert!(
        report
            .caps
            .iter()
            .any(|c| c.contains("content probe truncated")),
        "a caller cap must reach the manifest envelope: {:?}",
        report.caps
    );
}

#[test]
fn clean_input_is_not_a_lower_bound() {
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
        ],
        vec![],
        vec![],
    );
    let report = compute_regions(inputs);
    assert!(!report.lower_bound);
    assert!(report.caps.is_empty());
}

// ---------------------------------------------------------------------------
// regions + determinism
// ---------------------------------------------------------------------------

#[test]
fn regions_carry_line_ranges_and_context_reference() {
    let inputs = mk_inputs(
        vec![region("a.rs#alpha#function", "alpha", "a.rs", 12, 40)],
        vec![],
        vec![],
    );
    let report = compute_regions(inputs);
    assert_eq!(report.regions.len(), 1);
    let r = &report.regions[0];
    assert_eq!(r.uid, "a.rs#alpha#function");
    assert_eq!(r.name, "alpha");
    assert_eq!(r.file, "a.rs");
    assert_eq!(r.start_line, 12);
    assert_eq!(r.end_line, 40);
    assert_eq!(
        r.context_ref, r.uid,
        "the context reference is the uid `pixel pack-context` resolves"
    );
}

#[test]
fn regions_sort_by_file_then_line_then_uid() {
    let inputs = mk_inputs(
        vec![
            region("b.rs#zeta#function", "zeta", "b.rs", 1, 5),
            region("a.rs#beta#function", "beta", "a.rs", 20, 30),
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
        ],
        vec![],
        vec![],
    );
    let report = compute_regions(inputs);
    let uids: Vec<&str> = report.regions.iter().map(|r| r.uid.as_str()).collect();
    assert_eq!(
        uids,
        vec![
            "a.rs#alpha#function",
            "a.rs#beta#function",
            "b.rs#zeta#function"
        ]
    );
}

#[test]
fn compute_regions_is_deterministic() {
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("b.rs#beta#function", "beta", "b.rs", 1, 9),
            region("c.rs#gamma#function", "gamma", "c.rs", 1, 9),
        ],
        vec![
            ("a.rs#alpha#function", "b.rs#beta#function"),
            ("c.rs#gamma#function", "b.rs#beta#function"),
        ],
        vec![("a.rs", "types.rs"), ("c.rs", "types.rs")],
    );
    let first = compute_regions(inputs.clone());
    let second = compute_regions(inputs);
    assert_eq!(first, second, "same input must give byte-identical output");
}

#[test]
fn duplicate_uids_collapse_to_one_region() {
    let inputs = mk_inputs(
        vec![
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
            region("a.rs#alpha#function", "alpha", "a.rs", 1, 9),
        ],
        vec![],
        vec![],
    );
    let report = compute_regions(inputs);
    assert_eq!(report.regions.len(), 1, "a uid identifies one region");
    assert!(
        report.conflicts.is_empty(),
        "a duplicated region must not conflict with itself"
    );
}

#[test]
fn empty_input_gives_empty_manifest() {
    let report = compute_regions(RegionsInputs::default());
    assert!(report.regions.is_empty());
    assert!(report.conflicts.is_empty());
    assert!(report.layers.is_empty());
    assert!(report.shared_files.is_empty());
    assert!(!report.lower_bound, "no regions, no claims, no caps");
}
