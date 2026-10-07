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
        .env_remove("PIXEL_TASK_CONTEXT")
        .env_remove("PIXEL_TASK_BOUNDARY")
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
    fs::write(home.join(".pixel/config.yaml"), "metrics: 'off'\ndaemon_auto_start: false\ntask_context: false\nclassify: {engine: remote, remote_preset: deepseek}\nremote_keys: {deepseek: sk-secret}\nunknown: hidden-secret\n").unwrap();
    fs::write(
        repo.join(".pixel/config.yaml"),
        "metrics: 'on'\ntask_context: true\n",
    )
    .unwrap();
    let out = run(&home, &repo, &["config"]);
    let text = stdout(&out);
    assert!(text.contains("metrics: on (Repo)"), "{text}");
    assert!(text.contains("daemon_auto_start: false"), "{text}");
    assert!(text.contains("task_context: true"), "{text}");
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

#[cfg(unix)]
#[test]
fn editor_should_receive_one_path_preserve_legacy_values_and_report_bad_edits() {
    let home = Scratch::for_test("config", "editor home's");
    let repo = Scratch::for_test("config", "editor repo's");
    fs::create_dir_all(home.join(".pixel")).unwrap();
    fs::create_dir_all(repo.join(".pixel")).unwrap();
    fs::write(
        home.join(".pixel/config.json"),
        "{\"metrics\":\"off\",\"future\":42}",
    )
    .unwrap();
    let script = home.join("editor script.sh");
    fs::write(&script, "test \"$1\" = '--wait' || exit 19\ntest \"$#\" = 2 || exit 20\nprintf '\\n# edited\\n' >> \"$2\"\n").unwrap();
    let editor = format!("sh {} --wait", shell_words::quote(script.to_str().unwrap()));
    let invoke = |args: &[&str]| {
        pixel_command()
            .env("HOME", &*home)
            .env("VISUAL", &editor)
            .env("EDITOR", "does-not-exist")
            .current_dir(&*repo)
            .args(args)
            .output()
            .unwrap()
    };
    stdout(&invoke(&["config", "edit"]));
    let path = home.join(".pixel/config.yaml");
    let contents = fs::read_to_string(&path).unwrap();
    assert!(contents.contains("# edited"));
    let value: serde_json::Value = serde_saphyr::from_str(&contents).unwrap();
    assert_eq!(value, serde_json::json!({"metrics":"off", "future":42}));
    stdout(&invoke(&["config", "edit", "--repo"]));
    assert!(
        fs::read_to_string(repo.join(".pixel/config.yaml"))
            .unwrap()
            .contains("# edited")
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        contents,
        "repo edit leaves global untouched"
    );
    fs::write(&script, "printf 'metrics: [sk-secret' > \"$2\"\n").unwrap();
    let out = invoke(&["config", "edit"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid configuration"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("sk-secret"));
    fs::write(&script, "exit 42\n").unwrap();
    let out = invoke(&["config", "edit"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("editor exited"));
}

#[cfg(unix)]
#[test]
fn editor_should_fall_back_from_blank_visual_to_editor_then_vi() {
    use std::os::unix::fs::PermissionsExt;
    let home = Scratch::for_test("config", "fallback-home");
    let script = home.join("vi");
    fs::write(
        &script,
        "#!/bin/sh\nprintf '\\n# fallback editor\\n' >> \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let editor = shell_words::quote(script.to_str().unwrap()).into_owned();
    for configured in [&editor, "  "] {
        let out = pixel_command()
            .env("HOME", &*home)
            .env("VISUAL", "  ")
            .env("EDITOR", configured)
            .env("PATH", &*home)
            .current_dir(&*home)
            .args(["config", "edit"])
            .output()
            .unwrap();
        stdout(&out);
    }
    let contents = fs::read_to_string(home.join(".pixel/config.yaml")).unwrap();
    assert_eq!(contents.matches("# fallback editor").count(), 2);
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

#[test]
fn policy_should_resolve_environment_then_repository_then_global_and_write_each_layer() {
    let home = Scratch::for_test("config", "policy-home");
    let repo = Scratch::for_test("config", "policy-repo");
    crate::support::git(&repo, &["init", "-q"]);
    let global = home.join(".pixel/config.yaml");
    let repo_file = repo.join(".pixel/config.yaml");
    let query = |cwd: &Path, args: &[&str]| {
        pixel_command()
            .env("HOME", &*home)
            .env_remove("PIXEL_POLICY")
            .env_remove("PIXEL_TARGETS_GUARD")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap()
    };
    let policy = |args: &[&str]| stdout(&query(&repo, args));

    assert_eq!(
        policy(&["config", "policy"]),
        "policy: advisory — default (no config sets it)\n"
    );
    assert!(policy(&["config"]).contains("policy: advisory (default)\n"));
    let report = |cwd: &Path| -> serde_json::Value {
        let out = stdout(&query(cwd, &["config", "policy", "--json"]));
        assert!(out.ends_with('\n'), "{out}");
        serde_json::from_str(&out).unwrap()
    };
    assert_eq!(
        report(&repo),
        serde_json::json!({"policy":"advisory", "source":"default"})
    );

    // The global layer decides where no repository overrides it.
    fs::create_dir_all(home.join(".pixel")).unwrap();
    fs::write(&global, "policy: enforce\n").unwrap();
    assert_eq!(
        policy(&["config", "policy"]),
        format!("policy: enforce — global {}\n", global.display())
    );

    // `pixel config policy` writes the repository layer; only --global
    // touches the machine-wide file.
    assert_eq!(
        policy(&["config", "policy", "off"]),
        format!("policy: off — wrote {}\n", repo_file.display())
    );
    assert_eq!(
        report(&repo),
        serde_json::json!({
            "policy":"off", "source":"repo", "file": repo_file.display().to_string()
        })
    );
    assert_eq!(
        policy(&["config", "policy", "advisory", "--global"]),
        format!("policy: advisory — wrote {}\n", global.display())
    );
    let doc: serde_json::Value =
        serde_saphyr::from_str(&fs::read_to_string(&repo_file).unwrap()).unwrap();
    assert_eq!(doc, serde_json::json!({"policy":"off"}));

    // The environment overrides every file layer for one process.
    assert_eq!(
        stdout(
            &pixel_command()
                .env("HOME", &*home)
                .env("PIXEL_POLICY", "enforce")
                .current_dir(&*repo)
                .args(["config", "policy"])
                .output()
                .unwrap()
        ),
        "policy: enforce — PIXEL_POLICY (environment)\n"
    );

    // An unknown argument is refused instead of written.
    let out = query(&repo, &["config", "policy", "loudly"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("possible values: advisory, enforce, off"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // A hand-written value the reader cannot use is a validation error, not a
    // half-applied setting.
    fs::write(&repo_file, "policy: loudly\n").unwrap();
    let out = query(&repo, &["config"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("policy must be advisory, enforce, or off"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The 0.6.1 report: setup saved `metrics: "on"` globally while a legacy repo
/// `config.json` carried `metrics: "off"`, so every launch in that repository
/// hid the footer. The global write must name the layer that wins instead of
/// letting the answer look ignored.
#[test]
fn metrics_on_global_should_report_a_contradicting_repo_layer() {
    let home = Scratch::for_test("config", "metrics-home");
    let repo = Scratch::for_test("config", "metrics-repo");
    crate::support::git(&repo, &["init", "-q"]);
    let legacy = repo.join(".pixel/config.json");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::write(&legacy, "{\"metrics\":\"off\"}").unwrap();

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
            legacy.display()
        )),
        "{text}"
    );
    // The note only reports: the repo layer is untouched, and the report
    // command names it as the winner.
    assert_eq!(
        fs::read_to_string(&legacy).unwrap(),
        "{\"metrics\":\"off\"}"
    );
    assert_eq!(
        stdout(&run(&home, &repo, &["config", "metrics"])),
        format!("metrics: off — repo {}\n", legacy.display())
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
