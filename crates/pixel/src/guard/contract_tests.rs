// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the guard's pure helpers: the bash bypass advisories,
//! the shell-wrapper unwrapping, the provider payload adapters, the foreign
//! hook composition rules, the git tokenizers and the manifest scoping
//! verdicts. Each test names the contract it holds, so a change that weakens
//! a refusal or widens a grant fails with the reason it matters.

use super::*;

/// A unique scratch directory, canonicalized so path comparisons hold on
/// macOS where the temp dir is a symlink.
fn scratch(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "pixel-guard-contract-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    canonical(&dir)
}

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(ToString::to_string).collect()
}

/// A repository with a source directory, a source file, prose and data, so
/// each reader advisory can be told apart from a legitimate read.
fn bypass_repo() -> PathBuf {
    let root = scratch("bypass");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn a() {}\n").unwrap();
    std::fs::write(root.join("README.md"), "# readme\n").unwrap();
    std::fs::write(root.join("notes.txt"), "notes\n").unwrap();
    root
}

/// Every one-liner search tool the guard knows gets the advisory naming that
/// tool, with the Pixel alternative pointed at the indexed root: the agent
/// learns which command to run instead, in the repository it is working in.
#[test]
fn bypass_advisory_should_name_tool_and_root_when_a_search_tool_reads_the_repo() {
    let root = bypass_repo();
    let cases = [
        (
            "sed -n /fn/p src/lib.rs",
            "BLOCKED by pixel-guard: sed used as a search tool — use pixel search-content instead.",
        ),
        (
            "awk '/fn/' src/lib.rs",
            "BLOCKED by pixel-guard: awk used as a search tool — use pixel search-content instead.",
        ),
        (
            "perl -ne 'print if /fn/' src/lib.rs",
            "BLOCKED by pixel-guard: perl used as a search tool — use pixel search-content instead.",
        ),
        (
            "python3 -c 'print(1)' src/lib.rs",
            "BLOCKED by pixel-guard: python3 used as a search tool — use pixel search-content instead.",
        ),
        (
            "python -c 'print(1)' src/lib.rs",
            "BLOCKED by pixel-guard: python used as a search tool — use pixel search-content instead.",
        ),
        (
            "node -e 'x()' src/lib.rs",
            "BLOCKED by pixel-guard: node used as a search tool — use pixel search-content instead.",
        ),
        (
            "ruby -e 'x' src/lib.rs",
            "BLOCKED by pixel-guard: ruby used as a search tool — use pixel search-content instead.",
        ),
        (
            "lua -e 'x' src/lib.rs",
            "BLOCKED by pixel-guard: lua used as a search tool — use pixel search-content instead.",
        ),
        (
            "ag needle",
            "BLOCKED by pixel-guard: ag (silver searcher) used for code search — use pixel search-content instead.",
        ),
        (
            "ack needle",
            "BLOCKED by pixel-guard: ack used for code search — use pixel search-content instead.",
        ),
        (
            "egrep needle src",
            "BLOCKED by pixel-guard: egrep used for code search — use pixel search-content instead.",
        ),
        (
            "fgrep needle src",
            "BLOCKED by pixel-guard: fgrep used for code search — use pixel search-content instead.",
        ),
        (
            "find . -type f -exec grep -l needle {} +",
            "BLOCKED by pixel-guard: find -exec grep nests grep inside find — use pixel search-content directly.",
        ),
        (
            "xargs grep needle",
            "BLOCKED by pixel-guard: xargs grep pattern — use pixel search-content directly.",
        ),
    ];
    let suggestion = format!(
        "  pixel search-content '<pattern>' {} --context 5",
        root.display()
    );
    for (cmd, headline) in cases {
        let lines = bypass_advisory_lines(cmd, &root, &root)
            .unwrap_or_else(|| panic!("`{cmd}` is a search bypass and must get an advisory"));
        assert_eq!(lines[0], headline, "headline for `{cmd}`");
        assert_eq!(lines[1], suggestion, "suggestion for `{cmd}`");
        assert_eq!(lines.len(), 3, "advisory shape for `{cmd}`: {lines:?}");
    }
}

/// Absolute paths and the `rtk`, `command` and `builtin` wrappers reach the
/// same advisory as the bare tool: an agent cannot step around the guard by
/// spelling the binary differently.
#[test]
fn bypass_advisory_should_fire_when_the_tool_is_spelled_by_path_or_wrapper() {
    let root = bypass_repo();
    for (cmd, tool) in [
        ("/usr/bin/egrep needle", "egrep"),
        ("/usr/local/bin/ag needle", "ag"),
        ("command ack needle", "ack"),
        ("builtin fgrep needle", "fgrep"),
        ("rtk command ag needle", "ag"),
    ] {
        let lines = bypass_advisory_lines(cmd, &root, &root)
            .unwrap_or_else(|| panic!("`{cmd}` must be recognised as {tool}"));
        assert!(
            lines[0].starts_with(&format!("BLOCKED by pixel-guard: {tool} ")),
            "`{cmd}` must be advised as {tool}: {:?}",
            lines[0]
        );
    }
}

/// Commands that only look like a bypass stay silent: a wrapper with nothing
/// after it, an in-place `sed`, an `awk` program that is not a pattern, a
/// script run without an inline program, a tool given no argument, and a
/// `find`/`xargs` that runs no search. A false advisory teaches the agent to
/// ignore the real ones.
#[test]
fn bypass_advisory_should_stay_silent_when_the_command_only_looks_like_a_search() {
    let root = bypass_repo();
    for cmd in [
        "",
        "rtk",
        "command",
        "rtk command",
        "sed -i s/a/b/ src/lib.rs",
        "sed -n",
        "awk '{print $1}' src/lib.rs",
        "awk /fn/",
        "perl -v",
        "python3 -c 'print(1)'",
        "python -c 'print(1)'",
        "node -e 'x()'",
        "ruby -e 'x'",
        "lua -e 'x'",
        "perl '/fn/'",
        "python3 script.py one two",
        "python script.py one two",
        "node app.js one two",
        "ruby app.rb one two",
        "lua app.lua one two",
        "ag",
        "ack",
        "egrep",
        "fgrep",
        "find . -type f -exec wc -l {} +",
        "find . -type f",
        "xargs rm",
        "xargs",
        "rtk grep needle",
        "make build",
    ] {
        assert_eq!(
            bypass_advisory_lines(cmd, &root, &root),
            None,
            "`{cmd}` is not a search bypass"
        );
    }
}

/// `find -name` is file discovery: the advisory offers both the content
/// search and `pixel scope-task`, each pointed at the indexed root.
#[test]
fn find_name_advisory_should_offer_search_and_scope_task_when_discovering_files() {
    let root = bypass_repo();
    let lines = bypass_advisory_lines("find . -name '*.rs'", &root, &root).unwrap();
    assert_eq!(
        lines,
        vec![
            "BLOCKED by pixel-guard: find -name for file discovery — use pixel search-content or pixel scope-task.".to_string(),
            format!("  pixel search-content '<pattern>' {} --context 5  # for content search", root.display()),
            format!("  pixel scope-task \"<task>\" {}  # for file scoping", root.display()),
            "find -name patterns locate files by name; pixel search-content finds content, pixel scope-task scopes files.".to_string(),
        ]
    );
}

/// `ls`, `cat`, `head` and `tail` are advised only when they browse source:
/// a source directory or a source file that exists. Listing docs, reading
/// prose, a missing path or a flag-only call are ordinary shell use.
#[test]
fn reader_advisory_should_fire_only_when_existing_source_is_browsed() {
    let root = bypass_repo();
    let advised =
        |cmd: &str| bypass_advisory_lines(cmd, &root, &root).map(|lines| lines[0].clone());
    assert_eq!(
        advised("ls src").as_deref(),
        Some(
            "BLOCKED by pixel-guard: ls of source directory — use pixel search-content or pixel scope-task."
        )
    );
    assert_eq!(
        advised("cat src/lib.rs").as_deref(),
        Some(
            "BLOCKED by pixel-guard: cat of source file — use pixel find-code or pixel search-content."
        )
    );
    assert_eq!(
        advised("head -20 src/lib.rs").as_deref(),
        Some(
            "BLOCKED by pixel-guard: head of source file — use pixel search-content --context or Read."
        )
    );
    assert_eq!(
        advised("tail src/lib.rs").as_deref(),
        Some(
            "BLOCKED by pixel-guard: tail of source file — use pixel search-content --context or Read."
        )
    );
    for cmd in [
        "ls docs",
        "ls missing",
        "ls",
        "cat README.md",
        "cat missing.rs",
        "cat src/lib.rs README.md",
        "head notes.txt",
        "tail missing.rs",
        "head -20",
    ] {
        assert_eq!(advised(cmd), None, "`{cmd}` is not a source browse");
    }
    // The cat advisory names the definition jump, not just the search.
    let cat = bypass_advisory_lines("cat src/lib.rs", &root, &root).unwrap();
    assert_eq!(
        cat[1],
        format!(
            "  pixel find-code '<symbol>' {}  # jump to definition",
            root.display()
        )
    );
}

/// The legacy "BLOCKED" headline becomes an advisory that says the call
/// proceeds: the guard informs, it never forces a retry.
#[test]
fn non_blocking_advisory_should_say_the_call_proceeds_when_rewriting_a_block() {
    let lines = non_blocking_advisory_lines(&strings(&[
        "BLOCKED by pixel-guard: sed used as a search tool — BLOCKED twice.",
        "  pixel search-content x",
    ]));
    assert_eq!(
        lines,
        strings(&[
            "pixel-guard advisory by pixel-guard: sed used as a search tool — BLOCKED twice.",
            "  pixel search-content x",
            "Proceeding with the original command or tool call.",
        ])
    );
    assert_eq!(
        non_blocking_advisory_lines(&[]),
        strings(&["Proceeding with the original command or tool call."])
    );
}

/// `bash -lc '<script>'` and its spellings are unwrapped to the script, so a
/// `grep` hidden inside a shell wrapper is judged like a bare one; anything
/// that is not a `-c` invocation is left as typed.
#[test]
fn unwrap_shell_c_should_return_the_script_only_when_a_c_flag_wraps_it() {
    for (cmd, script) in [
        ("bash -lc \"grep -rn foo .\"", "grep -rn foo ."),
        ("/bin/zsh -c 'rg needle'", "rg needle"),
        ("sh -x -c 'cat a.rs'", "cat a.rs"),
        ("  fish -c ls  ", "ls"),
        ("bash script.sh -c x", "bash script.sh -c x"),
        ("bash -l", "bash -l"),
        ("python -c 'print(1)'", "python -c 'print(1)'"),
        ("", ""),
    ] {
        assert_eq!(unwrap_shell_c(cmd), script, "unwrap of `{cmd}`");
    }
}

/// A command arrives as a string or as an argv array (Codex `shell`). Both
/// forms give the script the shell would run; any other JSON type is no
/// command at all.
#[test]
fn command_text_should_give_the_script_when_sent_as_string_or_argv() {
    use serde_json::json;
    for (value, text) in [
        (json!("bash -lc 'grep foo .'"), "grep foo ."),
        (json!(["bash", "-lc", "grep -rn foo ."]), "grep -rn foo ."),
        (json!(["/bin/sh", "-c", "sh -c 'rg x'"]), "rg x"),
        (json!(["rg", "needle", "src"]), "rg needle src"),
        (json!(["bash", "run.sh"]), "bash run.sh"),
        (json!([]), ""),
        (json!([1, 2]), ""),
        (json!(42), ""),
        (Value::Null, ""),
    ] {
        assert_eq!(command_text(&value), text, "command text of {value}");
    }
}

/// A rewrite keeps the provider's representation: a string stays a string,
/// a shell argv keeps every token but the script, and an argv the guard
/// cannot place a script into is left alone rather than re-quoted.
#[test]
fn rewritten_command_value_should_replace_only_the_script_when_a_shell_wraps_it() {
    use serde_json::json;
    assert_eq!(
        rewritten_command_value(&json!("grep x"), "pixel search-content x".into()),
        Some(json!("pixel search-content x"))
    );
    assert_eq!(
        rewritten_command_value(&json!(["bash", "-lc", "grep x", "$0"]), "pixel y".into()),
        Some(json!(["bash", "-lc", "pixel y", "$0"]))
    );
    for original in [
        json!(["rg", "x"]),
        json!(["bash", "-lc"]),
        json!(["bash", "-x"]),
        json!(["bash", 1, "x"]),
        json!([]),
        json!(7),
        Value::Null,
    ] {
        assert_eq!(
            rewritten_command_value(&original, "pixel y".into()),
            None,
            "{original} has no script slot to rewrite"
        );
    }
}

/// Zcode needs an explicit allow beside `updatedInput`; Claude (its RTK
/// delegate), Devin and OpenCode take the bare rewrite. An allow sent to the
/// others would grant more than the rewrite asked for. Codex retrieval is
/// never rewritten.
#[test]
fn rewrite_json_should_grant_allow_only_when_the_host_requires_it() {
    use serde_json::json;
    let input = json!({"command": "pixel search-content x ."});
    assert_eq!(
        rewrite_json(Provider::Zcode, input.clone()),
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "updatedInput": input,
            "permissionDecision": "allow",
            "permissionDecisionReason": "Pixel compatibility routing: single-file literal read only.",
        }})
    );
    for provider in [Provider::Claude, Provider::Devin, Provider::Opencode] {
        assert_eq!(
            rewrite_json(provider, input.clone()),
            json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "updatedInput": input,
            }}),
            "{provider:?}"
        );
    }
}

/// Copilot's camelCase payload is mapped to the shared snake_case fields,
/// with `toolArgs` accepted as a JSON string or an object; fields the host
/// already sent in snake_case win.
#[test]
fn copilot_payload_should_be_normalized_when_fields_are_camel_case() {
    use serde_json::json;
    let string_args = provider_payload(
        Provider::Copilot,
        json!({"toolName": "bash", "toolArgs": "{\"command\":\"grep x\"}"}),
    );
    assert_eq!(string_args["tool_name"], json!("bash"));
    assert_eq!(string_args["tool_input"], json!({"command": "grep x"}));

    let object_args = provider_payload(
        Provider::Copilot,
        json!({"toolName": "view", "toolArgs": {"path": "a.rs"}}),
    );
    assert_eq!(object_args["tool_input"], json!({"path": "a.rs"}));

    let raw_string = provider_payload(
        Provider::Copilot,
        json!({"toolName": "bash", "toolArgs": "not json"}),
    );
    assert_eq!(raw_string["tool_input"], json!("not json"));

    let kept = provider_payload(
        Provider::Copilot,
        json!({"tool_name": "Read", "toolName": "bash", "tool_input": {"a": 1}, "toolArgs": "{}"}),
    );
    assert_eq!(kept["tool_name"], json!("Read"));
    assert_eq!(kept["tool_input"], json!({"a": 1}));
}

/// Antigravity's `toolCall` becomes the shared fields; its working
/// directory comes from the call's `Cwd`, else the first workspace path.
/// Other providers' payloads pass through untouched.
#[test]
fn antigravity_payload_should_lift_the_tool_call_when_one_is_present() {
    use serde_json::json;
    let with_cwd = provider_payload(
        Provider::Antigravity,
        json!({"toolCall": {"name": "run_command", "args": {"CommandLine": "ls", "Cwd": "/w/a"}},
               "workspacePaths": ["/w/b"]}),
    );
    assert_eq!(with_cwd["tool_name"], json!("run_command"));
    assert_eq!(
        with_cwd["tool_input"],
        json!({"CommandLine": "ls", "Cwd": "/w/a"})
    );
    assert_eq!(with_cwd["cwd"], json!("/w/a"));

    let from_workspace = provider_payload(
        Provider::Antigravity,
        json!({"toolCall": {"name": "view_file", "args": {"AbsolutePath": "/w/b/x"}},
               "workspacePaths": ["/w/b", "/w/c"]}),
    );
    assert_eq!(from_workspace["cwd"], json!("/w/b"));

    let bare = provider_payload(Provider::Antigravity, json!({"toolCall": {}}));
    assert_eq!(bare["tool_name"], Value::Null);
    assert_eq!(bare["tool_input"], Value::Null);
    assert_eq!(bare["cwd"], Value::Null);

    let untouched = json!({"toolCall": {"name": "x"}, "tool_name": "Bash"});
    assert_eq!(
        provider_payload(Provider::Codex, untouched.clone()),
        untouched
    );
    assert_eq!(
        provider_payload(Provider::Antigravity, json!({"tool_name": "Bash"})),
        json!({"tool_name": "Bash"})
    );
}

/// The working directory a call runs in: the payload `cwd`, else Cursor's
/// first workspace root, joined with the tool's own `workdir`/`cwd`/`Cwd`.
#[test]
fn provider_cwd_should_join_the_tool_directory_when_one_is_given() {
    use serde_json::json;
    let empty = json!({});
    assert_eq!(
        provider_cwd(&json!({"cwd": "/repo"}), &empty),
        Some(PathBuf::from("/repo"))
    );
    assert_eq!(
        provider_cwd(
            &json!({"cwd": "", "workspace_roots": ["/ws", "/other"]}),
            &empty
        ),
        Some(PathBuf::from("/ws"))
    );
    for (key, expected) in [
        ("workdir", "/repo/sub"),
        ("cwd", "/repo/sub"),
        ("Cwd", "/repo/sub"),
    ] {
        assert_eq!(
            provider_cwd(&json!({"cwd": "/repo"}), &json!({key: "sub"})),
            Some(PathBuf::from(expected)),
            "{key}"
        );
    }
    assert_eq!(
        provider_cwd(&json!({"cwd": "/repo"}), &json!({"workdir": "/abs"})),
        Some(PathBuf::from("/abs")),
        "an absolute tool directory replaces the host cwd"
    );
    assert_eq!(
        provider_cwd(
            &json!({"cwd": "/repo"}),
            &json!({"workdir": "a", "cwd": "b"})
        ),
        Some(PathBuf::from("/repo/a")),
        "workdir wins over cwd"
    );
}

/// OpenCode's camelCase `filePath` is aliased to `file_path` for the read
/// arms, never overwriting a `file_path` the host already sent.
#[test]
fn opencode_input_should_alias_file_path_when_none_was_sent() {
    use serde_json::json;
    assert_eq!(
        opencode_tool_input(json!({"tool_input": {"filePath": "a.rs"}})),
        json!({"tool_input": {"filePath": "a.rs", "file_path": "a.rs"}})
    );
    assert_eq!(
        opencode_tool_input(json!({"tool_input": {"filePath": "a.rs", "file_path": "b.rs"}})),
        json!({"tool_input": {"filePath": "a.rs", "file_path": "b.rs"}})
    );
    assert_eq!(
        opencode_tool_input(json!({"tool_input": "bash"})),
        json!({"tool_input": "bash"})
    );
    assert_eq!(opencode_tool_input(json!({})), json!({}));
}

/// A foreign hook's answer is kept only in shapes the composed guard can
/// merge: empty output, a Codex PreToolUse response, or a legacy `block`
/// turned into a deny. Anything else disables composition (None).
#[test]
fn foreign_hook_output_should_be_kept_only_when_mergeable() {
    use serde_json::json;
    assert_eq!(foreign_hook_output(Value::Null), Some(Value::Null));
    assert_eq!(
        foreign_hook_output(json!({"decision": "block", "reason": "policy says no"})),
        Some(foreign_deny_response("policy says no"))
    );
    assert_eq!(
        foreign_hook_output(json!({"decision": "block"})),
        Some(foreign_deny_response("A foreign hook blocked this call."))
    );
    let pre =
        json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "additionalContext": "x"}});
    assert_eq!(foreign_hook_output(pre.clone()), Some(pre));
    for unmergeable in [
        json!({"hookSpecificOutput": {"hookEventName": "PostToolUse"}}),
        json!({"hookSpecificOutput": "text"}),
        json!({"decision": "approve"}),
        json!({"anything": 1}),
    ] {
        assert_eq!(
            foreign_hook_output(unmergeable.clone()),
            None,
            "{unmergeable}"
        );
    }
    assert_eq!(
        foreign_deny_response("no"),
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": "no",
        }})
    );
}

/// A foreign answer that changes the input or decides the permission (in
/// either the nested or the flat spelling) is a mutation, so Pixel must not
/// layer its own rewrite on top; a denial is recognised in both spellings.
#[test]
fn foreign_mutation_should_be_detected_when_either_spelling_is_used() {
    use serde_json::json;
    for mutating in [
        json!({"hookSpecificOutput": {"updatedInput": {"command": "x"}}}),
        json!({"hookSpecificOutput": {"permissionDecision": "ask"}}),
        json!({"hookSpecificOutput": {}, "permissionDecision": "allow"}),
    ] {
        assert!(has_foreign_mutation(&mutating), "{mutating} mutates");
    }
    for observer in [
        json!({"hookSpecificOutput": {"additionalContext": "note"}}),
        json!({"permissionDecision": "deny"}),
        Value::Null,
    ] {
        assert!(!has_foreign_mutation(&observer), "{observer} only observes");
    }
    assert!(foreign_denial(
        &json!({"hookSpecificOutput": {"permissionDecision": "deny"}})
    ));
    assert!(foreign_denial(&json!({"permissionDecision": "deny"})));
    assert!(!foreign_denial(
        &json!({"hookSpecificOutput": {"permissionDecision": "allow"}})
    ));
    assert!(!foreign_denial(&json!({"permissionDecision": "ask"})));
    assert!(!foreign_denial(&Value::Null));
}

/// Foreign hooks' context notes are carried into the composed response,
/// before a note the response already holds; no notes leaves it untouched.
#[test]
fn compose_context_should_append_the_existing_note_when_foreign_notes_exist() {
    use serde_json::json;
    let response = json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "additionalContext": "pixel"}});
    assert_eq!(compose_context(response.clone(), &[]), response);
    assert_eq!(
        compose_context(response, &strings(&["first", "second"]))["hookSpecificOutput"]["additionalContext"],
        json!("first\nsecond\npixel")
    );
    let empty_note = json!({"hookSpecificOutput": {"additionalContext": ""}});
    assert_eq!(
        compose_context(empty_note, &strings(&["only"]))["hookSpecificOutput"]["additionalContext"],
        json!("only")
    );
    assert_eq!(
        compose_context(json!({}), &strings(&["fresh"])),
        json!({"hookSpecificOutput": {"additionalContext": "fresh"}})
    );
    assert_eq!(
        foreign_context(&json!({"hookSpecificOutput": {"additionalContext": 3}})),
        None
    );
}

/// Shell segments split on the separators a shell honours, never inside
/// quotes: `pixel search-content 'a|b'` stays one invocation.
#[test]
fn shell_segments_should_split_only_when_the_separator_is_unquoted() {
    assert_eq!(
        shell_segments("cd x && pixel search-content 'a|b' | head; echo \"x;y\"\nls"),
        vec![
            "cd x ",
            "",
            " pixel search-content 'a|b' ",
            " head",
            " echo \"x;y\"",
            "ls"
        ]
    );
    assert_eq!(shell_segments(""), vec![""]);
}

/// The pixel invocation is found behind env prefixes, launcher words, an
/// absolute path, the side build's name and a `bash -lc` wrapper; the
/// `PIXEL_METRICS=` value travels with it. A non-pixel command is none.
#[test]
fn pixel_invocation_should_be_found_when_behind_launchers_paths_or_wrappers() {
    let found = |cmd: &str| pixel_invocation(cmd);
    assert_eq!(
        found("sudo env PIXEL_METRICS=0 /opt/bin/pixel impact x"),
        Some(PixelInvocation {
            args: "impact x".into(),
            metrics_env: Some("0".into()),
        })
    );
    assert_eq!(
        found("time pixel-dev search-content y"),
        Some(PixelInvocation {
            args: "search-content y".into(),
            metrics_env: None,
        })
    );
    assert_eq!(
        found("PIXEL_METRICS=1 bash -lc 'pixel find-code z'"),
        Some(PixelInvocation {
            args: "find-code z".into(),
            metrics_env: Some("1".into()),
        })
    );
    for cmd in ["rg needle", "bash script.sh", "env FOO=1", "", "bash -l"] {
        assert_eq!(found(cmd), None, "`{cmd}` runs no pixel");
    }
}

fn invocation(sub: &str, args: &[&str], c_dir: Option<&str>) -> GitInvocation {
    GitInvocation {
        sub: sub.to_string(),
        args: strings(args),
        c_dir: c_dir.map(PathBuf::from),
    }
}

/// Every `git <sub>` of a compound command is found past the global flags
/// that take a value (`-C`, `-c`) and those that do not, and keeps the `-C`
/// directory it skipped; a bare `git` names no subcommand.
#[test]
fn git_invocations_should_skip_global_flags_when_finding_the_subcommand() {
    assert_eq!(
        git_invocations(
            "git -C /repo -c core.pager=cat --no-pager reset --hard; ls | git stash drop"
        ),
        vec![
            invocation("reset", &["--hard"], Some("/repo")),
            invocation("stash", &["drop"], None),
        ]
    );
    assert_eq!(
        git_invocations("git -c rebase.autosquash=false rebase main"),
        vec![invocation("rebase", &["main"], None)],
        "`-c` takes its value but names no directory"
    );
    assert_eq!(git_invocations("git"), vec![]);
    assert_eq!(git_invocations("git --no-pager"), vec![]);
    assert_eq!(git_invocations("git -C"), vec![]);
    assert_eq!(
        git_invocations("echo git status"),
        vec![invocation("status", &[], None)]
    );
    assert_eq!(git_invocations("ls -la"), vec![]);
}

/// Repeated `-C` flags compose in order the way git reads them: a relative
/// one goes under the previous, an absolute one starts over.
#[test]
fn git_invocations_should_compose_repeated_c_flags_like_git() {
    assert_eq!(
        git_invocations("git -C a -C b rebase main"),
        vec![invocation("rebase", &["main"], Some("a/b"))]
    );
    assert_eq!(
        git_invocations("git -C a -C /abs -c x=y -C c status"),
        vec![invocation("status", &[], Some("/abs/c"))]
    );
}

/// A short-flag cluster carries a letter only when it is a real cluster:
/// `--force` and `-` are not clusters, and a cluster with punctuation is not
/// read as flags.
#[test]
fn short_cluster_has_should_match_only_when_the_token_is_a_real_cluster() {
    for (token, c, expected) in [
        ("-fd", 'f', true),
        ("-Df", 'D', true),
        ("-f", 'f', true),
        ("-d", 'f', false),
        ("--force", 'f', false),
        ("-", 'f', false),
        ("-f=x", 'f', false),
        ("f", 'f', false),
    ] {
        assert_eq!(short_cluster_has(token, c), expected, "{token} has {c}");
    }
}

/// The destructive tier's remaining shapes: a forced branch delete in its
/// long spellings, a merge commit (denied, with `sync-branch` offered), the
/// merge-state exits that must stay open, and a reset to a branch that
/// suggests `checkout -B` with the current branch name.
#[test]
fn destructive_tier_should_deny_when_deleting_merging_or_resetting_to_a_branch() {
    let root = scratch("destructive");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/topic\n").unwrap();
    for cmd in [
        "git branch --delete --force feature",
        "git branch --delete -f feature",
        "git push -fu origin main",
    ] {
        assert!(
            bash_deny_lines(cmd, Some(&root)).is_some(),
            "`{cmd}` must be destructive-denied"
        );
    }
    for cmd in [
        "git branch --delete feature",
        "git merge --abort",
        "git merge --continue",
        "git merge --quit",
        "git status",
    ] {
        assert_eq!(
            bash_deny_lines(cmd, Some(&root)),
            None,
            "`{cmd}` must not be destructive-denied"
        );
    }
    let merge = bash_deny_lines("git merge feature", Some(&root)).unwrap();
    assert_eq!(
        merge[0],
        "BLOCKED by pixel-targets-guard: `git merge` creates a merge commit — forbidden without exception."
    );
    assert_eq!(
        merge[2],
        format!(
            "  pixel sync-branch {} --strategy rebase-if-clean",
            shell_quote(&root.display().to_string())
        )
    );
    let reset = bash_deny_lines("git reset --hard origin/main", Some(&root)).unwrap();
    assert_eq!(reset[2], "  git checkout -B topic origin/main");
    // No index root: the destructive tier has nothing to protect.
    assert_eq!(bash_deny_lines("git reset --hard HEAD~3", None), None);
    // No git in the command at all.
    assert_eq!(bash_deny_lines("rm -rf build", Some(&root)), None);

    let detached = scratch("destructive-detached");
    let reset = bash_deny_lines("git reset --hard origin/main", Some(&detached)).unwrap();
    assert_eq!(
        reset[2], "  git checkout -B <branch> origin/main",
        "an unreadable HEAD falls back to a placeholder, never a guess"
    );
}

/// Relative refs and OIDs of every accepted length are data loss when reset
/// to; names that only start like a ref, or are not all hex, are branches.
#[test]
fn is_branch_like_should_be_false_when_the_ref_is_relative_or_an_oid() {
    for (r, branch) in [
        ("HEAD@{2}", false),
        ("HEAD^2", false),
        ("abcdef0", false),
        ("0123456789abcdef0123456789abcdef01234567", false),
        (&"a".repeat(64), false),
        ("abcdef", true),
        ("feature/x", true),
        ("deadbeefz", true),
        ("HEADS", true),
        ("", false),
    ] {
        assert_eq!(is_branch_like(r), branch, "{r:?}");
    }
}

/// `cd <dir>` and `git -C <dir>` name the repository a command acts in only
/// when that directory exists; quotes are stripped and relative paths
/// resolve against the cwd.
#[test]
fn cd_and_git_c_targets_should_resolve_only_when_the_directory_exists() {
    let root = scratch("cd-target");
    std::fs::create_dir_all(root.join("sub dir")).unwrap();
    std::fs::create_dir_all(root.join("other")).unwrap();
    assert_eq!(
        extract_cd_target("cd 'sub dir' && git rebase main", &root),
        Some(root.join("sub dir"))
    );
    assert_eq!(
        extract_cd_target(
            &format!("cd {}; git status", root.join("other").display()),
            Path::new("/")
        ),
        Some(root.join("other"))
    );
    assert_eq!(extract_cd_target("cd missing && ls", &root), None);
    assert_eq!(extract_cd_target("cd  && ls", &root), None);
    assert_eq!(extract_cd_target("git status", &root), None);
    std::fs::write(root.join("file"), "x").unwrap();
    assert_eq!(extract_cd_target("cd file && ls", &root), None);

    assert_eq!(
        resolve_dir(Path::new("other"), &root),
        Some(root.join("other"))
    );
    assert_eq!(
        resolve_dir(&root.join("sub dir"), Path::new("/")),
        Some(root.join("sub dir"))
    );
    assert_eq!(
        resolve_dir(Path::new("other/../sub dir"), &root),
        Some(root.join("sub dir")),
        "the directory is canonicalized"
    );
    assert_eq!(resolve_dir(Path::new("missing"), &root), None);
    assert_eq!(resolve_dir(Path::new("file"), &root), None);
}

/// A conflict reported by `pixel sync-branch` is the one escape hatch for
/// raw `git rebase`; it exists exactly when its marker file does.
#[test]
fn reconcile_conflict_should_be_pending_only_when_the_marker_file_exists() {
    let root = scratch("reconcile");
    assert!(!reconcile_conflict_pending(&root));
    std::fs::create_dir_all(root.join(".pixel/reconcile-conflict.json")).unwrap();
    assert!(
        !reconcile_conflict_pending(&root),
        "a directory at the marker path is not a reported conflict"
    );
    std::fs::remove_dir(root.join(".pixel/reconcile-conflict.json")).unwrap();
    std::fs::write(root.join(".pixel/reconcile-conflict.json"), "{}").unwrap();
    assert!(reconcile_conflict_pending(&root));
}

/// The single file a reader command reads is found through `cd`, `rtk`, flags
/// and the sed/awk program; compound, looping, substituted or multi-file
/// commands name no single target.
#[test]
fn single_reader_target_should_name_the_file_only_when_one_file_is_read() {
    let root = bypass_repo();
    let lib = root.join("src/lib.rs");
    for cmd in [
        "cat src/lib.rs",
        "rtk cat src/lib.rs",
        "head -n5 src/lib.rs | wc -l",
        "sed -n 1,5p src/lib.rs",
        "awk 1 src/lib.rs",
        "cd src && cat lib.rs",
        "read src/lib.rs",
    ] {
        assert_eq!(
            single_reader_target(cmd, &root),
            Some(lib.clone()),
            "`{cmd}`"
        );
    }
    for cmd in [
        "cat $(echo src/lib.rs)",
        "cat `ls`",
        "cat <<EOF",
        "ls src | xargs cat",
        "for f in src/*; do cat $f; done",
        "while read l; do echo; done",
        "cat src/lib.rs README.md",
        "cat missing.rs",
        "cargo build",
        "cd src",
        "cd missing && cat lib.rs",
        "",
    ] {
        assert_eq!(single_reader_target(cmd, &root), None, "`{cmd}`");
    }
}

/// The quote-aware tokenizers keep quoted separators and spaces inside one
/// token and drop empty segments, so a quoted commit message can never open
/// a phantom command.
#[test]
fn tokenizers_should_keep_quoted_text_whole_when_it_holds_separators() {
    assert_eq!(
        simple_tokenize("  a 'b c' \"d'e\"  f"),
        strings(&["a", "b c", "d'e", "f"])
    );
    assert_eq!(simple_tokenize(""), Vec::<String>::new());
    assert_eq!(simple_tokenize("''"), Vec::<String>::new());
    assert_eq!(
        tokenize_segments("pixel commit --message 'fix; git add .' && git push\n\nls|  |wc"),
        vec![
            strings(&["pixel", "commit", "--message", "fix; git add ."]),
            strings(&["git", "push"]),
            strings(&["ls"]),
            strings(&["wc"]),
        ]
    );
    assert_eq!(tokenize_segments(";;&&"), Vec::<Vec<String>>::new());
}

/// Shell-safe text stays bare so roots read naturally; anything else is
/// single-quoted with embedded quotes escaped, and empty text is `''`.
#[test]
fn shell_quote_should_leave_text_bare_only_when_it_is_shell_safe() {
    for (raw, quoted) in [
        ("/repo/a_b-c.d:e=f+g@h~i", "/repo/a_b-c.d:e=f+g@h~i"),
        ("", "''"),
        ("a b", "'a b'"),
        ("it's", "'it'\\''s'"),
        ("$HOME", "'$HOME'"),
    ] {
        assert_eq!(shell_quote(raw), quoted, "{raw:?}");
    }
}

/// A grep becomes `pixel search-content` only when its output can be
/// reproduced: listing, counting, inverting, only-matching and max-count
/// flags refuse the rewrite; a quote in the pattern is escaped.
#[test]
fn search_can_replace_should_refuse_when_a_flag_changes_the_output() {
    assert_eq!(
        search_can_replace("it's", &strings(&["-A"]), "/repo"),
        Some("pixel search-content 'it'\\''s' /repo --context 5".into())
    );
    for flag in [
        "-l",
        "--files-with-matches",
        "-c",
        "--count",
        "-v",
        "--invert",
        "-o",
        "--only-matching",
        "-m",
        "--max-count",
    ] {
        assert_eq!(
            search_can_replace("x", &strings(&["-C", flag]), "/repo"),
            None,
            "{flag} changes the output"
        );
    }
}

/// `cd <dir> &&` moves the effective cwd and leaves the body; a `cd` without
/// an unquoted `&&`, or no `cd` at all, leaves both untouched.
#[test]
fn strip_cd_prefix_should_move_cwd_only_when_cd_is_chained_with_and() {
    let cwd = Path::new("/w");
    assert_eq!(
        strip_cd_prefix("cd /abs && rg x", cwd),
        (PathBuf::from("/abs"), "rg x")
    );
    assert_eq!(
        strip_cd_prefix("cd 'sub' &&   cat a", cwd),
        (PathBuf::from("/w/sub"), "cat a")
    );
    assert_eq!(
        strip_cd_prefix("cd sub; cat a", cwd),
        (PathBuf::from("/w"), "cd sub; cat a")
    );
    assert_eq!(strip_cd_prefix("rg x", cwd), (PathBuf::from("/w"), "rg x"));
}

/// A Read is bounded when it states a window of at most 200 lines, through a
/// `limit` or a start/end pair in either spelling; anything else is a
/// whole-file read.
#[test]
fn bounded_read_should_hold_only_when_a_window_of_at_most_200_lines_is_stated() {
    use serde_json::json;
    for (input, bounded) in [
        (json!({"limit": 200}), true),
        (json!({"limit": 1}), true),
        (json!({"limit": 0}), false),
        (json!({"limit": 201}), false),
        (json!({"StartLine": 10, "EndLine": 209}), true),
        (json!({"start_line": 10, "end_line": 210}), false),
        (json!({"start_line": 0, "end_line": 5}), false),
        (json!({"StartLine": 9, "EndLine": 3}), false),
        (json!({"StartLine": 1}), false),
        (json!({"EndLine": 5}), false),
        (json!({"limit": "50"}), false),
        (json!({}), false),
    ] {
        assert_eq!(bounded_read(&input), bounded, "{input}");
    }
}

/// `head`/`tail` counts are bounded at 1..=200; awk programs that redirect,
/// pipe or shell out are writes, not reads.
#[test]
fn head_count_and_awk_write_should_be_classified_when_judging_reads() {
    for (count, bounded) in [
        ("1", true),
        ("200", true),
        ("0", false),
        ("201", false),
        ("x", false),
        ("-5", false),
    ] {
        assert_eq!(head_count_is_bounded(count), bounded, "{count}");
    }
    assert!(awk_may_write(&strings(&["{print > \"out\"}", "f"])));
    assert!(awk_may_write(&strings(&["{print | \"sort\"}"])));
    assert!(awk_may_write(&strings(&["{system(\"rm x\")}"])));
    assert!(!awk_may_write(&strings(&["{print $1}", "f"])));
    assert!(!awk_may_write(&[]));
}

fn manifest(root: &Path, files: &[&str]) -> Manifest {
    Manifest {
        root: root.to_path_buf(),
        tasks: vec![TaskEntry {
            task: "fix the parser".into(),
            files: files
                .iter()
                .map(|f| ((*f).to_string(), "P0".to_string()))
                .collect(),
        }],
    }
}

/// Scoping allows the targets, their parent directories, Pixel's own state,
/// orientation files and anything outside the scoped repo; any other path in
/// the repo is outside the manifest.
#[test]
fn manifest_scoping_should_allow_a_path_when_targeted_orientation_or_outside() {
    let root = scratch("scoping");
    std::fs::create_dir_all(root.join("src/parse")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    let m = manifest(&root, &["src/parse/lexer.rs"]);
    for path in [
        root.join("src/parse/lexer.rs"),
        root.join("src"),
        root.join("src/parse"),
        root.clone(),
        root.join(".pixel"),
        root.join(".pixel/targets.json"),
        root.join("deep/README.md"),
        root.join("AGENTS.md"),
        root.join("Cargo.toml"),
        PathBuf::from("/elsewhere/x.rs"),
    ] {
        assert!(allowed(&path, &m), "{} must be allowed", path.display());
    }
    for path in [
        root.join("src/main.rs"),
        root.join("docs"),
        root.join("crates/Cargo.toml"),
        root.join(".pixelrc"),
    ] {
        assert!(
            !allowed(&path, &m),
            "{} is outside the manifest",
            path.display()
        );
    }
}

/// The edit mandate exempts paths outside the indexed repo, Pixel's state
/// and orientation files; source inside the repo is not exempt.
#[test]
fn edit_mandate_should_exempt_a_path_only_when_outside_state_or_orientation() {
    let root = Path::new("/repo");
    for (path, exempt) in [
        ("/other/a.rs", true),
        ("/repo/.pixel/targets.json", true),
        ("/repo/sub/CLAUDE.md", true),
        ("/repo/package.json", true),
        ("/repo/sub/package.json", false),
        ("/repo/src/lib.rs", false),
        ("/repo/.pixel", false),
    ] {
        assert_eq!(is_exempt(Path::new(path), root), exempt, "{path}");
    }
    assert_eq!(rel_of(Path::new("/repo/a/b.rs"), root), "a/b.rs");
    assert_eq!(rel_of(Path::new("/x/b.rs"), root), "/x/b.rs");
}

/// The manifest advisories name the path relative to the repo, every task
/// (shortened to 70 characters) and the commands that refresh or end
/// scoping.
#[test]
fn scoping_advisory_should_name_path_tasks_and_commands_when_out_of_scope() {
    let root = Path::new("/repo");
    let mut m = manifest(root, &["a.rs", "b.rs"]);
    m.tasks.push(TaskEntry {
        task: "x".repeat(80),
        files: vec![("c.rs".into(), "P1".into())],
    });
    let lines = scoping_advisory_lines(Path::new("/repo/z.rs"), &m);
    assert_eq!(
        lines,
        vec![
            "pixel-targets-guard advisory: 'z.rs' is outside the active targets manifest (2 task(s), 3 file(s)):".to_string(),
            "  - 'fix the parser'".to_string(),
            format!("  - '{}…'", "x".repeat(70)),
            "Proceeding. If scope has drifted, re-run `pixel scope-task \"<refined task>\"`".to_string(),
            "to refresh your task's list, or `pixel scope-task --clear` to end scoping.".to_string(),
        ]
    );
    assert_eq!(short_task("ééé", 3), "ééé");
    assert_eq!(short_task("éééé", 3), "ééé…");
    let mandate = mandate_advisory_lines(Path::new("/repo/src/lib.rs"), root);
    assert_eq!(
        mandate[1],
        "Proceeding with this edit (src/lib.rs), but scoping first is recommended:"
    );
    assert_eq!(mandate.len(), 5);
}

/// Manifest file entries need a string path; the tier defaults to empty.
#[test]
fn manifest_files_should_be_kept_only_when_they_carry_a_path() {
    use serde_json::json;
    assert_eq!(
        parse_manifest_files(&[
            json!({"path": "a.rs", "tier": "P0"}),
            json!({"path": "b.rs"}),
            json!({"path": 3, "tier": "P1"}),
            json!({"tier": "P2"}),
            json!("c.rs"),
        ]),
        vec![
            ("a.rs".to_string(), "P0".to_string()),
            ("b.rs".to_string(), String::new()),
        ]
    );
}

/// The tool classes the advisories key on: retrieval tools (search and
/// listing, Antigravity included), read tools, and source extensions; a
/// write, prose or config is none of them.
#[test]
fn tool_and_file_classes_should_match_only_when_listed() {
    for tool in [
        "Grep",
        "grep",
        "Glob",
        "glob",
        "find_file_by_name",
        "search",
        "find",
        "ls",
        "grep_search",
        "find_by_name",
        "list_dir",
        "file_search",
    ] {
        assert!(is_retrieval_tool(tool), "{tool} retrieves");
        assert!(!is_read_tool(tool), "{tool} does not read a file");
    }
    for tool in ["Read", "read", "read_file", "notebook_read", "view_file"] {
        assert!(is_read_tool(tool), "{tool} reads");
        assert!(!is_retrieval_tool(tool), "{tool} is not retrieval");
    }
    for tool in ["Edit", "Write", "Bash", ""] {
        assert!(!is_read_tool(tool) && !is_retrieval_tool(tool), "{tool}");
    }
    for ext in [
        "rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "c", "cpp", "h", "hpp", "cs", "rb",
        "swift", "kt", "scala", "clj", "ex", "exs", "erl", "hs", "ml", "fs", "nim", "zig", "v",
        "lua", "php", "pl", "r", "dart", "elm", "julia", "lisp", "sch",
    ] {
        assert!(
            is_source_file(Path::new(&format!("a.{ext}"))),
            "{ext} is source"
        );
    }
    for name in [
        "README.md",
        "package.json",
        "Cargo.toml",
        "Makefile",
        "a.txt",
    ] {
        assert!(!is_source_file(Path::new(name)), "{name} is not source");
    }
}

/// A result already showing the metrics box makes a relay a duplicate, in
/// each host's result field; a result without it does not.
#[test]
fn metrics_box_should_be_detected_when_any_result_field_shows_it() {
    use serde_json::json;
    for key in ["tool_response", "tool_output", "toolResult"] {
        assert!(
            result_carries_metrics_box(&json!({key: {"text": "out\n🟩 pixel 12 ms"}})),
            "{key}"
        );
    }
    assert!(!result_carries_metrics_box(
        &json!({"tool_response": "plain output"})
    ));
    assert!(!result_carries_metrics_box(&json!({"stderr": "🟩 pixel"})));
}

/// The binary name is its last path component, and exactly one layer of
/// matching outer quotes is stripped: `'a"` is not a quoted string.
#[test]
fn normalize_and_strip_should_remove_one_layer_when_applied() {
    assert_eq!(normalize_bin("/usr/bin/grep"), "grep");
    assert_eq!(normalize_bin("grep"), "grep");
    assert_eq!(normalize_bin("dir/"), "");
    assert_eq!(strip_outer_quotes("'a b'"), "a b");
    assert_eq!(strip_outer_quotes("\"a\""), "a");
    assert_eq!(strip_outer_quotes("'a\""), "'a\"");
    assert_eq!(strip_outer_quotes("'"), "'");
    assert_eq!(strip_outer_quotes(""), "");
}

/// An indexed repository (a shard file under `.pixel`) with a source file,
/// a git directory and a file outside it, for the policy decisions.
fn indexed_repo(name: &str) -> PathBuf {
    let root = scratch(name);
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join(pixel_index::index::SHARD_DIR)).unwrap();
    std::fs::write(
        root.join(pixel_index::index::SHARD_DIR)
            .join(pixel_index::index::SHARD_FILE),
        b"fixture shard marker",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn a() {}\n").unwrap();
    std::fs::write(root.join("README.md"), "text\n").unwrap();
    root
}

fn policy_payload(tool: &str, input: Value, cwd: &Path) -> Value {
    serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": tool,
        "tool_input": input,
        "cwd": cwd,
    })
}

/// Policy reasons for non-shell tools in an indexed repository: discovery
/// tools get the discovery reason, unbounded reads of a repository file get
/// the read reason (Devin's names its `exec` route), and a bounded read, a
/// read outside the repository or a pathless read get none.
#[test]
fn enforce_reason_should_judge_discovery_and_unbounded_reads_when_the_repo_is_indexed() {
    use serde_json::json;
    let root = indexed_repo("enforce-tools");
    let lib = root.join("src/lib.rs");
    for tool in [
        "grep_search",
        "find_by_name",
        "find_file_by_name",
        "list_dir",
        "file_search",
        "glob",
        "ls",
        "grep",
    ] {
        assert_eq!(
            enforce_reason(
                Provider::Antigravity,
                &policy_payload(tool, json!({"path": root}), &root)
            ),
            Some("repository discovery: use pixel search-content, find-code, or list-areas".into()),
            "{tool}"
        );
    }
    for tool in ["read", "view", "view_file", "notebook_read"] {
        assert_eq!(
            enforce_reason(
                Provider::Antigravity,
                &policy_payload(tool, json!({"file_path": lib}), &root)
            ),
            Some(REPO_READ_REASON.into()),
            "unbounded {tool}"
        );
        assert_eq!(
            enforce_reason(
                Provider::Antigravity,
                &policy_payload(tool, json!({"file_path": lib, "limit": 50}), &root)
            ),
            None,
            "bounded {tool}"
        );
        assert_eq!(
            enforce_reason(
                Provider::Antigravity,
                &policy_payload(tool, json!({}), &root)
            ),
            None,
            "pathless {tool}"
        );
    }
    assert_eq!(
        enforce_reason(
            Provider::Devin,
            &policy_payload("read", json!({"file_path": lib}), &root)
        ),
        Some(
            "repository read: use exec with pixel search-content or pixel pack-context <uid>"
                .into()
        )
    );
    assert_eq!(
        enforce_reason(
            Provider::Devin,
            &policy_payload(
                "read",
                json!({"file_path": lib, "start_line": 1, "end_line": 20}),
                &root
            )
        ),
        None
    );
    for key in [
        "path",
        "AbsolutePath",
        "SearchPath",
        "SearchDirectory",
        "DirectoryPath",
        "abs_path",
    ] {
        assert_eq!(
            enforce_reason(
                Provider::Antigravity,
                &policy_payload("view_file", json!({key: lib}), &root)
            ),
            Some(REPO_READ_REASON.into()),
            "{key} names the read path"
        );
    }
    let outside = scratch("enforce-outside");
    std::fs::write(outside.join("x.rs"), "x\n").unwrap();
    assert_eq!(
        enforce_reason(
            Provider::Antigravity,
            &policy_payload("read", json!({"file_path": outside.join("x.rs")}), &root)
        ),
        None,
        "a read outside the repository is not a repository read"
    );
    assert_eq!(
        enforce_reason(
            Provider::Antigravity,
            &policy_payload("Write", json!({"file_path": lib}), &root)
        ),
        None,
        "writes are not policy-judged"
    );
}

/// Policy never judges Claude, post-tool events, unrecognised events, a
/// repository without a shard (a bare `.pixel` is not an index), or a shell
/// call that carries its own environment.
#[test]
fn enforce_reason_should_stay_silent_when_the_call_is_not_policy_judged() {
    use serde_json::json;
    let root = indexed_repo("enforce-unjudged");
    let read =
        |cwd: &Path| policy_payload("read", json!({"file_path": cwd.join("src/lib.rs")}), cwd);
    assert_eq!(enforce_reason(Provider::Claude, &read(&root)), None);
    assert_eq!(
        enforce_reason(Provider::Codex, &read(&root)),
        None,
        "Codex retrieval stays native under every policy"
    );
    let mut post = read(&root);
    post["hook_event_name"] = json!("PostToolUse");
    assert_eq!(enforce_reason(Provider::Antigravity, &post), None);
    post["hook_event_name"] = json!("postToolUse");
    assert_eq!(enforce_reason(Provider::Antigravity, &post), None);
    let mut other = read(&root);
    other["hook_event_name"] = json!("SessionStart");
    assert_eq!(enforce_reason(Provider::Antigravity, &other), None);
    let mut unnamed = read(&root);
    unnamed.as_object_mut().unwrap().remove("hook_event_name");
    assert_eq!(
        enforce_reason(Provider::Antigravity, &unnamed),
        Some(REPO_READ_REASON.into()),
        "an unnamed event with tool fields is a pre-tool call (older Cursor)"
    );
    let bare = scratch("enforce-bare");
    std::fs::create_dir_all(bare.join(".git")).unwrap();
    std::fs::create_dir_all(bare.join(".pixel")).unwrap();
    std::fs::create_dir_all(bare.join("src")).unwrap();
    std::fs::write(bare.join("src/lib.rs"), "x\n").unwrap();
    assert_eq!(
        enforce_reason(Provider::Antigravity, &read(&bare)),
        None,
        "no shard, no policy"
    );
    for key in ["env", "environment"] {
        assert_eq!(
            enforce_reason(
                Provider::Antigravity,
                &policy_payload(
                    "Bash",
                    json!({"command": "cat README.md", key: {"A": "1"}}),
                    &root
                )
            ),
            None,
            "a shell call with its own {key} stays native"
        );
    }
}

/// Every host's shell tool, with the command under `command`, `cmd` or
/// `CommandLine` and as a string or an argv array, reaches the same leaf
/// judgement: `cat` of a repository file is a repository read.
#[test]
fn enforce_reason_should_judge_the_command_when_any_shell_tool_spelling_is_used() {
    use serde_json::json;
    let root = indexed_repo("enforce-shells");
    for tool in [
        "Bash",
        "bash",
        "shell",
        "Shell",
        "unified_exec",
        "local_shell",
        "exec_command",
        "run_command",
        "exec",
    ] {
        for input in [
            json!({"command": "cat README.md"}),
            json!({"cmd": "cat README.md"}),
            json!({"CommandLine": "cat README.md"}),
            json!({"command": ["bash", "-lc", "cat README.md"]}),
        ] {
            assert_eq!(
                enforce_reason(
                    Provider::Antigravity,
                    &policy_payload(tool, input.clone(), &root)
                ),
                Some(REPO_READ_REASON.into()),
                "{tool} {input}"
            );
        }
    }
    assert_eq!(
        enforce_reason(
            Provider::Antigravity,
            &policy_payload("Bash", json!({"description": "x"}), &root)
        ),
        None,
        "a shell call without a command has nothing to judge"
    );
}

/// The shell judgement leaves compound commands native when a directory
/// change or a wrapper word could move relative operands, and when the
/// syntax is outside the bounded parser.
#[test]
fn enforce_shell_should_stay_native_when_cd_wrappers_or_unparsed_syntax_appear() {
    let root = indexed_repo("enforce-compound");
    for command in [
        "cd src && cat lib.rs",
        "command cat README.md",
        "builtin cat README.md",
        "cat $(echo README.md)",
        "cat README.md > /tmp/x",
    ] {
        assert_eq!(
            enforce_shell_for_provider(command, &root, &root, true),
            None,
            "`{command}` stays native"
        );
    }
    assert_eq!(
        enforce_shell_for_provider("echo hi && cat README.md", &root, &root, false),
        Some(REPO_READ_REASON.into()),
        "a later leaf of a chain is still judged"
    );
}

/// `git status`, `diff` and `log` with no further arguments map to their
/// Pixel equivalents, past global options and their values; any argument
/// after the subcommand, another subcommand or no subcommand stays native.
#[test]
fn enforce_leaf_should_name_the_pixel_command_when_git_inspection_is_bare() {
    let root = indexed_repo("enforce-git");
    let leaf = |words: &[&str]| enforce_leaf("", &strings(words), false, &root, &root, false);
    for (words, alternative) in [
        (&["git", "status"][..], "repo-state"),
        (
            &["git", "-C", ".", "--no-pager", "diff"][..],
            "review-changes",
        ),
        (
            &["git", "--git-dir", ".git", "--work-tree", ".", "log"][..],
            "commit-history",
        ),
        (
            &["git", "-c", "color.ui=never", "--namespace", "x", "status"][..],
            "repo-state",
        ),
    ] {
        assert_eq!(
            leaf(words),
            Some(format!("repository inspection: use pixel {alternative}")),
            "{words:?}"
        );
    }
    for words in [
        &["git", "log", "--oneline"][..],
        &["git", "diff", "HEAD"][..],
        &["git", "show"][..],
        &["git"][..],
        &["git", "-C"][..],
    ] {
        assert_eq!(leaf(words), None, "{words:?}");
    }
}

/// Listing is judged only with the listing flags the leaf understands, and
/// a non-enforcing host judges `find` only in its `find DIR -name X` shape.
#[test]
fn enforce_leaf_should_judge_listing_and_find_only_when_the_shape_is_understood() {
    let root = indexed_repo("enforce-listing");
    let leaf = |words: &[&str], enforce: bool| {
        enforce_leaf("", &strings(words), false, &root, &root, enforce)
    };
    for flag in ["-a", "-l", "-la", "-al", "--all", "--long"] {
        assert_eq!(
            leaf(&["ls", flag, "src"], false),
            Some("repository discovery: use pixel list-areas or find-code".into()),
            "ls {flag}"
        );
    }
    assert_eq!(
        leaf(&["ls", "-R", "src"], false),
        None,
        "an unknown flag stays native"
    );
    assert_eq!(
        leaf(&["find", ".", "-name", "x"], false),
        Some("repository discovery: use pixel find-code or list-areas".into())
    );
    assert_eq!(leaf(&["find", ".", "-type", "f"], false), None);
    assert_eq!(
        leaf(&["find", "/", "-name", "x"], false),
        None,
        "outside the repo"
    );
    assert_eq!(leaf(&["find"], true), None);
}

/// An echo used as a separator in an approved chain is only a literal echo:
/// any expansion, redirection or chaining character disqualifies it, and so
/// does any other program.
#[test]
fn static_echo_should_hold_only_when_the_echo_is_literal() {
    for (command, literal) in [
        ("echo ---", true),
        ("echo 'section two'", true),
        ("echo $HOME", false),
        ("echo `id`", false),
        ("echo a > f", false),
        ("echo a < f", false),
        ("echo a | wc", false),
        ("echo a & b", false),
        ("echo a; ls", false),
        ("echo a\\nb", false),
        ("printf x", false),
        ("", false),
    ] {
        assert_eq!(is_static_echo(command), literal, "{command:?}");
    }
}

/// The bounded sed read shape is `[rtk] sed -n 'A,Bp' path` exactly, with a
/// non-empty window of at most 200 lines and a path that is neither a flag
/// nor credential-shaped.
#[test]
fn bounded_sed_shape_should_match_only_when_the_window_print_is_exact() {
    for (command, shape) in [
        ("sed -n '1,200p' src/lib.rs", Some((1, 200, "src/lib.rs"))),
        ("rtk sed -n 5,9p a.rs", Some((5, 9, "a.rs"))),
        ("sed -n '1,201p' a.rs", None),
        ("sed -n '9,5p' a.rs", None),
        ("sed -n '0,5p' a.rs", None),
        ("sed -n '1,5' a.rs", None),
        ("sed -n '1p' a.rs", None),
        ("sed -n 'a,5p' a.rs", None),
        ("sed -n '1,bp' a.rs", None),
        ("sed -e '1,5p' a.rs", None),
        ("sed -n '1,5p' -i", None),
        ("sed -n '1,5p' .env", None),
        ("sed -n '1,5p' a.rs b.rs", None),
        ("awk -n '1,5p' a.rs", None),
        ("sed -n '1,5p' a.rs > out", None),
        ("sed -n '1,5p' $FILE", None),
    ] {
        assert_eq!(
            bounded_sed_shape(command),
            shape.map(|(start, end, path)| (start, end, path.to_string())),
            "{command:?}"
        );
    }
}

/// Splitting at a separator ignores quoted separators and refuses an
/// escape or an unterminated quote rather than guessing.
#[test]
fn split_unquoted_should_refuse_when_an_escape_or_open_quote_appears() {
    assert_eq!(
        split_unquoted("a|'b|c'|\"d|e\"", '|'),
        Some(vec!["a", "'b|c'", "\"d|e\""])
    );
    assert_eq!(split_unquoted("", '|'), Some(vec![""]));
    assert_eq!(split_unquoted("a\\|b", '|'), None);
    assert_eq!(split_unquoted("a|'b", '|'), None);
    assert_eq!(split_unquoted("a;b", '|'), Some(vec!["a;b"]));
}

/// A safe chain splits at `;`, `&&` and `||`, keeps `2>&1` inside its stage,
/// and refuses a bare `&`, a trailing operator, an empty stage or an open
/// quote.
#[test]
fn safe_command_chain_should_split_only_when_the_operators_are_known() {
    assert_eq!(
        split_safe_command_chain("pixel a; pixel b && echo x || pixel c 2>&1"),
        Some(vec!["pixel a", "pixel b", "echo x", "pixel c 2>&1"])
    );
    assert_eq!(
        split_safe_command_chain("pixel search-content 'a;b' && echo \"c&&d\""),
        Some(vec!["pixel search-content 'a;b'", "echo \"c&&d\""])
    );
    for command in [
        "pixel a &",
        "pixel a & pixel b",
        "pixel a &&",
        "; pixel a",
        "pixel a ;; pixel b",
        "pixel 'a",
        "pixel a\rpixel b",
        "",
    ] {
        assert_eq!(split_safe_command_chain(command), None, "{command:?}");
    }
}

/// A word in a path role, or one that looks like a path, must resolve
/// inside the repository and outside `.git`, `.pixel` and credentials; a
/// plain pattern is not a path, and a missing relative path in a path role
/// passes on its typed name.
#[test]
fn word_should_stay_inside_the_repository_when_it_names_a_path() {
    let root = indexed_repo("word-in-repo");
    std::fs::write(root.join(".env"), "K=v\n").unwrap();
    let outside = scratch("word-outside");
    for (word, path_role, inside) in [
        ("needle", false, true),
        ("src/lib.rs", false, true),
        ("src/lib.rs", true, true),
        ("deleted/file.rs", true, true),
        ("deleted/file.rs", false, true),
        ("~/secrets", false, false),
        (".env", false, false),
        (".git", false, false),
        (".pixel", true, false),
        ("../x", true, false),
        ("../x", false, false),
        ("/nonexistent/abs", true, false),
    ] {
        assert_eq!(
            word_stays_in_repo(word, path_role, &root, &root),
            inside,
            "{word:?} (path role {path_role})"
        );
    }
    assert!(
        !word_stays_in_repo(&outside.display().to_string(), false, &root, &root),
        "an existing absolute path outside the repository"
    );
    assert!(
        word_stays_in_repo(&root.join("src").display().to_string(), false, &root, &root),
        "an absolute path inside the repository"
    );
}

/// Only the bare names `pixel`/`pixel-dev`, or an absolute path that is this
/// very executable, are Pixel; any other path is some other program.
#[test]
fn pixel_program_should_match_only_when_bare_or_this_executable() {
    assert!(is_pixel_program("pixel"));
    assert!(is_pixel_program("pixel-dev"));
    assert!(!is_pixel_program("pixelate"));
    assert!(!is_pixel_program("./pixel"));
    assert!(!is_pixel_program("/usr/bin/pixel-not-this-one"));
    let me = std::env::current_exe().unwrap();
    assert!(is_pixel_program(&me.display().to_string()));
}

/// Every source directory name draws the `ls` advisory, and `find -exec`
/// is advised for each nested search tool.
#[test]
fn bypass_advisory_should_name_every_source_dir_and_nested_search_tool() {
    let root = bypass_repo();
    for dir in ["lib", "test", "tests", "include"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        let lines = bypass_advisory_lines(&format!("ls {dir}"), &root, &root)
            .unwrap_or_else(|| panic!("`ls {dir}` lists source"));
        assert!(lines[0].contains("ls of source directory"), "{lines:?}");
    }
    for tool in ["rg", "ag", "ack"] {
        let cmd = format!("find . -type f -exec {tool} needle {{}} +");
        let lines = bypass_advisory_lines(&cmd, &root, &root)
            .unwrap_or_else(|| panic!("`{cmd}` nests a search"));
        assert!(lines[0].contains("find -exec grep"), "{lines:?}");
    }
}

/// `git branch --delete` is destructive only when forced.
#[test]
fn branch_delete_is_denied_only_when_forced() {
    let root = scratch("branch-delete");
    for args in [
        &["--delete", "--force", "x"][..],
        &["--delete", "-f", "x"][..],
        &["-D", "x"][..],
    ] {
        assert!(
            destructive_git_deny("branch", &strings(args), &root).is_some(),
            "{args:?}"
        );
    }
    assert_eq!(
        destructive_git_deny("branch", &strings(&["--delete", "x"]), &root),
        None
    );
    assert_eq!(
        destructive_git_deny("branch", &strings(&["--force", "x"]), &root),
        None
    );
}

/// A scratch repository directory with an empty `.pixel/`, so the reconcile
/// marker can be written into it. No `git init`: the escape hatch reads only
/// the marker, and spawning git outside `pixel-git` breaks its boundary test.
fn rebase_repo(name: &str) -> PathBuf {
    let root = scratch(name);
    std::fs::create_dir_all(root.join(".pixel")).unwrap();
    root
}

/// A raw `git rebase` passes only when the repository it is pointed at, by
/// `cd` or by `git -C`, has a reported reconcile conflict.
#[test]
fn rebase_escapes_the_substitute_only_into_a_reported_conflict() {
    let root = rebase_repo("rebase-escape-root");
    let alt = rebase_repo("rebase-escape-alt");
    let via_cd = format!("cd {} && git rebase main", alt.display());
    let via_c = format!("git -C {} rebase main", alt.display());
    for cmd in [&via_cd, &via_c] {
        assert!(
            git_mutation_substitute_lines(cmd, Some(&root), &root).is_some(),
            "`{cmd}` with no conflict is substituted"
        );
    }
    std::fs::write(alt.join(".pixel/reconcile-conflict.json"), "{}").unwrap();
    for cmd in [&via_cd, &via_c] {
        assert_eq!(
            git_mutation_substitute_lines(cmd, Some(&root), &root),
            None,
            "`{cmd}` resolves a reported conflict"
        );
    }
    let add = format!("cd {} && git add src/lib.rs", alt.display());
    assert!(
        git_mutation_substitute_lines(&add, Some(&root), &root).is_some(),
        "only a rebase takes the conflict escape"
    );
}

/// The `-C` target of a rebase is resolved against the hook's cwd (or the
/// `cd` before it) and canonicalized, through a `-c` flag; a clean, missing
/// or same-as-root target is still substituted.
#[test]
fn rebase_escape_should_resolve_the_c_directory_like_git() {
    let root = rebase_repo("rebase-c-root");
    let parent = scratch("rebase-c-parent");
    let alt = parent.join("alt");
    std::fs::create_dir_all(alt.join(".pixel")).unwrap();
    let clean = rebase_repo("rebase-c-clean");
    std::fs::write(alt.join(".pixel/reconcile-conflict.json"), "{}").unwrap();

    for (cmd, cwd) in [
        ("git -C alt rebase main".to_string(), &parent),
        ("git -C . -C alt rebase main".to_string(), &parent),
        (
            "git -c core.pager=cat -C alt/../alt rebase main".to_string(),
            &parent,
        ),
        (
            format!("cd {} && git -C alt rebase main", parent.display()),
            &root,
        ),
        (
            format!("git -C {} -C alt rebase main", parent.display()),
            &root,
        ),
    ] {
        assert_eq!(
            git_mutation_substitute_lines(&cmd, Some(&root), cwd),
            None,
            "`{cmd}` in {} reaches the conflicted repository",
            cwd.display()
        );
    }

    for cmd in [
        format!("git -C {} rebase main", clean.display()),
        format!("git -C {} rebase main", root.display()),
        "git -C missing rebase main".to_string(),
        // Without the `cd`, `alt` is looked up under the root, not `parent`.
        "git -C alt rebase main".to_string(),
    ] {
        assert!(
            git_mutation_substitute_lines(&cmd, Some(&root), &root).is_some(),
            "`{cmd}` is still substituted"
        );
    }
}

/// `git commit` flags map onto the substitute's fields, long and short.
#[test]
fn commit_args_should_read_every_flag_spelling() {
    assert!(parse_commit_args(&strings(&["--all"])).all);
    assert!(parse_commit_args(&strings(&["-a"])).all);
    assert!(parse_commit_args(&strings(&["--amend"])).amend);
    for flag in [
        "--interactive",
        "--patch",
        "--fixup",
        "--squash",
        "--fixup=abc",
        "--squash=abc",
        "-p",
    ] {
        assert!(parse_commit_args(&strings(&[flag])).interactive, "{flag}");
    }
    for args in [
        &["-m", "msg"][..],
        &["--message", "msg"][..],
        &["--message=msg"][..],
        &["-am", "msg"][..],
    ] {
        assert_eq!(
            parse_commit_args(&strings(args)).message.as_deref(),
            Some("msg"),
            "{args:?}"
        );
    }
    let plain = parse_commit_args(&strings(&["--author", "me", "-F", "f", "a.rs", "b.rs"]));
    assert_eq!(plain.files, strings(&["a.rs", "b.rs"]));
    assert!(!plain.all && !plain.amend && !plain.interactive);
    assert_eq!(plain.message, None);
}

fn composed_backup(dir: &Path, name: &str, pad_to: usize) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let mut body = serde_json::json!({
        "version": 1,
        "provider": "codex",
        "pre_tool_use": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "keep-check"}]}
        ],
    })
    .to_string();
    while body.len() < pad_to {
        body.push(' ');
    }
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

/// A backup of exactly the input cap loads; one byte more does not.
#[test]
fn composed_backup_should_load_up_to_its_byte_cap() {
    let dir = scratch("composed-cap");
    let exact = composed_backup(&dir, "exact.json", COMPOSED_MAX_INPUT);
    let hooks = load_composed_backup(&exact).expect("a backup at the cap loads");
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].command, "keep-check");
    let over = composed_backup(&dir, "over.json", COMPOSED_MAX_INPUT + 1);
    assert!(load_composed_backup(&over).is_none());
}

/// A backup path that is not a regular file is refused before it is read,
/// even when what it would yield is a valid backup: a FIFO fed by a writer.
#[test]
fn composed_backup_should_refuse_a_fifo() {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = scratch("composed-fifo");
    let fifo = dir.join("backup.json");
    let c_path = std::ffi::CString::new(fifo.display().to_string()).unwrap();
    // SAFETY: `c_path` is a valid NUL-terminated path for the call's duration.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let body = std::fs::read(composed_backup(&dir, "valid.json", 0)).unwrap();
    let writer_path = fifo.clone();
    let writer = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            // Non-blocking: opening fails until a reader is there.
            if let Ok(mut pipe) = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&writer_path)
            {
                let _ = std::io::Write::write_all(&mut pipe, &body);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });
    assert!(load_composed_backup(&fifo).is_none());
    writer.join().unwrap();
}

/// A foreign hook whose stderr runs past the output cap is ignored, even
/// with an empty stdout and a zero exit.
#[test]
fn foreign_command_output_past_the_cap_is_dropped() {
    let dir = scratch("foreign-output");
    assert_eq!(
        run_foreign_command("echo '{\"a\":1}'", b"{}", &dir),
        Some(serde_json::json!({"a": 1}))
    );
    assert_eq!(run_foreign_command("true", b"{}", &dir), Some(Value::Null));
    let over = COMPOSED_MAX_OUTPUT + 1;
    assert_eq!(
        run_foreign_command(&format!("head -c {over} /dev/zero >&2"), b"{}", &dir),
        None
    );
}

/// The composed hook reads a non-empty payload of at most the cap.
#[test]
fn composed_payload_should_be_non_empty_and_within_the_cap() {
    assert_eq!(
        read_composed_payload(&b"abcd"[..], 4),
        Some(b"abcd".to_vec())
    );
    assert_eq!(read_composed_payload(&b"abc"[..], 4), Some(b"abc".to_vec()));
    assert_eq!(read_composed_payload(&b"abcde"[..], 4), None);
    assert_eq!(read_composed_payload(&b""[..], 4), None);
}

/// Every harness's shell tool is guarded as a shell; file tools are not.
#[test]
fn shell_tools_should_be_recognised_by_every_harness_name() {
    for tool in [
        "Bash",
        "exec",
        "bash",
        "run_shell_command",
        "execute",
        "Shell",
        "run_command",
        "shell",
        "unified_exec",
        "local_shell",
    ] {
        assert!(is_shell_tool(tool), "{tool}");
    }
    for tool in ["Read", "Edit", "Write", "grep", ""] {
        assert!(!is_shell_tool(tool), "{tool}");
    }
}

/// Only an existing file draws an edit advisory: out of an active
/// manifest's scope, unscoped in an index, or outside any index.
#[test]
fn edit_advice_should_follow_manifest_then_index_then_none() {
    use EditAdvice::{OutOfScope, Proceed, SuggestIndex, Unscoped};
    for (in_scope, exempt) in [
        (Some(false), None),
        (Some(true), Some(false)),
        (None, Some(false)),
        (None, None),
    ] {
        assert_eq!(edit_advice(false, in_scope, exempt), Proceed, "new file");
    }
    assert_eq!(edit_advice(true, Some(false), Some(true)), OutOfScope);
    assert_eq!(edit_advice(true, Some(true), Some(false)), Proceed);
    assert_eq!(edit_advice(true, None, Some(false)), Unscoped);
    assert_eq!(edit_advice(true, None, Some(true)), Proceed);
    assert_eq!(edit_advice(true, None, None), SuggestIndex);
}

/// The post-edit note's path list is capped past eight files or when a
/// path is shortened.
#[test]
fn post_edit_paths_should_be_capped_past_the_limit_or_a_long_path() {
    let short = vec!["src/a.rs".to_string()];
    assert!(!snapshot_paths_capped(PATH_LIMIT, &short));
    assert!(snapshot_paths_capped(PATH_LIMIT + 1, &short));
    assert!(!snapshot_paths_capped(1, &["x".repeat(PATH_CHARS)]));
    assert!(snapshot_paths_capped(1, &["x".repeat(PATH_CHARS + 1)]));
}

/// A Bash command draws the bypass advisory in an index, else the manifest
/// scoping for a single out-of-scope read; substitutions draw nothing.
#[test]
fn bash_advisory_should_prefer_the_bypass_then_the_manifest_scope() {
    let root = bypass_repo();
    let m = manifest(&root, &["README.md"]);
    assert!(matches!(
        bash_advisory("ag needle", &root, Some(&root), Some(&m)),
        Some(BashAdvisory::Bypass(_))
    ));
    assert!(matches!(
        bash_advisory("cd src && egrep needle lib.rs", &root, Some(&root), None),
        Some(BashAdvisory::Bypass(_))
    ));
    assert_eq!(
        bash_advisory("cat notes.txt", &root, None, Some(&m)),
        Some(BashAdvisory::OutOfScope(root.join("notes.txt")))
    );
    assert_eq!(bash_advisory("cat README.md", &root, None, Some(&m)), None);
    assert_eq!(
        bash_advisory("cat notes.txt", &root, Some(&root), None),
        None
    );
    for cmd in ["ag $(echo x)", "ag `x`", "ag <<EOF"] {
        assert_eq!(
            bash_advisory(cmd, &root, Some(&root), Some(&m)),
            None,
            "{cmd}"
        );
    }
}
