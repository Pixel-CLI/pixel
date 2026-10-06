// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Structural check kinds: pure functions over graph/diff facts.
//!
//! Each kind answers one structural question the graph and scope pipeline
//! already computes, as a total function over plain data — no repository, no
//! sandbox, no clock. Every finding carries a named witness, and every result
//! reports `complete | capped` honestly: when a cap truncates the evidence
//! behind a verdict, `complete` is false and the cap is named in `note`, so
//! a capped check never claims an absence it did not attest.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// Cap on listed witnesses per check. The verdict is always computed over the
/// full input; beyond the cap the witness list is truncated, `complete` is
/// false and `note` names the truncation.
pub const MAX_WITNESSES: usize = 100;

/// Cap on call edges compared by [`graph_resolves`]. Re-extraction only
/// rewrites edges that touch a changed file, so the compared set is exactly
/// the set of edges that could have changed; the cap bounds that set and is
/// named in the result when hit.
pub const MAX_GRAPH_EDGES: usize = 5_000;

/// The facts a verify caller gathers for structural checks, passed from the
/// CLI (which owns the repository, the graph store and the manifest) to the
/// runner (which owns receipts). `None` fields mean the fact is unavailable
/// — no targets manifest, no graph — and the corresponding check reports
/// `Unavailable` with a diagnostic, never a silent pass.
#[derive(Debug, Clone, Default)]
pub struct StructuralContext {
    /// Working-tree diff paths: tracked changes plus untracked files.
    pub diff_paths: Vec<String>,
    /// Union of every active task's scoped manifest paths; `None` when no
    /// targets manifest exists.
    pub manifest_paths: Option<Vec<String>>,
    /// Resolved call edges touching changed files, before re-extraction;
    /// `None` when no graph exists.
    pub graph_before: Option<Vec<CallEdge>>,
    /// The same edges after the changed files were re-extracted.
    pub graph_after: Option<Vec<CallEdge>>,
    /// Test-path conventions for [`tests_touched`].
    pub test_conventions: TestPathConventions,
}

/// The verdict of one structural check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuralResult {
    /// False when a finding exists, or when a cap makes the verdict a lower
    /// bound the check cannot certify.
    pub passed: bool,
    /// False when a named cap truncated the evidence behind the verdict.
    pub complete: bool,
    /// Every finding, each a named witness.
    pub witnesses: Vec<String>,
    /// Epistemics note: caps hit, abstentions, vacuous cases.
    #[serde(default)]
    pub note: Option<String>,
}

/// One resolved call edge, identified by its endpoints so it survives the
/// symbol-id churn of re-extraction. `src`/`dst` are `path::symbol` labels
/// naming both endpoints; `site_line` is the call site in the source file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallEdge {
    pub src: String,
    pub dst: String,
    pub site_line: u32,
}

/// Test-path conventions for [`tests_touched`], with documented defaults.
///
/// Defaults (mirroring `pixel_graph::concept::is_test_path`, the convention
/// the graph's own test suggestions use): a `tests`, `test` or `__tests__`
/// path segment, or a filename containing `.spec.` or `.test.`, or a stem
/// ending in `_test`. Repos with other conventions extend them with
/// [`TestPathConventions::with_segments`] / [`TestPathConventions::with_markers`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestPathConventions {
    /// Extra directory segments that mark a test path, beyond the defaults.
    pub extra_segments: Vec<String>,
    /// Extra filename substrings that mark a test path, beyond the defaults.
    pub extra_markers: Vec<String>,
}

impl Default for TestPathConventions {
    fn default() -> Self {
        Self::new()
    }
}

impl TestPathConventions {
    pub fn new() -> Self {
        Self {
            extra_segments: Vec::new(),
            extra_markers: Vec::new(),
        }
    }

    pub fn with_segments(mut self, segments: &[&str]) -> Self {
        self.extra_segments
            .extend(segments.iter().map(ToString::to_string));
        self
    }

    pub fn with_markers(mut self, markers: &[&str]) -> Self {
        self.extra_markers
            .extend(markers.iter().map(ToString::to_string));
        self
    }

    /// True when `path` matches a declared test-path convention.
    pub fn is_test_path(&self, path: &str) -> bool {
        let file = path.rsplit('/').next().unwrap_or(path);
        if file.contains(".spec.") || file.contains(".test.") {
            return true;
        }
        if let Some((stem, _)) = file.rsplit_once('.')
            && stem.ends_with("_test")
        {
            return true;
        }
        path.split('/').any(|seg| {
            seg == "tests" || seg == "test" || seg == "__tests__" || self.extra_segments.iter().any(|extra| extra == seg)
        }) || self
            .extra_markers
            .iter()
            .any(|marker| file.contains(marker.as_str()))
    }
}

/// `diff-in-scope`: working-tree diff paths are a subset of the scoped
/// manifest. Witness: each out-of-scope path.
pub fn diff_in_scope(diff_paths: &[String], manifest_paths: &[String]) -> StructuralResult {
    let manifest: HashSet<&str> = manifest_paths.iter().map(String::as_str).collect();
    let out_of_scope: Vec<&String> = diff_paths
        .iter()
        .filter(|path| !manifest.contains(path.as_str()))
        .collect();
    let mut witnesses: Vec<String> = out_of_scope
        .iter()
        .map(|path| format!("out of scope: {path}"))
        .collect();
    witnesses.sort();
    let truncated = witnesses.len() > MAX_WITNESSES;
    let complete = !truncated;
    if truncated {
        witnesses.truncate(MAX_WITNESSES);
    }
    let mut note = None;
    if truncated {
        note = Some(format!(
            "witnesses capped at {MAX_WITNESSES}; {} out-of-scope paths total",
            out_of_scope.len()
        ));
    }
    StructuralResult {
        passed: out_of_scope.is_empty(),
        complete,
        witnesses,
        note,
    }
}

/// `graph-resolves`: after the changed files are re-extracted, no
/// previously-resolved call edge may have become unresolved. Witness: each
/// dropped edge, named with both endpoints.
pub fn graph_resolves(before: &[CallEdge], after: &[CallEdge]) -> StructuralResult {
    let capped = before.len() > MAX_GRAPH_EDGES || after.len() > MAX_GRAPH_EDGES;
    let after_set: HashSet<&CallEdge> = after.iter().take(MAX_GRAPH_EDGES).collect();
    let mut dropped: Vec<String> = before
        .iter()
        .take(MAX_GRAPH_EDGES)
        .filter(|edge| !after_set.contains(*edge))
        .map(|edge| {
            format!(
                "dropped edge: {} -> {} (site line {})",
                edge.src, edge.dst, edge.site_line
            )
        })
        .collect();
    dropped.sort();
    let dropped_total = dropped.len();
    let witness_truncated = dropped_total > MAX_WITNESSES;
    let complete = !capped && !witness_truncated;
    if witness_truncated {
        dropped.truncate(MAX_WITNESSES);
    }
    let mut notes: Vec<String> = Vec::new();
    if capped {
        notes.push(format!(
            "edge comparison capped at {MAX_GRAPH_EDGES}; {} before / {} after compared",
            before.len().min(MAX_GRAPH_EDGES),
            after.len().min(MAX_GRAPH_EDGES)
        ));
    }
    if witness_truncated {
        notes.push(format!(
            "witnesses capped at {MAX_WITNESSES}; {dropped_total} dropped edges total"
        ));
    }
    let note = (!notes.is_empty()).then(|| notes.join("; "));
    StructuralResult {
        passed: dropped.is_empty() && complete,
        complete,
        witnesses: dropped,
        note,
    }
}

/// `tests-touched`: a non-test source change with zero test-file changes is
/// a finding. Witness: each changed non-test path. A clean tree, or a diff
/// that touches at least one test path, has no finding.
pub fn tests_touched(changed_paths: &[String], conventions: &TestPathConventions) -> StructuralResult {
    let non_test: Vec<&String> = changed_paths
        .iter()
        .filter(|path| !conventions.is_test_path(path))
        .collect();
    let test_changed = changed_paths.iter().any(|path| conventions.is_test_path(path));
    let finding = !non_test.is_empty() && !test_changed;
    let mut witnesses: Vec<String> = if finding {
        non_test
            .iter()
            .map(|path| format!("non-test change without any test change: {path}"))
            .collect()
    } else {
        Vec::new()
    };
    witnesses.sort();
    let truncated = witnesses.len() > MAX_WITNESSES;
    let complete = !truncated;
    if truncated {
        witnesses.truncate(MAX_WITNESSES);
    }
    let mut note = None;
    if truncated {
        note = Some(format!(
            "witnesses capped at {MAX_WITNESSES}; {} non-test changes total",
            non_test.len()
        ));
    } else if finding {
        note = Some(format!(
            "{} non-test changes with zero test-file changes",
            non_test.len()
        ));
    } else if !changed_paths.is_empty() && non_test.is_empty() {
        note = Some("every changed path is a test path".into());
    }
    StructuralResult {
        passed: !finding,
        complete,
        witnesses,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(src: &str, dst: &str, site_line: u32) -> CallEdge {
        CallEdge {
            src: src.into(),
            dst: dst.into(),
            site_line,
        }
    }

    #[test]
    fn diff_in_scope_passes_when_every_diff_path_is_in_the_manifest() {
        let result = diff_in_scope(
            &["src/a.rs".into(), "src/b.rs".into()],
            &["src/a.rs".into(), "src/b.rs".into(), "src/c.rs".into()],
        );
        assert!(result.passed);
        assert!(result.complete);
        assert!(result.witnesses.is_empty());
        assert!(result.note.is_none());
    }

    #[test]
    fn diff_in_scope_names_each_out_of_scope_path_as_witness() {
        let result = diff_in_scope(
            &["src/a.rs".into(), "docs/readme.md".into(), "scripts/x.sh".into()],
            &["src/a.rs".into()],
        );
        assert!(!result.passed);
        assert!(result.complete);
        assert_eq!(
            result.witnesses,
            vec![
                "out of scope: docs/readme.md".to_string(),
                "out of scope: scripts/x.sh".to_string(),
            ]
        );
    }

    #[test]
    fn diff_in_scope_vacuous_pass_on_empty_diff() {
        let result = diff_in_scope(&[], &["src/a.rs".into()]);
        assert!(result.passed);
        assert!(result.complete);
        assert!(result.witnesses.is_empty());
    }

    #[test]
    fn diff_in_scope_caps_witnesses_and_reports_incomplete() {
        let diff: Vec<String> = (0..(MAX_WITNESSES + 5))
            .map(|i| format!("docs/{i}.md"))
            .collect();
        let result = diff_in_scope(&diff, &[]);
        assert!(!result.passed);
        assert!(!result.complete);
        assert_eq!(result.witnesses.len(), MAX_WITNESSES);
        let note = result.note.expect("cap must be named");
        assert!(note.contains(&MAX_WITNESSES.to_string()), "{note}");
        assert!(note.contains(&(MAX_WITNESSES + 5).to_string()), "{note}");
    }

    #[test]
    fn graph_resolves_passes_when_no_edge_dropped() {
        let before = vec![
            edge("src/a.rs::foo", "src/b.rs::bar", 12),
            edge("src/b.rs::baz", "src/a.rs::foo", 34),
        ];
        let after = before.clone();
        let result = graph_resolves(&before, &after);
        assert!(result.passed);
        assert!(result.complete);
        assert!(result.witnesses.is_empty());
    }

    #[test]
    fn graph_resolves_names_each_dropped_edge_with_both_endpoints() {
        let before = vec![
            edge("src/a.rs::foo", "src/b.rs::bar", 12),
            edge("src/b.rs::baz", "src/a.rs::foo", 34),
        ];
        let after = vec![edge("src/b.rs::baz", "src/a.rs::foo", 34)];
        let result = graph_resolves(&before, &after);
        assert!(!result.passed);
        assert!(result.complete);
        assert_eq!(
            result.witnesses,
            vec!["dropped edge: src/a.rs::foo -> src/b.rs::bar (site line 12)".to_string()]
        );
    }

    #[test]
    fn graph_resolves_identifies_edges_by_endpoints_not_position() {
        // Same edge at a different position in the list still resolves.
        let before = vec![edge("a::f", "b::g", 3)];
        let after = vec![edge("x::y", "z::w", 9), edge("a::f", "b::g", 3)];
        let result = graph_resolves(&before, &after);
        assert!(result.passed);
        assert!(result.complete);
    }

    #[test]
    fn graph_resolves_edge_moved_to_a_new_site_line_is_dropped() {
        // Re-extraction rewrites the site line; the edge identity includes it,
        // so a moved site is reported rather than silently passing.
        let before = vec![edge("a::f", "b::g", 3)];
        let after = vec![edge("a::f", "b::g", 4)];
        let result = graph_resolves(&before, &after);
        assert!(!result.passed);
        assert_eq!(result.witnesses.len(), 1);
    }

    #[test]
    fn graph_resolves_vacuous_pass_with_no_edges() {
        let result = graph_resolves(&[], &[]);
        assert!(result.passed);
        assert!(result.complete);
        assert!(result.witnesses.is_empty());
    }

    #[test]
    fn graph_resolves_caps_compared_edges_and_reports_incomplete() {
        let before: Vec<CallEdge> = (0..(MAX_GRAPH_EDGES + 10))
            .map(|i| edge(&format!("a{i}::f"), "b::g", i as u32))
            .collect();
        let after: Vec<CallEdge> = (0..(MAX_GRAPH_EDGES + 10))
            .map(|i| edge(&format!("a{i}::f"), "b::g", i as u32 + 1))
            .collect();
        let result = graph_resolves(&before, &after);
        assert!(!result.passed);
        assert!(!result.complete);
        assert_eq!(result.witnesses.len(), MAX_WITNESSES);
        let note = result.note.expect("cap must be named");
        assert!(note.contains(&MAX_GRAPH_EDGES.to_string()), "{note}");
    }

    #[test]
    fn graph_resolves_capped_comparison_cannot_certify_absence() {
        // Every compared edge dropped: the verdict is a lower bound, not a pass.
        let before: Vec<CallEdge> = (0..MAX_GRAPH_EDGES)
            .map(|i| edge(&format!("a{i}::f"), "b::g", 1))
            .collect();
        let result = graph_resolves(&before, &[]);
        assert!(!result.passed);
        assert!(!result.complete);
    }

    #[test]
    fn tests_touched_finds_a_non_test_change_with_zero_test_changes() {
        let result = tests_touched(
            &["src/a.rs".into(), "src/b.rs".into()],
            &TestPathConventions::new(),
        );
        assert!(!result.passed);
        assert!(result.complete);
        assert_eq!(
            result.witnesses,
            vec![
                "non-test change without any test change: src/a.rs".to_string(),
                "non-test change without any test change: src/b.rs".to_string(),
            ]
        );
    }

    #[test]
    fn tests_touched_passes_when_a_test_path_changed() {
        let result = tests_touched(
            &["src/a.rs".into(), "tests/a_test.rs".into()],
            &TestPathConventions::new(),
        );
        assert!(result.passed);
        assert!(result.complete);
        assert!(result.witnesses.is_empty());
    }

    #[test]
    fn tests_touched_passes_on_a_clean_tree() {
        let result = tests_touched(&[], &TestPathConventions::new());
        assert!(result.passed);
        assert!(result.complete);
        assert!(result.witnesses.is_empty());
    }

    #[test]
    fn tests_touched_default_conventions_match_documented_test_paths() {
        let conventions = TestPathConventions::new();
        for path in [
            "tests/a.rs",
            "test/a.rs",
            "src/__tests__/a.ts",
            "src/a_test.rs",
            "src/a.spec.ts",
            "src/a.test.ts",
        ] {
            assert!(conventions.is_test_path(path), "{path}");
        }
        for path in ["src/a.rs", "src/main.ts", "docs/tests.md"] {
            assert!(!conventions.is_test_path(path), "{path}");
        }
    }

    #[test]
    fn tests_touched_extra_conventions_extend_the_documented_defaults() {
        let conventions = TestPathConventions::new()
            .with_segments(&["spec"])
            .with_markers(&["_spec_"]);
        assert!(conventions.is_test_path("spec/a.rs"));
        assert!(conventions.is_test_path("src/a_spec_b.rs"));
        // Defaults still hold.
        assert!(conventions.is_test_path("tests/a.rs"));
        assert!(!conventions.is_test_path("src/a.rs"));
    }

    #[test]
    fn tests_touched_caps_witnesses_and_reports_incomplete() {
        let changed: Vec<String> = (0..(MAX_WITNESSES + 3))
            .map(|i| format!("src/{i}.rs"))
            .collect();
        let result = tests_touched(&changed, &TestPathConventions::new());
        assert!(!result.passed);
        assert!(!result.complete);
        assert_eq!(result.witnesses.len(), MAX_WITNESSES);
        let note = result.note.expect("cap must be named");
        assert!(note.contains(&MAX_WITNESSES.to_string()), "{note}");
    }
}
