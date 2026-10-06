// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel uninstall --wrappers-only`: the flag reaches the library through
//! the CLI and limits the run to the one shell-wrapper step, so the fix
//! `pixel doctor` names for a stray block never takes the install with it.

use std::path::Path;

use crate::support::{Scratch, pixel_command};

fn run(home: &Path, args: &[&str]) -> serde_json::Value {
    let out = pixel_command()
        .args(args)
        .env("HOME", home)
        .env("CODEX_HOME", home.join(".codex"))
        .current_dir(home)
        .output()
        .unwrap();
    assert!(out.status.success(), "pixel {args:?}: {out:?}");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{args:?}: {e}"))
}

#[test]
fn wrappers_only_removes_one_block_and_keeps_the_rest_installed() {
    let home = Scratch::for_test("uninstall-cli", "wrappers-only");
    run(&home, &["install", "--shell", "zsh", "--json"]);
    // The Claude task hooks stand for the rest of the install.
    let settings = home.join(".claude/settings.json");
    let installed = std::fs::read_to_string(&settings).expect("fixture: install wrote settings");
    assert!(
        installed.contains("run-hook task-event --provider claude --event session-start"),
        "fixture: install registered the Claude task hooks: {installed}"
    );
    // `install` no longer writes a wrapper (the prompt travels through the
    // lifecycle hooks); the block `--wrappers-only` exists for is the residue
    // of an older install, so the fixture writes one after installing.
    std::fs::write(
        home.join(".zshrc"),
        "export KEEP=1\n# >>> pixel-managed >>>\nclaude() { command claude \"$@\"; }\n# <<< pixel-managed <<<\n",
    )
    .unwrap();

    let report = run(
        &home,
        &["uninstall", "--wrappers-only", "--shell", "zsh", "--json"],
    );
    let steps = report["steps"].as_array().expect("steps");
    assert_eq!(steps.len(), 1, "{report}");
    assert_eq!(steps[0]["id"], "shell-wrappers");
    assert_eq!(report["ok"], true, "{report}");
    assert_eq!(
        std::fs::read_to_string(home.join(".zshrc")).unwrap(),
        "export KEEP=1\n",
        "the zsh block is gone and the user's own lines stay"
    );
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        installed,
        "the Claude hooks survive a wrappers-only uninstall"
    );

    // Without the flag the same command is the full uninstall. It names the
    // binary to remove: by default that is the running one, the test binary.
    let absent = home.join(".local/bin/pixel");
    let full = run(
        &home,
        &[
            "uninstall",
            "--shell",
            "zsh",
            "--binary-path",
            absent.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(full["steps"].as_array().unwrap().len() > 1, "{full}");
    let left: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(
        left,
        serde_json::json!({}),
        "a full uninstall removes the Claude hooks"
    );
}

/// `--shell` reaches the install: the retired wrapper is taken out of the
/// profile of the shell the flag names, whatever the account's login shell.
#[test]
fn install_cleans_the_profile_of_the_shell_it_is_given() {
    let home = Scratch::for_test("uninstall-cli", "install-shell");
    let block = "export KEEP=1\n# >>> pixel-managed >>>\nclaude() { command claude \"$@\"; }\n# <<< pixel-managed <<<\n";
    std::fs::write(home.join(".zshrc"), block).unwrap();
    run(&home, &["install", "--shell", "zsh", "--json"]);
    assert_eq!(
        std::fs::read_to_string(home.join(".zshrc")).unwrap(),
        "export KEEP=1\n"
    );
}

/// `--repo` reaches the uninstall: the run is scoped to that repository.
#[test]
fn uninstall_repo_reports_the_repository_it_was_given() {
    let home = Scratch::for_test("uninstall-cli", "uninstall-repo");
    let repo = home.join("project");
    std::fs::create_dir_all(&repo).unwrap();
    let report = run(
        &home,
        &[
            "uninstall",
            "--repo",
            repo.to_str().unwrap(),
            "--dry-run",
            "--json",
        ],
    );
    assert_eq!(report["home"], repo.display().to_string(), "{report}");
}

/// Executing a binary this test just copied can fail with ETXTBSY while a
/// sibling test's forked child still holds the write descriptor, or while
/// writeback finishes on overlay filesystems; a short bounded retry clears
/// it (the same race antigravity's registration probe retries under).
fn exec_while_text_busy(
    mut probe: impl FnMut() -> std::io::Result<std::process::Output>,
) -> std::io::Result<std::process::Output> {
    for _ in 0..50 {
        match probe() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            outcome => return outcome,
        }
    }
    probe()
}

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
            .args(["uninstall", "--shell", "zsh", "--json"])
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
