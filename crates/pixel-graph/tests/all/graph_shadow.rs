// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel_graph::shadow` on the adapter's fixture (`mvp_adapter.rs`): the
//! counts `pixel graph-shadow` prints, each traced to a site of the fixture.

use pixel_graph::shadow::{Calls, Imports, Overlap, Sides, V1Only, shadow};

#[test]
fn the_shadow_counts_what_each_resolver_links_alone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    crate::mvp_adapter::fixture(&root);
    let report = shadow(&root, 10).unwrap();
    assert_eq!(
        report.calls,
        Calls {
            // Both: run -> open, run -> helper, run -> Store::len (`s` is
            // annotated `S`), helper -> open.
            overlap: Overlap {
                v1: 8,
                mvp: 4,
                both: 4,
                v1_only: 4,
                mvp_only: 0,
            },
            v1_exact: 1,
            v1_probable: 7,
            v1_only_by: V1Only {
                exact: 0,
                probable: 4,
                // `.count()` and `.unwrap()` reach env.py's functions.
                into_other_language: 2,
                method_receiver: 4,
                path_receiver: 0,
                no_receiver: 0,
                mvp_abstains: 4,
                mvp_resolves_elsewhere: 0,
                mvp_has_no_site: 0,
            },
        }
    );
    // `helper -> Store::len` is right, and pixel-mvp's rules do not reach
    // it: the receiver's type is a function's return.
    let mut examples = report.examples.v1_only.clone();
    examples.sort();
    assert_eq!(
        examples,
        vec![
            "first -> unwrap (src/lib.rs:15)".to_string(),
            "helper -> Store::len (src/util.rs:2)".to_string(),
            "run -> count (src/lib.rs:11)".to_string(),
            "run -> join (src/lib.rs:9)".to_string(),
        ]
    );
    assert_eq!(
        report.imports,
        Imports {
            rows: 4,
            v1_resolved: 3,
            mvp_resolved: 3,
            same: 3,
            v1_only: 0,
            mvp_only: 0,
            different_file: 0,
        }
    );
    // pixel-mvp keeps `p.join`, `.count`, `v.unwrap` and `.len` unresolved
    // where this graph guessed; `x.iter` and `a.to_string` name no symbol of
    // the graph, so no `unresolved_calls` row records them on either side:
    // pixel-mvp's drops them (#1162), this resolver's records each once per
    // file in `unresolved_names` (#1163).
    assert_eq!(report.unresolved, Sides { v1: 0, mvp: 4 });
    // `tests::t` -> helper.
    assert_eq!(report.dropped_no_caller, 1);
    // env.py's `count` -> helper, linked by name.
    assert_eq!(report.into_rust_from_other_languages, 1);
    // Both graphs were built in a scratch directory.
    assert!(!root.join(".pixel").exists());
}

#[test]
fn examples_are_capped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    crate::mvp_adapter::fixture(&root);
    assert_eq!(shadow(&root, 1).unwrap().examples.v1_only.len(), 1);
}

/// `--lang ruby` on the Ruby fixture (`mvp_adapter_ruby.rs`): the merge's
/// counts, each traced to a site.
#[test]
fn the_ruby_shadow_counts_the_merge_site_by_site() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    crate::mvp_adapter_ruby::fixture(&root);
    let report = pixel_graph::shadow::ruby::shadow(&root, 10).unwrap();
    // Calls: `title` replaced (pixel-mvp proves the `attr_accessor` reader),
    // `item.save` and `sort` (into JavaScript) kept, `compute`, `Box.new`
    // and `.pack` in both.
    assert_eq!(
        (
            report.calls.v1,
            report.calls.hybrid,
            report.calls.both,
            report.calls.v1_kept,
            report.calls.v1_replaced
        ),
        (6, 5, 3, 2, 1)
    );
    assert_eq!((report.routes.both, report.associations.both), (2, 1));
    // The replaced site stays unresolved: one row more.
    assert_eq!(report.unresolved.hybrid, report.unresolved.v1 + 1);
    assert_eq!(
        report.examples.v1_replaced,
        vec!["Card#label -> Book#title (app/models/card.rb:8)"]
    );
    assert!(report.examples.mvp_added.is_empty());
    // Nothing under the repository: both graphs live in a scratch directory.
    assert!(!root.join(".pixel").exists());
}
