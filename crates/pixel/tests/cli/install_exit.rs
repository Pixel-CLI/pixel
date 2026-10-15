// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The `pixel setup` exit: `--json` and a piped stdout keep the
//! machine-readable report; the human banner (`pixel_setup::banner`) is
//! a stdout-is-a-TTY path unit-tested in the pixel-setup crate. These
//! tests pin the contract the banner branch must never break.

use crate::support::{Scratch, pixel_command};

#[test]
fn json_flag_prints_the_machine_readable_report() {
    let home = Scratch::for_test("install-exit", "json");
    let out = pixel_command()
        .args(["setup", "--json"])
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
        .args(["setup"])
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

/// `pixel setup` is the old `pixel install`: it writes the Claude task hooks
/// into the home it is given, and nothing else answers to the old name.
#[test]
fn setup_writes_the_claude_task_hooks_into_a_temporary_home() {
    let home = Scratch::for_test("install-exit", "setup-writes");
    let out = pixel_command()
        .args(["setup", "--json"])
        .env("HOME", &*home)
        .env("CODEX_HOME", home.join(".codex"))
        .current_dir(&*home)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let settings = std::fs::read_to_string(home.join(".claude/settings.json")).unwrap();
    assert!(
        settings.contains("run-hook task-event --provider claude --event session-start"),
        "{settings}"
    );
    let codex = std::fs::read_to_string(home.join(".codex/hooks.json")).unwrap();
    assert!(
        codex.contains("run-hook task-event --provider codex"),
        "{codex}"
    );
}

/// `--remove` owns the flags that only make sense when removing: without it
/// they are a usage error instead of being silently ignored by a real setup.
#[test]
fn the_removal_flags_need_remove() {
    let home = Scratch::for_test("install-exit", "needs-remove");
    for flag in [&["--dry-run"][..], &["--binary-path", "/nonexistent/pixel"]] {
        let out = pixel_command()
            .arg("setup")
            .args(flag)
            .env("HOME", &*home)
            .env("CODEX_HOME", home.join(".codex"))
            .current_dir(&*home)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{flag:?}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("--remove"), "{flag:?}: {stderr}");
    }
    assert!(
        !home.join(".claude").exists(),
        "a rejected setup wrote nothing"
    );
}
