// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel config` on the real binary, and the commands that obey it.

use serde_json::{Value, json};

use crate::support::{Fixture, project, stderr, stdout};

fn setting(fixture: &Fixture, key: &str) -> Value {
    let output = fixture.run(&["config", key, "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    serde_json::from_str(&stdout(&output)).unwrap()
}

fn repo_config(fixture: &Fixture) -> std::path::PathBuf {
    fixture.root().join(".pixel/config.yaml")
}

fn global_config(fixture: &Fixture) -> std::path::PathBuf {
    fixture.dir.path().join("home/.pixel/config.yaml")
}

#[test]
fn the_list_should_name_every_setting_with_its_default_and_source() {
    let fixture = project();
    let output = fixture.run(&["config", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).unwrap();
    let rows: Vec<(String, String, String)> = report["settings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["key"].as_str().unwrap().to_owned(),
                row["value"].as_str().unwrap().to_owned(),
                row["source"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let default = |key: &str, value: &str| (key.to_owned(), value.to_owned(), "default".to_owned());
    assert_eq!(
        rows,
        vec![
            default("metrics", "on"),
            // The fixture forbids starting a daemon through the environment.
            (
                "daemon_auto_start".to_owned(),
                "off".to_owned(),
                "environment PIXEL_DAEMON_AUTO_START".to_owned()
            ),
            default("brief", "on"),
            default("brief_parts", "3"),
            default("edit_hooks", "off"),
        ]
    );
    assert_eq!(report["problems"], json!([]));
    let text = stdout(&fixture.run(&["config"]));
    assert!(text.contains("brief_parts"), "{text}");
    assert!(text.contains("hooks that carry the brief"), "{text}");
}

#[test]
fn a_set_should_write_the_repository_file_and_a_later_read_should_name_it() {
    let fixture = project();
    let output = fixture.run(&["config", "metrics", "off"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("metrics off ("),
        "{}",
        stdout(&output)
    );
    let text = std::fs::read_to_string(repo_config(&fixture)).unwrap();
    assert!(text.ends_with("metrics: false\n"), "{text}");

    let read = setting(&fixture, "metrics");
    assert_eq!(read["value"], "off");
    assert!(
        read["source"]
            .as_str()
            .unwrap()
            .ends_with("repo/.pixel/config.yaml"),
        "{read}"
    );
    assert_eq!(stdout(&fixture.run(&["config", "metrics"])), "off\n");
}

#[test]
fn a_global_set_should_not_touch_the_repository_and_the_repository_should_win() {
    let fixture = project();
    assert!(
        fixture
            .run(&["config", "brief_parts", "5", "--global"])
            .status
            .success()
    );
    assert!(!repo_config(&fixture).exists());
    assert!(global_config(&fixture).is_file());
    assert_eq!(setting(&fixture, "brief_parts")["value"], "5");

    let set = fixture.run(&["config", "brief_parts", "2"]);
    assert!(set.status.success(), "{}", stderr(&set));
    assert!(
        stderr(&set).contains("run `pixel setup`"),
        "{}",
        stderr(&set)
    );
    assert_eq!(setting(&fixture, "brief_parts")["value"], "2");
    let global = fixture.run(&["config", "brief_parts", "--global", "--json"]);
    assert_eq!(
        serde_json::from_str::<Value>(&stdout(&global)).unwrap()["value"],
        "5"
    );
}

#[test]
fn a_set_that_the_environment_overrides_should_say_so() {
    let fixture = project();
    let output = fixture
        .pixel(&["config", "metrics", "off"])
        .env("PIXEL_METRICS", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output)
            .contains("metrics is still on here: environment PIXEL_METRICS takes precedence"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_unset_should_remove_the_line_and_say_when_there_was_none() {
    let fixture = project();
    fixture.write(
        ".pixel/config.yaml",
        "# mine\nmetrics: false\nbrief: true\n",
    );
    let output = fixture.run(&["config", "metrics", "--unset"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("metrics unset ("),
        "{}",
        stdout(&output)
    );
    assert_eq!(
        std::fs::read_to_string(repo_config(&fixture)).unwrap(),
        "# mine\nbrief: true\n"
    );
    let again = fixture.run(&["config", "metrics", "--unset"]);
    assert!(
        stdout(&again).starts_with("metrics was not set in"),
        "{}",
        stdout(&again)
    );
}

#[test]
fn a_bad_key_or_value_should_fail_without_writing() {
    let fixture = project();
    let key = fixture.run(&["config", "classifx", "on"]);
    assert_eq!(key.status.code(), Some(1));
    assert!(
        stderr(&key).contains("unknown setting \"classifx\""),
        "{}",
        stderr(&key)
    );
    let value = fixture.run(&["config", "brief_parts", "12"]);
    assert_eq!(value.status.code(), Some(1));
    assert!(stderr(&value).contains("1 to 8"), "{}", stderr(&value));
    let switch = fixture.run(&["config", "metrics", "maybe"]);
    assert_eq!(switch.status.code(), Some(1));
    assert!(!repo_config(&fixture).exists());
}

#[test]
fn a_settings_file_that_is_not_a_mapping_should_be_reported_and_left_alone() {
    let fixture = project();
    fixture.write(".pixel/config.yaml", "- not\n- a mapping\n");
    let list = fixture.run(&["config"]);
    assert!(list.status.success(), "{}", stderr(&list));
    assert!(stderr(&list).contains("config.yaml"), "{}", stderr(&list));
    let set = fixture.run(&["config", "metrics", "off"]);
    assert_eq!(set.status.code(), Some(1));
    assert_eq!(
        std::fs::read_to_string(repo_config(&fixture)).unwrap(),
        "- not\n- a mapping\n"
    );
}

#[test]
fn metrics_off_in_the_settings_should_silence_the_live_line_and_keep_the_accounting() {
    let fixture = project();
    let before = fixture.run(&["status"]);
    assert!(
        stderr(&before).contains("🟩 pixel status"),
        "{}",
        stderr(&before)
    );
    assert!(fixture.run(&["config", "metrics", "off"]).status.success());
    let after = fixture.run(&["status"]);
    assert!(!stderr(&after).contains("🟩"), "{}", stderr(&after));
    let lines = crate::support::action_log(&fixture.root());
    assert!(
        lines
            .iter()
            .filter(|line| line["command"] == "status")
            .count()
            >= 2,
        "{lines:?}"
    );
}

#[test]
fn brief_off_in_the_settings_should_decline_the_brief() {
    let fixture = project();
    assert!(fixture.run(&["config", "brief", "off"]).status.success());
    let output = fixture.run(&["brief", "where is run defined", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["declined"], "disabled");
    assert!(
        stderr(&output).contains("no brief: switched off"),
        "{}",
        stderr(&output)
    );
}
