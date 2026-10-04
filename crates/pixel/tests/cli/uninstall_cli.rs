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
    let prompt = home.join(".local/share/pixel/agent-prompt.md");
    assert!(prompt.is_file(), "fixture: install wrote the prompt");
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
    assert!(
        prompt.is_file(),
        "the prompt survives a wrappers-only uninstall"
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
    assert!(!prompt.exists(), "a full uninstall removes the prompt");
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
