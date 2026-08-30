//! Integration: `pixel hook guard` deny-with-answer over a real scratch
//! indexed repo — the Grep deny must embed actual pixel-search result
//! lines, and fall back to the suggestion-only message when the child
//! search cannot answer.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PIXEL: &str = env!("CARGO_BIN_EXE_pixel");

/// Unique scratch dir per test.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pixel-guard-deny-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?} failed in {}", dir.display());
}

/// A committed git repo containing a needle, with the pixel text index
/// built (first `pixel search` builds it lazily).
fn indexed_repo(tag: &str) -> PathBuf {
    let dir = scratch(tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn guard_needle() {\n    let GUARD_NEEDLE_XYZ = 42;\n    let _ = GUARD_NEEDLE_XYZ;\n}\n",
    )
    .unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "seed"]);
    // Build the index lazily via a first search; must succeed and hit.
    let out = Command::new(PIXEL)
        .args(["search", "GUARD_NEEDLE_XYZ", dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "seed search failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir
}

/// Pipe a PreToolUse payload into `pixel hook guard`; return (code, stderr).
fn run_guard(payload: &serde_json::Value) -> (i32, String) {
    let mut child = Command::new(PIXEL)
        .args(["hook", "guard"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn grep_deny_includes_inline_search_results() {
    let repo = indexed_repo("inline");
    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Grep",
        "cwd": repo.to_str().unwrap(),
        "tool_input": {"pattern": "GUARD_NEEDLE_XYZ"},
    });
    let (code, stderr) = run_guard(&payload);
    assert_eq!(code, 2, "Grep in an indexed repo must be denied: {stderr}");
    assert!(
        stderr.contains("BLOCKED Grep"),
        "deny-with-answer header missing: {stderr}"
    );
    assert!(
        stderr.contains("GUARD_NEEDLE_XYZ") && stderr.contains("lib.rs"),
        "actual search-result lines must be inline in the deny: {stderr}"
    );
    assert!(
        stderr.contains("pixel search"),
        "follow-up command must be present: {stderr}"
    );
}

#[test]
fn grep_deny_falls_back_when_search_cannot_answer() {
    // A .pixel dir with no usable index and no git repo: the child search
    // fails, so the deny must fall back to the suggestion-only message.
    let dir = scratch("fallback");
    std::fs::create_dir_all(dir.join(".pixel")).unwrap();
    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Grep",
        "cwd": dir.to_str().unwrap(),
        "tool_input": {"pattern": "ANYTHING_AT_ALL"},
    });
    let (code, stderr) = run_guard(&payload);
    assert_eq!(code, 2, "Grep must still be denied on fallback: {stderr}");
    assert!(
        stderr.contains("Run this via Bash: pixel search"),
        "fallback must carry the suggestion: {stderr}"
    );
    assert!(
        !stderr.contains("BLOCKED Grep —"),
        "fallback must not pretend to carry results: {stderr}"
    );
}

#[test]
fn targets_manifest_merges_two_tasks_and_guard_honors_union() {
    let repo = indexed_repo("two-tasks");
    // Add a second file so two distinct tasks rank different targets.
    std::fs::write(
        dir_join(&repo, "src/alpha_widget.rs"),
        "pub fn alpha_widget_render() { /* ALPHA_WIDGET_TOKEN */ }\n",
    )
    .unwrap();
    std::fs::write(
        dir_join(&repo, "src/beta_parser.rs"),
        "pub fn beta_parser_parse() { /* BETA_PARSER_TOKEN */ }\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "two modules"]);

    let run_targets = |task: &str| {
        let out = Command::new(PIXEL)
            .args(["targets", task, repo.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "targets '{task}' failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run_targets("alpha_widget_render alpha widget rendering ALPHA_WIDGET_TOKEN");
    run_targets("beta_parser_parse beta parser parsing BETA_PARSER_TOKEN");

    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".pixel").join("targets.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["version"], 2, "manifest must be v2: {manifest}");
    let tasks = manifest["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2, "both tasks must coexist: {manifest}");

    // Task A's file must be readable even though task B was written last —
    // the guard scopes to the UNION of active tasks.
    let alpha_listed = tasks[0]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["path"] == "src/alpha_widget.rs");
    assert!(alpha_listed, "task A must list its own file: {manifest}");
    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Read",
        "cwd": repo.to_str().unwrap(),
        "tool_input": {"file_path": repo.join("src/alpha_widget.rs").to_str().unwrap()},
    });
    let (code, stderr) = run_guard(&payload);
    assert_eq!(
        code, 0,
        "file listed in task A must stay allowed after task B's write: {stderr}"
    );
}

fn dir_join(base: &Path, rel: &str) -> PathBuf {
    base.join(rel)
}
