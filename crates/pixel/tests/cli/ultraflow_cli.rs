// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel ultraflow`'s outer contract: which verb exists, what it refuses
//! before it touches a browser or a model, and where a flow comes from.
//!
//! Every test here stops before `agent-browser` is spawned — the mechanical
//! loop is covered by `pixel-ultraflow`'s own suite, whose browser and
//! engine are scripted, and by live runs during development. What this file
//! pins is the part a user meets first: the flags, the kill switch, and the
//! order the two are checked in.

use std::fs;

use crate::support::{Scratch, pixel_command};

/// One flow document, saved into the store the run is pointed at.
const FLOW: &str = r#"{
  "name": "sign-in",
  "title": "Sign in",
  "description": "Sign in",
  "tags": ["ultraflow"],
  "url": "https://example.com",
  "steps": [{"action": "snapshot"}],
  "created_unix": 1, "revised_unix": 1, "revision": 1, "proven": false
}"#;

/// A scratch flow store holding `FLOW`.
fn store(tag: &str) -> Scratch {
    let dir = Scratch::for_test("ultraflow", tag);
    fs::write(dir.join("sign-in.json"), FLOW).unwrap();
    dir
}

/// `classify` is a global kill switch, and ultraflow is a classify caller:
/// with it off the run must say so, not silently open an engine.
#[test]
fn ultraflow_respects_the_classify_kill_switch() {
    let dir = store("disabled");
    let out = pixel_command()
        .env("PIXEL_FLOW_DIR", &*dir)
        .env("PIXEL_METRICS", "0")
        .args(["ultraflow", "replay", "sign-in"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("classify is disabled"), "{stderr}");
    assert!(stderr.contains("pixel config classify on"), "{stderr}");
}

/// The flow is read before the engine is resolved: an unknown name is an
/// error about the name, never about the model.
#[test]
fn an_unknown_flow_is_named_before_any_engine_is_opened() {
    let dir = Scratch::for_test("ultraflow", "unknown");
    let out = pixel_command()
        .env("PIXEL_FLOW_DIR", &*dir)
        .env("PIXEL_METRICS", "0")
        .args(["ultraflow", "replay", "not-there"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not-there"), "{stderr}");
    assert!(
        !stderr.contains("classify"),
        "the flow is read before the engine is resolved: {stderr}"
    );
}

/// `--var` is the caller's own input, so a malformed one is refused before
/// anything is opened — on both verbs.
#[test]
fn a_malformed_variable_is_refused_before_the_engine() {
    let dir = store("bad-var");
    let discover = pixel_command()
        .env("PIXEL_FLOW_DIR", &*dir)
        .env("PIXEL_METRICS", "0")
        .args([
            "ultraflow",
            "discover",
            "--url",
            "https://example.com",
            "--goal",
            "do the thing",
            "--var",
            "broken",
        ])
        .output()
        .unwrap();
    assert!(!discover.status.success());
    assert_eq!(
        String::from_utf8_lossy(&discover.stderr).trim(),
        "pixel: --var expects key=value, got 'broken'"
    );

    let replay = pixel_command()
        .env("PIXEL_FLOW_DIR", &*dir)
        .env("PIXEL_METRICS", "0")
        .args(["ultraflow", "replay", "sign-in", "--var", "=Zurich"])
        .output()
        .unwrap();
    assert!(!replay.status.success());
    assert_eq!(
        String::from_utf8_lossy(&replay.stderr).trim(),
        "pixel: --var expects a name before '=', got '=Zurich'"
    );
}

/// A `discover` run needs both halves of its task: the page and the goal.
#[test]
fn discover_needs_a_page_and_a_goal() {
    let out = pixel_command()
        .args(["ultraflow", "discover"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--url <URL>"), "{stderr}");
    assert!(stderr.contains("--goal <GOAL>"), "{stderr}");
}

/// With the switch on, the engine is resolved and the flow's own required
/// inputs are checked before any browser work — proven with an engine
/// address nothing answers at.
#[test]
fn an_enabled_run_reports_the_flows_missing_variable() {
    let home = Scratch::for_test("ultraflow", "enabled-home");
    fs::create_dir_all(home.join(".pixel")).unwrap();
    fs::write(
        home.join(".pixel/config.yaml"),
        "classify: {enabled: true}\n",
    )
    .unwrap();
    let dir = Scratch::for_test("ultraflow", "required-var");
    fs::write(
        dir.join("gated.json"),
        FLOW.replace(r#""name": "sign-in""#, r#""name": "gated""#)
            .replace(
                r#""steps": [{"action": "snapshot"}]"#,
                r#""vars": [{"name": "account", "description": "which account", "required": true}],
               "steps": [{"action": "snapshot"}]"#,
            ),
    )
    .unwrap();
    let out = pixel_command()
        .env("HOME", &*home)
        .env("PIXEL_FLOW_DIR", &*dir)
        .env("PIXEL_METRICS", "0")
        .args([
            "ultraflow",
            "replay",
            "gated",
            "--engine",
            "ollaya",
            "--ollaya-url",
            "http://127.0.0.1:9/",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("missing required variable 'account' for flow 'gated'"),
        "{stderr}"
    );
}

/// The flags a caller drives the loop with are the command's contract: they
/// are the same engine inputs `pixel classify` takes, and the loop's own
/// bounds.
#[test]
fn both_verbs_offer_their_flags() {
    let discover = pixel_command()
        .args(["ultraflow", "discover", "--help"])
        .output()
        .unwrap();
    assert!(discover.status.success());
    let text = String::from_utf8_lossy(&discover.stdout);
    for flag in [
        "--url <URL>",
        "--goal <GOAL>",
        "--var <KEY=VALUE>",
        "--save <SAVE>",
        "--title <TITLE>",
        "--tag <TAGS>",
        "--max-steps <MAX_STEPS>",
        "--max-stalled <MAX_STALLED>",
        "--repeat <REPEAT>",
        "--engine <ENGINE>",
        "--remote-preset <REMOTE_PRESET>",
        "--remote-model <REMOTE_MODEL>",
        "--ollaya-url <OLLAYA_URL>",
    ] {
        assert!(
            text.contains(flag),
            "{flag} is missing from discover --help"
        );
    }

    let replay = pixel_command()
        .args(["ultraflow", "replay", "--help"])
        .output()
        .unwrap();
    assert!(replay.status.success());
    let text = String::from_utf8_lossy(&replay.stdout);
    for flag in [
        "--var <KEY=VALUE>",
        "--no-repair",
        "--max-repairs <MAX_REPAIRS>",
        "--update",
        "--engine <ENGINE>",
        "--remote-preset <REMOTE_PRESET>",
    ] {
        assert!(text.contains(flag), "{flag} is missing from replay --help");
    }
}
