// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel setup` through the real CLI, with an isolated home directory.
//!
//! The golden files under `tests/setup/.agents/` are compared in-process by
//! `crates/pixel-install/src/setup/goldens.rs`, because the two flags that
//! drive the wizard from a script (`--selected-agents`,
//! `--selected-features`) are removed before the release. What this file holds
//! is what only the command can show: the `--print` contract, the refusal to
//! prompt without a terminal, and the files a scripted run actually writes.
//!
//! The tests below a `TEMPORARY (dev-flags)` marker use those flags and go with
//! them; the ones above it survive. After the removal the command keeps its
//! `--print` contract, the terminal refusal, and the in-process goldens for
//! everything else — the interactive prompts stay covered by the injected-I/O
//! unit tests in `setup_cmd.rs`, which is the same split `but agent setup`
//! settled on (`docs/design/but-agent-setup-reference.md`, "Testing strategy").

use crate::support::{Scratch, pixel_command};
use std::fs;
use std::path::Path;
use std::process::Output;

fn run(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    pixel_command()
        .env("HOME", home)
        .env_remove("PIXEL_METRICS")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}

fn stdout_of(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn print_prints_the_default_block_and_writes_nothing() {
    let home = Scratch::for_test("setup", "print-home");
    let repo = Scratch::for_test("setup", "print-repo");

    let text = stdout_of(&run(&home, &repo, &["setup", "--print"]));

    assert!(text.starts_with("<!-- pixel:setup:start -->"), "{text}");
    assert!(text.contains("## Pixel"), "{text}");
    assert!(
        text.trim_end().ends_with("<!-- pixel:setup:end -->"),
        "{text}"
    );
    assert!(
        !home.join(".claude").exists() && !repo.join("AGENTS.md").exists(),
        "--print is a render, not a run"
    );
    assert!(!home.join(".pixel").exists(), "--print creates no config");
}

// TEMPORARY (dev-flags): drives the wizard with the two selection flags.
#[test]
fn print_is_the_same_block_a_machine_wide_run_writes() {
    let home = Scratch::for_test("setup", "print-machine");
    let machine = Scratch::for_test("setup", "print-machine-run");
    let repo = Scratch::for_test("setup", "print-repo-run");
    fs::create_dir_all(machine.join(".claude")).unwrap();

    let printed = stdout_of(&run(&home, &repo, &["setup", "--print"]));
    let written = run(
        &machine,
        &repo,
        &["setup", "--selected-agents=1", "--selected-features=1"],
    );
    assert!(
        written.status.success(),
        "{}",
        String::from_utf8_lossy(&written.stderr)
    );
    let claude = fs::read_to_string(machine.join(".claude/CLAUDE.md")).unwrap();

    assert_eq!(
        printed.trim_end(),
        claude.trim_end(),
        "the preview has to be what the run writes, or it is a second truth"
    );
}

#[test]
fn without_a_terminal_the_command_points_at_print_instead_of_hanging() {
    let home = Scratch::for_test("setup", "no-tty-home");
    let repo = Scratch::for_test("setup", "no-tty-repo");

    let out = run(&home, &repo, &["setup"]);

    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("needs a terminal"), "{err}");
    assert!(err.contains("--print"), "{err}");
    assert!(
        !home.join(".claude").exists(),
        "a refused run writes nothing"
    );
}

// TEMPORARY (dev-flags): drives the wizard with the two selection flags.
#[test]
fn one_flag_without_a_terminal_is_refused_rather_than_silently_cancelled() {
    let home = Scratch::for_test("setup", "half-flag-home");
    let repo = Scratch::for_test("setup", "half-flag-repo");

    let out = run(&home, &repo, &["setup", "--selected-agents=1"]);

    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--selected-features"), "{err}");
    assert!(
        !home.join(".claude").exists(),
        "a run that cannot ask its second question writes nothing"
    );
}

// TEMPORARY (dev-flags): drives the wizard with the two selection flags.
#[test]
fn a_repository_run_writes_the_repository_files_and_not_the_home_ones() {
    let home = Scratch::for_test("setup", "repo-home");
    let repo = Scratch::for_test("setup", "repo-run");

    let out = run(
        &home,
        &repo,
        &[
            "setup",
            "--repo",
            repo.to_str().unwrap(),
            "--selected-agents=1,2",
            "--selected-features=1",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(repo.join("CLAUDE.md").is_file(), "the repository file");
    assert!(
        repo.join("AGENTS.md").is_file(),
        "the shared repository file"
    );
    assert!(
        !home.join(".claude").exists(),
        "--repo keeps the home install untouched"
    );
    let agents = fs::read_to_string(repo.join("AGENTS.md")).unwrap();
    assert!(agents.starts_with("<!-- pixel:setup:start -->"), "{agents}");
}

// TEMPORARY (dev-flags): drives the wizard with the two selection flags.
#[test]
fn an_index_outside_the_list_is_refused_with_the_range() {
    let home = Scratch::for_test("setup", "bad-index-home");
    let repo = Scratch::for_test("setup", "bad-index-repo");

    let out = run(
        &home,
        &repo,
        &["setup", "--selected-agents=1,99", "--selected-features=1"],
    );

    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("outside 1 to 9"), "{err}");
    assert!(!repo.join("CLAUDE.md").exists(), "nothing is written");
}

// TEMPORARY (dev-flags): drives the wizard with the two selection flags.
#[test]
fn a_second_run_leaves_the_file_byte_identical() {
    let home = Scratch::for_test("setup", "twice-home");
    let repo = Scratch::for_test("setup", "twice-repo");
    fs::create_dir_all(home.join(".claude")).unwrap();
    let args = ["setup", "--selected-agents=1", "--selected-features=1,2,4"];

    let first_run = run(&home, &repo, &args);
    assert!(
        first_run.status.success(),
        "the first run: {}",
        String::from_utf8_lossy(&first_run.stderr)
    );
    let first = fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap();
    let second_run = run(&home, &repo, &args);
    assert!(
        second_run.status.success(),
        "a second run must succeed on the state the first one left: {}",
        String::from_utf8_lossy(&second_run.stderr)
    );
    let second = fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap();

    assert_eq!(first, second, "the block must not grow on every re-run");
    assert_eq!(first.matches("<!-- pixel:setup:start -->").count(), 1);
}

// TEMPORARY (dev-flags): drives the wizard with the two selection flags.
#[test]
fn a_run_succeeds_on_a_machine_pixel_config_setup_already_configured() {
    let home = Scratch::for_test("setup", "configured-home");
    let repo = Scratch::for_test("setup", "configured-repo");
    fs::create_dir_all(home.join(".claude")).unwrap();
    // The state `pixel config setup` leaves: every key the wizard is about to
    // set already holds the value it wants to store.
    fs::create_dir_all(home.join(".pixel")).unwrap();
    fs::write(
        home.join(".pixel/config.yaml"),
        "brief: true\nmetrics: \"on\"\ndaemon_auto_start: true\nclassify:\n  enabled: true\n",
    )
    .unwrap();

    let out = run(
        &home,
        &repo,
        &[
            "setup",
            "--selected-agents=1",
            "--selected-features=1,2,4,5,7",
        ],
    );

    assert!(
        out.status.success(),
        "setting a key to the value it already holds is not a failure: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let config = fs::read_to_string(home.join(".pixel/config.yaml")).unwrap();
    assert!(config.contains("brief: true"), "{config}");
    assert!(config.contains("classify"), "{config}");
}

// TEMPORARY (dev-flags): drives the wizard with the two selection flags.
#[test]
fn a_dummy_run_leaves_the_real_global_configuration_alone() {
    let home = Scratch::for_test("setup", "dummy-config-home");
    let repo = Scratch::for_test("setup", "dummy-config-repo");

    let out = run(
        &home,
        &repo,
        &[
            "setup",
            "--dummy-apply",
            "--selected-agents=1",
            "--selected-features=1,2,4",
        ],
    );

    assert!(
        out.status.success(),
        "the dummy run: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !home.join(".pixel").exists(),
        "the flag promises nothing outside the repository changes, and the \
         global config is not where the goldens live"
    );
}

// TEMPORARY (dev-flags): drives the wizard with the two selection flags.
#[test]
fn a_users_own_paragraph_survives_the_block() {
    let home = Scratch::for_test("setup", "keep-home");
    let repo = Scratch::for_test("setup", "keep-repo");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(home.join(".claude/CLAUDE.md"), "# My rules\n\nKeep me.\n").unwrap();

    let out = run(
        &home,
        &repo,
        &["setup", "--selected-agents=1", "--selected-features=1"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let content = fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap();
    assert!(content.starts_with("# My rules\n\nKeep me.\n"), "{content}");
    assert!(content.contains("<!-- pixel:setup:start -->"));
}
