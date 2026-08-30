//! Golden parity gate: shells out to `tests/parity/harness.sh`, which
//! compares the old `gitpixel` binary against the new `pixel` binary across
//! search hit sets, target tiers, the code graph (symbol/impact/uses),
//! `--scope code` ranking, and the daemon code path.
//!
//! This is deliberately `#[ignore]`d by default: it requires BOTH binaries
//! to already be built in release mode, which is too heavy a precondition
//! for an ordinary `cargo test` / CI run without those binaries staged.
//!
//! Run it explicitly with:
//!
//! ```text
//! cd ~/Documents/gitpixel && cargo build --release
//! cd ~/Documents/pixel    && cargo build --release
//! cargo test -p pixel-bench --test parity -- --ignored
//! ```
//!
//! Override `GITPIXEL_BIN` / `PIXEL_BIN` in the environment to point at
//! different binary paths. Set `PARITY_REPO=<path-to-a-git-repo>` (e.g.
//! `PARITY_REPO=$HOME/Documents/gitpixel`) to additionally exercise the
//! harness's opt-in larger-repo comparisons -- see
//! `tests/parity/harness.sh` for exactly what each test checks and why a
//! comparison counts as a pass/fail.

use std::path::PathBuf;
use std::process::Command;

#[test]
#[ignore = "requires both the gitpixel and pixel binaries pre-built in release mode"]
fn golden_parity_harness_passes() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent() // crates/
        .and_then(|p| p.parent()) // workspace root (pixel/)
        .expect("pixel-bench crate should live two directories below the workspace root")
        .to_path_buf();

    let harness = workspace_root.join("tests/parity/harness.sh");
    assert!(
        harness.is_file(),
        "parity harness script not found at {}",
        harness.display()
    );

    let mut cmd = Command::new("bash");
    cmd.arg(&harness);

    // The harness itself defaults PIXEL_BIN to target/debug/pixel, since
    // that's the build most `cargo build` invocations produce. This test's
    // documented precondition is a --release build of both binaries, so
    // default to the release path here -- but still let an explicit
    // PIXEL_BIN/GITPIXEL_BIN from the caller's environment win.
    if std::env::var_os("PIXEL_BIN").is_none() {
        cmd.env("PIXEL_BIN", workspace_root.join("target/release/pixel"));
    }
    if std::env::var_os("GITPIXEL_BIN").is_none() {
        if let Some(gitpixel_workspace) = workspace_root.parent() {
            cmd.env(
                "GITPIXEL_BIN",
                gitpixel_workspace.join("gitpixel/target/release/gitpixel"),
            );
        }
    }

    let status = cmd
        .status()
        .expect("failed to spawn tests/parity/harness.sh -- is bash on PATH?");

    assert!(
        status.success(),
        "golden parity harness failed (exit status: {status}); \
         re-run manually for full diagnostic output: bash {}",
        harness.display()
    );
}
