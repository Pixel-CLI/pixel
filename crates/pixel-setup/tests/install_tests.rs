// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Integration tests for pixel-setup: doctor and install.

use std::fs;
use std::path::Path;

use pixel_setup::config::{MANAGED_BEGIN, MANAGED_END};
use pixel_setup::doctor::{CHECKS, CheckStatus, DoctorOptions, doctor};
use pixel_setup::install::{
    CheckStatus as StepStatus, InstallOptions, InstallReport, InstallSelection, InstallStep,
    install, install_selected,
};
use pixel_setup::setup::AgentTarget;
use pixel_setup::uninstall::{UninstallOptions, uninstall};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// test fixture: a fake "pixel" executable.
//
// pixel is a CLI + hooks tool, not an MCP server — `pixel setup` no longer
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

fn install_home(home: &std::path::Path) {
    install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    })
    .expect("install");
}

fn uninstall_home(home: &std::path::Path) {
    uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
        ..Default::default()
    })
    .expect("uninstall");
}

fn options_for(home: &Path) -> InstallOptions {
    InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    }
}

fn task_hook_group(exe: &Path, provider: &str, event: &str) -> serde_json::Value {
    task_hook_group_with(exe, provider, event, "")
}

/// Parts the prompt brief is split over by default, for Claude and Codex.
const BRIEF_PARTS: usize = 3;

/// The hook entries a global install writes for one task boundary: the
/// prompt brief is `BRIEF_PARTS` entries (`--part K/N`) for Claude and
/// Codex, every other boundary one.
fn task_hook_groups(exe: &Path, provider: &str, event: &str) -> Vec<serde_json::Value> {
    if event == "prompt-submit" && matches!(provider, "claude" | "codex") {
        (1..=BRIEF_PARTS)
            .map(|part| {
                task_hook_group_with(
                    exe,
                    provider,
                    event,
                    &format!(" --part {part}/{BRIEF_PARTS}"),
                )
            })
            .collect()
    } else {
        vec![task_hook_group(exe, provider, event)]
    }
}

fn task_hook_group_with(exe: &Path, provider: &str, event: &str, flags: &str) -> serde_json::Value {
    let executable = exe
        .canonicalize()
        .unwrap()
        .display()
        .to_string()
        .replace('\'', "'\\''");
    let mut group = serde_json::json!({"hooks":[{
        "type":"command",
        "command":format!("'{executable}' run-hook task-event --provider {provider} --event {event}{flags}{}",
            if event == "prompt-submit" { " || true" } else { "" }),
        "timeout":if matches!(event, "session-end" | "interrupt") { 3 } else { 10 }
    }]});
    // Devin shows no hook output; `show_output` prints that the brief hook ran.
    if provider == "devin" && event == "prompt-submit" {
        group["hooks"][0]["show_output"] = true.into();
    }
    group
}

/// Check every synchronous task boundary exactly once before comparing the
/// pre-existing lifecycle/foreign hooks independently of the new task hooks.
fn without_task_hooks(value: &serde_json::Value, provider: &str, exe: &Path) -> serde_json::Value {
    let mut remaining = value.clone();
    let mut events = vec![
        ("SessionStart", "session-start"),
        ("UserPromptSubmit", "prompt-submit"),
        ("PreToolUse", "pre-tool-use"),
        ("PostToolUse", "post-tool-use"),
        ("Stop", "stop"),
        ("SessionEnd", "session-end"),
        ("SubagentStart", "subagent-start"),
        ("SubagentStop", "subagent-stop"),
    ];
    match provider {
        "claude" => events.push(("PostToolUseFailure", "tool-failure")),
        // Devin rejects a `hooks` object naming an event it does not accept,
        // so install writes no Subagent*/Interrupt key there.
        "devin" => events.retain(|(event, _)| !event.starts_with("Subagent")),
        _ => events.push(("Interrupt", "interrupt")),
    }
    for (event, name) in events {
        let groups = remaining["hooks"][event].as_array_mut().unwrap();
        for expected in task_hook_groups(exe, provider, name) {
            assert_eq!(
                groups.iter().filter(|group| **group == expected).count(),
                1,
                "{event}: {groups:?}"
            );
            groups.retain(|group| group != &expected);
        }
        // Codex also gets the merged PostToolUse metrics relay, which
        // re-emits `pixel`'s metrics line after a shell command.
        if provider == "codex" && event == "PostToolUse" {
            let before = groups.len();
            groups.retain(|group| {
                !group
                    .to_string()
                    .contains("run-hook metrics --provider codex")
            });
            assert_eq!(before - groups.len(), 1, "{event}: {groups:?}");
        }
        if groups.is_empty() {
            remaining["hooks"].as_object_mut().unwrap().remove(event);
        }
    }
    // The PostToolUse metrics relay is installed beside the task hooks.
    if let Some(groups) = remaining["hooks"]
        .get_mut("PostToolUse")
        .and_then(serde_json::Value::as_array_mut)
    {
        groups.retain(|group| {
            !group["hooks"].as_array().is_some_and(|hooks| {
                hooks.iter().any(|hook| {
                    hook["command"]
                        .as_str()
                        .is_some_and(|command| command.contains("run-hook metrics"))
                })
            })
        });
        if groups.is_empty() {
            remaining["hooks"]
                .as_object_mut()
                .unwrap()
                .remove("PostToolUse");
        }
    }
    remaining
}

fn shell_profile_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".zshrc")
}

/// The prompt files releases before the native default deployed under
/// `~/.local/share/pixel/`; no host reads them any more.
const RETIRED_PROMPTS: [&str; 2] = [
    ".local/share/pixel/agent-prompt.md",
    ".local/share/pixel/subagent-prompt.md",
];

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
        dry_run: false,
    })
    .unwrap();
    assert_no_retired_prompt(home);

    // Claude receives task accounting and the dedicated PostToolUse metrics
    // relay. Retrieval prompts, advice and PreToolUse rewriting stay out.
    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(home.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        without_task_hooks(&settings, "claude", &exe)["hooks"],
        serde_json::json!({}),
        "Claude global hooks contain no automatic retrieval callbacks: {settings}"
    );
    assert_eq!(metrics_relays(&settings, "claude").len(), 1, "{settings}");
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
        matches!(&err, pixel_setup::InstallError::UnknownDoctorCheck(id) if id == "install.shell-wrappers"),
        "{err}"
    );
}

/// The fix travels with the finding: a red install check names `pixel
/// setup`, a red repo check names its repository, a green check none.
#[test]
fn doctor_should_attach_a_fix_to_failing_checks_only() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    // No Claude settings in the home make the install check red.
    let home = dir.path().join("home");
    let report = doctor(&DoctorOptions {
        home: Some(home),
        repo_root: Some(repo.clone()),
        only: ["binary.path", "install.claude-hooks", "index.freshness"]
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
    let hooks = check(&report, "install.claude-hooks");
    assert_eq!(hooks.status, CheckStatus::Red);
    assert_eq!(hooks.fix.as_deref(), Some("pixel setup"));
    assert_eq!(hooks.repair, Some(vec![vec!["setup".to_owned()]]));
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
        dry_run: false,
    };
    let report = install(&options).expect("install");

    assert!(report.ok, "install should succeed (ok=true)");
    assert!(report.summary.red == 0, "no red steps");
    assert!(report.summary.green > 0, "should have green steps");

    // The new install does NOT rewrite agent-config files — a stale managed
    // block from a previous install is left untouched (its cleanup is the
    // user's job, or `pixel setup --remove`). Original content survives verbatim.
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
        dry_run: false,
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
        dry_run: false,
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
        dry_run: true,
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

/// Antigravity doctor requires its prompt brief and verifies the plugin
/// metrics relay.
#[test]
#[cfg(unix)]
fn doctor_should_require_the_antigravity_brief_and_plugin() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let config = home.join(".gemini/config");
    fs::create_dir_all(&config).unwrap();
    let hooks_path = config.join("hooks.json");
    let foreign = serde_json::json!({
        "enabled": true,
        "PostToolUse": [{"matcher":"run_command", "hooks":[{"type":"command", "command":"audit"}]}]
    });
    fs::write(
        &hooks_path,
        serde_json::to_string_pretty(&serde_json::json!({"user-audit": foreign.clone()})).unwrap(),
    )
    .unwrap();
    let exe = fake_pixel_exe(home);
    let doctor_hooks = || {
        let report = doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            executable_path: Some(exe.clone()),
            only: vec!["install.antigravity".into()],
            ..Default::default()
        })
        .unwrap();
        check(&report, "install.antigravity").clone()
    };
    let missing = doctor_hooks();
    assert_eq!(missing.status, CheckStatus::Red, "{missing:?}");
    assert!(
        missing
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("pixel-brief")),
        "doctor names the missing prompt brief: {missing:?}"
    );

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(exe.clone()),
        ..Default::default()
    };
    install(&options).unwrap();
    let installed = read_json(&hooks_path);
    assert_eq!(installed["user-audit"], foreign);
    assert!(installed.get("pixel-metrics").is_none());
    assert_eq!(doctor_hooks().status, CheckStatus::Green);
    assert!(
        doctor_hooks()
            .summary
            .contains("deferred metrics relay configured")
    );

    install(&options).unwrap();
    assert_eq!(doctor_hooks().status, CheckStatus::Green);
    assert_eq!(read_json(&hooks_path)["user-audit"], foreign);
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
        dry_run: false,
    };
    install(&real_options).expect("real install");

    let settings_path = home.join(".claude").join("settings.json");
    let before_settings = fs::read(&settings_path).unwrap();

    // A dry-run install afterwards must not touch anything, even though a
    // real install already exists (idempotent no-op path).
    let dry_options = InstallOptions {
        repo: None,
        dry_run: true,
        ..real_options
    };
    let report = install(&dry_options).expect("dry-run install over existing state");
    assert!(report.dry_run);

    let after_settings = fs::read(&settings_path).unwrap();
    assert_eq!(
        before_settings, after_settings,
        "dry-run must not modify settings.json"
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
fn install_on_a_fresh_home_creates_claude_md_even_with_no_pre_existing_file() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    // Deliberately do NOT pre-create CLAUDE.md or AGENTS.md.
    let options = InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: None,
        dry_run: false,
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
    // Doctor verifies the install artifacts: install.claude-hooks (the
    // task-event hooks). This test walks it through its red and green states.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // The doctor runs as the binary that installs below, as it does in
    // production: hooks wired to another binary are yellow on their own.
    let doc_opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        ..Default::default()
    };

    // 1. With nothing installed, claude-hooks is red.
    let report = doctor(&doc_opts).expect("doctor runs");
    let hooks_check = check(&report, "install.claude-hooks");
    assert_eq!(
        hooks_check.status,
        pixel_setup::doctor::CheckStatus::Red,
        "claude-hooks should be red when not installed"
    );

    // 2. Run install: claude task hooks go in, so the check is green.
    install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    })
    .expect("install");

    let report = doctor(&doc_opts).expect("doctor runs");
    let hooks_check = check(&report, "install.claude-hooks");
    assert_eq!(
        hooks_check.status,
        pixel_setup::doctor::CheckStatus::Green,
        "claude-hooks should be green after install: {:?}",
        hooks_check.reason
    );
}

fn check<'a>(
    report: &'a pixel_setup::doctor::DoctorReport,
    id: &str,
) -> &'a pixel_setup::doctor::DoctorCheck {
    report
        .checks
        .iter()
        .find(|c| c.id == id)
        .unwrap_or_else(|| panic!("doctor has no {id} check"))
}

// ---------------------------------------------------------------------------
// uninstall tests
// ---------------------------------------------------------------------------

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
        ..Default::default()
    })
    .expect("uninstall");

    assert!(!bin.exists());
    assert_eq!(binary_step(&report).summary, "removed pixel binary");
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
    fs::write(&config, "{}").unwrap();
    let opts = InstallOptions {
        repo: None,
        home: Some(home.clone()),
        executable_path: Some(exe.clone()),
        dry_run: false,
    };
    // Every provider gets only synchronous task-event hooks. No automatic
    // retrieval callbacks or hook scripts are deployed globally.
    install(&opts).unwrap();
    let first = fs::read(&config).unwrap();
    install(&opts).unwrap();
    assert_eq!(
        fs::read(&config).unwrap(),
        first,
        "repeat install must leave the provider config stable"
    );
    let installed: serde_json::Value = serde_json::from_slice(&first).unwrap();
    let value = without_task_hooks(&installed, &provider, &exe);
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
            assert_eq!(value["hooks"], serde_json::json!({}), "{installed}");
            let command = pixel_commands(&installed, "UserPromptSubmit")[0].clone();
            assert!(command.contains("task-event --provider devin"), "{command}");
            assert!(
                command.starts_with('\'')
                    && command.contains("directory/pixel' run-hook")
                    && command.contains("'\\''"),
                "the executable path must survive spaces and quotes: {command}"
            );
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
// retired prompt files: `agent-prompt.md` and `subagent-prompt.md`
//
// Earlier releases deployed both under `~/.local/share/pixel/` for the
// retired `claude` wrapper and the explicit integrations. No host reads them
// any more: `pixel setup` removes them, a dry run only announces it, and
// nothing else Pixel keeps in that directory is touched.
// ---------------------------------------------------------------------------

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
    install_home(home);

    assert_eq!(codex_developer_instructions(home), None);
    let written = fs::read_to_string(codex_config_path(home)).unwrap();
    assert_eq!(written, USER_CODEX_CONFIG);

    install_home(home);
    assert_eq!(
        fs::read_to_string(codex_config_path(home)).unwrap(),
        written,
        "a re-install must be byte-for-byte idempotent"
    );
}

// ---------------------------------------------------------------------------
// Pi's APPEND_SYSTEM.md is auto-loaded, so upgrades remove retired Pixel
// prompt text while preserving user instructions. Explicit impact is a Pi
// package under ~/.local/share/pixel/pi-package that Pi's settings declare.
// ---------------------------------------------------------------------------

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
    pixel_setup::ClassifyPiPackage::with_agent_dir(home, None, "export default () => {};\n")
        .expect("Pi is configured")
        .install()
        .unwrap();
    let package = home.join(pixel_setup::CLASSIFY_PACKAGE_DIR);
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
        dry_run: false,
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

// ---------------------------------------------------------------------------
// repo-local install tests (`pixel setup --repo <path>`)
// ---------------------------------------------------------------------------

fn repo_install_options(repo: &std::path::Path, home: &std::path::Path) -> InstallOptions {
    InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
        repo: Some(repo.to_path_buf()),
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

/// One suite: every boundary once, the prompt brief in its default parts.
const ONE_EACH: [(&str, usize); 9] = [
    ("session-start", 1),
    ("prompt-submit", BRIEF_PARTS),
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

/// Going back from a dev build to the release: `pixel setup` after
/// `pixel-dev setup` must replace the dev entries, not add its own beside
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
    let edit =
        vec![serde_json::json!({"matcher":"Bash","hooks":[pixel("metrics --provider claude")]})];
    for copy in 0..3 {
        let mut start = vec![pixel("task-event --provider claude --event session-start")];
        if copy == 0 {
            start.push(notify.clone());
        }
        session.push(serde_json::json!({"hooks": start}));
        prompt.push(serde_json::json!({"hooks":[pixel("task-event --provider claude --event prompt-submit")]}));
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
            .extend(task_hook_groups(&exe, "claude", name));
    }
    fs::write(&path, serde_json::to_string_pretty(&stacked).unwrap()).unwrap();
    let doctor_hooks = || {
        let report = doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            executable_path: Some(exe.clone()),
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

/// After a global `pixel-dev setup` every hook is present and registered
/// once, yet every session on the machine runs the side build: the managed
/// `pixel` reads that as yellow with `pixel setup` as its fix, and the
/// same holds for hooks left on a previous release's path by an upgrade.
/// `pixel setup` points them back; the side build itself never judges.
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
            ..Default::default()
        })
        .unwrap();
    };
    let hooks_seen_by = |exe: &std::path::Path| {
        let report = doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            executable_path: Some(exe.to_path_buf()),
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
            .is_some_and(|f| f.starts_with("pixel setup")),
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

/// `pixel-dev setup --remove` removes the entries `pixel-dev setup` wrote; it
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
        ..Default::default()
    })
    .unwrap();
    uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(exe.clone()),
        executable_path: Some(exe),
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

/// Repeated global installs register each Claude task boundary and metrics
/// relay once, preserving unrelated PostToolUse hooks.
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
            ..Default::default()
        })
        .unwrap();
    }
    let settings = read_json(&home.join(".claude/settings.json"));
    assert_eq!(task_event_counts(&settings), ONE_EACH, "{settings}");
    let groups = settings["hooks"]["PostToolUse"].as_array().unwrap();
    assert!(groups.contains(&foreign), "foreign group kept: {settings}");
    assert!(
        metrics_relays(&settings, "claude").len() == 1,
        "the native-default Claude hooks include one metrics callback: {settings}"
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
            .is_some_and(|f| f.starts_with("pixel setup")),
        "{stale:?}"
    );
    install(&options).unwrap();
    let fixed = doctor_hooks();
    assert_eq!(fixed.status, CheckStatus::Green, "{fixed:?}");
}

/// Claude doctor requires one correctly shaped PostToolUse metrics relay;
/// setup repairs a missing or malformed registration without disturbing its
/// other hooks.
#[test]
#[cfg(unix)]
fn doctor_should_flag_a_missing_or_malformed_claude_metrics_relay() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let exe = fake_pixel_exe(home);
    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(exe.clone()),
        ..Default::default()
    };
    install(&options).unwrap();
    let path = home.join(".claude/settings.json");
    let doctor_hooks = || {
        let report = doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            executable_path: Some(exe.clone()),
            only: vec!["install.claude-hooks".into()],
            ..Default::default()
        })
        .unwrap();
        check(&report, "install.claude-hooks").clone()
    };

    let mut settings = read_json(&path);
    settings["hooks"]["PostToolUse"]
        .as_array_mut()
        .unwrap()
        .retain(|group| {
            !group["hooks"].as_array().is_some_and(|hooks| {
                hooks.iter().any(|hook| {
                    hook["command"].as_str().is_some_and(|command| {
                        command.contains("run-hook metrics --provider claude")
                    })
                })
            })
        });
    fs::write(&path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();
    let missing = doctor_hooks();
    assert_eq!(missing.status, CheckStatus::Red, "{missing:?}");
    assert!(
        missing
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("PostToolUse metrics relay")),
        "doctor names the missing relay: {missing:?}"
    );

    install(&options).unwrap();
    let mut settings = read_json(&path);
    let relay = settings["hooks"]["PostToolUse"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|group| {
            group["hooks"].as_array().is_some_and(|hooks| {
                hooks.iter().any(|hook| {
                    hook["command"].as_str().is_some_and(|command| {
                        command.contains("run-hook metrics --provider claude")
                    })
                })
            })
        })
        .unwrap();
    relay["matcher"] = "Read".into();
    fs::write(&path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();
    let malformed = doctor_hooks();
    assert_eq!(malformed.status, CheckStatus::Red, "{malformed:?}");

    install(&options).unwrap();
    let mut settings = read_json(&path);
    let relays = metrics_relays(&settings, "claude");
    assert_eq!(relays.len(), 1);
    let group = settings["hooks"]["PostToolUse"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| {
            group["hooks"].as_array().is_some_and(|hooks| {
                hooks.iter().any(|hook| {
                    hook["command"].as_str().is_some_and(|command| {
                        command.contains("run-hook metrics --provider claude")
                    })
                })
            })
        })
        .unwrap()
        .clone();
    settings["hooks"]["PostToolUse"]
        .as_array_mut()
        .unwrap()
        .push(group);
    fs::write(&path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();
    let duplicate = doctor_hooks();
    assert_eq!(duplicate.status, CheckStatus::Red, "{duplicate:?}");

    install(&options).unwrap();
    let fixed = doctor_hooks();
    assert_eq!(fixed.status, CheckStatus::Green, "{fixed:?}");
    assert_eq!(metrics_relays(&read_json(&path), "claude").len(), 1);
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
        dry_run: false,
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
            "approval for 12 of the 12 Pixel hook(s) in {} is missing, stale, disabled, or not verifiable (Interrupt #0.0, PostToolUse #0.0, PostToolUse #1.0, PreToolUse #0.0, SessionEnd #0.0, SessionStart #0.0, Stop #0.0, SubagentStart #0.0, SubagentStop #0.0, UserPromptSubmit #0.0, UserPromptSubmit #1.0, UserPromptSubmit #2.0): \
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
        12,
        "stale hashes must not make the global task hooks green"
    );
}

/// The `UserPromptSubmit` commands of a Claude settings file, in order.
fn prompt_submit_commands(settings: &serde_json::Value) -> Vec<String> {
    pixel_commands(settings, "UserPromptSubmit")
}

fn install_claude_in(home: &Path, exe: &Path) {
    install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(exe.to_path_buf()),
        ..Default::default()
    })
    .unwrap();
}

/// `brief_parts` in the global config decides how many hooks carry the
/// brief, for both hosts that cap an injected context; reinstalling after a
/// change replaces the set instead of stacking a second one.
#[test]
#[cfg(unix)]
fn install_should_write_as_many_brief_parts_as_the_config_says() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let exe = fake_pixel_exe(home);
    let claude = |parts: usize| -> Vec<String> {
        let stem = "run-hook task-event --provider claude --event prompt-submit";
        if parts == 1 {
            return vec![format!(
                "'{}' {stem} || true",
                exe.canonicalize().unwrap().display()
            )];
        }
        (1..=parts)
            .map(|part| {
                format!(
                    "'{}' {stem} --part {part}/{parts} || true",
                    exe.canonicalize().unwrap().display()
                )
            })
            .collect()
    };
    let settings = home.join(".claude/settings.json");
    let codex = home.join(".codex/hooks.json");

    install_claude_in(home, &exe);
    assert_eq!(prompt_submit_commands(&read_json(&settings)), claude(3));
    assert_eq!(prompt_submit_commands(&read_json(&codex)).len(), 3);

    for parts in [1usize, 2, 5, 3] {
        fs::create_dir_all(home.join(".pixel")).unwrap();
        fs::write(
            home.join(".pixel/config.yaml"),
            format!("brief_parts: {parts}\n"),
        )
        .unwrap();
        install_claude_in(home, &exe);
        let claude_settings = read_json(&settings);
        assert_eq!(
            prompt_submit_commands(&claude_settings),
            claude(parts),
            "claude, {parts}"
        );
        assert_eq!(
            prompt_submit_commands(&read_json(&codex)).len(),
            parts,
            "codex, {parts}"
        );
        assert_eq!(
            task_event_counts(&claude_settings)[1],
            ("prompt-submit", parts)
        );
    }
}

/// Uninstall takes every part out, whatever their count, and leaves a
/// foreign `UserPromptSubmit` hook in place.
#[test]
#[cfg(unix)]
fn uninstall_should_remove_every_brief_part_and_keep_a_foreign_prompt_hook() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    fs::create_dir_all(home.join(".claude")).unwrap();
    let foreign = serde_json::json!({"hooks":[{"type":"command","command":"my-prompt-hook"}]});
    fs::write(
        home.join(".claude/settings.json"),
        serde_json::json!({"hooks":{"UserPromptSubmit":[foreign.clone()]}}).to_string(),
    )
    .unwrap();
    let exe = fake_pixel_exe(home);
    install_claude_in(home, &exe);
    let before = read_json(&home.join(".claude/settings.json"));
    assert_eq!(
        before["hooks"]["UserPromptSubmit"]
            .as_array()
            .unwrap()
            .len(),
        4
    );

    uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(exe.clone()),
        executable_path: Some(exe),
        ..Default::default()
    })
    .unwrap();
    let after = read_json(&home.join(".claude/settings.json"));
    assert_eq!(
        after["hooks"]["UserPromptSubmit"],
        serde_json::json!([foreign])
    );
    assert!(
        !fs::read_to_string(home.join(".codex/hooks.json"))
            .unwrap_or_default()
            .contains("--part"),
        "no Codex part left"
    );
}

/// Doctor names the part that went missing from the set, and a set that is
/// complete but not the size the config asks for is yellow, not red.
#[test]
#[cfg(unix)]
fn doctor_should_flag_a_missing_brief_part_and_a_set_the_config_no_longer_asks_for() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let exe = fake_pixel_exe(home);
    install_claude_in(home, &exe);
    let settings = home.join(".claude/settings.json");
    let run = || {
        let report = doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            executable_path: Some(exe.clone()),
            only: vec!["install.claude-hooks".into()],
            ..Default::default()
        })
        .unwrap();
        check(&report, "install.claude-hooks").clone()
    };
    let green = run();
    assert_eq!(green.status, CheckStatus::Green, "{green:?}");

    let complete = read_json(&settings);
    let mut gap = complete.clone();
    gap["hooks"]["UserPromptSubmit"]
        .as_array_mut()
        .unwrap()
        .remove(1);
    fs::write(&settings, gap.to_string()).unwrap();
    let missing = run();
    assert_eq!(missing.status, CheckStatus::Red, "{missing:?}");
    assert!(
        missing
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("brief part 2/3 missing")),
        "{missing:?}"
    );

    let mut last = complete.clone();
    last["hooks"]["UserPromptSubmit"]
        .as_array_mut()
        .unwrap()
        .pop();
    fs::write(&settings, last.to_string()).unwrap();
    let missing = run();
    assert_eq!(missing.status, CheckStatus::Red, "{missing:?}");
    assert!(
        missing
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("brief part 3/3 missing")),
        "{missing:?}"
    );

    // Complete again, but the config now asks for two parts.
    fs::write(&settings, complete.to_string()).unwrap();
    fs::create_dir_all(home.join(".pixel")).unwrap();
    fs::write(home.join(".pixel/config.yaml"), "brief_parts: 2\n").unwrap();
    let drift = run();
    assert_eq!(drift.status, CheckStatus::Yellow, "{drift:?}");
    assert!(
        drift.summary.contains("registered in 3 hook(s)")
            && drift.summary.contains("`brief_parts` is 2"),
        "{drift:?}"
    );
    install_claude_in(home, &exe);
    let fixed = run();
    assert_eq!(fixed.status, CheckStatus::Green, "{fixed:?}");
}

/// Claude Code blocks the user's prompt when a `UserPromptSubmit` hook exits
/// 2, and a `pixel` older than `--part` exits 2 on the unknown flag: every
/// prompt command the install writes must exit 0 whatever the binary does.
/// The binary here is a stand-in that exits 2 with a usage error.
#[test]
#[cfg(unix)]
fn prompt_hook_commands_exit_zero_even_when_the_binary_exits_two() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let exe = home.join("pixel");
    fs::write(
        &exe,
        "#!/bin/sh\necho \"error: unexpected argument '--part' found\" >&2\nexit 2\n",
    )
    .unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    install_claude_in(home, &exe);

    let mut seen = 0;
    for file in [".claude/settings.json", ".codex/hooks.json"] {
        let commands = prompt_submit_commands(&read_json(&home.join(file)));
        assert_eq!(commands.len(), 3, "{file}");
        for command in commands {
            // Both hosts hand the command to a shell; so does this test.
            let out = std::process::Command::new("sh")
                .args(["-c", &command])
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(0), "{command}");
            assert!(out.stdout.is_empty(), "{command}");
            // The same command without the tail is what blocked the prompt.
            let bare = command.strip_suffix(" || true").unwrap();
            let blocked = std::process::Command::new("sh")
                .args(["-c", bare])
                .output()
                .unwrap();
            assert_eq!(blocked.status.code(), Some(2), "{bare}");
            seen += 1;
        }
    }
    assert_eq!(seen, 6);
}

/// Entries an earlier install wrote (no tail) are the same hooks to install:
/// an upgrade replaces them.
#[test]
#[cfg(unix)]
fn install_should_replace_prompt_entries_without_the_fail_open_tail() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let exe = fake_pixel_exe(home);
    install_claude_in(home, &exe);
    let path = home.join(".claude/settings.json");
    let mut old = read_json(&path);
    let mut group = task_hook_group(&exe, "claude", "prompt-submit");
    let bare = group["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .strip_suffix(" || true")
        .unwrap()
        .to_owned();
    group["hooks"][0]["command"] = bare.into();
    old["hooks"]["UserPromptSubmit"] = serde_json::json!([group]);
    fs::write(&path, old.to_string()).unwrap();
    install_claude_in(home, &exe);
    let after = prompt_submit_commands(&read_json(&path));
    assert_eq!(after.len(), 3, "{after:?}");
    assert!(after.iter().all(|c| c.ends_with(" || true")), "{after:?}");
}

// ---------------------------------------------------------------------------
// `install_selected`: what a guided `pixel setup` run picked
// ---------------------------------------------------------------------------

fn step_ids(report: &InstallReport) -> Vec<&str> {
    report.steps.iter().map(|step| step.id.as_str()).collect()
}

#[test]
fn no_selection_is_the_unattended_install() {
    let plain = TempDir::new().expect("tempdir");
    let none = TempDir::new().expect("tempdir");
    for home in [plain.path(), none.path()] {
        fs::create_dir_all(home.join(".codex")).unwrap();
    }

    let a = install(&options_for(plain.path())).expect("install");
    let b = install_selected(&options_for(none.path()), None).expect("install_selected");

    assert_eq!(
        step_ids(&a),
        step_ids(&b),
        "the same steps, in the same order"
    );
    // The hook commands name the fake executable inside each home.
    let settings = |home: &Path| {
        fs::read_to_string(home.join(".claude/settings.json"))
            .unwrap()
            .replace(home.to_str().unwrap(), "<home>")
    };
    assert_eq!(settings(plain.path()), settings(none.path()));
}

#[test]
fn a_selection_deploys_only_the_agents_and_hook_groups_it_names() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(home.join(".codex")).unwrap();

    let report = install_selected(
        &options_for(home),
        Some(&InstallSelection {
            agents: vec![AgentTarget::Codex],
            task_hooks: true,
            codex_hooks: true,
            pi_extension: true,
        }),
    )
    .expect("install_selected");

    assert!(
        fs::read_to_string(home.join(".codex/hooks.json"))
            .unwrap()
            .contains("task-event --provider codex"),
        "Codex was picked with its hook group"
    );
    assert!(
        !home.join(".claude/settings.json").exists(),
        "Claude Code was not picked: its settings are not written"
    );
    assert!(
        !step_ids(&report).contains(&"hooks.claude"),
        "{:?}",
        step_ids(&report)
    );
    assert!(
        !step_ids(&report).contains(&"hooks.pi-impact"),
        "pi was not picked, whatever the hook group says: {:?}",
        step_ids(&report)
    );
}

#[test]
fn a_picked_agent_without_its_hook_group_gets_no_hook() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(home.join(".codex")).unwrap();

    install_selected(
        &options_for(home),
        Some(&InstallSelection {
            agents: vec![AgentTarget::ClaudeCode, AgentTarget::Codex],
            task_hooks: false,
            codex_hooks: false,
            pi_extension: false,
        }),
    )
    .expect("install_selected");

    assert!(!home.join(".claude/settings.json").exists());
    assert!(!home.join(".codex/hooks.json").exists());
}

#[test]
fn a_selected_dry_run_reports_the_steps_and_writes_nothing() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".claude")).unwrap();
    let selection = InstallSelection {
        agents: vec![AgentTarget::ClaudeCode],
        task_hooks: true,
        codex_hooks: false,
        pi_extension: false,
    };

    let preview = install_selected(
        &InstallOptions {
            dry_run: true,
            ..options_for(home)
        },
        Some(&selection),
    )
    .expect("dry run");
    assert!(preview.dry_run);
    assert!(step_ids(&preview).contains(&"hooks.claude"));
    assert!(
        !home.join(".claude/settings.json").exists(),
        "a preview writes no hook"
    );

    let applied = install_selected(&options_for(home), Some(&selection)).expect("apply");
    assert_eq!(
        step_ids(&preview),
        step_ids(&applied),
        "the review shows the steps the apply runs"
    );
}
