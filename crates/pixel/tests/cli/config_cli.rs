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
