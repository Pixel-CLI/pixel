// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The issue #407 reproducer: `scripts/verify-ollaya-fresh-install.sh` must
//! exist, be executable, and encode the acceptance flow — clean Pixel
//! environment → `pixel install` → ollaya setup → model pull — so the same
//! flow can re-run on any machine. A regression in the flow is a release
//! blocker, so the script that reproduces it is pinned here.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// The reproducer script, relative to the repository root.
const SCRIPT: &str = "scripts/verify-ollaya-fresh-install.sh";

fn script_path() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR is crates/pixel; the repo root is two levels up.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(SCRIPT)
}

#[test]
fn the_reproducer_script_exists_and_is_executable() {
    let path = script_path();
    assert!(path.is_file(), "reproducer missing at {}", path.display());
    let mode = std::fs::metadata(&path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .permissions()
        .mode();
    // 0o100 is the owner-execute bit; the script must be runnable.
    assert!(
        mode & 0o100 != 0,
        "reproducer {} is not executable (mode {:o})",
        path.display(),
        mode
    );
}

#[test]
fn the_reproducer_script_carries_the_spdx_header() {
    let text = std::fs::read_to_string(script_path())
        .unwrap_or_else(|e| panic!("read {}: {e}", script_path().display()));
    assert!(
        text.contains("SPDX-FileCopyrightText: The Pixel contributors"),
        "reproducer is missing the SPDX copyright line"
    );
    assert!(
        text.contains("SPDX-License-Identifier: MIT"),
        "reproducer is missing the SPDX license line"
    );
}

#[test]
fn the_reproducer_encodes_the_acceptance_flow() {
    let text = std::fs::read_to_string(script_path())
        .unwrap_or_else(|e| panic!("read {}: {e}", script_path().display()));
    // The flow the issue names: pixel install, then the ollaya setup, then
    // the model pull — each step the script must run and check.
    for step in [
        "pixel install",
        "pixel config setup",
        "ollaya setup",
        "ollaya binary",
        "model pull",
        "winnow:e4b",
    ] {
        assert!(text.contains(step), "reproducer does not mention {step:?}");
    }
    // The contained environment: a fresh HOME so the run touches nothing
    // outside it.
    assert!(
        text.contains("mktemp"),
        "reproducer does not create a throwaway HOME"
    );
    assert!(text.contains("HOME="), "reproducer does not set HOME");
    // The pixel-managed ollaya prefix, matching OLLAYA_ROOT in
    // crates/pixel/src/classify_setup.rs.
    assert!(
        text.contains(".local/share/pixel/ollaya"),
        "reproducer does not use the pixel-managed ollaya prefix"
    );
}

#[test]
fn the_reproducer_uses_private_ollaya_server_and_verify_failure_propagation() {
    let text = std::fs::read_to_string(script_path())
        .unwrap_or_else(|e| panic!("read {}: {e}", script_path().display()));
    // The script should configure a private OLLAYA_HOST endpoint for its
    // ollaya CLI calls, so the model pull targets the server owned by this
    // run rather than a pre-existing server (issue #407).
    assert!(
        text.contains("127.0.0.1:11435"),
        "reproducer must target a private OLLAYA_HOST endpoint"
    );
    // The script must exit non-zero on the first failure so regressions
    // propagate up to CI.
    assert!(
        text.contains("set -eu"),
        "reproducer must use set -eu for failure propagation"
    );
    assert!(
        text.contains("|| fail"),
        "reproducer must use fail() to exit non-zero on step failure"
    );
    // The script should clean up (remove the temp HOME) on exit unless
    // KEEP=1 is set.
    assert!(
        text.contains("cleanup") && text.contains("trap cleanup EXIT"),
        "reproducer must clean up the throwaway environment on exit"
    );
}

#[test]
fn the_reproducer_verifies_the_recorded_launch_and_engine() {
    let text = std::fs::read_to_string(script_path())
        .unwrap_or_else(|e| panic!("read {}: {e}", script_path().display()));
    // After the pull, the setup records the daemon launch and stores the
    // engine preference; `pixel classify` reads both. The reproducer must
    // check them, or a setup that silently skips the recording passes.
    assert!(
        text.contains("engine: local"),
        "reproducer does not verify the classify engine is local"
    );
    assert!(
        text.contains("ollaya"),
        "reproducer does not verify the ollaya launch is recorded"
    );
}

/// The script's model name must match the code's DEFAULT_MODEL, so a pull
/// that starts serving a different model fails the reproducer rather than
/// passing on a stale name.
#[test]
fn the_reproducer_model_matches_the_code_default() {
    let code = include_str!("../../src/decide_ollaya.rs");
    let default_model = code
        .split("DEFAULT_MODEL: &str = \"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("DEFAULT_MODEL is defined in classify_setup.rs");
    let text = std::fs::read_to_string(script_path())
        .unwrap_or_else(|e| panic!("read {}: {e}", script_path().display()));
    assert!(
        text.contains(default_model),
        "reproducer does not pull the code's DEFAULT_MODEL {default_model:?}"
    );
}
