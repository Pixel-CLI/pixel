// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Post-edit hook runs against an existing graph snapshot, never a refreshed source scan.
use pixel_daemon::api::GRAPH_DB_FILE;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

const PIXEL: &str = env!("CARGO_BIN_EXE_pixel");
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new(callers: usize) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pixel-post-edit-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn saved() {}\npub fn local() { saved(); }\n",
        )
        .unwrap();
        fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
        for i in 0..callers {
            fs::write(
                root.join(format!("src/caller_{i:02}.rs")),
                format!("use crate::saved;\npub fn call_{i}() {{ saved(); }}\n"),
            )
            .unwrap();
        }
        for args in [
            vec!["init", "-q"],
            vec!["add", "."],
            vec!["commit", "-qm", "fixture"],
        ] {
            let result = Command::new("git")
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(&root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(result.status.success(), "{result:?}");
        }
        let fixture = Self(root.canonicalize().unwrap());
        let result = Command::new(PIXEL)
            .args(["repo-map", ".", "--json"])
            .current_dir(&fixture.0)
            .env("PIXEL_DAEMON_AUTO_START", "0")
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
        fixture
    }
    fn hook(&self, tool: &str, path: &str) -> Output {
        let payload =
            json!({"cwd": self.0, "tool_name":tool, "tool_input":{"file_path": self.0.join(path)}});
        let mut child = Command::new(PIXEL)
            .args(["run-hook", "post-tool-use", "--provider", "claude"])
            .current_dir(&self.0)
            .env("PIXEL_DAEMON_AUTO_START", "0")
            .env("PIXEL_METRICS", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
    fn note(&self) -> String {
        let output = self.hook("Edit", "src/lib.rs");
        assert!(output.status.success());
        assert!(
            output.stderr.is_empty(),
            "protected hook must not append metrics"
        );
        let data: Value = serde_json::from_slice(&output.stdout)
            .expect("one JSON document, no concatenated payloads");
        assert_eq!(data["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert!(data.get("decision").is_none());
        assert!(
            data["hookSpecificOutput"]
                .get("permissionDecision")
                .is_none()
        );
        assert_eq!(
            data["systemMessage"],
            data["hookSpecificOutput"]["additionalContext"]
        );
        data["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_string()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            crate::support::assert_no_daemon(&self.0);
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn same_file_callers_are_not_reported_as_elsewhere() {
    let fixture = Fixture::new(0);
    let note = fixture.note();
    assert!(note.contains("0 cross-file referencing symbols"), "{note}");
    assert!(note.contains("1 same-file referencing symbols"), "{note}");
    assert!(note.contains("Dependent paths: none indexed"));
    assert!(note.contains("lower_bound=false"));
    assert!(!note.contains("symbols elsewhere"));
}

#[test]
fn changed_source_keeps_snapshot_dependants_and_discloses_unchecked_freshness() {
    let fixture = Fixture::new(1);
    // The post-edit graph is deliberately stale: the changed file no longer
    // contains the indexed functions. A hook refresh would lose the evidence.
    fs::write(
        fixture.0.join("src/lib.rs"),
        "// functions removed by the edit\n",
    )
    .unwrap();
    let note = fixture.note();
    assert!(
        note.contains("1 cross-file referencing symbols in 1 files"),
        "{note}"
    );
    assert!(note.contains("1 same-file referencing symbols"));
    assert!(note.contains("src/caller_00.rs"));
    assert!(note.contains("Freshness unchecked"));
    assert!(note.contains("may predate the edit"));
    assert!(note.contains("no refresh or source read"));
}

#[test]
fn dependant_paths_are_bounded_and_partial_without_dropping_total_counts() {
    let fixture = Fixture::new(12);
    let note = fixture.note();
    assert!(
        note.contains("12 cross-file referencing symbols in 12 files"),
        "{note}"
    );
    assert_eq!(note.matches("src/caller_").count(), 8);
    assert!(note.contains("src/caller_00.rs"));
    assert!(note.contains("src/caller_07.rs"));
    assert!(!note.contains("src/caller_08.rs"));
    assert!(note.contains("paths_capped=true; lower_bound=true"));
    assert!(note.len() < 4096);
}

#[test]
fn non_edit_missing_file_and_missing_graph_are_silent_allow() {
    let fixture = Fixture::new(1);
    for (tool, path) in [("Read", "src/lib.rs"), ("Edit", "src/missing.rs")] {
        let output = fixture.hook(tool, path);
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
    fs::remove_file(fixture.0.join(".pixel").join(GRAPH_DB_FILE)).unwrap();
    let output = fixture.hook("Edit", "src/lib.rs");
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn unresolved_receiver_evidence_stays_uncertain_without_inflating_known_callers() {
    let fixture = Fixture::new(1);
    let store =
        pixel_graph::GraphStore::open(&fixture.0.join(".pixel").join(GRAPH_DB_FILE)).unwrap();
    let caller = store.file_by_path("src/caller_00.rs").unwrap().unwrap();
    store
        .insert_unresolved_call(
            caller.id,
            "saved",
            None,
            3,
            Some("unknown_receiver"),
            "call",
        )
        .unwrap();
    drop(store);
    let note = fixture.note();
    assert!(
        note.contains("1 cross-file referencing symbols in 1 files"),
        "{note}"
    );
    assert!(note.contains("paths_capped=false; lower_bound=true"));
    assert!(note.contains("unresolved same-name calls: 1"));
    assert!(note.contains("References may be approximate"));
}
