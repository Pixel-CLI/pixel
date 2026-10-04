// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Integration tests for what `changes::detect` says about each changed
//! symbol (`change`, `change_basis`, `signature_changed`) and about the call
//! sites the change may affect (`consumers`), against a real repository and
//! a real graph build.

use std::path::{Path, PathBuf};

use pixel_graph::build::build_graph;
use pixel_graph::changes::{ChangesReport, Consumer, detect};
use pixel_graph::store::GraphStore;

fn tmpdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pixel-consumers-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

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

fn lib_v1() -> String {
    [
        "export function keep(n: number): number {",
        "  return n + 1",
        "}",
        "export function gone(n: number): number {",
        "  return n + 2",
        "}",
        "",
    ]
    .join("\n")
}

/// `useKeep` calls `keep` on line 3, `useGone` calls `gone` on line 6.
fn use_v1() -> String {
    [
        "import { keep, gone } from './lib'",
        "export function useKeep(n: number): number {",
        "  return keep(n)",
        "}",
        "export function useGone(n: number): number {",
        "  return gone(n)",
        "}",
        "",
    ]
    .join("\n")
}

fn repo(name: &str) -> PathBuf {
    let root = tmpdir(name);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.ts"), lib_v1()).unwrap();
    std::fs::write(root.join("src/use.ts"), use_v1()).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "baseline"]);
    root
}

fn report(root: &Path, base: Option<&str>) -> ChangesReport {
    let db = tmpdir("db").join("graph.db");
    build_graph(root, &db).unwrap();
    let store = GraphStore::open(&db).unwrap();
    detect(&store, root, base, false).unwrap()
}

/// `useKeep`'s resolved call to `keep`, imported by name.
fn keep_consumer() -> Consumer {
    Consumer {
        of: "src/lib.ts#keep#function".into(),
        path: "src/use.ts".into(),
        line: 3,
        caller: Some("src/use.ts#useKeep#function".into()),
        basis: "calls".into(),
        tier: Some("exact".into()),
    }
}

fn judged(r: &ChangesReport, name: &str) -> (String, String, Option<bool>) {
    let s = r
        .symbols
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("{name} not reported: {:?}", r.symbols));
    (
        s.change.clone(),
        s.change_basis.clone(),
        s.signature_changed,
    )
}

#[test]
fn a_body_edit_is_modified_with_its_resolved_callers_as_consumers() {
    let root = repo("body");
    std::fs::write(
        root.join("src/lib.ts"),
        lib_v1().replace("return n + 1", "return n + 11"),
    )
    .unwrap();
    let r = report(&root, Some("HEAD"));
    assert_eq!(
        judged(&r, "keep"),
        ("modified".into(), "symbol".into(), Some(false))
    );
    assert_eq!(r.consumers, vec![keep_consumer()]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_signature_edit_is_flagged_as_one() {
    let root = repo("signature");
    std::fs::write(
        root.join("src/lib.ts"),
        lib_v1().replace(
            "export function keep(n: number): number {",
            "export function keep(n: number, m: number): number {",
        ),
    )
    .unwrap();
    let r = report(&root, Some("HEAD"));
    assert_eq!(
        judged(&r, "keep"),
        ("modified".into(), "symbol".into(), Some(true))
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_function_added_to_a_modified_file_is_added_not_modified() {
    let root = repo("added-symbol");
    let grown = format!(
        "{}{}",
        lib_v1(),
        ["export function fresh(): number {", "  return 3", "}", ""].join("\n")
    );
    std::fs::write(root.join("src/lib.ts"), grown).unwrap();
    let r = report(&root, Some("HEAD"));
    assert_eq!(judged(&r, "fresh"), ("added".into(), "symbol".into(), None));
    assert!(
        r.consumers.iter().all(|c| !c.of.contains("fresh")),
        "{:?}",
        r.consumers
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_new_file_is_added_on_the_file_status_alone() {
    let root = repo("added-file");
    std::fs::write(
        root.join("src/new.ts"),
        "export function brandNew(): number {\n  return 4\n}\n",
    )
    .unwrap();
    git(&root, &["add", "src/new.ts"]);
    let r = report(&root, Some("HEAD"));
    assert_eq!(
        judged(&r, "brandNew"),
        ("added".into(), "file".into(), None)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_deleted_function_lists_the_unresolved_sites_still_naming_it() {
    let root = repo("deleted");
    let without_gone = [
        "export function keep(n: number): number {",
        "  return n + 1",
        "}",
        "",
    ]
    .join("\n");
    std::fs::write(root.join("src/lib.ts"), without_gone).unwrap();
    let r = report(&root, Some("HEAD"));
    let names: Vec<&str> = r.unanchored.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(names, vec!["gone"]);
    // The deletion's anchor line touches `keep`, which ends right above
    // it, so `keep`'s caller is listed too, after `gone`'s by `of`.
    assert_eq!(
        r.consumers,
        vec![
            Consumer {
                of: "src/lib.ts#gone#function".into(),
                path: "src/use.ts".into(),
                line: 6,
                caller: Some("src/use.ts#useGone#function".into()),
                basis: "unresolved_name".into(),
                tier: None,
            },
            keep_consumer(),
        ]
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_base_names_the_side_the_diff_compared_against() {
    let root = repo("base-label");
    std::fs::write(
        root.join("src/lib.ts"),
        lib_v1().replace("return n + 1", "return n + 11"),
    )
    .unwrap();
    assert_eq!(report(&root, None).base, "index");
    assert_eq!(report(&root, Some("HEAD")).base, "HEAD");
    let _ = std::fs::remove_dir_all(&root);
}
