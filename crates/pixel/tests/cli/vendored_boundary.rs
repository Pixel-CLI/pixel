// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The owned mvp-* foundations keep their independent dependency boundary.
//! Their origin is recorded in ORIGIN.md; edits and tests belong here.
//! Each consumer reads through an adapter rather than coupling the index
//! to the rest of the product.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The crates allowed to depend on an `mvp-*` crate, and which ones each may
/// read:
///
/// - `pixel-graph` reads pixel-mvp's index (`mvp-index`, `mvp-store`,
///   `mvp-model`) through `pixel_graph::mvp_adapter` (ADR-0035 there);
/// - `pixel-cli` reads pixel-mvp's French-prompt rules (`mvp_model::language`,
///   `sub_words`, `prompt_terms`, `fold`, `Lexicon`: pure functions over a
///   prompt's text) through `execution_brief/french.rs` (ADR-0032, moved
///   under ADR-0036 there), and its BM25 terms (`mvp_model::terms`,
///   `sub_words`: the same tokeniser for a file's names and the prompt)
///   through `execution_brief/words.rs` (ADR-0030 and ADR-0031 there), and no
///   index crate: the plain-word leads read v1's own graph.
const READERS: [(&str, &[&str]); 2] = [
    ("pixel-graph", &["mvp-index", "mvp-model", "mvp-store"]),
    ("pixel-cli", &["mvp-model"]),
];

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
fn imported_mvp_crates_depend_only_on_mvp_model() {
    let members = members();
    let imported: Vec<_> = members
        .iter()
        .filter(|(name, _)| name.starts_with("mvp-"))
        .collect();
    assert!(
        imported.iter().any(|(name, _)| name == "mvp-model"),
        "no imported mvp-model in the workspace: {members:?}"
    );
    for (name, deps) in imported {
        let allowed: BTreeSet<String> = if name == "mvp-model" {
            BTreeSet::new()
        } else {
            BTreeSet::from(["mvp-model".to_string()])
        };
        assert!(
            deps.is_subset(&allowed),
            "`{name}` is an independent foundation and may depend on {allowed:?} only, not {deps:?}: \
             keep the independent foundation boundary (crates/{name}/ORIGIN.md)"
        );
    }
}

#[test]
fn only_the_named_readers_read_the_imported_crates() {
    for (name, deps) in members() {
        if name.starts_with("mvp-") {
            continue;
        }
        let allowed: &[&str] = READERS
            .iter()
            .find(|(reader, _)| *reader == name)
            .map_or(&[], |(_, crates)| crates);
        let read: Vec<_> = deps
            .iter()
            .filter(|dep| dep.starts_with("mvp-") && !allowed.contains(&dep.as_str()))
            .collect();
        assert!(
            read.is_empty(),
            "`{name}` depends on {read:?}; the imported mvp-* crates are read by {READERS:?} \
             only, each through its adapter (pixel_graph::mvp_adapter, the brief's french.rs and words.rs)"
        );
    }
}

/// In `pixel-cli`, two modules read pixel-mvp's rules, each for one part
/// moved under ADR-0036: the French words and the plain-word leads. A third
/// would couple more of the brief to the independent index implementation.
#[test]
fn only_the_briefs_french_and_words_modules_read_mvp_model_in_the_cli() {
    let src = repo_root().join("crates/pixel/src");
    let mut readers = Vec::new();
    let mut pending = vec![src.clone()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && std::fs::read_to_string(&path)
                    .unwrap()
                    .contains("mvp_model")
            {
                readers.push(path.strip_prefix(&src).unwrap().to_path_buf());
            }
        }
    }
    readers.sort();
    assert_eq!(
        readers,
        [
            PathBuf::from("execution_brief/french.rs"),
            PathBuf::from("execution_brief/words.rs"),
        ],
        "pixel-cli reads mvp_model through execution_brief/french.rs and words.rs only"
    );
}
