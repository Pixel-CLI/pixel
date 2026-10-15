// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel setup` through the real CLI, with an isolated home directory.
//!
//! The golden files under `tests/setup/.agents/` are compared in-process by
//! `crates/pixel-setup/src/setup/goldens.rs`, because the two flags that
//! drive the wizard from a script (`--selected-agents`,
//! `--selected-features`) are removed before the release. What this file holds
//! is what only the command can show: the `--print` contract, that a run
//! without a terminal is the unattended installer and not a wizard, and the
//! files a scripted run actually writes (instruction files and hooks).
//!
//! The tests below a `TEMPORARY (dev-flags)` marker use those flags and go with
//! them; the ones above it survive. After the removal the command keeps its
//! `--print` contract, the unattended path, and the in-process goldens for
//! everything else — the interactive prompts stay covered by the injected-I/O
//! unit tests in `setup_cmd.rs`, which is the same split `but agent setup`
//! settled on (`docs/archive/design/but-agent-setup-reference.md`, "Testing strategy").

use crate::support::{Scratch, pixel_command};
use std::fs;
use std::path::Path;
use std::process::Output;

fn run(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    pixel_command()
        .env("HOME", home)
        // Every variable that moves an agent's configuration directory points
        // at the scratch home or is gone: a developer's own would send the
        // hook files of these tests to their real agents.
        .env("CODEX_HOME", home.join(".codex"))
        .env_remove("PI_CODING_AGENT_DIR")
        .env_remove("XDG_CONFIG_HOME")
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
fn print_prints_the_pixel_skill_and_writes_nothing() {
    let home = Scratch::for_test("setup", "print-home");
    let repo = Scratch::for_test("setup", "print-repo");

    let text = stdout_of(&run(&home, &repo, &["setup", "--print"]));

    assert!(text.starts_with("---\nname: pixel"), "{text}");
    assert!(text.contains("pixel brief"), "{text}");
    assert!(text.contains("license: MIT"), "{text}");
    assert!(
        !home.join(".claude").exists() && !repo.join("AGENTS.md").exists(),
        "--print is a render, not a run"
    );
    assert!(!home.join(".pixel").exists(), "--print creates no config");
}

// TEMPORARY (dev-flags): drives the wizard with the two selection flags.
#[test]
fn print_is_the_same_skill_a_machine_wide_run_writes() {
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
    let skill = fs::read_to_string(machine.join(".claude/skills/pixel/SKILL.md")).unwrap();

    assert_eq!(
        printed.trim_end(),
        skill.trim_end(),
        "the preview has to be what the run writes, or it is a second truth"
    );
}

#[test]
fn without_a_terminal_setup_is_the_unattended_installer_and_never_asks() {
    let plain = Scratch::for_test("setup", "no-tty-plain");
    let yes = Scratch::for_test("setup", "no-tty-yes");
    let repo = Scratch::for_test("setup", "no-tty-repo");

    let piped = run(&plain, &repo, &["setup"]);
    let report: serde_json::Value = serde_json::from_str(&stdout_of(&piped)).unwrap();
    assert_eq!(report["ok"], true, "{report}");
    assert!(
        report["steps"].is_array(),
        "the installer's report: {report}"
    );

    // `--yes` asks for the same thing explicitly, and gets the same files.
    stdout_of(&run(&yes, &repo, &["setup", "--yes"]));
    let settings = |home: &Path| fs::read_to_string(home.join(".claude/settings.json")).unwrap();
    let written = settings(&plain);
    assert!(
        written.contains("run-hook task-event --provider claude --event session-start"),
        "the unattended run registers the Claude task hooks: {written}"
    );
    assert_eq!(settings(&yes), written, "--yes is the unattended run");
    assert!(
        plain.join(".claude/skills/pixel/SKILL.md").is_file(),
        "the unattended run deploys the pixel skill"
    );
    assert!(
        !plain.join(".claude/CLAUDE.md").exists() && !repo.join("AGENTS.md").exists(),
        "the instruction-file block is retired: nothing writes one"
    );
}

#[test]
fn yes_and_print_and_remove_do_not_combine() {
    let home = Scratch::for_test("setup", "flag-clash");
    let repo = Scratch::for_test("setup", "flag-clash-repo");
    for args in [
        ["setup", "--yes", "--print"],
        ["setup", "--yes", "--remove"],
        ["setup", "--print", "--remove"],
    ] {
        let out = run(&home, &repo, &args);
        assert!(!out.status.success(), "{args:?} must be refused");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("cannot be used with"),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
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
    let reverse = run(&home, &repo, &["setup", "--selected-features=1"]);
    assert!(!reverse.status.success());
    assert!(String::from_utf8_lossy(&reverse.stderr).contains("--selected-agents"));
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

    assert!(
        !repo.join("CLAUDE.md").exists() && !repo.join("AGENTS.md").exists(),
        "the retired block is not written to the repository either"
    );
    assert!(
        !home.join(".claude").exists(),
        "--repo keeps the home install untouched"
    );
    assert!(
        !home.join(".claude/settings.json").exists(),
        "and so do its hooks"
    );
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
fn a_second_run_leaves_the_skill_byte_identical() {
    let home = Scratch::for_test("setup", "twice-home");
    let repo = Scratch::for_test("setup", "twice-repo");
    fs::create_dir_all(home.join(".claude")).unwrap();
    let args = ["setup", "--selected-agents=1", "--selected-features=1,2,4"];
    let skill = ".claude/skills/pixel/SKILL.md";

    let first_run = run(&home, &repo, &args);
    assert!(
        first_run.status.success(),
        "the first run: {}",
        String::from_utf8_lossy(&first_run.stderr)
    );
    let first = fs::read_to_string(home.join(skill)).unwrap();
    let second_run = run(&home, &repo, &args);
    assert!(
        second_run.status.success(),
        "a second run must succeed on the state the first one left: {}",
        String::from_utf8_lossy(&second_run.stderr)
    );
    let second = fs::read_to_string(home.join(skill)).unwrap();

    assert_eq!(first, second, "the skill is stable across re-runs");
    assert!(first.contains("name: pixel"));
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
            "--selected-features=1,2,3,4",
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
fn a_users_own_paragraph_is_left_alone() {
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

    assert_eq!(
        fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap(),
        "# My rules\n\nKeep me.\n",
        "setup no longer edits instruction files at all"
    );
    assert!(
        home.join(".claude/skills/pixel/SKILL.md").is_file(),
        "the skill carries the pixel content instead"
    );
}

// TEMPORARY (dev-flags): drives the wizard with the two selection flags.
#[test]
fn the_hooks_follow_the_agents_and_the_hook_features_that_were_picked() {
    let picked = Scratch::for_test("setup", "hooks-picked-home");
    let features_off = Scratch::for_test("setup", "hooks-off-home");
    let other_agent = Scratch::for_test("setup", "hooks-other-agent-home");
    let repo = Scratch::for_test("setup", "hooks-repo");
    for home in [&picked, &features_off, &other_agent] {
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
    }
    let claude_hooks =
        |home: &Path| fs::read_to_string(home.join(".claude/settings.json")).unwrap_or_default();
    let task_event = "run-hook task-event --provider claude";
    // 1 prompt, 2 metrics, 3 daemon, 4 classify. Task hooks ride with Prompt;
    // Codex hooks are installed automatically when Codex is selected.
    let ok = |home: &Path, agents: &str, features: &str| {
        let out = run(
            home,
            &repo,
            &[
                "setup",
                &format!("--selected-agents={agents}"),
                &format!("--selected-features={features}"),
            ],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stderr).into_owned()
    };

    let review = ok(&picked, "1", "1");
    assert!(
        claude_hooks(&picked).contains(task_event),
        "Claude Code and Prompt: the Claude hooks are written"
    );
    assert!(
        review.contains("Hooks and agent configuration") && review.contains("hooks.claude"),
        "the review named the hook step before it was applied: {review}"
    );
    assert!(
        review.find("hooks.claude").unwrap() < review.find("pixel setup complete").unwrap(),
        "the review comes first"
    );

    ok(&features_off, "1", "2");
    assert!(
        !claude_hooks(&features_off).contains(task_event),
        "task hooks left off: the skill is written, the hooks are not"
    );
    assert!(features_off.join(".claude/skills/pixel/SKILL.md").is_file());

    ok(&other_agent, "2", "1");
    assert!(
        fs::read_to_string(other_agent.join(".codex/hooks.json"))
            .unwrap()
            .contains("task-event --provider codex"),
        "Codex was picked; its hooks are installed automatically"
    );
    assert!(
        !claude_hooks(&other_agent).contains(task_event),
        "Claude Code was not picked, so its settings are not touched"
    );
}

// TEMPORARY (dev-flags): drives the wizard with the two selection flags.
#[test]
fn remove_takes_out_the_skill_and_the_hooks_a_guided_run_wrote() {
    let home = Scratch::for_test("setup", "remove-home");
    let repo = Scratch::for_test("setup", "remove-repo");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(home.join(".claude/CLAUDE.md"), "# My rules\n\nKeep me.\n").unwrap();

    // Prompt: the task hooks and the skill deploy.
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
    assert!(
        fs::read_to_string(home.join(".claude/settings.json"))
            .unwrap()
            .contains("run-hook task-event"),
        "fixture: the hooks were written"
    );

    // The running binary is the test binary: name an absent one to remove.
    let absent = home.join(".local/bin/pixel");
    let removed = run(
        &home,
        &repo,
        &[
            "setup",
            "--remove",
            "--binary-path",
            absent.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );

    let claude_md = fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap();
    assert_eq!(
        claude_md, "# My rules\n\nKeep me.\n",
        "the user's file stays"
    );
    assert!(
        !home.join(".claude/skills/pixel").exists(),
        "the managed skill copy is removed"
    );
    let settings = fs::read_to_string(home.join(".claude/settings.json")).unwrap_or_default();
    assert!(
        !settings.contains("run-hook task-event"),
        "the hooks go: {settings}"
    );
}
