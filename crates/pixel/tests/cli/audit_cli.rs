// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel audit` through the binary: every row it prints is the one a user
//! re-derives with `pixel list-signatures <file>` and `wc -c`, which is the
//! promise the report's footer makes.

use serde_json::Value;

use crate::support::{Scratch, git, pixel_command};

fn fixture(tag: &str) -> Scratch {
    let repo = Scratch::for_test("pixel-audit-cli", tag);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let big: String = (0..30)
        .map(|i| format!("/// Doc {i}.\npub fn handler_{i}(input: &str) -> usize {{\n    input.len() + {i}\n}}\n"))
        .collect();
    std::fs::write(repo.join("src/big.rs"), big).unwrap();
    std::fs::write(
        repo.join("src/small.rs"),
        "pub fn one() -> u8 {\n    1\n}\n",
    )
    .unwrap();
    std::fs::write(repo.join(".gitignore"), ".pixel/\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "fixture"]);
    repo
}

#[test]
fn audit_rows_match_list_signatures_and_the_file_size() {
    let repo = fixture("rows");
    let skeleton = pixel_command()
        .current_dir(&*repo)
        .args(["list-signatures", "src/big.rs", "--metrics=off"])
        .output()
        .unwrap();
    assert!(skeleton.status.success(), "{skeleton:?}");

    let audit = pixel_command()
        .current_dir(&*repo)
        .args(["audit", "--json", "--metrics=off"])
        .output()
        .unwrap();
    assert!(audit.status.success(), "{audit:?}");
    let report: Value = serde_json::from_slice(&audit.stdout).unwrap();
    let paths: Vec<&str> = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["src/big.rs", "src/small.rs"]);

    let big = &report["files"][0];
    let file_bytes = std::fs::metadata(repo.join("src/big.rs")).unwrap().len();
    assert_eq!(big["full_tokens"], file_bytes / 4);
    assert_eq!(big["outline_tokens"], skeleton.stdout.len() as u64 / 4);
    assert_eq!(big["signatures"], 30);
    assert_eq!(big["lines"], 120);

    // Both files measured: the report covers the whole pool and says so.
    assert_eq!(report["marker"], "complete");
    assert_eq!(report["epistemics"]["lower_bound"], false);
    assert_eq!(report["epistemics"]["closed_world"], false);
    assert_eq!(report["snapshot"]["indexed_source_files"], 2);
    assert_eq!(report["snapshot"]["examined"], 2);
    assert!(
        report["snapshot"]["graph_signature"].is_string(),
        "{report}"
    );

    let capped = pixel_command()
        .current_dir(&*repo)
        .args(["audit", "--json", "--top", "1", "--metrics=off"])
        .output()
        .unwrap();
    let capped: Value = serde_json::from_slice(&capped.stdout).unwrap();
    assert_eq!(
        capped["marker"], "capped",
        "--top 1 leaves src/small.rs out"
    );
    assert_eq!(capped["epistemics"]["lower_bound"], true);
    assert_eq!(
        capped["snapshot"]["graph_signature"],
        report["snapshot"]["graph_signature"]
    );
}

/// A fresh clone answers on the first call, and only that call builds: a
/// second run reads the graph the first one left.
#[test]
fn audit_builds_the_graph_on_a_first_run_only() {
    let repo = fixture("first");
    let notice = "no code graph yet, building it (first run only)";
    let first = pixel_command()
        .current_dir(&*repo)
        .args(["audit", "--json", "--metrics=off"])
        .output()
        .unwrap();
    assert!(first.status.success(), "{first:?}");
    assert!(
        String::from_utf8_lossy(&first.stderr).contains(notice),
        "{first:?}"
    );
    let report: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(report["totals"]["files"], 2);

    let second = pixel_command()
        .current_dir(repo.join("src"))
        .args(["audit", "--json", "--metrics=off"])
        .output()
        .unwrap();
    assert!(second.status.success(), "{second:?}");
    assert!(
        !String::from_utf8_lossy(&second.stderr).contains(notice),
        "{second:?}"
    );
    let again: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(
        again["root"],
        repo.display().to_string(),
        "a subdirectory audits its repository"
    );
    assert_eq!(again["totals"], report["totals"]);
}

#[test]
fn audit_prints_the_table_and_refuses_a_zero_top() {
    let repo = fixture("table");
    let text = pixel_command()
        .current_dir(&*repo)
        .args(["audit", "--top", "1", "--metrics=off"])
        .output()
        .unwrap();
    assert!(text.status.success(), "{text:?}");
    let stdout = String::from_utf8(text.stdout).unwrap();
    assert!(
        stdout.contains("total, 1 of 2 indexed source files:"),
        "{stdout}"
    );
    assert!(
        stdout.lines().any(|l| l.ends_with("  src/big.rs")),
        "{stdout}"
    );
    assert!(
        !stdout.contains("src/small.rs"),
        "--top 1 measures one file: {stdout}"
    );

    let zero = pixel_command()
        .current_dir(&*repo)
        .args(["audit", "--top", "0"])
        .output()
        .unwrap();
    assert!(!zero.status.success(), "--top 0 is refused");
}
