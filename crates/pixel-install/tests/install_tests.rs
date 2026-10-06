// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Integration tests for pixel-install: doctor and install.

use std::fs;
use std::path::Path;

use pixel_install::config::{MANAGED_BEGIN, MANAGED_END, PI_SETTINGS_FILE};
use pixel_install::doctor::{CHECKS, CheckStatus, DoctorOptions, doctor};
use pixel_install::install::{
    CheckStatus as StepStatus, InstallOptions, InstallReport, InstallStep, install,
};
use pixel_install::uninstall::{UninstallOptions, uninstall};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// test fixture: a fake "pixel" executable.
//
// pixel is a CLI + hooks tool, not an MCP server — `pixel install` no longer
// probes for an `mcp` subcommand or registers a pixel MCP server entry. The
// fixture below just stands in for a real pixel binary so install has
// something to write into the guard/session-start hook scripts.
// ---------------------------------------------------------------------------

/// Write a tiny shell script to `dir` standing in for a real pixel binary.
/// Used so the guard/session-start hook scripts point at a real executable.
#[cfg(unix)]
fn fake_pixel_exe(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("pixel");
    fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn task_hook_group(exe: &Path, provider: &str, event: &str) -> serde_json::Value {
    let executable = exe
        .canonicalize()
        .unwrap()
        .display()
        .to_string()
        .replace('\'', "'\\''");
    serde_json::json!({"hooks":[{
        "type":"command",
        "command":format!("'{executable}' run-hook task-event --provider {provider} --event {event}"),
        "timeout":if matches!(event, "session-end" | "interrupt") { 3 } else { 10 }
    }]})
}

/// Check every synchronous task boundary exactly once before comparing the
/// pre-existing lifecycle/foreign hooks independently of the new task hooks.
fn without_task_hooks(value: &serde_json::Value, provider: &str, exe: &Path) -> serde_json::Value {
    let mut remaining = value.clone();
    let extra = if provider == "claude" {
        ("PostToolUseFailure", "tool-failure")
    } else {
        ("Interrupt", "interrupt")
    };
    for (event, name) in [
        ("SessionStart", "session-start"),
        ("UserPromptSubmit", "prompt-submit"),
        ("PreToolUse", "pre-tool-use"),
        ("PostToolUse", "post-tool-use"),
        ("Stop", "stop"),
        ("SessionEnd", "session-end"),
        ("SubagentStart", "subagent-start"),
        ("SubagentStop", "subagent-stop"),
        extra,
    ] {
        let expected = task_hook_group(exe, provider, name);
        let groups = remaining["hooks"][event].as_array_mut().unwrap();
        assert_eq!(
            groups.iter().filter(|group| **group == expected).count(),
            1,
            "{event}: {groups:?}"
        );
        groups.retain(|group| group != &expected);
        if groups.is_empty() {
            remaining["hooks"].as_object_mut().unwrap().remove(event);
        }
    }
    remaining
}

// The install wires task-event accounting hooks into provider settings and
// removes legacy `claude()` shell-wrapper blocks. Every test pins an explicit
// shell instead of inheriting the runner's `$SHELL`, so a developer running
// `cargo test` from fish gets the same result as one running it from zsh —
// and so the fish cases below exercise fish on every machine.
/// Claude Code versions on both sides of the `--append-subagent-system-prompt-file`
/// line (its CHANGELOG entry is 2.1.261; earlier releases exit 1 on the
/// unknown option).
const CLAUDE_WITH_SUBAGENT_FLAG: &str = "2.1.269";

/// A fake `claude` that only answers `--version` the way Claude Code prints it.
#[cfg(unix)]
fn fake_claude_exe(dir: &std::path::Path, version: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(format!("claude-{version}"));
    fs::write(
        &path,
        format!("#!/bin/sh\nprintf '%s (Claude Code)\\n' '{version}'\n"),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

const TEST_SHELL: &str = "/bin/zsh";
fn shell_profile_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".zshrc")
}
const PIXEL_MANAGED_BEGIN: &str = "# >>> pixel-managed >>>";

/// The prompt files releases before the native default deployed under
/// `~/.local/share/pixel/`; no host reads them any more.
const RETIRED_PROMPTS: [&str; 2] = [
    ".local/share/pixel/agent-prompt.md",
    ".local/share/pixel/subagent-prompt.md",
];

/// Write both retired prompt files the way an earlier release left them.
fn write_retired_prompts(home: &Path) {
    for rel in RETIRED_PROMPTS {
        let path = home.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "# Pixel prompt from an earlier release\n").unwrap();
    }
}

/// Neither retired prompt file is on disk.
fn assert_no_retired_prompt(home: &Path) {
    for rel in RETIRED_PROMPTS {
        assert!(!home.join(rel).exists(), "{rel} must not be deployed");
    }
}

/// This proves instruction delivery and stream preservation, not model obedience.
#[test]
#[cfg(unix)]
fn global_install_registers_only_task_hooks_for_claude_and_codex() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let exe = fake_pixel_exe(home);
    install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(exe.clone()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .unwrap();
    assert_no_retired_prompt(home);

    // Claude receives task accounting only. Retrieval prompts, metrics,
    // PostToolUse advice and PreToolUse rewriting stay out of global hooks.
    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(home.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        without_task_hooks(&settings, "claude", &exe)["hooks"],
        serde_json::json!({}),
        "Claude global hooks contain no automatic retrieval callbacks: {settings}"
    );
    assert_eq!(codex_developer_instructions(home), None);
    let codex: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(home.join(".codex/hooks.json")).unwrap()).unwrap();
    assert_eq!(
        without_task_hooks(&codex, "codex", &exe)["hooks"],
        serde_json::json!({}),
        "Codex global hooks contain no retrieval callbacks or metrics relay: {codex}"
    );
}

// ---------------------------------------------------------------------------
// doctor tests
// ---------------------------------------------------------------------------

#[test]
fn doctor_runs_and_returns_report() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Doctor with no installed config — should still run and return a report
    // (some checks will be red, which is expected).
    let options = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None, // uses current_exe
        shell: Some(TEST_SHELL.into()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        ..Default::default()
    };

    let report = doctor(&options).expect("doctor runs");
    assert!(!report.checks.is_empty(), "doctor should produce checks");
    assert!(
        report.summary.green + report.summary.yellow + report.summary.red > 0,
        "summary should tally checks"
    );
}

/// A full run with a repo and a CLI parser runs every catalogued check, in
/// catalogue order: a check added without an entry would panic, and an entry
/// no run produces would be an id `--only` accepts but never runs.
#[test]
fn doctor_should_run_every_catalogued_check_in_order_when_nothing_is_filtered() {
    let dir = TempDir::new().unwrap();
    let report = doctor(&DoctorOptions {
        home: Some(dir.path().join("home")),
        repo_root: Some(dir.path().join("repo")),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();
    let ran: Vec<&str> = report.checks.iter().map(|c| c.id.as_str()).collect();
    let catalogue: Vec<&str> = CHECKS.iter().map(|c| c.id).collect();
    assert_eq!(ran, catalogue);
    assert_eq!(report.summary.skipped, 0);
}

/// `--only` runs just the named checks and `--skip` leaves its ids out; the
/// skip count lets a focused gate confirm it ran what it meant to.
#[test]
fn doctor_should_run_only_the_selected_checks_and_count_the_rest_as_skipped() {
    let dir = TempDir::new().unwrap();
    let options = |only: &[&str], skip: &[&str]| DoctorOptions {
        home: Some(dir.path().join("home")),
        repo_root: Some(dir.path().join("repo")),
        shell: Some(TEST_SHELL.into()),
        only: only.iter().map(ToString::to_string).collect(),
        skip: skip.iter().map(ToString::to_string).collect(),
        ..Default::default()
    };

    let only = doctor(&options(&["binary.path", "index.freshness"], &[])).unwrap();
    let ran: Vec<&str> = only.checks.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ran, ["binary.path", "index.freshness"]);
    assert_eq!(only.summary.skipped, CHECKS.len() - 2);

    let skip = doctor(&options(&[], &["binary.path"])).unwrap();
    assert!(skip.checks.iter().all(|c| c.id != "binary.path"));
    assert_eq!(skip.checks.len(), CHECKS.len() - 1);
    assert_eq!(skip.summary.skipped, 1);
}

#[test]
fn doctor_should_refuse_an_unknown_check_id_before_running_anything() {
    let dir = TempDir::new().unwrap();
    let err = doctor(&DoctorOptions {
        home: Some(dir.path().to_path_buf()),
        only: vec!["install.shell-wrappers".into()],
        ..Default::default()
    })
    .unwrap_err();
    assert!(
        matches!(&err, pixel_install::InstallError::UnknownDoctorCheck(id) if id == "install.shell-wrappers"),
        "{err}"
    );
}

/// The fix travels with the finding: a red install check names `pixel
/// install --shell <shell>`, a red repo check names its repository, a green check none.
#[test]
fn doctor_should_attach_a_fix_to_failing_checks_only() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    // A prompt an earlier release deployed makes the install check red.
    let home = dir.path().join("home");
    write_retired_prompts(&home);
    let report = doctor(&DoctorOptions {
        home: Some(home),
        repo_root: Some(repo.clone()),
        shell: Some(TEST_SHELL.into()),
        only: ["binary.path", "install.agent-prompt", "index.freshness"]
            .map(String::from)
            .to_vec(),
        ..Default::default()
    })
    .unwrap();
    let binary = check(&report, "binary.path");
    assert_eq!(
        (binary.status, binary.fix.as_deref()),
        (CheckStatus::Green, None)
    );
    let prompt = check(&report, "install.agent-prompt");
    assert_eq!(prompt.status, CheckStatus::Red);
    // The shell the check read travels with the fix, and with the argv
    // `--fix` runs, so the repair rewrites that same profile.
    assert_eq!(
        prompt.fix.as_deref(),
        Some(format!("pixel install --shell {TEST_SHELL}").as_str())
    );
    assert_eq!(
        prompt.repair,
        Some(vec![vec![
            "install".to_owned(),
            "--shell".to_owned(),
            TEST_SHELL.to_owned()
        ]])
    );
    let index = check(&report, "index.freshness");
    assert_eq!(index.status, CheckStatus::Red, "{index:?}");
    assert_eq!(
        index.fix.as_deref(),
        Some(format!("pixel prepare-repo '{}'", repo.display()).as_str())
    );
    assert!(report.fails(CheckStatus::Red));
}

// ---------------------------------------------------------------------------
// install tests
// ---------------------------------------------------------------------------

#[test]
fn install_creates_config_with_managed_markers() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Pre-create a CLAUDE.md with some existing content AND a stale pixel
    // managed block from a previous (hook-based) install.
    let original = format!(
        "# My Project\n\nSome notes.\n\n{MANAGED_BEGIN}\n# old pixel rules\n{MANAGED_END}\n"
    );
    fs::write(home.join("CLAUDE.md"), original.clone()).unwrap();

    let options = InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    };
    let report = install(&options).expect("install");

    assert!(report.ok, "install should succeed (ok=true)");
    assert!(report.summary.red == 0, "no red steps");
    assert!(report.summary.green > 0, "should have green steps");

    // The new install does NOT rewrite agent-config files — a stale managed
    // block from a previous install is left untouched (its cleanup is the
    // user's job, or `pixel uninstall`). Original content survives verbatim.
    let claude = fs::read_to_string(home.join("CLAUDE.md")).expect("CLAUDE.md");
    assert_eq!(
        claude, original,
        "CLAUDE.md must be byte-identical — install no longer rewrites agent configs"
    );
    // Task-event hooks are the install artifacts; no prompt file, shell
    // wrapper or automatic retrieval callback is written.
    assert_no_retired_prompt(home);
    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(home.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        settings["hooks"]["SessionStart"],
        serde_json::json!([task_hook_group(
            &home.join("pixel"),
            "claude",
            "session-start"
        )]),
        "Claude task-event hooks should be installed: {settings}"
    );
    assert!(
        !shell_profile_path(home).exists(),
        "no shell wrapper is installed"
    );
}

#[test]
fn install_is_idempotent() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    let options = InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    };

    // First install.
    let r1 = install(&options).expect("install 1");
    assert!(r1.ok);

    // Second install — should succeed again without error.
    let r2 = install(&options).expect("install 2");
    assert!(r2.ok, "second install should succeed");
    assert!(r2.summary.red == 0, "no red steps on re-install");

    // The install deploys Claude's task-event hooks, stable across
    // re-installs, and never a prompt file.
    let settings = home.join(".claude/settings.json");
    let s1 = fs::read(&settings).expect("claude settings installed");

    install(&options).expect("install 3");
    assert_no_retired_prompt(home);
    assert_eq!(
        fs::read(&settings).unwrap(),
        s1,
        "claude settings.json must be byte-identical across re-installs"
    );

    // No managed blocks are ever written by the new install.
    let claude = fs::read_to_string(home.join(".claude").join("CLAUDE.md")).unwrap_or_default();
    assert!(
        !claude.contains(MANAGED_BEGIN),
        "install must not write managed blocks"
    );
}

#[test]
fn global_install_removes_retired_codex_hook_and_preserves_foreign_config() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let codex_path = home.join(pixel_install::config::CODEX_HOOKS_FILE);
    fs::create_dir_all(codex_path.parent().unwrap()).unwrap();
    let original = serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "Bash",
                "hooks": [
                    { "type": "command", "command": "~/.claude/hooks/pixel-targets-guard" },
                    { "type": "command", "command": "~/.claude/hooks/keep-this-hook" }
                ]
            }]
        },
        "unrelated": true
    });
    fs::write(
        &codex_path,
        serde_json::to_string_pretty(&original).unwrap(),
    )
    .unwrap();

    let options = InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    };
    install(&options).expect("install");

    // Global installs add the task-event suite, remove the retired Pixel
    // retrieval hook, and preserve foreign hooks and unrelated config.
    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&codex_path).unwrap()).unwrap();
    assert!(
        !after.to_string().contains("pixel-targets-guard"),
        "the retired Pixel retrieval hook must be removed: {after}"
    );
    assert_eq!(
        without_task_hooks(&after, "codex", options.executable_path.as_ref().unwrap())["hooks"]["PreToolUse"],
        serde_json::json!([{
            "matcher": "Bash",
            "hooks": [{ "type": "command", "command": "~/.claude/hooks/keep-this-hook" }]
        }]),
        "the foreign command in the mixed hook group must survive exactly"
    );
    assert_eq!(after["unrelated"], true);
}

#[test]
fn install_leaves_settings_json_valid_after_install() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    // Pre-create .claude/settings.json so installed_agents detects Claude
    // even when the `claude` binary is not on PATH (e.g. Linux CI).
    let claude_dir = home.join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(claude_dir.join("settings.json"), "{}").unwrap();

    let options = InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: None,
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    };
    install(&options).expect("install");

    let settings_path = home.join(".claude").join("settings.json");
    let raw = fs::read_to_string(&settings_path).expect("settings.json readable");
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(&raw);
    assert!(
        parsed.is_ok(),
        "settings.json must remain valid JSON after install, got parse error: {:?}\ncontent:\n{}",
        parsed.err(),
        raw
    );
    // Regression guard: settings.json must never be run through the
    // Markdown managed-marker rewrite (find_agent_configs must not list it).
    assert!(
        !raw.contains(MANAGED_BEGIN) && !raw.contains(MANAGED_END),
        "settings.json must never contain Markdown managed markers, got:\n{raw}"
    );
}

#[test]
fn find_agent_configs_never_includes_settings_json() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let settings_dir = home.join(".claude");
    fs::create_dir_all(&settings_dir).unwrap();
    fs::write(settings_dir.join("settings.json"), "{}").unwrap();
    fs::write(home.join("CLAUDE.md"), "# Project\n").unwrap();

    let configs = pixel_install::config::find_agent_configs(home);
    assert!(
        !configs.iter().any(|p| p.ends_with("settings.json")),
        "find_agent_configs must never return settings.json, got: {configs:?}"
    );
    assert!(
        configs.iter().any(|p| p.ends_with("CLAUDE.md")),
        "find_agent_configs should still find CLAUDE.md, got: {configs:?}"
    );
}

#[test]
fn stale_block_removal_never_deletes_incidental_mentions() {
    // Regression test for a real, confirmed bug: the OLD implementation
    // deleted any line merely CONTAINING "gitnexus"/"codebase-memory" as a
    // substring, anywhere. A real ~/.claude/CLAUDE.md rule reads: "...
    // override every other discovery protocol (codebase-memory, gitnexus,
    // generic exploration)." — a hand-written bullet point listing OTHER
    // tools it deprioritizes, not a stale GitNexus block. That line must
    // survive untouched; only a genuine section HEADER announcing a
    // GitNexus/codebase-memory block should trigger removal.
    let original = "\
# My Rules

- While a manifest is active, targets override every other discovery \
protocol (codebase-memory, gitnexus, generic exploration).
- Some other rule entirely.
";
    let (cleaned, removed) = pixel_install::config::strip_stale_blocks(original);
    assert_eq!(
        removed, 0,
        "no genuine stale block header exists; nothing should be removed"
    );
    assert_eq!(
        cleaned, original,
        "a bare incidental mention of gitnexus/codebase-memory in hand-written prose must survive verbatim"
    );
}

// ---------------------------------------------------------------------------
// dry-run tests
// ---------------------------------------------------------------------------

#[test]
fn dry_run_writes_nothing_on_a_clean_home() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    let options = InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: true,
        shell: Some(TEST_SHELL.into()),
    };
    let report = install(&options).expect("dry-run install");

    // pixel is a CLI + hooks tool, not an MCP server — there is no mcp.pixel
    // step anymore. A dry-run on a clean home should report ok (all steps
    // green, nothing to write) and leave nothing on disk.
    assert!(report.dry_run, "report should mark itself as a dry run");
    assert!(
        report.ok,
        "dry-run on clean home should report ok: {report:?}"
    );

    // Nothing should exist on disk: no .claude dir, no CLAUDE.md, no hooks.
    assert!(
        !home.join(".claude").exists(),
        ".claude directory must not be created in dry-run mode"
    );
    assert!(
        !home.join("CLAUDE.md").exists(),
        "CLAUDE.md must not be created in dry-run mode"
    );
    assert!(
        !home.join(".claude").join("CLAUDE.md").exists(),
        ".claude/CLAUDE.md must not be created in dry-run mode"
    );
}

#[test]
fn dry_run_install_has_no_copilot_step_without_copilot_config() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    let report = install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: true,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("dry-run install");

    // Pixel only writes ~/.copilot/hooks/pixel.json when ~/.copilot already
    // exists. A machine that has never run Copilot CLI gets no copilot-hooks
    // step and no directory fabricated for it.
    assert!(
        !report.steps.iter().any(|s| s.id == "copilot-hooks"),
        "no copilot step without ~/.copilot: {report:?}"
    );
    assert!(!home.join(".copilot").exists());
}

/// Copilot keeps its native tools: on a machine that has run Copilot CLI,
/// `pixel install` writes no hook file, and the `pixel.json` an earlier
/// release deployed is removed while the user's own hook files stay.
#[test]
fn install_removes_the_copilot_hooks_an_earlier_release_wrote() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let hooks_dir = home.join(".copilot").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let options = InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    };

    // Nothing to remove: no hook file appears.
    let report = install(&options).expect("install");
    assert!(report.ok, "{report:?}");
    assert_eq!(fs::read_dir(&hooks_dir).unwrap().count(), 0);

    let pixel = hooks_dir.join("pixel.json");
    let retired = serde_json::json!({
        "version": 1,
        "_pixel_managed": "pixel-managed-copilot-hooks-v1",
        "hooks": {
            "preToolUse": [{"type": "exec", "exec": "/opt/pixel", "args": ["run-hook", "guard", "--provider", "copilot"]}],
            "postToolUse": [{"type": "exec", "exec": "/opt/pixel", "args": ["run-hook", "metrics", "--provider", "copilot"]}],
        }
    });
    fs::write(&pixel, serde_json::to_string_pretty(&retired).unwrap()).unwrap();
    let mine = hooks_dir.join("mine.json");
    let user_hooks =
        "{\"version\":1,\"hooks\":{\"preToolUse\":[{\"type\":\"exec\",\"exec\":\"notify\"}]}}\n";
    fs::write(&mine, user_hooks).unwrap();

    let report = install(&options).expect("install over an earlier release");
    assert!(report.ok, "{report:?}");
    let step = report
        .steps
        .iter()
        .find(|s| s.id == "copilot-hooks")
        .expect("copilot step");
    assert_eq!(step.summary, format!("removed {}", pixel.display()));
    assert!(!pixel.exists(), "the retired Pixel hook file is removed");
    assert_eq!(fs::read_to_string(&mine).unwrap(), user_hooks);
    assert_eq!(
        fs::read_dir(&hooks_dir).unwrap().count(),
        1,
        "only the user's hook file is left"
    );
}

/// Cursor, ZCode and Antigravity keep their native tools: on a machine that
/// uses them, `pixel install` writes no hook, prompt or plugin, and removes
/// the ones an earlier release wrote while every foreign entry, setting and
/// line of user text stays. Antigravity's doctor check is red on the
/// leftovers and green after.
#[test]
#[cfg(unix)]
fn global_install_removes_the_cursor_zcode_and_antigravity_integrations_an_earlier_release_wrote() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let old = "'/opt/old/pixel'";

    let cursor = home.join(".cursor/hooks.json");
    fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    fs::write(
        &cursor,
        serde_json::json!({"version": 1, "hooks": {
            "preToolUse": [
                {"command": format!("{old} run-hook guard --provider cursor"), "matcher": "Shell"},
                {"command": "notify-send done"},
            ],
            "postToolUse": [{"command": format!("{old} run-hook metrics --provider cursor")}],
        }})
        .to_string(),
    )
    .unwrap();

    let zcode = home.join(".zcode/cli/config.json");
    fs::create_dir_all(zcode.parent().unwrap()).unwrap();
    let zcode_foreign = serde_json::json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "keep-zcode-check"}]});
    fs::write(
        &zcode,
        serde_json::json!({"model": "glm", "hooks": {"events": {"PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": format!("{old} run-hook guard --provider zcode")}]},
            zcode_foreign.clone(),
        ]}}})
        .to_string(),
    )
    .unwrap();
    let zcode_agents = home.join(".zcode/AGENTS.md");
    fs::write(
        &zcode_agents,
        format!("mine\n{MANAGED_BEGIN}\nold prompt\n{MANAGED_END}\n"),
    )
    .unwrap();

    let gemini = home.join(".gemini/config");
    let plugin = gemini.join("plugins/pixel");
    fs::create_dir_all(&plugin).unwrap();
    fs::write(plugin.join("plugin.json"), r#"{"managedBy":"pixel"}"#).unwrap();
    fs::write(
        gemini.join("config.json"),
        r#"{"plugins":{"other":{"enabled":true},"pixel":{"enabled":true}},"keep":"config"}"#,
    )
    .unwrap();
    let mine =
        serde_json::json!({"PreToolUse": [{"matcher": "*", "hooks": [{"command": "audit"}]}]});
    fs::write(
        gemini.join("hooks.json"),
        serde_json::json!({
            "pixel-guard": {"PreToolUse": [{"matcher": "*", "hooks": [{"command": format!("{old} run-hook guard --provider antigravity")}]}]},
            "mine": mine.clone(),
        })
        .to_string(),
    )
    .unwrap();

    let antigravity_check = || {
        let report = doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            shell: Some(TEST_SHELL.into()),
            only: vec!["install.antigravity".into()],
            ..Default::default()
        })
        .unwrap();
        check(&report, "install.antigravity").clone()
    };
    let red = antigravity_check();
    assert_eq!(red.status, CheckStatus::Red, "{red:?}");
    assert_eq!(
        red.reason.as_deref(),
        Some(
            format!(
                "retired Pixel Antigravity integration remains: {}, the pixel plugin entry in {}, the pixel-guard in {} — run `pixel install` to remove it",
                plugin.display(),
                gemini.join("config.json").display(),
                gemini.join("hooks.json").display()
            )
            .as_str()
        )
    );

    let report = install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("install");
    assert!(report.ok, "{report:?}");

    assert_eq!(
        read_json(&cursor),
        serde_json::json!({"version": 1, "hooks": {"preToolUse": [{"command": "notify-send done"}]}})
    );
    assert_eq!(
        read_json(&zcode),
        serde_json::json!({"model": "glm", "hooks": {"events": {"PreToolUse": [zcode_foreign]}}})
    );
    assert_eq!(fs::read_to_string(&zcode_agents).unwrap(), "mine\n");
    assert!(!plugin.exists(), "the managed plugin directory is removed");
    assert_eq!(
        read_json(&gemini.join("config.json")),
        serde_json::json!({"plugins": {"other": {"enabled": true}}, "keep": "config"})
    );
    assert_eq!(
        read_json(&gemini.join("hooks.json")),
        serde_json::json!({"mine": mine})
    );
    assert_eq!(antigravity_check().status, CheckStatus::Green);
}

#[test]
fn dry_run_leaves_pre_existing_files_byte_identical() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    // Pre-create .claude/settings.json so installed_agents detects Claude
    // even when the `claude` binary is not on PATH (e.g. Linux CI).
    let claude_dir = home.join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(claude_dir.join("settings.json"), "{}").unwrap();

    // Pre-create real state as if a previous non-dry-run install ran.
    let real_options = InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: None,
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    };
    install(&real_options).expect("real install");

    let settings_path = home.join(".claude").join("settings.json");
    // A prompt an earlier release deployed: a real install removes it, a
    // dry run only says it would.
    write_retired_prompts(home);
    let prompt_path = home.join(RETIRED_PROMPTS[0]);
    // A profile with user content (no pixel block): the legacy-wrapper
    // cleanup must leave it byte-identical, dry-run or not.
    let profile_path = shell_profile_path(home);
    fs::write(&profile_path, "# user aliases\nexport EDITOR=vim\n").unwrap();
    let before_settings = fs::read(&settings_path).unwrap();
    let before_prompt = fs::read(&prompt_path).unwrap();
    let before_profile = fs::read(&profile_path).unwrap();

    // A dry-run install afterwards must not touch anything, even though a
    // real install already exists (idempotent no-op path).
    let dry_options = InstallOptions {
        repo: None,
        dry_run: true,
        shell: Some(TEST_SHELL.into()),
        ..real_options
    };
    let report = install(&dry_options).expect("dry-run install over existing state");
    assert!(report.dry_run);

    let after_settings = fs::read(&settings_path).unwrap();
    let after_prompt = fs::read(&prompt_path).unwrap();
    let after_profile = fs::read(&profile_path).unwrap();
    assert_eq!(
        before_settings, after_settings,
        "dry-run must not modify settings.json"
    );
    assert_eq!(
        before_prompt, after_prompt,
        "dry-run must not remove the retired agent-prompt.md"
    );
    assert!(home.join(RETIRED_PROMPTS[1]).is_file());
    assert_eq!(
        before_profile, after_profile,
        "dry-run must not modify the shell profile"
    );

    // And it must not have written any backup files either.
    let home_entries: Vec<String> = fs::read_dir(home)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !home_entries.iter().any(|n| n.contains("pixel-bak")),
        "dry-run must not create backup files, got: {home_entries:?}"
    );
}

// ---------------------------------------------------------------------------
// capability advertisement — the SessionStart block is derived from the live
// op registry in pixel-proto (`SESSION_CAPABILITIES`, tested exhaustively
// there); the old hand-maintained duplicate registry in this crate is gone.
// ---------------------------------------------------------------------------

#[test]
fn session_capabilities_registry_is_live_and_excludes_internal_ops() {
    let caps = pixel_proto::op::SESSION_CAPABILITIES;
    for expected in [
        "search", "targets", "publish", "push", "ship", "resolve", "impact",
    ] {
        assert!(
            caps.contains(&expected),
            "expected capability {expected} missing from SESSION_CAPABILITIES"
        );
    }
    assert!(
        !caps.contains(&"shutdown"),
        "internal shutdown op must not be advertised as a capability"
    );
}

#[test]
fn reinstall_is_byte_for_byte_idempotent_on_managed_claude_md() {
    // Regression test: apply_managed_markers previously grew the file by
    // one trailing newline on every re-install (295 bytes -> 296 -> 297...)
    // because the tail extraction re-included the block's own trailing
    // newline. Three consecutive installs must produce byte-identical
    // output after the first.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::write(home.join("CLAUDE.md"), "# Project\n\nHand-written notes.\n").unwrap();
    let options = InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: None,
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    };
    install(&options).expect("install 1");
    let c1 = fs::read_to_string(home.join("CLAUDE.md")).unwrap();
    install(&options).expect("install 2");
    let c2 = fs::read_to_string(home.join("CLAUDE.md")).unwrap();
    install(&options).expect("install 3");
    let c3 = fs::read_to_string(home.join("CLAUDE.md")).unwrap();
    assert_eq!(c1, c2, "second install must not change CLAUDE.md at all");
    assert_eq!(c2, c3, "third install must not change CLAUDE.md at all");
}

#[test]
fn install_on_a_fresh_home_creates_claude_md_even_with_no_pre_existing_file() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    // Deliberately do NOT pre-create CLAUDE.md or AGENTS.md.
    let options = InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: None,
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    };
    install(&options).expect("install on fresh home");

    // The new install does NOT create or rewrite any CLAUDE.md/AGENTS.md,
    // and deploys no prompt file either.
    assert!(
        !home.join("CLAUDE.md").exists(),
        "fresh install must not create root CLAUDE.md"
    );
    assert!(
        !home.join(".claude").join("CLAUDE.md").exists(),
        "fresh install must not create .claude/CLAUDE.md"
    );
    assert!(
        !home.join("AGENTS.md").exists(),
        "fresh install must not create root AGENTS.md"
    );

    // No agent system prompt is deployed: without Pi there is nothing for
    // Pixel to keep under its data directory at all.
    assert_no_retired_prompt(home);
    assert!(
        !home.join(".local/share/pixel").exists(),
        "a fresh install without Pi writes nothing under ~/.local/share/pixel"
    );

    // Claude task-event hooks are installed in ~/.claude/settings.json; no
    // retrieval callbacks or shell profile are created.
    let settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(home.join(".claude/settings.json"))
            .expect("claude settings.json should be created on a fresh home"),
    )
    .unwrap();
    // Only task-event hooks are installed globally; retrieval callbacks are
    // absent from every event.
    let legacy = without_task_hooks(&settings, "claude", &std::env::current_exe().unwrap());
    assert!(
        legacy["hooks"] == serde_json::json!({}),
        "global install must not add automatic callbacks: {settings}"
    );
    assert!(
        !shell_profile_path(home).exists(),
        "no shell wrapper is written"
    );
    assert_eq!(
        codex_developer_instructions(home),
        None,
        "a fresh install leaves Codex's native instructions untouched"
    );
}

// ---------------------------------------------------------------------------
// doctor install-artifact check tests (agent-prompt, claude-hooks,
// legacy-wrappers)
// ---------------------------------------------------------------------------

#[test]
fn doctor_install_artifact_checks_red_and_green() {
    // Doctor verifies the install artifacts: install.agent-prompt (no
    // retired prompt file is left), install.claude-hooks (the task-event
    // hooks), and install.legacy-wrappers (no stale `claude()` block
    // survives). This test walks each through its red and green states.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // The doctor runs as the binary that installs below, as it does in
    // production: hooks wired to another binary are yellow on their own.
    let doc_opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        shell: Some(TEST_SHELL.into()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        ..Default::default()
    };

    // 1. With nothing installed, claude-hooks is red; agent-prompt and
    //    legacy-wrappers are green (there is nothing to remove).
    let report = doctor(&doc_opts).expect("doctor runs");
    let prompt_check = check(&report, "install.agent-prompt");
    assert_eq!(
        prompt_check.status,
        pixel_install::doctor::CheckStatus::Green,
        "agent-prompt is green when no prompt file exists: {prompt_check:?}"
    );
    let hooks_check = report
        .checks
        .iter()
        .find(|c| c.id == "install.claude-hooks")
        .unwrap();
    assert_eq!(
        hooks_check.status,
        pixel_install::doctor::CheckStatus::Red,
        "claude-hooks should be red when not installed"
    );
    let legacy_check = report
        .checks
        .iter()
        .find(|c| c.id == "install.legacy-wrappers")
        .unwrap();
    assert_eq!(
        legacy_check.status,
        pixel_install::doctor::CheckStatus::Green,
        "legacy-wrappers should be green when no stale block exists"
    );

    // 2. Prompt files an earlier release deployed are red, each named, with
    //    the install that removes them as the fix.
    write_retired_prompts(home);
    let report = doctor(&doc_opts).expect("doctor runs");
    let prompt_check = check(&report, "install.agent-prompt");
    assert_eq!(
        prompt_check.status,
        pixel_install::doctor::CheckStatus::Red,
        "{prompt_check:?}"
    );
    assert_eq!(
        prompt_check.reason.as_deref(),
        Some(
            format!(
                "retired Pixel prompt file(s) remain: {}, {} — run `pixel install` to remove them",
                home.join(RETIRED_PROMPTS[0]).display(),
                home.join(RETIRED_PROMPTS[1]).display()
            )
            .as_str()
        )
    );
    assert_eq!(
        prompt_check.fix.as_deref(),
        Some(format!("pixel install --shell {TEST_SHELL}").as_str())
    );

    // 3. Run install: removes the prompts, installs claude task hooks → all
    //    green.
    install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("install");

    let report = doctor(&doc_opts).expect("doctor runs");
    for id in [
        "install.agent-prompt",
        "install.claude-hooks",
        "install.legacy-wrappers",
    ] {
        let check = report.checks.iter().find(|c| c.id == id).unwrap();
        assert_eq!(
            check.status,
            pixel_install::doctor::CheckStatus::Green,
            "{id} should be green after install, got {:?}: {:?}",
            check.status,
            check.reason
        );
    }
    assert_no_retired_prompt(home);
}

fn check<'a>(
    report: &'a pixel_install::doctor::DoctorReport,
    id: &str,
) -> &'a pixel_install::doctor::DoctorCheck {
    report
        .checks
        .iter()
        .find(|c| c.id == id)
        .unwrap_or_else(|| panic!("doctor has no {id} check"))
}

/// No prompt or rule text is deployed any more, so the checks that validated
/// it are retired: their ids are no longer catalogued, and naming one is
/// refused like any unknown id rather than silently running nothing.
#[test]
fn doctor_should_refuse_the_retired_prompt_validation_check_ids() {
    let dir = TempDir::new().unwrap();
    for id in ["install.subagent-prompt", "rule.parity", "rule.scenarios"] {
        assert!(
            CHECKS.iter().all(|c| c.id != id),
            "{id} is still catalogued"
        );
        let err = doctor(&DoctorOptions {
            home: Some(dir.path().to_path_buf()),
            only: vec![id.into()],
            ..Default::default()
        })
        .unwrap_err();
        assert!(
            matches!(&err, pixel_install::InstallError::UnknownDoctorCheck(found) if found == id),
            "{err}"
        );
    }
    assert_eq!(
        CHECKS.iter().filter(|c| c.id.starts_with("rule.")).count(),
        0
    );
}

// ---------------------------------------------------------------------------
// uninstall tests
// ---------------------------------------------------------------------------

/// After uninstall, CLAUDE.md should have no managed block but the original
/// user content should be preserved. The new install no longer writes
/// managed blocks, so the fixture manually creates one (modeling a leftover
/// from a previous hook-based install) for uninstall to strip.
#[test]
fn uninstall_removes_managed_block_and_preserves_user_content() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Manually create a CLAUDE.md with user content AND a stale pixel
    // managed block (install() no longer writes these).
    let original = format!(
        "# My Project\n\nSome notes.\n\n{MANAGED_BEGIN}\n# old pixel rules\n{MANAGED_END}\n"
    );
    fs::write(home.join("CLAUDE.md"), original).unwrap();

    let claude = fs::read_to_string(home.join("CLAUDE.md")).unwrap();
    assert!(
        claude.contains(MANAGED_BEGIN),
        "fixture should carry a managed block"
    );

    // Uninstall
    let uninstall_opts = UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    };
    let report = uninstall(&uninstall_opts).expect("uninstall");
    assert!(report.ok, "uninstall should succeed");
    assert_eq!(report.summary.red, 0, "no red steps");

    let claude = fs::read_to_string(home.join("CLAUDE.md")).unwrap();
    assert!(
        !claude.contains(MANAGED_BEGIN),
        "CLAUDE.md should have no managed block after uninstall"
    );
    assert!(
        claude.contains("Some notes."),
        "original user content should be preserved after uninstall"
    );
}

#[test]
fn uninstall_removes_pixel_managed_copilot_hooks() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // A leftover pixel-managed Copilot hooks file (as `pixel install` writes).
    let hooks = home.join(".copilot").join("hooks").join("pixel.json");
    fs::create_dir_all(hooks.parent().unwrap()).unwrap();
    fs::write(
        &hooks,
        "{\"version\":1,\"_pixel_managed\":\"pixel-managed-copilot-hooks-v1\",\"hooks\":{}}\n",
    )
    .unwrap();

    let uninstall_opts = UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    };
    let report = uninstall(&uninstall_opts).expect("uninstall");
    assert!(report.ok, "uninstall should succeed");
    assert_eq!(report.summary.red, 0, "no red steps");
    assert!(
        !hooks.exists(),
        "pixel-managed copilot hooks must be removed on uninstall"
    );
}

/// After uninstall, Claude settings.json should have no pixel run-hook entries,
/// and the hook scripts should be deleted. The new install no longer
/// installs hooks or scripts, so the fixture manually creates them
/// (modeling a leftover from a previous hook-based install) for uninstall
/// to remove.
#[test]
fn uninstall_removes_claude_hooks_and_scripts() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let claude_dir = home.join(".claude");
    let hooks_dir = claude_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();

    // Manually wire pixel run-hook entries into settings.json (install() no
    // longer does this) — including the blocking guard, a session-start,
    // and a prompt-submit entry.
    let settings = claude_dir.join("settings.json");
    fs::write(
        &settings,
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{ "type": "command", "command": "~/.claude/hooks/pixel-targets-guard" }]
                }],
                "SessionStart": [{
                    "hooks": [{ "type": "command", "command": "~/.claude/hooks/pixel-session-start" }]
                }],
                "UserPromptSubmit": [{
                    "hooks": [{ "type": "command", "command": "~/.claude/hooks/pixel-prompt-submit" }]
                }]
            }
        }))
        .unwrap(),
    )
    .unwrap();

    // Manually create the hook scripts (install() no longer does this).
    let guard_script = hooks_dir.join("pixel-targets-guard");
    let session_script = hooks_dir.join("pixel-session-start");
    let prompt_script = hooks_dir.join("pixel-prompt-submit");
    for script in [&guard_script, &session_script, &prompt_script] {
        fs::write(script, "#!/bin/sh\nexit 0\n").unwrap();
    }
    assert!(guard_script.is_file(), "fixture guard script present");
    assert!(session_script.is_file(), "fixture session script present");

    // Uninstall
    let uninstall_opts = UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    };
    uninstall(&uninstall_opts).expect("uninstall");

    // Settings should have no pixel run-hook references.
    let settings_content = fs::read_to_string(&settings).unwrap_or_default();
    assert!(
        !settings_content.contains("pixel-targets-guard"),
        "settings should have no pixel guard hook after uninstall"
    );
    assert!(
        !settings_content.contains("pixel-session-start"),
        "settings should have no pixel session-start hook after uninstall"
    );
    assert!(
        !settings_content.contains("pixel-prompt-submit"),
        "settings should have no pixel prompt-submit hook after uninstall"
    );

    // Hook scripts should be deleted.
    assert!(!guard_script.is_file(), "guard script should be deleted");
    assert!(
        !session_script.is_file(),
        "session-start script should be deleted"
    );
    assert!(
        !prompt_script.is_file(),
        "prompt-submit script should be deleted"
    );
}

/// Uninstall removes the pixel binary.
#[test]
fn uninstall_removes_binary() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let bin = home.join("pixel");
    fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();

    let opts = UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        binary_path: Some(bin.clone()),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    };
    uninstall(&opts).expect("uninstall");

    assert!(!bin.is_file(), "binary should be deleted after uninstall");
}

fn binary_step(report: &InstallReport) -> &InstallStep {
    report
        .steps
        .iter()
        .find(|s| s.id == "binary")
        .expect("binary step")
}

fn uninstall_running(home: &Path, running: &Path) -> InstallReport {
    uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        running_binary: Some(running.to_path_buf()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .expect("uninstall")
}

/// `install.sh` with `PIXEL_INSTALL_DIR` puts the binary outside
/// `~/.local/bin`; uninstall must remove the one that runs, not report "no
/// binary found" and leave it — and must not touch another copy.
#[test]
fn uninstall_removes_the_running_binary_outside_local_bin() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let running = home.join("opt/pixel/bin/pixel");
    fs::create_dir_all(running.parent().unwrap()).unwrap();
    fs::write(&running, "#!/bin/sh\nexit 0\n").unwrap();
    let other = home.join(".local/bin/pixel");
    fs::create_dir_all(other.parent().unwrap()).unwrap();
    fs::write(&other, "#!/bin/sh\nexit 0\n").unwrap();

    let report = uninstall_running(home, &running);

    assert!(!running.exists(), "the running binary must be removed");
    assert!(
        other.is_file(),
        "another copy is not the one being uninstalled"
    );
    let step = binary_step(&report);
    assert_eq!(step.status, StepStatus::Green);
    assert_eq!(step.summary, "removed pixel binary");
    assert_eq!(
        step.detail.as_deref(),
        Some(format!("path={}", running.display()).as_str())
    );
    assert_eq!(report.executable_path, running.display().to_string());
}

/// A Homebrew binary, reached through the prefix symlink as `brew` links it
/// (Linuxbrew here): deleting it would leave Homebrew listing a formula whose
/// file is gone, so uninstall leaves it and names `brew uninstall`.
#[test]
fn uninstall_leaves_a_homebrew_binary_to_brew() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let prefix = home.join("linuxbrew/.linuxbrew");
    let cellar = prefix.join("Cellar/pixel/0.6.1/bin/pixel");
    fs::create_dir_all(cellar.parent().unwrap()).unwrap();
    fs::write(&cellar, "#!/bin/sh\nexit 0\n").unwrap();
    let link = prefix.join("bin/pixel");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&cellar, &link).unwrap();

    let report = uninstall_running(home, &link);

    assert!(cellar.is_file(), "the Cellar file stays");
    assert!(link.exists(), "the brew link stays");
    let step = binary_step(&report);
    assert_eq!(step.status, StepStatus::Yellow);
    assert_eq!(
        step.summary,
        "left the pixel binary to Homebrew: remove it with `brew uninstall pixel`"
    );
    assert!(
        report.ok,
        "a binary left to its manager is not a failed uninstall"
    );
}

/// A mise install is left to mise the same way, naming the directory mise
/// installed it under rather than guessing the tool's spelling.
#[test]
fn uninstall_leaves_a_mise_binary_to_mise() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let bin = home.join(".local/share/mise/installs/ubi-liviogama-pixel/0.6.1/pixel");
    fs::create_dir_all(bin.parent().unwrap()).unwrap();
    fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();

    let report = uninstall_running(home, &bin);

    assert!(bin.is_file());
    let step = binary_step(&report);
    assert_eq!(step.status, StepStatus::Yellow);
    assert_eq!(
        step.summary,
        "left the pixel binary to mise: remove it with `mise uninstall` on the tool installed \
         under `installs/ubi-liviogama-pixel`"
    );
}

/// `--binary-path` is the user's decision: it wins over the running binary
/// and is honoured even inside a Cellar, as `--install-path` is for upgrades.
#[test]
fn uninstall_binary_path_wins_over_the_running_binary_even_when_managed() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let named = home.join("Cellar/pixel/0.6.1/bin/pixel");
    fs::create_dir_all(named.parent().unwrap()).unwrap();
    fs::write(&named, "#!/bin/sh\nexit 0\n").unwrap();
    let running = home.join("runner/pixel");
    fs::create_dir_all(running.parent().unwrap()).unwrap();
    fs::write(&running, "#!/bin/sh\nexit 0\n").unwrap();

    let report = uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(named.clone()),
        running_binary: Some(running.clone()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .expect("uninstall");

    assert!(!named.exists(), "the named binary is removed");
    assert!(running.is_file(), "the running binary was not named");
    assert_eq!(binary_step(&report).status, StepStatus::Green);
}

/// Without a running binary (library callers), the historical target stays.
#[test]
fn uninstall_without_running_binary_targets_local_bin() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let bin = home.join(".local/bin/pixel");
    fs::create_dir_all(bin.parent().unwrap()).unwrap();
    fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();

    let report = uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .expect("uninstall");

    assert!(!bin.exists());
    assert_eq!(binary_step(&report).summary, "removed pixel binary");
}

/// Uninstall is idempotent: running twice does not error.
#[test]
fn uninstall_is_idempotent() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    let install_opts = InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    };
    install(&install_opts).expect("install");

    let uninstall_opts = UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    };
    let r1 = uninstall(&uninstall_opts).expect("uninstall 1");
    assert!(r1.ok);

    // Second uninstall — should succeed, finding nothing to remove.
    let r2 = uninstall(&uninstall_opts).expect("uninstall 2");
    assert!(r2.ok, "second uninstall should succeed");
    assert_eq!(r2.summary.red, 0, "no red steps on re-uninstall");
}

/// Every file under `root` whose name carries the `.pixel-bak.` marker, found
/// by walking the whole tree: what is really on disk, independent of the
/// directories uninstall chooses to look in.
fn backups_on_disk(root: &Path) -> Vec<std::path::PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if entry.file_name().to_string_lossy().contains(".pixel-bak.") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

/// The paths of `rm -- '<a>' '<b>'` as a POSIX shell reads them: each word
/// single-quoted, a quote inside one spelled `'\''`.
fn rm_command_paths(command: &str) -> Vec<std::path::PathBuf> {
    let words = command
        .strip_prefix("rm -- ")
        .unwrap_or_else(|| panic!("not an rm command: {command}"));
    let mut paths = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = words.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => quoted = !quoted,
            '\\' if !quoted => current.push(chars.next().expect("escaped char")),
            ' ' if !quoted => paths.push(std::path::PathBuf::from(std::mem::take(&mut current))),
            _ => current.push(c),
        }
    }
    paths.push(std::path::PathBuf::from(current));
    paths
}

fn backups_step(report: &InstallReport) -> &InstallStep {
    report
        .steps
        .iter()
        .find(|s| s.id == "backups")
        .expect("backups step")
}

/// Uninstall keeps every backup install and uninstall wrote (each is the only
/// undo of one write), but a user who never asked for them must learn they
/// exist and how to drop them: the report lists exactly the backups on disk,
/// and the command it prints removes them all, even from a home whose path a
/// shell would split or cut at the quote.
#[test]
#[cfg(unix)]
fn uninstall_reports_every_backup_it_leaves_with_a_command_that_removes_them() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path().join("it's my home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::create_dir_all(home.join(".pi/agent")).unwrap();
    let personal = [
        (
            ".claude/settings.json",
            r#"{"model":"opus","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo mine"}]}]}}"#,
        ),
        (
            ".codex/hooks.json",
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo my-codex"}]}]}}"#,
        ),
        (".pi/agent/APPEND_SYSTEM.md", "# my pi system append\n"),
        (".zshrc", "export MINE=1\n"),
    ];
    for (rel, text) in personal {
        fs::write(home.join(rel), text).unwrap();
    }
    install(&InstallOptions {
        repo: None,
        home: Some(home.clone()),
        executable_path: Some(fake_pixel_exe(&home)),
        claude_executable: Some(fake_claude_exe(&home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("install");

    let report = uninstall(&UninstallOptions {
        repo: None,
        home: Some(home.clone()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .expect("uninstall");

    let on_disk = backups_on_disk(&home);
    let backup_of = |rel: &str| {
        let prefix = format!("{}.pixel-bak.", home.join(rel).display());
        on_disk
            .iter()
            .any(|p| p.display().to_string().starts_with(&prefix))
    };
    for rel in [
        ".claude/settings.json",
        ".codex/hooks.json",
        ".pi/agent/settings.json",
    ] {
        assert!(backup_of(rel), "no backup of {rel} in {on_disk:?}");
    }
    let step = backups_step(&report);
    assert_eq!(step.status, StepStatus::Green, "{step:?}");
    assert_eq!(
        step.summary,
        format!(
            "kept {} backup(s) of the files pixel rewrote, each the undo of one write; remove them once you no longer need them",
            on_disk.len()
        )
    );
    let command = step.detail.as_deref().expect("the command to remove them");
    assert_eq!(rm_command_paths(command), on_disk, "{command}");

    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .unwrap();
    assert!(out.status.success(), "{command}: {out:?}");
    assert_eq!(backups_on_disk(&home), Vec::<std::path::PathBuf>::new());
    // Settings come back as equal JSON values (pixel rewrites them
    // formatted), text files byte for byte.
    for (rel, text) in personal {
        let now = fs::read_to_string(home.join(rel)).unwrap();
        if rel.ends_with(".json") {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&now).unwrap(),
                serde_json::from_str::<serde_json::Value>(text).unwrap(),
                "{rel} must be the user's own settings again, and survive the rm"
            );
        } else {
            assert_eq!(
                now, text,
                "{rel} must be the user's own file again, and survive the rm"
            );
        }
    }

    let again = uninstall(&UninstallOptions {
        repo: None,
        home: Some(home.clone()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .expect("second uninstall");
    let step = backups_step(&again);
    assert_eq!(step.summary, "no pixel backup left");
    assert_eq!(step.detail, None);
}

/// `pixel uninstall --repo` names the backups it and `pixel install --repo`
/// left inside the repository, the same way the global uninstall does.
#[test]
#[cfg(unix)]
fn repo_uninstall_reports_the_backups_left_in_the_repository() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("a 'repo'");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("AGENTS.md"), "# user instruction\n").unwrap();
    // A Devin guard an earlier release wrote beside a foreign hook: the
    // install that removes it backs the file up first.
    fs::create_dir_all(repo.join(".devin")).unwrap();
    fs::write(
        repo.join(".devin/config.local.json"),
        serde_json::json!({"hooks":{"PreToolUse":[
            {"matcher":"exec","hooks":[{"type":"command","command":"/opt/old/pixel run-hook guard --provider devin"}]},
            {"matcher":"exec","hooks":[{"type":"command","command":"audit-exec"}]},
        ]}})
        .to_string(),
    )
    .unwrap();
    install(&repo_install_options(&repo, &home)).expect("repo install");
    assert_eq!(
        backups_on_disk(&repo.join(".devin")).len(),
        1,
        "the removal backed up the Devin config"
    );

    let report = uninstall(&UninstallOptions {
        home: Some(home.clone()),
        repo: Some(repo.clone()),
        ..Default::default()
    })
    .expect("repo uninstall");

    let on_disk = backups_on_disk(&repo);
    assert!(
        on_disk.iter().all(|p| p.parent() != Some(repo.as_path())),
        "removing retired guidance does not rewrite or back up user AGENTS.md: {on_disk:?}"
    );
    let command = backups_step(&report).detail.as_deref().expect("command");
    assert_eq!(rm_command_paths(command), on_disk, "{command}");
    assert_eq!(
        fs::read_to_string(repo.join("AGENTS.md")).unwrap(),
        "# user instruction\n"
    );
}

/// Dry-run uninstall does not modify the filesystem. The new install no
/// longer writes managed blocks, so the fixture manually creates one (plus
/// the pixel binary) for the dry-run to report against without touching.
#[test]
fn uninstall_dry_run_does_not_modify() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Manually create a CLAUDE.md with a stale pixel managed block (install()
    // no longer writes these) and the pixel binary.
    let claude_path = home.join("CLAUDE.md");
    fs::write(
        &claude_path,
        format!("# Project\n\n{MANAGED_BEGIN}\n# old pixel rules\n{MANAGED_END}\n"),
    )
    .unwrap();
    let bin = home.join("pixel");
    fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
    let before_claude = fs::read(&claude_path).unwrap();
    let before_bin = fs::read(&bin).unwrap();

    // Dry-run uninstall
    let uninstall_opts = UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        binary_path: Some(bin.clone()),
        dry_run: true,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    };
    let report = uninstall(&uninstall_opts).expect("dry-run uninstall");
    assert!(report.dry_run, "report should be dry-run");

    // Nothing should have changed.
    assert!(
        bin.is_file(),
        "binary should still exist after dry-run uninstall"
    );
    assert_eq!(
        fs::read(&bin).unwrap(),
        before_bin,
        "binary must be byte-identical after dry-run uninstall"
    );
    let claude = fs::read_to_string(&claude_path).unwrap();
    assert!(
        claude.contains(MANAGED_BEGIN),
        "managed block should still exist after dry-run uninstall"
    );
    assert_eq!(
        fs::read(&claude_path).unwrap(),
        before_claude,
        "CLAUDE.md must be byte-identical after dry-run uninstall"
    );
}

/// Uninstall removes pixel run-hook entries from Codex hooks.json while
/// preserving non-pixel entries.
#[test]
fn uninstall_removes_codex_hooks_preserving_others() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Pre-create Codex hooks.json with a pixel entry AND a non-pixel entry.
    let codex_path = home.join(".codex").join("hooks.json");
    fs::create_dir_all(codex_path.parent().unwrap()).unwrap();
    let initial = serde_json::json!({
        "hooks": {
            "PreToolUse": [
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "~/.claude/hooks/pixel-targets-guard" }] },
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "~/.claude/hooks/other-tool" }] }
            ],
            "PostToolUse": [
                { "hooks": [{ "type": "command", "command": "/opt/pixel run-hook metrics --provider codex" }] },
                { "hooks": [{ "type": "command", "command": "~/.cmux/hooks/cmux-feed" }] }
            ]
        }
    });
    fs::write(&codex_path, serde_json::to_string_pretty(&initial).unwrap()).unwrap();

    let opts = UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    };
    uninstall(&opts).expect("uninstall");

    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&codex_path).unwrap()).unwrap();
    let pre = after["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 1, "only the non-pixel entry should remain");
    assert_eq!(
        pre[0]["hooks"][0]["command"].as_str().unwrap(),
        "~/.claude/hooks/other-tool",
        "the other-tool entry should be preserved"
    );
    let post = after["hooks"]["PostToolUse"].as_array().unwrap();
    assert_eq!(post.len(), 1, "only the non-pixel relay should remain");
    assert_eq!(
        post[0]["hooks"][0]["command"].as_str().unwrap(),
        "~/.cmux/hooks/cmux-feed",
        "the foreign PostToolUse entry should be preserved"
    );
}

/// Uninstall removes the rule source file.
#[test]
fn uninstall_removes_rule_source() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Create the rule source file.
    let rules_dir = home.join(".agent-config").join("rules");
    fs::create_dir_all(&rules_dir).unwrap();
    let rule_file = rules_dir.join("pixel.md");
    fs::write(&rule_file, "# pixel rules\n").unwrap();

    let opts = UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    };
    uninstall(&opts).expect("uninstall");

    assert!(!rule_file.is_file(), "rule source file should be deleted");
}

#[test]
fn routing_full_install_rtk_round_trip_preserves_foreign_hooks() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let settings = home.join(".claude/settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let rtk = serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]});
    let foreign = serde_json::json!({"matcher":"startup","hooks":[{"type":"command","command":"keep-session-check"}]});
    let original = serde_json::json!({"hooks":{"PreToolUse":[rtk.clone()],"SessionStart":[foreign.clone()]},"unrelated":true});
    fs::write(&settings, serde_json::to_vec(&original).unwrap()).unwrap();
    let exe = fake_pixel_exe(home);
    let opts = InstallOptions {
        repo: None,
        home: Some(home.into()),
        executable_path: Some(exe.clone()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    };
    // The global install wires lifecycle and task hooks, no retrieval guard, and
    // foreign entries (RTK + SessionStart) pass through untouched.
    install(&opts).unwrap();
    let once = fs::read(&settings).unwrap();
    install(&opts).unwrap();
    assert_eq!(
        fs::read(&settings).unwrap(),
        once,
        "repeat install must leave the merged settings stable"
    );
    let installed: serde_json::Value = serde_json::from_slice(&once).unwrap();
    // No pixel delegate is added; the RTK entry survives verbatim.
    assert_eq!(
        without_task_hooks(&installed, "claude", &exe)["hooks"]["PreToolUse"],
        serde_json::json!([rtk]),
        "the foreign RTK entry must survive verbatim beside the task gate"
    );
    // The foreign SessionStart group is kept, pixel lifecycle entries added.
    let session = installed["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(session[0], foreign, "foreign SessionStart group preserved");
    assert!(
        session.iter().skip(1).any(|g| {
            g["hooks"].as_array().is_some_and(|h| {
                h.iter().any(|hook| {
                    hook["command"].as_str().is_some_and(|c| {
                        c.contains("task-event --provider claude --event session-start")
                    })
                })
            })
        }),
        "pixel session-start lifecycle hook registered: {installed}"
    );
    assert!(
        installed["hooks"]["UserPromptSubmit"]
            .as_array()
            .is_some_and(|g| !g.is_empty()),
        "pixel prompt-submit lifecycle hook registered: {installed}"
    );
    assert!(
        !installed
            .to_string()
            .contains(pixel_install::config::GUARD_HOOK),
        "install must not wire any pixel guard"
    );
    uninstall(&UninstallOptions {
        repo: None,
        home: Some(home.into()),
        binary_path: Some(exe),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();
    let restored: serde_json::Value = serde_json::from_slice(&fs::read(settings).unwrap()).unwrap();
    assert_eq!(restored, original);
}

#[test]
#[cfg(unix)]
fn routing_providers_install_and_execute_without_ambient_claude() {
    for provider in ["claude", "codex", "devin"] {
        let home = TempDir::new().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "routing_isolated_provider_child", "--nocapture"])
            .env("PIXEL_INSTALL_TEST_CHILD", provider)
            .env("HOME", home.path())
            .env("PATH", "")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{provider}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
#[cfg(unix)]
fn routing_isolated_provider_child() {
    use std::os::unix::fs::PermissionsExt;
    let Ok(provider) = std::env::var("PIXEL_INSTALL_TEST_CHILD") else {
        return;
    };
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let config = home.join(match provider.as_str() {
        "claude" => ".claude/settings.json",
        "codex" => ".codex/hooks.json",
        "devin" => ".config/devin/config.json",
        _ => panic!("unexpected provider"),
    });
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let bin_dir = home.join("Pixel hook tools' directory");
    fs::create_dir_all(&bin_dir).unwrap();
    let exe = bin_dir.join("pixel");
    fs::write(&exe, "#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s' \"$*\"\n").unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    if provider == "devin" {
        // The lifecycle hook an earlier release wrote, under the quoted path.
        let quoted = format!("'{}'", exe.display().to_string().replace('\'', "'\\''"));
        fs::write(
            &config,
            serde_json::json!({"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":format!("{quoted} run-hook session-start --provider devin")}]}]}})
                .to_string(),
        )
        .unwrap();
    } else {
        fs::write(&config, "{}").unwrap();
    }
    let opts = InstallOptions {
        repo: None,
        home: Some(home.clone()),
        executable_path: Some(exe.clone()),
        claude_executable: Some(fake_claude_exe(&home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    };
    // Claude and Codex get only synchronous task-event hooks. Devin gets
    // none. No automatic retrieval callbacks or hook scripts are deployed
    // globally.
    install(&opts).unwrap();
    let first = fs::read(&config).unwrap();
    install(&opts).unwrap();
    assert_eq!(
        fs::read(&config).unwrap(),
        first,
        "repeat install must leave the provider config stable"
    );
    let installed: serde_json::Value = serde_json::from_slice(&first).unwrap();
    let value = if provider == "devin" {
        installed.clone()
    } else {
        without_task_hooks(&installed, &provider, &exe)
    };
    match provider.as_str() {
        "codex" => {
            assert_eq!(value["hooks"], serde_json::json!({}), "{installed}");
            let command = pixel_commands(&installed, "PreToolUse")[0].clone();
            assert!(command.contains("task-event --provider codex"), "{command}");
            // The executable path holds a space and a quote: it must arrive
            // shell-quoted so the hook actually launches, with the embedded
            // apostrophe emitted as the '\'' escape sequence.
            assert!(
                command.starts_with('\'')
                    && command.contains("directory/pixel' run-hook")
                    && command.contains("'\\''"),
                "the executable path must survive spaces and quotes: {command}"
            );
        }
        "claude" => {
            assert_eq!(value["hooks"], serde_json::json!({}), "{installed}");
            let command = pixel_commands(&installed, "PreToolUse")[0].clone();
            assert!(
                command.contains("task-event --provider claude"),
                "{command}"
            );
            assert!(
                command.starts_with('\'')
                    && command.contains("directory/pixel' run-hook")
                    && command.contains("'\\''"),
                "the executable path must survive spaces and quotes: {command}"
            );
        }
        "devin" => {
            // Devin keeps its native retrieval: the lifecycle hook an earlier
            // release registered under this quoted executable path is
            // recognised and removed, and nothing replaces it.
            assert_eq!(installed, serde_json::json!({}), "{installed}");
        }
        _ => unreachable!("unexpected provider {provider}"),
    }
    // No provider gets a ~/.claude/hooks directory from the install.
    assert!(
        !home.join(".claude/hooks").exists(),
        "install must not create ~/.claude/hooks for any provider"
    );
    // No provider gets a prompt file either.
    assert_no_retired_prompt(&home);
}

// ---------------------------------------------------------------------------
// fish support
//
// fish reads neither ~/.zshrc nor ~/.bashrc, and rejects POSIX function
// syntax outright (`claude() { ...; }` is a parse error, `$@` does not exist).
// Before fish was handled, a fish user's `pixel install` wrote a POSIX block
// into ~/.zshrc: wrappers that never loaded, and a doctor that called them
// green. These tests pin both halves — the right file, and syntax the target
// shell actually accepts.
// ---------------------------------------------------------------------------

const FISH_SHELL: &str = "/opt/homebrew/bin/fish";

fn fish_dropin(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".config/fish/conf.d/pixel.fish")
}

/// A legacy `claude()` wrapper block the way the retired install wrote it —
/// fixtures hand-place it to exercise the removal path.
fn legacy_posix_block() -> String {
    format!(
        "{PIXEL_MANAGED_BEGIN}\n\
         claude() {{ command claude --append-system-prompt-file \"$HOME/.local/share/pixel/agent-prompt.md\" \"$@\"; }}\n\
         # <<< pixel-managed <<<\n"
    )
}

fn legacy_fish_block() -> String {
    format!(
        "{PIXEL_MANAGED_BEGIN}\n\
         function claude; command claude --append-system-prompt-file \"$HOME/.local/share/pixel/agent-prompt.md\" $argv; end\n\
         # <<< pixel-managed <<<\n"
    )
}

fn install_for_shell(home: &std::path::Path, shell: &str) {
    install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(shell.into()),
    })
    .expect("install");
}

#[test]
fn install_removes_a_legacy_fish_dropin_and_writes_no_profile() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    // A legacy install's fish drop-in: pixel owns the whole file, so once
    // the block is stripped the empty leftover is deleted outright.
    let dropin = fish_dropin(home);
    fs::create_dir_all(dropin.parent().unwrap()).unwrap();
    fs::write(&dropin, legacy_fish_block()).unwrap();

    install_for_shell(home, FISH_SHELL);

    assert!(
        !dropin.exists(),
        "the owned drop-in must be deleted once its block is stripped"
    );
    assert!(
        !home.join(".zshrc").exists() && !home.join(".bashrc").exists(),
        "a fish install must not write into a profile fish never sources"
    );
    // The doctrine still arrives — through the lifecycle hooks.
    assert!(
        home.join(".claude/settings.json").is_file(),
        "claude lifecycle hooks must be configured instead"
    );
}

#[test]
fn doctor_reports_a_clean_home_and_a_stale_wrapper_block() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, FISH_SHELL);

    let legacy_check = |shell: &str| {
        let report = doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            executable_path: None,
            shell: Some(shell.into()),
            claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
            ..Default::default()
        })
        .expect("doctor runs");
        report
            .checks
            .iter()
            .find(|c| c.id == "install.legacy-wrappers")
            .expect("legacy-wrappers check")
            .clone()
    };

    assert_eq!(
        legacy_check(FISH_SHELL).status,
        pixel_install::doctor::CheckStatus::Green,
        "a fresh install has no stale wrapper"
    );
    assert_eq!(
        legacy_check("/bin/zsh").status,
        pixel_install::doctor::CheckStatus::Green,
        "no stale block exists for zsh either"
    );

    // A block an older install left in a profile is red — the wrapper
    // double-injects next to the SessionStart hook.
    fs::write(home.join(".zshrc"), legacy_posix_block()).unwrap();
    let stale = legacy_check(FISH_SHELL);
    assert_eq!(stale.status, pixel_install::doctor::CheckStatus::Red);
    let zshrc = home.join(".zshrc").display().to_string();
    assert!(
        stale.reason.as_deref().unwrap_or_default().contains(&zshrc),
        "the stale profile must be named: {stale:?}"
    );

    // `pixel install` is the named fix: it strips the stray block.
    install_for_shell(home, FISH_SHELL);
    assert_eq!(
        legacy_check(FISH_SHELL).status,
        pixel_install::doctor::CheckStatus::Green,
        "install removes the stale block"
    );
    assert!(
        !fs::read_to_string(home.join(".zshrc"))
            .unwrap()
            .contains("pixel-managed"),
        "the stray zsh block is gone"
    );
}

/// `pixel uninstall --wrappers-only` removes only the named shell's block —
/// the surgical fix for a block in a profile the login shell never loads.
#[test]
fn uninstall_wrappers_only_removes_one_shells_block_and_nothing_else() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, FISH_SHELL);
    // Prompt files an earlier release left: only a full uninstall or
    // install removes them, never the wrapper-only cleanup.
    write_retired_prompts(home);
    let prompt = home.join(RETIRED_PROMPTS[0]);
    // Two legacy blocks, one per shell, as an old install left them. The
    // .zshrc carries user content around its block so the profile survives
    // once the block is stripped.
    fs::write(
        home.join(".zshrc"),
        format!("export EDITOR=vim\n{}", legacy_posix_block()),
    )
    .unwrap();
    let dropin = fish_dropin(home);
    fs::create_dir_all(dropin.parent().unwrap()).unwrap();
    fs::write(&dropin, legacy_fish_block()).unwrap();

    let report = uninstall(&UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        shell: Some("/bin/zsh".into()),
        wrappers_only: true,
        ..Default::default()
    })
    .expect("uninstall runs");
    assert!(report.ok, "{report:?}");
    assert_eq!(
        report.steps.len(),
        1,
        "only the wrapper step ran: {report:?}"
    );
    assert_eq!(report.steps[0].id, "shell-wrappers");
    assert_eq!(
        (
            report.summary.green,
            report.summary.yellow,
            report.summary.red
        ),
        (1, 0, 0),
        "{report:?}"
    );

    assert!(
        !fs::read_to_string(home.join(".zshrc"))
            .unwrap()
            .contains("pixel-managed"),
        "the zsh block is gone"
    );
    assert!(
        fs::read_to_string(&dropin)
            .unwrap()
            .contains("pixel-managed"),
        "the fish block stays"
    );
    assert!(prompt.is_file(), "the prompt files stay");
    assert!(home.join(RETIRED_PROMPTS[1]).is_file());

    // Doctor still flags the remaining fish block — it is stale too.
    let check = doctor(&DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        shell: Some(FISH_SHELL.into()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        ..Default::default()
    })
    .expect("doctor runs")
    .checks
    .into_iter()
    .find(|c| c.id == "install.legacy-wrappers")
    .unwrap();
    assert_eq!(
        check.status,
        pixel_install::doctor::CheckStatus::Red,
        "the surviving fish block is stale: {check:?}"
    );
}

/// A begin marker with no end marker used to mean "everything to EOF is
/// ours": the rest of the user's profile was deleted and the truncated file
/// rewritten. Both install and uninstall now refuse it and write nothing.
#[test]
fn an_unterminated_managed_block_refuses_the_rewrite_and_keeps_the_profile() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let profile = shell_profile_path(home);
    let original = "# user aliases\n\
                    alias gs='git status'\n\
                    # >>> pixel-managed >>>\n\
                    # a block nobody closed\n\
                    alias last='kept'\n";
    fs::write(&profile, original).unwrap();

    let report = install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("install reports the refusal, it does not fail the run");
    let step = wrappers_step(&report);
    assert_eq!(
        step.status,
        pixel_install::install::CheckStatus::Red,
        "{step:?}"
    );
    assert!(
        step.summary.contains("pixel-managed"),
        "the step must name what is wrong: {step:?}"
    );
    assert_eq!(
        fs::read_to_string(&profile).unwrap(),
        original,
        "the lines after the unterminated marker are the user's"
    );

    let report = uninstall(&UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .expect("uninstall reports the refusal");
    let step = report
        .steps
        .iter()
        .find(|s| s.id == "shell-wrappers")
        .expect("shell-wrappers step");
    assert_eq!(
        step.status,
        pixel_install::install::CheckStatus::Red,
        "{step:?}"
    );
    assert_eq!(
        fs::read_to_string(&profile).unwrap(),
        original,
        "uninstall leaves the broken profile alone too"
    );
}

/// Removing the legacy block rewrites the profile, so the bytes it replaces
/// are backed up first, and a re-install that finds nothing left to remove
/// adds no second backup.
#[test]
fn removing_a_legacy_wrapper_backs_up_the_profile_first() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let profile = shell_profile_path(home);
    let original = format!(
        "# user aliases\nexport EDITOR=vim\n{}",
        legacy_posix_block()
    );
    fs::write(&profile, &original).unwrap();

    install_for_shell(home, TEST_SHELL);

    let profile_backups = |home: &std::path::Path| -> Vec<std::path::PathBuf> {
        let mut paths: Vec<std::path::PathBuf> = fs::read_dir(home)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(".zshrc.pixel-bak."))
            })
            .collect();
        paths.sort();
        paths
    };
    let backups = profile_backups(home);
    assert_eq!(backups.len(), 1, "one backup of the profile: {backups:?}");
    assert_eq!(
        fs::read_to_string(&backups[0]).unwrap(),
        original,
        "the backup holds the bytes install replaced"
    );
    let cleaned = fs::read_to_string(&profile).unwrap();
    assert!(cleaned.contains("export EDITOR=vim"), "{cleaned}");
    assert!(!cleaned.contains(PIXEL_MANAGED_BEGIN), "{cleaned}");

    install_for_shell(home, TEST_SHELL);
    assert_eq!(
        profile_backups(home).len(),
        1,
        "a re-install with nothing to remove must not back up the profile again"
    );
    assert_eq!(
        fs::read_to_string(&profile).unwrap(),
        cleaned,
        "a re-install with nothing to remove must leave the profile byte-identical"
    );
}

#[test]
fn doctor_flags_a_posix_block_sitting_in_the_fish_dropin_as_stale() {
    // The markers in the file fish sources mean a legacy install ran here —
    // fish cannot parse a line of the POSIX block, and next to the
    // SessionStart hook it would double-inject. Doctor must flag it red.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, FISH_SHELL);
    fs::create_dir_all(fish_dropin(home).parent().unwrap()).unwrap();
    fs::write(fish_dropin(home), legacy_posix_block()).unwrap();

    let report = doctor(&DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        shell: Some(FISH_SHELL.into()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        ..Default::default()
    })
    .expect("doctor runs");
    let check = report
        .checks
        .iter()
        .find(|c| c.id == "install.legacy-wrappers")
        .expect("legacy-wrappers check");
    assert_eq!(
        check.status,
        pixel_install::doctor::CheckStatus::Red,
        "a POSIX block in the fish drop-in is stale, got {:?}",
        check.reason
    );
}

#[test]
fn uninstall_deletes_a_fish_dropin_it_emptied() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    // A drop-in that is only the pixel block — pixel owns the whole file.
    let dropin = fish_dropin(home);
    fs::create_dir_all(dropin.parent().unwrap()).unwrap();
    fs::write(&dropin, legacy_fish_block()).unwrap();

    uninstall(&UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        binary_path: None,
        dry_run: false,
        shell: Some(FISH_SHELL.into()),
        ..Default::default()
    })
    .expect("uninstall");

    assert!(
        !dropin.exists(),
        "pixel owned the whole drop-in — an empty leftover file is litter"
    );
}

// ---------------------------------------------------------------------------
// retired prompt files: `agent-prompt.md` and `subagent-prompt.md`
//
// Earlier releases deployed both under `~/.local/share/pixel/` for the
// retired `claude` wrapper and the explicit integrations. No host reads them
// any more: `pixel install` removes them, a dry run only announces it, and
// nothing else Pixel keeps in that directory is touched.
// ---------------------------------------------------------------------------

fn subagent_prompt_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".local/share/pixel/subagent-prompt.md")
}

#[test]
fn install_removes_a_subagent_prompt_an_earlier_release_deployed() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let data = home.join(".local/share/pixel");
    fs::create_dir_all(&data).unwrap();
    fs::write(subagent_prompt_path(home), "pixel who-calls X --callers\n").unwrap();
    // A file of the user's beside it is not a prompt Pixel deployed.
    fs::write(data.join("notes.md"), "mine\n").unwrap();

    let report = install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("install");
    let step = report
        .steps
        .iter()
        .find(|s| s.id == "agent-prompt")
        .expect("agent-prompt step");
    assert_eq!(step.status, StepStatus::Green);
    assert_eq!(step.summary, "removed subagent-prompt.md");
    assert_no_retired_prompt(home);
    assert_eq!(fs::read_to_string(data.join("notes.md")).unwrap(), "mine\n");

    // Nothing left: the next install writes no prompt back.
    install_for_shell(home, TEST_SHELL);
    assert_no_retired_prompt(home);
}

#[test]
fn dry_run_does_not_remove_the_retired_prompts() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    write_retired_prompts(home);
    let report = install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: true,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("dry-run install");
    assert!(report.dry_run);
    for rel in RETIRED_PROMPTS {
        assert_eq!(
            fs::read_to_string(home.join(rel)).unwrap(),
            "# Pixel prompt from an earlier release\n",
            "dry-run must not remove {rel}"
        );
    }
    let step = report
        .steps
        .iter()
        .find(|s| s.id == "agent-prompt")
        .expect("agent-prompt step");
    assert_eq!(
        step.summary, "[dry-run] would report: removed agent-prompt.md and subagent-prompt.md",
        "the dry-run report announces the removal it would make"
    );
}

fn codex_config_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".codex/config.toml")
}

/// The `developer_instructions` string of `~/.codex/config.toml`, read the way
/// Codex reads it (a TOML parse, not a substring search), or `None` when the
/// file or the key is absent.
fn codex_developer_instructions(home: &std::path::Path) -> Option<String> {
    let text = fs::read_to_string(codex_config_path(home)).ok()?;
    let doc: toml_edit::DocumentMut = text.parse().expect("config.toml must stay valid TOML");
    doc.get("developer_instructions")
        .and_then(|item| item.as_str())
        .map(str::to_string)
}

const PIXEL_BLOCK_BEGIN: &str = "<!-- pixel:managed:begin -->";
const PIXEL_BLOCK_END: &str = "<!-- pixel:managed:end -->";

/// A config.toml the way the Codex desktop app leaves it: comments, root
/// keys, sub-tables with dotted keys. Every line of it must survive an
/// install byte for byte — the app rewrites this file too, and a
/// regenerated layout would fight it.
const USER_CODEX_CONFIG: &str = r#"# my codex settings
personality = "pragmatic"
model = "gpt-5.6-sol"  # trailing comment

[mcp_servers.node_repl]
args = []
command = "/opt/node_repl"

[features]
js_repl = false
token_budget.enabled = true
"#;

/// Global installation removes the retired block but preserves user-owned
/// Codex settings without creating a replacement prompt.
#[test]
fn install_does_not_add_codex_developer_instructions_or_rewrite_other_settings() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(codex_config_path(home), USER_CODEX_CONFIG).unwrap();
    install_for_shell(home, TEST_SHELL);

    assert_eq!(codex_developer_instructions(home), None);
    let written = fs::read_to_string(codex_config_path(home)).unwrap();
    assert_eq!(written, USER_CODEX_CONFIG);

    install_for_shell(home, TEST_SHELL);
    assert_eq!(
        fs::read_to_string(codex_config_path(home)).unwrap(),
        written,
        "a re-install must be byte-for-byte idempotent"
    );
}

#[test]
fn install_removes_a_retired_codex_block_and_preserves_surrounding_user_text() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(
        codex_config_path(home),
        format!(
            "developer_instructions = {}\n",
            toml_edit::Value::from(format!(
                "Mine first.\n\n{PIXEL_BLOCK_BEGIN}\nold prompt\n{PIXEL_BLOCK_END}\nMine last.\n"
            ))
        ),
    )
    .unwrap();
    install_for_shell(home, TEST_SHELL);
    assert_eq!(
        codex_developer_instructions(home).as_deref(),
        Some("Mine first.\n\nMine last.\n"),
        "only the retired block is removed"
    );
}

/// OpenCode keeps its native tools: with OpenCode present, `pixel install`
/// writes no prompt block and no plugin, and removes the ones an earlier
/// release wrote (the managed block in its global `AGENTS.md`, the managed
/// `plugins/pixel.js`, stale `opencode.json` entries) while user text, a
/// user plugin and user settings survive. Doctor is red before, green after.
#[test]
fn install_removes_the_opencode_prompt_and_plugin_an_earlier_release_wrote() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let opencode = home.join(".config/opencode");
    let agents_md = opencode.join("AGENTS.md");
    let config = opencode.join("opencode.json");
    let plugin = opencode.join("plugins/pixel.js");
    let mine = opencode.join("plugins/mine.js");

    // No ~/.config/opencode: the step is skipped and no files appear.
    install_for_shell(home, TEST_SHELL);
    assert!(
        !opencode.exists(),
        "install must not create OpenCode config for a user without OpenCode"
    );

    // Already native: OpenCode present without anything of Pixel's gains no
    // block, no plugin and no config.
    fs::create_dir_all(&opencode).unwrap();
    fs::write(&agents_md, "user rules stay\n").unwrap();
    install_for_shell(home, TEST_SHELL);
    assert_eq!(fs::read_to_string(&agents_md).unwrap(), "user rules stay\n");
    assert!(!opencode.join("plugins").exists() && !config.exists());

    // What an earlier release left.
    fs::write(
        &agents_md,
        format!("user rules stay\n{PIXEL_BLOCK_BEGIN}\nold prompt\n{PIXEL_BLOCK_END}\n"),
    )
    .unwrap();
    fs::create_dir_all(plugin.parent().unwrap()).unwrap();
    fs::write(
        &plugin,
        format!("// {PIXEL_BLOCK_BEGIN}\nexport default {{}};\n"),
    )
    .unwrap();
    fs::write(&mine, "export default {};\n").unwrap();
    fs::write(
        &config,
        serde_json::to_string_pretty(&serde_json::json!({
            "model": "anthropic/claude-sonnet-4-5",
            "instructions": ["/old/home/.local/share/pixel/agent-prompt.md"],
            "plugin": ["~/nowhere/pixel.mjs", "./plugins/caveman/plugin.js"]
        }))
        .unwrap(),
    )
    .unwrap();
    let opencode_check = || {
        let report = doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            shell: Some(TEST_SHELL.into()),
            only: vec!["install.opencode-agents-md".into()],
            ..Default::default()
        })
        .unwrap();
        check(&report, "install.opencode-agents-md").clone()
    };
    let red = opencode_check();
    assert_eq!(red.status, CheckStatus::Red, "{red:?}");
    assert_eq!(
        red.reason.as_deref(),
        Some(
            format!(
                "retired Pixel OpenCode integration remains: the Pixel block in {} and the guard plugin {} — run `pixel install` to remove it",
                agents_md.display(),
                plugin.display()
            )
            .as_str()
        )
    );

    install_for_shell(home, TEST_SHELL);
    assert_eq!(fs::read_to_string(&agents_md).unwrap(), "user rules stay\n");
    assert!(!plugin.exists(), "the managed plugin is removed");
    assert_eq!(fs::read_to_string(&mine).unwrap(), "export default {};\n");
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "model": "anthropic/claude-sonnet-4-5",
            "plugin": ["./plugins/caveman/plugin.js"],
        }),
        "the stale instructions entry and the missing-file pixel.mjs entry go"
    );
    assert_eq!(opencode_check().status, CheckStatus::Green);

    let written = fs::read(&agents_md).unwrap();
    install_for_shell(home, TEST_SHELL);
    assert_eq!(
        fs::read(&agents_md).unwrap(),
        written,
        "a re-install must be byte-for-byte idempotent"
    );
}

#[test]
fn install_refuses_to_rewrite_a_codex_config_it_cannot_parse() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".codex")).unwrap();
    let broken = "model = \"gpt\"\n[features\njs_repl = false\n";
    fs::write(codex_config_path(home), broken).unwrap();
    let report = install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .unwrap();
    let step = report
        .steps
        .iter()
        .find(|s| s.id == "codex-config")
        .expect("codex-config step");
    assert_eq!(
        step.status,
        pixel_install::install::CheckStatus::Red,
        "a file codex itself cannot load is reported, not repaired: {step:?}"
    );
    assert_eq!(
        fs::read_to_string(codex_config_path(home)).unwrap(),
        broken,
        "an unparseable config.toml must not be rewritten — that would drop what it holds"
    );
    assert!(
        !report.ok,
        "the report must not read ok with the codex step red"
    );
}

#[test]
fn dry_run_leaves_codex_config_absent_and_untouched() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let options = InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: true,
        shell: Some(TEST_SHELL.into()),
    };
    let report = install(&options).unwrap();
    assert!(report.ok);
    assert!(
        !home.join(".codex").exists(),
        "dry-run must not create ~/.codex"
    );
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(codex_config_path(home), USER_CODEX_CONFIG).unwrap();
    let report = install(&options).unwrap();
    let step = report
        .steps
        .iter()
        .find(|s| s.id == "codex-config")
        .unwrap();
    assert!(
        step.summary.starts_with("[dry-run]"),
        "dry-run must say what it would do: {step:?}"
    );
    assert_eq!(
        fs::read_to_string(codex_config_path(home)).unwrap(),
        USER_CODEX_CONFIG
    );
}

#[test]
fn doctor_codex_config_check_is_green_without_pixel_and_red_for_a_retired_block() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let status = || {
        doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            executable_path: None,
            shell: Some(TEST_SHELL.into()),
            claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
            ..Default::default()
        })
        .unwrap()
        .checks
        .into_iter()
        .find(|c| c.id == "install.codex-config")
        .expect("codex-config check")
    };
    assert_eq!(status().status, CheckStatus::Green, "nothing installed");

    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(
        codex_config_path(home),
        "developer_instructions = \"Always answer in French.\"\n",
    )
    .unwrap();
    let check = status();
    assert_eq!(check.status, CheckStatus::Green, "user text remains valid");

    let retired =
        format!("Mine first.\n\n{PIXEL_BLOCK_BEGIN}\nold prompt\n{PIXEL_BLOCK_END}\nMine last.\n");
    fs::write(
        codex_config_path(home),
        format!(
            "developer_instructions = {}\n",
            toml_edit::Value::from(retired)
        ),
    )
    .unwrap();
    let check = status();
    assert_eq!(
        check.status,
        CheckStatus::Red,
        "any retired Pixel block must be removed: {check:?}"
    );
    assert!(
        check
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("retired Pixel block remains")),
        "{check:?}"
    );

    for orphaned_marker in [
        format!("{PIXEL_BLOCK_BEGIN}\nold prompt\n"),
        format!("old prompt\n{PIXEL_BLOCK_END}\n"),
    ] {
        let original = format!(
            "developer_instructions = {}\n",
            toml_edit::Value::from(orphaned_marker)
        );
        fs::write(codex_config_path(home), &original).unwrap();

        let check = status();

        assert_eq!(
            check.status,
            CheckStatus::Red,
            "an orphaned retired marker must still be reported: {check:?}"
        );
        assert!(
            check
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("retired Pixel block remains")),
            "{check:?}"
        );
        assert_eq!(
            fs::read_to_string(codex_config_path(home)).unwrap(),
            original,
            "doctor must not rewrite malformed user configuration"
        );
    }
}

#[test]
fn uninstall_takes_only_the_pixel_block_out_of_codex_config() {
    use pixel_install::uninstall::{UninstallOptions, uninstall};

    // A retired block is removed with its key; unrelated settings in the
    // same config.toml survive byte-for-byte.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(
        codex_config_path(home),
        format!(
            "developer_instructions = {}\n{USER_CODEX_CONFIG}",
            toml_edit::Value::from(format!(
                "{PIXEL_BLOCK_BEGIN}\nold prompt\n{PIXEL_BLOCK_END}\n"
            ))
        ),
    )
    .unwrap();
    uninstall(&UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        codex_developer_instructions(home),
        None,
        "the key must be removed"
    );
    let after = fs::read_to_string(codex_config_path(home)).unwrap();
    for line in USER_CODEX_CONFIG.lines() {
        assert!(
            after.contains(line),
            "user line {line:?} lost by uninstall:\n{after}"
        );
    }

    // User text surrounding a retired block remains after removal.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(
        codex_config_path(home),
        format!(
            "developer_instructions = {}\n",
            toml_edit::Value::from(format!(
                "Always answer in French.\n\n{PIXEL_BLOCK_BEGIN}\nold prompt\n{PIXEL_BLOCK_END}\n"
            ))
        ),
    )
    .unwrap();
    uninstall(&UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        codex_developer_instructions(home).as_deref(),
        Some("Always answer in French.\n"),
        "uninstall must hand the key back to the user"
    );
}

#[test]
fn reinstall_never_recreates_a_removed_wrapper_block() {
    for shell in [TEST_SHELL, FISH_SHELL] {
        let dir = TempDir::new().expect("tempdir");
        let home = dir.path();
        let profile = match shell {
            FISH_SHELL => fish_dropin(home),
            _ => shell_profile_path(home),
        };
        fs::create_dir_all(profile.parent().unwrap()).unwrap();
        let block = match shell {
            FISH_SHELL => legacy_fish_block(),
            _ => legacy_posix_block(),
        };
        fs::write(&profile, format!("export EDITOR=vim\n{block}")).unwrap();

        install_for_shell(home, shell);
        install_for_shell(home, shell);
        install_for_shell(home, shell);
        let content = fs::read_to_string(&profile).expect("profile survives");
        assert_eq!(
            content.matches(PIXEL_MANAGED_BEGIN).count(),
            0,
            "{shell}: re-installs must not recreate the wrapper block:\n{content}"
        );
        assert!(
            content.contains("export EDITOR=vim"),
            "{shell}: the user's own lines survive:\n{content}"
        );
    }
}

/// One retired prompt file is enough for `install.agent-prompt` to go red,
/// naming only that file; the install that removes it turns the check green.
#[test]
fn doctor_flags_a_retired_subagent_prompt_until_install_removes_it() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    let check = |home: &std::path::Path| {
        doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            shell: Some(TEST_SHELL.into()),
            claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
            only: vec!["install.agent-prompt".into()],
            ..Default::default()
        })
        .expect("doctor")
        .checks
        .into_iter()
        .find(|c| c.id == "install.agent-prompt")
        .expect("install.agent-prompt check")
    };
    assert_eq!(
        check(home).status,
        pixel_install::doctor::CheckStatus::Green,
        "a fresh install leaves no prompt file"
    );
    fs::create_dir_all(subagent_prompt_path(home).parent().unwrap()).unwrap();
    fs::write(subagent_prompt_path(home), "pixel who-calls X --callers\n").unwrap();
    let red = check(home);
    assert_eq!(
        red.status,
        pixel_install::doctor::CheckStatus::Red,
        "{red:?}"
    );
    assert_eq!(
        red.reason.as_deref(),
        Some(
            format!(
                "retired Pixel prompt file(s) remain: {} — run `pixel install` to remove them",
                subagent_prompt_path(home).display()
            )
            .as_str()
        )
    );
    install_for_shell(home, TEST_SHELL);
    assert_eq!(
        check(home).status,
        pixel_install::doctor::CheckStatus::Green
    );
    assert!(!subagent_prompt_path(home).exists());
}

#[test]
fn uninstall_removes_the_retired_prompts() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    write_retired_prompts(home);

    uninstall(&UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        binary_path: None,
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .expect("uninstall");

    assert_no_retired_prompt(home);
}

// ---------------------------------------------------------------------------
// Pi's APPEND_SYSTEM.md is auto-loaded, so upgrades remove retired Pixel
// prompt text while preserving user instructions. Explicit impact is a Pi
// package under ~/.local/share/pixel/pi-package that Pi's settings declare.
// ---------------------------------------------------------------------------

fn pi_prompt_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".pi/agent/APPEND_SYSTEM.md")
}

fn pi_prompt_backups(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    fs::read_dir(home.join(".pi/agent"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("APPEND_SYSTEM.md.pixel-bak."))
        })
        .collect()
}

const PRE_MARKER_PI_PROMPT: &str = "# Pixel Retrieval Layer — Mandatory Agent Protocol\n\n## THE COMPLETE REPLACEMENT MAP\npixel search \"term\"\n## ENVIRONMENT\nAll commands accept `[PATH]` (default: current directory).\n";

fn uninstall_home(home: &std::path::Path) {
    uninstall(&UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .expect("uninstall");
}

/// Whether `pixel install` declared its Pi package in `home`'s Pi settings
/// and wrote the explicit command into it.
fn pi_impact_package_installed(home: &std::path::Path) -> bool {
    let package = home.join(".local/share/pixel/pi-package");
    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(home.join(PI_SETTINGS_FILE)).unwrap_or_default())
            .unwrap_or_default();
    settings["packages"].as_array().is_some_and(|packages| {
        packages.contains(&serde_json::json!(package.display().to_string()))
    }) && package.join("extensions/pixel-impact.ts").is_file()
}

#[test]
fn install_and_uninstall_strip_only_the_retired_pi_prompt() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let pi_path = pi_prompt_path(home);
    fs::create_dir_all(pi_path.parent().unwrap()).unwrap();
    let original =
        format!("# my own pi instructions\nAlways answer in French.\n{PRE_MARKER_PI_PROMPT}");
    fs::write(&pi_path, &original).unwrap();

    install_for_shell(home, TEST_SHELL);

    let migrated = "# my own pi instructions\nAlways answer in French.\n";
    assert_eq!(fs::read_to_string(&pi_path).unwrap(), migrated);
    assert!(
        pi_impact_package_installed(home),
        "the explicit impact command is installed as a Pi package"
    );

    let once = fs::read(&pi_path).unwrap();
    install_for_shell(home, TEST_SHELL);
    assert_eq!(
        fs::read(&pi_path).unwrap(),
        once,
        "a reinstall leaves the migrated user file byte-identical"
    );

    uninstall_home(home);
    let after = fs::read_to_string(&pi_path).expect("the user's file survives uninstall");
    assert_eq!(
        after, migrated,
        "uninstall removes Pixel files, not user text"
    );
    assert!(!after.contains(MANAGED_BEGIN), "{after}");
    assert!(!home.join(".pi/agent/extensions/pixel-impact.ts").exists());
}

#[test]
fn doctor_detects_a_pre_marker_prompt_above_the_managed_block_until_install_repairs_it() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    let pi_path = pi_prompt_path(home);
    fs::create_dir_all(pi_path.parent().unwrap()).unwrap();
    let stale = format!("My own note.\n{PRE_MARKER_PI_PROMPT}## My section\nKeep this.\n");
    fs::write(&pi_path, &stale).unwrap();
    let doctor_opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        shell: Some(TEST_SHELL.into()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        ..Default::default()
    };
    let pi_status = || check(&doctor(&doctor_opts).expect("doctor"), "install.pi-prompt").status;
    assert_eq!(
        pi_status(),
        CheckStatus::Red,
        "the retired command map is still active"
    );

    install_for_shell(home, TEST_SHELL);
    let repaired = fs::read_to_string(&pi_path).unwrap();
    assert_eq!(
        repaired, "My own note.\n## My section\nKeep this.\n",
        "install must remove only the recognized historical prompt"
    );
    assert_eq!(pi_status(), CheckStatus::Green);
    assert_eq!(
        fs::read_to_string(pi_prompt_backups(home).last().unwrap()).unwrap(),
        stale
    );
}

#[test]
fn uninstall_reclaims_pre_marker_prompt_copies_without_erasing_user_text() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    let pi_path = pi_prompt_path(home);
    fs::create_dir_all(pi_path.parent().unwrap()).unwrap();
    fs::write(
        &pi_path,
        format!("Before.\n{PRE_MARKER_PI_PROMPT}## My section\nKeep this.\nAfter.\n"),
    )
    .unwrap();

    uninstall_home(home);

    assert_eq!(
        fs::read_to_string(&pi_path).unwrap(),
        "Before.\n## My section\nKeep this.\nAfter.\n"
    );
}

#[test]
fn first_install_should_remove_duplicate_prompts_and_leave_doctor_green() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let pi_path = pi_prompt_path(home);
    fs::create_dir_all(pi_path.parent().unwrap()).unwrap();
    fs::write(
        &pi_path,
        format!("Before.\n{PRE_MARKER_PI_PROMPT}Between.\n{PRE_MARKER_PI_PROMPT}After.\n"),
    )
    .unwrap();

    install_for_shell(home, TEST_SHELL);
    let report = doctor(&DoctorOptions {
        home: Some(home.to_path_buf()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        check(&report, "install.pi-prompt").status,
        CheckStatus::Green
    );

    uninstall_home(home);
    assert_eq!(
        fs::read_to_string(pi_path).unwrap(),
        "Before.\nBetween.\nAfter.\n"
    );
}

#[test]
fn install_and_uninstall_preserve_a_retired_prompt_quoted_in_user_prose() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let pi_path = pi_prompt_path(home);
    fs::create_dir_all(pi_path.parent().unwrap()).unwrap();
    let existing = format!("```markdown\n{PRE_MARKER_PI_PROMPT}```\nAfter.\n");
    fs::write(&pi_path, &existing).unwrap();

    let report = doctor(&DoctorOptions {
        home: Some(home.to_path_buf()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        check(&report, "install.pi-prompt").status,
        CheckStatus::Green
    );
    install_for_shell(home, TEST_SHELL);
    assert_eq!(fs::read_to_string(&pi_path).unwrap(), existing);

    uninstall_home(home);
    assert_eq!(fs::read_to_string(pi_path).unwrap(), existing);
}

#[test]
fn doctor_and_install_should_remove_a_managed_pi_prompt_and_preserve_orphan_markers() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let pi_path = pi_prompt_path(home);
    let prefix = format!("{MANAGED_END}\nBefore.\n");
    let original = format!("{prefix}{MANAGED_BEGIN}\nstale\n{MANAGED_END}");
    fs::create_dir_all(pi_path.parent().unwrap()).unwrap();
    fs::write(&pi_path, &original).unwrap();
    assert_eq!(
        pixel_install::config::strip_managed_block(&original),
        prefix
    );

    let opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    };
    assert_eq!(
        check(&doctor(&opts).unwrap(), "install.pi-prompt").status,
        CheckStatus::Red
    );
    install_for_shell(home, TEST_SHELL);
    assert_eq!(fs::read_to_string(&pi_path).unwrap(), prefix);
    assert_eq!(
        check(&doctor(&opts).unwrap(), "install.pi-prompt").status,
        CheckStatus::Green
    );
    uninstall_home(home);
    assert_eq!(fs::read_to_string(&pi_path).unwrap(), prefix);
}

#[test]
fn install_removes_the_automatic_prompt_written_by_an_earlier_release() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    // What `pixel install` wrote before it treated the file as shared: the
    // prompt verbatim, no markers around it.
    let asset = PRE_MARKER_PI_PROMPT;
    let pi_path = pi_prompt_path(home);
    fs::create_dir_all(pi_path.parent().unwrap()).unwrap();
    fs::write(&pi_path, format!("Before.\n{asset}After.\n")).unwrap();

    install_for_shell(home, TEST_SHELL);

    let deployed = fs::read_to_string(&pi_path).expect("user text survives migration");
    assert_eq!(deployed, "Before.\nAfter.\n");
    assert!(!deployed.contains(MANAGED_BEGIN));
    assert!(pi_impact_package_installed(home));
}

#[test]
fn legacy_pi_prompt_migration_preserves_surrounding_user_sections() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    let asset = PRE_MARKER_PI_PROMPT;
    let pi_path = pi_prompt_path(home);
    fs::create_dir_all(pi_path.parent().unwrap()).unwrap();
    fs::write(
        &pi_path,
        format!("My Pi note.\n{asset}\n## My notes\nKeep this.\n"),
    )
    .expect("legacy prompt fixture");

    install_for_shell(home, TEST_SHELL);

    let deployed = fs::read_to_string(&pi_path).expect("migrated Pi prompt");
    assert!(deployed.starts_with("My Pi note.\n"), "{deployed}");
    assert!(deployed.contains("## My notes\nKeep this.\n"), "{deployed}");
    assert!(!deployed.contains(asset), "{deployed}");
    assert!(!deployed.contains(MANAGED_BEGIN), "{deployed}");
    assert!(!deployed.contains(MANAGED_END), "{deployed}");
    let report = doctor(&DoctorOptions {
        home: Some(home.to_path_buf()),
        shell: Some(TEST_SHELL.into()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        ..Default::default()
    })
    .expect("doctor");
    assert_eq!(
        check(&report, "install.pi-prompt").status,
        CheckStatus::Green
    );
}

#[test]
fn edited_legacy_pi_prompt_is_replaced_without_consuming_following_user_text() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    // A legacy Pi prompt: pre-#475 doctrine, edited by hand — the legacy
    // signature (heading, sections, PATH line) is what the migration strips.
    let edited = "# Pixel Retrieval Layer\n\
                  Pixel provides deterministic code retrieval, edited by hand.\n\
                  ## MANDATORY WORKFLOW\nDo the workflow.\n\
                  ## REPLACEMENT MAP\nMap.\n\
                  All commands accept `[PATH]`, default current directory.\n";
    let pi_path = pi_prompt_path(home);
    fs::create_dir_all(pi_path.parent().unwrap()).unwrap();
    fs::write(&pi_path, format!("Before.\n{edited}After.\n"))
        .expect("edited legacy prompt fixture");

    install_for_shell(home, TEST_SHELL);

    let deployed = fs::read_to_string(&pi_path).expect("migrated Pi prompt");
    assert!(deployed.starts_with("Before.\n"), "{deployed}");
    assert!(deployed.ends_with("After.\n"), "{deployed}");
    assert!(!deployed.contains("edited by hand"), "{deployed}");
    assert!(!deployed.contains("# Pixel Retrieval Layer"), "{deployed}");
    assert!(!deployed.contains(MANAGED_BEGIN), "{deployed}");
    assert!(!deployed.contains(MANAGED_END), "{deployed}");
    let report = doctor(&DoctorOptions {
        home: Some(home.to_path_buf()),
        shell: Some(TEST_SHELL.into()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        ..Default::default()
    })
    .expect("doctor");
    assert_eq!(
        check(&report, "install.pi-prompt").status,
        CheckStatus::Green
    );
}

#[test]
fn uninstall_removes_a_retired_pixel_only_pi_prompt_file() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let pi_path = pi_prompt_path(home);
    fs::create_dir_all(pi_path.parent().unwrap()).unwrap();
    fs::write(
        &pi_path,
        format!("{MANAGED_BEGIN}\n# pixel's own prompt\n{MANAGED_END}\n"),
    )
    .unwrap();

    uninstall_home(home);

    assert!(
        !pi_path.exists(),
        "a file that held only the retired Pixel prompt is Pixel's to delete"
    );
}

#[test]
fn uninstall_removes_the_classify_helpers_and_keeps_a_backup_of_them() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    // What the classify-helpers proposal writes: the skills, and the Pi
    // package declared beside a package of the user's.
    let skills = [
        home.join(".pi/agent/skills/pixel-classify/SKILL.md"),
        home.join(".claude/skills/pixel-classify/SKILL.md"),
        home.join(".codex/skills/pixel-classify/SKILL.md"),
    ];
    for file in &skills {
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, "shipped content\n").unwrap();
    }
    let settings = home.join(".pi/agent/settings.json");
    fs::write(&settings, r#"{"theme":"dark","packages":["npm:mine"]}"#).unwrap();
    pixel_install::ClassifyPiPackage::with_agent_dir(home, None, "export default () => {};\n")
        .expect("Pi is configured")
        .install()
        .unwrap();
    let package = home.join(pixel_install::CLASSIFY_PACKAGE_DIR);
    assert!(package.join("package.json").is_file());

    uninstall_home(home);

    for file in &skills {
        assert!(!file.exists(), "{file:?} must be removed");
    }
    assert!(
        !package.exists(),
        "the classify package is Pixel's to remove"
    );
    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(
        settings,
        serde_json::json!({"theme": "dark", "packages": ["npm:mine"]})
    );
    let backups = fs::read_dir(home.join(".claude"))
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".pixel-bak."))
        .count();
    assert_eq!(backups, 1, "a renamed .pixel-bak dir keeps the user copy");
    // Outside `skills/`: a backup there would still load as a skill.
    let left = fs::read_dir(home.join(".claude/skills"))
        .unwrap()
        .filter_map(std::result::Result::ok)
        .count();
    assert_eq!(left, 0, "nothing skill-shaped stays in .claude/skills");
}

#[test]
fn uninstall_survives_a_missing_pi_prompt_file() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let prompts = home.join(".local/share/pixel");
    fs::create_dir_all(&prompts).unwrap();
    fs::write(prompts.join("agent-prompt.md"), "deployed prompt\n").unwrap();

    let report = uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .expect("uninstall");

    assert!(
        !prompts.join("agent-prompt.md").exists(),
        "the prompt is removed even when the pi file was never deployed"
    );
    assert!(!pi_prompt_path(home).exists());
    let prompt_step = report
        .steps
        .iter()
        .find(|step| step.id == "agent-prompt")
        .unwrap();
    assert_eq!(
        prompt_step.summary, "removed agent-prompt.md",
        "an absent Pi file or subagent prompt must not be reported as removed"
    );
}

#[test]
fn install_skips_pi_when_the_configuration_path_is_not_a_directory() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    // A file where the directory should be: creating ~/.pi/agent fails
    // whatever the user's permissions are.
    let pi_path = home.join(".pi");
    let original = b"not a directory\n";
    fs::write(&pi_path, original).unwrap();

    let report = install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("a malformed optional Pi configuration must not fail other installs");

    let pi_step = report
        .steps
        .iter()
        .find(|step| step.id == "hooks.pi-impact")
        .expect("Pi status is reported");
    assert_eq!(pi_step.status, StepStatus::Green);
    assert!(pi_step.summary.contains("not installed"), "{pi_step:?}");
    assert_eq!(fs::read(&pi_path).unwrap(), original);
    assert!(!home.join(".local/share/pixel/pi-package").exists());
}

#[test]
fn doctor_distinguishes_retired_pi_prompt_from_explicit_impact_extension() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    let pi_path = pi_prompt_path(home);
    let doc_opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(home.join("pixel")),
        shell: Some(TEST_SHELL.into()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        ..Default::default()
    };
    let status = |home: &std::path::Path| {
        check(
            &doctor(&DoctorOptions {
                home: Some(home.to_path_buf()),
                ..doc_opts.clone()
            })
            .expect("doctor"),
            "install.pi-prompt",
        )
        .status
    };
    assert_eq!(
        status(home),
        CheckStatus::Green,
        "an absent automatic prompt is healthy"
    );
    let impact_check = |home: &std::path::Path| {
        check(
            &doctor(&DoctorOptions {
                home: Some(home.to_path_buf()),
                ..doc_opts.clone()
            })
            .expect("doctor"),
            "install.pi-impact",
        )
        .status
    };
    assert_eq!(impact_check(home), CheckStatus::Green);
    let extension = home.join(".local/share/pixel/pi-package/extensions/pixel-impact.ts");
    assert!(
        !home.join(".pi/agent").exists(),
        "an unconfigured Pi installation must not create its configuration directory"
    );

    // A configured Pi home receives the explicit command as a package its
    // settings declare, beside the user's own settings.
    let pi_settings = home.join(PI_SETTINGS_FILE);
    fs::create_dir_all(pi_settings.parent().unwrap()).unwrap();
    fs::write(&pi_settings, "{\"extensions\": []}\n").unwrap();
    install_for_shell(home, TEST_SHELL);
    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&pi_settings).unwrap()).unwrap();
    assert_eq!(
        settings,
        serde_json::json!({
            "extensions": [],
            "packages": [home.join(".local/share/pixel/pi-package").display().to_string()],
        })
    );
    assert!(pi_impact_package_installed(home));
    assert_eq!(impact_check(home), CheckStatus::Green);
    let source = fs::read_to_string(&extension).expect("explicit command extension installed");
    assert!(
        source.contains("pi.registerCommand(\"pixel-impact\""),
        "{source}"
    );
    assert!(source.contains("--no-refresh"), "{source}");
    assert!(
        !source.contains("registerTool("),
        "impact is explicit, not automatic"
    );

    // The user's automatic prompt is untouched and does not make the check stale.
    fs::create_dir_all(pi_path.parent().unwrap()).unwrap();
    fs::write(&pi_path, "my own instructions only\n").unwrap();
    assert_eq!(
        status(home),
        CheckStatus::Green,
        "user-owned Pi system instructions remain valid"
    );

    // A retired Pixel automatic prompt is red until the upgrade removes it.
    fs::write(
        &pi_path,
        format!("my own instructions\n{PRE_MARKER_PI_PROMPT}"),
    )
    .unwrap();
    assert_eq!(
        status(home),
        CheckStatus::Red,
        "retired automatic guidance must be removed"
    );
    install_for_shell(home, TEST_SHELL);
    assert_eq!(
        fs::read_to_string(&pi_path).unwrap(),
        "my own instructions\n"
    );
    assert_eq!(status(home), CheckStatus::Green);

    fs::write(
        &extension,
        format!("{source}\n// stale managed extension\n"),
    )
    .unwrap();
    assert_eq!(
        impact_check(home),
        CheckStatus::Red,
        "stale binary/source is caught"
    );
    install_for_shell(home, TEST_SHELL);
    assert_eq!(impact_check(home), CheckStatus::Green);
}

// ---------------------------------------------------------------------------
// Legacy wrapper removal
//
// The retired install wrote a `claude()` shell function. Every install and
// uninstall now strips that block through the "shell-wrappers" step so the
// wrapper cannot double-inject the prompt next to the SessionStart hook.
// ---------------------------------------------------------------------------

fn wrappers_step(report: &InstallReport) -> &pixel_install::install::InstallStep {
    report
        .steps
        .iter()
        .find(|s| s.id == "shell-wrappers")
        .expect("shell-wrappers step")
}

/// Plugin-manifest skill files come from the curated impact-skill asset;
/// other generated rule surfaces still come from `assets/pixel-agent-prompt.md`.
/// Keep both sets synchronized through the generator.
#[test]
fn plugin_assets_are_in_sync() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let script = repo.join("scripts").join("gen-plugin-assets.sh");
    assert!(script.is_file(), "missing {}", script.display());
    let out = std::process::Command::new("/bin/sh")
        .arg(&script)
        .arg("--check")
        .output()
        .expect("run gen-plugin-assets.sh --check");
    assert!(
        out.status.success(),
        "plugin assets stale — run scripts/gen-plugin-assets.sh\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn plugin_skill_is_focused_explicit_and_replaces_the_broad_skill() {
    let repo = repo_root();
    let read =
        |rel: &str| fs::read_to_string(repo.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
    let claude: serde_json::Value =
        serde_json::from_str(&read(".claude-plugin/plugin.json")).unwrap();
    let codex: serde_json::Value =
        serde_json::from_str(&read(".codex-plugin/plugin.json")).unwrap();
    assert_eq!(codex["skills"], "./skills/");
    assert_eq!(claude["skills"], "./claude-skills/");
    assert!(
        codex.get("hooks").is_none(),
        "Codex registers a retrieval hook"
    );
    assert!(
        claude.get("hooks").is_none(),
        "Claude registers a retrieval hook"
    );

    let codex_skill = read("skills/pixel-impact/SKILL.md");
    assert!(codex_skill.contains("name: pixel-impact"));
    assert!(codex_skill.contains("blast radius"));
    assert!(!codex_skill.contains("disable-model-invocation"));
    assert!(!codex_skill.contains("pixel build-index"));
    let claude_skill = read("claude-skills/pixel-impact/SKILL.md");
    assert!(claude_skill.contains("disable-model-invocation: true"));
    assert_eq!(
        claude_skill.replacen("disable-model-invocation: true\n", "", 1),
        codex_skill,
        "provider-specific copies must share one curated skill body"
    );
    let codex_policy = read("skills/pixel-impact/agents/openai.yaml");
    assert!(codex_policy.contains("allow_implicit_invocation: false"));

    let openclaw = read(".openclaw/skills/pixel/SKILL.md");
    assert!(openclaw.contains("name: pixel\n"));
    assert!(openclaw.ends_with(include_str!("../assets/pixel-agent-prompt.md")));
    for retired in [
        "skills/pixel/SKILL.md",
        ".agents/skills/pixel/SKILL.md",
        ".agents/skills/pixel-impact/SKILL.md",
    ] {
        assert!(
            !repo.join(retired).exists(),
            "retired broad skill remains: {retired}"
        );
    }
}

#[test]
fn prompt_packet_guidance_allows_exploration_beyond_candidates() {
    let prompt = include_str!("../assets/pixel-agent-prompt.md");
    assert!(prompt.contains("bounded set of"));
    assert!(
        prompt.contains("not an action recommendation") && prompt.contains("a read/edit boundary")
    );
    assert!(prompt.contains("continue exploring any files or"));
}

/// Every heading and paragraph of a deployed prompt occurs once: a repeated
/// block (#571 shipped the opening section twice, #698) costs every session
/// its tokens and reads as two instructions.
#[test]
fn deployed_prompts_carry_each_heading_and_paragraph_once() {
    for (name, prompt) in [
        ("agent", include_str!("../assets/pixel-agent-prompt.md")),
        (
            "subagent",
            include_str!("../assets/pixel-subagent-prompt.md"),
        ),
    ] {
        let mut headings = std::collections::HashSet::new();
        for heading in prompt.lines().filter(|line| line.starts_with('#')) {
            assert!(
                headings.insert(heading),
                "{name} prompt repeats a heading: {heading}"
            );
        }
        let mut paragraphs = std::collections::HashSet::new();
        for block in prompt.split("\n\n") {
            // A paragraph is a block's text without its heading line, so a
            // repeated body under a new heading is still a repeat.
            let text = block
                .lines()
                .filter(|line| !line.starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n");
            let text = text.trim();
            assert!(
                text.len() <= 80 || paragraphs.insert(text.to_string()),
                "{name} prompt repeats a paragraph:\n{text}"
            );
        }
    }
}

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// A plugin root in a temp dir: the real hook script, the given context
/// files, and a `bin/` holding a fake `pixel` whose `repo-state --help`
/// exits with `repo_state_exit` (no `pixel` at all when `None`).
#[cfg(unix)]
fn plugin_root(context: &str, subagent: &str, repo_state_exit: Option<i32>) -> TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("hooks")).unwrap();
    fs::create_dir_all(dir.path().join("bin")).unwrap();
    fs::copy(
        repo_root().join("hooks/pixel-context.sh"),
        dir.path().join("hooks/pixel-context.sh"),
    )
    .unwrap();
    fs::write(dir.path().join("PIXEL.md"), context).unwrap();
    fs::write(dir.path().join("PIXEL-SUBAGENT.md"), subagent).unwrap();
    if let Some(code) = repo_state_exit {
        let exe = dir.path().join("bin/pixel");
        fs::write(
            &exe,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && echo 'pixel 0.1.0' && exit 0\n[ \"$1\" = repo-state ] && exit {code}\nexit 0\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// Run the hook as a harness does (stdin JSON, one event argument) with a
/// PATH of the fake `bin/` plus the system tools, and parse its one line.
#[cfg(unix)]
fn run_context_hook(root: &std::path::Path, event: &str) -> serde_json::Value {
    use std::io::Write;
    let mut child = std::process::Command::new("/bin/sh")
        .arg(root.join("hooks/pixel-context.sh"))
        .arg(event)
        .env("HOME", root)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"hook_event_name\":\"SessionStart\"}")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "one JSON line: {text}");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

/// The protocol reaches the model byte for byte, whatever it contains:
/// quotes, backslashes, tabs and non-ASCII survive the JSON encoding, and a
/// sub-agent gets the short sub-agent prompt, not the 18 KB one.
#[cfg(unix)]
#[test]
fn context_hook_injects_the_prompt_for_its_event_verbatim() {
    let context = "# Pixel \u{1F7E9}\n\n| `grep \"x\"` | `pixel search-content \"x\"` |\n\tpath\\to\\file\r\nend";
    let root = plugin_root(context, "sub-agent prompt\n", Some(0));

    let session = run_context_hook(root.path(), "SessionStart");
    assert_eq!(
        session["hookSpecificOutput"]["hookEventName"],
        "SessionStart"
    );
    assert_eq!(session["hookSpecificOutput"]["additionalContext"], context);

    let subagent = run_context_hook(root.path(), "SubagentStart");
    assert_eq!(
        subagent["hookSpecificOutput"]["hookEventName"],
        "SubagentStart"
    );
    assert_eq!(
        subagent["hookSpecificOutput"]["additionalContext"],
        "sub-agent prompt"
    );

    let unknown = run_context_hook(root.path(), "Weird\"Event");
    assert_eq!(
        unknown["hookSpecificOutput"]["hookEventName"], "SessionStart",
        "the event name in the JSON is never taken from the argument verbatim"
    );
}

/// A plugin is useful alone, but a global `pixel install` lifecycle hook owns
/// the same prompt when both paths are present.
#[cfg(unix)]
#[test]
fn context_hook_stays_silent_when_global_pixel_lifecycle_is_installed() {
    let root = plugin_root("PROTOCOL", "SUB", Some(0));
    let claude_dir = root.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(
        claude_dir.join("settings.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"command":"'/usr/local/bin/pixel' run-hook session-start --provider claude"}]}]}}"#,
    )
    .unwrap();
    let out = std::process::Command::new("/bin/sh")
        .arg(root.path().join("hooks/pixel-context.sh"))
        .arg("SessionStart")
        .env("HOME", root.path())
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.path().join("bin").display()),
        )
        .stdin(std::process::Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(
        out.stdout.is_empty(),
        "plugin must defer to global hooks: {out:?}"
    );
}

/// `pixel install` registers no SubagentStart context hook, so a global
/// SessionStart hook must not silence the plugin's sub-agent prompt.
#[cfg(unix)]
#[test]
fn context_hook_keeps_the_subagent_prompt_beside_global_lifecycle_hooks() {
    let root = plugin_root("PROTOCOL", "SUB", Some(0));
    let claude_dir = root.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(
        claude_dir.join("settings.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"command":"'/usr/local/bin/pixel' run-hook session-start --provider claude"}]}]}}"#,
    )
    .unwrap();
    let out = run_context_hook(root.path(), "SubagentStart");
    assert_eq!(
        out["hookSpecificOutput"]["hookEventName"], "SubagentStart",
        "{out}"
    );
    assert_eq!(
        out["hookSpecificOutput"]["additionalContext"], "SUB",
        "{out}"
    );
}

/// Without a usable binary the protocol is a list of failing commands: the
/// hook says why it was not loaded instead, and never tells the agent to
/// fetch an installer.
#[cfg(unix)]
#[test]
fn context_hook_replaces_the_protocol_with_a_notice_when_pixel_cannot_run_it() {
    let missing = plugin_root("PROTOCOL", "SUB", None);
    let notice = run_context_hook(missing.path(), "SessionStart");
    let text = notice["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(text.contains("`pixel` binary is not on PATH"), "{text}");
    assert!(!text.contains("PROTOCOL"), "{text}");
    assert!(!text.contains("curl"), "{text}");

    let outdated = plugin_root("PROTOCOL", "SUB", Some(2));
    let notice = run_context_hook(outdated.path(), "SubagentStart");
    let text = notice["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        text.contains("installed pixel 0.1.0 does not accept"),
        "{text}"
    );
    assert!(!text.contains("SUB"), "{text}");
    assert_eq!(
        notice["hookSpecificOutput"]["hookEventName"],
        "SubagentStart"
    );
}

/// Every manifest parses and every path it hands a harness exists in the
/// repository: a renamed hook script or a moved skills directory breaks the
/// plugin silently at install time, never in a build.
#[test]
fn plugin_manifests_parse_and_point_at_files_that_exist() {
    let repo = repo_root();
    let json = |rel: &str| -> serde_json::Value {
        let text = fs::read_to_string(repo.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{rel}: {e}"))
    };
    let exists = |owner: &str, rel: &str| {
        let rel = rel.trim_start_matches("./");
        assert!(
            repo.join(rel).exists(),
            "{owner} names {rel}, which is not in the repository"
        );
    };
    for rel in [
        ".claude-plugin/plugin.json",
        ".codex-plugin/plugin.json",
        ".qoder-plugin/plugin.json",
    ] {
        let manifest = json(rel);
        for field in ["skills", "hooks", "rules"] {
            if let Some(path) = manifest[field].as_str() {
                exists(rel, path);
            }
        }
    }
    for rel in [
        ".claude-plugin/marketplace.json",
        ".grok-plugin/marketplace.json",
        ".agents/plugins/marketplace.json",
    ] {
        let plugins = json(rel)["plugins"].as_array().cloned().unwrap_or_default();
        assert!(!plugins.is_empty(), "{rel} lists no plugin");
        for plugin in plugins {
            exists(rel, plugin["source"].as_str().unwrap());
        }
    }
    exists(
        "gemini-extension.json",
        json("gemini-extension.json")["contextFileName"]
            .as_str()
            .unwrap(),
    );
    let package = json("package.json");
    exists("package.json", package["main"].as_str().unwrap());
    let package_files = package["files"].as_array().unwrap();
    assert!(
        package_files
            .iter()
            .any(|entry| entry.as_str() == Some("claude-skills/")),
        "npm package must include the Claude-specific explicit-only skill copy"
    );
    for entry in package_files {
        exists("package.json", entry.as_str().unwrap());
    }
    for entry in json("opencode.json")["plugin"].as_array().unwrap() {
        exists("opencode.json", entry.as_str().unwrap());
    }

    let hooks = json("hooks/plugin-hooks.json");
    let mut commands = 0;
    for (event, matchers) in hooks["hooks"].as_object().unwrap() {
        for matcher in matchers.as_array().unwrap() {
            for hook in matcher["hooks"].as_array().unwrap() {
                let command = hook["command"].as_str().unwrap();
                commands += 1;
                assert!(
                    command.starts_with(
                        "\"${CLAUDE_PLUGIN_ROOT:-$PLUGIN_ROOT}/hooks/pixel-context.sh\""
                    ),
                    "{event}: the script must resolve from the plugin root Claude Code and Codex set: {command}"
                );
                assert!(
                    command.ends_with(&format!(" {event}")),
                    "{event}: {command}"
                );
            }
        }
    }
    assert_eq!(commands, 2, "SessionStart and SubagentStart");
    exists("hooks/plugin-hooks.json", "hooks/pixel-context.sh");

    // The legacy hook file remains available only to users who opt into it;
    // the default Codex and Claude manifests above deliberately do not load it.
    // A root `plugin.json` wins over the tool directories: Copilot CLI reads
    // it before `.claude-plugin/plugin.json`, and Codex's Agent Plugins loader
    // then ignores hooks declared in `.codex-plugin/plugin.json`
    // (openai/codex#39895). A bare one shipped neither skills nor hooks.
    assert!(
        !repo.join("plugin.json").exists(),
        "a root plugin.json shadows .claude-plugin/ and .codex-plugin/"
    );
}

// ---------------------------------------------------------------------------
// repo-local install tests (`pixel install --repo <path>`)
// ---------------------------------------------------------------------------

fn repo_install_options(repo: &std::path::Path, home: &std::path::Path) -> InstallOptions {
    InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
        repo: Some(repo.to_path_buf()),
        ..Default::default()
    }
}

#[test]
#[cfg(unix)]
fn repo_install_keeps_native_defaults_for_every_host() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&repo).unwrap();

    let report = install(&repo_install_options(&repo, &home)).expect("repo install");
    assert!(report.ok, "{report:?}");

    // Claude's old repo-local callbacks are removed. Lifecycle task hooks
    // belong to the global settings, while Claude's own tools retrieve.
    assert!(
        !repo.join(".claude/settings.json").exists(),
        "the shared settings.json must not carry a machine-local guard"
    );
    // Native cleanup has nothing to remove in a fresh repository, so it
    // registers no callback and creates no empty hook file.
    assert!(
        !repo.join(".claude/settings.local.json").exists(),
        "repo Claude must not get an empty settings file"
    );

    // Codex gets only task-scoped hooks; install does not create a permanent
    // developer-instructions config or root Pixel-first AGENTS.md block.
    assert!(!repo.join(".codex/config.toml").exists());
    assert!(!repo.join("AGENTS.md").exists());

    // Codex project settings carry no retrieval callback and do not
    // duplicate the global task-event suite: there is no file at all.
    assert!(
        !repo.join(".codex/hooks.json").exists(),
        "repo Codex must not get an empty hooks file"
    );
    assert!(
        !repo
            .join(".codex/pixel-composed-guard-backup.json")
            .exists(),
        "a fresh install has no composed-guard backup sidecar"
    );

    // Devin keeps its native tools: no guard, prompt or approval hook, and
    // no `.devin/` directory at all, in either config Devin could read.
    assert!(!repo.join(".devin").exists());

    // Pi keeps its native tools: no project extension is written; the
    // explicit impact command is the global package.
    assert!(!repo.join(".pi").exists());

    // Nothing global was touched.
    assert!(!home.join(".local/share/pixel").exists());
    assert!(!home.join(".codex").exists());
}

/// Upgrading a previous composed Codex guard restores its owned snapshot,
/// removes the wrapper and sidecar, and adds no project task-hook duplicates.
#[test]
#[cfg(unix)]
fn repo_install_restores_foreign_codex_hooks_from_a_legacy_composed_guard() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".codex")).unwrap();

    let foreign = serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":"keep-security-check"}]});
    let wrapper = serde_json::json!({
        "matcher": "Bash",
        "hooks": [{
            "type": "command",
            "command": format!(
                "{} run-hook composed-guard --provider codex --backup {}",
                fake_pixel_exe(&home).display(),
                repo.join(".codex/pixel-composed-guard-backup.json").display()
            )
        }]
    });
    let sidecar_path = repo.join(".codex/pixel-composed-guard-backup.json");
    fs::write(
        &sidecar_path,
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "provider": "codex",
            "pre_tool_use": [foreign.clone()],
            "managed_pre_tool_use": [wrapper.clone()]
        }))
        .unwrap(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&sidecar_path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(
        repo.join(".codex/hooks.json"),
        serde_json::to_vec(&serde_json::json!({ "hooks": { "PreToolUse": [wrapper] } })).unwrap(),
    )
    .unwrap();

    install(&repo_install_options(&repo, &home)).expect("legacy config is upgraded");
    let hooks: serde_json::Value =
        serde_json::from_slice(&fs::read(repo.join(".codex/hooks.json")).unwrap()).unwrap();
    assert_eq!(
        hooks["hooks"]["PreToolUse"],
        serde_json::json!([foreign]),
        "the original foreign PreToolUse group is restored exactly"
    );
    assert_eq!(
        hooks["hooks"]["SessionStart"],
        serde_json::Value::Null,
        "the project config does not duplicate global task hooks"
    );
    assert_eq!(
        hooks["hooks"]["PreToolUse"],
        serde_json::json!([foreign.clone()]),
        "no composed retrieval wrapper remains after upgrade"
    );
    assert!(
        !sidecar_path.exists(),
        "the consumed private backup is retired"
    );
}

#[test]
#[cfg(unix)]
fn repo_install_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&repo).unwrap();

    install(&repo_install_options(&repo, &home)).unwrap();
    let artifacts = [
        ".claude/settings.local.json",
        ".codex/hooks.json",
        ".devin/config.local.json",
        ".pi/extensions/pixel-guard.ts",
    ];
    // Native cleanup leaves the absent hook files absent; `None` compares too.
    let snapshot = |rel: &str| fs::read(repo.join(rel)).ok();
    let before: Vec<_> = artifacts.iter().map(|rel| snapshot(rel)).collect();

    let report = install(&repo_install_options(&repo, &home)).unwrap();
    assert!(report.ok);
    let after: Vec<_> = artifacts.iter().map(|rel| snapshot(rel)).collect();
    assert_eq!(before, after, "reinstall must be byte-identical");
}

#[test]
#[cfg(unix)]
fn repo_install_preserves_foreign_hooks_and_removes_retired_callbacks() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".claude")).unwrap();
    fs::create_dir_all(repo.join(".codex")).unwrap();
    fs::create_dir_all(repo.join(".devin")).unwrap();

    // .claude/settings.json (team-shared): a foreign lifecycle hook and a
    // foreign PreToolUse group that does not overlap the shell. The install
    // must not write a byte into it.
    let claude_foreign_lifecycle = serde_json::json!({"matcher":"startup","hooks":[{"type":"command","command":"keep-session-check"}]});
    let claude_foreign_guard = serde_json::json!({"matcher":"Write","hooks":[{"type":"command","command":"keep-write-check"}]});
    fs::write(
        repo.join(".claude/settings.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {
                "SessionStart": [claude_foreign_lifecycle.clone()],
                "PreToolUse": [claude_foreign_guard.clone()],
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let shared_before = fs::read(repo.join(".claude/settings.json")).unwrap();
    // .claude/settings.local.json (personal): a retired Pixel delegate had
    // adopted RTK, beside an unrelated user hook. Upgrade restores RTK and
    // removes only Pixel's callback.
    let claude_local_foreign = serde_json::json!({"matcher":"Edit","hooks":[{"type":"command","command":"keep-edit-check"}]});
    let rtk_group = serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]});
    let retired_delegate = serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":format!("{} run-hook guard --provider claude --delegate-rtk", fake_pixel_exe(&home).display())}]});
    fs::write(
        repo.join(".claude/settings.local.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "permissions": {"allow": ["Bash(ls:*)"]},
            "hooks": {"PreToolUse": [claude_local_foreign.clone(), retired_delegate.clone()]}
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        repo.join(".claude/pixel-rtk-hooks.json"),
        serde_json::to_vec(&serde_json::json!([rtk_group.clone()])).unwrap(),
    )
    .unwrap();

    let foreign = serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":"keep-security-check"}]});
    let codex_task_pre = task_hook_group(&fake_pixel_exe(&home), "codex", "pre-tool-use");
    let codex_task_stop = task_hook_group(&fake_pixel_exe(&home), "codex", "stop");
    fs::write(
        repo.join(".codex/hooks.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {
                "PreToolUse": [foreign.clone(), codex_task_pre.clone()],
                "Stop": [codex_task_stop.clone()]
            }
        }))
        .unwrap(),
    )
    .unwrap();
    // .devin/config.local.json: a foreign group beside the guard an earlier
    // release registered.
    let devin_retired_guard = serde_json::json!({"matcher":"exec","hooks":[{"type":"command","command":format!("{} run-hook guard --provider devin", fake_pixel_exe(&home).display())}]});
    fs::write(
        repo.join(".devin/config.local.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "permissions": {"allow": ["read"]},
            "hooks": {"PreToolUse": [foreign.clone(), devin_retired_guard]}
        }))
        .unwrap(),
    )
    .unwrap();

    install(&repo_install_options(&repo, &home)).unwrap();

    assert_eq!(
        fs::read(repo.join(".claude/settings.json")).unwrap(),
        shared_before,
        "the shared settings.json is not rewritten"
    );
    // Claude local: user keys and foreign hooks survive, and no retired
    // retrieval guard or lifecycle event is added.
    let claude: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo.join(".claude/settings.local.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(claude["permissions"]["allow"][0], "Bash(ls:*)");
    assert!(claude["hooks"].get("SessionStart").is_none(), "{claude}");
    assert_eq!(
        claude["hooks"]["PreToolUse"],
        serde_json::json!([claude_local_foreign, rtk_group]),
        "the exact adopted RTK registration returns beside the unrelated hook"
    );
    assert!(pixel_commands(&claude, "PreToolUse").is_empty(), "{claude}");
    assert!(
        !repo.join(".claude/pixel-rtk-hooks.json").exists(),
        "the backup is retired once RTK is restored"
    );

    // Codex: the foreign group remains in place beside task-event hooks;
    // native retrieval does not run through a composed wrapper.
    let codex: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(repo.join(".codex/hooks.json")).unwrap()).unwrap();
    assert_eq!(
        codex["hooks"]["PreToolUse"],
        serde_json::json!([foreign.clone(), codex_task_pre]),
        "foreign and pre-existing task hooks stay in their original order"
    );
    assert_eq!(codex["hooks"]["Stop"], serde_json::json!([codex_task_stop]));
    assert!(
        pixel_commands(&codex, "PreToolUse")
            .iter()
            .all(|command| command.contains("task-event")),
        "repo cleanup preserves task events without retrieval callbacks: {codex}"
    );
    assert!(
        pixel_commands(&codex, "SessionStart").is_empty(),
        "global task hooks are not duplicated by the repo install: {codex}"
    );
    assert!(
        !repo
            .join(".codex/pixel-composed-guard-backup.json")
            .exists()
    );

    // Devin: foreign group and keys kept, the retired Pixel guard removed
    // and nothing added.
    assert_eq!(
        read_json(&repo.join(".devin/config.local.json")),
        serde_json::json!({
            "permissions": {"allow": ["read"]},
            "hooks": {"PreToolUse": [foreign]}
        })
    );
}

#[test]
#[cfg(unix)]
fn repo_uninstall_removes_only_pixel_artifacts() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".claude")).unwrap();
    fs::create_dir_all(repo.join(".codex")).unwrap();
    fs::create_dir_all(repo.join(".devin")).unwrap();
    let claude_foreign = serde_json::json!({"matcher":"Write","hooks":[{"type":"command","command":"keep-write-check"}]});
    fs::write(
        repo.join(".claude/settings.local.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {"PreToolUse": [claude_foreign.clone()]}
        }))
        .unwrap(),
    )
    .unwrap();
    let foreign =
        serde_json::json!({"matcher":"exec","hooks":[{"type":"command","command":"keep-me"}]});
    fs::write(
        repo.join(".devin/config.local.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {"PreToolUse": [foreign.clone()]}
        }))
        .unwrap(),
    )
    .unwrap();
    let codex_foreign = serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":"keep-codex-check"}]});
    fs::write(
        repo.join(".codex/hooks.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {"PreToolUse": [codex_foreign.clone()]}
        }))
        .unwrap(),
    )
    .unwrap();

    install(&repo_install_options(&repo, &home)).unwrap();

    let report = uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        repo: Some(repo.clone()),
        ..Default::default()
    })
    .unwrap();
    assert!(report.ok, "{report:?}");

    // Codex task hooks are removed while its original foreign PreToolUse
    // group remains intact.
    let codex: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(repo.join(".codex/hooks.json")).unwrap()).unwrap();
    let mut pixel_commands = Vec::new();
    if let Some(events) = codex["hooks"].as_object() {
        for (event, groups) in events {
            for g in groups.as_array().into_iter().flatten() {
                for hook in g["hooks"].as_array().into_iter().flatten() {
                    if let Some(c) = hook["command"].as_str()
                        && c.contains("pixel")
                    {
                        pixel_commands.push(format!("{event}: {c}"));
                    }
                }
            }
        }
    }
    assert!(
        pixel_commands.is_empty(),
        "no pixel hook commands may survive repo uninstall: {pixel_commands:?} in {codex}"
    );
    assert_eq!(
        codex["hooks"]["PreToolUse"],
        serde_json::json!([codex_foreign]),
        "Codex's foreign PreToolUse group survives uninstall"
    );
    assert!(
        !repo
            .join(".codex/pixel-composed-guard-backup.json")
            .exists()
    );

    // Repo install/uninstall never creates a permanent Codex prompt.
    assert!(!repo.join(".codex/config.toml").exists());

    // Claude's foreign group remains — no repo retrieval callback is installed.
    let claude: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo.join(".claude/settings.local.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        claude["hooks"]["PreToolUse"],
        serde_json::json!([claude_foreign]),
        "foreign claude group preserved, pixel guard removed: {claude}"
    );

    // Devin: only the foreign group remains.
    let devin: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(repo.join(".devin/config.local.json")).unwrap())
            .unwrap();
    assert_eq!(devin["hooks"]["PreToolUse"], serde_json::json!([foreign]));

    // Pi guard gone, and the directories it alone occupied.
    assert!(!repo.join(".pi/extensions/pixel-guard.ts").exists());
    assert!(!repo.join(".pi").exists());
}

#[test]
#[cfg(unix)]
fn repo_install_dry_run_writes_nothing() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&repo).unwrap();

    let mut options = repo_install_options(&repo, &home);
    options.dry_run = true;
    let report = install(&options).unwrap();
    assert!(report.dry_run);
    assert!(report.ok, "{report:?}");

    assert!(!repo.join(".claude").exists());
    assert!(!repo.join(".codex").exists());
    assert!(!repo.join(".devin").exists());
    assert!(!repo.join(".pi").exists());
}

// ---------------------------------------------------------------------------
// repo-local artifacts stay machine-local (review of #222)
// ---------------------------------------------------------------------------

/// `git` in `dir` with a fixed identity and no global configuration, so the
/// developer's own excludes or hooks cannot change what the fixture sees.
fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// A repo-local hook file that native cleanup leaves absent when it has
/// nothing to remove: read as the empty hooks object it stands for.
fn read_local_hooks(path: &std::path::Path) -> serde_json::Value {
    if path.exists() {
        read_json(path)
    } else {
        serde_json::json!({"hooks": {}})
    }
}

fn pixel_commands(value: &serde_json::Value, event: &str) -> Vec<String> {
    value["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|group| group["hooks"].as_array())
        .flatten()
        .filter_map(|hook| hook["command"].as_str())
        .filter(|command| command.contains("pixel"))
        .map(ToString::to_string)
        .collect()
}

/// The machine-local paths `pixel install --repo` writes, as `git status`
/// prints them relative to the work tree root.
const MACHINE_LOCAL: &[&str] = &[
    ".claude/settings.local.json",
    ".claude/pixel-rtk-hooks.json",
    ".codex/hooks.json",
    ".codex/pixel-composed-guard-backup.json",
];

/// A guard an earlier `--repo` install wrote into shared settings runs this
/// machine's path on every clone. Upgrade removes that callback and keeps
/// the shared file's foreign hooks.
#[test]
#[cfg(unix)]
fn repo_install_should_move_a_guard_left_in_shared_settings_to_the_local_file() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".claude")).unwrap();
    fs::create_dir_all(repo.join(".devin")).unwrap();
    let write = serde_json::json!({"matcher":"Write","hooks":[{"type":"command","command":"keep-write-check"}]});
    let start = serde_json::json!({"hooks":[{"type":"command","command":"keep-session-check"}]});
    let stale_guard = serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":"'/Users/someone/.local/bin/pixel' run-hook guard --provider claude","timeout":10}]});
    fs::write(
        repo.join(".claude/settings.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {"PreToolUse": [write.clone(), stale_guard], "SessionStart": [start.clone()]}
        }))
        .unwrap(),
    )
    .unwrap();
    // The Devin guard of that earlier install sat in .devin/hooks.json,
    // which Devin CLI never reads; a foreign group there stays.
    let devin_foreign =
        serde_json::json!({"matcher":"exec","hooks":[{"type":"command","command":"keep-me"}]});
    let devin_stale = serde_json::json!({"matcher":"exec","hooks":[{"type":"command","command":"'/Users/someone/.local/bin/pixel' run-hook guard --provider devin"}]});
    fs::write(
        repo.join(".devin/hooks.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {"PreToolUse": [devin_foreign.clone(), devin_stale]}
        }))
        .unwrap(),
    )
    .unwrap();

    let report = install(&repo_install_options(&repo, &home)).unwrap();
    assert!(report.ok, "{report:?}");
    let claude_step = report
        .steps
        .iter()
        .find(|s| s.id == "hooks.claude")
        .unwrap();
    assert_eq!(
        claude_step.status,
        pixel_install::install::CheckStatus::Green,
        "{claude_step:?}"
    );
    assert!(
        claude_step.summary.contains("native tools preserved"),
        "native cleanup is reported: {claude_step:?}"
    );

    let shared = read_json(&repo.join(".claude/settings.json"));
    assert_eq!(shared["hooks"]["PreToolUse"], serde_json::json!([write]));
    assert_eq!(shared["hooks"]["SessionStart"], serde_json::json!([start]));
    let local = read_local_hooks(&repo.join(".claude/settings.local.json"));
    assert!(pixel_commands(&local, "PreToolUse").is_empty(), "{local}");

    let devin_legacy = read_json(&repo.join(".devin/hooks.json"));
    assert_eq!(
        devin_legacy["hooks"]["PreToolUse"],
        serde_json::json!([devin_foreign])
    );
    // No replacement guard moves to the file Devin does read.
    assert!(!repo.join(".devin/config.local.json").exists());

    let doctor_report = doctor(&DoctorOptions {
        home: Some(home.clone()),
        repo_root: Some(repo.clone()),
        ..Default::default()
    })
    .unwrap();
    let claude = check(&doctor_report, "repo.claude-hooks");
    assert_eq!(claude.status, CheckStatus::Green, "{claude:?}");
    assert!(claude.summary.contains("native"), "{claude:?}");
    let devin = check(&doctor_report, "repo.devin-hooks");
    assert_eq!(devin.status, CheckStatus::Green, "{devin:?}");
    assert_eq!(
        devin.summary,
        "no Pixel hook; the agent keeps its native tools"
    );
}

/// An earlier delegated guard in shared settings is removed and its adopted
/// RTK hook is restored; native retrieval remains available.
#[test]
#[cfg(unix)]
fn repo_install_should_hold_back_the_guard_beside_a_shell_rewriter_in_shared_settings() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".claude")).unwrap();
    fs::write(
        repo.join(".claude/settings.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {"PreToolUse": [{"matcher":"Bash","hooks":[{"type":"command","command":"'/old/pixel' run-hook guard --provider claude --delegate-rtk"}]}]}
        }))
        .unwrap(),
    )
    .unwrap();

    let report = install(&repo_install_options(&repo, &home)).unwrap();
    let claude_step = report
        .steps
        .iter()
        .find(|s| s.id == "hooks.claude")
        .unwrap();
    assert_eq!(
        claude_step.status,
        pixel_install::install::CheckStatus::Green,
        "{claude_step:?}"
    );
    assert!(
        claude_step.summary.contains("native tools preserved"),
        "{claude_step:?}"
    );
    let shared = read_json(&repo.join(".claude/settings.json"));
    assert_eq!(
        shared["hooks"]["PreToolUse"],
        serde_json::json!([{"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]}]),
        "the RTK group the delegate had adopted runs again"
    );
    let local = read_local_hooks(&repo.join(".claude/settings.local.json"));
    assert!(pixel_commands(&local, "PreToolUse").is_empty(), "{local}");
}

/// A plain RTK group already in repo settings remains untouched: native
/// cleanup neither adopts it nor creates a repository backup.
#[test]
#[cfg(unix)]
fn repo_install_and_uninstall_preserve_a_plain_rtk_registration() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".claude")).unwrap();
    let rtk = serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]});
    fs::write(
        repo.join(".claude/settings.local.json"),
        serde_json::to_string_pretty(&serde_json::json!({"hooks": {"PreToolUse": [rtk.clone()]}}))
            .unwrap(),
    )
    .unwrap();

    install(&repo_install_options(&repo, &home)).unwrap();
    assert!(
        !home.join(".claude/pixel-rtk-hooks.json").exists(),
        "the repo's adoption must not land in the global backup"
    );
    assert!(!repo.join(".claude/pixel-rtk-hooks.json").exists());
    let local = read_local_hooks(&repo.join(".claude/settings.local.json"));
    assert_eq!(
        local["hooks"]["PreToolUse"],
        serde_json::json!([rtk.clone()])
    );

    // A reinstall reads the repo backup back (the delegate requires it).
    install(&repo_install_options(&repo, &home)).expect("reinstall finds the repo backup");
    // A global install does not copy repo-local RTK into home settings.
    install(&InstallOptions {
        home: Some(home.clone()),
        executable_path: Some(fake_pixel_exe(&home)),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();
    let global = read_json(&home.join(".claude/settings.json"));
    assert!(
        !global.to_string().contains("rtk hook claude"),
        "global settings gained the repo's RTK group: {global}"
    );

    uninstall(&UninstallOptions {
        home: Some(home.clone()),
        repo: Some(repo.clone()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        read_local_hooks(&repo.join(".claude/settings.local.json"))["hooks"]["PreToolUse"],
        serde_json::json!([rtk])
    );
    assert!(!repo.join(".claude/pixel-rtk-hooks.json").exists());
}

/// A delegate guard whose repo backup is gone cannot be uninstalled without
/// losing the RTK registration it replaced: refuse and name the backup.
#[test]
#[cfg(unix)]
fn repo_uninstall_should_refuse_a_delegate_guard_without_its_repo_backup() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".claude")).unwrap();
    let local_path = repo.join(".claude/settings.local.json");
    fs::write(
        &local_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {"PreToolUse": [{"matcher":"Bash","hooks":[{"type":"command","command":"'/p/pixel' run-hook guard --provider claude --delegate-rtk"}]}]}
        }))
        .unwrap(),
    )
    .unwrap();
    let before = fs::read(&local_path).unwrap();
    let err = uninstall(&UninstallOptions {
        home: Some(home.clone()),
        repo: Some(repo.clone()),
        ..Default::default()
    })
    .expect_err("a delegate without its backup is refused");
    assert!(err.to_string().contains("pixel-rtk-hooks.json"), "{err}");
    assert_eq!(fs::read(&local_path).unwrap(), before);
}

/// Repo uninstall also takes out a guard an earlier install left in the
/// shared settings.json and in .devin/hooks.json, and leaves a local file
/// without a delegate free of any `PreToolUse` event it did not have.
#[test]
#[cfg(unix)]
fn repo_uninstall_should_clean_guards_left_in_shared_and_legacy_files() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".claude")).unwrap();
    fs::create_dir_all(repo.join(".devin")).unwrap();
    let stale = |provider: &str| serde_json::json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":format!("'/old/pixel' run-hook guard --provider {provider}")}]}]}});
    fs::write(
        repo.join(".claude/settings.json"),
        serde_json::to_string_pretty(&stale("claude")).unwrap(),
    )
    .unwrap();
    fs::write(
        repo.join(".devin/hooks.json"),
        serde_json::to_string_pretty(&stale("devin")).unwrap(),
    )
    .unwrap();
    fs::write(
        repo.join(".claude/settings.local.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "permissions": {"allow": ["Bash(ls:*)"]},
            "hooks": {"SessionStart": [{"hooks":[{"type":"command","command":"'/p/pixel' run-hook session-start"}]}]}
        }))
        .unwrap(),
    )
    .unwrap();

    let report = uninstall(&UninstallOptions {
        home: Some(home.clone()),
        repo: Some(repo.clone()),
        ..Default::default()
    })
    .unwrap();
    let claude_step = report
        .steps
        .iter()
        .find(|s| s.id == "hooks.claude")
        .unwrap();
    assert!(
        claude_step
            .summary
            .contains("from 2 Claude settings file(s)"),
        "{claude_step:?}"
    );
    let devin_step = report.steps.iter().find(|s| s.id == "hooks.devin").unwrap();
    assert!(
        devin_step.summary.contains("removed 1 Devin"),
        "the legacy file counts: {devin_step:?}"
    );
    let shared = read_json(&repo.join(".claude/settings.json"));
    assert!(pixel_commands(&shared, "PreToolUse").is_empty(), "{shared}");
    let legacy = read_json(&repo.join(".devin/hooks.json"));
    assert!(pixel_commands(&legacy, "PreToolUse").is_empty(), "{legacy}");
    let local = read_local_hooks(&repo.join(".claude/settings.local.json"));
    assert_eq!(
        local,
        serde_json::json!({"permissions": {"allow": ["Bash(ls:*)"]}, "hooks": {}}),
        "no PreToolUse event is created where none was"
    );
}

/// Every repo artifact names this machine's binary. In a git clone the
/// install lists them in the clone's own `info/exclude` (never shared), so
/// `git status` offers none of them for a commit, and it leaves a
/// `.codex/hooks.json` the project tracks untouched: Codex has no personal
/// project file, and the composed guard would put this machine's path into a
/// file every clone runs.
#[test]
#[cfg(unix)]
fn repo_install_should_keep_machine_local_artifacts_out_of_git() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".codex")).unwrap();
    git(&repo, &["init", "-q"]);
    let team_hooks = serde_json::to_string_pretty(&serde_json::json!({
        "hooks": {"PreToolUse": [{"matcher":"Bash","hooks":[{"type":"command","command":"./scripts/team-check.sh"}]}]}
    }))
    .unwrap();
    fs::write(repo.join(".codex/hooks.json"), &team_hooks).unwrap();
    git(&repo, &["add", "-f", ".codex/hooks.json"]);
    git(&repo, &["commit", "-q", "-m", "team codex hooks"]);
    // No info/ directory at all: the install creates what it needs.
    let git_dir = repo.join(".git");
    fs::remove_dir_all(git_dir.join("info")).unwrap();

    let mut dry = repo_install_options(&repo, &home);
    dry.dry_run = true;
    let dry_report = install(&dry).unwrap();
    assert!(
        !git_dir.join("info/exclude").exists(),
        "a dry run writes no exclude"
    );
    let dry_step = dry_report
        .steps
        .iter()
        .find(|s| s.id == "repo.git-exclude")
        .unwrap();
    assert_eq!(
        dry_step.summary,
        format!(
            "[dry-run] would report: {} machine-local path(s) added to the clone's info/exclude",
            MACHINE_LOCAL.len()
        ),
        "{dry_step:?}"
    );
    assert_eq!(MACHINE_LOCAL.len(), 4);

    let report = install(&repo_install_options(&repo, &home)).unwrap();
    assert!(report.ok, "{report:?}");
    let codex_step = report.steps.iter().find(|s| s.id == "hooks.codex").unwrap();
    assert_eq!(
        codex_step.status,
        pixel_install::install::CheckStatus::Yellow,
        "{codex_step:?}"
    );
    assert!(
        codex_step.summary.contains("tracked by git"),
        "{codex_step:?}"
    );
    assert_eq!(
        fs::read_to_string(repo.join(".codex/hooks.json")).unwrap(),
        team_hooks,
        "a tracked Codex hook file is not rewritten"
    );
    assert!(
        !repo
            .join(".codex/pixel-composed-guard-backup.json")
            .exists()
    );

    let exclude = fs::read_to_string(git_dir.join("info/exclude")).unwrap();
    assert!(
        exclude.starts_with("# pixel install --repo"),
        "no blank line before the block in a new file: {exclude:?}"
    );
    for rel in MACHINE_LOCAL {
        let pattern = format!("/{rel}");
        assert_eq!(
            exclude.lines().filter(|line| *line == pattern).count(),
            1,
            "{pattern} in {exclude}"
        );
    }
    let status = git(&repo, &["status", "--porcelain", "--untracked-files=all"]);
    for rel in MACHINE_LOCAL {
        assert!(
            !status.lines().any(|line| line.ends_with(rel)),
            "{rel} is offered for a commit:\n{status}"
        );
    }
    assert!(
        !repo.join(".codex/config.toml").exists(),
        "repo install does not create a permanent Codex prompt"
    );

    // A second install adds nothing to the exclude file.
    let again = install(&repo_install_options(&repo, &home)).unwrap();
    assert_eq!(
        fs::read_to_string(git_dir.join("info/exclude")).unwrap(),
        exclude
    );
    let again_step = again
        .steps
        .iter()
        .find(|s| s.id == "repo.git-exclude")
        .unwrap();
    assert!(
        again_step.summary.starts_with("0 machine-local"),
        "{again_step:?}"
    );
    assert!(again_step.detail.is_none(), "{again_step:?}");
}

/// A repository below the work-tree root gets patterns anchored at its own
/// path, appended after the clone's existing excludes on a line of their own.
#[test]
#[cfg(unix)]
fn repo_install_should_anchor_excludes_at_a_nested_repo_path() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let top = dir.path().join("top");
    let repo = top.join("sub");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&repo).unwrap();
    git(&top, &["init", "-q"]);
    fs::write(top.join(".git/info/exclude"), "*.log").unwrap();

    install(&repo_install_options(&repo, &home)).unwrap();

    let exclude = fs::read_to_string(top.join(".git/info/exclude")).unwrap();
    assert!(
        exclude.starts_with("*.log\n# pixel install --repo"),
        "{exclude:?}"
    );
    assert!(
        exclude
            .lines()
            .any(|l| l == "/sub/.claude/settings.local.json"),
        "{exclude}"
    );
    let status = git(&top, &["status", "--porcelain", "--untracked-files=all"]);
    assert!(
        !status.contains("settings.local.json") && !status.contains("pixel-guard.ts"),
        "{status}"
    );
}

/// Many repositories keep their own `.claude/settings.json`,
/// `.claude/settings.local.json`, `.devin/config.local.json` and `.codex/`
/// files. With nothing of Pixel's in them, the repo was never repo-installed,
/// which is a valid state: `doctor` must not go red on it.
#[test]
#[cfg(unix)]
fn doctor_repo_checks_should_stay_green_on_a_project_with_its_own_configs() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    for sub in [".claude", ".devin", ".codex"] {
        fs::create_dir_all(repo.join(sub)).unwrap();
    }
    let foreign = serde_json::json!({
        "permissions": {"allow": ["Bash(ls:*)"]},
        "hooks": {"PreToolUse": [{"matcher":"Bash","hooks":[{"type":"command","command":"./scripts/check.sh"}]}]}
    });
    for rel in [
        ".claude/settings.json",
        ".claude/settings.local.json",
        ".devin/config.local.json",
        ".codex/hooks.json",
    ] {
        fs::write(
            repo.join(rel),
            serde_json::to_string_pretty(&foreign).unwrap(),
        )
        .unwrap();
    }
    fs::write(
        repo.join(".codex/config.toml"),
        "model = \"o3\"\ndeveloper_instructions = \"Follow CONTRIBUTING.md.\"\n",
    )
    .unwrap();

    let report = doctor(&DoctorOptions {
        home: Some(home.clone()),
        repo_root: Some(repo.clone()),
        ..Default::default()
    })
    .unwrap();
    for id in [
        "repo.codex-config",
        "repo.codex-hooks",
        "repo.devin-hooks",
        "repo.claude-hooks",
    ] {
        let c = check(&report, id);
        assert_eq!(c.status, CheckStatus::Green, "{id}: {c:?}");
        if id == "repo.codex-config" {
            assert!(c.summary.contains("no retired Pixel block"), "{id}: {c:?}");
        } else if id == "repo.codex-hooks" {
            assert!(
                c.summary.contains("native Codex hooks preserved"),
                "{id}: {c:?}"
            );
        } else if id == "repo.claude-hooks" {
            assert!(
                c.summary.contains("native Claude hooks preserved"),
                "{id}: {c:?}"
            );
        } else {
            assert_eq!(
                c.summary, "no Pixel hook; the agent keeps its native tools",
                "{id}: {c:?}"
            );
        }
    }
}

/// Record `[projects."<repo>"] trust_level = "<level>"` in the global Codex
/// config, the way Codex itself does. The whole file is rewritten: Codex keeps
/// one table per project, and appending a second one for the same key makes the
/// document a duplicate-key error rather than a trust level.
fn set_codex_trust(home: &std::path::Path, repo: &std::path::Path, level: &str) {
    let path = home.join(".codex/config.toml");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        format!(
            "[projects.\"{}\"]\ntrust_level = \"{level}\"\n",
            repo.display()
        ),
    )
    .unwrap();
}

/// Native Codex retrieval needs no project hook. Project trust therefore has
/// no effect on the clean native-default install check.
#[test]
#[cfg(unix)]
fn doctor_repo_codex_hooks_should_not_depend_on_project_trust() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&repo).unwrap();
    let report = install(&repo_install_options(&repo, &home)).unwrap();
    assert!(report.ok, "{report:?}");
    let doctor_options = DoctorOptions {
        home: Some(home.clone()),
        repo_root: Some(repo.clone()),
        ..Default::default()
    };

    for level in ["", "untrusted", "trusted"] {
        if !level.is_empty() {
            set_codex_trust(&home, &repo, level);
        }
        let doctor_report = doctor(&doctor_options).unwrap();
        let c = check(&doctor_report, "repo.codex-hooks");
        assert_eq!(c.status, CheckStatus::Green, "trust={level:?}: {c:?}");
        assert!(
            !c.summary.contains("trust"),
            "a native-default Codex check has no project trust claim: {c:?}"
        );
    }
}

/// Evidence of a Pixel install that is broken or retired stays red: a Pixel
/// hook without the guard, a Devin hook of any kind, an RTK backup without
/// the guard, a Pixel block gone stale, a Pixel hook without its
/// composed-guard sidecar, or a guard sitting in the shared settings.json
/// where it runs this machine's path on every clone.
#[test]
#[cfg(unix)]
fn doctor_repo_checks_should_go_red_on_a_broken_pixel_install() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    for sub in [".claude", ".devin", ".codex"] {
        fs::create_dir_all(repo.join(sub)).unwrap();
    }
    let lifecycle_only = serde_json::json!({
        "hooks": {"SessionStart": [{"hooks":[{"type":"command","command":"'/p/pixel' run-hook session-start"}]}]}
    });
    for rel in [
        ".claude/settings.local.json",
        ".devin/config.local.json",
        ".codex/hooks.json",
    ] {
        fs::write(
            repo.join(rel),
            serde_json::to_string_pretty(&lifecycle_only).unwrap(),
        )
        .unwrap();
    }
    fs::write(
        repo.join(".codex/config.toml"),
        format!("developer_instructions = '''\n{MANAGED_BEGIN}\nold prompt\n{MANAGED_END}\n'''\n"),
    )
    .unwrap();
    let doctor_options = DoctorOptions {
        home: Some(home.clone()),
        repo_root: Some(repo.clone()),
        ..Default::default()
    };

    let report = doctor(&doctor_options).unwrap();
    assert_eq!(check(&report, "repo.codex-config").status, CheckStatus::Red);
    assert_eq!(check(&report, "repo.codex-hooks").status, CheckStatus::Red);
    assert_eq!(check(&report, "repo.devin-hooks").status, CheckStatus::Red);
    assert_ne!(
        check(&report, "repo.claude-hooks").status,
        CheckStatus::Green
    );

    // Devin keeps its native tools: a Pixel guard left in the legacy
    // `.devin/hooks.json` alone is red too, naming that file and the repo
    // install that removes it.
    fs::write(
        repo.join(".devin/config.local.json"),
        serde_json::json!({"hooks": {"PreToolUse": [{"matcher":"exec","hooks":[{"type":"command","command":"keep-me"}]}]}}).to_string(),
    )
    .unwrap();
    fs::write(
        repo.join(".devin/hooks.json"),
        serde_json::json!({"hooks": {"PreToolUse": [{"matcher":"exec|Bash","hooks":[{"type":"command","command":"'/p/pixel' run-hook guard --provider devin"}]}]}}).to_string(),
    )
    .unwrap();
    let report = doctor(&doctor_options).unwrap();
    let devin = check(&report, "repo.devin-hooks");
    assert_eq!(devin.status, CheckStatus::Red, "{devin:?}");
    assert_eq!(
        devin.reason.as_deref(),
        Some(
            format!(
                "retired Pixel hooks remain in {} — run `pixel install --repo '{}'` to remove them",
                repo.join(".devin/hooks.json").display(),
                repo.display()
            )
            .as_str()
        )
    );

    // An RTK backup alone is evidence too.
    fs::write(repo.join(".claude/settings.local.json"), "{}").unwrap();
    fs::write(
        repo.join(".claude/pixel-rtk-hooks.json"),
        r#"[{"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]}]"#,
    )
    .unwrap();
    let report = doctor(&doctor_options).unwrap();
    assert_eq!(
        check(&report, "repo.claude-hooks").status,
        CheckStatus::Yellow
    );

    // Existing task controls remain valid; retrieval guards are retired in
    // both local and shared files.
    fs::remove_file(repo.join(".claude/pixel-rtk-hooks.json")).unwrap();
    fs::write(
        repo.join(".claude/settings.local.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {"Stop": [{"hooks":[{"type":"command","command":"'/p/pixel' run-hook task-event --provider claude --event stop"}]}]}
        })).unwrap(),
    ).unwrap();
    let report = doctor(&doctor_options).unwrap();
    assert_eq!(
        check(&report, "repo.claude-hooks").status,
        CheckStatus::Green
    );
    let guard = serde_json::to_string_pretty(&serde_json::json!({
        "hooks": {"PreToolUse": [{"matcher":"Bash","hooks":[{"type":"command","command":"'/p/pixel' run-hook guard --provider claude"}]}]}
    }))
    .unwrap();
    fs::write(repo.join(".claude/settings.local.json"), &guard).unwrap();
    let report = doctor(&doctor_options).unwrap();
    assert_eq!(check(&report, "repo.claude-hooks").status, CheckStatus::Red);
    fs::write(repo.join(".claude/settings.json"), &guard).unwrap();
    let report = doctor(&doctor_options).unwrap();
    let claude = check(&report, "repo.claude-hooks");
    assert_eq!(claude.status, CheckStatus::Red, "{claude:?}");
    assert!(
        claude
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("shared")),
        "{claude:?}"
    );
}

/// The code graph is `.pixel/graph.v2.db` since the graph schema bump; a
/// doctor still looking for `graph.db` reports a freshly built graph as
/// missing, and a leftover `graph.db` from an older build as present.
#[test]
fn doctor_graph_freshness_should_read_the_file_the_daemon_builds() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".pixel")).unwrap();
    let doctor_options = DoctorOptions {
        home: Some(home.clone()),
        repo_root: Some(repo.clone()),
        ..Default::default()
    };
    fs::write(repo.join(".pixel/graph.db"), b"old schema").unwrap();
    let report = doctor(&doctor_options).unwrap();
    assert_eq!(
        check(&report, "graph.freshness").status,
        CheckStatus::Red,
        "a pre-bump graph.db is not the graph"
    );
    assert_eq!(pixel_daemon::api::GRAPH_DB_FILE, "graph.v2.db");
    fs::write(repo.join(".pixel/graph.v2.db"), b"built").unwrap();
    let report = doctor(&doctor_options).unwrap();
    let graph = check(&report, "graph.freshness");
    assert_eq!(graph.status, CheckStatus::Green, "{graph:?}");
}

/// Upgrade removes bare scripts from the automatic retrieval path, installs
/// one task-event set, and keeps foreign hooks that merely mention Pixel.
#[test]
#[cfg(unix)]
fn install_should_replace_legacy_script_hooks_instead_of_stacking_new_ones() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    fs::create_dir_all(home.join(".claude")).unwrap();
    let foreign = serde_json::json!({"hooks":[{"type":"command","command":"notify --on pixel-session-start"}]});
    fs::write(
        home.join(".claude/settings.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {"matcher":"Bash","hooks":[{"type":"command","command":"~/.claude/hooks/gitpixel-targets-guard"}]},
                    {"matcher":"Bash","hooks":[{"type":"command","command":"~/.claude/hooks/pixel-targets-guard"}]}
                ],
                "SessionStart": [
                    {"hooks":[{"type":"command","command":"~/.claude/hooks/pixel-session-start"}]},
                    {"matcher":"compact","hooks":[{"type":"command","command":"~/.claude/hooks/pixel-post-compaction"}]},
                    foreign.clone()
                ],
                "UserPromptSubmit": [
                    {"hooks":[{"type":"command","command":"~/.claude/hooks/pixel-prompt-submit"}]}
                ]
            }
        }))
        .unwrap(),
    )
    .unwrap();

    install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();

    let installed = read_json(&home.join(".claude/settings.json"));
    let settings = without_task_hooks(&installed, "claude", &home.join("pixel"));
    assert!(
        !settings.to_string().contains("/.claude/hooks/"),
        "legacy scripts left: {settings}"
    );
    assert!(
        settings["hooks"].get("PreToolUse").is_none(),
        "the global install registers no retrieval guard: {settings}"
    );
    assert!(
        settings["hooks"]["SessionStart"]
            .as_array()
            .unwrap()
            .contains(&foreign),
        "a foreign command naming a pixel verb is kept: {settings}"
    );
}

/// A stand-in pixel build installed under another name, the way
/// `pixel self-update --dev` installs `pixel-dev`, canonicalized as install
/// writes it into the hook commands.
#[cfg(unix)]
fn fake_dev_exe(home: &std::path::Path) -> std::path::PathBuf {
    fake_exe_named(home, "pixel-dev")
}

/// A stand-in pixel build installed as `name` under `~/.local/bin`,
/// canonicalized as install writes it into the hook commands.
#[cfg(unix)]
fn fake_exe_named(home: &std::path::Path, name: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = home.join(".local/bin");
    fs::create_dir_all(&bin).unwrap();
    let path = bin.join(name);
    fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path.canonicalize().unwrap()
}

/// Each synchronous task boundary has one global Claude task hook.
fn task_event_counts(settings: &serde_json::Value) -> Vec<(&'static str, usize)> {
    [
        ("SessionStart", "session-start"),
        ("UserPromptSubmit", "prompt-submit"),
        ("PreToolUse", "pre-tool-use"),
        ("PostToolUse", "post-tool-use"),
        ("Stop", "stop"),
        ("SessionEnd", "session-end"),
        ("SubagentStart", "subagent-start"),
        ("SubagentStop", "subagent-stop"),
        ("PostToolUseFailure", "tool-failure"),
    ]
    .into_iter()
    .map(|(event, name)| {
        let verb = format!("task-event --provider claude --event {name}");
        (
            name,
            pixel_commands(settings, event)
                .iter()
                .filter(|c| c.contains(&verb))
                .count(),
        )
    })
    .collect()
}

const ONE_EACH: [(&str, usize); 9] = [
    ("session-start", 1),
    ("prompt-submit", 1),
    ("pre-tool-use", 1),
    ("post-tool-use", 1),
    ("stop", 1),
    ("session-end", 1),
    ("subagent-start", 1),
    ("subagent-stop", 1),
    ("tool-failure", 1),
];

/// A build not named `pixel` must replace the entries it wrote on the last
/// install. It used to append a new set each time (0 → 4 → 8 → 12 entries),
/// so every prompt and every edit ran each hook once per install. Both the
/// name `self-update --dev` writes and a name chosen by hand.
#[test]
#[cfg(unix)]
fn install_by_a_binary_not_named_pixel_should_replace_its_own_hooks_not_stack_them() {
    for name in ["pixel-dev", "pixel-livio"] {
        let dir = TempDir::new().unwrap();
        let home = dir.path();
        let exe = fake_exe_named(home, name);
        for _ in 0..3 {
            install(&InstallOptions {
                home: Some(home.to_path_buf()),
                executable_path: Some(exe.clone()),
                shell: Some(TEST_SHELL.into()),
                ..Default::default()
            })
            .unwrap();
        }
        let settings = read_json(&home.join(".claude/settings.json"));
        assert_eq!(task_event_counts(&settings), ONE_EACH, "{name}: {settings}");
        assert!(
            settings.to_string().contains(&format!("{name}' run-hook")),
            "the entries name the {name} build: {settings}"
        );
    }
}

/// Going back from a dev build to the release: `pixel install` after
/// `pixel-dev install` must replace the dev entries, not add its own beside
/// them and run every hook twice.
#[test]
#[cfg(unix)]
fn release_install_after_a_dev_install_should_replace_the_dev_hooks() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    for exe in [fake_dev_exe(home), fake_pixel_exe(home)] {
        install(&InstallOptions {
            home: Some(home.to_path_buf()),
            executable_path: Some(exe),
            shell: Some(TEST_SHELL.into()),
            ..Default::default()
        })
        .unwrap();
    }
    let settings = read_json(&home.join(".claude/settings.json"));
    assert_eq!(task_event_counts(&settings), ONE_EACH, "{settings}");
    assert!(
        !settings.to_string().contains("pixel-dev"),
        "no dev entry left: {settings}"
    );
}

/// Machines that ran the stacking install still hold several copies per
/// event. Doctor reports them red, and one install collapses them to one
/// entry each while a foreign hook in the same event (herdr's SessionStart
/// hook, with its own matcher) or in the same group as a pixel copy comes
/// out unchanged.
#[test]
#[cfg(unix)]
fn one_install_should_collapse_stacked_dev_hooks_and_keep_foreign_ones_unchanged() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let exe = fake_dev_exe(home);
    let quoted = format!("'{}'", exe.display());
    let pixel = |verb: &str| serde_json::json!({"type":"command","command":format!("{quoted} run-hook {verb}"),"timeout":5});
    let herdr = serde_json::json!({"matcher":"startup","hooks":[{"type":"command","command":"herdr hook session-start --agent claude","timeout":10}]});
    let notify = serde_json::json!({"type":"command","command":"notify-send claude-started"});
    let mut session = vec![herdr.clone()];
    let mut prompt = Vec::new();
    let mut edit =
        vec![serde_json::json!({"matcher":"Bash","hooks":[pixel("metrics --provider claude")]})];
    for copy in 0..3 {
        let mut start = vec![pixel("session-start")];
        if copy == 0 {
            start.push(notify.clone());
        }
        session.push(serde_json::json!({"hooks": start}));
        session.push(serde_json::json!({"matcher":"compact","hooks":[pixel("post-compaction --provider claude")]}));
        prompt.push(serde_json::json!({"hooks":[pixel("prompt-submit --provider claude")]}));
        edit.push(serde_json::json!({"matcher":"Edit","hooks":[pixel("post-tool-use --provider claude")]}));
    }
    let path = home.join(".claude/settings.json");
    fs::create_dir_all(home.join(".claude")).unwrap();
    let mut stacked = serde_json::json!({"hooks":{
        "SessionStart": session,
        "UserPromptSubmit": prompt,
        "PostToolUse": edit,
    }});
    for (event, name) in [
        ("SessionStart", "session-start"),
        ("UserPromptSubmit", "prompt-submit"),
        ("PreToolUse", "pre-tool-use"),
        ("PostToolUse", "post-tool-use"),
        ("Stop", "stop"),
        ("SessionEnd", "session-end"),
        ("SubagentStart", "subagent-start"),
        ("SubagentStop", "subagent-stop"),
        ("PostToolUseFailure", "tool-failure"),
    ] {
        stacked["hooks"]
            .as_object_mut()
            .unwrap()
            .entry(event)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .unwrap()
            .push(task_hook_group(&exe, "claude", name));
    }
    fs::write(&path, serde_json::to_string_pretty(&stacked).unwrap()).unwrap();
    let doctor_hooks = || {
        let report = doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            executable_path: Some(exe.clone()),
            shell: Some(TEST_SHELL.into()),
            only: vec!["install.claude-hooks".into()],
            ..Default::default()
        })
        .unwrap();
        check(&report, "install.claude-hooks").clone()
    };
    let before = doctor_hooks();
    assert_eq!(before.status, CheckStatus::Red, "{before:?}");
    assert!(
        before
            .reason
            .as_deref()
            .is_some_and(|reason| !reason.is_empty()),
        "{before:?}"
    );

    install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(exe.clone()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();

    let settings = read_json(&path);
    assert_eq!(task_event_counts(&settings), ONE_EACH, "{settings}");
    let groups = settings["hooks"]["SessionStart"].as_array().unwrap();
    assert!(groups.contains(&herdr), "herdr's group changed: {settings}");
    assert!(
        groups.contains(&serde_json::json!({"hooks":[notify]})),
        "the foreign hook sharing a group with a pixel copy is kept alone in it: {settings}"
    );
    let after = doctor_hooks();
    assert_eq!(after.status, CheckStatus::Green, "{after:?}");
}

/// A home with `pixel install` run and the Pixel plugin `pixel@local`
/// installed in Claude (recorded in `installed_plugins.json`), but enabled
/// nowhere yet.
#[cfg(unix)]
fn home_with_global_hooks_and_installed_plugin(home: &Path) -> std::path::PathBuf {
    let exe = fake_pixel_exe(home);
    install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(exe.clone()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();
    let plugins = home.join(".claude/plugins");
    fs::create_dir_all(&plugins).unwrap();
    fs::write(
        plugins.join("installed_plugins.json"),
        r#"{"version":2,"plugins":{"pixel@local":[{"scope":"user"}]}}"#,
    )
    .unwrap();
    exe
}

/// Set `enabledPlugins` in the Claude settings file at `path`, keeping the
/// rest of it.
fn enable_plugins(path: &Path, plugins: serde_json::Value) {
    let mut settings = if path.is_file() {
        read_json(path)
    } else {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        serde_json::json!({})
    };
    settings["enabledPlugins"] = plugins;
    fs::write(path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();
}

#[cfg(unix)]
fn claude_hooks_check(
    home: &Path,
    exe: std::path::PathBuf,
    repo: Option<&Path>,
) -> pixel_install::doctor::DoctorCheck {
    let report = doctor(&DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(exe),
        shell: Some(TEST_SHELL.into()),
        repo_root: repo.map(Path::to_path_buf),
        only: vec!["install.claude-hooks".into()],
        ..Default::default()
    })
    .unwrap();
    check(&report, "install.claude-hooks").clone()
}

#[test]
#[cfg(unix)]
fn doctor_should_warn_when_claude_plugin_and_global_hooks_both_own_prompt() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let exe = home_with_global_hooks_and_installed_plugin(home);
    let settings = home.join(".claude/settings.json");
    enable_plugins(&settings, serde_json::json!({"pixel@local": true}));
    let finding = claude_hooks_check(home, exe, None);
    assert_eq!(finding.status, CheckStatus::Yellow, "{finding:?}");
    assert!(
        finding
            .summary
            .contains("plugin `pixel@local` and global lifecycle hooks"),
        "{finding:?}"
    );
    assert!(
        finding.summary.contains(&settings.display().to_string()),
        "the warning names the file that enables the plugin: {finding:?}"
    );
    // `pixel install` rewrites the global hooks and leaves the plugin
    // enabled: offering it as the repair would never converge.
    assert_eq!(finding.fix, None, "{finding:?}");
}

/// The overlap follows the plugin state Claude applies in the repository:
/// project settings enable it, local settings override them, and a plugin
/// Claude has not installed runs nothing.
#[test]
#[cfg(unix)]
fn doctor_should_judge_the_plugin_state_claude_applies_in_the_repo() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&repo).unwrap();
    let exe = home_with_global_hooks_and_installed_plugin(&home);
    let project = repo.join(".claude/settings.json");
    let local = repo.join(".claude/settings.local.json");

    // Enabled by the project only.
    enable_plugins(&project, serde_json::json!({"pixel@local": true}));
    let finding = claude_hooks_check(&home, exe.clone(), Some(&repo));
    assert_eq!(finding.status, CheckStatus::Yellow, "{finding:?}");
    assert!(
        finding.summary.contains(&project.display().to_string()),
        "{finding:?}"
    );
    // Without the repository, the home alone enables nothing.
    let finding = claude_hooks_check(&home, exe.clone(), None);
    assert_eq!(finding.status, CheckStatus::Green, "{finding:?}");

    // Local settings disable what the project enabled.
    enable_plugins(&local, serde_json::json!({"pixel@local": false}));
    let finding = claude_hooks_check(&home, exe.clone(), Some(&repo));
    assert_eq!(finding.status, CheckStatus::Green, "{finding:?}");

    // Enabled, but not a plugin Claude has installed.
    fs::remove_file(&local).unwrap();
    enable_plugins(&project, serde_json::json!({"pixel@elsewhere": true}));
    let finding = claude_hooks_check(&home, exe, Some(&repo));
    assert_eq!(finding.status, CheckStatus::Green, "{finding:?}");
}

/// After a global `pixel-dev install` every hook is present and registered
/// once, yet every session on the machine runs the side build: the managed
/// `pixel` reads that as yellow with `pixel install` as its fix, and the
/// same holds for hooks left on a previous release's path by an upgrade.
/// `pixel install` points them back; the side build itself never judges.
#[test]
#[cfg(unix)]
fn doctor_should_flag_claude_hooks_that_run_another_pixel_binary() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let release = fake_exe_named(home, "pixel");
    let dev = fake_dev_exe(home);
    let old_dir = home.join(".local/share/mise/installs/pixel/0.6.0/bin");
    fs::create_dir_all(&old_dir).unwrap();
    fs::write(old_dir.join("pixel"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(old_dir.join("pixel"), fs::Permissions::from_mode(0o755)).unwrap();
    let old = old_dir.join("pixel").canonicalize().unwrap();
    let install_as = |exe: &std::path::Path| {
        install(&InstallOptions {
            home: Some(home.to_path_buf()),
            executable_path: Some(exe.to_path_buf()),
            shell: Some(TEST_SHELL.into()),
            ..Default::default()
        })
        .unwrap();
    };
    let hooks_seen_by = |exe: &std::path::Path| {
        let report = doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            executable_path: Some(exe.to_path_buf()),
            shell: Some(TEST_SHELL.into()),
            only: vec!["install.claude-hooks".into()],
            ..Default::default()
        })
        .unwrap();
        check(&report, "install.claude-hooks").clone()
    };

    install_as(&dev);
    let taken = hooks_seen_by(&release);
    assert_eq!(taken.status, CheckStatus::Yellow, "{taken:?}");
    assert!(
        taken
            .summary
            .contains(&format!("run {}, not this pixel", dev.display())),
        "{taken:?}"
    );
    assert!(
        taken
            .fix
            .as_deref()
            .is_some_and(|f| f.starts_with("pixel install")),
        "{taken:?}"
    );
    let own = hooks_seen_by(&dev);
    assert_eq!(own.status, CheckStatus::Green, "{own:?}");

    install_as(&release);
    let back = hooks_seen_by(&release);
    assert_eq!(back.status, CheckStatus::Green, "{back:?}");
    let side = hooks_seen_by(&dev);
    assert_eq!(
        side.status,
        CheckStatus::Green,
        "a side build does not judge: {side:?}"
    );

    install_as(&old);
    let upgraded = hooks_seen_by(&release);
    assert_eq!(upgraded.status, CheckStatus::Yellow, "{upgraded:?}");
    assert!(
        upgraded.summary.contains(&old.display().to_string()),
        "{upgraded:?}"
    );
}

/// `pixel-dev uninstall` removes the entries `pixel-dev install` wrote; it
/// used to report success and leave all of them running.
#[test]
#[cfg(unix)]
fn uninstall_by_a_binary_not_named_pixel_should_remove_its_own_hooks() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let exe = fake_dev_exe(home);
    let keep = serde_json::json!({"matcher":"startup","hooks":[{"type":"command","command":"herdr hook session-start --agent claude"}]});
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(
        home.join(".claude/settings.json"),
        serde_json::to_string(&serde_json::json!({"hooks":{"SessionStart":[keep.clone()]}}))
            .unwrap(),
    )
    .unwrap();
    install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(exe.clone()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();
    uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(exe.clone()),
        executable_path: Some(exe),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();
    let settings = read_json(&home.join(".claude/settings.json"));
    assert_eq!(
        settings,
        serde_json::json!({"hooks":{"SessionStart":[keep]}}),
        "only the foreign hook is left"
    );
}

/// The guard script from before the `gitpixel` → `pixel` rename: uninstall
/// removes its settings entry and deletes the script itself.
#[test]
#[cfg(unix)]
fn uninstall_should_remove_the_pre_rename_guard_entry_and_script() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    fs::create_dir_all(home.join(".claude/hooks")).unwrap();
    let script = home.join(".claude/hooks/gitpixel-targets-guard");
    fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
    let keep = serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":"keep-security-check"}]});
    fs::write(
        home.join(".claude/settings.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {"PreToolUse": [
                keep.clone(),
                {"matcher":"Bash","hooks":[{"type":"command","command":"~/.claude/hooks/gitpixel-targets-guard"}]}
            ]}
        }))
        .unwrap(),
    )
    .unwrap();

    uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();

    assert!(!script.exists(), "the pre-rename guard script is deleted");
    let settings = read_json(&home.join(".claude/settings.json"));
    assert_eq!(settings["hooks"]["PreToolUse"], serde_json::json!([keep]));
}

/// A Pi guard extension Pixel wrote, in `.pi/extensions/` or (releases up
/// to 0.4.0) `<repo>/.pi/agent/`, is retired: doctor is red with the
/// command that removes it, `install --repo` removes it, and a file at that
/// path Pixel did not write is the user's and stays green.
#[test]
#[cfg(unix)]
fn doctor_pi_guard_should_flag_a_retired_guard_until_repo_install_removes_it() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    // A space in the path: the suggested command is pasted into a shell.
    let repo = dir.path().join("my repo");
    fs::create_dir_all(&home).unwrap();
    let pi_check = || {
        let report = doctor(&DoctorOptions {
            home: Some(home.clone()),
            repo_root: Some(repo.clone()),
            ..Default::default()
        })
        .unwrap();
        check(&report, "repo.pi-guard").clone()
    };
    for retired in [
        repo.join(".pi/agent/extensions/pixel-guard.ts"),
        repo.join(".pi/extensions/pixel-guard.ts"),
    ] {
        fs::create_dir_all(retired.parent().unwrap()).unwrap();
        fs::write(&retired, format!("// {MANAGED_BEGIN}\n")).unwrap();
        let c = pi_check();
        assert_eq!(c.status, CheckStatus::Red, "{c:?}");
        assert!(
            c.reason.as_deref().is_some_and(|r| r.contains("retired")
                && r.contains(&format!("pixel install --repo '{}'", repo.display()))),
            "{c:?}"
        );

        install(&repo_install_options(&repo, &home)).unwrap();
        let c = pi_check();
        assert_eq!(c.status, CheckStatus::Green, "{c:?}");
        assert!(!retired.exists());
    }
    assert!(!repo.join(".pi").exists(), "nothing of Pixel's is left");

    let mine = repo.join(".pi/extensions/pixel-guard.ts");
    fs::create_dir_all(mine.parent().unwrap()).unwrap();
    fs::write(&mine, "// mine\n").unwrap();
    install(&repo_install_options(&repo, &home)).unwrap();
    assert_eq!(pi_check().status, CheckStatus::Green);
    assert_eq!(fs::read_to_string(&mine).unwrap(), "// mine\n");
}

/// `REPO_ARTIFACTS` is the list the README and `--repo` help are checked
/// against, so it must name every file the install writes: a file the list
/// forgets is one the docs cannot mention and `info/exclude` never gets.
#[test]
#[cfg(unix)]
fn repo_artifacts_should_name_every_file_a_repo_install_writes() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".claude")).unwrap();
    fs::create_dir_all(repo.join(".codex")).unwrap();
    git(&repo, &["init", "-q"]);
    // Native-default migration preserves RTK in place; recovery sidecars
    // remain documented artifacts but are no longer created by a fresh install.
    fs::write(
        repo.join(".claude/settings.local.json"),
        serde_json::to_string_pretty(&serde_json::json!({"hooks": {"PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "rtk hook claude"}]}
        ]}}))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        repo.join(".codex/config.toml"),
        format!(
            "developer_instructions = '''\nKeep the user's first instruction.\n\n{MANAGED_BEGIN}\nretired Pixel instructions\n{MANAGED_END}\n\nKeep the user's last instruction.\n'''\n"
        ),
    )
    .unwrap();
    fs::write(
        repo.join("AGENTS.md"),
        "Keep the user's first project instruction.\n\n<!-- pixel:warp-retrieval:begin -->\nretired Pixel-first instructions\n<!-- pixel:warp-retrieval:end -->\n\nKeep the user's last project instruction.\n",
    )
    .unwrap();
    install(&repo_install_options(&repo, &home)).unwrap();

    let codex = fs::read_to_string(repo.join(".codex/config.toml")).unwrap();
    assert!(!codex.contains(MANAGED_BEGIN), "{codex}");
    assert!(
        codex.contains("Keep the user's first instruction."),
        "{codex}"
    );
    assert!(
        codex.contains("Keep the user's last instruction."),
        "{codex}"
    );
    let agents = fs::read_to_string(repo.join("AGENTS.md")).unwrap();
    assert!(!agents.contains("pixel:warp-retrieval:"), "{agents}");
    assert!(
        agents.contains("Keep the user's first project instruction."),
        "{agents}"
    );
    assert!(
        agents.contains("Keep the user's last project instruction."),
        "{agents}"
    );

    let mut written = Vec::new();
    let mut stack = vec![repo.clone()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path
                .strip_prefix(&repo)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if rel == ".git" || rel.contains(".pixel-bak.") {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else {
                written.push(rel);
            }
        }
    }
    written.sort();
    // Recovery sidecars, and a Codex hooks file native cleanup has nothing
    // to remove from, are documented artifacts no fresh install creates.
    let legacy_sidecars = [
        ".claude/pixel-rtk-hooks.json",
        ".codex/hooks.json",
        ".codex/pixel-composed-guard-backup.json",
    ];
    for path in legacy_sidecars {
        assert!(
            !repo.join(path).exists(),
            "native install must not create {path}"
        );
    }
    let mut listed: Vec<String> = pixel_install::install::REPO_ARTIFACTS
        .iter()
        .filter(|a| !legacy_sidecars.contains(&a.path))
        .map(|a| a.path.to_string())
        .collect();
    listed.sort();
    assert_eq!(written, listed);
}

/// Repo installation removes the retired permanent Pixel-first block and
/// leaves project-authored instructions unchanged.
#[test]
#[cfg(unix)]
fn repo_install_removes_retired_pixel_first_rules_without_rewriting_user_text() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    let before = "# Existing project rules\nPreserve this instruction.\n\n";
    let retired = "<!-- pixel:warp-retrieval:begin -->\nold Pixel-first prompt\n<!-- pixel:warp-retrieval:end -->";
    let after = "\nKeep this trailing instruction.\n";
    let original = format!("{before}{retired}{after}");
    fs::write(repo.join("AGENTS.md"), &original).unwrap();
    let doctor_options = DoctorOptions {
        home: Some(home.clone()),
        repo_root: Some(repo.clone()),
        only: vec!["repo.pixel-first".into()],
        ..Default::default()
    };

    let legacy = doctor(&doctor_options).unwrap();
    assert_eq!(check(&legacy, "repo.pixel-first").status, CheckStatus::Red);

    let installed = install(&repo_install_options(&repo, &home)).unwrap();
    assert!(installed.ok, "{installed:?}");
    let rules_path = repo.join("AGENTS.md");
    assert_eq!(
        fs::read_to_string(&rules_path).unwrap(),
        format!("{before}{after}")
    );
    let current = doctor(&doctor_options).unwrap();
    assert_eq!(
        check(&current, "repo.pixel-first").status,
        CheckStatus::Green
    );

    uninstall(&UninstallOptions {
        home: Some(home),
        repo: Some(repo),
        ..Default::default()
    })
    .unwrap();
    let remaining = fs::read_to_string(&rules_path).unwrap();
    assert_eq!(remaining, format!("{before}{after}"));
}

/// A global RTK backup with no delegating guard is a leftover (an
/// `install --repo` build once wrote the repository's there): doctor flags
/// it in yellow with the command that removes it, and stays green once the
/// file is gone.
#[test]
#[cfg(unix)]
fn doctor_should_flag_a_global_rtk_backup_no_guard_delegates_to() {
    let dir = TempDir::new().unwrap();
    // The command is pasted into a shell: an apostrophe in the path must
    // come out escaped.
    let home = dir.path().join("o'neil");
    let backup = home.join(".claude/pixel-rtk-hooks.json");
    fs::create_dir_all(backup.parent().unwrap()).unwrap();
    fs::write(
        &backup,
        r#"[{"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]}]"#,
    )
    .unwrap();
    let rtk_check = || {
        let report = doctor(&DoctorOptions {
            home: Some(home.clone()),
            ..Default::default()
        })
        .unwrap();
        check(&report, "install.rtk-backup").clone()
    };

    let c = rtk_check();
    assert_eq!(c.status, CheckStatus::Yellow, "{c:?}");
    let quoted = format!("'{}'", backup.display().to_string().replace('\'', "'\\''"));
    assert!(quoted.contains("o'\\''neil"), "{quoted}");
    assert!(c.summary.ends_with(&format!("rm {quoted}")), "{c:?}");

    fs::remove_file(&backup).unwrap();
    let c = rtk_check();
    assert_eq!(c.status, CheckStatus::Green, "{c:?}");
}

/// A global RTK hook is left alone by repo native-cleanup; no additional
/// Pixel retrieval callback is installed beside it.
#[test]
#[cfg(unix)]
fn repo_install_should_hold_back_the_guard_beside_a_global_rtk_hook() {
    for with_shared_settings in [true, false] {
        let dir = TempDir::new().unwrap();
        let home = dir.path().join("home");
        let repo = dir.path().join("repo");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(repo.join(".claude")).unwrap();
        let global = home.join(".claude/settings.json");
        let global_text =
            serde_json::to_string_pretty(&serde_json::json!({"hooks": {"PreToolUse": [
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "rtk hook claude"}]}
            ]}}))
            .unwrap();
        fs::write(&global, &global_text).unwrap();
        if with_shared_settings {
            fs::write(
                repo.join(".claude/settings.json"),
                r#"{"hooks":{"PreToolUse":[{"matcher":"Write","hooks":[{"type":"command","command":"keep-write-check"}]}]}}"#,
            )
            .unwrap();
        }

        let report = install(&repo_install_options(&repo, &home)).unwrap();

        let step = report
            .steps
            .iter()
            .find(|s| s.id == "hooks.claude")
            .unwrap();
        assert_eq!(
            step.status,
            pixel_install::install::CheckStatus::Green,
            "{step:?}"
        );
        assert!(step.summary.contains("native tools preserved"), "{step:?}");
        let local = repo.join(".claude/settings.local.json");
        let local = if local.is_file() {
            read_json(&local)
        } else {
            serde_json::json!({})
        };
        assert!(pixel_commands(&local, "PreToolUse").is_empty(), "{local}");
        assert_eq!(fs::read_to_string(&global).unwrap(), global_text);
    }
}

#[test]
#[cfg(unix)]
fn renamed_executable_preserves_existing_local_task_hooks_without_adding_repo_hooks() {
    for shared in [false, true] {
        for foreign in [false, true] {
            let dir = TempDir::new().unwrap();
            let home = dir.path().join("home");
            let repo = dir.path().join("repo");
            fs::create_dir_all(home.join(".claude")).unwrap();
            fs::create_dir_all(repo.join(".claude")).unwrap();
            git(&repo, &["init", "-q"]);
            let mut options = repo_install_options(&repo, &home);
            let exe = home.join("our-agent");
            fs::rename(options.executable_path.as_ref().unwrap(), &exe).unwrap();
            options.executable_path = Some(exe.clone());
            let task = task_hook_group(&exe, "claude", "pre-tool-use");
            let mut groups = vec![task];
            if foreign {
                groups.push(serde_json::json!({"matcher":"Bash","hooks":[{
                    "type":"command","command":"keep-security-check"
                }]}));
            }
            let inherited = if shared {
                repo.join(".claude/settings.json")
            } else {
                home.join(".claude/settings.json")
            };
            let settings = serde_json::json!({"model":"keep-model","hooks":{"PreToolUse":groups}});
            fs::write(&inherited, serde_json::to_vec(&settings).unwrap()).unwrap();
            let report = install(&options).unwrap();
            let step = report
                .steps
                .iter()
                .find(|step| step.id == "hooks.claude")
                .unwrap();
            assert_eq!(
                step.status,
                StepStatus::Green,
                "shared={shared}, foreign={foreign}: {step:?}"
            );
            assert_eq!(read_json(&inherited), settings);
            let local = read_local_hooks(&repo.join(".claude/settings.local.json"));
            assert!(pixel_commands(&local, "PreToolUse").is_empty(), "{local}");
            let report = doctor(&DoctorOptions {
                home: Some(home),
                repo_root: Some(repo),
                executable_path: Some(exe),
                only: vec!["repo.claude-hooks".into()],
                ..Default::default()
            })
            .unwrap();
            let checked = check(&report, "repo.claude-hooks");
            assert_eq!(
                checked.status,
                CheckStatus::Green,
                "shared={shared}, foreign={foreign}: {checked:?}"
            );
            if foreign {
                assert!(
                    checked.fix.is_none(),
                    "foreign hooks do not require a guard: {checked:?}"
                );
            }
        }
    }
}

/// A personal Bash hook (`/usr/local/bin/my-guard`) keeps the Claude guard
/// out, on purpose: two rewriters on one shell call race. The user must still
/// learn what to do about it, from install and afterwards from doctor, which
/// must not call the repository healthy while its sessions run unguarded,
/// nor offer a `--fix` that cannot converge: only the user can choose between
/// their hook and the guard. Narrowing the matcher, the step the message
/// names, must then be enough for the guard to go in and doctor to turn green.
#[test]
#[cfg(unix)]
fn repo_native_default_preserves_personal_bash_hooks_without_adding_a_guard() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    // The command the user pastes quotes the repository.
    let repo = dir.path().join("it's a repo");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    let global = home.join(".claude/settings.json");
    let personal = |matcher: &str| {
        serde_json::to_string_pretty(&serde_json::json!({"model": "opus", "hooks": {"PreToolUse": [
            {"matcher": matcher, "hooks": [{"type": "command", "command": "/usr/local/bin/my-guard"}]}
        ]}}))
        .unwrap()
    };
    fs::write(&global, personal("Bash")).unwrap();
    let options = repo_install_options(&repo, &home);
    let claude_hooks = || {
        let report = doctor(&DoctorOptions {
            home: Some(home.clone()),
            repo_root: Some(repo.clone()),
            executable_path: options.executable_path.clone(),
            only: vec!["repo.claude-hooks".into()],
            ..Default::default()
        })
        .unwrap();
        check(&report, "repo.claude-hooks").clone()
    };

    // A repository nobody prepared is not broken, whatever the user's hooks.
    let untouched = claude_hooks();
    assert_eq!(untouched.status, CheckStatus::Green, "{untouched:?}");

    let report = install(&options).unwrap();
    let step = report
        .steps
        .iter()
        .find(|s| s.id == "hooks.claude")
        .unwrap();
    assert_eq!(step.status, StepStatus::Green, "{step:?}");
    assert!(step.summary.contains("native tools preserved"), "{step:?}");
    assert_eq!(fs::read_to_string(&global).unwrap(), personal("Bash"));

    let held = claude_hooks();
    assert_eq!(held.status, CheckStatus::Green, "{held:?}");
    assert!(
        held.summary.contains("native Claude hooks preserved"),
        "{held:?}"
    );
    assert_eq!(held.fix, None, "no command can make this choice: {held:?}");
    assert_eq!(
        held.repair, None,
        "--fix must not rerun an install that cannot converge"
    );

    fs::write(&global, personal("Edit|Write")).unwrap();
    let report = install(&options).unwrap();
    let step = report
        .steps
        .iter()
        .find(|s| s.id == "hooks.claude")
        .unwrap();
    assert_eq!(step.status, StepStatus::Green, "{step:?}");
    let guarded = claude_hooks();
    assert_eq!(guarded.status, CheckStatus::Green, "{guarded:?}");
    assert!(
        guarded.summary.contains("native Claude hooks preserved"),
        "{guarded:?}"
    );
}

/// Existing project setup artifacts do not turn native Claude defaults into
/// an installer-managed guard requirement.
#[test]
#[cfg(unix)]
fn doctor_claude_native_defaults_ignore_retired_guard_conflict_artifacts() {
    for artifact in ["pixel-first", "codex-hooks", "devin-hooks", "pi-guard"] {
        let dir = TempDir::new().unwrap();
        let home = dir.path().join("home");
        let repo = dir.path().join("repo");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let exe = fake_pixel_exe(&home);
        let global = home.join(".claude/settings.json");
        fs::write(
            &global,
            serde_json::to_vec(&serde_json::json!({"hooks": {"PreToolUse": [
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "/usr/local/bin/my-guard"}]}
            ]}}))
            .unwrap(),
        )
        .unwrap();

        match artifact {
            "pixel-first" => fs::write(
                repo.join("AGENTS.md"),
                "<!-- pixel:warp-retrieval:begin -->\nPixel retrieval\n<!-- pixel:warp-retrieval:end -->\n",
            )
            .unwrap(),
            "codex-hooks" => {
                let path = repo.join(".codex/hooks.json");
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(
                    path,
                    serde_json::to_vec(&serde_json::json!({"hooks": {
                        "PreToolUse": [task_hook_group(&exe, "codex", "pre-tool-use")]
                    }}))
                    .unwrap(),
                )
                .unwrap();
            }
            "devin-hooks" => {
                let path = repo.join(".devin/config.local.json");
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(
                    path,
                    serde_json::to_vec(&serde_json::json!({"hooks": {
                        "PreToolUse": [{"hooks":[{
                            "type":"command",
                            "command":format!("{} run-hook guard --provider devin", exe.display())
                        }]}]
                    }}))
                    .unwrap(),
                )
                .unwrap();
            }
            "pi-guard" => {
                let path = repo.join(".pi/extensions/pixel-guard.ts");
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, "export {};\n").unwrap();
            }
            _ => unreachable!("fixture list is exhaustive"),
        }

        let report = doctor(&DoctorOptions {
            home: Some(home),
            repo_root: Some(repo),
            executable_path: Some(exe),
            only: vec!["repo.claude-hooks".into()],
            ..Default::default()
        })
        .unwrap();
        let check = check(&report, "repo.claude-hooks");
        assert_eq!(check.status, CheckStatus::Green, "{artifact}: {check:?}");
        assert!(
            check.fix.is_none(),
            "native defaults need no guard fix: {check:?}"
        );
    }
}

/// An unreadable global settings file must not block the repo install: the
/// guard goes in and the step says what was not checked.
#[test]
#[cfg(unix)]
fn repo_install_should_install_the_guard_when_the_global_settings_is_unreadable() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(&repo).unwrap();
    fs::write(home.join(".claude/settings.json"), "{ not json").unwrap();

    let report = install(&repo_install_options(&repo, &home)).unwrap();

    let step = report
        .steps
        .iter()
        .find(|s| s.id == "hooks.claude")
        .unwrap();
    assert_eq!(
        step.status,
        pixel_install::install::CheckStatus::Green,
        "{step:?}"
    );
    assert!(step.summary.contains("native tools preserved"), "{step:?}");
    let local = read_local_hooks(&repo.join(".claude/settings.local.json"));
    assert!(pixel_commands(&local, "PreToolUse").is_empty(), "{local}");
}

/// A repository at `$HOME` has the global file as its shared one. The stale
/// guard it holds is taken out of it, so a dry run must not count it again as
/// a global rewriter and predict a held-back guard the real run installs.
/// The repository is named through a symlink: the two paths differ as
/// written and only resolve to the same file.
#[test]
#[cfg(unix)]
fn repo_install_at_home_should_read_the_global_file_once() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("home-link");
    fs::create_dir_all(home.join(".claude")).unwrap();
    std::os::unix::fs::symlink(&home, &repo).unwrap();
    fs::write(
        home.join(".claude/settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"'/old/pixel' run-hook guard --provider claude","timeout":10}]}]}}"#,
    )
    .unwrap();
    let global_before = fs::read(home.join(".claude/settings.json")).unwrap();
    for dry_run in [true, false] {
        let mut options = repo_install_options(&repo, &home);
        options.dry_run = dry_run;
        let report = install(&options).unwrap();
        let step = report
            .steps
            .iter()
            .find(|s| s.id == "hooks.claude")
            .unwrap();
        assert_eq!(
            step.status,
            pixel_install::install::CheckStatus::Green,
            "dry_run={dry_run}: {step:?}"
        );
        if dry_run {
            assert_eq!(
                fs::read(home.join(".claude/settings.json")).unwrap(),
                global_before,
                "a dry run writes nothing, the global file included"
            );
        }
    }
    // The global file is this repository's shared one: the stale guard is
    // removed there, with no repo-local replacement.
    let global = read_json(&home.join(".claude/settings.json"));
    assert!(pixel_commands(&global, "PreToolUse").is_empty(), "{global}");
    let local = read_local_hooks(&home.join(".claude/settings.local.json"));
    assert!(pixel_commands(&local, "PreToolUse").is_empty(), "{local}");
}

/// A repo install leaves an older global Pixel hook to the global install;
/// it does not add a second repository callback.
#[test]
#[cfg(unix)]
fn repo_install_should_not_duplicate_a_global_pixel_callback() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        home.join(".claude/settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"'/old/pixel' run-hook guard --provider claude","timeout":10}]}]}}"#,
    )
    .unwrap();

    let global_path = home.join(".claude/settings.json");
    let global_before = fs::read(&global_path).unwrap();
    let report = install(&repo_install_options(&repo, &home)).unwrap();

    let step = report
        .steps
        .iter()
        .find(|s| s.id == "hooks.claude")
        .unwrap();
    assert_eq!(
        step.status,
        pixel_install::install::CheckStatus::Green,
        "{step:?}"
    );
    assert!(step.summary.contains("native tools preserved"), "{step:?}");
    assert_eq!(fs::read(global_path).unwrap(), global_before);
    assert!(
        pixel_commands(
            &read_local_hooks(&repo.join(".claude/settings.local.json")),
            "PreToolUse"
        )
        .is_empty()
    );
}

/// Doctor reports a healthy native-default repo even when global hooks are
/// present; it makes no claim that a repo retrieval callback is active.
#[test]
#[cfg(unix)]
fn doctor_repo_claude_hooks_should_ignore_global_retrieval_callbacks() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    // A space in the path: the suggested command is pasted into a shell.
    let repo = dir.path().join("my repo");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(&repo).unwrap();
    install(&repo_install_options(&repo, &home)).unwrap();
    fs::write(
        home.join(".claude/settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]}]}}"#,
    )
    .unwrap();
    let claude_check = || {
        let report = doctor(&DoctorOptions {
            home: Some(home.clone()),
            repo_root: Some(repo.clone()),
            ..Default::default()
        })
        .unwrap();
        check(&report, "repo.claude-hooks").clone()
    };

    let c = claude_check();
    assert_eq!(c.status, CheckStatus::Green, "{c:?}");
    assert!(c.summary.contains("native Claude hooks preserved"), "{c:?}");

    install(&repo_install_options(&repo, &home)).unwrap();
    let c = claude_check();
    assert_eq!(c.status, CheckStatus::Green, "{c:?}");
    assert!(c.fix.is_none(), "native-default is a valid state: {c:?}");
}

/// Read the `PostToolUse` groups whose command runs Pixel's metrics relay
/// for `provider`, as `(matcher, command, timeout)`.
fn metrics_relays(value: &serde_json::Value, provider: &str) -> Vec<(String, String, u64)> {
    let verb = format!("run-hook metrics --provider {provider}");
    let mut found = Vec::new();
    for group in value["hooks"]["PostToolUse"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let matcher = group["matcher"].as_str().unwrap_or("");
        for hook in group["hooks"].as_array().into_iter().flatten() {
            let command = hook["command"].as_str().unwrap_or("");
            if command.contains(&verb) {
                let timeout = hook["timeout"].as_u64().unwrap_or(0);
                found.push((matcher.to_string(), command.to_string(), timeout));
            }
        }
    }
    found
}

/// Repeated global installs register every synchronous task-event boundary
/// once and preserve unrelated PostToolUse hooks without a metrics callback.
#[test]
#[cfg(unix)]
fn claude_install_should_register_one_task_event_suite_and_keep_foreign_groups() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let exe = fake_pixel_exe(home);
    fs::create_dir_all(home.join(".claude")).unwrap();
    let foreign = serde_json::json!({"matcher":"Write","hooks":[{"type":"command","command":"fmt-on-write.sh"}]});
    fs::write(
        home.join(".claude/settings.json"),
        serde_json::to_string_pretty(
            &serde_json::json!({"hooks":{"PostToolUse":[foreign.clone()]}}),
        )
        .unwrap(),
    )
    .unwrap();
    for _ in 0..3 {
        install(&InstallOptions {
            home: Some(home.to_path_buf()),
            executable_path: Some(exe.clone()),
            shell: Some(TEST_SHELL.into()),
            ..Default::default()
        })
        .unwrap();
    }
    let settings = read_json(&home.join(".claude/settings.json"));
    assert_eq!(task_event_counts(&settings), ONE_EACH, "{settings}");
    let groups = settings["hooks"]["PostToolUse"].as_array().unwrap();
    assert!(groups.contains(&foreign), "foreign group kept: {settings}");
    assert!(
        metrics_relays(&settings, "claude").is_empty(),
        "the native-default global hooks have no automatic metrics callback: {settings}"
    );
}

/// A missing task-event boundary is red with the install fix and green once
/// the complete global task suite is restored.
#[test]
#[cfg(unix)]
fn doctor_should_flag_a_claude_install_missing_a_task_event_hook() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let exe = fake_pixel_exe(home);
    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(exe.clone()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    };
    install(&options).unwrap();
    let path = home.join(".claude/settings.json");
    let mut settings = read_json(&path);
    settings["hooks"]["SessionEnd"]
        .as_array_mut()
        .unwrap()
        .clear();
    fs::write(&path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();
    let doctor_hooks = || {
        let report = doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            executable_path: Some(exe.clone()),
            shell: Some(TEST_SHELL.into()),
            only: vec!["install.claude-hooks".into()],
            ..Default::default()
        })
        .unwrap();
        check(&report, "install.claude-hooks").clone()
    };
    let stale = doctor_hooks();
    assert_eq!(stale.status, CheckStatus::Red, "{stale:?}");
    assert!(
        stale
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("task lifecycle gates")),
        "the missing task-event suite is named: {stale:?}"
    );
    assert!(
        stale
            .fix
            .as_deref()
            .is_some_and(|f| f.starts_with("pixel install")),
        "{stale:?}"
    );
    install(&options).unwrap();
    let fixed = doctor_hooks();
    assert_eq!(fixed.status, CheckStatus::Green, "{fixed:?}");
}

/// Devin keeps its native tools in a repository too: `install --repo`
/// writes no Devin hook, and the guard and metrics relay an earlier release
/// registered in `.devin/config.local.json` (and the legacy
/// `.devin/hooks.json`) are removed while a foreign group survives. Doctor
/// is red on the leftover with the quoted repo fix, green after; a
/// reinstall is byte-identical, and a repository without `.devin/` gains
/// none.
#[test]
#[cfg(unix)]
fn devin_repo_install_should_remove_the_retired_guard_and_metrics_relay() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    // A space and an apostrophe: the fix is pasted into a shell.
    let repo = dir.path().join("a 'repo'");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".devin")).unwrap();
    let exe = fake_pixel_exe(&home);
    let quoted = format!("'{}'", exe.display());
    let foreign = serde_json::json!({"hooks":[{"type":"command","command":"vibe-island-bridge --source devin"}]});
    let config = repo.join(".devin/config.local.json");
    fs::write(
        &config,
        serde_json::to_string_pretty(&serde_json::json!({"hooks":{
            "PreToolUse":[{"matcher":"exec","hooks":[{"type":"command","command":format!("{quoted} run-hook guard --provider devin")}]}],
            "PostToolUse":[
                foreign.clone(),
                {"matcher":"exec","hooks":[{"type":"command","command":format!("{quoted} run-hook metrics --provider devin"),"timeout":10}]},
            ],
        }}))
        .unwrap(),
    )
    .unwrap();
    let legacy = repo.join(".devin/hooks.json");
    fs::write(
        &legacy,
        serde_json::json!({"hooks":{"PreToolUse":[{"matcher":"exec","hooks":[{"type":"command","command":"/opt/old/pixel run-hook guard --provider devin"}]}]}}).to_string(),
    )
    .unwrap();
    let doctor_options = DoctorOptions {
        home: Some(home.clone()),
        executable_path: Some(exe.clone()),
        repo_root: Some(repo.clone()),
        only: vec!["repo.devin-hooks".into()],
        ..Default::default()
    };
    let red = doctor(&doctor_options).unwrap();
    let devin = check(&red, "repo.devin-hooks");
    assert_eq!(devin.status, CheckStatus::Red, "{devin:?}");
    let fix = format!(
        "pixel install --repo '{}'",
        repo.display().to_string().replace('\'', "'\\''")
    );
    assert_eq!(
        devin.reason.as_deref(),
        Some(
            format!(
                "retired Pixel hooks remain in {}, {} — run `{fix}` to remove them",
                config.display(),
                legacy.display()
            )
            .as_str()
        )
    );
    assert_eq!(devin.fix.as_deref(), Some(fix.as_str()));

    let options = repo_install_options(&repo, &home);
    install(&options).unwrap();
    assert_eq!(
        read_json(&config),
        serde_json::json!({"hooks":{"PostToolUse":[foreign]}}),
        "the guard and the relay are gone, the foreign group stays"
    );
    assert_eq!(read_json(&legacy), serde_json::json!({}));
    let green = doctor(&doctor_options).unwrap();
    assert_eq!(check(&green, "repo.devin-hooks").status, CheckStatus::Green);

    let first = fs::read(&config).unwrap();
    let report = install(&options).unwrap();
    assert_eq!(
        fs::read(&config).unwrap(),
        first,
        "reinstall is byte-identical"
    );
    let step = report.steps.iter().find(|s| s.id == "hooks.devin").unwrap();
    assert_eq!(step.summary, "removed 0 Devin hook entry/entries");

    let bare = dir.path().join("bare");
    fs::create_dir_all(&bare).unwrap();
    install(&repo_install_options(&bare, &home)).unwrap();
    assert!(!bare.join(".devin").exists(), "no Devin config is created");
}

/// Uninstall removes global task hooks while preserving foreign PostToolUse
/// groups; a repo-local Devin config holding only a foreign group is never
/// touched by install or uninstall.
#[test]
#[cfg(unix)]
fn uninstall_should_remove_claude_task_hooks_and_leave_a_foreign_devin_config() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".devin")).unwrap();
    fs::create_dir_all(home.join(".claude")).unwrap();
    let foreign = serde_json::json!({"matcher":"Write","hooks":[{"type":"command","command":"fmt-on-write.sh"}]});
    let original = serde_json::to_string_pretty(
        &serde_json::json!({"hooks":{"PostToolUse":[foreign.clone()]}}),
    )
    .unwrap();
    let devin = repo.join(".devin/config.local.json");
    for path in [&home.join(".claude/settings.json"), &devin] {
        fs::write(path, &original).unwrap();
    }
    let exe = fake_pixel_exe(&home);
    install(&InstallOptions {
        home: Some(home.clone()),
        executable_path: Some(exe.clone()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();
    install(&repo_install_options(&repo, &home)).unwrap();
    assert_eq!(
        task_event_counts(&read_json(&home.join(".claude/settings.json"))),
        ONE_EACH
    );
    assert_eq!(fs::read_to_string(&devin).unwrap(), original);

    for repo in [Some(repo.clone()), None] {
        uninstall(&UninstallOptions {
            home: Some(home.clone()),
            repo,
            binary_path: Some(exe.clone()),
            executable_path: Some(exe.clone()),
            shell: Some(TEST_SHELL.into()),
            ..Default::default()
        })
        .unwrap();
    }
    let value = read_json(&home.join(".claude/settings.json"));
    assert!(pixel_commands(&value, "PostToolUse").is_empty(), "{value}");
    assert_eq!(
        value["hooks"]["PostToolUse"],
        serde_json::json!([foreign]),
        "{value}"
    );
    assert_eq!(fs::read_to_string(&devin).unwrap(), original);
}

/// A repository at `$HOME`: the global task suite remains singular, repo
/// cleanup adds no duplicate callbacks, and no Devin config appears, global
/// or repo-local.
#[test]
#[cfg(unix)]
fn repo_install_at_home_should_keep_global_task_hooks_singular() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let exe = fake_pixel_exe(&home);
    install(&InstallOptions {
        home: Some(home.clone()),
        executable_path: Some(exe.clone()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();
    for _ in 0..2 {
        install(&repo_install_options(&home, &home)).unwrap();
    }
    let global = read_json(&home.join(".claude/settings.json"));
    assert_eq!(task_event_counts(&global), ONE_EACH, "{global}");
    let local = read_local_hooks(&home.join(".claude/settings.local.json"));
    assert!(pixel_commands(&local, "PreToolUse").is_empty(), "{local}");
    assert!(!home.join(".devin").exists());
    assert!(!home.join(".config/devin").exists());
}

/// A `pixel mcp` entry an older release wrote into Warp's config points Warp
/// at a server Pixel no longer ships: doctor must name it, `install --repo`
/// must take it out without touching what the user put there, and a config
/// Git tracks, which install never edits, is reported without a false fix.
#[test]
#[cfg(unix)]
fn repo_install_should_retire_the_warp_mcp_entry_older_releases_wrote() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".warp")).unwrap();
    git(&repo, &["init", "-q"]);
    let root = repo.canonicalize().unwrap();
    let config = repo.join(".warp/.mcp.json");
    let legacy = serde_json::json!({
        "command": "/old/pixel",
        "args": ["mcp", root],
        "working_directory": root,
    });
    let doctor_options = DoctorOptions {
        home: Some(home.clone()),
        repo_root: Some(repo.clone()),
        only: vec!["repo.warp-mcp".into()],
        ..Default::default()
    };

    let foreign = serde_json::json!({"mcpServers": {"pixel": {
        "command": "/old/pixel", "args": ["mcp", "/other/repo"], "working_directory": "/other/repo",
    }}});
    fs::write(&config, foreign.to_string()).unwrap();
    let report = doctor(&doctor_options).unwrap();
    assert_eq!(
        check(&report, "repo.warp-mcp").status,
        CheckStatus::Green,
        "an entry for another repository is not this install's leftover"
    );

    fs::write(
        &config,
        serde_json::json!({"mcpServers": {"pixel": legacy, "lint": {"command": "lint"}}})
            .to_string(),
    )
    .unwrap();
    let report = doctor(&doctor_options).unwrap();
    let red = check(&report, "repo.warp-mcp");
    assert_eq!(red.status, CheckStatus::Red, "{red:?}");
    assert!(
        red.reason
            .as_deref()
            .is_some_and(|r| r.contains("retired MCP server")),
        "{red:?}"
    );

    install(&repo_install_options(&repo, &home)).unwrap();

    let kept: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(
        kept,
        serde_json::json!({"mcpServers": {"lint": {"command": "lint"}}})
    );
    let report = doctor(&doctor_options).unwrap();
    assert_eq!(check(&report, "repo.warp-mcp").status, CheckStatus::Green);

    fs::write(
        &config,
        serde_json::json!({"mcpServers": {"pixel": legacy}}).to_string(),
    )
    .unwrap();
    git(&repo, &["add", "--", ".warp/.mcp.json"]);
    let before = fs::read(&config).unwrap();
    install(&repo_install_options(&repo, &home)).unwrap();
    assert_eq!(fs::read(&config).unwrap(), before);
    let report = doctor(&doctor_options).unwrap();
    let tracked = check(&report, "repo.warp-mcp");
    assert_eq!(tracked.status, CheckStatus::Yellow, "{tracked:?}");
    assert!(
        tracked.summary.contains("tracked by git"),
        "{}",
        tracked.summary
    );

    git(&repo, &["rm", "-q", "--cached", "--", ".warp/.mcp.json"]);
    install(&repo_install_options(&repo, &home)).unwrap();
    assert!(
        !config.exists(),
        "a config that held only Pixel's entry is Pixel's file, and goes"
    );
    let report = doctor(&doctor_options).unwrap();
    assert_eq!(check(&report, "repo.warp-mcp").status, CheckStatus::Green);
}

/// A Devin hook group running `command`, in Devin's Claude-style schema.
fn devin_group(command: &str) -> serde_json::Value {
    serde_json::json!({"hooks":[{"command":command,"type":"command"}]})
}

/// Devin keeps its native retrieval: the global install writes no Devin
/// hook, and the lifecycle hooks an earlier release registered in
/// `~/.config/devin/config.json` are removed while the user's settings and
/// foreign hooks stay. Doctor is red on the leftover and green after. A
/// machine that never ran Devin gains no Devin config.
#[test]
#[cfg(unix)]
fn global_install_removes_the_devin_lifecycle_hooks_an_earlier_release_wrote() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let exe = fake_pixel_exe(home);
    let devin_config = home.join(".config/devin/config.json");
    fs::create_dir_all(devin_config.parent().unwrap()).unwrap();
    let foreign = devin_group("/opt/foreign --source devin");
    let old = "'/opt/old/pixel'";
    fs::write(
        &devin_config,
        serde_json::to_string_pretty(&serde_json::json!({
            "agent": {"model": "swe-2-medium"},
            "hooks": {
                "SessionStart": [foreign.clone(), devin_group(&format!("{old} run-hook session-start --provider devin"))],
                "UserPromptSubmit": [devin_group(&format!("{old} run-hook prompt-submit --provider devin"))],
                "PostCompaction": [devin_group(&format!("{old} run-hook post-compaction"))],
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let options = InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(exe.clone()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    };
    let devin_check = || {
        let report = doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            executable_path: Some(exe.clone()),
            shell: Some(TEST_SHELL.into()),
            only: vec!["install.devin-hooks".into()],
            ..Default::default()
        })
        .unwrap();
        check(&report, "install.devin-hooks").clone()
    };

    let red = devin_check();
    assert_eq!(red.status, CheckStatus::Red, "{red:?}");
    assert_eq!(
        red.reason.as_deref(),
        Some(
            format!(
                "retired Pixel hooks remain in {} — run `pixel install` to remove them",
                devin_config.display()
            )
            .as_str()
        )
    );
    assert_eq!(
        red.fix.as_deref(),
        Some(format!("pixel install --shell {TEST_SHELL}").as_str())
    );

    install(&options).unwrap();
    assert_eq!(
        read_json(&devin_config),
        serde_json::json!({
            "agent": {"model": "swe-2-medium"},
            "hooks": {"SessionStart": [foreign]},
        }),
        "only Pixel's hooks are removed"
    );
    assert_eq!(devin_check().status, CheckStatus::Green);

    // Idempotent: a second install rewrites nothing and reports nothing.
    let before = fs::read(&devin_config).unwrap();
    let report = install(&options).unwrap();
    assert_eq!(fs::read(&devin_config).unwrap(), before);
    let step = report.steps.iter().find(|s| s.id == "hooks.devin").unwrap();
    assert_eq!(step.summary, "removed 0 Devin hook entry/entries");

    // Without a Devin config dir, the install creates none and runs no Devin
    // step: a machine that never ran Devin must not gain a config for it.
    let devinless = TempDir::new().unwrap();
    let report = install(&InstallOptions {
        repo: None,
        home: Some(devinless.path().to_path_buf()),
        executable_path: Some(fake_pixel_exe(devinless.path())),
        claude_executable: Some(fake_claude_exe(devinless.path(), CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .unwrap();
    assert!(report.steps.iter().all(|s| s.id != "hooks.devin"));
    assert!(
        !devinless.path().join(".config/devin").exists(),
        "no Devin config on a machine without Devin"
    );
}

/// The `install.devin-hooks` check judges only what Pixel wrote: a machine
/// without Devin is green-absent, a Devin config with foreign hook groups
/// naming Pixel's verbs is green (they are not Pixel's entries), one with a
/// hook Pixel's binary runs is red, and a real install turns it green again
/// with the foreign groups kept.
#[test]
fn doctor_devin_hooks_judge_only_what_pixel_wrote() {
    let options = |home: &std::path::Path| DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        shell: Some(TEST_SHELL.into()),
        only: vec!["install.devin-hooks".into()],
        ..Default::default()
    };
    let status = |home: &std::path::Path| {
        let report = doctor(&options(home)).unwrap();
        check(&report, "install.devin-hooks").status
    };

    // No Devin on the machine: green-absent.
    let dir = TempDir::new().unwrap();
    assert_eq!(status(dir.path()), CheckStatus::Green);

    // Foreign groups that name the exact verbs without Pixel's binary.
    let dir = TempDir::new().unwrap();
    let config = dir.path().join(".config/devin/config.json");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let foreign = serde_json::json!({
        "SessionStart": [devin_group("/opt/foreign run-hook session-start --provider devin")],
        "UserPromptSubmit": [devin_group("/opt/foreign run-hook prompt-submit --provider devin")],
        "PostCompaction": [devin_group("/opt/foreign run-hook post-compaction")],
    });
    fs::write(&config, serde_json::json!({"hooks": foreign}).to_string()).unwrap();
    assert_eq!(status(dir.path()), CheckStatus::Green);

    // One hook Pixel's binary runs: red.
    let mut hooks = foreign.clone();
    hooks["SessionStart"]
        .as_array_mut()
        .unwrap()
        .push(devin_group(
            "/usr/local/bin/pixel run-hook session-start --provider devin",
        ));
    fs::write(&config, serde_json::json!({"hooks": hooks}).to_string()).unwrap();
    assert_eq!(status(dir.path()), CheckStatus::Red);

    install(&InstallOptions {
        repo: None,
        home: Some(dir.path().to_path_buf()),
        executable_path: Some(fake_pixel_exe(dir.path())),
        claude_executable: Some(fake_claude_exe(dir.path(), CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .unwrap();
    assert_eq!(status(dir.path()), CheckStatus::Green);
    assert_eq!(read_json(&config), serde_json::json!({"hooks": foreign}));
}

/// Doctor options that run the one Codex hook-review check under `home`.
fn hook_review_options(home: &Path, exe: &Path, id: &str, repo: Option<&Path>) -> DoctorOptions {
    DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(exe.to_path_buf()),
        repo_root: repo.map(Path::to_path_buf),
        only: vec![id.into()],
        ..Default::default()
    }
}

/// Doctor reports missing, stale or disabled approval and turns green only
/// when the exact normalized hook identity has a current enabled review.
#[test]
fn doctor_reports_codex_hooks_codex_has_not_reviewed() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let exe = fake_pixel_exe(home);
    install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(exe.clone()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("install");
    let hooks = home.join(".codex/hooks.json");
    let config = home.join(".codex/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "# Codex settings owned by the test\n").unwrap();
    let options = hook_review_options(home, &exe, "install.codex-hook-review", None);

    let report = doctor(&options).unwrap();
    let unreviewed = check(&report, "install.codex-hook-review");
    assert_eq!(unreviewed.status, CheckStatus::Yellow, "{unreviewed:?}");
    assert_eq!(
        unreviewed.summary,
        format!(
            "approval for 9 of the 9 Pixel hook(s) in {} is missing, stale, disabled, or not verifiable (Interrupt #0.0, PostToolUse #0.0, PreToolUse #0.0, SessionEnd #0.0, SessionStart #0.0, Stop #0.0, SubagentStart #0.0, SubagentStop #0.0, UserPromptSubmit #0.0): \
             start `codex` in this directory and inspect `/hooks`",
            hooks.display()
        )
    );
    assert_eq!(
        unreviewed.fix, None,
        "no command can review a hook for the user"
    );

    // A review recorded for another file, another event, or an entry without
    // a hash, is not this hook's review.
    let base = fs::read_to_string(&config).unwrap();
    let review = |key: &str, entry: &str| {
        fs::write(
            &config,
            format!("{base}\n[hooks.state.\"{key}\"]\n{entry}\n"),
        )
        .unwrap();
        doctor(&options).unwrap()
    };
    for (key, entry) in [
        (
            format!(
                "{}:post_tool_use:0:0",
                home.join("other/hooks.json").display()
            ),
            "trusted_hash = \"sha256:x\"",
        ),
        (
            format!("{}:post_tool_use:0:0", hooks.display()),
            "enabled = true",
        ),
        (
            format!("{}:pre_tool_use:0:0", hooks.display()),
            "trusted_hash = \"sha256:x\"",
        ),
        (
            format!("{}:post_tool_use:0:1", hooks.display()),
            "trusted_hash = \"sha256:x\"",
        ),
    ] {
        let report = review(&key, entry);
        assert_eq!(
            check(&report, "install.codex-hook-review").status,
            CheckStatus::Yellow,
            "{key} {entry}"
        );
    }

    let report = review(
        &format!("{}:post_tool_use:0:0", hooks.display()),
        "trusted_hash = \"sha256:x\"",
    );
    assert_eq!(
        check(&report, "install.codex-hook-review").status,
        CheckStatus::Yellow,
        "reviewing metrics alone leaves task gates unreviewed"
    );
    let mut reviews = base.clone();
    for event in [
        "interrupt",
        "post_tool_use",
        "pre_tool_use",
        "session_end",
        "session_start",
        "stop",
        "subagent_start",
        "subagent_stop",
        "user_prompt_submit",
    ] {
        let key = format!("{}:{event}:0:0", hooks.display());
        reviews.push_str(&format!(
            "\n[hooks.state.{key:?}]\nenabled = true\ntrusted_hash = \"sha256:stale\"\n"
        ));
    }
    fs::write(&config, reviews).unwrap();
    let report = doctor(&options).unwrap();
    let stale = check(&report, "install.codex-hook-review");
    assert_eq!(stale.status, CheckStatus::Yellow, "{stale:?}");
    assert_eq!(
        stale.detail.as_ref().unwrap()["unreviewed"]
            .as_array()
            .unwrap()
            .len(),
        9,
        "stale hashes must not make the global task hooks green"
    );
}

/// The project guard `install --repo` writes is a Codex hook like any other:
/// counted per handler, foreign hooks beside it never counted, and green only
/// once every Pixel handler is reviewed. No hooks file is not a finding.
#[test]
fn doctor_reports_unreviewed_project_codex_hooks_and_ignores_foreign_ones() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(repo.join(".codex")).unwrap();
    fs::create_dir_all(home.join(".codex")).unwrap();
    let exe = fake_pixel_exe(&home);
    let options = hook_review_options(&home, &exe, "repo.codex-hook-review", Some(&repo));

    let report = doctor(&options).unwrap();
    let absent = check(&report, "repo.codex-hook-review");
    assert_eq!(absent.status, CheckStatus::Green, "{absent:?}");
    assert_eq!(
        absent.summary,
        format!(
            "no Pixel hook for Codex in {}",
            repo.join(".codex/hooks.json").display()
        )
    );

    let hooks = repo.join(".codex/hooks.json");
    let pixel = |verb: &str| {
        serde_json::json!({
            "type": "command",
            "command": format!("'{}' run-hook {verb} --provider codex", exe.display()),
        })
    };
    fs::write(
        &hooks,
        serde_json::json!({"hooks": {
            "PreToolUse": [{"matcher": "Bash", "hooks": [
                {"type": "command", "command": "/usr/local/bin/lint-guard"},
                pixel("composed-guard"),
            ]}],
            "SessionStart": [{"hooks": [pixel("session-start")]}],
        }})
        .to_string(),
    )
    .unwrap();
    let report = doctor(&options).unwrap();
    let unreviewed = check(&report, "repo.codex-hook-review");
    assert_eq!(unreviewed.status, CheckStatus::Yellow, "{unreviewed:?}");
    assert_eq!(
        unreviewed.detail.as_ref().unwrap()["unreviewed"],
        serde_json::json!(["PreToolUse #0.1", "SessionStart #0.0"]),
        "the foreign lint hook is not Pixel's to report"
    );

    fs::write(
        home.join(".codex/config.toml"),
        format!(
            "[hooks.state.\"{}:pre_tool_use:0:1\"]\nenabled = true\ntrusted_hash = \"sha256:stale\"\n\n[hooks.state.\"{}:session_start:0:0\"]\nenabled = true\ntrusted_hash = \"sha256:stale\"\n",
            hooks.display(),
            hooks.display()
        ),
    )
    .unwrap();
    let report = doctor(&options).unwrap();
    let stale = check(&report, "repo.codex-hook-review");
    assert_eq!(stale.status, CheckStatus::Yellow, "{stale:?}");
    assert_eq!(
        stale.detail.as_ref().unwrap()["unreviewed"],
        serde_json::json!(["PreToolUse #0.1", "SessionStart #0.0"]),
        "stale hashes must not make a project hook green"
    );
}
