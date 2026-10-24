// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The plugin host on the real binary: install, run through the dispatch of
//! an unknown word, trust, uninstall, and what the action log keeps.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;

use serde_json::Value;

use crate::support::{Fixture, action_log, project, stderr, stdout};

const MANIFEST: &str = "name = \"hello\"\nversion = \"1.0.0\"\napi = 1\ndescription = \"says hello\"\nrun = \"run.sh\"\ncapabilities = [\"command\"]\nskill = \"SKILL.md\"\n";

fn home(fixture: &Fixture) -> PathBuf {
    fixture.dir.path().join("home")
}

/// A plugin directory `hello` outside the repository: it prints what it was
/// told and exits with the status its first argument names.
fn plugin_source(fixture: &Fixture, manifest: &str) -> PathBuf {
    let dir = fixture.dir.path().join("hello-src");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("pixel-plugin.toml"), manifest).unwrap();
    std::fs::write(dir.join("SKILL.md"), "say hello\n").unwrap();
    let run = dir.join("run.sh");
    std::fs::write(
        &run,
        "#!/bin/sh\necho \"api=$PIXEL_API root=$PIXEL_REPO_ROOT args=$*\"\nexit \"${1:-0}\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn install(fixture: &Fixture, source: &std::path::Path) -> Output {
    fixture.run(&["install", &source.to_string_lossy()])
}

#[test]
fn a_directory_plugin_installs_delivers_its_skill_runs_and_uninstalls() {
    let fixture = project();
    let source = plugin_source(&fixture, MANIFEST);
    std::fs::create_dir_all(home(&fixture).join(".codex")).unwrap();
    let installed = install(&fixture, &source);
    assert!(installed.status.success(), "{}", stderr(&installed));
    let text = stdout(&installed);
    assert!(text.contains("installed plugin hello 1.0.0"), "{text}");
    assert!(text.contains("delivered skill"), "{text}");
    for root in [".claude", ".codex"] {
        assert_eq!(
            std::fs::read_to_string(home(&fixture).join(root).join("skills/hello/SKILL.md"))
                .unwrap(),
            "say hello\n"
        );
    }

    let listed = stdout(&fixture.run(&["plugin", "list"]));
    assert!(listed.contains("hello"), "{listed}");

    // `pixel hello …` is not a core command: the plugin runs, with its environment.
    let ran = fixture.run(&["hello", "0", "extra"]);
    assert!(ran.status.success(), "{}", stderr(&ran));
    let out = stdout(&ran);
    assert!(out.contains("api=1 root="), "{out}");
    assert!(out.contains("args=0 extra"), "{out}");
    // Its exit status is the plugin's own, and the run is in the action log.
    let failed = fixture.run(&["hello", "3"]);
    assert_eq!(failed.status.code(), Some(3));
    let runs: Vec<Value> = action_log(&fixture.root())
        .into_iter()
        .filter(|line| line["command"] == "hello")
        .collect();
    assert_eq!(runs.len(), 2, "{runs:?}");

    let removed = fixture.run(&["uninstall", "hello"]);
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(
        stdout(&removed).contains("removed skill"),
        "{}",
        stdout(&removed)
    );
    assert!(!home(&fixture).join(".claude/skills/hello").exists());
    let gone = fixture.run(&["hello"]);
    assert_eq!(
        gone.status.code(),
        Some(2),
        "clap's own error for an unknown word"
    );
}

#[test]
fn a_skill_copy_the_user_changed_is_kept_on_uninstall() {
    let fixture = project();
    let source = plugin_source(&fixture, MANIFEST);
    assert!(install(&fixture, &source).status.success());
    let copy = home(&fixture).join(".claude/skills/hello/SKILL.md");
    std::fs::write(&copy, "mine\n").unwrap();
    let removed = fixture.run(&["uninstall", "hello"]);
    assert!(
        stdout(&removed).contains("kept skill"),
        "{}",
        stdout(&removed)
    );
    assert_eq!(std::fs::read_to_string(copy).unwrap(), "mine\n");
}

#[test]
fn a_skill_named_like_a_core_skill_installs_nothing_anywhere() {
    let fixture = project();
    let manifest = MANIFEST.replace("name = \"hello\"", "name = \"pixel\"");
    let source = plugin_source(&fixture, &manifest);
    let installed = install(&fixture, &source);
    assert_eq!(installed.status.code(), Some(1));
    assert!(!home(&fixture).join(".claude/skills/pixel").exists());
}

#[test]
fn a_repo_plugin_runs_only_after_trust_and_loses_it_on_any_change() {
    let fixture = project();
    let dir = fixture.root().join(".pixel/plugins/probe");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pixel-plugin.toml"),
        "name = \"probe\"\nversion = \"1\"\napi = 1\ndescription = \"d\"\nrun = \"run.sh\"\ncapabilities = [\"command\"]\n",
    )
    .unwrap();
    let run = dir.join("run.sh");
    std::fs::write(&run, "#!/bin/sh\necho trusted-run\n").unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();

    let untrusted = fixture.run(&["probe"]);
    assert_eq!(untrusted.status.code(), Some(1));
    assert!(
        stderr(&untrusted).contains("pixel plugin trust probe"),
        "{}",
        stderr(&untrusted)
    );

    let trusted = fixture.run(&["plugin", "trust", "probe"]);
    assert!(trusted.status.success(), "{}", stderr(&trusted));
    assert!(stdout(&fixture.run(&["probe"])).contains("trusted-run"));

    std::fs::write(&run, "#!/bin/sh\necho changed\n").unwrap();
    let revoked = fixture.run(&["probe"]);
    assert_eq!(revoked.status.code(), Some(1), "{}", stderr(&revoked));
    assert!(!stdout(&revoked).contains("changed"));
}

#[test]
fn an_npm_source_is_refused_with_exit_two_and_a_url_credential_is_masked_in_the_log() {
    let fixture = project();
    let npm = fixture.run(&["install", "npm:left-pad"]);
    assert_eq!(npm.status.code(), Some(2), "{}", stderr(&npm));
    assert!(stderr(&npm).contains("npm"), "{}", stderr(&npm));

    let secret = fixture.run(&["install", "https://user:hunter2@127.0.0.1:9/none.git"]);
    assert_eq!(secret.status.code(), Some(1));
    let log = std::fs::read_to_string(fixture.root().join(".pixel/actions.jsonl")).unwrap();
    assert!(log.contains("\"command\":\"install\""), "{log}");
    assert!(
        !log.contains("hunter2"),
        "the credential reached the action log"
    );
    assert!(!stderr(&secret).contains("hunter2"), "{}", stderr(&secret));
}
