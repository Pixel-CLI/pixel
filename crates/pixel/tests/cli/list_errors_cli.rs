// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel list-errors`: the error sink is only useful if what one command
//! records, another reads back. The query layer has its own tests in
//! `pixel-session`; these pin the CLI dispatch in front of it.

use serde_json::Value;

use crate::support::{Scratch, pixel_command};

fn list_errors(state: &Scratch, repo: &Scratch, args: &[&str]) -> String {
    let out = pixel_command()
        .arg("list-errors")
        .args(args)
        .arg("--repo")
        .arg(&**repo)
        .arg("--json")
        .env("PIXEL_SNIPER_STATE_ROOT", &**state)
        .output()
        .unwrap();
    assert!(out.status.success(), "list-errors {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn a_failed_wrapped_command_is_what_last_reads_back() {
    let state = Scratch::for_test("list-errors", "state");
    let repo = Scratch::for_test("list-errors", "repo");

    let wrapped = pixel_command()
        .args(["list-errors", "run", "--repo"])
        .arg(&*repo)
        .args([
            "--",
            "/bin/sh",
            "-c",
            "printf sniper_boundary_marker >&2; exit 9",
        ])
        .env("PIXEL_SNIPER_STATE_ROOT", &*state)
        .output()
        .unwrap();
    assert_eq!(
        wrapped.status.code(),
        Some(9),
        "run mirrors the wrapped exit code: {wrapped:?}"
    );

    let last: Value =
        serde_json::from_str(&list_errors(&state, &repo, &["last"])).expect("last prints JSON");
    let rows = last["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("last lists `errors`: {last}"));
    assert_eq!(rows.len(), 1, "{last}");
    assert!(
        rows[0].to_string().contains("sniper_boundary_marker"),
        "the stored record carries the wrapped command's output: {last}"
    );
}
