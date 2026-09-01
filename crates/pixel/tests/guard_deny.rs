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
    let (code, _stdout, stderr) = run_guard_env(payload, &[]);
    (code, stderr)
}

/// Like `run_guard` but with explicit env vars, and capturing stdout too
/// (advisories are JSON on stdout with exit 0). The guard's escape-hatch
/// vars are always cleared first so the ambient shell can't skew a test.
fn run_guard_env(payload: &serde_json::Value, envs: &[(&str, &str)]) -> (i32, String, String) {
    let mut cmd = Command::new(PIXEL);
    cmd.args(["hook", "guard"])
        .env_remove("PIXEL_GUARD_RAW_GIT")
        .env_remove("PIXEL_GUARD_RAW_TRANSCRIPTS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn bash_payload(cwd: &Path, command: &str) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "cwd": cwd.to_str().unwrap(),
        "tool_input": {"command": command},
    })
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

// --- SUBSTITUTE tier (end-to-end through the real binary) -----------------

#[test]
fn bash_git_commit_denied_with_substitute() {
    let repo = indexed_repo("sub-commit");
    let payload = bash_payload(&repo, "git commit -m 'fix parser'");
    let (code, stdout, stderr) = run_guard_env(&payload, &[]);
    assert_eq!(code, 2, "raw git commit must be denied: {stderr} {stdout}");
    assert!(stderr.contains("BLOCKED [PIXEL_SUBSTITUTE]"), "{stderr}");
    assert!(stderr.contains("pixel publish"), "{stderr}");
    assert!(
        stderr.contains("--message 'fix parser'"),
        "parsed -m must enrich the substitute: {stderr}"
    );
    assert!(
        !stderr.contains("PIXEL_GUARD_RAW_GIT=1"),
        "human-override env var must NOT be advertised: {stderr}"
    );
}

#[test]
fn bash_git_add_denied_with_substitute() {
    let repo = indexed_repo("sub-add");
    let payload = bash_payload(&repo, "git add src/a.rs src/b.rs");
    let (code, _stdout, stderr) = run_guard_env(&payload, &[]);
    assert_eq!(code, 2, "raw git add must be denied: {stderr}");
    assert!(stderr.contains("BLOCKED [PIXEL_SUBSTITUTE]"), "{stderr}");
    assert!(stderr.contains("pixel publish"), "{stderr}");
    assert!(
        stderr.contains("--files src/a.rs --files src/b.rs"),
        "each pathspec must be its own --files: {stderr}"
    );
    assert!(
        !stderr.contains("PIXEL_GUARD_RAW_GIT=1"),
        "human-override env var must NOT be advertised: {stderr}"
    );
}

#[test]
fn bash_git_add_dot_denied() {
    let repo = indexed_repo("sub-add-dot");
    let payload = bash_payload(&repo, "git add .");
    let (code, _stdout, stderr) = run_guard_env(&payload, &[]);
    assert_eq!(code, 2, "raw git add . must be denied: {stderr}");
    assert!(stderr.contains("BLOCKED [PIXEL_SUBSTITUTE]"), "{stderr}");
    assert!(stderr.contains("pixel publish"), "{stderr}");
    assert!(
        stderr.contains("List each modified tracked file"),
        "`git add .` must suggest enumerating files: {stderr}"
    );
}

#[test]
fn bash_git_add_interactive_passes_through() {
    let repo = indexed_repo("sub-add-interactive");
    for cmd in ["git add -p", "git add --patch", "git add -i", "git add --interactive"] {
        let payload = bash_payload(&repo, cmd);
        let (code, _stdout, stderr) = run_guard_env(&payload, &[]);
        assert_eq!(
            code, 0,
            "`{cmd}` must pass through (interactive hunk staging): {stderr}"
        );
        assert!(
            !stderr.contains("BLOCKED"),
            "`{cmd}` must not be denied: {stderr}"
        );
    }
}

#[test]
fn bash_sequencer_state_passes_add_commit_and_side_selection() {
    // End-to-end sequencer pass-through: with MERGE_HEAD present, the full
    // merge-conclusion workflow — stage, select a side, commit — must run.
    // `pixel publish` cannot substitute mid-sequencer (a merge commit needs
    // both parents; a plain publish would corrupt the graph).
    let repo = indexed_repo("sequencer-merge");
    std::fs::write(repo.join(".git").join("MERGE_HEAD"), b"abc123\n").unwrap();
    for cmd in [
        "git add src/lib.rs",
        "git commit -m 'resolve merge'",
        "git checkout --theirs -- src/lib.rs",
        "git checkout --ours -- src/lib.rs",
        "git merge --continue",
    ] {
        let payload = bash_payload(&repo, cmd);
        let (code, _stdout, stderr) = run_guard_env(&payload, &[]);
        assert_eq!(code, 0, "`{cmd}` must pass during an active merge: {stderr}");
        assert!(!stderr.contains("BLOCKED"), "`{cmd}` must not be denied: {stderr}");
    }
}

#[test]
fn bash_publish_message_mentioning_git_add_not_denied() {
    // Regression: the guard's segment splitter used to cut through quoted
    // strings, so a multi-line commit message describing a git command
    // denied pixel's own substitute command.
    let repo = indexed_repo("quoted-message");
    let payload = bash_payload(
        &repo,
        "pixel publish --files a.rs --message \"fix(guard): pass git add through\nraw git commit stays denied\" --request-id x .",
    );
    let (code, _stdout, stderr) = run_guard_env(&payload, &[]);
    assert_eq!(code, 0, "quoted message must not trigger a deny: {stderr}");
    assert!(!stderr.contains("BLOCKED"), "{stderr}");
}

#[test]
fn escape_hatch_downgrades_commit_to_advisory() {
    let repo = indexed_repo("sub-escape");
    let payload = bash_payload(&repo, "git commit -m 'fix parser'");
    let (code, stdout, stderr) =
        run_guard_env(&payload, &[("PIXEL_GUARD_RAW_GIT", "1")]);
    assert_eq!(code, 0, "escape hatch must allow the command: {stderr}");
    assert!(
        stdout.contains("pixel publish"),
        "advisory must still carry the substitute: {stdout}"
    );
    assert!(
        stdout.contains("advisory") && !stdout.contains("BLOCKED"),
        "must be advisory wording, not a deny: {stdout}"
    );
}

#[test]
fn escape_hatch_does_not_touch_destructive_tier() {
    let repo = indexed_repo("sub-escape-destructive");
    let payload = bash_payload(&repo, "git reset --hard HEAD~1");
    let (code, _stdout, stderr) =
        run_guard_env(&payload, &[("PIXEL_GUARD_RAW_GIT", "1")]);
    assert_eq!(
        code, 2,
        "PIXEL_GUARD_RAW_GIT must downgrade ONLY the substitute tier: {stderr}"
    );
}

// --- transcript escalation (end-to-end through the real binary) ------------

const TRANSCRIPT_POKE: &str =
    "sqlite3 ~/.local/share/devin/cli/sessions.db 'select title from sessions'";

#[test]
fn transcript_poke_denied_when_recall_index_exists() {
    let dir = scratch("recall-ready");
    let recall_dir = dir.join("recall");
    std::fs::create_dir_all(&recall_dir).unwrap();
    std::fs::write(recall_dir.join("recall.db"), b"").unwrap();
    let payload = bash_payload(&dir, TRANSCRIPT_POKE);
    let (code, _stdout, stderr) = run_guard_env(
        &payload,
        &[("PIXEL_RECALL_DIR", recall_dir.to_str().unwrap())],
    );
    assert_eq!(code, 2, "poke must be denied when the index exists: {stderr}");
    assert!(stderr.contains("BLOCKED [PIXEL_SUBSTITUTE]"), "{stderr}");
    assert!(stderr.contains("pixel recall sessions --agent devin"), "{stderr}");
    assert!(
        !stderr.contains("PIXEL_GUARD_RAW_TRANSCRIPTS=1"),
        "human-override env var must NOT be advertised: {stderr}"
    );

    // The dedicated escape hatch downgrades it back to the advisory.
    let (code, stdout, _stderr) = run_guard_env(
        &payload,
        &[
            ("PIXEL_RECALL_DIR", recall_dir.to_str().unwrap()),
            ("PIXEL_GUARD_RAW_TRANSCRIPTS", "1"),
        ],
    );
    assert_eq!(code, 0, "PIXEL_GUARD_RAW_TRANSCRIPTS=1 must downgrade: {stdout}");
    assert!(stdout.contains("Advisory"), "{stdout}");
}

#[test]
fn transcript_poke_advisory_when_no_recall_index() {
    let dir = scratch("recall-missing");
    let empty = dir.join("empty-recall");
    std::fs::create_dir_all(&empty).unwrap();
    let payload = bash_payload(&dir, TRANSCRIPT_POKE);
    let (code, stdout, stderr) = run_guard_env(
        &payload,
        &[("PIXEL_RECALL_DIR", empty.to_str().unwrap())],
    );
    assert_eq!(
        code, 0,
        "without a recall index there is no substitute — advisory only: {stderr}"
    );
    assert!(stdout.contains("Advisory"), "{stdout}");
    assert!(stdout.contains("pixel recall"), "{stdout}");
    assert!(!stdout.contains("BLOCKED"), "{stdout}");
}

#[test]
fn zcode_poke_flagged_via_marker() {
    let dir = scratch("zcode-marker");
    let empty = dir.join("empty-recall");
    std::fs::create_dir_all(&empty).unwrap();
    let payload = bash_payload(&dir, "sqlite3 ~/.zcode/cli/db/db.sqlite '.tables'");
    let (code, stdout, _stderr) = run_guard_env(
        &payload,
        &[("PIXEL_RECALL_DIR", empty.to_str().unwrap())],
    );
    assert_eq!(code, 0, "no index → advisory: {stdout}");
    assert!(
        stdout.contains(".zcode/cli/db") && stdout.contains("pixel recall"),
        "zcode store must be recognized: {stdout}"
    );
}
