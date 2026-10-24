// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel setup`: the real binary against a throwaway home holding a settings
//! file of every host, the way a person's machine does.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::support::{Fixture, project, stderr, stdout};

fn home(fixture: &Fixture) -> PathBuf {
    fixture.dir.path().join("home")
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A home with every host configured and a foreign hook in each file that
/// takes one, in formatting a rewrite would visibly change.
fn configured_home(fixture: &Fixture) -> Vec<(PathBuf, String)> {
    let home = home(fixture);
    let hooks = |event: &str| {
        format!(
            "{{ \"theme\" : \"dark\", \"hooks\": {{ \"{event}\": [{{\"hooks\": [{{\"type\": \"command\", \"command\": \"user-check\"}}]}}] }} }}\n"
        )
    };
    let files = vec![
        (home.join(".claude/settings.json"), hooks("Stop")),
        (home.join(".codex/hooks.json"), hooks("Stop")),
        (home.join(".config/devin/config.json"), hooks("Stop")),
        (home.join(".gemini/settings.json"), hooks("AfterAgent")),
        (
            home.join(".gemini/config/hooks.json"),
            "{ \"user-audit\" : {\"enabled\": true} }\n".to_owned(),
        ),
        (
            home.join(".pi/agent/settings.json"),
            "{ \"theme\" : \"dark\", \"packages\": [\"npm:other@1\"] }\n".to_owned(),
        ),
    ];
    for (path, text) in &files {
        write(path, text);
    }
    std::fs::create_dir_all(home.join(".config/opencode")).unwrap();
    files
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn json_of(path: &Path) -> Value {
    serde_json::from_str(&read(path)).unwrap()
}

fn commands(settings: &Value, event: &str) -> Vec<String> {
    settings["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .filter_map(|hook| hook["command"].as_str())
        .map(str::to_owned)
        .collect()
}

#[test]
fn print_reports_every_step_and_writes_nothing() {
    let fixture = project();
    let files = configured_home(&fixture);
    let output = fixture.run(&["setup", "--print"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    for step in [
        "skills.pixel",
        "hooks.claude",
        "hooks.codex",
        "hooks.gemini",
        "hooks.pi",
        "opencode-brief",
    ] {
        assert!(text.contains(step), "{step}\n{text}");
    }
    assert!(text.contains("(nothing written)"), "{text}");
    for (path, original) in &files {
        assert_eq!(&read(path), original, "{}", path.display());
    }
    assert!(!home(&fixture).join(".claude/skills").exists());
    assert!(!home(&fixture).join(".pixel").exists());
}

#[test]
fn setup_registers_every_host_keeps_foreign_hooks_and_is_idempotent() {
    let fixture = project();
    configured_home(&fixture);
    let home = home(&fixture);
    let output = fixture.run(&["setup"]);
    assert!(
        output.status.success(),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );

    let claude = json_of(&home.join(".claude/settings.json"));
    assert_eq!(claude["theme"], "dark");
    let stop = commands(&claude, "Stop");
    assert_eq!(stop[0], "user-check", "foreign hooks keep their place");
    assert!(
        stop[1].ends_with("hook stop --host claude || true"),
        "{stop:?}"
    );
    let prompt = commands(&claude, "UserPromptSubmit");
    assert_eq!(prompt.len(), 3, "the brief is split in three for Claude");
    assert!(
        prompt[0].contains("hook prompt-submit --host claude --part 1/3"),
        "{prompt:?}"
    );
    assert_eq!(
        commands(
            &json_of(&home.join(".codex/hooks.json")),
            "UserPromptSubmit"
        )
        .len(),
        3
    );
    let devin = json_of(&home.join(".config/devin/config.json"));
    assert_eq!(commands(&devin, "UserPromptSubmit").len(), 1);
    assert_eq!(
        devin["hooks"]["UserPromptSubmit"][0]["hooks"][0]["show_output"],
        true
    );
    let gemini = json_of(&home.join(".gemini/settings.json"));
    assert_eq!(
        gemini["hooks"]["BeforeAgent"][0]["hooks"][0]["name"],
        "pixel-prompt-submit"
    );
    assert_eq!(commands(&gemini, "AfterAgent")[0], "user-check");
    assert!(read(&home.join(".config/opencode/plugins/pixel-brief.js")).contains("pixel.brief"));
    let pi = json_of(&home.join(".pi/agent/settings.json"));
    assert_eq!(pi["packages"][0], "npm:other@1");
    assert!(
        pi["packages"][1]
            .as_str()
            .unwrap()
            .ends_with(".local/share/pixel/pi-package")
    );
    assert!(
        home.join(".local/share/pixel/pi-package/extensions/pixel.ts")
            .is_file()
    );
    for root in [
        ".claude",
        ".codex",
        ".gemini",
        ".config/opencode",
        ".pi/agent",
    ] {
        let skill = read(&home.join(root).join("skills/pixel/SKILL.md"));
        assert!(skill.starts_with("---\nname: pixel\n"), "{root}");
        assert!(skill.len() < 2_048 + 700, "{root}: {} bytes", skill.len());
    }

    // A second run changes nothing: not a byte of a file, not a new backup.
    let snapshot = |dir: &Path| {
        let mut entries: Vec<(PathBuf, String)> = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(path) = stack.pop() {
            for entry in std::fs::read_dir(&path).unwrap().flatten() {
                if entry.path().is_dir() {
                    stack.push(entry.path());
                } else {
                    entries.push((entry.path(), read(&entry.path())));
                }
            }
        }
        entries.sort();
        entries
    };
    let before = snapshot(&home);
    let again = fixture.run(&["setup"]);
    assert!(again.status.success(), "{}", stderr(&again));
    assert_eq!(snapshot(&home), before);
}

#[test]
fn remove_puts_every_file_back_byte_for_byte_and_leaves_no_trace() {
    let fixture = project();
    let files = configured_home(&fixture);
    let home = home(&fixture);
    let before_dirs = |dir: &Path| -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let top = before_dirs(&home);
    assert!(fixture.run(&["setup"]).status.success());
    assert_ne!(read(&files[0].0), files[0].1);

    let none = fixture.dir.path().join("no-such-binary");
    let removed = fixture.run(&["setup", "--remove", "--binary-path", none.to_str().unwrap()]);
    assert!(
        removed.status.success(),
        "{}{}",
        stdout(&removed),
        stderr(&removed)
    );

    for (path, original) in &files {
        assert_eq!(&read(path), original, "{}", path.display());
    }
    assert!(
        !home
            .join(".config/opencode/plugins/pixel-brief.js")
            .exists()
    );
    assert!(!home.join(".local/share/pixel").exists());
    assert!(!home.join(".claude/skills/pixel").exists());
    assert_eq!(
        before_dirs(&home),
        top,
        "nothing is left at the top of the home"
    );
}

#[test]
fn setup_should_preserve_legacy_pixel_hooks_and_remain_idempotent() {
    let fixture = project();
    configured_home(&fixture);
    let home = home(&fixture);
    let pixel_prompt = "'/usr/local/bin/pixel' run-hook task-event --provider claude --event prompt-submit --part 1/3 || true";
    write(
        &home.join(".claude/settings.json"),
        &json!({"hooks": {
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": pixel_prompt}]}],
            "Stop": [{"hooks": [{"type": "command", "command": "user-check"}]}],
        }})
        .to_string(),
    );
    let pixel_skill = home.join(".claude/skills/pixel/SKILL.md");
    write(&pixel_skill, "---\nname: pixel\n---\nmine, edited\n");
    let edited_skill = std::fs::read(&pixel_skill).unwrap();

    let output = fixture.run(&["setup", "--agent", "claude"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let claude = read(&home.join(".claude/settings.json"));
    assert!(
        claude.contains(pixel_prompt),
        "legacy Pixel hook is preserved: {claude}"
    );
    assert_eq!(claude.matches("user-check").count(), 1, "{claude}");
    assert_eq!(
        claude
            .matches("hook prompt-submit --host claude --part 1/3")
            .count(),
        1,
        "the current Pixel hook is installed once: {claude}"
    );
    assert!(pixel_skill.is_file(), "the installed skill remains present");
    assert!(
        !stdout(&output).contains("migration.skill"),
        "legacy skill migration warning is absent: {}",
        stdout(&output)
    );
    let backups = || {
        std::fs::read_dir(pixel_skill.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".pixel-bak."))
            .map(|entry| entry.path())
            .collect::<Vec<_>>()
    };
    let first_backups = backups();
    assert_eq!(first_backups.len(), 1, "one edited skill backup");
    assert_eq!(std::fs::read(&first_backups[0]).unwrap(), edited_skill);

    let first: Value = serde_json::from_str(&claude).unwrap();
    let repeated = fixture.run(&["setup", "--agent", "claude"]);
    assert!(repeated.status.success(), "{}", stderr(&repeated));
    let second: Value = serde_json::from_str(&read(&home.join(".claude/settings.json"))).unwrap();
    assert_eq!(
        second, first,
        "reinstall does not rewrite or duplicate hooks"
    );
    assert_eq!(
        backups(),
        first_backups,
        "reinstall does not replace the backup"
    );
    assert_eq!(std::fs::read(&first_backups[0]).unwrap(), edited_skill);
}

#[test]
fn agent_limits_the_run_to_the_agents_named() {
    let fixture = project();
    configured_home(&fixture);
    let home = home(&fixture);
    let codex_before = read(&home.join(".codex/hooks.json"));
    let output = fixture.run(&["setup", "--agent", "claude"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(read(&home.join(".claude/settings.json")).contains("hook stop --host claude"));
    assert_eq!(read(&home.join(".codex/hooks.json")), codex_before);
    assert!(!home.join(".codex/skills").exists());
    assert!(
        !home
            .join(".config/opencode/plugins/pixel-brief.js")
            .exists()
    );

    let bad = fixture.run(&["setup", "--agent", "emacs"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(stderr(&bad).contains("unknown agent"), "{}", stderr(&bad));
}

#[test]
fn json_prints_the_report() {
    let fixture = project();
    configured_home(&fixture);
    let output = fixture.run(&["setup", "--print", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["ok"], true);
    assert!(report["steps"].as_array().unwrap().len() >= 6);
}
