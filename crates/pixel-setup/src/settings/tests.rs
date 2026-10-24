// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use super::*;

fn no_env(_: &str) -> Option<String> {
    None
}

fn write_file(dir: &Path, text: &str) {
    fs::create_dir_all(dir.join(".pixel")).unwrap();
    fs::write(config_path(dir), text).unwrap();
}

fn value_of(settings: &Settings, setting: Setting) -> (Value, Source) {
    let entry = settings.entry(setting);
    (entry.value, entry.source.clone())
}

#[test]
fn nothing_set_should_give_every_default() {
    let home = tempfile::tempdir().unwrap();
    let settings = Settings::load(home.path(), None, &no_env);
    assert_eq!(settings.entries.len(), Setting::ALL.len());
    assert!(settings.problems.is_empty());
    for setting in Setting::ALL {
        assert_eq!(
            value_of(&settings, setting),
            (setting.default_value(), Source::Default)
        );
    }
    assert!(settings.on(Setting::Metrics));
    assert!(settings.on(Setting::DaemonAutoStart));
    assert!(settings.on(Setting::Brief));
    assert!(!settings.on(Setting::EditHooks));
    assert_eq!(settings.count(Setting::BriefParts), BRIEF_PARTS_DEFAULT);
}

#[test]
fn a_value_should_come_from_the_environment_then_the_repo_then_the_global_file() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_file(
        home.path(),
        "metrics: false\nbrief: false\nedit_hooks: true\n",
    );
    write_file(repo.path(), "metrics: true\nedit_hooks: false\n");
    let env = |name: &str| (name == "PIXEL_BRIEF").then(|| "1".to_owned());
    let settings = Settings::load(home.path(), Some(repo.path()), &env);
    assert_eq!(
        value_of(&settings, Setting::Brief),
        (Value::Switch(true), Source::Env("PIXEL_BRIEF"))
    );
    assert_eq!(
        value_of(&settings, Setting::Metrics),
        (Value::Switch(true), Source::Repo(config_path(repo.path())))
    );
    assert_eq!(
        value_of(&settings, Setting::EditHooks),
        (Value::Switch(false), Source::Repo(config_path(repo.path())))
    );
    write_file(repo.path(), "");
    let settings = Settings::load(home.path(), Some(repo.path()), &no_env);
    assert_eq!(
        value_of(&settings, Setting::Metrics),
        (
            Value::Switch(false),
            Source::Global(config_path(home.path()))
        )
    );
}

#[test]
fn an_environment_value_nobody_can_read_should_fall_through_to_the_files() {
    let home = tempfile::tempdir().unwrap();
    write_file(home.path(), "metrics: false\n");
    let env = |_: &str| Some("maybe".to_owned());
    let settings = Settings::load(home.path(), None, &env);
    assert_eq!(
        value_of(&settings, Setting::Metrics),
        (
            Value::Switch(false),
            Source::Global(config_path(home.path()))
        )
    );
}

#[test]
fn the_switch_spellings_should_all_parse() {
    for on in ["on", "true", "1", "yes", " ON "] {
        assert_eq!(
            Setting::Metrics.parse(on),
            Some(Value::Switch(true)),
            "{on}"
        );
    }
    for off in ["off", "false", "0", "no", "Off"] {
        assert_eq!(
            Setting::Metrics.parse(off),
            Some(Value::Switch(false)),
            "{off}"
        );
    }
    assert_eq!(Setting::Metrics.parse("2"), None);
    assert_eq!(Setting::Metrics.parse(""), None);
}

#[test]
fn a_count_should_parse_only_inside_its_range() {
    assert_eq!(Setting::BriefParts.parse("1"), Some(Value::Count(1)));
    assert_eq!(Setting::BriefParts.parse("8"), Some(Value::Count(8)));
    assert_eq!(Setting::BriefParts.parse("0"), None);
    assert_eq!(Setting::BriefParts.parse("9"), None);
    assert_eq!(Setting::BriefParts.parse("on"), None);
}

#[test]
fn a_file_count_out_of_range_should_be_clamped_and_a_text_one_ignored() {
    let home = tempfile::tempdir().unwrap();
    write_file(home.path(), "brief_parts: 99\n");
    assert_eq!(
        Settings::load(home.path(), None, &no_env).count(Setting::BriefParts),
        8
    );
    write_file(home.path(), "brief_parts: 0\n");
    assert_eq!(
        Settings::load(home.path(), None, &no_env).count(Setting::BriefParts),
        1
    );
    write_file(home.path(), "brief_parts: lots\n");
    assert_eq!(
        Settings::load(home.path(), None, &no_env).count(Setting::BriefParts),
        BRIEF_PARTS_DEFAULT
    );
}

#[test]
fn a_file_switch_should_read_a_bool_a_word_and_a_digit() {
    let home = tempfile::tempdir().unwrap();
    write_file(
        home.path(),
        "metrics: off\nbrief: 0\ndaemon_auto_start: false\nedit_hooks: 7\n",
    );
    let settings = Settings::load(home.path(), None, &no_env);
    assert!(!settings.on(Setting::Metrics));
    assert!(!settings.on(Setting::Brief));
    assert!(!settings.on(Setting::DaemonAutoStart));
    assert_eq!(
        value_of(&settings, Setting::EditHooks),
        (Value::Switch(false), Source::Default)
    );
}

#[test]
fn a_file_that_is_not_a_mapping_should_be_reported_and_skipped() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_file(home.path(), "metrics: false\n");
    write_file(repo.path(), "- not\n- a mapping\n");
    let settings = Settings::load(home.path(), Some(repo.path()), &no_env);
    assert_eq!(settings.problems.len(), 1);
    assert!(
        settings.problems[0].contains("must be a mapping")
            || settings.problems[0].contains("invalid configuration")
    );
    assert_eq!(
        value_of(&settings, Setting::Metrics),
        (
            Value::Switch(false),
            Source::Global(config_path(home.path()))
        )
    );
}

#[test]
fn keys_should_round_trip_and_unknown_ones_name_nothing() {
    for setting in Setting::ALL {
        assert_eq!(Setting::from_key(setting.key()), Some(setting));
    }
    assert_eq!(Setting::from_key("classify"), None);
    assert_eq!(Setting::from_key("metrics "), None);
}

#[test]
fn write_should_create_the_file_with_a_header_and_owner_only_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        write(dir.path(), Setting::Metrics, "off"),
        Ok(Value::Switch(false))
    );
    let text = fs::read_to_string(config_path(dir.path())).unwrap();
    assert_eq!(text, format!("{TEMPLATE}\nmetrics: false\n"));
    let mode = fs::metadata(config_path(dir.path()))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn write_should_replace_a_line_in_place_and_keep_the_comments() {
    let dir = tempfile::tempdir().unwrap();
    write_file(
        dir.path(),
        "# mine\nmetrics: true\n# between\nbrief_parts: 3\n",
    );
    write(dir.path(), Setting::Metrics, "off").unwrap();
    assert_eq!(
        fs::read_to_string(config_path(dir.path())).unwrap(),
        "# mine\nmetrics: false\n# between\nbrief_parts: 3\n"
    );
    write(dir.path(), Setting::BriefParts, "5").unwrap();
    assert!(
        fs::read_to_string(config_path(dir.path()))
            .unwrap()
            .contains("\nbrief_parts: 5\n")
    );
}

#[test]
fn write_should_append_to_a_file_without_a_final_newline_and_ignore_commented_keys() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "# metrics: false\nbrief: true");
    write(dir.path(), Setting::Metrics, "off").unwrap();
    assert_eq!(
        fs::read_to_string(config_path(dir.path())).unwrap(),
        "# metrics: false\nbrief: true\nmetrics: false\n"
    );
}

#[test]
fn write_should_refuse_a_bad_value_and_a_file_it_cannot_parse_and_touch_neither() {
    let dir = tempfile::tempdir().unwrap();
    let error = write(dir.path(), Setting::Metrics, "sometimes").unwrap_err();
    assert!(error.contains("on or off"), "{error}");
    let error = write(dir.path(), Setting::BriefParts, "12").unwrap_err();
    assert!(error.contains("1 to 8"), "{error}");
    assert!(!config_path(dir.path()).exists());
    write_file(dir.path(), "- a list\n");
    assert!(write(dir.path(), Setting::Metrics, "off").is_err());
    assert_eq!(
        fs::read_to_string(config_path(dir.path())).unwrap(),
        "- a list\n"
    );
}

#[test]
fn write_should_not_follow_a_link_planted_at_the_file() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("target.yaml");
    fs::write(&target, "kept: true\n").unwrap();
    fs::create_dir_all(dir.path().join(".pixel")).unwrap();
    symlink(&target, config_path(dir.path())).unwrap();
    let _ = write(dir.path(), Setting::Metrics, "off");
    assert_eq!(fs::read_to_string(&target).unwrap(), "kept: true\n");
}

#[test]
fn remove_should_drop_the_line_and_say_whether_it_was_there() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "# keep\nmetrics: false\nbrief: true\n");
    assert_eq!(remove(dir.path(), Setting::Metrics), Ok(true));
    assert_eq!(
        fs::read_to_string(config_path(dir.path())).unwrap(),
        "# keep\nbrief: true\n"
    );
    assert_eq!(remove(dir.path(), Setting::Metrics), Ok(false));
    assert_eq!(
        remove(tempfile::tempdir().unwrap().path(), Setting::Brief),
        Ok(false)
    );
}

#[test]
fn remove_should_leave_a_file_without_the_key_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "brief: true");
    assert_eq!(remove(dir.path(), Setting::Metrics), Ok(false));
    assert_eq!(
        fs::read_to_string(config_path(dir.path())).unwrap(),
        "brief: true"
    );
}

#[test]
fn a_key_prefix_should_not_match_a_longer_key() {
    assert!(is_entry("brief: on", "brief"));
    assert!(is_entry("brief :on", "brief"));
    assert!(!is_entry("brief_parts: 3", "brief"));
    assert!(!is_entry("  brief: on", "brief"));
    assert!(!is_entry("# brief: on", "brief"));
}

#[test]
fn values_should_display_as_a_person_writes_them() {
    assert_eq!(Value::Switch(true).to_string(), "on");
    assert_eq!(Value::Switch(false).to_string(), "off");
    assert_eq!(Value::Count(3).to_string(), "3");
    assert_eq!(Source::Default.to_string(), "default");
    assert_eq!(Source::Env("PIXEL_X").to_string(), "environment PIXEL_X");
    assert_eq!(
        Source::Global(PathBuf::from("/h/.pixel/config.yaml")).to_string(),
        "/h/.pixel/config.yaml"
    );
}
