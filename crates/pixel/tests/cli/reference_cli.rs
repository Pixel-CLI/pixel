// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel reference` end-to-end: the add/list/remove/query contract and the
//! `setup` exit status. A failing `setup` still prints its per-entry report —
//! the only place a failed corpus is named — so the last test asserts stdout
//! even though the exit status is a failure.

use std::process::Output;

use crate::support::{Scratch, pixel_command};

fn pixel(args: &[&str]) -> Output {
    pixel_command().args(args).output().unwrap()
}

#[test]
fn add_list_remove_roundtrip_exposes_the_documented_json_fields() {
    let root = Scratch::for_test("reference-cli", "roundtrip");
    let path = root.to_str().unwrap();

    let added = pixel(&[
        "reference",
        "add",
        "serde",
        "https://example.com/serde",
        "v1.0.0",
        path,
    ]);
    assert!(added.status.success(), "{added:?}");
    assert!(
        String::from_utf8_lossy(&added.stdout).contains("added serde"),
        "{added:?}"
    );

    let listed = pixel(&["reference", "list", "--json", path]);
    assert!(listed.status.success(), "{listed:?}");
    let value: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(value["format"], 1, "{value}");
    let entries = value["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry["id"], "serde");
    assert_eq!(entry["repo"], "https://example.com/serde");
    assert_eq!(entry["revision"], "v1.0.0");
    assert_eq!(entry["source"], ".pixel/references/serde/v1.0.0");
    assert_eq!(entry["licence"], "");
    assert_eq!(entry["provenance"], "");
    assert_eq!(entry["role"], "reference");

    // `query` never fetched anything, so the corpus is disclosed as missing.
    let queried = pixel(&["reference", "query", "--json", path]);
    assert!(queried.status.success(), "{queried:?}");
    let query: serde_json::Value = serde_json::from_slice(&queried.stdout).unwrap();
    assert!(query["results"].as_array().unwrap().is_empty(), "{query}");
    let disclosures = query["disclosures"].as_array().unwrap();
    assert_eq!(disclosures.len(), 1);
    assert_eq!(disclosures[0]["id"], "serde");
    assert_eq!(disclosures[0]["status"], "missing");

    let removed = pixel(&["reference", "remove", "serde", path]);
    assert!(removed.status.success(), "{removed:?}");

    let after = pixel(&["reference", "list", "--json", path]);
    let value: serde_json::Value = serde_json::from_slice(&after.stdout).unwrap();
    assert!(value["entries"].as_array().unwrap().is_empty(), "{value}");
}

#[test]
fn a_failing_setup_prints_the_report_and_still_exits_nonzero() {
    let root = Scratch::for_test("reference-cli", "setup-fail");
    let path = root.to_str().unwrap();
    let missing = root.join("no-such-repo");
    let missing = missing.to_str().unwrap();

    let added = pixel(&["reference", "add", "bad-crate", missing, "v1.0.0", path]);
    assert!(added.status.success(), "{added:?}");

    let setup = pixel(&["reference", "setup", path]);
    assert!(
        !setup.status.success(),
        "a failed setup must exit nonzero: {setup:?}"
    );
    // The report still reaches stdout, naming the corpus and its own error.
    let report: serde_json::Value = serde_json::from_slice(&setup.stdout).unwrap();
    assert_eq!(report["total"], 1, "{report}");
    let failures = report["failures"].as_array().unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["id"], "bad-crate");
    assert!(
        failures[0]["error"].as_str().is_some_and(|e| !e.is_empty()),
        "{report}"
    );
}

#[test]
fn an_unsafe_revision_is_refused_at_add() {
    let root = Scratch::for_test("reference-cli", "unsafe");
    let path = root.to_str().unwrap();
    // `--` hands the flag-shaped revision to the parser as a positional, so
    // the command's own guard — not clap — is what refuses it.
    let out = pixel(&[
        "reference",
        "add",
        "evil",
        "https://example.com/x",
        "--",
        "--upload-pack=/bin/sh",
        path,
    ]);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unsafe revision"),
        "{out:?}"
    );
}
