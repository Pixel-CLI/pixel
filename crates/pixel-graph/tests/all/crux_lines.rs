// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The crux lines a graph build stores for each symbol: the guard and
//! early-exit lines of that symbol's own body, cut from the file it was
//! extracted from.
//!
//! Both write paths slice every symbol's body out of one split of the file
//! (#339): the full build (`build_graph`) and the incremental one
//! (`update_files`). A slice taken from the wrong lines, the wrong file or a
//! stale split would hand `impact`/`pack-context` another symbol's guards,
//! so each path is pinned on a file with two symbols.

use std::path::Path;

use pixel_graph::build::{build_graph, update_files};
use pixel_graph::store::GraphStore;

fn git(dir: &Path, args: &[&str]) {
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

/// The stored crux texts of the symbol called `name`.
fn crux_of(db: &Path, name: &str) -> Vec<String> {
    let store = GraphStore::open(db).unwrap();
    let id: i64 = store
        .conn()
        .query_row("SELECT id FROM symbols WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .unwrap();
    store
        .symbol_crux_by_id(id)
        .unwrap()
        .into_iter()
        .map(|c| c.text)
        .collect()
}

const V1: &str = "\
pub fn first(x: i32) -> i32 {
    if x < 0 {
        return 0;
    }
    x
}

pub fn second(n: u32) -> u32 {
    while n > 10 {
        panic!(\"too big\");
    }
    n
}
";

/// `first` gains a guard and `second` loses its loop: the incremental
/// path must store the new bodies, not the ones of the first build.
const V2: &str = "\
pub fn first(x: i32) -> i32 {
    if x < 0 {
        return 0;
    }
    if x > 100 {
        return 100;
    }
    x
}

pub fn second(n: u32) -> u32 {
    n + 1
}
";

#[test]
fn each_symbol_keeps_its_own_guard_lines_through_both_build_paths() {
    // Dropped at the end of the test, and on a failed assertion's unwind.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("lib.rs"), V1).unwrap();
    git(root, &["init", "-q"]);
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "v1"]);
    let db = root.join(".pixel").join("graph.v2.db");

    build_graph(root, &db).unwrap();
    assert_eq!(crux_of(&db, "first"), ["if x < 0 {", "return 0;"]);
    assert_eq!(
        crux_of(&db, "second"),
        ["while n > 10 {", "panic!(\"too big\");"]
    );

    std::fs::write(src.join("lib.rs"), V2).unwrap();
    update_files(root, &db, &[("src/lib.rs", false)]).unwrap();
    assert_eq!(
        crux_of(&db, "first"),
        ["if x < 0 {", "return 0;", "if x > 100 {", "return 100;"]
    );
    assert_eq!(crux_of(&db, "second"), Vec::<String>::new());
}
