// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel list-errors`: the error sink is only useful if what one command
//! records, another reads back. The query layer has its own tests in
//! `pixel-session`; these pin the CLI dispatch in front of it.

use std::io::Write;
use std::process::Stdio;

use serde_json::Value;

use crate::support::{Scratch, pixel_command};

fn list_errors(state: &Scratch, repo: &Scratch, args: &[&str], stdin: &str) -> String {
    let mut child = pixel_command()
        .arg("list-errors")
        .args(args)
        .arg("--repo")
        .arg(&**repo)
        .arg("--json")
        .env("PIXEL_SNIPER_STATE_ROOT", &**state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "list-errors {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn a_reported_error_is_what_last_reads_back() {
    let state = Scratch::for_test("list-errors", "state");
    let repo = Scratch::for_test("list-errors", "repo");
    let record = r#"{"surface":"reported","message":"sniper boundary marker","run_id":"cli-run"}"#;

    let reported: Value = serde_json::from_str(&list_errors(&state, &repo, &["report"], record))
        .expect("report prints the stored record as JSON");
    let id = reported["id"].as_i64().expect("report names the stored id");

    let last: Value =
        serde_json::from_str(&list_errors(&state, &repo, &["last"], "")).expect("last prints JSON");
    let rows = last["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("last lists `errors`: {last}"));
    assert_eq!(rows.len(), 1, "{last}");
    assert_eq!(rows[0]["id"].as_i64(), Some(id), "{last}");
    assert_eq!(rows[0]["message"], "sniper boundary marker", "{last}");
    assert_eq!(rows[0]["run_id"], "cli-run", "{last}");
}
