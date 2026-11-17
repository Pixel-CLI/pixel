// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! User configuration contracts through the real CLI, with isolated home directories.
use crate::support::{Scratch, pixel_command};
use std::fs;
use std::path::Path;
use std::process::Output;

fn run(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    pixel_command()
        .env("HOME", home)
        .env_remove("PIXEL_METRICS")
        .env_remove("PIXEL_DAEMON_AUTO_START")
        .env_remove("OPENAI_API_KEY")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn overview_should_show_effective_layers_without_creating_files_or_printing_secrets() {
    let home = Scratch::for_test("config", "overview-home");
    let repo = Scratch::for_test("config", "overview-repo");
    let empty = stdout(&run(&home, &repo, &["config"]));
    assert!(empty.contains("config.yaml"));
    assert!(empty.contains("metrics: on"));
    assert!(empty.contains("daemon_auto_start: true (default)"));
    assert!(!home.join(".pixel/config.yaml").exists());
    fs::create_dir_all(home.join(".pixel")).unwrap();
    fs::create_dir_all(repo.join(".pixel")).unwrap();
    fs::write(home.join(".pixel/config.yaml"), "metrics: 'off'\ndaemon_auto_start: false\nclassify: {engine: remote, remote_preset: deepseek}\nremote_keys: {deepseek: sk-secret}\nunknown: hidden-secret\n").unwrap();
    fs::write(
        repo.join(".pixel/config.yaml"),
        "metrics: 'on'\ndaemon_auto_start: true\n",
    )
    .unwrap();
    let out = run(&home, &repo, &["config"]);
    let text = stdout(&out);
    assert!(text.contains("metrics: on (Repo)"), "{text}");
    assert!(text.contains("daemon_auto_start: true"), "{text}");
    assert!(text.contains("classify.engine: remote"));
    assert!(text.contains("classify.remote_preset: deepseek"));
    assert!(text.contains("remote_keys.deepseek: set"));
    assert!(!text.contains("secret"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("secret"));
    for (env, expected) in [
        ("0", "metrics: off (PIXEL_METRICS)"),
        ("1", "metrics: on (Repo)"),
    ] {
        let out = pixel_command()
            .env("HOME", &*home)
            .env("PIXEL_METRICS", env)
            .current_dir(&*repo)
            .arg("config")
            .output()
            .unwrap();
        assert!(stdout(&out).contains(expected));
    }
}

#[test]
fn classify_off_should_block_every_engine_and_batch_before_reading_input() {
    let home = Scratch::for_test("config", "classify-off-home");
    fs::create_dir_all(home.join(".pixel")).unwrap();
    fs::write(
        home.join(".pixel/config.yaml"),
        "classify: {engine: remote}\n",
    )
    .unwrap();
    stdout(&run(&home, &home, &["config", "classify", "off"]));
    assert!(stdout(&run(&home, &home, &["config"])).contains("classify.enabled: false"));
    for args in [
        vec!["classify", "hello"],
        vec![
            "classify", "hello", "--engine", "remote", "--label", "a", "--label", "b",
        ],
        vec!["classify", "hello", "--engine", "ollaya"],
        vec!["classify", "--jsonl"],
    ] {
        let out = run(&home, &home, &args);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("classify is disabled"));
        assert!(out.stdout.is_empty());
    }
    stdout(&run(&home, &home, &["config", "classify", "on"]));
    assert!(stdout(&run(&home, &home, &["config"])).contains("classify.enabled: true"));
    let doc: serde_json::Value =
        serde_saphyr::from_str(&fs::read_to_string(home.join(".pixel/config.yaml")).unwrap())
            .unwrap();
    assert_eq!(doc["classify"]["engine"], "remote");
    fs::write(
        home.join(".pixel/config.yaml"),
        "classify: {enabled: 'false'}\n",
    )
    .unwrap();
    for args in [&["config"][..], &["classify", "hello"][..]] {
        let out = run(&home, &home, args);
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("classify.enabled must be true or false")
        );
    }
}

/// The 0.6.1 report: setup saved `metrics: "on"` globally while a repo
/// `config.yaml` carried `metrics: "off"`, so every launch in that repository
/// hid the footer. The global write must name the layer that wins instead of
/// letting the answer look ignored.
#[test]
fn metrics_on_global_should_report_a_contradicting_repo_layer() {
    let home = Scratch::for_test("config", "metrics-home");
    let repo = Scratch::for_test("config", "metrics-repo");
    crate::support::git(&repo, &["init", "-q"]);
    let repo_file = repo.join(".pixel/config.yaml");
    fs::create_dir_all(repo_file.parent().unwrap()).unwrap();
    fs::write(&repo_file, "{\"metrics\":\"off\"}").unwrap();

    let text = stdout(&run(&home, &repo, &["config", "metrics", "on", "--global"]));
    assert!(
        text.contains(&format!(
            "metrics: on — wrote {}",
            home.join(".pixel/config.yaml").display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "note: {} sets metrics: off and wins in this repository — \
             run `pixel config metrics on` here to apply this answer",
            repo_file.display()
        )),
        "{text}"
    );
    // The note only reports: the repo layer is untouched, and the report
    // command names it as the winner.
    assert_eq!(
        fs::read_to_string(&repo_file).unwrap(),
        "{\"metrics\":\"off\"}"
    );
    assert_eq!(
        stdout(&run(&home, &repo, &["config", "metrics"])),
        format!("metrics: off — repo {}\n", repo_file.display())
    );
}

#[test]
fn setup_should_refuse_piped_input_without_writing_configuration() {
    let home = Scratch::for_test("config", "setup-piped-home");
    let out = run(&home, &home, &["config", "setup"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("setup needs a terminal"));
    assert!(!home.join(".pixel/config.yaml").exists());
}

#[test]
fn classify_switch_should_create_config_and_repair_a_non_mapping_classify_section() {
    let home = Scratch::for_test("config", "classify-create-home");
    stdout(&run(&home, &home, &["config", "classify", "off"]));
    let path = home.join(".pixel/config.yaml");
    let doc: serde_json::Value =
        serde_saphyr::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(doc["classify"]["enabled"], false);
    fs::write(&path, "classify: stale\nmetrics: 'off'\n").unwrap();
    stdout(&run(&home, &home, &["config", "classify", "on"]));
    let doc: serde_json::Value =
        serde_saphyr::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        doc,
        serde_json::json!({"classify":{"enabled":true},"metrics":"off"})
    );
}

#[test]
fn classify_should_be_opt_in_even_when_an_engine_or_credentials_are_present() {
    let home = Scratch::for_test("config", "classify-default-off-home");
    for configuration in [
        None,
        Some("classify: {engine: remote}\nremote_keys: {openrouter: unused-test-key}\n"),
    ] {
        if let Some(config) = configuration {
            fs::create_dir_all(home.join(".pixel")).unwrap();
            fs::write(home.join(".pixel/config.yaml"), config).unwrap();
        }
        assert!(stdout(&run(&home, &home, &["config"])).contains("classify.enabled: false"));
        for args in [
            &["classify", "hello", "--engine", "remote"][..],
            &["classify", "hello", "--engine", "ollaya"][..],
            &["classify", "--jsonl"][..],
        ] {
            let out = run(&home, &home, args);
            assert!(!out.status.success());
            assert!(String::from_utf8_lossy(&out.stderr).contains("classify is disabled"));
            assert!(out.stdout.is_empty());
        }
        assert!(!home.join(".local/share/pixel/ollaya").exists());
    }
}

#[test]
fn classify_engine_jev_should_store_the_remote_preset_not_a_plain_engine() {
    let home = Scratch::for_test("config", "classify-engine-jev-home");
    stdout(&run(&home, &home, &["config", "classify-engine", "jev"]));
    let path = home.join(".pixel/config.yaml");
    let doc: serde_json::Value =
        serde_saphyr::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(doc["classify"]["engine"], "remote");
    assert_eq!(doc["classify"]["remote_preset"], "jev");

    stdout(&run(&home, &home, &["config", "classify-engine", "local"]));
    let doc: serde_json::Value =
        serde_saphyr::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(doc["classify"]["engine"], "local");
    assert_eq!(doc["classify"]["remote_preset"], "jev");
}

#[test]
fn classify_accepts_the_openai_preset_and_names_its_key_when_missing() {
    let home = Scratch::for_test("config", "classify-openai-home");
    let repo = Scratch::for_test("config", "classify-openai-repo");
    // The preset is a valid clap value.
    stdout(&run(&home, &repo, &["config", "remote-preset", "openai"]));
    let doc: serde_json::Value =
        serde_saphyr::from_str(&fs::read_to_string(home.join(".pixel/config.yaml")).unwrap())
            .unwrap();
    assert_eq!(doc["classify"]["remote_preset"], "openai");

    // Without a key anywhere the classify call must fail naming OPENAI_API_KEY
    // and the `pixel config remote-key openai` remedy — before any network call.
    stdout(&run(&home, &repo, &["config", "classify", "on"]));
    let out = run(
        &home,
        &repo,
        &[
            "classify",
            "t",
            "--engine",
            "remote",
            "--remote-preset",
            "openai",
            "--label",
            "a",
            "--label",
            "b",
        ],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(
            "remote preset openai needs an API key: set OPENAI_API_KEY or run `pixel config remote-key openai -`",
        ),
        "{stderr}"
    );
}

#[test]
fn config_remote_key_openai_round_trips_and_clears() {
    let home = Scratch::for_test("config", "remote-key-openai-home");
    stdout(&run(
        &home,
        &home,
        &["config", "remote-key", "openai", "sk-openai-test"],
    ));
    let doc: serde_json::Value =
        serde_saphyr::from_str(&fs::read_to_string(home.join(".pixel/config.yaml")).unwrap())
            .unwrap();
    assert_eq!(doc["remote_keys"]["openai"], "sk-openai-test");

    let out = run(&home, &home, &["config", "remote-key", "openai"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("set"));
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("sk-openai-test"),
        "the stored key value must never be echoed"
    );

    stdout(&run(
        &home,
        &home,
        &["config", "remote-key", "openai", "--clear"],
    ));
    let doc: serde_json::Value =
        serde_saphyr::from_str(&fs::read_to_string(home.join(".pixel/config.yaml")).unwrap())
            .unwrap();
    assert!(doc["remote_keys"].get("openai").is_none());
}

#[test]
fn brief_parts_should_show_its_effective_value_and_reject_what_is_not_a_part_count() {
    let home = Scratch::for_test("config", "brief-parts-home");
    let repo = Scratch::for_test("config", "brief-parts-repo");
    assert!(stdout(&run(&home, &repo, &["config"])).contains("brief_parts: 3\n"));
    fs::create_dir_all(home.join(".pixel")).unwrap();
    fs::create_dir_all(repo.join(".pixel")).unwrap();
    fs::write(home.join(".pixel/config.yaml"), "brief_parts: 5\n").unwrap();
    assert!(stdout(&run(&home, &repo, &["config"])).contains("brief_parts: 5\n"));
    fs::write(repo.join(".pixel/config.yaml"), "brief_parts: 1\n").unwrap();
    assert!(stdout(&run(&home, &repo, &["config"])).contains("brief_parts: 1\n"));
    for bad in ["0", "9", "-1", "2.5", "many", "[2]"] {
        fs::write(
            repo.join(".pixel/config.yaml"),
            format!("brief_parts: {bad}\n"),
        )
        .unwrap();
        let out = run(&home, &repo, &["config"]);
        assert!(!out.status.success(), "{bad}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("brief_parts must be a whole number from 1 to 8"),
            "{bad}: {stderr}"
        );
    }
}

fn repo_with_git(tag: &str) -> Scratch {
    let repo = Scratch::for_test("config", tag);
    crate::support::git(&repo, &["init", "-q"]);
    repo
}

fn yaml(path: &Path) -> serde_json::Value {
    serde_saphyr::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn a_known_setting_should_be_written_to_the_repository_layer_and_read_back() {
    let home = Scratch::for_test("config", "set-home");
    let repo = repo_with_git("set-repo");
    let repo_file = repo.join(".pixel/config.yaml");

    let text = stdout(&run(&home, &repo, &["config", "brief_parts", "5"]));
    assert!(
        text.contains(&format!("brief_parts: 5 — wrote {}", repo_file.display())),
        "{text}"
    );
    // Native hooks get their part count from `--part K/N`, written by
    // `pixel setup` from the global layer only (#1427): a repository count
    // reaches Pi's package alone, and the note must not send the user to a
    // `pixel setup` that leaves the count as it was.
    assert!(
        text.contains(
            "note: native hooks keep the 3 parts `pixel setup` registers from the global layer (default); \
             only Pi's package follows this repository value. To change the registered count: \
             `pixel config brief_parts 5 --global`, then `pixel setup`\n"
        ),
        "{text}"
    );
    assert!(
        !text.contains("so the registered hooks carry the new count"),
        "{text}"
    );
    assert_eq!(yaml(&repo_file)["brief_parts"], 5, "a number, not a string");
    assert!(!home.join(".pixel/config.yaml").exists());
    assert_eq!(
        stdout(&run(&home, &repo, &["config", "brief_parts"])),
        format!(
            "brief_parts: 5 — {}\nregistered by `pixel setup`: 3 — default\n",
            repo_file.display()
        )
    );
    assert!(stdout(&run(&home, &repo, &["config"])).contains("brief_parts: 5\n"));

    stdout(&run(&home, &repo, &["config", "brief", "off"]));
    assert_eq!(yaml(&repo_file)["brief"], false, "a boolean, not a string");
    // `pixel setup` does not read `brief`: its read has no registration line.
    assert_eq!(
        stdout(&run(&home, &repo, &["config", "brief"])),
        format!("brief: off — {}\n", repo_file.display())
    );
    assert!(stdout(&run(&home, &repo, &["config"])).contains("brief: false"));
}

#[test]
fn a_global_set_should_leave_the_repository_alone_and_name_a_layer_that_wins() {
    let home = Scratch::for_test("config", "global-set-home");
    let repo = repo_with_git("global-set-repo");
    let repo_file = repo.join(".pixel/config.yaml");
    stdout(&run(&home, &repo, &["config", "brief_parts", "2"]));

    let text = stdout(&run(
        &home,
        &repo,
        &["config", "brief_parts", "6", "--global"],
    ));
    let global_file = home.join(".pixel/config.yaml");
    assert!(
        text.contains(&format!("brief_parts: 6 — wrote {}", global_file.display())),
        "{text}"
    );
    // The repository value wins only where the configured count is read at
    // run time, Pi's package; native hooks carry the global count `pixel
    // setup` registers, so the global write still asks for `pixel setup`.
    assert!(
        text.contains(&format!(
            "note: {} sets brief_parts: 2 here and wins for Pi's package; native hooks keep the count `pixel setup` registers from the global layer\n",
            repo_file.display()
        )),
        "{text}"
    );
    assert!(
        text.contains("note: run `pixel setup` so the registered hooks carry the new count\n"),
        "{text}"
    );
    assert_eq!(yaml(&global_file)["brief_parts"], 6);
    assert_eq!(
        yaml(&repo_file)["brief_parts"],
        2,
        "the repository layer is untouched"
    );
}

#[test]
fn a_value_that_is_not_valid_for_its_setting_should_be_refused_and_write_nothing() {
    let home = Scratch::for_test("config", "invalid-home");
    let repo = repo_with_git("invalid-repo");
    for (key, bad, expected) in [
        (
            "brief_parts",
            "0",
            "brief_parts must be a whole number from 1 to 8",
        ),
        (
            "brief_parts",
            "9",
            "brief_parts must be a whole number from 1 to 8",
        ),
        (
            "brief_parts",
            "many",
            "brief_parts must be a whole number from 1 to 8",
        ),
        ("brief", "maybe", "brief takes on or off"),
        (
            "daemon_auto_start",
            "2",
            "daemon_auto_start takes on or off",
        ),
    ] {
        let out = run(&home, &repo, &["config", key, bad]);
        assert_eq!(out.status.code(), Some(1), "{key} {bad}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(expected), "{key} {bad}: {stderr}");
        assert!(out.stdout.is_empty(), "{key} {bad}");
        assert!(!repo.join(".pixel/config.yaml").exists(), "{key} {bad}");
        assert!(!home.join(".pixel/config.yaml").exists(), "{key} {bad}");
    }
}

#[test]
fn an_unknown_key_should_list_the_settings_and_never_echo_or_log_its_value() {
    let home = Scratch::for_test("config", "unknown-home");
    let repo = repo_with_git("unknown-repo");
    let secret = "sk-live-0123456789abcdef0123456789abcdef";
    for args in [
        vec!["config", "remote_keys.openai", secret],
        vec!["config", "--global", "remote_keys.openai", secret],
    ] {
        let out = run(&home, &repo, &args);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("unknown setting")
                && stderr.contains("brief_parts")
                && stderr.contains("daemon_auto_start"),
            "{stderr}"
        );
        assert!(!stderr.contains(secret), "{stderr}");
        assert!(!repo.join(".pixel/config.yaml").exists());
        assert!(!home.join(".pixel/config.yaml").exists());
    }
    // The action log recorded the attempt, with the value masked.
    let log = fs::read_to_string(repo.join(".pixel/actions.jsonl")).unwrap();
    assert!(
        log.contains("config remote_keys.openai <redacted>"),
        "{log}"
    );
    assert!(!log.contains(secret), "{log}");
}

#[test]
fn the_action_log_should_keep_the_value_of_a_setting_it_can_show() {
    let home = Scratch::for_test("config", "logged-home");
    let repo = repo_with_git("logged-repo");
    stdout(&run(&home, &repo, &["config", "brief_parts", "4"]));
    let log = fs::read_to_string(repo.join(".pixel/actions.jsonl")).unwrap();
    assert!(log.contains("config brief_parts 4"), "{log}");
}

#[test]
fn edit_hooks_should_be_off_by_default_and_follow_a_written_value() {
    let home = Scratch::for_test("config", "edit-hooks-home");
    let repo = repo_with_git("edit-hooks-repo");
    assert!(stdout(&run(&home, &repo, &["config"])).contains("edit_hooks: false (default)\n"));
    let text = stdout(&run(&home, &repo, &["config", "edit_hooks"]));
    assert!(text.starts_with("edit_hooks: off — default"), "{text}");

    let global = home.join(".pixel/config.yaml");
    let text = stdout(&run(
        &home,
        &repo,
        &["config", "edit_hooks", "on", "--global"],
    ));
    assert!(
        text.contains(&format!("edit_hooks: on — wrote {}", global.display())),
        "{text}"
    );
    assert!(text.contains("run `pixel setup`"), "{text}");
    assert_eq!(yaml(&global)["edit_hooks"], true);
    assert!(stdout(&run(&home, &repo, &["config"])).contains("edit_hooks: true"));

    // A repository layer wins, and anything but on/off is refused unwritten.
    stdout(&run(&home, &repo, &["config", "edit_hooks", "off"]));
    assert!(stdout(&run(&home, &repo, &["config", "edit_hooks"])).starts_with("edit_hooks: off"));
    let bad = run(&home, &repo, &["config", "edit_hooks", "maybe"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("edit_hooks takes on or off"));
}

/// `pixel setup` registers the edit hooks from the global layer only (#1427),
/// while the hook itself reads the repository layer first. A repository `on`
/// under a global `off` makes setup register nothing, so the write must not
/// send the user to a `pixel setup` that changes nothing. It must not claim
/// the value has no effect either: hooks an earlier setup installed are still
/// there, and the hook then obeys this value. `pixel config` cannot see what
/// is installed; it says so and names `pixel doctor`.
#[test]
fn a_repository_edit_hooks_on_under_a_global_off_should_name_what_setup_will_not_register() {
    let home = Scratch::for_test("config", "edit-hooks-repo-on-home");
    let repo = repo_with_git("edit-hooks-repo-on-repo");
    let text = stdout(&run(&home, &repo, &["config", "edit_hooks", "on"]));
    assert!(
        text.contains(
            "note: `pixel setup` does not register the edit hooks while the global layer has them off (default); \
             this value turns them on here only if an earlier setup left them installed (`pixel doctor` shows it). \
             To register them: `pixel config edit_hooks on --global`, then `pixel setup`\n"
        ),
        "{text}"
    );
    assert!(
        !text.contains("so the edit hooks are registered or removed")
            && !text.contains("no effect"),
        "neither a pointless `pixel setup` nor an unconditional claim about installed hooks: {text}"
    );
    assert_eq!(
        yaml(&repo.join(".pixel/config.yaml"))["edit_hooks"],
        true,
        "the value is still written: the hook reads it once the global layer registers the hooks"
    );
    let read = stdout(&run(&home, &repo, &["config", "edit_hooks"]));
    assert!(
        read.ends_with("registered by `pixel setup`: off — default\n"),
        "{read}"
    );
}

/// Under a global `on`, setup registers the hooks, and a repository value is
/// what the hook obeys here: `off` is a working per-repository opt-out. A
/// global `on` does not prove setup has run, so the note says "once they are
/// installed".
#[test]
fn a_repository_edit_hooks_value_should_say_it_applies_here_when_the_global_layer_registers_them() {
    let home = Scratch::for_test("config", "edit-hooks-registered-home");
    let repo = repo_with_git("edit-hooks-registered-repo");
    let global = home.join(".pixel/config.yaml");
    stdout(&run(
        &home,
        &repo,
        &["config", "edit_hooks", "on", "--global"],
    ));
    for (value, effect) in [("off", "off"), ("on", "on")] {
        let text = stdout(&run(&home, &repo, &["config", "edit_hooks", value]));
        assert!(
            text.contains(&format!(
                "note: `pixel setup` registers the edit hooks from the global layer ({}); \
                 once they are installed, this value turns them {effect} in this repository\n",
                global.display()
            )),
            "{value}: {text}"
        );
        assert!(!text.contains("does not register"), "{value}: {text}");
    }
    let read = stdout(&run(&home, &repo, &["config", "edit_hooks"]));
    assert!(
        read.ends_with(&format!(
            "registered by `pixel setup`: on — {}\n",
            global.display()
        )),
        "{read}"
    );
}

/// A repository `off` under a global `off` changes nothing anyone registers,
/// so it carries no registration note at all.
#[test]
fn a_repository_edit_hooks_off_under_a_global_off_should_print_no_registration_note() {
    let home = Scratch::for_test("config", "edit-hooks-off-off-home");
    let repo = repo_with_git("edit-hooks-off-off-repo");
    let text = stdout(&run(&home, &repo, &["config", "edit_hooks", "off"]));
    assert_eq!(
        text,
        format!(
            "edit_hooks: off — wrote {}\n",
            repo.join(".pixel/config.yaml").display()
        )
    );
}

/// Setup skips a global value of the wrong type and registers the default, so
/// the registration line must name `default`, not the file that holds the
/// unreadable value: value and source come from the same reading.
#[test]
fn a_registration_line_should_name_default_when_the_global_value_is_unreadable() {
    let home = Scratch::for_test("config", "registration-unreadable-home");
    let repo = repo_with_git("registration-unreadable-repo");
    fs::create_dir_all(home.join(".pixel")).unwrap();
    fs::write(
        home.join(".pixel/config.yaml"),
        "edit_hooks: maybe\nbrief_parts: many\n",
    )
    .unwrap();
    let edit = stdout(&run(&home, &repo, &["config", "edit_hooks"]));
    assert!(
        edit.ends_with("registered by `pixel setup`: off — default\n"),
        "{edit}"
    );
    let parts = stdout(&run(&home, &repo, &["config", "brief_parts"]));
    assert!(
        parts.ends_with("registered by `pixel setup`: 3 — default\n"),
        "{parts}"
    );
}

/// The source `pixel config <key>` names is the layer the value came from: a
/// repository `brief_parts` the reader skips (not a whole number) leaves the
/// global value in force, and the line must name the global file, not the
/// repository's (#1428).
#[test]
fn a_setting_read_should_name_the_layer_its_value_came_from() {
    let home = Scratch::for_test("config", "provenance-home");
    let repo = repo_with_git("provenance-repo");
    fs::create_dir_all(home.join(".pixel")).unwrap();
    fs::create_dir_all(repo.join(".pixel")).unwrap();
    fs::write(home.join(".pixel/config.yaml"), "brief_parts: 5\n").unwrap();
    fs::write(repo.join(".pixel/config.yaml"), "brief_parts: many\n").unwrap();
    let text = stdout(&run(&home, &repo, &["config", "brief_parts"]));
    assert!(
        text.starts_with(&format!(
            "brief_parts: 5 — {}\n",
            home.join(".pixel/config.yaml").display()
        )),
        "{text}"
    );
}
