//! Integration tests for pixel-install: doctor and install.

use std::fs;
use std::path::Path;

use pixel_install::config::{MANAGED_BEGIN, MANAGED_END};
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

// The install wires the Claude lifecycle hooks into ~/.claude/settings.json
// (the SessionStart hook injects the prompt into every Claude process) and
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

/// This proves instruction delivery and stream preservation, not model obedience.
#[test]
#[cfg(unix)]
fn installed_metrics_guidance_reaches_wrapped_agents_without_rewriting_streams() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .unwrap();
    let prompt = fs::read_to_string(home.join(".local/share/pixel/agent-prompt.md")).unwrap();
    for required in [
        "## LIVE OPERATION METRICS",
        "tool-call result",
        "exact line once",
        "never a global latest",
        // A panel already in the tool-call result is treated as relayed;
        // the model must not echo it as a separate message.
        "already in the tool-call result",
        "already relayed by the host",
        "PIXEL_METRICS=0",
        "PIXEL_METRICS_ROUND_TRIP_MS",
        "sequential-v1",
        "default `round_trip_ms` is 2000",
        "search-compat",
        "Do not invent",
    ] {
        assert!(
            prompt.contains(required),
            "missing relay contract: {required}"
        );
    }

    // Codex reads the prompt from config.toml itself: the value the file
    // carries is what its developer message gets.
    let codex_value = codex_developer_instructions(home).expect("developer_instructions written");
    assert!(
        codex_value.contains("## LIVE OPERATION METRICS"),
        "the relay contract must reach codex through config.toml"
    );
    // Claude gets the doctrine through the SessionStart hook, which injects
    // the deployed prompt itself — every `claude` process, not only shells
    // launched through the retired wrapper.
    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(home.join(".claude/settings.json")).unwrap())
            .unwrap();
    let session_hooks = settings["hooks"]["SessionStart"].as_array().unwrap();
    assert!(
        session_hooks.iter().any(|g| {
            g["hooks"].as_array().is_some_and(|h| {
                h.iter().any(|hook| {
                    hook["command"]
                        .as_str()
                        .is_some_and(|c| c.contains("run-hook session-start"))
                })
            })
        }),
        "the SessionStart hook must be registered: {settings}"
    );
    // And the file that hook injects is the verified prompt above.
    assert!(
        prompt.contains("## LIVE OPERATION METRICS"),
        "the SessionStart-injected prompt carries the relay contract"
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
        syntax_validator: Some(|_| Ok(())),
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
        syntax_validator: Some(|_| Ok(())),
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
    let report = doctor(&DoctorOptions {
        home: Some(dir.path().join("home")),
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
    // The lifecycle hooks + agent prompt are the install artifacts; no
    // shell wrapper is written anymore.
    assert!(
        home.join(".local/share/pixel/agent-prompt.md").is_file(),
        "agent-prompt.md should be deployed"
    );
    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(home.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert!(
        settings["hooks"]["SessionStart"]
            .as_array()
            .is_some_and(|g| !g.is_empty()),
        "claude lifecycle hooks should be installed: {settings}"
    );
    assert!(
        !shell_profile_path(home).exists(),
        "no shell wrapper is installed — SessionStart injects the prompt"
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

    // The install deploys the agent prompt and the Claude lifecycle hooks.
    // Both must be stable across re-installs.
    let prompt = home.join(".local/share/pixel/agent-prompt.md");
    let p1 = fs::read(&prompt).expect("agent-prompt deployed");
    let settings = home.join(".claude/settings.json");
    let s1 = fs::read(&settings).expect("claude settings installed");

    install(&options).expect("install 3");
    assert_eq!(
        fs::read(&prompt).unwrap(),
        p1,
        "agent-prompt must be byte-identical across re-installs"
    );
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
fn install_leaves_codex_config_untouched() {
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

    // Install adds the metrics relay and synchronous task lifecycle gates, and
    // leaves every foreign hook and unrelated key untouched. Stale pixel
    // guards are still `pixel uninstall`'s job, not install's.
    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&codex_path).unwrap()).unwrap();
    let post_tool_use = after["hooks"]["PostToolUse"]
        .as_array()
        .expect("metrics hook registered under PostToolUse");
    assert!(
        post_tool_use.iter().any(|entry| {
            entry["hooks"].as_array().is_some_and(|hooks| {
                hooks.iter().any(|hook| {
                    hook["command"]
                        .as_str()
                        .is_some_and(|c| c.contains("run-hook metrics"))
                })
            })
        }),
        "the metrics relay must be registered: {after}"
    );
    assert_eq!(
        without_task_hooks(&after, "codex", options.executable_path.as_ref().unwrap())["hooks"]["PreToolUse"],
        original["hooks"]["PreToolUse"],
        "foreign hooks must pass through untouched"
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

#[test]
fn install_deploys_copilot_hooks_when_copilot_config_is_present() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    // A machine that has run Copilot CLI has ~/.copilot; pixel install then
    // deploys its dedicated hooks file next to it.
    fs::create_dir_all(home.join(".copilot")).unwrap();

    install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("install");

    let hooks = home.join(".copilot").join("hooks").join("pixel.json");
    let doc: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&hooks).unwrap()).unwrap();
    assert_eq!(doc["_pixel_managed"], "pixel-managed-copilot-hooks-v1");
    assert!(
        doc["hooks"]["preToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| {
                e["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| a.as_str() == Some("guard"))
            }),
        "guard entry must be deployed: {doc}"
    );
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
    let prompt_path = home.join(".local/share/pixel/agent-prompt.md");
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
        "dry-run must not modify agent-prompt.md"
    );
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

    // The new install does NOT create or rewrite any CLAUDE.md/AGENTS.md —
    // it deploys the agent system prompt and shell wrappers instead.
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

    // The agent system prompt is deployed.
    let prompt_path = home.join(".local/share/pixel/agent-prompt.md");
    let prompt = fs::read_to_string(&prompt_path)
        .expect("agent-prompt.md should be deployed on a fresh home");
    assert!(
        prompt.contains("## Retrieval commands"),
        "agent-prompt.md should carry the retrieval commands"
    );

    // Claude lifecycle hooks are installed in ~/.claude/settings.json; no
    // shell profile is created or touched.
    let settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(home.join(".claude/settings.json"))
            .expect("claude settings.json should be created on a fresh home"),
    )
    .unwrap();
    for (event, verb) in [
        ("SessionStart", "session-start"),
        ("UserPromptSubmit", "prompt-submit"),
    ] {
        let groups = settings["hooks"][event].as_array().unwrap();
        assert!(
            groups.iter().any(|g| {
                g["hooks"].as_array().is_some_and(|h| {
                    h.iter().any(|hook| {
                        hook["command"]
                            .as_str()
                            .is_some_and(|c| c.contains(&format!("run-hook {verb}")))
                    })
                })
            }),
            "missing {event} → run-hook {verb}: {settings}"
        );
    }
    // Task gates are global; the retrieval rewriter stays repo-local.
    let legacy = without_task_hooks(&settings, "claude", &std::env::current_exe().unwrap());
    assert!(
        legacy["hooks"].get("PreToolUse").is_none(),
        "global install must not add a retrieval rewriter: {settings}"
    );
    assert!(
        !shell_profile_path(home).exists(),
        "no shell wrapper is written — the SessionStart hook injects the prompt"
    );
    assert!(
        codex_developer_instructions(home).is_some_and(|v| v.contains("## Retrieval commands")),
        "a fresh install must write the agent prompt into ~/.codex/config.toml"
    );
}

// ---------------------------------------------------------------------------
// doctor install-artifact check tests (agent-prompt, claude-hooks,
// legacy-wrappers)
// ---------------------------------------------------------------------------

#[test]
fn doctor_install_artifact_checks_red_and_green() {
    // Doctor verifies the install artifacts: install.agent-prompt,
    // install.claude-hooks (the SessionStart prompt injection), and
    // install.legacy-wrappers (no stale `claude()` block survives). This test
    // walks each through its red and green states.
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

    // 1. With nothing installed, agent-prompt and claude-hooks are red;
    //    legacy-wrappers is green (there is nothing to remove).
    let report = doctor(&doc_opts).expect("doctor runs");
    let prompt_check = report
        .checks
        .iter()
        .find(|c| c.id == "install.agent-prompt")
        .unwrap();
    assert_eq!(
        prompt_check.status,
        pixel_install::doctor::CheckStatus::Red,
        "agent-prompt should be red when not deployed"
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

    // 2. Run install: deploys agent-prompt + claude lifecycle hooks → all green.
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

    // 3. A prompt edited after deployment is stale, even though it still
    //    carries the two headline sections the old substring heuristic looked
    //    for: the check asserts byte equality with the bundled asset.
    let prompt_path = home.join(".local/share/pixel/agent-prompt.md");
    let mut edited = fs::read_to_string(&prompt_path).expect("agent-prompt deployed");
    assert!(
        edited.contains("# Pixel — deterministic repository facts")
            && edited.contains("## Retrieval commands"),
        "fixture: the edited prompt must still satisfy the old heuristic"
    );
    edited.push_str("\nOne extra rule the bundled prompt does not carry.\n");
    fs::write(&prompt_path, &edited).unwrap();
    let report = doctor(&doc_opts).expect("doctor runs");
    let prompt_check = report
        .checks
        .iter()
        .find(|c| c.id == "install.agent-prompt")
        .unwrap();
    assert_eq!(
        prompt_check.status,
        pixel_install::doctor::CheckStatus::Red,
        "agent-prompt should be red when the deployed prompt is stale"
    );
}

/// Dry-run "parser" standing in for the CLI's clap definition: rejects the
/// one subcommand the tests plant, accepts everything else.
fn stub_validator(argv: &[String]) -> Result<(), String> {
    if argv.iter().any(|a| a == "bogus-subcommand") {
        Err("unrecognized subcommand 'bogus-subcommand'".into())
    } else {
        Ok(())
    }
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

/// 0.2.x installs write no managed block: the rule text agents receive is
/// the deployed `agent-prompt.md`, so that is what `rule.parity` and
/// `rule.scenarios` must validate. Before this, both reported yellow "no
/// installed rule text" on every current install, and a prompt documenting
/// a command line the binary rejects went unnoticed.
#[test]
fn doctor_rule_checks_validate_the_deployed_agent_prompt() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let doc_opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        shell: Some(TEST_SHELL.into()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        syntax_validator: Some(stub_validator),
        ..Default::default()
    };

    // 1. Nothing installed: no rule text anywhere → yellow, pointing at install.
    let report = doctor(&doc_opts).expect("doctor runs");
    for id in ["rule.parity", "rule.scenarios"] {
        let c = check(&report, id);
        assert_eq!(
            c.status,
            pixel_install::doctor::CheckStatus::Yellow,
            "{id}: {c:?}"
        );
        assert!(
            c.summary.contains("agent-prompt.md") && c.summary.contains("pixel install"),
            "{id} names the missing artifact and the fix: {}",
            c.summary
        );
    }

    // 2. A plain install (no CLAUDE.md, no managed block): both checks read
    //    the deployed prompt and go green.
    install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("install");
    let prompt_path = home.join(".local/share/pixel/agent-prompt.md");
    let report = doctor(&doc_opts).expect("doctor runs");
    for id in ["rule.parity", "rule.scenarios"] {
        let c = check(&report, id);
        assert_eq!(
            c.status,
            pixel_install::doctor::CheckStatus::Green,
            "{id} after install: {:?} {:?}",
            c.summary,
            c.reason
        );
        assert_eq!(
            c.detail.as_ref().and_then(|d| d["source"].as_str()),
            Some(prompt_path.to_str().unwrap()),
            "{id} validated the deployed prompt"
        );
    }
    let parity = check(&report, "rule.parity");
    let parsed = parity.detail.as_ref().unwrap()["parsed_ok"]
        .as_u64()
        .unwrap();
    assert!(
        parsed > 0,
        "the deployed prompt documents pixel command lines: {parity:?}"
    );

    // 3. A deployed prompt documenting a command the binary rejects is red,
    //    even with a legacy managed block that would pass: the deployed
    //    prompt is what agents read, so it wins over the older text.
    let prompt = fs::read_to_string(&prompt_path).unwrap();
    fs::write(
        &prompt_path,
        format!(
            "{prompt}
```bash
pixel bogus-subcommand .
```
"
        ),
    )
    .unwrap();
    fs::write(
        home.join("CLAUDE.md"),
        format!(
            "# mine
{MANAGED_BEGIN}
```bash
pixel scope-task task
pixel find-code x
pixel plan-rollback
pixel sync-branch
pixel impact x
```
{MANAGED_END}
"
        ),
    )
    .unwrap();
    let report = doctor(&doc_opts).expect("doctor runs");
    let parity = check(&report, "rule.parity");
    assert_eq!(
        parity.status,
        pixel_install::doctor::CheckStatus::Red,
        "{parity:?}"
    );
    assert!(
        parity
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("bogus-subcommand"),
        "the rejected line is named: {:?}",
        parity.reason
    );
    // Red at all proves the precedence: the managed block alone parses
    // green, so the rejected line can only have come from the deployed
    // prompt (a red check carries no detail to name its source).

    // 4. With the deployed prompt gone, the legacy managed block is the
    //    fallback for installs that predate agent-prompt.md.
    fs::remove_file(&prompt_path).unwrap();
    let report = doctor(&doc_opts).expect("doctor runs");
    let parity = check(&report, "rule.parity");
    assert_eq!(
        parity.status,
        pixel_install::doctor::CheckStatus::Green,
        "{parity:?}"
    );
    assert_eq!(
        parity.detail.as_ref().and_then(|d| d["source"].as_str()),
        Some(home.join("CLAUDE.md").to_str().unwrap()),
        "legacy managed block is read when no prompt is deployed"
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
        ".pi/agent/APPEND_SYSTEM.md",
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
    install(&repo_install_options(&repo, &home)).expect("repo install");

    let report = uninstall(&UninstallOptions {
        home: Some(home.clone()),
        repo: Some(repo.clone()),
        ..Default::default()
    })
    .expect("repo uninstall");

    let on_disk = backups_on_disk(&repo);
    assert!(
        on_disk.iter().any(|p| p.parent() == Some(repo.as_path())),
        "the AGENTS.md rewrite keeps a backup at the root: {on_disk:?}"
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
                    hook["command"]
                        .as_str()
                        .is_some_and(|c| c.contains("run-hook session-start"))
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
    fs::write(&config, "{}").unwrap();
    let bin_dir = home.join("Pixel hook tools' directory");
    fs::create_dir_all(&bin_dir).unwrap();
    let exe = bin_dir.join("pixel");
    fs::write(&exe, "#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s' \"$*\"\n").unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    let opts = InstallOptions {
        repo: None,
        home: Some(home.clone()),
        executable_path: Some(exe.clone()),
        claude_executable: Some(fake_claude_exe(&home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    };
    // Claude gets the lifecycle hooks (the SessionStart prompt injection is
    // how the doctrine reaches every `claude` process); Codex gets the
    // metrics PostToolUse relay (Codex's exec layer merges the invocation's
    // stderr into the tool result it records and shows, so the relay's
    // dedupe drops the duplicate and a PostToolUse entry only re-emits the
    // 🟩 line for the rare host whose tool result drops it). Devin's config
    // passes through untouched, and no hooks directory or hook scripts are
    // created.
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
        installed
    } else {
        without_task_hooks(&installed, &provider, &exe)
    };
    match provider.as_str() {
        "codex" => {
            let post = value["hooks"]["PostToolUse"]
                .as_array()
                .expect("codex gets exactly the metrics PostToolUse relay");
            assert_eq!(post.len(), 1);
            let command = post[0]["hooks"][0]["command"].as_str().unwrap();
            assert!(command.contains("run-hook metrics"), "{command}");
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
            // Beyond the task gates, no retrieval guard belongs in global settings.
            let hooks = value["hooks"].as_object().unwrap();
            assert!(
                hooks.get("PreToolUse").is_none(),
                "global claude install must not wire a retrieval guard: {value}"
            );
            for (event, verb) in [
                ("SessionStart", "run-hook session-start"),
                ("UserPromptSubmit", "run-hook prompt-submit"),
                ("PostToolUse", "run-hook post-tool-use"),
            ] {
                let groups = hooks[event].as_array().unwrap();
                assert!(
                    groups.iter().any(|g| {
                        g["hooks"].as_array().is_some_and(|h| {
                            h.iter().any(|hook| {
                                hook["command"].as_str().is_some_and(|c| c.contains(verb))
                            })
                        })
                    }),
                    "{event} must register {verb}: {value}"
                );
            }
            // Claude's post-compaction rides SessionStart with matcher "compact".
            let session = hooks["SessionStart"].as_array().unwrap();
            assert!(
                session.iter().any(|g| {
                    g["matcher"].as_str() == Some("compact")
                        && g["hooks"].as_array().is_some_and(|h| {
                            h.iter().any(|hook| {
                                hook["command"]
                                    .as_str()
                                    .is_some_and(|c| c.contains("run-hook post-compaction"))
                            })
                        })
                }),
                "the compact SessionStart entry must be registered: {value}"
            );
            // The quoted executable with a space and apostrophe must survive.
            let command = session[0]["hooks"][0]["command"].as_str().unwrap();
            assert!(
                command.starts_with('\'')
                    && command.contains("directory/pixel' run-hook")
                    && command.contains("'\\''"),
                "the executable path must survive spaces and quotes: {command}"
            );
        }
        "devin" => {
            // Devin's own lifecycle protocol: the three lifecycle hooks with
            // `--provider devin` where the provider decides the dialect (the
            // post-compaction manifest reader needs none), and no
            // PostToolUse — the relay is repo-scoped and would double it in
            // every installed repository.
            let hooks = value["hooks"].as_object().unwrap();
            assert!(
                hooks.get("PreToolUse").is_none() && hooks.get("PostToolUse").is_none(),
                "global devin install wires lifecycle only: {value}"
            );
            for (event, verb) in [
                ("SessionStart", "run-hook session-start --provider devin"),
                (
                    "UserPromptSubmit",
                    "run-hook prompt-submit --provider devin",
                ),
                ("PostCompaction", "run-hook post-compaction"),
            ] {
                let groups = hooks[event].as_array().unwrap();
                assert!(
                    groups.iter().any(|g| {
                        g["hooks"].as_array().is_some_and(|h| {
                            h.iter().any(|hook| {
                                hook["command"].as_str().is_some_and(|c| c.contains(verb))
                            })
                        })
                    }),
                    "{event} must register {verb}: {value}"
                );
            }
        }
        _ => unreachable!("unexpected provider {provider}"),
    }
    // No provider gets a ~/.claude/hooks directory from the install.
    assert!(
        !home.join(".claude/hooks").exists(),
        "install must not create ~/.claude/hooks for any provider"
    );
    // The install artifacts (agent-prompt + lifecycle hooks) are deployed
    // regardless of provider.
    assert!(
        home.join(".local/share/pixel/agent-prompt.md").is_file(),
        "agent-prompt.md should be deployed"
    );
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
    let prompt = home.join(".local/share/pixel/agent-prompt.md");
    assert!(prompt.is_file(), "fixture: the prompt is installed");
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
// sub-agent prompt: `--append-subagent-system-prompt-file`
//
// Claude Code sub-agents receive neither the session's
// `--append-system-prompt-file` nor its history, so the `claude` wrapper's
// agent prompt never reaches them. Claude Code honours
// `--append-subagent-system-prompt-file` in print mode only, and the same
// wrapper fronts interactive sessions, so the flag is added exactly when
// `-p`/`--print` is among the arguments. These tests pin the asset, the
// argument-dependent flag in every supported shell, and the uninstall path.
// ---------------------------------------------------------------------------

const SUBAGENT_PROMPT_ASSET: &str = include_str!("../assets/pixel-subagent-prompt.md");

fn subagent_prompt_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".local/share/pixel/subagent-prompt.md")
}

#[test]
fn install_deploys_the_bundled_subagent_prompt_under_two_kilobytes() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);

    let deployed = fs::read_to_string(subagent_prompt_path(home))
        .expect("subagent-prompt.md should be deployed next to agent-prompt.md");
    assert_eq!(
        deployed, SUBAGENT_PROMPT_ASSET,
        "the deployed file must be the bundled asset, byte for byte"
    );
    // A long sub-agent prompt loses to a long agent body; the whole point of
    // a separate file is that it stays short enough to be obeyed.
    assert!(
        deployed.len() <= 2048,
        "subagent-prompt.md must stay under 2 KB, is {} bytes",
        deployed.len()
    );
    for stale in ["--callers", "--callees", "~/.local/bin/pixel", "MANDATORY"] {
        assert!(
            !deployed.contains(stale),
            "sub-agent prompt must not carry syntax the CLI rejects or an install path that is often wrong: {stale}"
        );
    }
}

#[test]
fn dry_run_does_not_write_the_subagent_prompt() {
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
    assert!(report.dry_run);
    assert!(
        !subagent_prompt_path(home).exists(),
        "dry-run must not deploy subagent-prompt.md"
    );
    let step = report
        .steps
        .iter()
        .find(|s| s.id == "agent-prompt")
        .expect("agent-prompt step");
    assert!(
        step.summary.contains("subagent-prompt.md"),
        "the dry-run report must announce the sub-agent prompt it would deploy: {}",
        step.summary
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

/// Codex has no file-backed `developer_instructions`, so the prompt is
/// embedded in the file. Two things must hold: Codex reads back exactly the
/// bundled prompt (a TOML round trip, no escaping accident), and nothing else
/// in a file the desktop app also owns moves.
#[test]
fn install_writes_the_agent_prompt_into_codex_config_and_leaves_the_rest_of_the_file_alone() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(codex_config_path(home), USER_CODEX_CONFIG).unwrap();
    install_for_shell(home, TEST_SHELL);

    let asset = fs::read_to_string(home.join(".local/share/pixel/agent-prompt.md")).unwrap();
    let value = codex_developer_instructions(home).expect("developer_instructions written");
    assert_eq!(
        value,
        format!("{PIXEL_BLOCK_BEGIN}\n{asset}{PIXEL_BLOCK_END}\n"),
        "codex must read back the bundled prompt between the pixel markers"
    );
    let written = fs::read_to_string(codex_config_path(home)).unwrap();
    for line in USER_CODEX_CONFIG.lines() {
        assert!(
            written.contains(line),
            "user line {line:?} must survive the install verbatim:\n{written}"
        );
    }
    assert!(
        written.contains("developer_instructions = '''\n"),
        "the prompt must be a literal multi-line string, so the file shows it unescaped:\n{written}"
    );
    let doc: toml_edit::DocumentMut = written.parse().unwrap();
    assert!(
        doc.get("developer_instructions")
            .is_some_and(toml_edit::Item::is_value),
        "the key must sit in the root table, not inside [features] at the end of the file"
    );
    assert_eq!(
        doc["features"]["token_budget"]["enabled"].as_bool(),
        Some(true),
        "sub-tables must be untouched"
    );

    install_for_shell(home, TEST_SHELL);
    assert_eq!(
        fs::read_to_string(codex_config_path(home)).unwrap(),
        written,
        "a re-install must be byte-for-byte idempotent"
    );
    assert!(
        !shell_profile_path(home).exists(),
        "no shell wrapper is written — codex carries the prompt in config.toml"
    );
}

#[test]
fn install_keeps_a_users_own_developer_instructions_and_refreshes_a_stale_pixel_block() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(
        codex_config_path(home),
        "developer_instructions = \"Always answer in French.\"\n",
    )
    .unwrap();
    install_for_shell(home, TEST_SHELL);
    let asset = fs::read_to_string(home.join(".local/share/pixel/agent-prompt.md")).unwrap();
    let expected =
        format!("Always answer in French.\n\n{PIXEL_BLOCK_BEGIN}\n{asset}{PIXEL_BLOCK_END}\n");
    assert_eq!(
        codex_developer_instructions(home).as_deref(),
        Some(expected.as_str()),
        "the user's own instructions come first, the pixel block is appended"
    );

    // A block left by an older pixel (different prompt) plus text the user
    // added after it: only the block changes.
    fs::write(
        codex_config_path(home),
        format!(
            "developer_instructions = '''\nMine first.\n\n{PIXEL_BLOCK_BEGIN}\nold prompt\n{PIXEL_BLOCK_END}\nMine last.\n'''\n"
        ),
    )
    .unwrap();
    install_for_shell(home, TEST_SHELL);
    assert_eq!(
        codex_developer_instructions(home).as_deref(),
        Some(
            format!("Mine first.\n\n{PIXEL_BLOCK_BEGIN}\n{asset}{PIXEL_BLOCK_END}\nMine last.\n")
                .as_str()
        ),
        "a stale block is replaced in place, text on both sides survives"
    );
}

#[test]
fn install_writes_the_agent_prompt_into_opencode_agents_md_when_opencode_is_present() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let opencode = home.join(".config/opencode");
    let agents_md = opencode.join("AGENTS.md");
    let config = opencode.join("opencode.json");

    // No ~/.config/opencode: the step is skipped and no files appear.
    install_for_shell(home, TEST_SHELL);
    assert!(
        !agents_md.exists() && !config.exists(),
        "install must not create OpenCode config for a user without OpenCode"
    );

    // OpenCode present with a global AGENTS.md and a config carrying a
    // stale pixel instructions entry plus a dead pixel.mjs plugin entry:
    // the block merges into AGENTS.md, user text survives, and the dead
    // config entries are swept.
    fs::create_dir_all(&opencode).unwrap();
    fs::write(&agents_md, "user rules stay\n").unwrap();
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
    install_for_shell(home, TEST_SHELL);
    let content = fs::read_to_string(&agents_md).unwrap();
    assert!(content.contains("user rules stay"), "{content}");
    assert!(content.contains(PIXEL_BLOCK_BEGIN), "{content}");
    let prompt = fs::read_to_string(home.join(".local/share/pixel/agent-prompt.md")).unwrap();
    assert!(content.contains(&prompt), "the bundled prompt is embedded");
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert!(
        value.get("instructions").is_none(),
        "the stale instructions entry is swept: {value}"
    );
    assert_eq!(
        value["plugin"],
        serde_json::json!(["./plugins/caveman/plugin.js"]),
        "only the missing-file pixel.mjs entry goes"
    );
    assert_eq!(value["model"], "anthropic/claude-sonnet-4-5");

    let written = fs::read_to_string(&agents_md).unwrap();
    install_for_shell(home, TEST_SHELL);
    assert_eq!(
        fs::read_to_string(&agents_md).unwrap(),
        written,
        "a re-install must be byte-for-byte idempotent"
    );

    // Uninstall strips only the block.
    uninstall(&UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join(".local/bin/pixel")),
        running_binary: None,
        executable_path: None,
        shell: Some(TEST_SHELL.into()),
        dry_run: false,
        wrappers_only: false,
    })
    .expect("uninstall");
    assert_eq!(fs::read_to_string(&agents_md).unwrap(), "user rules stay\n");
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
fn doctor_codex_config_check_is_red_until_the_current_block_is_in_place() {
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
    assert_eq!(status().status, CheckStatus::Red, "nothing installed");

    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(
        codex_config_path(home),
        "developer_instructions = \"Always answer in French.\"\n",
    )
    .unwrap();
    assert_eq!(
        status().status,
        CheckStatus::Red,
        "a value without the pixel block does not carry the prompt"
    );

    install_for_shell(home, TEST_SHELL);
    let check = status();
    assert_eq!(check.status, CheckStatus::Green, "{check:?}");

    let written = fs::read_to_string(codex_config_path(home)).unwrap();
    fs::write(
        codex_config_path(home),
        written.replace("## Retrieval commands", "## Retrieval output"),
    )
    .unwrap();
    let check = status();
    assert_eq!(
        check.status,
        CheckStatus::Red,
        "a block that differs from the bundled prompt is stale: {check:?}"
    );
    assert!(
        check.reason.as_deref().is_some_and(|r| r.contains("stale")),
        "{check:?}"
    );
}

#[test]
fn uninstall_takes_only_the_pixel_block_out_of_codex_config() {
    use pixel_install::uninstall::{UninstallOptions, uninstall};

    // Only pixel in the key: the key goes, the rest of the file stays.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(codex_config_path(home), USER_CODEX_CONFIG).unwrap();
    install_for_shell(home, TEST_SHELL);
    assert!(codex_developer_instructions(home).is_some());
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

    // The user's own text around the block: the block goes, the text stays.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(
        codex_config_path(home),
        "developer_instructions = \"Always answer in French.\"\n",
    )
    .unwrap();
    install_for_shell(home, TEST_SHELL);
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

#[test]
fn doctor_flags_a_missing_or_stale_subagent_prompt() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    let check = |home: &std::path::Path| {
        doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            shell: Some(TEST_SHELL.into()),
            claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
            ..Default::default()
        })
        .expect("doctor")
        .checks
        .into_iter()
        .find(|c| c.id == "install.subagent-prompt")
        .expect("install.subagent-prompt check")
        .status
    };
    assert_eq!(
        check(home),
        pixel_install::doctor::CheckStatus::Green,
        "freshly installed sub-agent prompt must be green"
    );
    fs::write(subagent_prompt_path(home), "pixel who-calls X --callers\n").unwrap();
    assert_eq!(
        check(home),
        pixel_install::doctor::CheckStatus::Red,
        "a sub-agent prompt that differs from the bundled asset is stale: every print-mode \
         sub-agent would be taught it"
    );
    fs::remove_file(subagent_prompt_path(home)).unwrap();
    assert_eq!(
        check(home),
        pixel_install::doctor::CheckStatus::Red,
        "the wrapper passes a path that no longer exists"
    );
}

#[test]
fn uninstall_removes_the_subagent_prompt() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    assert!(
        subagent_prompt_path(home).is_file(),
        "precondition: sub-agent prompt deployed"
    );

    uninstall(&UninstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        binary_path: None,
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .expect("uninstall");

    assert!(
        !subagent_prompt_path(home).exists(),
        "uninstall must remove subagent-prompt.md with the wrapper that referenced it"
    );
    assert!(
        !home.join(".local/share/pixel/agent-prompt.md").exists(),
        "agent-prompt.md is removed alongside"
    );
}

// ---------------------------------------------------------------------------
// Pi's APPEND_SYSTEM.md is a shared file: pixel owns a managed block inside
// it, never the file. `pixel install` used to replace the whole file and
// `pixel uninstall` deleted it, so the user's own pi instructions were lost.
// ---------------------------------------------------------------------------

fn pi_prompt_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".pi/agent/APPEND_SYSTEM.md")
}

const PRE_MARKER_PI_PROMPT: &str = "# Pixel Retrieval Layer — Mandatory Agent Protocol\n\n## THE COMPLETE REPLACEMENT MAP\npixel search \"term\"\n## ENVIRONMENT\nAll commands accept `[PATH]` (default: current directory).\n";

/// Backup files `pixel install` wrote for the pi prompt, newest last.
fn pi_backups(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<std::path::PathBuf> = fs::read_dir(home.join(".pi/agent"))
        .map(|entries| {
            entries
                .filter_map(std::result::Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().contains("pixel-bak"))
                })
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    paths
}

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

#[test]
fn install_and_uninstall_keep_the_users_own_text_in_pis_append_system_file() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let pi_path = pi_prompt_path(home);
    fs::create_dir_all(pi_path.parent().unwrap()).unwrap();
    let original = "# my own pi instructions\nAlways answer in French.\n";
    fs::write(&pi_path, original).unwrap();

    install_for_shell(home, TEST_SHELL);

    let deployed = fs::read_to_string(&pi_path).expect("pi prompt deployed");
    assert!(
        deployed.contains(original.trim_end()),
        "the user's own instructions must survive install:\n{deployed}"
    );
    assert!(
        deployed.contains(MANAGED_BEGIN) && deployed.contains(MANAGED_END),
        "the bundled prompt must sit inside the managed markers:\n{deployed}"
    );
    let backups = pi_backups(home);
    assert_eq!(backups.len(), 1, "one backup of the file install changed");
    assert_eq!(
        fs::read_to_string(&backups[0]).unwrap(),
        original,
        "the backup holds the bytes install replaced"
    );

    // A second install that finds a current block changes nothing.
    let once = fs::read(&pi_path).unwrap();
    install_for_shell(home, TEST_SHELL);
    assert_eq!(
        fs::read(&pi_path).unwrap(),
        once,
        "a current managed block is rewritten with the same bytes"
    );

    uninstall_home(home);
    let after = fs::read_to_string(&pi_path).expect("the user's file survives uninstall");
    assert_eq!(after, original, "uninstall removes the block, not the file");
    assert!(!after.contains(MANAGED_BEGIN), "{after}");
}

#[test]
fn doctor_detects_a_pre_marker_prompt_above_the_managed_block_until_install_repairs_it() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    let pi_path = pi_prompt_path(home);
    let current = fs::read_to_string(&pi_path).unwrap();
    let stale = format!("My own note.\n{PRE_MARKER_PI_PROMPT}## My section\nKeep this.\n{current}");
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
        repaired,
        format!("My own note.\n## My section\nKeep this.\n{current}"),
        "install must remove only the recognized historical prompt"
    );
    assert_eq!(pi_status(), CheckStatus::Green);
    assert_eq!(
        fs::read_to_string(pi_backups(home).last().unwrap()).unwrap(),
        stale
    );
}

#[test]
fn uninstall_reclaims_pre_marker_prompt_copies_without_erasing_user_text() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    let pi_path = pi_prompt_path(home);
    let current = fs::read_to_string(&pi_path).unwrap();
    fs::write(
        &pi_path,
        format!("Before.\n{PRE_MARKER_PI_PROMPT}## My section\nKeep this.\n{current}After.\n"),
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
fn install_doctor_and_uninstall_should_preserve_a_fence_spanning_the_managed_block() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    let pi_path = pi_prompt_path(home);
    let current = fs::read_to_string(&pi_path).unwrap();
    let existing = format!("```markdown\n{current}{PRE_MARKER_PI_PROMPT}```\nAfter.\n");
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
    assert_eq!(
        fs::read_to_string(pi_path).unwrap(),
        format!("```markdown\n{PRE_MARKER_PI_PROMPT}```\nAfter.\n")
    );
}

#[test]
fn pi_lifecycle_should_preserve_an_orphan_end_marker_and_repair_the_real_block() {
    for closing in ["", MANAGED_END] {
        let dir = TempDir::new().expect("tempdir");
        let home = dir.path();
        install_for_shell(home, TEST_SHELL);
        let pi_path = pi_prompt_path(home);
        let current = fs::read_to_string(&pi_path).unwrap();
        let prefix = format!("{MANAGED_END}\nBefore.\n");
        let original = format!("{prefix}{MANAGED_BEGIN}\nstale\n{closing}");
        fs::write(&pi_path, &original).unwrap();
        assert_eq!(
            pixel_install::config::strip_managed_block(&original),
            if closing.is_empty() {
                original.clone()
            } else {
                prefix.clone()
            }
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
        assert_eq!(
            fs::read_to_string(&pi_path).unwrap(),
            format!("{prefix}{current}")
        );
        assert_eq!(
            check(&doctor(&opts).unwrap(), "install.pi-prompt").status,
            CheckStatus::Green
        );
        uninstall_home(home);
        assert_eq!(fs::read_to_string(&pi_path).unwrap(), prefix);
    }
}

#[test]
fn a_pi_prompt_written_by_an_earlier_install_is_wrapped_not_duplicated() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    // What `pixel install` wrote before it treated the file as shared: the
    // prompt verbatim, no markers around it.
    let asset = fs::read_to_string(home.join(".local/share/pixel/agent-prompt.md"))
        .expect("deployed prompt");
    let pi_path = pi_prompt_path(home);
    fs::write(&pi_path, &asset).unwrap();

    install_for_shell(home, TEST_SHELL);

    let deployed = fs::read_to_string(&pi_path).expect("pi prompt deployed");
    assert!(
        deployed.starts_with(MANAGED_BEGIN),
        "the upgrade must put the markers around the prompt, not above it:\n{deployed}"
    );
    assert_eq!(
        deployed.matches(asset.as_str()).count(),
        0,
        "the long legacy prompt is replaced by Pi's short rule"
    );
    assert!(deployed.contains("Use the pixel tool for repository retrieval"));
}

#[test]
fn legacy_pi_prompt_migration_removes_stale_sections_and_stays_healthy() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    let asset = fs::read_to_string(home.join(".local/share/pixel/agent-prompt.md"))
        .expect("deployed legacy prompt");
    let pi_path = pi_prompt_path(home);
    fs::write(
        &pi_path,
        format!("My Pi note.\n{asset}\n## GitNexus — Legacy\nStale instructions.\n## My notes\nKeep this.\n"),
    )
    .expect("legacy prompt fixture");

    install_for_shell(home, TEST_SHELL);

    let deployed = fs::read_to_string(&pi_path).expect("migrated Pi prompt");
    assert!(deployed.starts_with("My Pi note.\n"), "{deployed}");
    assert!(deployed.contains("## My notes\nKeep this.\n"), "{deployed}");
    assert!(!deployed.contains("GitNexus"), "{deployed}");
    assert!(!deployed.contains("Stale instructions."), "{deployed}");
    assert!(!deployed.contains(&asset), "{deployed}");
    assert_eq!(deployed.matches(MANAGED_BEGIN).count(), 1, "{deployed}");
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
    fs::write(&pi_path, format!("Before.\n{edited}After.\n"))
        .expect("edited legacy prompt fixture");

    install_for_shell(home, TEST_SHELL);

    let deployed = fs::read_to_string(&pi_path).expect("migrated Pi prompt");
    assert!(deployed.starts_with("Before.\n"), "{deployed}");
    assert!(deployed.ends_with("After.\n"), "{deployed}");
    assert!(!deployed.contains("edited by hand"), "{deployed}");
    assert!(!deployed.contains("# Pixel Retrieval Layer"), "{deployed}");
    assert_eq!(deployed.matches(MANAGED_BEGIN).count(), 1, "{deployed}");
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
fn uninstall_removes_the_pi_prompt_file_when_it_held_nothing_else() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let pi_path = pi_prompt_path(home);
    fs::create_dir_all(pi_path.parent().unwrap()).unwrap();
    fs::write(
        &pi_path,
        format!("{MANAGED_BEGIN}\n# pixel's own prompt\n{MANAGED_END}\n"),
    )
    .unwrap();

    install_for_shell(home, TEST_SHELL);
    uninstall_home(home);

    assert!(
        !pi_path.exists(),
        "a file that held nothing but the pixel block is pixel's to delete"
    );
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
        prompt_step.summary, "removed agent-prompt.md and subagent-prompt.md",
        "an absent Pi file must not be reported as removed"
    );
}

#[test]
fn install_reports_a_pi_prompt_it_cannot_write_instead_of_greening_it() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    // A file where the directory should be: creating ~/.pi/agent fails
    // whatever the user's permissions are.
    fs::write(home.join(".pi"), "not a directory\n").unwrap();

    let result = install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    });

    assert!(
        result.is_err(),
        "a pi prompt that cannot be written must fail the install, not report a green step: {result:?}"
    );
}

#[test]
fn doctor_pi_prompt_check_is_red_until_the_managed_block_is_current() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    let pi_path = pi_prompt_path(home);
    let doc_opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
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
        "a freshly installed pi prompt is green"
    );

    // The user's own text outside the markers is theirs: it does not make the
    // check stale.
    let with_user_text = format!("{}Be concise.\n", fs::read_to_string(&pi_path).unwrap());
    fs::write(&pi_path, &with_user_text).unwrap();
    assert_eq!(
        status(home),
        CheckStatus::Green,
        "text outside the pixel markers belongs to the user"
    );

    // A block that no longer matches the bundled prompt is stale.
    fs::write(
        &pi_path,
        format!("{MANAGED_BEGIN}\n# stale prompt\n{MANAGED_END}\n"),
    )
    .unwrap();
    assert_eq!(
        status(home),
        CheckStatus::Red,
        "a stale block must send the user back to pixel install"
    );

    // And the file must carry the block at all.
    fs::write(&pi_path, "my own instructions only\n").unwrap();
    assert_eq!(status(home), CheckStatus::Red, "no block, no green");
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

/// Plugin-manifest surfaces (skills/, .cursor/rules/, …) are generated from
/// `assets/pixel-agent-prompt.md` by `scripts/gen-plugin-assets.sh`. They must
/// never drift: an edited prompt with stale plugin files silently ships an old
/// protocol to every CLI that installs via plugin manifests.
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
fn prompt_packet_guidance_allows_exploration_beyond_candidates() {
    let prompt = include_str!("../assets/pixel-agent-prompt.md");
    assert!(prompt.contains("bounded set of"));
    assert!(
        prompt.contains("not an action recommendation") && prompt.contains("a read/edit boundary")
    );
    assert!(prompt.contains("continue exploring any files or"));
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
    for entry in package["files"].as_array().unwrap() {
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

    // A root `plugin.json` wins over the tool directories: Copilot CLI reads
    // it before `.claude-plugin/plugin.json`, and Codex's Agent Plugins loader
    // then ignores the hooks of `.codex-plugin/plugin.json`
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
fn repo_install_writes_all_five_artifacts() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&repo).unwrap();

    let report = install(&repo_install_options(&repo, &home)).expect("repo install");
    assert!(report.ok, "{report:?}");

    // .claude/settings.local.json — repo-local guard only: a pixel PreToolUse
    // group, no lifecycle events (those live in the user-level settings).
    // The command names this machine's binary, so the team-shared
    // settings.json is never created for it.
    assert!(
        !repo.join(".claude/settings.json").exists(),
        "the shared settings.json must not carry a machine-local guard"
    );
    let claude: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo.join(".claude/settings.local.json")).unwrap(),
    )
    .unwrap();
    let claude_hooks = claude["hooks"].as_object().unwrap();
    let pre = claude_hooks["PreToolUse"].as_array().unwrap();
    assert!(
        pre.iter().any(|g| {
            g["matcher"].as_str() == Some("Bash|Read|Grep")
                && g["hooks"].as_array().is_some_and(|h| {
                    h.iter().any(|hook| {
                        hook["command"]
                            .as_str()
                            .is_some_and(|c| c.contains("run-hook guard --provider claude"))
                    })
                })
        }),
        "repo .claude/settings.local.json must carry the pixel guard: {claude}"
    );
    for event in ["SessionStart", "UserPromptSubmit", "PostToolUse"] {
        assert!(
            claude_hooks.get(event).is_none(),
            "repo settings must not wire {event} — lifecycle is global: {claude}"
        );
    }

    // .codex/config.toml — developer_instructions managed block.
    let config = fs::read_to_string(repo.join(".codex/config.toml")).unwrap();
    assert!(config.contains("developer_instructions"), "{config}");
    assert!(config.contains(MANAGED_BEGIN), "{config}");

    // .codex/hooks.json — exactly the composed guard group + sidecar backup.
    let hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(repo.join(".codex/hooks.json")).unwrap()).unwrap();
    assert_eq!(
        hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        format!(
            "'{}' run-hook session-start --provider codex",
            home.join("pixel").canonicalize().unwrap().display()
        ),
        "the installed Codex hook must select its strict response schema"
    );
    let pre = hooks["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 1, "{hooks}");
    let command = pre[0]["hooks"][0]["command"].as_str().unwrap();
    assert!(
        command.contains("run-hook composed-guard --provider codex --backup"),
        "{command}"
    );
    let sidecar: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo.join(".codex/pixel-composed-guard-backup.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(sidecar["provider"], "codex");
    assert!(sidecar["pre_tool_use"].is_array());
    assert_eq!(
        sidecar["managed_pre_tool_use"],
        hooks["hooks"]["PreToolUse"]
    );

    // .devin/config.local.json — pixel guard group, in the personal config
    // Devin CLI reads (it never reads .devin/hooks.json).
    assert!(!repo.join(".devin/hooks.json").exists());
    let devin: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(repo.join(".devin/config.local.json")).unwrap())
            .unwrap();
    let devin_pre = devin["hooks"]["PreToolUse"].as_array().unwrap();
    assert!(
        devin_pre.iter().any(|group| {
            group["matcher"]
                .as_str()
                .is_some_and(|matcher| matcher.contains("glob"))
        }),
        "native Devin glob must reach the Pixel guard: {devin}"
    );
    assert!(
        devin_pre.iter().any(|g| {
            g["hooks"].as_array().is_some_and(|h| {
                h.iter().any(|hook| {
                    hook["command"]
                        .as_str()
                        .is_some_and(|c| c.contains("run-hook guard --provider devin"))
                })
            })
        }),
        "{devin}"
    );
    let devin_prompt = devin["hooks"]["UserPromptSubmit"].as_array().unwrap();
    assert!(
        devin_prompt.iter().any(|group| {
            group["hooks"].as_array().is_some_and(|hooks| {
                hooks.iter().any(|hook| {
                    hook["command"].as_str().is_some_and(|command| {
                        command.contains("run-hook prompt-submit --provider devin")
                    })
                })
            })
        }),
        "Devin must receive Pixel-first context on every prompt without blocking: {devin}"
    );
    let devin_permission = devin["hooks"]["PermissionRequest"].as_array().unwrap();
    assert!(
        devin_permission.iter().any(|group| {
            group["matcher"] == "exec"
                && group["hooks"].as_array().is_some_and(|hooks| {
                    hooks.iter().any(|hook| {
                        hook["command"].as_str().is_some_and(|command| {
                            command.contains("run-hook guard --provider devin")
                        })
                    })
                })
        }),
        "Devin must silently approve only Pixel retrieval execs: {devin}"
    );

    // .pi/extensions/pixel-guard.ts, the project directory pi discovers
    // extensions from; nothing under .pi/agent/, which pi reads only in ~.
    let ext = fs::read_to_string(repo.join(".pi/extensions/pixel-guard.ts")).unwrap();
    assert!(ext.contains(MANAGED_BEGIN), "{ext}");
    assert!(ext.contains("pi.registerTool({"), "{ext}");
    assert!(ext.contains("pi.on(\"tool_call\""), "{ext}");
    assert!(!repo.join(".pi/agent").exists());

    // Nothing global was touched.
    assert!(!home.join(".local/share/pixel").exists());
    assert!(!home.join(".codex").exists());
}

/// A deleted `.codex/hooks.json` with a surviving composed-guard sidecar is
/// not a user edit to reconcile: the reinstall must start fresh from the
/// sidecar instead of refusing on the absent PreToolUse group, and the
/// adopted groups the sidecar held come back with it.
#[test]
#[cfg(unix)]
fn repo_install_recovers_a_deleted_codex_hooks_file() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".codex")).unwrap();

    let foreign = serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":"keep-security-check"}]});
    fs::write(
        repo.join(".codex/hooks.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {"PreToolUse": [foreign.clone()]}
        }))
        .unwrap(),
    )
    .unwrap();
    install(&repo_install_options(&repo, &home)).unwrap();

    fs::remove_file(repo.join(".codex/hooks.json")).unwrap();

    install(&repo_install_options(&repo, &home))
        .expect("an orphaned sidecar is not a user edit: reinstall must run");
    let hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(repo.join(".codex/hooks.json")).unwrap()).unwrap();
    assert!(
        hooks["hooks"]["PreToolUse"].as_array().unwrap()[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("composed-guard"),
        "{hooks}"
    );
    let sidecar: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo.join(".codex/pixel-composed-guard-backup.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        sidecar["pre_tool_use"],
        serde_json::json!([foreign]),
        "the groups the first install adopted survive in the sidecar"
    );
    assert_eq!(
        sidecar["managed_pre_tool_use"], hooks["hooks"]["PreToolUse"],
        "the rewritten sidecar names the group it just published"
    );
}

/// A reinstall that agrees with the published contract does not touch the
/// sidecar at all: it is the runtime's private input, so even an identical
/// rewrite (new inode, new mtime) is a change we do not make.
#[test]
#[cfg(unix)]
fn repo_install_leaves_an_agreeing_sidecar_file_untouched() {
    use std::os::unix::fs::MetadataExt;
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&repo).unwrap();

    install(&repo_install_options(&repo, &home)).unwrap();
    let sidecar = repo.join(".codex/pixel-composed-guard-backup.json");
    let inode = fs::metadata(&sidecar).unwrap().ino();

    install(&repo_install_options(&repo, &home)).unwrap();
    assert_eq!(
        fs::metadata(&sidecar).unwrap().ino(),
        inode,
        "a reinstall that agrees must not rewrite the sidecar"
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
        ".codex/config.toml",
        ".codex/hooks.json",
        ".codex/pixel-composed-guard-backup.json",
        ".devin/config.local.json",
        ".pi/extensions/pixel-guard.ts",
    ];
    let snapshot = |rel: &str| fs::read(repo.join(rel)).unwrap();
    let before: Vec<_> = artifacts.iter().map(|rel| snapshot(rel)).collect();

    let report = install(&repo_install_options(&repo, &home)).unwrap();
    assert!(report.ok);
    let after: Vec<_> = artifacts.iter().map(|rel| snapshot(rel)).collect();
    assert_eq!(before, after, "reinstall must be byte-identical");
}

#[test]
#[cfg(unix)]
fn repo_install_preserves_foreign_hooks() {
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
    // .claude/settings.local.json (personal): the user's own permissions and
    // a non-shell PreToolUse group; both survive next to the pixel guard.
    let claude_local_foreign = serde_json::json!({"matcher":"Edit","hooks":[{"type":"command","command":"keep-edit-check"}]});
    fs::write(
        repo.join(".claude/settings.local.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "permissions": {"allow": ["Bash(ls:*)"]},
            "hooks": {"PreToolUse": [claude_local_foreign.clone()]}
        }))
        .unwrap(),
    )
    .unwrap();

    let foreign = serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":"keep-security-check"}]});
    fs::write(
        repo.join(".codex/hooks.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {"PreToolUse": [foreign.clone()]}
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        repo.join(".devin/config.local.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "permissions": {"allow": ["read"]},
            "hooks": {"PreToolUse": [foreign.clone()]}
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
    // Claude local: the user's keys and group intact, the pixel guard
    // appended, and no lifecycle events added — those live in the
    // user-level settings.
    let claude: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo.join(".claude/settings.local.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(claude["permissions"]["allow"][0], "Bash(ls:*)");
    assert!(claude["hooks"].get("SessionStart").is_none(), "{claude}");
    let claude_pre = claude["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(claude_pre[0], claude_local_foreign);
    assert_eq!(claude_pre.len(), 2, "{claude}");
    assert!(
        claude_pre[1]["hooks"].as_array().is_some_and(|h| {
            h.iter().any(|hook| {
                hook["command"]
                    .as_str()
                    .is_some_and(|c| c.contains("run-hook guard --provider claude"))
            })
        }),
        "{claude}"
    );

    // Codex: the foreign group moved into the sidecar; the live PreToolUse is
    // the single composed-guard group.
    let codex: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(repo.join(".codex/hooks.json")).unwrap()).unwrap();
    let sidecar: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo.join(".codex/pixel-composed-guard-backup.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        sidecar["pre_tool_use"],
        serde_json::json!([foreign.clone()])
    );
    assert!(
        codex["hooks"]["PreToolUse"].as_array().unwrap()[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("composed-guard")
    );

    // Devin: foreign group and keys kept, pixel group appended.
    let devin: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(repo.join(".devin/config.local.json")).unwrap())
            .unwrap();
    assert_eq!(devin["permissions"]["allow"][0], "read");
    let pre = devin["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre[0], foreign, "foreign devin group preserved");
    assert_eq!(pre.len(), 2);
}

#[test]
#[cfg(unix)]
fn repo_uninstall_removes_only_pixel_artifacts() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".claude")).unwrap();
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

    install(&repo_install_options(&repo, &home)).unwrap();

    let report = uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        repo: Some(repo.clone()),
        ..Default::default()
    })
    .unwrap();
    assert!(report.ok, "{report:?}");

    // Codex hooks.json: composed group + lifecycle entries + sidecar gone
    // (there were no pre-existing project hooks to restore).
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
    assert!(
        !repo
            .join(".codex/pixel-composed-guard-backup.json")
            .exists()
    );

    // config.toml: developer_instructions block gone.
    let config = fs::read_to_string(repo.join(".codex/config.toml")).unwrap();
    assert!(!config.contains(MANAGED_BEGIN), "{config}");

    // Claude: only the foreign group remains — the pixel guard is gone.
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
    ".devin/config.local.json",
    ".codex/hooks.json",
    ".codex/pixel-composed-guard-backup.json",
    ".pi/extensions/pixel-guard.ts",
];

/// A guard an earlier `--repo` install wrote into the team-shared
/// `settings.json` runs this machine's binary path on every clone: the
/// install takes it out of that file and registers it in the personal one,
/// leaving the file's other hooks, lifecycle included, as they were.
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
        claude_step
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("pixel guard removed from shared")),
        "the move is reported: {claude_step:?}"
    );

    let shared = read_json(&repo.join(".claude/settings.json"));
    assert_eq!(shared["hooks"]["PreToolUse"], serde_json::json!([write]));
    assert_eq!(shared["hooks"]["SessionStart"], serde_json::json!([start]));
    let local = read_json(&repo.join(".claude/settings.local.json"));
    assert_eq!(pixel_commands(&local, "PreToolUse").len(), 1, "{local}");

    let devin_legacy = read_json(&repo.join(".devin/hooks.json"));
    assert_eq!(
        devin_legacy["hooks"]["PreToolUse"],
        serde_json::json!([devin_foreign])
    );
    let devin = read_json(&repo.join(".devin/config.local.json"));
    assert_eq!(pixel_commands(&devin, "PreToolUse").len(), 1, "{devin}");

    let doctor_report = doctor(&DoctorOptions {
        home: Some(home.clone()),
        repo_root: Some(repo.clone()),
        ..Default::default()
    })
    .unwrap();
    for id in ["repo.claude-hooks", "repo.devin-hooks"] {
        let c = check(&doctor_report, id);
        assert_eq!(c.status, CheckStatus::Green, "{id}: {c:?}");
        assert!(c.summary.contains("registered"), "{id}: {c:?}");
    }
}

/// The shared settings.json keeps running in the same Claude session as the
/// personal file. A shell rewriter there (here the RTK group an earlier
/// delegate guard had adopted, now restored) would race the guard, so the
/// guard is not installed and the step says why.
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
        pixel_install::install::CheckStatus::Yellow,
        "{claude_step:?}"
    );
    assert!(
        claude_step.summary.contains("unknown overlapping hook"),
        "{claude_step:?}"
    );
    let shared = read_json(&repo.join(".claude/settings.json"));
    assert_eq!(
        shared["hooks"]["PreToolUse"],
        serde_json::json!([{"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]}]),
        "the RTK group the delegate had adopted runs again"
    );
    let local = read_json(&repo.join(".claude/settings.local.json"));
    assert!(pixel_commands(&local, "PreToolUse").is_empty(), "{local}");
}

/// An RTK group adopted from the repo's own settings belongs to the repo:
/// backed up under `<repo>/.claude/`, never in `$HOME`, where the next global
/// install would inject it into `~/.claude/settings.json`. Uninstalling the
/// repo puts it back and deletes the repo backup.
#[test]
#[cfg(unix)]
fn repo_rtk_adoption_should_stay_in_the_repo_and_come_back_on_repo_uninstall() {
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
    assert_eq!(
        read_json(&repo.join(".claude/pixel-rtk-hooks.json")),
        serde_json::json!([rtk.clone()])
    );
    let local = read_json(&repo.join(".claude/settings.local.json"));
    let guard = pixel_commands(&local, "PreToolUse");
    assert_eq!(guard.len(), 1, "{local}");
    assert!(guard[0].ends_with("--delegate-rtk"), "{guard:?}");

    // A reinstall reads the repo backup back (the delegate requires it).
    install(&repo_install_options(&repo, &home)).expect("reinstall finds the repo backup");
    // A global install afterwards leaves ~/.claude/settings.json without RTK.
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
        read_json(&repo.join(".claude/settings.local.json"))["hooks"]["PreToolUse"],
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
    let local = read_json(&repo.join(".claude/settings.local.json"));
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
    assert!(dry_step.summary.contains("6 machine-local"), "{dry_step:?}");

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
        status.contains(".codex/config.toml"),
        "the portable Codex instructions stay visible to git:\n{status}"
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
        assert!(c.summary.contains("not installed"), "{id}: {c:?}");
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

/// An installed repo-local Codex guard is byte-correct, but Codex composes the
/// project-scoped `.codex/` layer only for a trusted project. The check stays
/// green and its summary says which of the two it is, so an untrusted checkout
/// is not reported as a guard that fires.
#[test]
#[cfg(unix)]
fn doctor_repo_codex_hooks_summary_reports_codex_project_trust() {
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

    // Unspecified: the install is correct, the project is simply not listed.
    let doctor_report = doctor(&doctor_options).unwrap();
    let c = check(&doctor_report, "repo.codex-hooks");
    assert_eq!(c.status, CheckStatus::Green, "{c:?}");
    assert!(c.summary.contains("will not load until it does"), "{c:?}");
    assert!(!c.summary.contains("codex trusts this project"), "{c:?}");

    // Untrusted: an explicit refusal reads the same way.
    set_codex_trust(&home, &repo, "untrusted");
    let doctor_report = doctor(&doctor_options).unwrap();
    let c = check(&doctor_report, "repo.codex-hooks");
    assert_eq!(c.status, CheckStatus::Green, "{c:?}");
    assert!(c.summary.contains("will not load until it does"), "{c:?}");
    assert!(!c.summary.contains("codex trusts this project"), "{c:?}");

    // Trusted: the guard loads, and the summary says so.
    set_codex_trust(&home, &repo, "trusted");
    let doctor_report = doctor(&doctor_options).unwrap();
    let c = check(&doctor_report, "repo.codex-hooks");
    assert_eq!(c.status, CheckStatus::Green, "{c:?}");
    assert!(c.summary.contains("codex trusts this project"), "{c:?}");
    assert!(!c.summary.contains("will not load"), "{c:?}");

    // A trust file that does not parse degrades to "unknown", green as ever.
    fs::write(home.join(".codex/config.toml"), "not toml = = =\n").unwrap();
    let doctor_report = doctor(&doctor_options).unwrap();
    let c = check(&doctor_report, "repo.codex-hooks");
    assert_eq!(c.status, CheckStatus::Green, "{c:?}");
    assert!(c.summary.contains("codex trust unknown"), "{c:?}");
}

/// Evidence of a Pixel install that is broken stays red: a Pixel hook without
/// the guard, an RTK backup without the guard, a Pixel block gone stale, a
/// Pixel hook without its composed-guard sidecar, or a guard sitting in the
/// shared settings.json where it runs this machine's path on every clone.
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
    for id in [
        "repo.codex-config",
        "repo.codex-hooks",
        "repo.devin-hooks",
        "repo.claude-hooks",
    ] {
        let c = check(&report, id);
        assert_eq!(c.status, CheckStatus::Red, "{id}: {c:?}");
    }

    let devin_without_permission_approval = serde_json::json!({
        "hooks": {
            "PreToolUse": [{"matcher":"exec|Bash","hooks":[{"type":"command","command":"'/p/pixel' run-hook guard --provider devin"}]}],
            "UserPromptSubmit": [{"hooks":[{"type":"command","command":"'/p/pixel' run-hook prompt-submit --provider devin"}]}]
        }
    });
    fs::write(
        repo.join(".devin/config.local.json"),
        serde_json::to_string_pretty(&devin_without_permission_approval).unwrap(),
    )
    .unwrap();
    let report = doctor(&doctor_options).unwrap();
    let devin = check(&report, "repo.devin-hooks");
    assert_eq!(devin.status, CheckStatus::Red, "{devin:?}");
    assert!(
        devin
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("PermissionRequest")),
        "{devin:?}"
    );

    // An RTK backup alone is evidence too.
    fs::write(repo.join(".claude/settings.local.json"), "{}").unwrap();
    fs::write(
        repo.join(".claude/pixel-rtk-hooks.json"),
        r#"[{"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]}]"#,
    )
    .unwrap();
    let report = doctor(&doctor_options).unwrap();
    assert_eq!(check(&report, "repo.claude-hooks").status, CheckStatus::Red);

    // A correct local guard does not excuse one left in the shared file.
    fs::remove_file(repo.join(".claude/pixel-rtk-hooks.json")).unwrap();
    let guard = serde_json::to_string_pretty(&serde_json::json!({
        "hooks": {"PreToolUse": [{"matcher":"Bash","hooks":[{"type":"command","command":"'/p/pixel' run-hook guard --provider claude"}]}]}
    }))
    .unwrap();
    fs::write(repo.join(".claude/settings.local.json"), &guard).unwrap();
    let report = doctor(&doctor_options).unwrap();
    assert_eq!(
        check(&report, "repo.claude-hooks").status,
        CheckStatus::Green
    );
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

/// Installs before `pixel run-hook` wrote bare scripts into
/// `~/.claude/settings.json`. The global install must replace them, not add
/// a `run-hook` entry beside them: two SessionStart hooks inject the prompt
/// twice. A foreign command that merely mentions a Pixel verb is kept.
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
    let session = pixel_commands(&settings, "SessionStart");
    for verb in ["run-hook session-start", "run-hook post-compaction"] {
        assert_eq!(
            session.iter().filter(|c| c.contains(verb)).count(),
            1,
            "exactly one {verb}: {session:?}"
        );
    }
    assert_eq!(pixel_commands(&settings, "UserPromptSubmit").len(), 1);
    assert!(
        settings["hooks"].get("PreToolUse").is_none(),
        "the global install registers no guard: {settings}"
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

/// How many times each pixel lifecycle verb is registered in the global
/// Claude settings: one entry each is the only correct count, anything more
/// runs that hook several times on every prompt, edit or session.
fn lifecycle_counts(settings: &serde_json::Value) -> Vec<(&'static str, usize)> {
    [
        ("SessionStart", "run-hook session-start --provider claude"),
        ("SessionStart", "run-hook post-compaction --provider claude"),
        (
            "UserPromptSubmit",
            "run-hook prompt-submit --provider claude",
        ),
        ("PostToolUse", "run-hook post-tool-use --provider claude"),
        ("PostToolUse", "run-hook metrics --provider claude"),
    ]
    .into_iter()
    .map(|(event, verb)| {
        (
            verb,
            pixel_commands(settings, event)
                .iter()
                .filter(|c| c.ends_with(verb))
                .count(),
        )
    })
    .collect()
}

const ONE_EACH: [(&str, usize); 5] = [
    ("run-hook session-start --provider claude", 1),
    ("run-hook post-compaction --provider claude", 1),
    ("run-hook prompt-submit --provider claude", 1),
    ("run-hook post-tool-use --provider claude", 1),
    ("run-hook metrics --provider claude", 1),
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
        assert_eq!(lifecycle_counts(&settings), ONE_EACH, "{name}: {settings}");
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
    assert_eq!(lifecycle_counts(&settings), ONE_EACH, "{settings}");
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
    let reason = before.reason.clone().unwrap_or_default();
    for stacked in [
        "SessionStart→session-start ×3",
        "SessionStart→post-compaction --provider claude ×3",
        "UserPromptSubmit→prompt-submit --provider claude ×3",
        "PostToolUse→post-tool-use --provider claude ×3",
    ] {
        assert!(reason.contains(stacked), "{stacked} not in {reason}");
    }

    install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(exe.clone()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();

    let settings = read_json(&path);
    assert_eq!(lifecycle_counts(&settings), ONE_EACH, "{settings}");
    let groups = settings["hooks"]["SessionStart"].as_array().unwrap();
    assert!(groups.contains(&herdr), "herdr's group changed: {settings}");
    assert!(
        groups.contains(&serde_json::json!({"hooks":[notify]})),
        "the foreign hook sharing a group with a pixel copy is kept alone in it: {settings}"
    );
    let after = doctor_hooks();
    assert_eq!(after.status, CheckStatus::Green, "{after:?}");
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

/// A guard an older release left in `<repo>/.pi/agent/` never ran, since pi
/// reads that directory only under `~`: doctor says so in yellow with the
/// command that moves it, and the move turns the check green. A file at the
/// guard's path that pixel did not write fails the check.
#[test]
#[cfg(unix)]
fn doctor_pi_guard_should_flag_a_guard_pi_never_loads() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    // A space in the path: the suggested command is pasted into a shell.
    let repo = dir.path().join("my repo");
    fs::create_dir_all(&home).unwrap();
    let legacy = repo.join(".pi/agent/extensions/pixel-guard.ts");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::write(&legacy, format!("// {MANAGED_BEGIN}\n")).unwrap();
    let pi_check = || {
        let report = doctor(&DoctorOptions {
            home: Some(home.clone()),
            repo_root: Some(repo.clone()),
            ..Default::default()
        })
        .unwrap();
        check(&report, "repo.pi-guard").clone()
    };

    let c = pi_check();
    assert_eq!(c.status, CheckStatus::Yellow, "{c:?}");
    assert!(c.summary.contains("never loads"), "{c:?}");
    assert!(
        c.summary
            .contains(&format!("pixel install --repo '{}'", repo.display())),
        "{c:?}"
    );

    install(&repo_install_options(&repo, &home)).unwrap();
    let c = pi_check();
    assert_eq!(c.status, CheckStatus::Green, "{c:?}");
    assert!(c.summary.contains(".pi/extensions/pixel-guard.ts"), "{c:?}");
    assert!(!legacy.exists());

    fs::write(repo.join(".pi/extensions/pixel-guard.ts"), "// mine\n").unwrap();
    let c = pi_check();
    assert_eq!(c.status, CheckStatus::Red, "{c:?}");
    assert!(
        c.reason
            .as_deref()
            .is_some_and(|r| r.contains("not a pixel-managed guard extension")
                && r.contains(&format!("pixel install --repo '{}'", repo.display()))),
        "{c:?}"
    );
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
    git(&repo, &["init", "-q"]);
    // An exact RTK group in the personal settings is adopted by the guard,
    // which makes the install write its backup, the one conditional artifact.
    fs::write(
        repo.join(".claude/settings.local.json"),
        serde_json::to_string_pretty(&serde_json::json!({"hooks": {"PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "rtk hook claude"}]}
        ]}}))
        .unwrap(),
    )
    .unwrap();
    install(&repo_install_options(&repo, &home)).unwrap();

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
    let mut listed: Vec<String> = pixel_install::install::REPO_ARTIFACTS
        .iter()
        .map(|a| a.path.to_string())
        .collect();
    listed.sort();
    assert_eq!(written, listed);
}

/// Repo installation makes Pixel-first instructions available to project-aware
/// agents and doctor detects drift without treating an unconfigured repo as broken.
#[test]
#[cfg(unix)]
fn repo_install_should_manage_pixel_first_project_rules_fail_open() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    fs::write(
        repo.join("AGENTS.md"),
        "# Existing project rules\nPreserve this instruction.\n",
    )
    .unwrap();
    let doctor_options = DoctorOptions {
        home: Some(home.clone()),
        repo_root: Some(repo.clone()),
        only: vec!["repo.pixel-first".into()],
        ..Default::default()
    };

    let absent = doctor(&doctor_options).unwrap();
    assert_eq!(
        check(&absent, "repo.pixel-first").status,
        CheckStatus::Green
    );

    let installed = install(&repo_install_options(&repo, &home)).unwrap();
    assert!(installed.ok, "{installed:?}");
    let rules_path = repo.join("AGENTS.md");
    let managed = fs::read_to_string(&rules_path).unwrap();
    assert!(managed.starts_with("# Existing project rules\nPreserve this instruction."));
    assert!(managed.contains("pixel:warp-retrieval:begin"));
    assert!(managed.contains("pixel search-content -F '<identifier>'"));
    assert!(managed.contains("native tools stay available"));
    let current = doctor(&doctor_options).unwrap();
    assert_eq!(
        check(&current, "repo.pixel-first").status,
        CheckStatus::Green
    );

    // The block re-wrapped by hand, as a committed AGENTS.md carries it:
    // doctor stays green and a reinstall writes nothing, backup included.
    let reflowed = managed.replace(". ", ".\n  ");
    assert_ne!(reflowed, managed, "the fixture must change the layout");
    fs::write(&rules_path, &reflowed).unwrap();
    let rewrapped = doctor(&doctor_options).unwrap();
    assert_eq!(
        check(&rewrapped, "repo.pixel-first").status,
        CheckStatus::Green
    );
    let agents_backups = || {
        let mut names: Vec<_> = fs::read_dir(&repo)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("AGENTS.md.pixel-bak."))
            .collect();
        names.sort();
        names
    };
    let backups_before = agents_backups();
    let reinstalled = install(&repo_install_options(&repo, &home)).unwrap();
    assert!(reinstalled.ok, "{reinstalled:?}");
    assert_eq!(fs::read_to_string(&rules_path).unwrap(), reflowed);
    assert_eq!(agents_backups(), backups_before);

    let stale_text = managed.replace("missed retrieval", "optional retrieval");
    assert_ne!(
        stale_text, managed,
        "the test mutation must alter managed policy"
    );
    fs::write(&rules_path, &stale_text).unwrap();
    let stale = doctor(&doctor_options).unwrap();
    let stale_check = check(&stale, "repo.pixel-first");
    assert_eq!(stale_check.status, CheckStatus::Red, "{stale_check:?}");

    uninstall(&UninstallOptions {
        home: Some(home),
        repo: Some(repo),
        ..Default::default()
    })
    .unwrap();
    let remaining = fs::read_to_string(&rules_path).unwrap();
    assert_eq!(
        remaining,
        "# Existing project rules\nPreserve this instruction.\n"
    );
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

/// `rtk hook claude` registered globally (`rtk init -g`) runs in every
/// project's session: Claude Code merges the global, shared and personal
/// settings. A repo guard beside it would be a second Bash rewriter, so the
/// install holds the guard back and names the global hook; the global file
/// is only read.
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
            pixel_install::install::CheckStatus::Yellow,
            "{step:?}"
        );
        assert_eq!(
            step.summary,
            format!(
                "claude guard not installed: `rtk hook claude` in {} also rewrites shell calls — narrow that hook's `matcher` to tools other than Bash (an explicit list such as `Edit|Write` runs beside the guard), then run `pixel install --repo '{}'`; or keep it and work without the guard",
                global.display(),
                repo.display()
            ),
            "{step:?}"
        );
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
fn renamed_executable_task_gates_should_agree_between_repo_install_and_doctor() {
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
            let expected = if foreign {
                CheckStatus::Yellow
            } else {
                CheckStatus::Green
            };
            assert_eq!(
                step.status,
                if foreign {
                    StepStatus::Yellow
                } else {
                    StepStatus::Green
                },
                "shared={shared}, foreign={foreign}: {step:?}"
            );
            assert_eq!(read_json(&inherited), settings);
            let local = read_json(&repo.join(".claude/settings.local.json"));
            let pre = local["hooks"]["PreToolUse"].as_array().unwrap();
            if foreign {
                assert!(pre.is_empty(), "{local}");
            } else {
                assert_eq!(pre.len(), 1);
                assert_eq!(
                    pre[0]["hooks"][0]["command"],
                    format!(
                        "'{}' run-hook guard --provider claude",
                        exe.canonicalize().unwrap().display()
                    )
                );
            }
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
                checked.status, expected,
                "shared={shared}, foreign={foreign}: {checked:?}"
            );
            if foreign {
                assert!(
                    checked.summary.contains("keep-security-check"),
                    "{checked:?}"
                );
                assert!(!checked.summary.contains("task-event"), "{checked:?}");
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
fn a_personal_bash_hook_that_holds_the_guard_back_is_reported_with_what_to_do() {
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
    let hint = format!(
        " — narrow that hook's `matcher` to tools other than Bash (an explicit list such as `Edit|Write` runs beside the guard), then run `pixel install --repo '{}'`; or keep it and work without the guard",
        repo.display().to_string().replace('\'', "'\\''")
    );
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
    assert_eq!(step.status, StepStatus::Yellow, "{step:?}");
    assert_eq!(
        step.summary,
        format!(
            "claude guard not installed: `/usr/local/bin/my-guard` in {} also rewrites shell calls{hint}",
            global.display()
        )
    );
    assert_eq!(fs::read_to_string(&global).unwrap(), personal("Bash"));

    let held = claude_hooks();
    assert_eq!(held.status, CheckStatus::Yellow, "{held:?}");
    assert_eq!(
        held.summary,
        format!(
            "claude guard not installed: `/usr/local/bin/my-guard` in {} also rewrites shell calls{hint}",
            global.display()
        )
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
    assert_eq!(
        guarded.summary,
        format!(
            "claude guard registered in {}",
            repo.join(".claude/settings.local.json").display()
        )
    );
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
        pixel_install::install::CheckStatus::Yellow,
        "{step:?}"
    );
    assert!(
        step.summary
            .contains("unreadable (json: key must be a string at line 1 column 3), its PreToolUse hooks were not checked"),
        "{step:?}"
    );
    let local = read_json(&repo.join(".claude/settings.local.json"));
    assert_eq!(pixel_commands(&local, "PreToolUse").len(), 1, "{local}");
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
    // The global file is this repository's shared one: the stale guard moves
    // out of it into the personal file, and only the guard leaves.
    let global = read_json(&home.join(".claude/settings.json"));
    assert!(pixel_commands(&global, "PreToolUse").is_empty(), "{global}");
    let local = read_json(&home.join(".claude/settings.local.json"));
    assert_eq!(pixel_commands(&local, "PreToolUse").len(), 1, "{local}");
}

/// A full install of an older release left pixel's own guard in the global
/// file: the repo guard is held back like beside any rewriter, and the step
/// names the command that takes the global guard out.
#[test]
#[cfg(unix)]
fn repo_install_should_name_pixel_install_for_a_global_pixel_guard() {
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

    let report = install(&repo_install_options(&repo, &home)).unwrap();

    let step = report
        .steps
        .iter()
        .find(|s| s.id == "hooks.claude")
        .unwrap();
    assert!(
        step.summary.ends_with(
            "also rewrites shell calls — run `pixel install` to take pixel's global guard out"
        ),
        "{step:?}"
    );
}

/// A guard installed before `rtk init -g` (or by a release that did not read
/// the global file) runs beside the global RTK hook: doctor turns yellow with
/// the command that holds the guard back, and that command does. The guard
/// is then absent while the sessions run: doctor stays yellow, naming what
/// the user can do, and offers no command, since none can choose for them.
#[test]
#[cfg(unix)]
fn doctor_repo_claude_hooks_should_flag_a_guard_beside_a_global_rewriter() {
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
    assert_eq!(c.status, CheckStatus::Yellow, "{c:?}");
    assert!(
        c.summary.contains(&format!(
            "runs beside another shell rewriter (`rtk hook claude` in {}) — run `pixel install --repo '{}'`",
            home.join(".claude/settings.json").display(),
            repo.display()
        )),
        "{c:?}"
    );

    assert_eq!(
        c.fix,
        Some(format!("pixel install --repo '{}'", repo.display())),
        "rerunning the install does hold the guard back: {c:?}"
    );

    install(&repo_install_options(&repo, &home)).unwrap();
    let c = claude_check();
    assert_eq!(c.status, CheckStatus::Yellow, "{c:?}");
    assert!(
        c.summary.starts_with(&format!(
            "claude guard not installed: `rtk hook claude` in {} also rewrites shell calls — narrow that hook's `matcher`",
            home.join(".claude/settings.json").display()
        )),
        "{c:?}"
    );
    assert_eq!(c.fix, None, "{c:?}");
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

/// Claude's tool results carry the box already, but only the hook's
/// `systemMessage` reaches the user's transcript: the global install
/// registers the relay under PostToolUse on `Bash`, once, beside the edit
/// hook and any foreign PostToolUse group, however often it reruns.
#[test]
#[cfg(unix)]
fn claude_install_should_register_one_metrics_relay_on_bash_and_keep_foreign_groups() {
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
    let relays = metrics_relays(&settings, "claude");
    assert_eq!(relays.len(), 1, "{settings}");
    let (matcher, command, timeout) = &relays[0];
    assert_eq!(matcher, "Bash");
    assert!(
        command.starts_with('\'')
            && command.ends_with("/pixel' run-hook metrics --provider claude"),
        "{command}"
    );
    assert!(
        *timeout > 0,
        "a hook with no timeout inherits the default: {settings}"
    );
    let groups = settings["hooks"]["PostToolUse"].as_array().unwrap();
    assert!(groups.contains(&foreign), "foreign group kept: {settings}");
    assert!(
        groups.iter().any(|g| g["matcher"] == "Edit"),
        "the edit hook stays: {settings}"
    );
    assert!(
        metrics_relays(&settings, "codex").is_empty()
            && metrics_relays(&settings, "devin").is_empty(),
        "no other provider's relay in Claude's file: {settings}"
    );
}

/// A stale install without the relay is red with the install fix, green once
/// reinstalled; a Claude file that never held Pixel is still the existing
/// "not found" red, so the relay adds no new state for a foreign config.
#[test]
#[cfg(unix)]
fn doctor_should_flag_a_claude_install_missing_the_metrics_relay() {
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
    settings["hooks"]["PostToolUse"]
        .as_array_mut()
        .unwrap()
        .retain(|g| g["matcher"] != "Bash");
    // A foreign relay on the same matcher is not Pixel's.
    settings["hooks"]["PostToolUse"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":"/usr/bin/other run-hook metrics --provider claude"}]}));
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
            .is_some_and(|r| r.contains("PostToolUse(Bash)→metrics")),
        "{stale:?}"
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

/// Devin's repo-local config gets the relay on `exec` beside its guard. A
/// reinstall verifies instead of rewriting, a foreign PostToolUse group
/// survives, doctor is green, and a guard-only file from an older install
/// is red until reinstalled.
#[test]
#[cfg(unix)]
fn devin_repo_install_should_register_an_idempotent_exec_metrics_relay() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".devin")).unwrap();
    let foreign = serde_json::json!({"hooks":[{"type":"command","command":"vibe-island-bridge --source devin"}]});
    let config = repo.join(".devin/config.local.json");
    fs::write(
        &config,
        serde_json::to_string_pretty(
            &serde_json::json!({"hooks":{"PostToolUse":[foreign.clone()]}}),
        )
        .unwrap(),
    )
    .unwrap();
    let options = repo_install_options(&repo, &home);
    install(&options).unwrap();
    let first = fs::read(&config).unwrap();
    let report = install(&options).unwrap();
    assert_eq!(
        fs::read(&config).unwrap(),
        first,
        "reinstall is byte-identical"
    );
    let step = report.steps.iter().find(|s| s.id == "hooks.devin").unwrap();
    assert!(step.summary.contains("verified"), "{step:?}");

    let value = read_json(&config);
    let relays = metrics_relays(&value, "devin");
    assert_eq!(relays.len(), 1, "{value}");
    assert_eq!(relays[0].0, "exec");
    assert!(
        relays[0].1.ends_with("run-hook metrics --provider devin"),
        "{relays:?}"
    );
    assert!(
        value["hooks"]["PostToolUse"]
            .as_array()
            .unwrap()
            .contains(&foreign),
        "{value}"
    );

    let doctor_options = DoctorOptions {
        home: Some(home.clone()),
        executable_path: Some(fake_pixel_exe(&home)),
        repo_root: Some(repo.clone()),
        ..Default::default()
    };
    let green = doctor(&doctor_options).unwrap();
    assert_eq!(check(&green, "repo.devin-hooks").status, CheckStatus::Green);

    let mut stale = value.clone();
    stale["hooks"]["PostToolUse"]
        .as_array_mut()
        .unwrap()
        .retain(|g| g["matcher"] != "exec");
    stale["hooks"]["PostToolUse"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"matcher":"exec","hooks":[{"type":"command","command":"/usr/bin/other run-hook metrics --provider devin"}]}));
    fs::write(&config, serde_json::to_string_pretty(&stale).unwrap()).unwrap();
    let red = doctor(&doctor_options).unwrap();
    let devin = check(&red, "repo.devin-hooks");
    assert_eq!(devin.status, CheckStatus::Red, "{devin:?}");
    assert!(
        devin
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("metrics relay")),
        "{devin:?}"
    );
    install(&options).unwrap();
    assert_eq!(fs::read(&config).unwrap(), first, "reinstall restores it");
}

/// Uninstall takes only Pixel's relay out: Claude's global file keeps its
/// foreign PostToolUse group, and the Devin repo file keeps its own.
#[test]
#[cfg(unix)]
fn uninstall_should_remove_the_claude_and_devin_metrics_relays_only() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(repo.join(".devin")).unwrap();
    fs::create_dir_all(home.join(".claude")).unwrap();
    let foreign = serde_json::json!({"matcher":"Write","hooks":[{"type":"command","command":"fmt-on-write.sh"}]});
    for path in [
        home.join(".claude/settings.json"),
        repo.join(".devin/config.local.json"),
    ] {
        fs::write(
            path,
            serde_json::to_string_pretty(
                &serde_json::json!({"hooks":{"PostToolUse":[foreign.clone()]}}),
            )
            .unwrap(),
        )
        .unwrap();
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
        metrics_relays(&read_json(&home.join(".claude/settings.json")), "claude").len(),
        1
    );
    assert_eq!(
        metrics_relays(&read_json(&repo.join(".devin/config.local.json")), "devin").len(),
        1
    );

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
    for path in [
        home.join(".claude/settings.json"),
        repo.join(".devin/config.local.json"),
    ] {
        let value = read_json(&path);
        assert!(pixel_commands(&value, "PostToolUse").is_empty(), "{value}");
        assert_eq!(
            value["hooks"]["PostToolUse"],
            serde_json::json!([foreign]),
            "{}",
            path.display()
        );
    }
}

/// A repository at `$HOME`: Claude's relay lives in the global file (which
/// is also the repo's shared one) exactly once, the personal guard file
/// carries none, and Devin's repo-local relay sits in `~/.devin`, apart from
/// the global `~/.config/devin/config.json`.
#[test]
#[cfg(unix)]
fn repo_install_at_home_should_keep_each_metrics_relay_in_one_file() {
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
    assert_eq!(metrics_relays(&global, "claude").len(), 1, "{global}");
    let local = read_json(&home.join(".claude/settings.local.json"));
    assert!(metrics_relays(&local, "claude").is_empty(), "{local}");
    let devin = read_json(&home.join(".devin/config.local.json"));
    assert_eq!(metrics_relays(&devin, "devin").len(), 1, "{devin}");
    assert!(!home.join(".config/devin/config.json").exists());
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

/// Devin imports Claude's hooks, so its sessions previously received Pixel
/// only through the imported Claude text. The global install now registers
/// Devin's own lifecycle protocol — the three lifecycle hooks with
/// `--provider devin` — in `~/.config/devin/config.json`, but only when
/// Devin has been used on the machine. The metrics relay stays repo-scoped:
/// a global one would double the repo-local relay in every installed repo.
#[test]
#[cfg(unix)]
fn global_install_gives_devin_its_own_lifecycle_hooks_only_when_devin_is_in_use() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let devin_config = home.join(".config/devin/config.json");
    fs::create_dir_all(devin_config.parent().unwrap()).unwrap();
    fs::write(
        &devin_config,
        r#"{"agent":{"model":"swe-2-medium"},"hooks":{"SessionStart":[{"hooks":[{"command":"/opt/foreign --source devin","type":"command"}]}]}}"#,
    )
    .unwrap();
    install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .unwrap();

    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(&devin_config).unwrap()).unwrap();
    // The foreign session-start hook survives beside Pixel's group.
    let session = value["hooks"]["SessionStart"].as_array().unwrap();
    assert!(
        session.iter().any(|g| {
            g["hooks"].as_array().is_some_and(|h| {
                h.iter().any(|hook| {
                    hook["command"]
                        .as_str()
                        .is_some_and(|c| c.contains("foreign"))
                })
            })
        }),
        "{session:?}"
    );
    let has = |event: &str, verb: &str| {
        value["hooks"][event].as_array().is_some_and(|groups| {
            groups.iter().any(|g| {
                g["hooks"].as_array().is_some_and(|h| {
                    h.iter().any(|hook| {
                        hook["command"].as_str().is_some_and(|c| {
                            c.contains(&format!("run-hook {verb} --provider devin"))
                        })
                    })
                })
            })
        })
    };
    assert!(has("SessionStart", "session-start"));
    assert!(has("UserPromptSubmit", "prompt-submit"));
    // Post-compaction reads the repo manifest; it needs no provider argument.
    assert!(
        value["hooks"]["PostCompaction"]
            .as_array()
            .is_some_and(|groups| {
                groups.iter().any(|g| {
                    g["hooks"].as_array().is_some_and(|h| {
                        h.iter().any(|hook| {
                            hook["command"]
                                .as_str()
                                .is_some_and(|c| c.contains("run-hook post-compaction"))
                        })
                    })
                })
            }),
        "{:?}",
        value["hooks"]["PostCompaction"]
    );
    // The relay is repo-scoped: no global PostToolUse entry at all.
    assert!(
        value["hooks"].get("PostToolUse").is_none(),
        "{:?}",
        value["hooks"].get("PostToolUse")
    );

    // Idempotent: a second install rewrites nothing.
    let before = fs::read(&devin_config).unwrap();
    install(&InstallOptions {
        repo: None,
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .unwrap();
    let after = fs::read(&devin_config).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&before).unwrap(),
        serde_json::from_slice::<serde_json::Value>(&after).unwrap()
    );

    // Without a Devin config dir, the install creates none: a machine that
    // never ran Devin must not gain a config for it.
    let devinless = TempDir::new().unwrap();
    install(&InstallOptions {
        repo: None,
        home: Some(devinless.path().to_path_buf()),
        executable_path: Some(fake_pixel_exe(devinless.path())),
        claude_executable: Some(fake_claude_exe(devinless.path(), CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .unwrap();
    assert!(
        !devinless.path().join(".config/devin").exists(),
        "no Devin config on a machine without Devin"
    );
}

/// The `install.devin-hooks` check judges only what Pixel wrote, in the
/// crate's own suite (the mutants of the check's match arms are scored
/// here, not by the CLI suite): a machine without Devin is green-absent, a
/// Devin config without the hooks is red, foreign hook groups that name
/// the same verbs are still not Pixel's entries, and a real install turns
/// the check green.
#[test]
fn doctor_devin_hooks_judge_only_what_pixel_wrote() {
    let options = |home: &std::path::Path| DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    };
    let check = |report: &pixel_install::doctor::DoctorReport| {
        report
            .checks
            .iter()
            .find(|c| c.id == "install.devin-hooks")
            .expect("the check ran")
            .status
    };

    // No Devin on the machine: green-absent.
    let dir = TempDir::new().unwrap();
    let report = doctor(&options(dir.path())).unwrap();
    assert_eq!(check(&report), CheckStatus::Green, "{report:?}");

    // Devin used, nothing installed: red. The foreign hook groups name the
    // exact verbs (and the provider argument the two provider-decided
    // entries carry) without Pixel's binary, so only a real install can
    // turn the check green.
    let dir = TempDir::new().unwrap();
    let config = dir.path().join(".config/devin/config.json");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        r#"{"hooks":{
            "SessionStart":[{"hooks":[{"command":"/opt/foreign run-hook session-start --provider devin","type":"command"}]}],
            "UserPromptSubmit":[{"hooks":[{"command":"/opt/foreign run-hook prompt-submit --provider devin","type":"command"}]}],
            "PostCompaction":[{"hooks":[{"command":"/opt/foreign run-hook post-compaction","type":"command"}]}]
        }}"#,
    )
    .unwrap();
    let report = doctor(&options(dir.path())).unwrap();
    assert_eq!(check(&report), CheckStatus::Red, "{report:?}");
    let finding = report
        .checks
        .iter()
        .find(|c| c.id == "install.devin-hooks")
        .unwrap();
    let reason = finding.reason.as_deref().unwrap_or_default();
    for event in [
        "SessionStart→session-start",
        "UserPromptSubmit→prompt-submit",
        "PostCompaction→post-compaction",
    ] {
        assert!(reason.contains(event), "{event}: {reason}");
    }

    // A real install writes the three entries and the check turns green.
    install(&InstallOptions {
        repo: None,
        home: Some(dir.path().to_path_buf()),
        executable_path: Some(fake_pixel_exe(dir.path())),
        claude_executable: Some(fake_claude_exe(dir.path(), CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .unwrap();
    let report = doctor(&options(dir.path())).unwrap();
    assert_eq!(check(&report), CheckStatus::Green, "{report:?}");
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

/// Codex 0.159 skips a hook the user has not reviewed (`/hooks`), without a
/// message: Pixel's metrics and prompt-submit hooks installed but never reviewed are dormant, so
/// doctor must say so, with the step only the user can take, and turn green
/// once Codex's config records the review for that exact hook.
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
    let options = hook_review_options(home, &exe, "install.codex-hook-review", None);

    let report = doctor(&options).unwrap();
    let unreviewed = check(&report, "install.codex-hook-review");
    assert_eq!(unreviewed.status, CheckStatus::Yellow, "{unreviewed:?}");
    assert_eq!(
        unreviewed.summary,
        format!(
            "Codex skips 11 of the 11 Pixel hook(s) in {} until you review them (Interrupt #0.0, PostToolUse #0.0, PostToolUse #1.0, PreToolUse #0.0, SessionEnd #0.0, SessionStart #0.0, Stop #0.0, SubagentStart #0.0, SubagentStop #0.0, UserPromptSubmit #0.0, UserPromptSubmit #1.0): \
             start `codex` in this directory, run `/hooks` and trust them",
            hooks.display()
        )
    );
    assert_eq!(
        unreviewed.fix, None,
        "no command can review a hook for the user"
    );

    // A review recorded for another file, another event, or an entry without
    // a hash, is not this hook's review.
    let config = home.join(".codex/config.toml");
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
    for (event, group) in [
        ("interrupt", 0),
        ("post_tool_use", 0),
        ("post_tool_use", 1),
        ("pre_tool_use", 0),
        ("session_end", 0),
        ("session_start", 0),
        ("stop", 0),
        ("subagent_start", 0),
        ("subagent_stop", 0),
        ("user_prompt_submit", 0),
        ("user_prompt_submit", 1),
    ] {
        reviews.push_str(&format!(
            "\n[hooks.state.\"{}:{event}:{group}:0\"]\ntrusted_hash = \"sha256:fixture\"\n",
            hooks.display()
        ));
    }
    fs::write(&config, reviews).unwrap();
    let report = doctor(&options).unwrap();
    let reviewed = check(&report, "install.codex-hook-review");
    assert_eq!(reviewed.status, CheckStatus::Green, "{reviewed:?}");
    assert_eq!(
        reviewed.summary,
        format!(
            "Codex has reviewed the 11 Pixel hook(s) in {}",
            hooks.display()
        )
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

    let key = |event: &str, g: usize, h: usize| format!("{}:{event}:{g}:{h}", hooks.display());
    fs::write(
        home.join(".codex/config.toml"),
        format!(
            "[hooks.state.\"{}\"]\ntrusted_hash = \"sha256:a\"\n\n[hooks.state.\"{}\"]\ntrusted_hash = \"sha256:b\"\n",
            key("pre_tool_use", 0, 1),
            key("session_start", 0, 0)
        ),
    )
    .unwrap();
    let report = doctor(&options).unwrap();
    let reviewed = check(&report, "repo.codex-hook-review");
    assert_eq!(reviewed.status, CheckStatus::Green, "{reviewed:?}");
    assert_eq!(
        reviewed.summary,
        format!(
            "Codex has reviewed the 2 Pixel hook(s) in {}",
            hooks.display()
        )
    );
}
