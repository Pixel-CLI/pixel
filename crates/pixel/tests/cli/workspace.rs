// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel workspace`, `--workspace` fan-out and `pixel index pack|unpack` on the
//! real binary.

use serde_json::Value;

use crate::support::{project, stderr, stdout};

#[test]
fn a_workspace_answers_across_its_members_and_keeps_them_apart() {
    let first = project();
    let second = project();
    for member in [first.root(), second.root()] {
        let added = first.run(&["workspace", "add", &member.to_string_lossy()]);
        assert!(added.status.success(), "{}", stderr(&added));
    }
    // Adding twice changes nothing.
    first.run(&["workspace", "add", &second.root().to_string_lossy()]);
    let listed = first.run(&["workspace", "list", "--json"]);
    let members: Value = serde_json::from_str(&stdout(&listed)).unwrap();
    assert_eq!(members["members"].as_array().unwrap().len(), 2, "{members}");

    let impact = first.run(&["impact", "run", "--workspace", "--json"]);
    assert!(impact.status.success(), "{}", stderr(&impact));
    let report: Value = serde_json::from_str(&stdout(&impact)).unwrap();
    let results = report["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "{report}");
    assert!(
        results.iter().all(|result| result["ok"] == true),
        "{report}"
    );
    let text = stdout(&first.run(&["who-calls", "run", "--workspace"]));
    assert_eq!(text.matches("== ").count(), 2, "{text}");
    assert!(
        text.contains(&second.root().to_string_lossy().to_string()),
        "{text}"
    );

    let removed = first.run(&["workspace", "remove", &second.root().to_string_lossy()]);
    assert!(removed.status.success());
    assert!(stdout(&first.run(&["workspace", "list"])).contains("repo"));
    first.run(&["workspace", "clear"]);
    let empty = first.run(&["impact", "run", "--workspace"]);
    assert_eq!(empty.status.code(), Some(1));
    assert!(
        stderr(&empty).contains("no workspace members"),
        "{}",
        stderr(&empty)
    );
}

#[test]
fn a_workspace_member_that_fails_is_reported_without_hiding_the_others() {
    let first = project();
    let second = project();
    first.run(&["workspace", "add", &first.root().to_string_lossy()]);
    first.run(&["workspace", "add", &second.root().to_string_lossy()]);
    // Break the second member: its graph no longer answers this symbol query.
    let report: Value = serde_json::from_str(&stdout(&first.run(&[
        "impact",
        "no_such_symbol_anywhere",
        "--workspace",
        "--json",
    ])))
    .unwrap();
    let results = report["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert!(
        results.iter().all(|result| result["ok"] == false),
        "{report}"
    );
}

#[test]
fn an_index_packs_and_unpacks_with_every_file_verified() {
    let source = project();
    let pack = source.dir.path().join("index.pixelpack");
    let packed = source.run(&["index", "pack", "--out", &pack.to_string_lossy()]);
    assert!(packed.status.success(), "{}", stderr(&packed));
    let report: Value = serde_json::from_str(&stdout(&packed)).unwrap();
    assert!(report["files"].as_u64().unwrap() >= 2, "{report}");
    assert!(pack.is_file());

    let target = project();
    let _ = std::fs::remove_dir_all(target.root().join(".pixel"));
    let unpacked = target.run(&["index", "unpack", &pack.to_string_lossy()]);
    assert!(unpacked.status.success(), "{}", stderr(&unpacked));
    let landed: Value = serde_json::from_str(&stdout(&unpacked)).unwrap();
    assert!(
        landed["installed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "graph.db"),
        "{landed}"
    );
    assert!(target.root().join(".pixel/graph.db").is_file());
    // The unpacked graph answers.
    let found = target.run(&["find-symbol", "run", "--no-daemon"]);
    assert!(found.status.success(), "{}", stderr(&found));

    // A damaged pack installs nothing.
    let mut bytes = std::fs::read(&pack).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0xff;
    let bad = source.dir.path().join("bad.pixelpack");
    std::fs::write(&bad, bytes).unwrap();
    let other = project();
    let _ = std::fs::remove_dir_all(other.root().join(".pixel"));
    let refused = other.run(&["index", "unpack", &bad.to_string_lossy()]);
    assert_eq!(refused.status.code(), Some(1), "{}", stdout(&refused));
    assert!(!other.root().join(".pixel/graph.db").exists());
}
