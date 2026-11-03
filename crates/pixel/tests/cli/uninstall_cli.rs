// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel setup --remove`: the CLI hands the library the binary that runs.

use std::path::Path;

use crate::support::{Scratch, exec_while_text_busy};

/// `install.sh` with `PIXEL_INSTALL_DIR` puts the binary outside
/// `~/.local/bin`: the CLI hands the library the binary that runs, so
/// uninstall removes that one instead of reporting "no binary found".
#[test]
fn uninstall_removes_the_binary_that_runs_it() {
    let home = Scratch::for_test("uninstall-cli", "running-binary");
    let dir = home.join("opt/pixel/bin");
    std::fs::create_dir_all(&dir).unwrap();
    let copy = dir.join("pixel");
    std::fs::copy(env!("CARGO_BIN_EXE_pixel"), &copy).unwrap();
    let out = exec_while_text_busy(|| {
        std::process::Command::new(&copy)
            .args(["setup", "--remove", "--json"])
            .env("PIXEL_DAEMON_AUTO_START", "0")
            .env("HOME", &*home)
            .env("CODEX_HOME", home.join(".codex"))
            .current_dir(&*home)
            .output()
    })
    .unwrap();
    assert!(out.status.success(), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(!copy.exists(), "the running binary is removed: {report}");
    let step = report["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "binary")
        .expect("binary step");
    assert_eq!(step["summary"], "removed pixel binary", "{report}");
    let reported = Path::new(report["executable_path"].as_str().unwrap());
    assert_eq!(
        reported.parent().unwrap().canonicalize().unwrap(),
        dir.canonicalize().unwrap(),
        "{report}"
    );
}
