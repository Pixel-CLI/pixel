//! Integration tests for pixel-install: doctor and install.

use std::fs;

use pixel_install::config::{MANAGED_BEGIN, MANAGED_END};
use pixel_install::doctor::{DoctorOptions, doctor};
use pixel_install::install::{InstallOptions, install};
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
    let path = dir.join("fake-pixel");
    fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
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
    // With no settings.json, install.mcp should be green (nothing to scrub).
    let mcp_check = report
        .checks
        .iter()
        .find(|c| c.id == "install.mcp")
        .expect("should have install.mcp check");
    assert!(
        mcp_check.status == pixel_install::doctor::CheckStatus::Green,
        "install.mcp should be green when no settings.json exists (nothing to scrub), got {:?}: {:?}",
        mcp_check.status,
        mcp_check.reason
    );
}

#[test]
fn doctor_after_install_reports_green_mcp() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Run install first — pixel is a CLI + hooks tool, not an MCP server,
    // so install only scrubs deprecated MCP entries and wires hooks. The
    // install.mcp doctor check verifies no deprecated servers linger, not
    // that pixel itself is registered as an MCP server.
    let install_opts = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    };
    install(&install_opts).expect("install");

    let doc_opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        ..Default::default()
    };
    let report = doctor(&doc_opts).expect("doctor after install");

    let mcp_check = report
        .checks
        .iter()
        .find(|c| c.id == "install.mcp")
        .expect("should have install.mcp check");
    assert!(
        mcp_check.status == pixel_install::doctor::CheckStatus::Green,
        "install.mcp should be green after install (no deprecated servers), got {:?}: {:?}",
        mcp_check.status,
        mcp_check.reason
    );
}

// ---------------------------------------------------------------------------
// install tests
// ---------------------------------------------------------------------------

#[test]
fn install_creates_config_with_managed_markers() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Pre-create a CLAUDE.md with some existing content.
    fs::write(home.join("CLAUDE.md"), "# My Project\n\nSome notes.\n").unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    };
    let report = install(&options).expect("install");

    assert!(report.ok, "install should succeed (ok=true)");
    assert!(report.summary.red == 0, "no red steps");
    assert!(
        report.summary.green > 0,
        "should have green steps"
    );

    // CLAUDE.md should now contain managed markers.
    let claude = fs::read_to_string(home.join("CLAUDE.md")).expect("CLAUDE.md");
    assert!(
        claude.contains(MANAGED_BEGIN),
        "CLAUDE.md should have managed begin marker"
    );
    assert!(
        claude.contains(MANAGED_END),
        "CLAUDE.md should have managed end marker"
    );
    // Original content should be preserved.
    assert!(
        claude.contains("Some notes."),
        "original CLAUDE.md content should be preserved"
    );
}

#[test]
fn install_is_idempotent() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    let options = InstallOptions {
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

    // CLAUDE.md should still have exactly one managed block.
    let claude = fs::read_to_string(home.join("CLAUDE.md")).unwrap_or_default();
    let begin_count = claude.matches(MANAGED_BEGIN).count();
    assert_eq!(
        begin_count, 1,
        "should have exactly one managed begin marker after re-install, got {begin_count}"
    );
}

#[test]
fn install_removes_deprecated_mcp_servers() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Pre-create settings.json with a deprecated server.
    let settings_dir = home.join(".claude");
    fs::create_dir_all(&settings_dir).unwrap();
    let settings = serde_json::json!({
        "mcpServers": {
            "usable-git": { "command": "old-binary", "args": ["mcp"] },
            "gitpixel": { "command": "old-binary", "args": ["mcp"] }
        }
    });
    fs::write(
        settings_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    };
    install(&options).expect("install");

    let after = fs::read_to_string(settings_dir.join("settings.json")).unwrap();
    assert!(
        !after.contains("\"usable-git\""),
        "deprecated usable-git server should be removed"
    );
    assert!(
        !after.contains("\"gitpixel\""),
        "deprecated gitpixel server should be removed"
    );
}

#[test]
fn install_leaves_settings_json_valid_after_install() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    let options = InstallOptions {
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
fn install_strips_stale_gitnexus_blocks() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Pre-create CLAUDE.md with two genuine, header-bounded stale blocks —
    // modeling how a real GitNexus-generated block actually looks (a
    // dedicated section with its own subsections), not a bare inline
    // mention. See `stale_block_removal_never_deletes_incidental_mentions`
    // below for the false-positive this design specifically avoids.
    let original = "\
# Project

## GitNexus — Code Intelligence
This project is indexed by GitNexus.

### Always Do
MUST run impact analysis before editing any symbol.

## codebase-memory setup
Legacy codebase-memory config lived here.

## Notes
Real project notes.
";
    fs::write(home.join("CLAUDE.md"), original).unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        dry_run: false,
    };
    let report = install(&options).expect("install");

    // The agent-config step should report stale blocks removed.
    let agent_step = report
        .steps
        .iter()
        .find(|s| s.id == "agent-config")
        .expect("should have agent-config step");
    assert!(
        agent_step
            .detail
            .as_ref()
            .map(|d| d.contains("stale_blocks_removed=2"))
            .unwrap_or(false),
        "should remove 2 stale blocks, got detail: {:?}",
        agent_step.detail
    );

    let claude = fs::read_to_string(home.join("CLAUDE.md")).unwrap();
    assert!(
        !claude.to_lowercase().contains("gitnexus"),
        "GitNexus section (header + all its subsections) should be stripped entirely"
    );
    assert!(
        !claude.contains("codebase-memory"),
        "codebase-memory section should be stripped entirely"
    );
    assert!(
        !claude.contains("Legacy codebase-memory config"),
        "the stale section's BODY content must also be gone, not just its header"
    );
    assert!(
        claude.contains("Real project notes."),
        "non-stale content (a later, unrelated section) should be preserved"
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
    assert_eq!(removed, 0, "no genuine stale block header exists; nothing should be removed");
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
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: true,
    };
    let report = install(&options).expect("dry-run install");

    // pixel is a CLI + hooks tool, not an MCP server — there is no mcp.pixel
    // step anymore. A dry-run on a clean home should report ok (all steps
    // green, nothing to write) and leave nothing on disk.
    assert!(report.dry_run, "report should mark itself as a dry run");
    assert!(report.ok, "dry-run on clean home should report ok: {report:?}");

    // Nothing should exist on disk: no .claude dir, no CLAUDE.md, no hooks.
    assert!(
        !home.join(".claude").exists(),
        ".claude directory must not be created in dry-run mode"
    );
    assert!(
        !home.join("CLAUDE.md").exists(),
        "CLAUDE.md must not be created in dry-run mode"
    );
}

// ---------------------------------------------------------------------------
// deprecated-MCP scrub (pixel is a CLI + hooks tool, not an MCP server —
// install removes the retired usable-git/gitpixel/sniper MCP entries
// unconditionally, since pixel replaces them via Bash + the guard hook.)
// ---------------------------------------------------------------------------

#[test]
fn install_removes_deprecated_mcp_servers_unconditionally() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let settings_dir = home.join(".claude");
    fs::create_dir_all(&settings_dir).unwrap();
    let settings_path = settings_dir.join("settings.json");
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": {
                "usable-git": { "command": "old-usable-git-binary", "args": ["mcp"] },
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    };
    let report = install(&options).expect("install");

    // install should succeed — pixel is not an MCP server, so there's no
    // mcp.pixel step to fail. The deprecated usable-git entry is removed
    // unconditionally.
    assert!(report.ok, "install should succeed: {report:?}");
    assert!(
        report.steps.iter().all(|s| s.id != "mcp.pixel"),
        "no mcp.pixel step should exist — pixel is not an MCP server"
    );

    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).expect("still valid JSON");
    assert!(
        after["mcpServers"].get("usable-git").is_none(),
        "the deprecated usable-git MCP server must be removed — pixel replaces it via Bash + the guard hook"
    );
    assert!(
        after["mcpServers"].get("pixel").is_none(),
        "pixel must NOT register an MCP server entry — it is a CLI + hooks tool"
    );
}

#[test]
fn dry_run_leaves_pre_existing_files_byte_identical() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Pre-create real state as if a previous non-dry-run install ran.
    let real_options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        dry_run: false,
    };
    install(&real_options).expect("real install");

    let settings_path = home.join(".claude").join("settings.json");
    let claude_path = home.join("CLAUDE.md");
    let before_settings = fs::read(&settings_path).unwrap();
    let before_claude = fs::read(&claude_path).unwrap();

    // A dry-run install afterwards must not touch anything, even though a
    // real install already exists (idempotent no-op path).
    let dry_options = InstallOptions {
        dry_run: true,
        ..real_options
    };
    let report = install(&dry_options).expect("dry-run install over existing state");
    assert!(report.dry_run);

    let after_settings = fs::read(&settings_path).unwrap();
    let after_claude = fs::read(&claude_path).unwrap();
    assert_eq!(
        before_settings, after_settings,
        "dry-run must not modify settings.json"
    );
    assert_eq!(
        before_claude, after_claude,
        "dry-run must not modify CLAUDE.md"
    );

    // And it must not have written any backup files either.
    let claude_dir_entries: Vec<String> = fs::read_dir(home)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !claude_dir_entries.iter().any(|n| n.contains("pixel-bak")),
        "dry-run must not create backup files, got: {claude_dir_entries:?}"
    );
}

// ---------------------------------------------------------------------------
// backup tests
// ---------------------------------------------------------------------------

#[test]
fn reinstall_backs_up_claude_md_only_when_content_actually_changes() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Pre-create a CLAUDE.md with hand-written content that install will
    // need to rewrite (adds managed markers => content changes).
    fs::write(home.join("CLAUDE.md"), "# Project\n\nHand-written notes.\n").unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        dry_run: false,
    };
    install(&options).expect("install 1");

    let backups_after_first: Vec<_> = fs::read_dir(home)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("CLAUDE.md.pixel-bak"))
        .collect();
    assert_eq!(
        backups_after_first.len(),
        1,
        "first install should back up the original hand-written CLAUDE.md exactly once"
    );

    // The backup should contain the ORIGINAL (pre-managed-markers) content.
    let backup_content = fs::read_to_string(backups_after_first[0].path()).unwrap();
    assert!(
        backup_content.contains("Hand-written notes."),
        "backup should preserve the original content verbatim, got: {backup_content}"
    );
    assert!(
        !backup_content.contains(MANAGED_BEGIN),
        "backup should be the pre-rewrite content, not the managed version"
    );

    // Second install: content won't change (idempotent), so no new backup.
    install(&options).expect("install 2");
    let backups_after_second: Vec<_> = fs::read_dir(home)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("CLAUDE.md.pixel-bak"))
        .collect();
    assert_eq!(
        backups_after_second.len(),
        1,
        "idempotent re-install must not create a second backup when nothing changed"
    );
}

// ---------------------------------------------------------------------------
// realistic settings.json fixture — models a real ~/.claude/settings.json
// with pre-existing MCP servers, multiple hook events (including a
// PreToolUse entry referencing the OLD guard hook by its real command
// shape, and a SessionStart array with multiple unrelated matcher groups
// from other tools), and unrelated top-level keys.
// ---------------------------------------------------------------------------

fn realistic_settings_json() -> serde_json::Value {
    serde_json::json!({
        "model": "claude-sonnet-5",
        "mcpServers": {
            "usable-git": { "command": "old-usable-git-binary", "args": ["mcp"] },
            "github": { "command": "gh-mcp-server", "args": ["serve"] },
        },
        "hooks": {
            "PreToolUse": [
                { "matcher": "*", "hooks": [{ "type": "command", "command": "/bin/sh -c 'echo bridge'" }] },
                {
                    "matcher": "Grep|Glob",
                    "hooks": [{
                        "type": "command",
                        "command": "~/.claude/hooks/gitpixel-targets-guard",
                        "timeout": 5,
                    }],
                },
            ],
            "SessionStart": [
                { "matcher": "startup", "hooks": [{ "type": "command", "command": "~/.claude/hooks/cbm-session-reminder" }] },
                { "matcher": "resume", "hooks": [{ "type": "command", "command": "~/.claude/hooks/cbm-session-reminder" }] },
                { "matcher": "clear", "hooks": [{ "type": "command", "command": "~/.claude/hooks/cbm-session-reminder" }] },
            ],
            "Stop": [
                { "hooks": [{ "type": "command", "command": "~/.claude/hooks/verify-before-done", "timeout": 10 }] },
            ],
        },
    })
}

#[test]
fn install_against_realistic_settings_json_is_safe() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let settings_dir = home.join(".claude");
    fs::create_dir_all(&settings_dir).unwrap();
    let settings_path = settings_dir.join("settings.json");
    let before = realistic_settings_json();
    fs::write(&settings_path, serde_json::to_string_pretty(&before).unwrap()).unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    };
    let report = install(&options).expect("install against realistic settings.json");
    assert!(report.ok, "install should succeed: {report:?}");

    // (a) settings.json must still be valid JSON.
    let raw = fs::read_to_string(&settings_path).expect("settings.json readable");
    let after: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("settings.json corrupted: {e}\ncontent:\n{raw}"));

    // (b) unrelated top-level content and unrelated entries survive.
    assert_eq!(after["model"], "claude-sonnet-5", "unrelated top-level key must survive");
    assert_eq!(
        after["mcpServers"]["github"]["command"], "gh-mcp-server",
        "unrelated MCP server must survive"
    );
    assert!(
        after["mcpServers"].get("usable-git").is_none(),
        "deprecated usable-git MCP server should be removed"
    );
    let stop_hooks = after["hooks"]["Stop"].as_array().expect("Stop hooks array survives");
    assert_eq!(stop_hooks.len(), 1, "unrelated Stop hook must survive untouched");
    assert_eq!(stop_hooks[0]["hooks"][0]["command"], "~/.claude/hooks/verify-before-done");

    // The three pre-existing, unrelated SessionStart entries from another
    // tool must all survive — install must MERGE, not overwrite.
    let session_start = after["hooks"]["SessionStart"]
        .as_array()
        .expect("SessionStart array survives");
    let cbm_entries = session_start
        .iter()
        .filter(|e| {
            e["hooks"][0]["command"]
                .as_str()
                .map(|c| c.contains("cbm-session-reminder"))
                .unwrap_or(false)
        })
        .count();
    assert_eq!(
        cbm_entries, 3,
        "all 3 pre-existing SessionStart entries from another tool must survive, got session_start={session_start:?}"
    );

    // (c) pixel's own entries were correctly added. pixel is a CLI + hooks
    // tool, not an MCP server — so we check the SessionStart hook was
    // merged in, NOT that a pixel MCP server entry was registered.
    assert!(
        after["mcpServers"].get("pixel").is_none(),
        "pixel must NOT be registered as an MCP server — it is a CLI + hooks tool"
    );
    let has_pixel_session_start = session_start.iter().any(|e| {
        e["hooks"][0]["command"]
            .as_str()
            .map(|c| c.contains("hook session-start"))
            .unwrap_or(false)
    });
    assert!(has_pixel_session_start, "pixel's own SessionStart entry should be present");

    // The old guard hook's PreToolUse entry (matcher "Grep|Glob") is
    // repointed by scrub_settings_json, then REPLACED by install_guard_hook
    // with a new entry carrying the full GUARD_MATCHER (covers both Claude
    // and Devin tool names). The old entry's timeout is not preserved —
    // the new entry doesn't need it (the guard script exits quickly).
    let pre_tool_use = after["hooks"]["PreToolUse"].as_array().expect("PreToolUse array survives");
    assert_eq!(pre_tool_use.len(), 2, "PreToolUse: bridge entry + new pixel guard entry");
    let guard_entry = pre_tool_use
        .iter()
        .find(|e| {
            e["hooks"].as_array()
                .map(|hs| hs.iter().any(|h| h["command"].as_str() == Some("~/.claude/hooks/pixel-targets-guard")))
                .unwrap_or(false)
        })
        .expect("pixel guard PreToolUse entry present");
    // The new matcher covers both Claude and Devin tool names.
    assert_eq!(
        guard_entry["matcher"], pixel_install::config::GUARD_MATCHER,
        "guard entry should use the full matcher covering Claude + Devin tool names"
    );
    let bridge_entry = pre_tool_use
        .iter()
        .find(|e| e["matcher"] == "*")
        .expect("unrelated bridge PreToolUse entry survives");
    assert_eq!(bridge_entry["hooks"][0]["command"], "/bin/sh -c 'echo bridge'");

    // (d) a .bak was written before the destructive rewrite. `install()`
    // touches settings.json across THREE separate steps (scrub deprecated
    // entries, wire the PreToolUse guard entry, wire the SessionStart
    // hook) — each backs up independently when its own write actually
    // changes the content, so multiple backup files can legitimately exist
    // in one install run. `fs::read_dir`'s order is unspecified, so pick
    // the EARLIEST one by its embedded nanosecond timestamp (filenames are
    // `settings.json.pixel-bak.<nanos>-<seq>`) to find the TRUE pre-install
    // snapshot rather than an arbitrary intermediate one.
    let mut backups: Vec<_> = fs::read_dir(&settings_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("settings.json.pixel-bak"))
        .collect();
    assert!(!backups.is_empty(), "settings.json must be backed up before rewrite");
    backups.sort_by_key(|e| {
        e.file_name()
            .to_string_lossy()
            .rsplit("pixel-bak.")
            .next()
            .and_then(|rest| rest.split('-').next())
            .and_then(|nanos| nanos.parse::<u128>().ok())
            .unwrap_or(u128::MAX)
    });
    let earliest_backup = &backups[0];
    let backup_content = fs::read_to_string(earliest_backup.path()).unwrap();
    let backup_parsed: serde_json::Value = serde_json::from_str(&backup_content)
        .unwrap_or_else(|e| panic!("backup should itself be valid JSON: {e}"));
    assert_eq!(
        backup_parsed["mcpServers"]["usable-git"]["command"], "old-usable-git-binary",
        "the earliest backup should preserve the ORIGINAL pre-install content, got {backups:?}"
    );

    // Re-running install must stay idempotent: no duplicate pixel entries,
    // no duplicate MCP server, no re-corruption.
    install(&options).expect("second install");
    let raw2 = fs::read_to_string(&settings_path).unwrap();
    let after2: serde_json::Value = serde_json::from_str(&raw2).expect("still valid JSON after re-install");
    let session_start2 = after2["hooks"]["SessionStart"].as_array().unwrap();
    let pixel_count2 = session_start2
        .iter()
        .filter(|e| {
            e["hooks"][0]["command"]
                .as_str()
                .map(|c| c.contains("hook session-start"))
                .unwrap_or(false)
        })
        .count();
    assert_eq!(pixel_count2, 1, "re-install must not duplicate pixel's own SessionStart entry");
    assert_eq!(session_start2.len(), 4, "3 foreign entries + 1 pixel entry, stable across re-install");
}

// ---------------------------------------------------------------------------
// capability advertisement — the SessionStart block is derived from the live
// op registry in pixel-proto (`SESSION_CAPABILITIES`, tested exhaustively
// there); the old hand-maintained duplicate registry in this crate is gone.
// ---------------------------------------------------------------------------

#[test]
fn session_capabilities_registry_is_live_and_excludes_internal_ops() {
    let caps = pixel_proto::op::SESSION_CAPABILITIES;
    for expected in ["search", "targets", "publish", "push", "ship", "resolve", "impact"] {
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
        home: Some(home.to_path_buf()),
        executable_path: None,
        dry_run: false,
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
        home: Some(home.to_path_buf()),
        executable_path: None,
        dry_run: false,
    };
    install(&options).expect("install on fresh home");
    let claude = fs::read_to_string(home.join("CLAUDE.md"))
        .expect("CLAUDE.md should be created even when no agent-config file pre-existed");
    assert!(claude.contains(MANAGED_BEGIN));
    assert!(claude.contains(MANAGED_END));
}
