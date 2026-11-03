// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The adapter in front of pixel-mvp's index (`pixel_graph::mvp_adapter`),
//! on a small Rust crate whose sites this graph's name-only resolver gets
//! wrong: what the 2026-10-14 comparison found on pixel's own repository,
//! one case of each.

use std::fs;
use std::path::Path;

use mvp_model::Graph;
use pixel_graph::build::build_graph_from;
use pixel_graph::mvp_adapter::{RustSource, Unresolved, refresh, refresh_changes, rust_rows};
use pixel_graph::{EdgeKind, GraphStore, Tier};

pub(crate) fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A crate with a `join` of its own in `paths.rs`, `Path::join` called in
/// `lib.rs`, an imported `open`, a glob import, a Python `unwrap` and a
/// Python `count` calling `helper` by name, and a test module calling
/// `helper`. Line numbers are the assertions'.
pub(crate) fn fixture(root: &Path) {
    let files = [
        (
            "src/lib.rs",
            [
                "pub mod paths;",                            // 1
                "pub mod store;",                            // 2
                "pub mod util;",                             // 3
                "use std::path::Path;",                      // 4
                "use crate::store::{open, Store as S};",     // 5
                "use util::*;",                              // 6
                "",                                          // 7
                "pub fn run(p: &Path) -> usize {",           // 8
                "    let x = p.join(\"a\");",                // 9
                "    let s: S = open();",                    // 10
                "    s.len() + helper() + x.iter().count()", // 11
                "}",                                         // 12
                "",                                          // 13
                "pub fn first(v: Option<u8>) -> u8 {",       // 14
                "    v.unwrap()",                            // 15
                "}",                                         // 16
                "",                                          // 17
                "#[cfg(test)]",                              // 18
                "mod tests {",                               // 19
                "    use super::*;",                         // 20
                "    #[test]",                               // 21
                "    fn t() {",                              // 22
                "        let n = helper();",                 // 23
                "        assert_eq!(n, 1);",                 // 24
                "    }",                                     // 25
                "}",                                         // 26
            ]
            .join("\n"),
        ),
        (
            "src/paths.rs",
            "pub fn join(a: &str) -> String {\n    a.to_string()\n}\n".to_string(),
        ),
        (
            "src/store.rs",
            [
                "pub struct Store {",
                "    n: usize,",
                "}",
                "impl Store {",
                "    pub fn len(&self) -> usize {",
                "        self.n",
                "    }",
                "}",
                "pub fn open() -> Store {",
                "    Store { n: 1 }",
                "}",
            ]
            .join("\n"),
        ),
        (
            "src/util.rs",
            "pub fn helper() -> usize {\n    crate::store::open().len()\n}\n".to_string(),
        ),
        (
            "Cargo.toml",
            "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n".to_string(),
        ),
        (
            "scripts/env.py",
            "def unwrap(value):\n    return value\n\n\ndef count():\n    return helper()\n"
                .to_string(),
        ),
    ];
    for (path, body) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    git(root, &["init", "-q"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "fixture"]);
}

/// The fixture's graph, built by this graph's own resolver whatever the
/// switch says, and what
/// pixel-mvp's index decides at its Rust sites.
fn built() -> (
    tempfile::TempDir,
    GraphStore,
    pixel_graph::mvp_adapter::RustRows,
) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    let db = root.join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph_from(&root, &db, RustSource::V1).unwrap();
    let store = GraphStore::open(&db).unwrap();
    let mvp = refresh(&root, &root.join(".pixel")).unwrap();
    let rows = rust_rows(&store, &mvp).unwrap();
    (dir, store, rows)
}

fn id(store: &GraphStore, path: &str, qualified: &str) -> i64 {
    store
        .conn()
        .query_row(
            "SELECT s.id FROM symbols s JOIN files f ON f.id = s.file_id
              WHERE f.path = ?1 AND s.qualified = ?2",
            [path, qualified],
            |r| r.get(0),
        )
        .unwrap()
}

fn file_id(store: &GraphStore, path: &str) -> i64 {
    store.file_by_path(path).unwrap().unwrap().id
}

#[test]
fn path_join_is_left_unresolved_where_the_name_only_resolver_links_a_repo_join() {
    let (_dir, store, rows) = built();
    let run = id(&store, "src/lib.rs", "run");
    let join = id(&store, "src/paths.rs", "join");
    // This graph's own resolver links `p.join` to the crate's `join` (the
    // comparison found 14 such `Path::join` edges on pixel's repository).
    let own = store.edges_from(run, Some(EdgeKind::Calls)).unwrap();
    assert!(
        own.iter().any(|e| e.dst_id == join && e.site_line == 9),
        "precondition: the name-only resolver links Path::join: {own:?}"
    );
    // pixel-mvp's rules do not: no edge of either kind reaches `join`, and
    // the site is kept unresolved, with this graph's receiver spelling.
    assert!(
        rows.calls
            .iter()
            .chain(&rows.references)
            .all(|e| e.dst_id != join),
        "{rows:?}"
    );
    assert!(rows.unresolved.contains(&Unresolved {
        file_id: file_id(&store, "src/lib.rs"),
        name: "join".to_string(),
        enclosing: Some(run),
        site_line: 9,
        receiver: Some(".p".to_string()),
        kind: "calls",
    }));
}

#[test]
fn a_call_the_rules_prove_is_exact_with_the_name_the_site_wrote() {
    let (_dir, store, rows) = built();
    let run = id(&store, "src/lib.rs", "run");
    let helper = id(&store, "src/util.rs", "helper");
    let open = id(&store, "src/store.rs", "open");
    let edge = |dst: i64, line: u32| {
        rows.calls
            .iter()
            .find(|e| e.src_id == run && e.dst_id == dst && e.site_line == line)
            .cloned()
    };
    let to_open = edge(open, 10).expect("run -> open, imported by `use crate::store::open`");
    assert_eq!(
        (
            to_open.kind,
            to_open.tier,
            to_open.callee.as_deref(),
            to_open.receiver.as_deref()
        ),
        (EdgeKind::Calls, Tier::Exact, Some("open"), None)
    );
    // Through a glob import.
    assert!(edge(helper, 11).is_some(), "{:?}", rows.calls);
    // A path call keeps its path as the receiver.
    let from_helper = rows
        .calls
        .iter()
        .find(|e| e.src_id == helper && e.dst_id == open)
        .expect("helper -> crate::store::open");
    assert_eq!(from_helper.receiver.as_deref(), Some("crate::store"));
    // Every call edge is exact: the rules link a site only when it is proven.
    assert!(rows.calls.iter().all(|e| e.tier == Tier::Exact));
}

#[test]
fn a_rust_call_never_reaches_another_language() {
    let (_dir, store, rows) = built();
    let first = id(&store, "src/lib.rs", "first");
    let python = id(&store, "scripts/env.py", "unwrap");
    // The name-only resolver sends `Option::unwrap` to the Python function
    // (7 of the 54 wrong v1 calls sampled on pixel's repository).
    let own = store.edges_from(first, Some(EdgeKind::Calls)).unwrap();
    assert!(
        own.iter().any(|e| e.dst_id == python),
        "precondition: {own:?}"
    );
    let rust_symbols: Vec<i64> = {
        let mut stmt = store
            .conn()
            .prepare(
                "SELECT s.id FROM symbols s JOIN files f ON f.id = s.file_id
                  WHERE f.lang = 'rust'",
            )
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    for edge in rows.calls.iter().chain(&rows.references) {
        assert!(rust_symbols.contains(&edge.dst_id), "{edge:?}");
        assert!(rust_symbols.contains(&edge.src_id), "{edge:?}");
    }
    assert!(rows.unresolved.iter().any(|u| u.enclosing == Some(first)
        && u.name == "unwrap"
        && u.receiver.as_deref() == Some(".v")));
}

#[test]
fn imports_take_the_file_the_rules_resolved_or_none() {
    let (_dir, store, rows) = built();
    let lib = file_id(&store, "src/lib.rs");
    let store_rs = file_id(&store, "src/store.rs");
    let util_rs = file_id(&store, "src/util.rs");
    let mut by_path: Vec<(String, Option<i64>)> = rows
        .imports
        .iter()
        .map(|&(import, target)| {
            let (file, path): (i64, String) = store
                .conn()
                .query_row(
                    "SELECT file_id, path FROM imports WHERE id = ?1",
                    [import],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(file, lib);
            (path, target)
        })
        .collect();
    by_path.sort();
    assert_eq!(
        by_path,
        vec![
            ("crate::store::Store".to_string(), Some(store_rs)),
            ("crate::store::open".to_string(), Some(store_rs)),
            ("std::path::Path".to_string(), None),
            ("util::*".to_string(), Some(util_rs)),
        ]
    );
}

#[test]
fn test_code_this_graph_does_not_extract_leaves_no_row() {
    let (_dir, store, rows) = built();
    let helper = id(&store, "src/util.rs", "helper");
    // `tests::t` calls `helper` on line 23; this graph has no symbol for
    // `t`, so the edge is dropped and counted, never hung on another symbol.
    assert!(
        rows.calls.iter().all(|e| e.site_line != 23),
        "{:?}",
        rows.calls
    );
    assert!(rows.dropped.no_caller >= 1, "{:?}", rows.dropped);
    assert!(
        rows.unresolved.iter().all(|u| u.site_line < 18),
        "{:?}",
        rows.unresolved
    );
    // The helper itself is still a target where real code calls it.
    assert!(rows.calls.iter().any(|e| e.dst_id == helper));
}

#[test]
fn a_refresh_after_an_edit_reads_the_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    let db = root.join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph_from(&root, &db, RustSource::V1).unwrap();
    refresh(&root, &root.join(".pixel")).unwrap();
    // `first` now calls `helper` (through the glob import) as well.
    let lib = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        lib.replace("    v.unwrap()", "    v.unwrap() + helper() as u8"),
    )
    .unwrap();
    build_graph_from(&root, &db, RustSource::V1).unwrap();
    let store = GraphStore::open(&db).unwrap();
    let rows = rust_rows(&store, &refresh(&root, &root.join(".pixel")).unwrap()).unwrap();
    let first = id(&store, "src/lib.rs", "first");
    let helper = id(&store, "src/util.rs", "helper");
    assert!(
        rows.calls
            .iter()
            .any(|e| e.src_id == first && e.dst_id == helper && e.site_line == 15),
        "{:?}",
        rows.calls
    );
}

/// The hybrid's update merges again only the files a refresh changed, and
/// trusts the rows it kept only when the refresh started from the
/// generation the graph was merged against: so a refresh must name that
/// generation as the index published it, and name the files whose rows
/// it wrote again.
#[test]
fn a_refresh_names_the_generation_it_started_from_and_the_files_it_wrote_again() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    let state = root.join(".pixel");
    let (first, changes) = refresh_changes(&root, &state).unwrap();
    assert_eq!(changes.before, None, "no generation before the first");
    let published = first.provenance().unwrap().generation;
    drop(first);
    let (again, changes) = refresh_changes(&root, &state).unwrap();
    assert_eq!(changes.before.as_deref(), Some(published.as_str()));
    assert!(changes.rewritten.is_empty() && changes.redirected.is_empty());
    assert_eq!(again.provenance().unwrap().generation, published);
    drop(again);
    let lib = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        format!("{lib}\npub fn added() {{}}\n"),
    )
    .unwrap();
    let (_, changes) = refresh_changes(&root, &state).unwrap();
    assert_eq!(changes.before.as_deref(), Some(published.as_str()));
    assert_eq!(
        changes.rewritten.iter().collect::<Vec<_>>(),
        vec!["src/lib.rs"]
    );
}

#[test]
fn outside_a_git_work_tree_there_is_no_index() {
    let dir = tempfile::tempdir().unwrap();
    let err = refresh(dir.path(), &dir.path().join(".pixel"))
        .err()
        .expect("pixel-mvp lists files through git");
    assert!(!err.to_string().is_empty());
    assert!(!dir.path().join(".pixel/.pixel-mvp").exists());
}

// pixel-mvp's writer keeps a copy of git's index in its own state
// directory (`git_index`, ADR-0039 there), named `git-index.<key>`. This
// repository's state is `.pixel/` alone, and its `## On-disk state` lists
// what `.pixel/.pixel-mvp/` holds: the adapter places that one copy there,
// makes no `.pixel-mvp/` at the work tree's top level, and leaves git's own
// index as it found it.
#[cfg(unix)]
#[test]
fn a_refresh_writes_only_its_index_under_the_state_directory_it_is_given() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fixture(&root);
    let stamp = |path: &Path| {
        let meta = fs::symlink_metadata(path).unwrap();
        (meta.ino(), meta.len(), meta.mtime(), meta.mtime_nsec())
    };
    let git_index = root.join(".git/index");
    let before = stamp(&git_index);
    refresh(&root, &root.join(".pixel")).unwrap();
    refresh(&root, &root.join(".pixel")).unwrap();
    assert_eq!(stamp(&git_index), before, "git's index rewritten");
    assert!(!root.join(".pixel-mvp").exists());
    let mut held: Vec<String> = fs::read_dir(root.join(".pixel/.pixel-mvp"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    held.sort();
    // pixel-mvp's copy of git's index lives beside its index, one copy,
    // not under the root's `.pixel-mvp/` (ADR-0039 there).
    let copies: Vec<&String> = held
        .iter()
        .filter(|name| name.starts_with("git-index."))
        .collect();
    assert_eq!(copies.len(), 1, "{held:?}");
    assert!(held.contains(&"graph.db".to_owned()), "{held:?}");
}
