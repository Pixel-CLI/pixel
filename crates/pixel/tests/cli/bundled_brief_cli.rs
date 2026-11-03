// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::support::{Scratch, git, pixel_command};

const PROMPT: &str =
    "What does the fixture-engine library expose, and which other crates does it depend on?";

fn isolated_pixel(home: &Path, root: &Path) -> Command {
    let mut command = pixel_command();
    command
        .current_dir(root)
        .env("HOME", home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("AI_AGENT", "codex")
        .env("PIXEL_METRICS", "0")
        .env("PIXEL_BRIEF_PROBES", "1")
        .env("PIXEL_BRIEF_LOG", "1")
        .env("PIXEL_BRIEF_PACK", "1")
        .env_remove("PIXEL_BRIEF");
    command
}

fn fixture(tag: &str) -> Scratch {
    let root = Scratch::for_test("bundled-brief", tag);
    std::fs::create_dir_all(root.join("crates/engine/src")).unwrap();
    std::fs::create_dir_all(root.join("crates/codec/src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/engine\", \"crates/codec\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("crates/engine/Cargo.toml"),
        [
            "[package]",
            "name = \"fixture-engine\"",
            "version = \"0.1.0\"",
            "edition = \"2021\"",
            "description = \"Fixture engine facade for native probe coverage.\"",
            "",
            "[dependencies]",
            "fixture-codec = { path = \"../codec\" }",
            "",
        ]
        .join("\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("crates/engine/src/lib.rs"),
        "//! The fixture engine exposes its codec through a stable facade.\npub use fixture_codec::Codec;\npub struct Engine;\n",
    )
    .unwrap();
    std::fs::write(
        root.join("crates/codec/Cargo.toml"),
        [
            "[package]",
            "name = \"fixture-codec\"",
            "version = \"0.1.0\"",
            "edition = \"2021\"",
            "description = \"Fixture codec used by the engine facade.\"",
            "",
        ]
        .join("\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("crates/codec/src/lib.rs"),
        "//! The fixture codec converts byte slices for engine consumers.\npub struct Codec;\n",
    )
    .unwrap();
    std::fs::write(root.join("fixture-marker.txt"), format!("{tag}\n")).unwrap();
    std::fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "fixture"]);
    root
}

fn prompt_submit(home: &Path, root: &Path) -> Value {
    let payload = json!({
        "session_id": "bundled-overview-session",
        "prompt": PROMPT,
        "cwd": root,
        "hook_event_name": "UserPromptSubmit",
    });
    let mut command = isolated_pixel(home, root);
    command
        .args([
            "run-hook",
            "task-event",
            "--provider",
            "codex",
            "--event",
            "prompt-submit",
            "--part",
            "1/3",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn native prompt hook");
    child
        .stdin
        .take()
        .expect("prompt hook stdin")
        .write_all(payload.to_string().as_bytes())
        .expect("write prompt hook payload");
    let output = child
        .wait_with_output()
        .expect("wait for native prompt hook");
    assert!(
        output.status.success(),
        "native prompt hook: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("prompt hook JSON")
}

#[test]
fn setup_should_install_the_package_overview_probe_into_native_hook_context() {
    let root = fixture("native-overview");
    let home = Scratch::for_test("bundled-brief-home", "codex");
    std::fs::create_dir_all(home.join(".codex")).unwrap();

    let setup = isolated_pixel(&home, &root)
        .args(["setup", "--yes", "--json"])
        .output()
        .expect("run isolated setup");
    assert!(
        setup.status.success(),
        "setup: {}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let report: Value = serde_json::from_slice(&setup.stdout).expect("setup JSON report");
    assert_eq!(report["ok"], true, "{report}");
    assert!(
        home.join(".pixel/plugins/overview-answer-evidence/probe")
            .is_file(),
        "setup must install the bundled overview probe"
    );

    for (args, label) in [
        (
            vec![
                "search-content",
                "-F",
                "Engine",
                "-l",
                "--no-daemon",
                "--metrics",
                "off",
            ],
            "build text index",
        ),
        (
            vec!["rebuild-graph", "--metrics", "off"],
            "build code graph",
        ),
    ] {
        let output = isolated_pixel(&home, &root)
            .args(args)
            .arg(&*root)
            .output()
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let hook = prompt_submit(&home, &root);
    let context = hook["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_else(|| panic!("native hook returned no context: {hook}"));
    for evidence in [
        "The fixture engine exposes its codec through a stable facade.",
        "pub use fixture_codec::Codec;",
        "fixture-codec — Fixture codec used by the engine facade.",
        "The fixture codec converts byte slices for engine consumers.",
    ] {
        assert!(
            context.contains(evidence),
            "missing {evidence:?}: {context}"
        );
    }

    let log = std::fs::read_to_string(root.join(".pixel/brief-decisions.jsonl"))
        .expect("native hook decision log");
    let runs: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).expect("decision JSON"))
        .collect();
    assert!(
        runs.iter()
            .flat_map(|row| row["probes"].as_array().into_iter().flatten())
            .any(|probe| probe["name"] == "overview-answer-evidence" && probe["outcome"] == "ok"),
        "native decision log must record a successful overview probe: {runs:?}"
    );
}
