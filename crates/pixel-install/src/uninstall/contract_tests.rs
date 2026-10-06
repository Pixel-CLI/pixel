// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the uninstall steps: what each step removes, what it
//! must leave in place (the user's own entries and text, a binary a package
//! manager owns), what a dry run must not touch, and the undo copy every
//! destructive write leaves behind.

use super::*;
use serde_json::json;

/// A Claude-style hook group whose single command is `command`.
fn group(matcher: &str, command: &str) -> serde_json::Value {
    json!({"matcher": matcher, "hooks": [{"type": "command", "command": command}]})
}

/// The `.pixel-bak.` copies present in `dir`.
fn backups_in(dir: &Path) -> Vec<PathBuf> {
    find_backups(&[dir.to_path_buf()])
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn managed(body: &str) -> String {
    format!(
        "{}\n{body}\n{}\n",
        config::MANAGED_BEGIN,
        config::MANAGED_END
    )
}

// -- zcode -------------------------------------------------------------------

#[test]
fn remove_zcode_hooks_should_skip_without_creating_a_config_when_zcode_is_absent() {
    let home = tempfile::tempdir().unwrap();
    let step = remove_zcode_hooks(home.path(), false).unwrap();
    assert_eq!(step.id, "hooks.zcode");
    assert_eq!(step.status, CheckStatus::Green);
    assert_eq!(step.summary, "no zcode config — skipping");
    assert!(step.detail.is_none());
    assert!(!home.path().join(config::ZCODE_CONFIG_FILE).exists());
}

#[test]
fn remove_zcode_hooks_should_drop_emptied_events_and_hooks_but_keep_other_settings_when_only_pixel_hooked()
 {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(config::ZCODE_CONFIG_FILE);
    let original = json!({
        "theme": "dark",
        "hooks": {"events": {
            "PreToolUse": [group("Bash", "/opt/pixel run-hook guard --provider zcode")],
        }},
    });
    install::write_settings(&path, &original, false).unwrap();

    let step = remove_zcode_hooks(home.path(), false).unwrap();

    assert_eq!(step.summary, "removed 1 zcode hook entry/entries");
    assert_eq!(
        install::read_settings(&path).unwrap(),
        json!({"theme": "dark"}),
        "an emptied event, then `events`, then `hooks` go; the user's keys stay"
    );
    let backups = backups_in(path.parent().unwrap());
    assert_eq!(backups.len(), 1, "the rewrite keeps one undo copy");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(&backups[0]).unwrap())
            .unwrap(),
        original,
        "the undo copy holds the file as it was"
    );
    assert!(
        step.detail.unwrap().contains(&path.display().to_string()),
        "the step names the file it rewrote"
    );
}

#[test]
fn remove_zcode_hooks_should_keep_foreign_hooks_and_their_event_when_an_event_mixes_both() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(config::ZCODE_CONFIG_FILE);
    let foreign = group("Bash", "lint-check");
    install::write_settings(
        &path,
        &json!({"hooks": {
            "enabled": true,
            "events": {
                "PreToolUse": [group("Bash", "/opt/pixel run-hook guard --provider zcode"), foreign.clone()],
                "Stop": [group("", "notify-done")],
            },
        }}),
        false,
    )
    .unwrap();

    let step = remove_zcode_hooks(home.path(), false).unwrap();

    assert_eq!(step.summary, "removed 1 zcode hook entry/entries");
    assert_eq!(
        install::read_settings(&path).unwrap(),
        json!({"hooks": {
            "enabled": true,
            "events": {"PreToolUse": [foreign], "Stop": [group("", "notify-done")]},
        }}),
        "only pixel's group goes; the foreign group, its event, the untouched event and the other hook settings stay"
    );
}

#[test]
fn remove_zcode_hooks_should_not_rewrite_or_back_up_a_config_without_pixel_hooks() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(config::ZCODE_CONFIG_FILE);
    let text = "{\"hooks\":{\"events\":{\"Stop\":[{\"matcher\":\"\",\"hooks\":[{\"type\":\"command\",\"command\":\"notify-done\"}]}]}}}";
    write(&path, text);

    let step = remove_zcode_hooks(home.path(), false).unwrap();

    assert_eq!(step.summary, "removed 0 zcode hook entry/entries");
    assert_eq!(fs::read_to_string(&path).unwrap(), text, "byte for byte");
    assert!(backups_in(path.parent().unwrap()).is_empty());
}

#[test]
fn remove_zcode_hooks_should_strip_the_agents_managed_block_and_keep_the_users_text() {
    let home = tempfile::tempdir().unwrap();
    install::write_settings(
        &home.path().join(config::ZCODE_CONFIG_FILE),
        &json!({}),
        false,
    )
    .unwrap();
    let agents = home.path().join(".zcode/AGENTS.md");
    write(
        &agents,
        &format!("# mine\n{}keep me\n", managed("pixel rules")),
    );

    let step = remove_zcode_hooks(home.path(), false).unwrap();

    assert_eq!(
        step.summary,
        "removed 0 zcode hook entry/entries + stripped AGENTS.md managed block"
    );
    assert_eq!(fs::read_to_string(&agents).unwrap(), "# mine\nkeep me\n");
    assert_eq!(backups_in(agents.parent().unwrap()).len(), 1);
}

#[test]
fn remove_zcode_hooks_should_report_but_write_nothing_when_dry_run() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(config::ZCODE_CONFIG_FILE);
    let original = json!({"hooks": {"events": {
        "PreToolUse": [group("Bash", "/opt/pixel run-hook guard --provider zcode")],
    }}});
    install::write_settings(&path, &original, false).unwrap();
    let agents = home.path().join(".zcode/AGENTS.md");
    let agents_text = format!("top\n{}", managed("pixel rules"));
    write(&agents, &agents_text);

    let step = remove_zcode_hooks(home.path(), true).unwrap();

    assert_eq!(
        step.summary,
        "[dry-run] would report: removed 1 zcode hook entry/entries + stripped AGENTS.md managed block"
    );
    assert_eq!(install::read_settings(&path).unwrap(), original);
    assert_eq!(fs::read_to_string(&agents).unwrap(), agents_text);
    assert!(backups_in(path.parent().unwrap()).is_empty());
    assert!(backups_in(agents.parent().unwrap()).is_empty());
}

// -- pi ------------------------------------------------------------------------

#[test]
fn remove_pi_extension_dir_should_skip_when_pi_is_not_configured() {
    let home = tempfile::tempdir().unwrap();
    let step = remove_pi_extension(home.path(), false).unwrap();
    assert_eq!(step.id, "hooks.pi");
    assert_eq!(step.summary, "no pi config dir — skipping");
    assert!(step.detail.is_none());
}

#[test]
fn remove_pi_extension_dir_should_remove_the_guard_and_strip_the_block_keeping_an_undo_copy_of_each()
 {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(config::PI_CONFIG_DIR);
    let ext = dir.join("extensions/pixel-guard.ts");
    write(
        &ext,
        &format!("{}\nexport default () => {{}};\n", config::MANAGED_BEGIN),
    );
    let agents = dir.join("AGENTS.md");
    write(&agents, &format!("user rules\n{}", managed("pixel")));

    let step = remove_pi_extension_dir(&dir, false).unwrap();

    assert_eq!(
        step.summary,
        "removed pi guard extension + stripped AGENTS.md managed block"
    );
    assert!(!ext.exists());
    assert_eq!(fs::read_to_string(&agents).unwrap(), "user rules\n");
    assert_eq!(
        backups_in(&dir.join("extensions")).len(),
        1,
        "the deleted extension keeps its undo copy"
    );
    assert_eq!(backups_in(&dir).len(), 1, "so does the rewritten AGENTS.md");
    assert_eq!(step.detail, Some(format!("ext={}", ext.display())));
}

#[test]
fn remove_pi_extension_dir_should_leave_an_unmanaged_agents_file_untouched() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(config::PI_CONFIG_DIR);
    let agents = dir.join("AGENTS.md");
    write(&agents, "only the user's rules\n");

    let step = remove_pi_extension_dir(&dir, false).unwrap();

    assert_eq!(step.summary, "no pi extension found");
    assert_eq!(
        fs::read_to_string(&agents).unwrap(),
        "only the user's rules\n"
    );
    assert!(backups_in(&dir).is_empty());
}

#[test]
fn remove_pi_extension_dir_should_keep_every_file_when_dry_run() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(config::PI_CONFIG_DIR);
    let ext = dir.join("extensions/pixel-guard.ts");
    write(&ext, &format!("{}\nguard\n", config::MANAGED_BEGIN));
    let agents = dir.join("AGENTS.md");
    let text = managed("pixel");
    write(&agents, &text);

    let step = remove_pi_extension_dir(&dir, true).unwrap();

    assert_eq!(
        step.summary,
        "[dry-run] would report: removed pi guard extension + stripped AGENTS.md managed block"
    );
    assert!(ext.is_file());
    assert_eq!(fs::read_to_string(&agents).unwrap(), text);
}

// -- rule source and agent prompt ----------------------------------------------

#[test]
fn remove_rule_source_should_skip_when_no_rule_file_exists() {
    let home = tempfile::tempdir().unwrap();
    let step = remove_rule_source(home.path(), false).unwrap();
    assert_eq!(step.id, "rule.source");
    assert_eq!(step.summary, "no rule source file — skipping");
}

#[test]
fn remove_rule_source_should_delete_the_rule_and_keep_its_undo_copy() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(config::PIXEL_RULES_REL);
    write(&path, "# pixel rule\n");

    let step = remove_rule_source(home.path(), false).unwrap();

    assert_eq!(step.summary, "removed pixel rule source file");
    assert!(!path.exists());
    let backups = backups_in(path.parent().unwrap());
    assert_eq!(backups.len(), 1);
    assert_eq!(fs::read_to_string(&backups[0]).unwrap(), "# pixel rule\n");
}

#[test]
fn remove_rule_source_should_keep_the_rule_when_dry_run() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(config::PIXEL_RULES_REL);
    write(&path, "# pixel rule\n");
    let step = remove_rule_source(home.path(), true).unwrap();
    assert_eq!(
        step.summary,
        "[dry-run] would report: removed pixel rule source file"
    );
    assert!(path.is_file());
    assert!(backups_in(path.parent().unwrap()).is_empty());
}

#[test]
fn remove_agent_prompt_should_skip_when_no_prompt_file_exists() {
    let home = tempfile::tempdir().unwrap();
    let step = remove_agent_prompt(home.path(), false).unwrap();
    assert_eq!(step.id, "agent-prompt");
    assert_eq!(step.summary, "no agent-prompt file — skipping");
}

#[test]
fn remove_agent_prompt_should_remove_both_prompts_and_leave_a_foreign_pi_prompt_alone() {
    let home = tempfile::tempdir().unwrap();
    let share = home.path().join(".local/share/pixel");
    write(&share.join("agent-prompt.md"), "prompt");
    write(&share.join(install::SUBAGENT_PROMPT_FILE), "sub");
    let pi = home.path().join(install::PI_PROMPT_REL);
    write(&pi, "the user's own system prompt\n");

    let step = remove_agent_prompt(home.path(), false).unwrap();

    assert_eq!(
        step.summary,
        "removed agent-prompt.md and subagent-prompt.md"
    );
    assert!(!share.join("agent-prompt.md").exists());
    assert!(!share.join(install::SUBAGENT_PROMPT_FILE).exists());
    assert_eq!(
        fs::read_to_string(&pi).unwrap(),
        "the user's own system prompt\n",
        "a pi prompt without pixel's block is the user's"
    );
}

#[test]
fn remove_agent_prompt_should_delete_a_pi_prompt_holding_only_pixels_block() {
    let home = tempfile::tempdir().unwrap();
    let pi = home.path().join(install::PI_PROMPT_REL);
    write(&pi, &managed("pixel prompt"));

    let step = remove_agent_prompt(home.path(), false).unwrap();

    assert_eq!(step.summary, "removed the pi prompt file");
    assert!(!pi.exists(), "nothing of the user's was left in it");

    // With both prompts beside it, the summary names all three.
    let share = home.path().join(".local/share/pixel");
    write(&share.join("agent-prompt.md"), "prompt");
    write(&share.join(install::SUBAGENT_PROMPT_FILE), "sub");
    write(&pi, &managed("pixel prompt"));
    let step = remove_agent_prompt(home.path(), false).unwrap();
    assert_eq!(
        step.summary,
        "removed agent-prompt.md, subagent-prompt.md and the pi prompt file"
    );
}

#[test]
fn remove_agent_prompt_should_name_only_the_files_it_removed() {
    let home = tempfile::tempdir().unwrap();
    let share = home.path().join(".local/share/pixel");
    write(&share.join(install::SUBAGENT_PROMPT_FILE), "sub");

    let step = remove_agent_prompt(home.path(), false).unwrap();

    assert_eq!(step.summary, "removed subagent-prompt.md");
    assert!(!share.join(install::SUBAGENT_PROMPT_FILE).exists());
}

#[test]
fn remove_agent_prompt_should_skip_a_pi_prompt_holding_only_the_users_text() {
    let home = tempfile::tempdir().unwrap();
    let pi = home.path().join(install::PI_PROMPT_REL);
    write(&pi, "the user's own system prompt\n");

    let step = remove_agent_prompt(home.path(), false).unwrap();

    assert_eq!(step.summary, "no agent-prompt file — skipping");
    assert_eq!(
        fs::read_to_string(&pi).unwrap(),
        "the user's own system prompt\n"
    );
}

#[test]
fn remove_agent_prompt_should_keep_the_users_text_around_pixels_pi_block() {
    let home = tempfile::tempdir().unwrap();
    let pi = home.path().join(install::PI_PROMPT_REL);
    write(&pi, &format!("be terse\n{}", managed("pixel prompt")));

    let step = remove_agent_prompt(home.path(), false).unwrap();

    assert_eq!(
        step.summary,
        "removed the pixel block from APPEND_SYSTEM.md, kept the text around it"
    );
    assert_eq!(fs::read_to_string(&pi).unwrap(), "be terse\n");
    assert_eq!(backups_in(pi.parent().unwrap()).len(), 1);
}

#[test]
fn remove_agent_prompt_should_touch_no_file_when_dry_run() {
    let home = tempfile::tempdir().unwrap();
    let share = home.path().join(".local/share/pixel");
    write(&share.join("agent-prompt.md"), "prompt");
    let pi = home.path().join(install::PI_PROMPT_REL);
    let text = format!("be terse\n{}", managed("pixel prompt"));
    write(&pi, &text);

    remove_agent_prompt(home.path(), true).unwrap();

    assert!(share.join("agent-prompt.md").is_file());
    assert_eq!(fs::read_to_string(&pi).unwrap(), text);
}

// -- binary --------------------------------------------------------------------

#[test]
fn removal_target_should_prefer_the_named_binary_then_the_running_one_then_local_bin() {
    let home = Path::new("/home/u");
    let named = UninstallOptions {
        binary_path: Some("/x/pixel".into()),
        running_binary: Some("/y/pixel".into()),
        ..UninstallOptions::default()
    };
    assert_eq!(
        removal_target(&named, home),
        RemovalTarget {
            path: "/x/pixel".into(),
            explicit: true
        }
    );
    let running = UninstallOptions {
        running_binary: Some("/y/pixel".into()),
        ..UninstallOptions::default()
    };
    assert_eq!(
        removal_target(&running, home),
        RemovalTarget {
            path: "/y/pixel".into(),
            explicit: false
        }
    );
    assert_eq!(
        removal_target(&UninstallOptions::default(), home),
        RemovalTarget {
            path: "/home/u/.local/bin/pixel".into(),
            explicit: false
        }
    );
}

#[test]
fn package_manager_owner_should_name_homebrew_and_mise_trees_and_nothing_else() {
    assert_eq!(
        package_manager_owner(Path::new(
            "/nonexistent/homebrew/Cellar/pixel/0.7.0/bin/pixel"
        )),
        Some(("Homebrew", "`brew uninstall pixel`".to_string()))
    );
    assert_eq!(
        package_manager_owner(Path::new(
            "/nonexistent/.local/share/mise/installs/ubi-pixel-cli-pixel/0.7.0/pixel"
        )),
        Some((
            "mise",
            "`mise uninstall` on the tool installed under `installs/ubi-pixel-cli-pixel`"
                .to_string()
        ))
    );
    assert_eq!(
        package_manager_owner(Path::new("/nonexistent/.local/bin/pixel")),
        None
    );
    assert_eq!(
        package_manager_owner(Path::new("/nonexistent/Cellar")),
        None,
        "a marker with nothing after it names no formula"
    );
}

#[cfg(unix)]
#[test]
fn package_manager_owner_should_follow_a_symlink_into_the_cellar() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("Cellar/pixel/0.7.0/bin/pixel");
    write(&real, "bin");
    let link = root.path().join("bin/pixel");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert_eq!(
        package_manager_owner(&link),
        Some(("Homebrew", "`brew uninstall pixel`".to_string())),
        "the brew-linked `bin/pixel` is the Cellar file"
    );
}

#[test]
fn remove_binary_should_skip_when_the_target_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pixel");
    let step = remove_binary(
        &RemovalTarget {
            path: path.clone(),
            explicit: false,
        },
        false,
    )
    .unwrap();
    assert_eq!(step.status, CheckStatus::Green);
    assert_eq!(step.summary, "no binary found — skipping");
    assert_eq!(step.detail, Some(format!("path={}", path.display())));
}

#[test]
fn remove_binary_should_leave_a_package_manager_binary_unless_the_user_named_it() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("Cellar/pixel/0.7.0/bin/pixel");
    write(&path, "bin");

    let implicit = remove_binary(
        &RemovalTarget {
            path: path.clone(),
            explicit: false,
        },
        false,
    )
    .unwrap();
    assert_eq!(implicit.status, CheckStatus::Yellow);
    assert_eq!(
        implicit.summary,
        "left the pixel binary to Homebrew: remove it with `brew uninstall pixel`"
    );
    assert!(
        path.is_file(),
        "Homebrew still lists it: its command removes both"
    );

    let named = remove_binary(
        &RemovalTarget {
            path: path.clone(),
            explicit: true,
        },
        false,
    )
    .unwrap();
    assert_eq!(named.status, CheckStatus::Green);
    assert_eq!(named.summary, "removed pixel binary");
    assert!(
        !path.exists(),
        "a binary the user named goes, whoever owns it"
    );
}

#[test]
fn remove_binary_should_delete_an_unmanaged_binary_but_not_on_a_dry_run() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pixel");
    write(&path, "bin");
    let target = RemovalTarget {
        path: path.clone(),
        explicit: false,
    };

    let dry = remove_binary(&target, true).unwrap();
    assert_eq!(dry.summary, "[dry-run] would report: removed pixel binary");
    assert!(path.is_file());

    remove_binary(&target, false).unwrap();
    assert!(!path.exists());
}

// -- settings files --------------------------------------------------------------

#[test]
fn remove_pixel_hooks_from_settings_should_report_nothing_for_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    assert_eq!(
        remove_pixel_hooks_from_settings(&path, Path::new("/opt/pixel"), false).unwrap(),
        (0, None)
    );
    assert!(!path.exists());
}

#[test]
fn remove_gemini_hooks_should_remove_marked_groups_and_keep_foreign_ones() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(config::GEMINI_SETTINGS_FILE);
    let foreign = group("", "keep-me");
    install::write_settings(
        &path,
        &json!({
            "model": "x",
            "hooks": {
                "SessionStart": [group("", &format!("~/.claude/hooks/{}", config::SESSION_START_HOOK))],
                "BeforeTool": [group("run_shell", &format!("~/.claude/hooks/{}", config::GUARD_HOOK)), foreign.clone()],
            },
        }),
        false,
    )
    .unwrap();

    let step = remove_gemini_hooks(home.path(), Path::new("/opt/pixel"), false).unwrap();

    assert_eq!(step.id, "hooks.gemini");
    assert_eq!(
        step.summary, "removed 1 Gemini hook entry/entries",
        "the ownership pass takes both script entries in one rewrite"
    );
    assert_eq!(
        install::read_settings(&path).unwrap(),
        json!({"model": "x", "hooks": {"BeforeTool": [foreign]}}),
        "an event left empty goes, a mixed one keeps the foreign group"
    );
    assert!(
        step.detail.unwrap().contains(" (backup="),
        "the rewrite names its undo copy"
    );
}

#[test]
fn remove_pixel_hooks_from_settings_should_drop_the_hooks_key_once_every_event_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    install::write_settings(
        &path,
        &json!({"keep": 1, "hooks": {"Stop": [group("", "pixel run-hook guard")]}}),
        false,
    )
    .unwrap();
    let (removed, backup) =
        remove_pixel_hooks_from_settings(&path, Path::new("/opt/pixel"), false).unwrap();
    assert_eq!(removed, 1);
    assert!(backup.is_some());
    assert_eq!(install::read_settings(&path).unwrap(), json!({"keep": 1}));
}

#[test]
fn remove_pixel_hooks_from_settings_should_count_but_not_write_when_dry_run() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let original = json!({"hooks": {"Stop": [group("", "pixel run-hook guard")]}});
    install::write_settings(&path, &original, false).unwrap();
    let (removed, backup) =
        remove_pixel_hooks_from_settings(&path, Path::new("/opt/pixel"), true).unwrap();
    assert_eq!(removed, 1);
    assert_eq!(backup, None);
    assert_eq!(install::read_settings(&path).unwrap(), original);
}

#[test]
fn remove_pixel_hooks_from_settings_should_leave_a_file_without_hooks_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    write(&path, "{\"model\":\"x\"}");
    assert_eq!(
        remove_pixel_hooks_from_settings(&path, Path::new("/opt/pixel"), false).unwrap(),
        (0, None)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "{\"model\":\"x\"}");
}

// -- agent configs and search roots ------------------------------------------------

#[test]
fn strip_agent_configs_should_strip_managed_blocks_and_count_clean_files() {
    let home = tempfile::tempdir().unwrap();
    let zcode = home.path().join(".zcode/AGENTS.md");
    write(&zcode, &format!("z\n{}", managed("pixel")));
    let pi = home.path().join(config::PI_CONFIG_DIR).join("AGENTS.md");
    write(&pi, "already clean\n");

    let step = strip_agent_configs(home.path(), false).unwrap();

    assert_eq!(step.id, "agent-config");
    assert!(
        step.summary
            .starts_with("stripped managed block from 1 file(s) (")
            && step.summary.ends_with(" already clean)"),
        "{}",
        step.summary
    );
    assert_eq!(fs::read_to_string(&zcode).unwrap(), "z\n");
    assert_eq!(fs::read_to_string(&pi).unwrap(), "already clean\n");
    assert!(
        step.detail.unwrap().contains(&zcode.display().to_string()),
        "the step names the file whose undo copy it kept"
    );
}

#[test]
fn strip_agent_configs_should_count_without_writing_when_dry_run() {
    let home = tempfile::tempdir().unwrap();
    let zcode = home.path().join(".zcode/AGENTS.md");
    let text = format!("z\n{}", managed("pixel"));
    write(&zcode, &text);

    let step = strip_agent_configs(home.path(), true).unwrap();

    assert!(
        step.summary
            .starts_with("[dry-run] would report: stripped managed block from 1 file(s)"),
        "{}",
        step.summary
    );
    assert!(
        step.detail.is_none(),
        "no undo copy is written on a dry run"
    );
    assert_eq!(fs::read_to_string(&zcode).unwrap(), text);
}

#[test]
fn project_hook_search_roots_should_list_the_directories_under_documents_and_desktop_only() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join("Documents/proj-a")).unwrap();
    fs::create_dir_all(home.path().join("Desktop/proj-b")).unwrap();
    fs::create_dir_all(home.path().join("Downloads/proj-c")).unwrap();
    write(&home.path().join("Documents/notes.txt"), "not a project");

    let mut roots = project_hook_search_roots(home.path());
    roots.sort();

    assert_eq!(
        roots,
        vec![
            home.path().join("Desktop/proj-b"),
            home.path().join("Documents/proj-a"),
        ]
    );
}

#[test]
fn project_hook_search_roots_should_be_empty_when_neither_parent_exists() {
    let home = tempfile::tempdir().unwrap();
    assert!(project_hook_search_roots(home.path()).is_empty());
}

// -- whole runs --------------------------------------------------------------------

#[test]
fn uninstall_should_remove_only_the_shell_wrapper_when_wrappers_only() {
    let home = tempfile::tempdir().unwrap();
    let rule = home.path().join(config::PIXEL_RULES_REL);
    write(&rule, "# pixel rule\n");
    let binary = home.path().join(".local/bin/pixel");
    write(&binary, "bin");

    let report = uninstall(&UninstallOptions {
        home: Some(home.path().to_path_buf()),
        executable_path: Some(binary.clone()),
        shell: Some("/bin/zsh".into()),
        wrappers_only: true,
        ..UninstallOptions::default()
    })
    .unwrap();

    assert_eq!(report.steps.len(), 1, "one step: the wrapper");
    assert!(report.ok);
    assert_eq!(report.summary.red, 0);
    assert_eq!(
        report.summary.green + report.summary.yellow,
        1,
        "the summary counts that one step"
    );
    assert!(rule.is_file(), "every other artifact stays");
    assert!(binary.is_file());
}

#[test]
fn uninstall_should_report_every_step_and_touch_nothing_on_a_dry_run() {
    let home = tempfile::tempdir().unwrap();
    let rule = home.path().join(config::PIXEL_RULES_REL);
    write(&rule, "# pixel rule\n");
    let binary = home.path().join(".local/bin/pixel");
    write(&binary, "bin");

    let report = uninstall(&UninstallOptions {
        home: Some(home.path().to_path_buf()),
        executable_path: Some(binary.clone()),
        shell: Some("/bin/zsh".into()),
        dry_run: true,
        ..UninstallOptions::default()
    })
    .unwrap();

    assert!(report.dry_run);
    assert!(report.ok);
    let ids: Vec<&str> = report.steps.iter().map(|s| s.id.as_str()).collect();
    for id in [
        "agent-config",
        "hooks.zcode",
        "hooks.pi",
        "rule.source",
        "binary",
        "backups",
    ] {
        assert!(ids.contains(&id), "missing step {id}: {ids:?}");
    }
    assert_eq!(
        report.summary.green + report.summary.yellow + report.summary.red,
        report.steps.len(),
        "the summary counts every step once"
    );
    assert!(rule.is_file());
    assert!(binary.is_file(), "a dry run removes no binary");
    assert_eq!(report.executable_path, binary.display().to_string());
}
