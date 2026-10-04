// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Integration: `pixel run-hook guard` rewrites equivalent searches and emits
//! non-blocking guidance for operations that Pixel cannot safely rewrite.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::support::{Scratch, pixel_command};

/// Unique scratch dir per test.
fn scratch(tag: &str) -> Scratch {
    Scratch::for_test("pixel-guard-deny", tag)
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
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

/// The guard keeps a search native when the user configured that tool
/// (`RIPGREP_CONFIG_PATH`, `GREP_OPTIONS`). A developer shell exporting one
/// of them must not change what these tests observe: every hook invocation
/// starts from a clean search-tool environment.
fn hermetic_search_env(cmd: &mut Command) {
    cmd.env_remove("RIPGREP_CONFIG_PATH")
        .env_remove("GREP_OPTIONS");
}

/// A committed git repo containing a needle, with the pixel text index
/// built (first `pixel search-content` builds it lazily).
fn indexed_repo(tag: &str) -> Scratch {
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
    // PIXEL_TEST=1 disables the call-guard circuit breaker so repeated
    // search calls in tests don't get blocked.
    let out = pixel_command()
        .args(["search-content", "GUARD_NEEDLE_XYZ", dir.to_str().unwrap()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("PIXEL_TEST", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "seed search failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir
}

/// Pipe a PreToolUse payload into `pixel run-hook guard`; return (code, stderr).
fn run_guard(payload: &serde_json::Value) -> (i32, String) {
    let (code, _stdout, stderr) = run_guard_env(payload, &[]);
    (code, stderr)
}

/// Like `run_guard` but with explicit env vars, and capturing stdout too
/// (advisories are JSON on stdout with exit 0). The guard's escape-hatch
/// vars are always cleared first so the ambient shell can't skew a test.
fn run_guard_env(payload: &serde_json::Value, envs: &[(&str, &str)]) -> (i32, String, String) {
    let mut cmd = pixel_command();
    hermetic_search_env(&mut cmd);
    cmd.args(["run-hook", "guard"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("PIXEL_TEST", "1")
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

fn run_composed_codex(raw: &[u8], backup: &Path) -> (i32, String, String) {
    let mut cmd = pixel_command();
    hermetic_search_env(&mut cmd);
    cmd.args([
        "run-hook",
        "composed-guard",
        "--provider",
        "codex",
        "--backup",
        backup.to_str().unwrap(),
    ])
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("GIT_CONFIG_SYSTEM", "/dev/null")
    .env("PIXEL_TEST", "1")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child.stdin.as_mut().unwrap().write_all(raw).unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn composed_backup(dir: &Path, command: &str) -> PathBuf {
    let path = dir.join("codex-foreign-pretooluse.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "version": 1,
            "provider": "codex",
            "pre_tool_use": [{
                "matcher": "shell",
                "hooks": [{"type": "command", "command": command}]
            }],
            "managed_pre_tool_use": []
        })
        .to_string(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

#[test]
fn composed_codex_merges_context_and_rewrites_after_exact_stdin_replay() {
    let repo = indexed_repo("composed-context");
    let expected = repo.join("expected.json");
    let script = repo.join("foreign-context.sh");
    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "shell",
        "cwd": repo.to_str().unwrap(),
        "tool_input": {"command": "grep -n GUARD_NEEDLE_XYZ src/lib.rs"},
    });
    let raw = format!("  {payload}\n");
    std::fs::write(&expected, &raw).unwrap();
    std::fs::write(
        &script,
        format!(
            "cmp -s {} /dev/stdin || exit 9\nprintf '%s' '{{\"hookSpecificOutput\":{{\"hookEventName\":\"PreToolUse\",\"additionalContext\":\"foreign context\"}}}}'\n",
            expected.display()
        ),
    )
    .unwrap();
    let backup = composed_backup(&repo, "/bin/sh \"$PWD/foreign-context.sh\"");
    let (code, stdout, stderr) = run_composed_codex(raw.as_bytes(), &backup);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("pixel search-like-rg"), "{stdout}");
    assert!(stdout.contains("foreign context"), "{stdout}");
}

#[test]
fn composed_codex_preserves_foreign_denial_without_pixel_rewrite() {
    let repo = indexed_repo("composed-deny");
    let backup = composed_backup(
        &repo,
        "printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"foreign guard\"}}'",
    );
    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse", "tool_name": "shell", "cwd": repo.to_str().unwrap(),
        "tool_input": {"command": "grep -n GUARD_NEEDLE_XYZ src/lib.rs"}
    });
    let (code, stdout, stderr) = run_composed_codex(payload.to_string().as_bytes(), &backup);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("foreign guard"), "{stdout}");
    assert!(!stdout.contains("updatedInput"), "{stdout}");
}

#[test]
fn composed_codex_malformed_foreign_response_fails_open_without_rewrite() {
    let repo = indexed_repo("composed-malformed");
    let backup = composed_backup(&repo, "printf '%s' not-json");
    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse", "tool_name": "shell", "cwd": repo.to_str().unwrap(),
        "tool_input": {"command": "grep -n GUARD_NEEDLE_XYZ src/lib.rs"}
    });
    let (code, stdout, stderr) = run_composed_codex(payload.to_string().as_bytes(), &backup);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.is_empty(),
        "malformed foreign response must not rewrite: {stdout}"
    );
}

#[cfg(unix)]
#[test]
fn composed_codex_refuses_a_non_private_backup() {
    use std::os::unix::fs::PermissionsExt;

    let repo = indexed_repo("composed-permissions");
    let backup = composed_backup(&repo, "printf '%s' '{}'");
    std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o644)).unwrap();
    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse", "tool_name": "shell", "cwd": repo.to_str().unwrap(),
        "tool_input": {"command": "grep -n GUARD_NEEDLE_XYZ src/lib.rs"}
    });
    let (code, stdout, stderr) = run_composed_codex(payload.to_string().as_bytes(), &backup);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.is_empty(),
        "non-private backup must not rewrite: {stdout}"
    );
}

fn bash_payload(cwd: &Path, command: &str) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "cwd": cwd.to_str().unwrap(),
        "tool_input": {"command": command},
    })
}

/// Equivalent Bash searches are transparently rewritten to Pixel search.
#[test]
fn bash_literal_file_search_uses_compatibility_rewrite() {
    let repo = indexed_repo("rtk-race");
    for cmd in [
        "grep -n GUARD_NEEDLE_XYZ src/lib.rs",
        "rg GUARD_NEEDLE_XYZ src/lib.rs",
    ] {
        let (code, stdout, stderr) = run_guard_env(&bash_payload(&repo, cmd), &[]);
        assert_eq!(code, 0, "`{cmd}` must remain available: {stderr}");
        assert!(
            stdout.contains("updatedInput") && stdout.contains("pixel search-like-rg"),
            "{stdout}"
        );
    }
}

/// A pipeline must remain native: enriched search output is not a compatible
/// input to downstream filters, counts or control flow.
#[test]
fn bash_grep_pipeline_preserves_original_execution() {
    let repo = indexed_repo("rtk-race-pipe");
    for cmd in [
        "grep -rln GUARD_NEEDLE_XYZ . | sort",
        "bash -lc 'grep -n GUARD_NEEDLE_XYZ src/lib.rs'",
    ] {
        let (code, stdout, stderr) = run_guard_env(&bash_payload(&repo, cmd), &[]);
        assert_eq!(code, 0, "compound command must remain native: {stderr}");
        assert!(
            !stdout.contains("updatedInput"),
            "pipeline/wrapper must not receive a non-equivalent rewrite: {stdout}"
        );
    }
}

/// `~/.config/<devin>/` is a CONFIG directory (config.json, mcp_config.json,
/// skills/) — the very files `pixel install` writes. It was listed as a
/// transcript store, so the guard denied reads of pixel's own installed hook
/// config and pointed the agent at `pixel recall`, which cannot answer a
/// config question. The real store is `.local/share/<devin>/cli/...`.
#[test]
fn devin_config_dir_is_not_treated_as_a_transcript_store() {
    // Same setup as `transcript_poke_denied_when_recall_index_exists`: a
    // recall index exists, so a genuine transcript poke MUST be denied.
    // Anything not denied here is therefore not classified as a store.
    let dir = scratch("devin-config-not-store");
    let recall_dir = dir.join("recall");
    std::fs::create_dir_all(&recall_dir).unwrap();
    std::fs::write(recall_dir.join("recall.db"), b"").unwrap();
    let env = [("PIXEL_RECALL_DIR", recall_dir.to_str().unwrap())];

    // Control: the real store path is still identified, but remains allowed.
    let (code, stdout, stderr) = run_guard_env(&bash_payload(&dir, TRANSCRIPT_POKE), &env);
    assert_eq!(
        code, 0,
        "real transcript store must remain available: {stderr}"
    );
    assert!(
        stdout.contains("pixel recall"),
        "advisory should name the substitute: {stdout}"
    );

    // Regression: the CONFIG dir must not be treated as a store.
    let cfg_poke = "sqlite3 ~/.config/devin/config.json .tables";
    let (code, stdout, stderr) = run_guard_env(&bash_payload(&dir, cfg_poke), &env);
    assert_ne!(
        code, 2,
        "devin CONFIG dir is not a transcript store — must not be denied: {stderr}"
    );
    assert!(
        !stdout.contains("pixel recall") && !stderr.contains("pixel recall"),
        "must not point at pixel recall for a config file: {stdout}{stderr}"
    );
}

/// These noncanonical historical shell shapes are not current Codex hook
/// events (which use Bash/command string). They remain native, not guessed
/// into a different executable or authorized by the compatibility router.
#[test]
fn noncanonical_shell_shapes_preserve_native_execution() {
    let repo = indexed_repo("codex_shell");
    for tool in ["shell", "unified_exec", "local_shell"] {
        // Unfamiliar argv-array form.
        let payload = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": tool,
            "cwd": repo.to_str().unwrap(),
            "tool_input": {"command": ["bash", "-lc", "grep -rn GUARD_NEEDLE_XYZ ."]},
        });
        let (code, stdout, stderr) = run_guard_env(&payload, &[]);
        assert_eq!(code, 0, "{tool}: {stderr}");
        assert!(!stdout.contains("updatedInput"), "{tool}: {stdout}");
        assert!(!stdout.contains("permissionDecision"), "{tool}: {stdout}");

        // A string payload does not make an unsupported search equivalent
        // (`-l` files-only has no pixel replacement and must pass through).
        let payload = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": tool,
            "cwd": repo.to_str().unwrap(),
            "tool_input": {"command": "grep -rl GUARD_NEEDLE_XYZ ."},
        });
        let (code, stdout, stderr) = run_guard_env(&payload, &[]);
        assert_eq!(code, 0, "{tool}: {stderr}");
        assert!(!stdout.contains("updatedInput"), "{tool}: {stdout}");
        assert!(!stdout.contains("permissionDecision"), "{tool}: {stdout}");
    }
}

#[test]
fn grep_tool_remains_available_with_search_advisory() {
    let repo = indexed_repo("inline");
    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Grep",
        "cwd": repo.to_str().unwrap(),
        "tool_input": {"pattern": "GUARD_NEEDLE_XYZ"},
    });
    let (code, stdout, stderr) = run_guard_env(&payload, &[]);
    assert_eq!(
        code, 0,
        "Grep in an indexed repo must remain available: {stderr}"
    );
    assert!(
        stdout.contains("pixel-guard advisory"),
        "advisory header missing: {stdout}"
    );
    assert!(
        stdout.contains("GUARD_NEEDLE_XYZ") && stdout.contains("pixel search-content"),
        "advisory should include the equivalent search: {stdout}"
    );
}

#[test]
fn grep_advisory_falls_back_when_search_cannot_answer() {
    // A .pixel dir with no usable index and no git repo: the original Grep
    // call remains available with a suggestion-only advisory.
    let dir = scratch("fallback");
    std::fs::create_dir_all(dir.join(".pixel")).unwrap();
    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Grep",
        "cwd": dir.to_str().unwrap(),
        "tool_input": {"pattern": "ANYTHING_AT_ALL"},
    });
    let (code, stdout, stderr) = run_guard_env(&payload, &[]);
    assert_eq!(code, 0, "Grep must remain available on fallback: {stderr}");
    assert!(
        stdout.contains("pixel search-content"),
        "fallback must carry the suggestion: {stderr}"
    );
    assert!(
        !stdout.contains("BLOCKED"),
        "fallback must not pretend to deny the call: {stdout}"
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
        let out = pixel_command()
            .args(["scope-task", task, repo.to_str().unwrap()])
            .env("PIXEL_TEST", "1")
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
fn bash_git_commit_gets_substitute_advisory() {
    let repo = indexed_repo("sub-commit");
    let payload = bash_payload(&repo, "git commit -m 'fix parser'");
    let (code, stdout, stderr) = run_guard_env(&payload, &[]);
    assert_eq!(
        code, 0,
        "raw git commit must remain available: {stderr} {stdout}"
    );
    assert!(
        stdout.contains("pixel-guard advisory [PIXEL_SUBSTITUTE]"),
        "{stdout}"
    );
    assert!(stdout.contains("pixel commit"), "{stdout}");
    assert!(
        stdout.contains("--message 'fix parser'"),
        "parsed -m must enrich the substitute: {stdout}"
    );
    assert!(
        !stdout.contains("PIXEL_GUARD_RAW_GIT=1"),
        "human-override env var must NOT be advertised: {stdout}"
    );
}

#[test]
fn bash_git_add_gets_substitute_advisory() {
    let repo = indexed_repo("sub-add");
    let payload = bash_payload(&repo, "git add src/a.rs src/b.rs");
    let (code, stdout, stderr) = run_guard_env(&payload, &[]);
    assert_eq!(code, 0, "raw git add must remain available: {stderr}");
    assert!(
        stdout.contains("pixel-guard advisory [PIXEL_SUBSTITUTE]"),
        "{stdout}"
    );
    assert!(stdout.contains("pixel commit"), "{stdout}");
    assert!(
        stdout.contains("--files src/a.rs --files src/b.rs"),
        "each pathspec must be its own --files: {stdout}"
    );
    assert!(
        !stdout.contains("PIXEL_GUARD_RAW_GIT=1"),
        "human-override env var must NOT be advertised: {stdout}"
    );
}

#[test]
fn bash_git_add_dot_gets_substitute_advisory() {
    let repo = indexed_repo("sub-add-dot");
    let payload = bash_payload(&repo, "git add .");
    let (code, stdout, stderr) = run_guard_env(&payload, &[]);
    assert_eq!(code, 0, "raw git add . must remain available: {stderr}");
    assert!(
        stdout.contains("pixel-guard advisory [PIXEL_SUBSTITUTE]"),
        "{stdout}"
    );
    assert!(stdout.contains("pixel commit"), "{stdout}");
    assert!(
        stdout.contains("List each modified tracked file"),
        "`git add .` must suggest enumerating files: {stdout}"
    );
}

#[test]
fn bash_git_add_interactive_passes_through() {
    let repo = indexed_repo("sub-add-interactive");
    for cmd in [
        "git add -p",
        "git add --patch",
        "git add -i",
        "git add --interactive",
    ] {
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
    // `pixel commit` cannot substitute mid-sequencer (a merge commit needs
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
        assert_eq!(
            code, 0,
            "`{cmd}` must pass during an active merge: {stderr}"
        );
        assert!(
            !stderr.contains("BLOCKED"),
            "`{cmd}` must not be denied: {stderr}"
        );
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
    let (code, stdout, stderr) = run_guard_env(&payload, &[("PIXEL_GUARD_RAW_GIT", "1")]);
    assert_eq!(code, 0, "escape hatch must allow the command: {stderr}");
    assert!(
        stdout.contains("pixel commit"),
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
    let (code, stdout, stderr) = run_guard_env(&payload, &[("PIXEL_GUARD_RAW_GIT", "1")]);
    assert_eq!(code, 0, "destructive commands remain available: {stderr}");
    assert!(stdout.contains("pixel-guard advisory"), "{stdout}");
}

// --- transcript escalation (end-to-end through the real binary) ------------

const TRANSCRIPT_POKE: &str =
    "sqlite3 ~/.local/share/devin/cli/sessions.db 'select title from sessions'";

#[test]
fn transcript_poke_gets_advisory_when_recall_index_exists() {
    let dir = scratch("recall-ready");
    let recall_dir = dir.join("recall");
    std::fs::create_dir_all(&recall_dir).unwrap();
    std::fs::write(recall_dir.join("recall.db"), b"").unwrap();
    let payload = bash_payload(&dir, TRANSCRIPT_POKE);
    let (code, stdout, stderr) = run_guard_env(
        &payload,
        &[("PIXEL_RECALL_DIR", recall_dir.to_str().unwrap())],
    );
    assert_eq!(
        code, 0,
        "poke must remain available when the index exists: {stderr}"
    );
    assert!(stdout.contains("pixel recall sessions --agent"), "{stdout}");
    assert!(
        !stdout.contains("PIXEL_GUARD_RAW_TRANSCRIPTS=1"),
        "human-override env var must NOT be advertised: {stdout}"
    );

    // The dedicated escape hatch downgrades it back to the advisory.
    let (code, stdout, _stderr) = run_guard_env(
        &payload,
        &[
            ("PIXEL_RECALL_DIR", recall_dir.to_str().unwrap()),
            ("PIXEL_GUARD_RAW_TRANSCRIPTS", "1"),
        ],
    );
    assert_eq!(
        code, 0,
        "PIXEL_GUARD_RAW_TRANSCRIPTS=1 must downgrade: {stdout}"
    );
    assert!(stdout.contains("Advisory"), "{stdout}");
}

#[test]
fn transcript_poke_advisory_when_no_recall_index() {
    let dir = scratch("recall-missing");
    let empty = dir.join("empty-recall");
    std::fs::create_dir_all(&empty).unwrap();
    let payload = bash_payload(&dir, TRANSCRIPT_POKE);
    let (code, stdout, stderr) =
        run_guard_env(&payload, &[("PIXEL_RECALL_DIR", empty.to_str().unwrap())]);
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
    let (code, stdout, _stderr) =
        run_guard_env(&payload, &[("PIXEL_RECALL_DIR", empty.to_str().unwrap())]);
    assert_eq!(code, 0, "no index → advisory: {stdout}");
    assert!(
        stdout.contains(".zcode/cli/db") && stdout.contains("pixel recall"),
        "zcode store must be recognized: {stdout}"
    );
}
