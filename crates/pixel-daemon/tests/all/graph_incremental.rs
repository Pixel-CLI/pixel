// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Integration: an existing `graph.db` follows the working tree file by
//! file instead of being rebuilt from scratch on the first graph op after
//! an edit.
//!
//! Why it matters: an agent's loop is "edit two files, then `changes` /
//! `impact`". With a rebuild-on-drift policy every cycle paid a full walk +
//! extraction of the whole tree (100 s on a 10 000-file Rails app in CI,
//! 36 s locally) although only the edited files changed. The graph must
//! re-extract those files only, drop deleted ones, and say so in
//! `graph_build`; the full rebuild stays as the fallback above the drift
//! threshold.

use std::path::{Path, PathBuf};
use std::process::Command;

use pixel_daemon::{Request, Service};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// Five source files, one committed baseline. `worker.ts` calls `helper`
/// from `util.ts`; the other three are independent filler so a threshold
/// test can drift a majority of them.
fn fixture(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "gpx-graph-incr-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/util.ts"),
        "export function helper(x: number): number { return x + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/worker.ts"),
        "import { helper } from \"./util\";\nexport function work(n: number): number {\n  return helper(n)\n}\n",
    )
    .unwrap();
    for name in ["alpha", "beta", "gamma"] {
        std::fs::write(
            dir.join(format!("src/{name}.ts")),
            format!("export function {name}(): number {{ return 1 }}\n"),
        )
        .unwrap();
    }
    std::fs::write(dir.join(".gitignore"), ".pixel/\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "baseline"]);
    dir
}

/// Build index + graph once (the "valid `.pixel/` copy" a CI job restores).
fn build_graph(dir: &Path) -> serde_json::Value {
    let mut svc = Service::open(dir).unwrap();
    let resp = svc.handle(Request::Symbol {
        name: "work".into(),
    });
    assert!(resp.ok, "{:?}", resp.error);
    let data = resp.into_data();
    assert_eq!(
        data["graph_build"]["incremental"], false,
        "first use is a full build: {data}"
    );
    data
}

fn changes(dir: &Path) -> serde_json::Value {
    let mut svc = Service::open(dir).unwrap();
    let resp = svc.handle(Request::Changes {
        base: None,
        offset: None,
        include_tests: false,
    });
    assert!(resp.ok, "{:?}", resp.error);
    resp.into_data()
}

fn symbol(dir: &Path, name: &str) -> serde_json::Value {
    let mut svc = Service::open(dir).unwrap();
    let resp = svc.handle(Request::Symbol { name: name.into() });
    assert!(resp.ok, "{:?}", resp.error);
    resp.into_data()
}

/// (a) One edited file: `changes` answers from an incremental update, and
/// `impact` on the edited method sees its new body (a new callee).
#[test]
fn one_edited_file_updates_the_graph_incrementally() {
    let dir = fixture("edit");
    build_graph(&dir);

    // `work` now also calls `alpha`: a new outgoing edge only an
    // extraction of worker.ts can produce.
    std::fs::write(
        dir.join("src/worker.ts"),
        "import { helper } from \"./util\";\nimport { alpha } from \"./alpha\";\nexport function work(n: number): number {\n  alpha();\n  return helper(n)\n}\n",
    )
    .unwrap();

    let data = changes(&dir);
    let build = &data["graph_build"];
    assert_eq!(build["incremental"], true, "{data}");
    assert_eq!(build["changed_files"], 1, "{data}");
    assert_eq!(build["removed_files"], 0, "{data}");
    assert_eq!(
        build["stats"]["files"], 5,
        "untouched files survive the update: {data}"
    );
    let changed: Vec<&str> = data["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["uid"].as_str())
        .collect();
    assert!(
        changed.iter().any(|uid| uid.contains("worker.ts#work")),
        "changes must report the edited method: {changed:?}"
    );

    // The graph is now fresh: the next op does not build again ...
    let mut svc = Service::open(&dir).unwrap();
    let resp = svc.handle(Request::Impact {
        uid_or_name: "src/worker.ts#work#function".into(),
        direction: "downstream".into(),
        depth: Some(1),
    });
    assert!(resp.ok, "{:?}", resp.error);
    let impact = resp.into_data();
    assert!(
        impact.get("graph_build").is_none(),
        "graph must be fresh after the incremental update: {impact}"
    );
    // ... and the edited body is what the graph knows.
    let text = impact.to_string();
    assert!(
        text.contains("alpha.ts#alpha"),
        "impact must see the new callee from the re-extracted file: {impact}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// (b) A deleted file: its symbols disappear without a full rebuild.
#[test]
fn deleted_file_drops_its_symbols_incrementally() {
    let dir = fixture("delete");
    build_graph(&dir);
    assert_eq!(symbol(&dir, "beta")["symbols"].as_array().unwrap().len(), 1);

    std::fs::remove_file(dir.join("src/beta.ts")).unwrap();

    let data = symbol(&dir, "beta");
    let build = &data["graph_build"];
    assert_eq!(build["incremental"], true, "{data}");
    assert_eq!(build["changed_files"], 0, "{data}");
    assert_eq!(build["removed_files"], 1, "{data}");
    assert_eq!(build["stats"]["files"], 4, "{data}");
    assert!(
        data["symbols"].as_array().unwrap().is_empty(),
        "symbols of a deleted file must be gone: {data}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// (c) Drift above the threshold (3 of 5 files, default 20 %): full rebuild,
/// reported as such so the caller can tell the two paths apart.
#[test]
fn drift_above_threshold_rebuilds_from_scratch() {
    let dir = fixture("threshold");
    build_graph(&dir);

    for name in ["alpha", "beta", "gamma"] {
        std::fs::write(
            dir.join(format!("src/{name}.ts")),
            format!("export function {name}(): number {{ return 2 }}\n"),
        )
        .unwrap();
    }

    let data = changes(&dir);
    let build = &data["graph_build"];
    assert_eq!(build["incremental"], false, "{data}");
    assert_eq!(build["reason"], "threshold", "{data}");
    assert_eq!(build["stats"]["files"], 5, "{data}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The CI scenario verbatim: a valid `.pixel/` copied into another checkout
/// of the same commit, then one file edited there. The copy must be reused
/// and updated, not rebuilt.
#[test]
fn copied_pixel_dir_is_updated_not_rebuilt() {
    let dir = fixture("copy-src");
    build_graph(&dir);

    let copy = dir.with_file_name(format!(
        "{}-copy",
        dir.file_name().unwrap().to_string_lossy()
    ));
    std::fs::remove_dir_all(&copy).ok();
    let out = Command::new("git")
        .args(["clone", "-q"])
        .arg(&dir)
        .arg(&copy)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let out = Command::new("cp")
        .arg("-R")
        .arg(dir.join(".pixel"))
        .arg(copy.join(".pixel"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");

    std::fs::write(
        copy.join("src/util.ts"),
        "export function helper(x: number): number { return x + 2 }\n",
    )
    .unwrap();

    let data = changes(&copy);
    let build = &data["graph_build"];
    assert_eq!(build["incremental"], true, "{data}");
    assert_eq!(build["changed_files"], 1, "{data}");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&copy);
}
