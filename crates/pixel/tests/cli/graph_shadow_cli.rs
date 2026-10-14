// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel graph-shadow` end to end: the JSON a script reads, and that the
//! comparison leaves the repository's own graph alone.

use crate::support::{Scratch, git, pixel_command};

/// `run` calls `Path::join` and the crate's own `paths::join` exists: this
/// graph links the first to the second by name, pixel-mvp does not.
fn fixture(tag: &str) -> Scratch {
    let dir = Scratch::for_test("graph-shadow-cli", tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub mod paths;\nuse std::path::Path;\npub fn run(p: &Path) -> usize {\n    p.join(\"a\").iter().count() + paths::size()\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/paths.rs"),
        "pub fn join(a: &str) -> String {\n    a.to_string()\n}\npub fn size() -> usize {\n    1\n}\n",
    )
    .unwrap();
    // pixel-mvp's Rust rules resolve a module path from a Cargo crate root.
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), ".pixel/\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "baseline"]);
    dir
}

#[test]
fn the_json_counts_both_graphs_and_the_repository_graph_is_not_built() {
    let dir = fixture("json");
    let out = pixel_command()
        .args(["graph-shadow", "--json"])
        .arg(&*dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    // run -> paths::size is in both; run -> paths::join only in this graph.
    assert_eq!(report["calls"]["both"], 1, "{report}");
    assert_eq!(report["calls"]["v1_only"], 1, "{report}");
    assert_eq!(report["calls"]["mvp_only"], 0, "{report}");
    assert_eq!(report["calls"]["v1_only_by"]["mvp_abstains"], 1, "{report}");
    assert_eq!(
        report["examples"]["v1_only"],
        serde_json::json!(["run -> join (src/lib.rs:4)"]),
        "{report}"
    );
    assert!(report["mvp_commit"].as_str().is_some_and(|c| c.len() == 7));
    assert!(!dir.join(".pixel/graph.v2.db").exists());
    assert!(!dir.join(".pixel/.pixel-mvp").exists());
}

#[test]
fn outside_a_git_work_tree_it_fails_with_a_message() {
    let dir = Scratch::for_test("graph-shadow-cli", "no-git");
    std::fs::write(dir.join("lib.rs"), "pub fn f() {}\n").unwrap();
    let out = pixel_command()
        .args(["graph-shadow"])
        .arg(&*dir)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("graph-shadow:"), "{stderr}");
}
