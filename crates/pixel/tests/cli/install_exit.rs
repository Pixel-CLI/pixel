// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The `pixel install` exit: `--json` and a piped stdout keep the
//! machine-readable report; the human banner (`pixel_install::banner`) is
//! a stdout-is-a-TTY path unit-tested in the pixel-install crate. These
//! tests pin the contract the banner branch must never break.

use crate::support::{Scratch, pixel_command};

#[test]
fn json_flag_prints_the_machine_readable_report() {
    let home = Scratch::for_test("install-exit", "json");
    let out = pixel_command()
        .args(["install", "--shell", "zsh", "--json"])
        .env("HOME", &*home)
        .env("CODEX_HOME", home.join(".codex"))
        .current_dir(&*home)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["version"], "v1", "{report}");
    assert_eq!(report["ok"], true, "{report}");
    assert!(report["steps"].as_array().unwrap().len() >= 5, "{report}");
}

#[test]
fn a_piped_stdout_keeps_the_json_report_without_the_flag() {
    let home = Scratch::for_test("install-exit", "piped");
    let out = pixel_command()
        .args(["install", "--shell", "zsh"])
        .env("HOME", &*home)
        .env("CODEX_HOME", home.join(".codex"))
        .current_dir(&*home)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    // Piped, not a terminal: the agent-parsable form, not the banner.
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let steps = report["steps"].as_array().unwrap().len() as u64;
    let summary = &report["summary"];
    assert_eq!(
        summary["green"].as_u64().unwrap()
            + summary["yellow"].as_u64().unwrap()
            + summary["red"].as_u64().unwrap(),
        steps,
        "{report}"
    );
    // The banner's text never leaks into the machine-readable stream.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("installed."), "{stdout}");
    assert!(!stdout.contains("pixel-cli.dev"), "{stdout}");
    assert!(!stdout.contains('\x1b'), "{stdout}");
}
