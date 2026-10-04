// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Real CLI boundaries with a disposable home and a fake browser only.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture(PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pixel-flow-cli-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let stub = root.join("bin/agent-browser");
        std::fs::write(&stub, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$AUDIT_CALLS\"\nprintf 'https://example.test\\n'\nexit \"${AUDIT_EXIT:-0}\"\n").unwrap();
        std::fs::set_permissions(stub, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            root.join("steps.json"),
            r#"[{"action":"open","url":"https://example.test"}]"#,
        )
        .unwrap();
        let fixture = Self(root);
        let saved = fixture.run(
            &[
                "flow",
                "save",
                "audit",
                "--title",
                "Audit",
                "--from-file",
                "steps.json",
                "--json",
            ],
            false,
        );
        assert!(saved.status.success(), "{saved:?}");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&saved.stdout).unwrap()["saved"],
            true
        );
        fixture
    }

    fn run(&self, args: &[&str], failing_browser: bool) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pixel"))
            .args(args)
            .current_dir(&self.0)
            .env("HOME", &self.0)
            .env("PIXEL_FLOW_DIR", self.0.join("flows"))
            .env("PIXEL_METRICS", "0")
            .env("PIXEL_DAEMON_AUTO_START", "0")
            .env("PATH", self.0.join("bin"))
            .env("AUDIT_CALLS", self.0.join("calls"))
            .env("AUDIT_EXIT", if failing_browser { "7" } else { "0" })
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            crate::support::assert_no_daemon(&self.0);
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn saving_an_existing_flow_fails_and_names_the_current_revise_command() {
    let fixture = Fixture::new("dup");
    let output = fixture.run(
        &[
            "flow",
            "save",
            "audit",
            "--title",
            "Audit again",
            "--from-file",
            "steps.json",
            "--json",
        ],
        false,
    );
    assert!(
        !output.status.success(),
        "a second save must not overwrite the proven flow: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("`pixel flow revise audit`"),
        "the error must name the command that updates the flow: {stderr}"
    );
}

#[test]
fn flow_json_lifecycle_emits_documents_and_executes_only_when_requested() {
    let fixture = Fixture::new("json");
    for args in [
        vec!["flow", "get", "audit", "--json"],
        vec!["flow", "list", "--json"],
        vec!["flow", "show", "audit", "--json"],
        vec![
            "flow",
            "revise",
            "audit",
            "--title",
            "Revised audit",
            "--json",
        ],
        vec!["flow", "replay", "audit", "--json"],
    ] {
        let output = fixture.run(&args, false);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|e| panic!("{args:?}: {e}: {output:?}"));
        assert!(value.is_object() || value.is_array(), "{args:?}: {value}");
    }
    assert!(!fixture.0.join("calls").exists());
    let output = fixture.run(&["flow", "run", "audit", "--json"], false);
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["success"], true, "{value}");
    let calls = std::fs::read_to_string(fixture.0.join("calls")).unwrap();
    // The open's first browser call is the URL probe the navigation poll
    // compares against; the open itself follows. (The stub logs every arg,
    // session flag included.)
    assert!(
        calls.lines().next().unwrap().ends_with("get url"),
        "{calls}"
    );
    assert!(calls.contains("open https://example.test"), "{calls}");
    let deleted = fixture.run(&["flow", "delete", "audit", "--json"], false);
    assert!(deleted.status.success(), "{deleted:?}");
    assert!(serde_json::from_slice::<serde_json::Value>(&deleted.stdout).is_ok());
    assert!(!fixture.0.join("flows/audit.json").exists());
}

#[test]
fn flow_execute_failure_is_nonzero_and_never_a_success_document() {
    let fixture = Fixture::new("failure");
    let output = fixture.run(&["flow", "run", "audit", "--json"], true);
    assert!(
        !output.status.success(),
        "browser failure must fail CLI: {output:?}"
    );
    // Under `--json` the answer is the failure envelope, never a success
    // document: `ok: false` with the reason and no result, so a parser cannot
    // read a failed execution as a completed one.
    let doc: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("--json failure must be a JSON document ({e}): {output:?}"));
    assert_eq!(doc["ok"], false, "{output:?}");
    assert!(doc["result"].is_null(), "{output:?}");
    assert!(
        doc["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("flow execution failed")),
        "{output:?}"
    );
    assert!(!output.stderr.is_empty());
    assert!(fixture.0.join("calls").exists());
}

/// Without `--json`, `flow run` prints the browser log to stderr and one
/// summary line to stdout: the agent reads the verdict, not a document.
#[test]
fn flow_execute_prints_a_summary_line_and_the_log_on_stderr() {
    let fixture = Fixture::new("summary");
    let output = fixture.run(&["flow", "run", "audit"], false);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim_end(),
        "✓ Flow executed: 1 steps, 0 skipped",
        "{output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("# Executing flow: audit"), "{stderr}");
    assert!(
        stderr.contains("agent-browser open \"https://example.test\""),
        "{stderr}"
    );
}

/// `pixel flow show <name>` prints the flow document to stdout (a JSON
/// document, not the rendered command sequence). A mutant that deletes the
/// `Show` arm in `run_command` routes `show` through the wrong path; this
/// test pins the stdout shape.
#[test]
fn flow_show_prints_the_flow_document_to_stdout() {
    let fixture = Fixture::new("print");
    let show = fixture.run(&["flow", "show", "audit"], false);
    assert!(show.status.success(), "{show:?}");
    let show_stdout = String::from_utf8_lossy(&show.stdout);
    let show_json: serde_json::Value =
        serde_json::from_slice(&show.stdout).expect("show must emit valid JSON");
    assert_eq!(show_json["title"], "Audit", "{show_stdout:?}");
    assert_eq!(
        show_json["name"], "audit",
        "show must round-trip the flow name: {show_stdout:?}"
    );
}
