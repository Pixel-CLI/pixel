// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The vendored `mvp-*` crates stay a copy of pixel-mvp
//! (`crates/mvp-*/VENDORED.md`). A sync is `scripts/vendor-mvp.sh` plus the
//! gates, so these crates may not grow a dependency on a `pixel-*` crate,
//! and they keep pixel-mvp's own shape: `mvp-model` depends on no internal
//! crate, every other `mvp-*` crate on `mvp-model` only (pixel-mvp's
//! ADR-0013). On the other side, only `pixel-graph` reads them: a second
//! reader would tie another crate's code to a copy it cannot edit.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The one crate allowed to depend on an `mvp-*` crate.
const READERS: [&str; 1] = ["pixel-graph"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// `(package name, internal deps of every kind)` per workspace member.
fn members() -> Vec<(String, BTreeSet<String>)> {
    let root = repo_root();
    let workspace: toml_edit::DocumentMut = std::fs::read_to_string(root.join("Cargo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    workspace["workspace"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| {
            let path = root.join(member.as_str().unwrap()).join("Cargo.toml");
            let doc: toml_edit::DocumentMut =
                std::fs::read_to_string(path).unwrap().parse().unwrap();
            let name = doc["package"]["name"].as_str().unwrap().to_string();
            let deps = ["dependencies", "dev-dependencies", "build-dependencies"]
                .iter()
                .filter_map(|table| doc.get(table).and_then(toml_edit::Item::as_table_like))
                .flat_map(|deps| {
                    deps.iter()
                        .map(|(dep, _)| dep.to_string())
                        .collect::<Vec<_>>()
                })
                .filter(|dep| {
                    (dep.starts_with("pixel-") || dep.starts_with("mvp-")) && *dep != name
                })
                .collect();
            (name, deps)
        })
        .collect()
}

#[test]
fn vendored_mvp_crates_depend_only_on_mvp_model() {
    let members = members();
    let vendored: Vec<_> = members
        .iter()
        .filter(|(name, _)| name.starts_with("mvp-"))
        .collect();
    assert!(
        vendored.iter().any(|(name, _)| name == "mvp-model"),
        "no vendored mvp-model in the workspace: {members:?}"
    );
    for (name, deps) in vendored {
        let allowed: BTreeSet<String> = if name == "mvp-model" {
            BTreeSet::new()
        } else {
            BTreeSet::from(["mvp-model".to_string()])
        };
        assert!(
            deps.is_subset(&allowed),
            "`{name}` is a copy of pixel-mvp and may depend on {allowed:?} only, not {deps:?}: \
             a change of its dependencies lands in pixel-mvp first (crates/{name}/VENDORED.md)"
        );
    }
}

#[test]
fn only_pixel_graph_reads_the_vendored_crates() {
    for (name, deps) in members() {
        if name.starts_with("mvp-") || READERS.contains(&name.as_str()) {
            continue;
        }
        let read: Vec<_> = deps.iter().filter(|dep| dep.starts_with("mvp-")).collect();
        assert!(
            read.is_empty(),
            "`{name}` depends on {read:?}; only {READERS:?} reads the vendored mvp-* crates, \
             through its adapter (pixel_graph::mvp_adapter)"
        );
    }
}
