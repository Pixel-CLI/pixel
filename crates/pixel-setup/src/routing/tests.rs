// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use super::*;

fn release() -> &'static Path {
    Path::new("/usr/local/bin/pixel")
}

fn merged(host: Host, exe: &Path, parts: u8) -> Value {
    let mut value = json!({"hooks": {}});
    merge_hooks(value["hooks"].as_object_mut().unwrap(), host, exe, parts).unwrap();
    value
}

fn commands_of(value: &Value, event: &str) -> Vec<String> {
    value["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|group| group["hooks"][0]["command"].as_str())
        .map(str::to_owned)
        .collect()
}

#[test]
fn claude_gets_the_four_events_with_the_brief_split_in_order() {
    let value = merged(Host::Claude, release(), 3);
    let exe = "'/usr/local/bin/pixel'";
    assert_eq!(
        commands_of(&value, "UserPromptSubmit"),
        [
            format!("{exe} hook prompt-submit --host claude --part 1/3 || true"),
            format!("{exe} hook prompt-submit --host claude --part 2/3 || true"),
            format!("{exe} hook prompt-submit --host claude --part 3/3 || true"),
        ]
    );
    assert_eq!(
        commands_of(&value, "SessionStart"),
        [format!("{exe} hook session-start --host claude || true")]
    );
    assert_eq!(
        commands_of(&value, "PostToolUse"),
        [format!("{exe} hook post-tool --host claude || true")]
    );
    assert_eq!(
        commands_of(&value, "Stop"),
        [format!("{exe} hook stop --host claude || true")]
    );
    assert_eq!(value["hooks"].as_object().unwrap().len(), 4);
}

#[test]
fn only_a_host_with_a_per_hook_cap_splits_the_brief() {
    for host in [Host::Claude, Host::Codex] {
        assert_eq!(
            commands_of(&merged(host, release(), 2), "UserPromptSubmit").len(),
            2,
            "{host:?}"
        );
    }
    let devin = merged(Host::Devin, release(), 3);
    assert_eq!(commands_of(&devin, "UserPromptSubmit").len(), 1);
    assert_eq!(
        devin["hooks"]["UserPromptSubmit"][0]["hooks"][0]["show_output"],
        true
    );
    assert!(
        devin["hooks"]["Stop"][0]["hooks"][0]
            .get("show_output")
            .is_none()
    );
    // A part count outside 1..=8 reads as the nearest bound.
    assert_eq!(
        commands_of(&merged(Host::Claude, release(), 0), "UserPromptSubmit").len(),
        1
    );
    assert_eq!(
        commands_of(&merged(Host::Claude, release(), 200), "UserPromptSubmit").len(),
        8
    );
}

#[test]
fn the_brief_gets_the_long_timeout_and_the_quick_events_the_short_one() {
    let value = merged(Host::Claude, release(), 1);
    assert_eq!(
        value["hooks"]["UserPromptSubmit"][0]["hooks"][0]["timeout"],
        config::HOOK_TIMEOUT
    );
    for event in ["SessionStart", "PostToolUse", "Stop"] {
        assert_eq!(
            value["hooks"][event][0]["hooks"][0]["timeout"],
            config::QUICK_HOOK_TIMEOUT,
            "{event}"
        );
    }
}

#[test]
fn merging_again_replaces_only_pixels_entries_and_keeps_foreign_ones_where_they_were() {
    let user = json!({"hooks":[{"type":"command","command":"user-check"}]});
    let mut value = json!({"hooks": {"Stop": [user.clone()]}});
    for _ in 0..3 {
        configure_hooks(&mut value, Host::Claude, release(), 3).unwrap();
    }
    let stop = value["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 2, "{stop:?}");
    assert_eq!(stop[0], user);
    assert_eq!(
        commands_of(&value, "UserPromptSubmit").len(),
        3,
        "no stacking"
    );
}

#[test]
fn codex_keeps_every_group_index_when_setup_runs_again() {
    let user = json!({"hooks":[{"type":"command","command":"user-check"}]});
    for pixel_first in [true, false] {
        let mut value = json!({"hooks":{}});
        merge_hooks(
            value["hooks"].as_object_mut().unwrap(),
            Host::Codex,
            release(),
            1,
        )
        .unwrap();
        let pixel = value["hooks"]["Stop"][0].clone();
        value["hooks"]["Stop"] = if pixel_first {
            json!([pixel, user])
        } else {
            json!([user, pixel])
        };
        let before = value.clone();
        let hooks = value["hooks"].as_object_mut().unwrap();
        merge_hooks(hooks, Host::Codex, release(), 1).unwrap();
        assert_eq!(
            value["hooks"]["Stop"], before["hooks"]["Stop"],
            "pixel_first={pixel_first}"
        );
    }
}

#[test]
fn ownership_rejects_other_executables_and_command_mentions() {
    for command in [
        "'/usr/local/bin/pixel' hook stop --host claude",
        "'/usr/local/bin/pixel' hook stop --host claude || true",
        "'/opt/pixel-dev' hook prompt-submit --host codex --part 2/3 || true",
        "pixel hook post-tool --host devin",
    ] {
        assert!(is_pixel_hook(command, release()), "{command}");
    }
    for command in [
        "echo pixel hook stop --host claude",
        "'/usr/local/bin/pixel-helper' hook stop --host claude",
        "'/usr/local/bin/pixel' hook stop",
        "'/usr/local/bin/pixel' hook nonsense --host claude",
        "'/usr/local/bin/pixel' hook stop --host cursor",
        "'/usr/local/bin/pixel' hook stop --host claude --bogus",
        "'/usr/local/bin/pixel' hook post-tool --host claude --part 1/2",
        "'/usr/local/bin/pixel' hook prompt-submit --host claude --part 1/1",
        "'/usr/local/bin/pixel' hook prompt-submit --host claude --part 3/2",
        "'/usr/local/bin/pixel' hook prompt-submit --host claude --part 1/9",
        "'/usr/local/bin/other' hook stop --host claude",
        "git hook stop --host claude",
    ] {
        assert!(!is_pixel_hook(command, release()), "{command}");
    }
}

#[test]
fn the_installing_executable_owns_its_entries_under_any_name_but_no_other_does() {
    let custom = Path::new("/tmp/build/pixel-nightly");
    let command = format!("{} hook stop --host claude", quoted_executable(custom));
    assert!(is_pixel_hook(&command, custom));
    assert!(!is_pixel_hook(&command, release()));
    let spaced = Path::new("/tmp/it's here/pixel");
    let command = format!(
        "{} hook stop --host claude{FAIL_OPEN}",
        quoted_executable(spaced)
    );
    assert!(is_pixel_hook(&command, spaced));
}

#[test]
fn a_quoted_executable_reads_back_as_the_path_it_was_written_from() {
    for path in ["/usr/bin/pixel", "/tmp/it's here/pixel", "/tmp/a b/pixel"] {
        let path = Path::new(path);
        assert_eq!(
            unquoted_executable(&quoted_executable(path)),
            Some(path.to_path_buf())
        );
    }
    assert_eq!(unquoted_executable("pixel"), Some(PathBuf::from("pixel")));
    assert_eq!(unquoted_executable("sudo pixel"), None);
}

#[test]
fn installing_preserves_foreign_hooks_and_does_not_stack_its_own() {
    let mut value = json!({"hooks": {
        "UserPromptSubmit": [
            {"hooks":[{"type":"command","command":"'/usr/local/bin/pixel' run-hook task-event --provider claude --event prompt-submit --part 1/3 || true"}]},
            {"hooks":[{"type":"command","command":"foreign-prompt"}]},
        ],
        "PostToolUse": [
            {"matcher":"Bash","hooks":[{"type":"command","command":"'/usr/local/bin/pixel' run-hook metrics --provider claude"}]},
        ],
    }});
    configure_hooks(&mut value, Host::Claude, release(), 1).unwrap();
    let prompt = commands_of(&value, "UserPromptSubmit");
    assert_eq!(
        prompt[0],
        "'/usr/local/bin/pixel' run-hook task-event --provider claude --event prompt-submit --part 1/3 || true"
    );
    assert_eq!(prompt[1], "foreign-prompt");
    assert_eq!(prompt.len(), 3);
    assert!(hooks_registered(&value, Host::Claude, release()));
}

#[test]
fn the_registered_events_are_exactly_the_ones_the_host_table_names() {
    for host in [Host::Claude, Host::Codex, Host::Devin] {
        let value = merged(host, release(), 1);
        let mut names: Vec<&str> = value["hooks"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["PostToolUse", "SessionStart", "Stop", "UserPromptSubmit"],
            "{host:?}"
        );
    }
}

#[test]
fn prompt_parts_names_what_is_wrong_with_an_incomplete_set() {
    let prompt = |commands: &[&str]| {
        json!({"hooks": {"UserPromptSubmit": commands.iter().map(|command| {
            json!({"hooks":[{"type":"command","command":command}]})
        }).collect::<Vec<_>>()}})
    };
    let exe = "'/usr/local/bin/pixel' hook prompt-submit --host claude";
    let check = |commands: &[&str]| prompt_parts(&prompt(commands), Host::Claude, release());
    assert_eq!(check(&[exe]), Ok(1));
    assert_eq!(
        check(&[&format!("{exe} --part 1/2"), &format!("{exe} --part 2/2")]),
        Ok(2)
    );
    assert_eq!(check(&[]), Err(NO_PROMPT_HOOK.to_owned()));
    assert_eq!(
        check(&[&format!("{exe} --part 1/3"), &format!("{exe} --part 3/3")]),
        Err("brief part 2/3 missing".to_owned())
    );
    assert_eq!(
        check(&[exe, &format!("{exe} --part 1/2")]),
        Err("the whole brief and brief parts are both registered".to_owned())
    );
    assert_eq!(
        check(&[&format!("{exe} --part 1/2"), &format!("{exe} --part 2/3")]),
        Err("brief parts of different counts are registered".to_owned())
    );
    // Another host's entry is not this host's brief.
    assert_eq!(
        prompt_parts(&prompt(&[exe]), Host::Codex, release()),
        Err(NO_PROMPT_HOOK.to_owned())
    );
}

#[test]
fn prompt_parts_ignores_asynchronous_and_restricted_entries() {
    let command = "'/usr/local/bin/pixel' hook prompt-submit --host claude";
    let restricted = json!({"hooks": {"UserPromptSubmit": [
        {"matcher": "Write", "hooks": [{"type": "command", "command": command}]},
        {"hooks": [{"type": "command", "command": command, "async": true}]},
    ]}});
    assert_eq!(
        prompt_parts(&restricted, Host::Claude, release()),
        Err(NO_PROMPT_HOOK.to_owned())
    );
}

#[test]
fn stacked_entries_are_named_once_per_event_and_verb() {
    let exe = release();
    let mut value = merged(Host::Claude, exe, 1);
    assert!(stacked_pixel_hooks(&value, exe).is_empty());
    let again = value["hooks"]["Stop"][0].clone();
    value["hooks"]["Stop"].as_array_mut().unwrap().push(again);
    assert_eq!(
        stacked_pixel_hooks(&value, exe),
        ["Stop→stop --host claude ×2".to_owned()]
    );
}

#[test]
fn hooks_running_another_binary_are_named_once_in_order() {
    let mut value = merged(Host::Claude, Path::new("/opt/pixel-dev"), 1);
    let others = pixel_hooks_running_other_binaries(&value, release());
    assert_eq!(others, [PathBuf::from("/opt/pixel-dev")]);
    value = merged(Host::Claude, release(), 1);
    assert!(pixel_hooks_running_other_binaries(&value, release()).is_empty());
}
