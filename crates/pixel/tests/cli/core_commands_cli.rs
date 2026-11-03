// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `workspace`, `coverage`, `index-pack` and `index-unpack` are core
//! commands (#878): the real binary against a disposable repository.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

use crate::support::{Scratch, pixel_command};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A committed repository holding one Rust file.
fn repo(tag: &str) -> Scratch {
    let dir = Scratch::for_test("pixel-core-cmds", tag);
    std::fs::write(dir.join("lib.rs"), "pub fn login_user() {}\n").unwrap();
    std::fs::write(dir.join(".gitignore"), ".pixel/\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "fixture"]);
    dir
}

fn pixel(dir: &Path, args: &[&str]) -> Output {
    pixel_command()
        .args(args)
        .arg("--metrics=off")
        .current_dir(dir)
        .output()
        .unwrap()
}

fn json(out: &Output) -> Value {
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"))
}

#[test]
fn workspace_registers_each_member_once_and_refuses_a_stranger() {
    let root = repo("workspace");
    let member = repo("workspace-member");
    let stranger = repo("workspace-stranger");
    let member_path = member.to_str().unwrap();
    for _ in 0..2 {
        let added = pixel(&root, &["workspace", "add", member_path]);
        assert!(added.status.success(), "{added:?}");
        assert_eq!(
            String::from_utf8_lossy(&added.stdout),
            format!("workspace: added {member_path}\n")
        );
    }
    let listed = json(&pixel(&root, &["workspace", "list", "--json"]));
    assert_eq!(listed["members"], serde_json::json!([member_path]));
    let registry: Value =
        serde_json::from_slice(&std::fs::read(root.join(".pixel/workspace.json")).unwrap())
            .unwrap();
    assert_eq!(registry["members"], serde_json::json!([member_path]));

    let refused = pixel(&root, &["workspace", "remove", stranger.to_str().unwrap()]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("is not a member"),
        "{refused:?}"
    );
    let removed = pixel(&root, &["workspace", "remove", member_path]);
    assert!(removed.status.success(), "{removed:?}");
    let after = json(&pixel(&root, &["workspace", "list", "--json"]));
    assert_eq!(after["members"], serde_json::json!([]));
}

#[test]
fn coverage_counts_files_on_disk_and_notes_a_missing_graph() {
    let root = repo("coverage");
    let report = json(&pixel(&root, &["coverage", "--json"]));
    assert_eq!(report["graph_present"], false, "{report}");
    assert_eq!(
        report["languages"],
        serde_json::json!([{
            "lang": "rust",
            "on_disk": 1,
            "indexed": 0,
            "coverage_pct": 0.0,
            "symbols": 0,
        }])
    );
    assert_eq!(report["totals"]["on_disk"], 1, "{report}");
    assert_eq!(report["unrecognized_files"], 1, "{report}");

    let text = pixel(&root, &["coverage"]);
    assert!(text.status.success(), "{text:?}");
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(text.contains("rust"), "{text}");
    assert!(
        text.contains("1 file(s) with unrecognized extensions"),
        "{text}"
    );
    assert!(text.contains("no graph.db"), "{text}");
}

#[test]
fn an_index_pack_installs_into_another_repository() {
    let source = repo("pack-source");
    let target = repo("pack-target");
    let built = pixel(&source, &["build-index"]);
    assert!(built.status.success(), "{built:?}");
    let bundle = source.join("index.pxpack");
    let packed = json(&pixel(
        &source,
        &["index-pack", "--out", bundle.to_str().unwrap()],
    ));
    assert_eq!(packed["files"], 1, "{packed}");
    assert!(bundle.is_file());

    assert!(!target.join(".pixel/base.shard").exists());
    let unpacked = json(&pixel(&target, &["index-unpack", bundle.to_str().unwrap()]));
    assert_eq!(unpacked["installed"], serde_json::json!(["base.shard"]));
    assert!(target.join(".pixel/base.shard").is_file());
}
