// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Contract tests for the daemon's op surface, driven through
//! `Service::handle` the way the socket drives it: what each op answers for
//! an unknown, ambiguous or malformed request (the recovery an agent needs),
//! the envelope every answer carries, and the pure helpers behind them.

use super::*;
use std::path::PathBuf;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pixel-daemon-api-contract-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// `login` is called once by `go`; `helper` is defined twice, so a bare
/// `helper` is ambiguous.
fn fixture(tag: &str) -> PathBuf {
    let root = tmpdir(tag);
    std::fs::write(
        root.join("login.rs"),
        "pub fn login(user: &str) -> bool { !user.is_empty() }\npub fn helper() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("caller.rs"),
        "use crate::login::login;\npub fn go() { login(\"a\"); }\npub fn helper() {}\n",
    )
    .unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "init"]);
    root
}

/// The envelope of `req`, as the socket would serialize it.
fn call(service: &mut Service, req: Request) -> Value {
    serde_json::to_value(service.handle(req)).unwrap()
}

fn ok(service: &mut Service, req: Request) -> Value {
    let env = call(service, req);
    assert_eq!(env["ok"], true, "{env}");
    env["result"].clone()
}

fn err(service: &mut Service, req: Request) -> (String, String) {
    let env = call(service, req);
    assert_eq!(env["ok"], false, "{env}");
    (
        env["error"]["code"].as_str().unwrap().to_string(),
        env["error"]["message"].as_str().unwrap().to_string(),
    )
}

fn names(candidates: &Value) -> Vec<String> {
    let mut out: Vec<String> = candidates["candidates"]
        .as_array()
        .unwrap_or_else(|| panic!("candidates in {candidates}"))
        .iter()
        .map(|c| {
            format!(
                "{}@{}",
                c["name"].as_str().unwrap(),
                c["path"].as_str().unwrap()
            )
        })
        .collect();
    out.sort();
    out
}

// -- envelope ----------------------------------------------------------------

#[test]
fn ping_should_report_the_root_and_the_protocol_version_without_epistemics() {
    let root = tmpdir("ping");
    let mut service = Service::open(&root).unwrap();
    let env = call(&mut service, Request::Ping);
    assert_eq!(env["ok"], true);
    assert_eq!(env["result"]["pong"], true);
    assert_eq!(env["result"]["root"], root.display().to_string());
    assert_eq!(env["result"]["protocol_version"], PROTOCOL_VERSION);
    assert!(env.get("epistemics").is_none(), "ping is not a retrieval");
    assert!(env.get("snapshot").is_none(), "ping reports no tree state");
    assert_eq!(
        ok(&mut service, Request::Shutdown),
        json!({"shutting_down": true})
    );
}

#[test]
fn recall_should_be_refused_with_the_daemon_that_serves_it() {
    let root = tmpdir("recall");
    let mut service = Service::open(&root).unwrap();
    let (code, message) = err(
        &mut service,
        Request::Recall {
            action: "search".into(),
            params: Value::Null,
        },
    );
    assert_eq!(code, "INVALID_INPUT");
    assert!(
        message.contains("`pixel recall daemon start`"),
        "the refusal names the daemon to start: {message}"
    );
}

#[test]
fn a_retrieval_answer_should_carry_epistemics_and_a_compact_snapshot() {
    let root = fixture("envelope-retrieval");
    let mut service = Service::open(&root).unwrap();
    let env = call(
        &mut service,
        Request::Symbol {
            name: "login".into(),
        },
    );
    assert_eq!(env["ok"], true, "{env}");
    assert_eq!(env["epistemics"]["closed_world"], false);
    assert!(
        env["epistemics"]["basis"]
            .as_str()
            .unwrap()
            .starts_with("code graph"),
        "{env}"
    );
    assert!(env["snapshot"]["head"].is_string(), "{env}");
    assert!(
        env["snapshot"]
            .get("dirty")
            .is_none_or(|d| d.as_array().is_some_and(Vec::is_empty)),
        "a retrieval answer does not enumerate the dirty tree: {env}"
    );
}

// -- symbol lookup protocol ------------------------------------------------------

#[test]
fn symbol_should_list_every_definition_of_a_shared_name_with_its_file() {
    let root = fixture("symbol-shared");
    let mut service = Service::open(&root).unwrap();
    let out = ok(
        &mut service,
        Request::Symbol {
            name: "helper".into(),
        },
    );
    let mut paths: Vec<&str> = out["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["path"].as_str().unwrap())
        .collect();
    paths.sort_unstable();
    assert_eq!(paths, ["caller.rs", "login.rs"]);
    assert!(
        out["graph_build"].is_object(),
        "the first query built the graph"
    );
}

#[test]
fn skeleton_should_accept_an_absolute_path_inside_the_root_and_name_a_missing_file() {
    let root = fixture("skeleton");
    let mut service = Service::open(&root).unwrap();
    let out = ok(
        &mut service,
        Request::Skeleton {
            file: root.join("login.rs").display().to_string(),
        },
    );
    assert_eq!(out["file"], "login.rs");
    let syms: Vec<&str> = out["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(syms, ["login", "helper"]);

    let (code, message) = err(
        &mut service,
        Request::Skeleton {
            file: "/nope.rs".into(),
        },
    );
    assert_eq!(code, "INVALID_INPUT");
    assert_eq!(
        message,
        "no indexed file matching '/nope.rs' (looked for 'nope.rs')"
    );
}

#[test]
fn context_should_return_candidates_for_an_ambiguous_name_and_not_found_for_an_unknown_one() {
    let root = fixture("context");
    let mut service = Service::open(&root).unwrap();
    let out = ok(
        &mut service,
        Request::Context {
            uid: "helper".into(),
            budget_tokens: None,
        },
    );
    assert_eq!(names(&out), ["helper@caller.rs", "helper@login.rs"]);
    assert_eq!(out["hint"], "ambiguous name; re-call with uid");

    let (code, message) = err(
        &mut service,
        Request::Context {
            uid: "helper".into(),
            budget_tokens: Some(1),
        },
    );
    assert_eq!(code, "INVALID_INPUT");
    assert!(
        message.starts_with("ambiguous name matches 2 symbols;"),
        "the candidate list must fit the budget too: {message}"
    );

    let (code, message) = err(
        &mut service,
        Request::Context {
            uid: "nosuchsymbol".into(),
            budget_tokens: None,
        },
    );
    assert_eq!(code, "NOT_FOUND");
    assert!(
        message.contains("pixel find-symbol nosuchsymbol"),
        "{message}"
    );
}

#[test]
fn impact_and_trace_should_stop_at_an_ambiguous_name_with_its_candidates() {
    let root = fixture("impact-ambiguous");
    let mut service = Service::open(&root).unwrap();
    let impact = ok(
        &mut service,
        Request::Impact {
            uid_or_name: "helper".into(),
            direction: "upstream".into(),
            depth: None,
        },
    );
    assert_eq!(names(&impact), ["helper@caller.rs", "helper@login.rs"]);
    let trace = ok(
        &mut service,
        Request::Trace {
            from: "go".into(),
            to: "helper".into(),
        },
    );
    assert_eq!(
        names(&trace),
        ["helper@caller.rs", "helper@login.rs"],
        "an ambiguous `to` is resolved before any path is searched"
    );
    let trace_from = ok(
        &mut service,
        Request::Trace {
            from: "helper".into(),
            to: "login".into(),
        },
    );
    assert_eq!(names(&trace_from), ["helper@caller.rs", "helper@login.rs"]);
}

#[test]
fn impact_should_name_a_uid_that_matches_nothing() {
    let root = fixture("impact-uid");
    let mut service = Service::open(&root).unwrap();
    let (code, message) = err(
        &mut service,
        Request::Impact {
            uid_or_name: "rust:nowhere.rs#ghost".into(),
            direction: "upstream".into(),
            depth: Some(1),
        },
    );
    assert_eq!(code, "NOT_FOUND");
    assert_eq!(
        message,
        "no symbol with uid \"rust:nowhere.rs#ghost\"; run `pixel find-symbol <name>` to list uids"
    );
}

#[test]
fn uses_should_page_callers_and_report_callees_with_their_outgoing_uncertainty() {
    let root = fixture("uses");
    let mut service = Service::open(&root).unwrap();
    let callers = ok(
        &mut service,
        Request::Uses {
            uid_or_name: "login".into(),
            role: "callers".into(),
            offset: None,
        },
    );
    assert_eq!(callers["role"], "callers");
    assert_eq!(callers["total_edges"], 1);
    assert_eq!(callers["returned_edges"], 1);
    assert_eq!(callers["edges"][0]["symbol"]["name"], "go");
    assert_eq!(callers["truncated"], false);
    assert_eq!(callers["next_offset"], Value::Null);
    assert!(
        callers["envelope"].get("unresolved_outgoing").is_none(),
        "outgoing uncertainty is a callees-only field"
    );

    let past_the_end = ok(
        &mut service,
        Request::Uses {
            uid_or_name: "login".into(),
            role: "callers".into(),
            offset: Some(10),
        },
    );
    assert_eq!(
        past_the_end["offset"], 1,
        "an offset past the end is clamped"
    );
    assert_eq!(past_the_end["edges"], json!([]));
    assert_eq!(past_the_end["truncated"], false);

    let callees = ok(
        &mut service,
        Request::Uses {
            uid_or_name: "go".into(),
            role: "callees".into(),
            offset: None,
        },
    );
    assert_eq!(callees["role"], "callees");
    assert_eq!(callees["edges"][0]["symbol"]["name"], "login");
    assert_eq!(callees["envelope"]["unresolved_outgoing"], 0);
    assert!(
        callees["envelope"].get("caps").is_none_or(Value::is_null),
        "no unresolved call, no cap: {callees}"
    );

    let ambiguous = ok(
        &mut service,
        Request::Uses {
            uid_or_name: "helper".into(),
            role: "callers".into(),
            offset: None,
        },
    );
    assert_eq!(names(&ambiguous), ["helper@caller.rs", "helper@login.rs"]);
}

#[test]
fn uses_callees_should_mark_a_lower_bound_when_the_symbol_has_unresolved_calls() {
    let root = tmpdir("uses-unresolved");
    std::fs::write(
        root.join("a.rs"),
        "pub fn outer() { mystery_external(); }\n",
    )
    .unwrap();
    let mut service = Service::open(&root).unwrap();
    let out = ok(
        &mut service,
        Request::Uses {
            uid_or_name: "outer".into(),
            role: "callees".into(),
            offset: None,
        },
    );
    assert_eq!(out["envelope"]["unresolved_outgoing"], 1, "{out}");
    assert_eq!(out["envelope"]["lower_bound"], true);
    assert_eq!(
        out["envelope"]["caps"],
        json!([
            "graph lower bound: 1 unresolved outgoing call site(s) — callees beyond this answer may exist"
        ])
    );
}

// -- rename ----------------------------------------------------------------------

#[test]
fn rename_should_refuse_a_new_name_that_is_not_an_identifier() {
    let root = fixture("rename-ident");
    let mut service = Service::open(&root).unwrap();
    for bad in ["", "9lives", "has-dash", "with space"] {
        let (code, message) = err(
            &mut service,
            Request::Rename {
                name: "login".into(),
                new_name: bad.into(),
                file: None,
                uid: None,
                dry_run: true,
            },
        );
        assert_eq!(code, "INVALID_INPUT");
        assert_eq!(
            message,
            format!("rename: {bad:?} is not an identifier (letters, digits, `_`, non-digit first)")
        );
    }
}

#[test]
fn rename_should_disambiguate_by_file_and_name_what_it_could_not_find() {
    let root = fixture("rename-scope");
    let mut service = Service::open(&root).unwrap();
    let ambiguous = ok(
        &mut service,
        Request::Rename {
            name: "helper".into(),
            new_name: "aide".into(),
            file: None,
            uid: None,
            dry_run: true,
        },
    );
    assert_eq!(names(&ambiguous), ["helper@caller.rs", "helper@login.rs"]);
    assert_eq!(
        ambiguous["hint"],
        "ambiguous name; re-call with --file <path> or --uid <uid>"
    );

    let scoped = ok(
        &mut service,
        Request::Rename {
            name: "helper".into(),
            new_name: "aide".into(),
            file: Some("login.rs".into()),
            uid: None,
            dry_run: true,
        },
    );
    assert_eq!(scoped["symbol"]["path"], "login.rs");
    assert_eq!(scoped["dry_run"], true);

    let (_, message) = err(
        &mut service,
        Request::Rename {
            name: "go".into(),
            new_name: "run".into(),
            file: Some("login.rs".into()),
            uid: None,
            dry_run: true,
        },
    );
    assert_eq!(message, "no symbol named \"go\" in login.rs");

    let (_, message) = err(
        &mut service,
        Request::Rename {
            name: "go".into(),
            new_name: "run".into(),
            file: Some("missing.rs".into()),
            uid: None,
            dry_run: true,
        },
    );
    assert_eq!(message, "no indexed file matching 'missing.rs'");

    let (_, message) = err(
        &mut service,
        Request::Rename {
            name: "ignored".into(),
            new_name: "run".into(),
            file: None,
            uid: Some("rust:x.rs#ghost".into()),
            dry_run: true,
        },
    );
    assert_eq!(message, "no symbol with uid \"rust:x.rs#ghost\"");

    let (code, message) = err(
        &mut service,
        Request::Rename {
            name: "ghost".into(),
            new_name: "run".into(),
            file: None,
            uid: None,
            dry_run: true,
        },
    );
    assert_eq!(code, "NOT_FOUND");
    assert_eq!(message, "no symbol named \"ghost\"");
}

#[test]
fn rename_dry_run_should_list_the_edits_and_leave_every_file_untouched() {
    let root = fixture("rename-dry");
    let mut service = Service::open(&root).unwrap();
    let before_login = std::fs::read_to_string(root.join("login.rs")).unwrap();
    let before_caller = std::fs::read_to_string(root.join("caller.rs")).unwrap();
    let out = ok(
        &mut service,
        Request::Rename {
            name: "login".into(),
            new_name: "sign_in".into(),
            file: None,
            uid: None,
            dry_run: true,
        },
    );
    assert_eq!(out["old_name"], "login");
    assert_eq!(out["new_name"], "sign_in");
    let mut paths: Vec<&str> = out["edits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    paths.sort_unstable();
    assert_eq!(paths, ["caller.rs", "login.rs"]);
    assert!(out["edit_count"].as_u64().unwrap() >= 2, "{out}");
    assert!(out.get("applied").is_none(), "a dry run applies nothing");
    assert_eq!(
        std::fs::read_to_string(root.join("login.rs")).unwrap(),
        before_login
    );
    assert_eq!(
        std::fs::read_to_string(root.join("caller.rs")).unwrap(),
        before_caller
    );
}

#[test]
fn rename_should_rewrite_the_definition_and_its_call_sites_and_answer_from_the_new_tree() {
    let root = fixture("rename-apply");
    let mut service = Service::open(&root).unwrap();
    let out = ok(
        &mut service,
        Request::Rename {
            name: "login".into(),
            new_name: "sign_in".into(),
            file: None,
            uid: None,
            dry_run: false,
        },
    );
    assert!(
        out["applied"].is_number() || out["applied"].is_array(),
        "{out}"
    );
    let login = std::fs::read_to_string(root.join("login.rs")).unwrap();
    let caller = std::fs::read_to_string(root.join("caller.rs")).unwrap();
    assert!(login.contains("pub fn sign_in("), "{login}");
    assert!(caller.contains("sign_in(\"a\")"), "{caller}");
    let after = ok(
        &mut service,
        Request::Symbol {
            name: "sign_in".into(),
        },
    );
    assert_eq!(
        after["symbols"].as_array().map(Vec::len),
        Some(1),
        "the next op re-syncs instead of serving pre-rename rows: {after}"
    );
}

// -- notes -----------------------------------------------------------------------

#[test]
fn note_should_round_trip_and_key_absolute_and_dot_relative_paths_alike() {
    let root = fixture("notes");
    let mut service = Service::open(&root).unwrap();
    let note = |action: &str, file: Option<String>, target: Option<&str>, text: Option<&str>| {
        Request::Note {
            action: action.into(),
            file,
            target: target.map(String::from),
            note: text.map(String::from),
        }
    };
    let set = ok(
        &mut service,
        note(
            "set",
            Some(root.join("login.rs").display().to_string()),
            Some("login"),
            Some("checks only emptiness"),
        ),
    );
    assert_eq!(
        set,
        json!({"ok": true, "file": "login.rs", "target": "login", "note": "checks only emptiness"})
    );
    let got = ok(
        &mut service,
        note("get", Some("./login.rs".into()), Some("login"), None),
    );
    assert_eq!(got["note"], "checks only emptiness");

    let listed = ok(&mut service, note("list", None, None, None));
    assert_eq!(listed["total"], 1);
    assert_eq!(listed["capped"], false);
    let per_file = ok(
        &mut service,
        note("list", Some("caller.rs".into()), None, None),
    );
    assert_eq!(per_file["total"], 0, "notes are per file");

    let removed = ok(
        &mut service,
        note("rm", Some("login.rs".into()), Some("login"), None),
    );
    assert_eq!(removed["removed"], true);
    let again = ok(
        &mut service,
        note("delete", Some("login.rs".into()), Some("login"), None),
    );
    assert_eq!(again["removed"], false, "nothing left to remove");
    let gone = ok(
        &mut service,
        note("get", Some("login.rs".into()), Some("login"), None),
    );
    assert_eq!(gone["note"], Value::Null);
}

#[test]
fn note_should_name_the_arguments_each_action_requires() {
    let root = tmpdir("notes-usage");
    let mut service = Service::open(&root).unwrap();
    let cases = [
        (
            "set",
            Some("a.rs"),
            Some("t"),
            None,
            "note set requires <file> <target> <note>",
        ),
        (
            "set",
            Some(""),
            Some("t"),
            Some("n"),
            "note set requires <file> <target> <note>",
        ),
        (
            "get",
            Some("a.rs"),
            None,
            None,
            "note get requires <file> <target>",
        ),
        (
            "get",
            Some("a.rs"),
            Some(""),
            None,
            "note get requires <file> <target>",
        ),
        (
            "rm",
            None,
            Some("t"),
            None,
            "note rm requires <file> <target>",
        ),
        (
            "edit",
            None,
            None,
            None,
            "unknown note action 'edit' — expected set|get|rm|list",
        ),
    ];
    for (action, file, target, text, expected) in cases {
        let (code, message) = err(
            &mut service,
            Request::Note {
                action: action.into(),
                file: file.map(String::from),
                target: target.map(String::from),
                note: text.map(String::from),
            },
        );
        assert_eq!(code, "INVALID_INPUT");
        assert_eq!(message, expected, "{action} {file:?} {target:?}");
    }
}

// -- map ---------------------------------------------------------------------------

#[test]
fn map_should_list_files_in_path_order_and_render_the_markdown_projection() {
    let root = fixture("map");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/deep.rs"), "pub fn deep() {}\n").unwrap();
    let mut service = Service::open(&root).unwrap();

    let plain = ok(&mut service, Request::Map { markdown: false });
    let paths: Vec<&str> = plain["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["caller.rs", "login.rs", "sub/deep.rs"]);
    assert_eq!(plain["file_count"], 3);
    assert_eq!(plain["symbol_count"], 5);
    assert_eq!(plain["truncated"], false);
    assert!(plain.get("markdown").is_none());

    let md = ok(&mut service, Request::Map { markdown: true });
    let text = md["markdown"].as_str().unwrap();
    let root_name = root.file_name().unwrap().to_string_lossy();
    assert!(
        text.starts_with(&format!(
            "# pixel repo-map — {root_name}\n\n3 files · 5 symbols"
        )),
        "{text}"
    );
    assert!(text.contains("\n### `caller.rs`\n"), "{text}");
    assert!(text.contains("\n## sub\n"), "{text}");
    assert!(text.contains("\n### `sub/deep.rs`\n"), "{text}");
    assert!(
        text.contains("**deep** (L1–1)"),
        "each symbol names its line span: {text}"
    );
    assert!(!text.contains("truncated at"), "{text}");
}

// -- history-backed ops ----------------------------------------------------------------

#[test]
fn lifecycle_should_require_a_path_or_a_token() {
    let root = fixture("lifecycle");
    let mut service = Service::open(&root).unwrap();
    let (code, message) = err(
        &mut service,
        Request::Lifecycle {
            path: None,
            token: None,
        },
    );
    assert_eq!(code, "INVALID_INPUT");
    assert_eq!(message, "lifecycle requires a path or token");
}

#[test]
fn lifecycle_should_report_coverage_for_a_token_found_nowhere() {
    let root = fixture("lifecycle-token");
    let mut service = Service::open(&root).unwrap();
    let out = ok(
        &mut service,
        Request::Lifecycle {
            path: None,
            token: Some("never_written_token".into()),
        },
    );
    assert!(
        out.get("coverage").is_some_and(|c| !c.is_null()),
        "a token found nowhere says how much history was searched: {out}"
    );
    assert!(out["index_state"].is_object(), "{out}");

    let by_path = ok(
        &mut service,
        Request::Lifecycle {
            path: Some("login.rs".into()),
            token: None,
        },
    );
    assert!(
        by_path.get("coverage").is_none(),
        "a path answer carries no token coverage: {by_path}"
    );
}

#[test]
fn journal_should_record_the_event_with_whichever_fields_were_given() {
    let root = tmpdir("journal");
    let mut service = Service::open(&root).unwrap();
    let mut ids = Vec::new();
    for (path, detail) in [
        (Some("a.rs"), Some("edited")),
        (Some("a.rs"), None),
        (None, Some("note")),
        (None, None),
    ] {
        let out = ok(
            &mut service,
            Request::Journal {
                kind: "edit".into(),
                path: path.map(String::from),
                detail: detail.map(String::from),
            },
        );
        assert_eq!(out["recorded"], true);
        assert_eq!(out["kind"], "edit");
        ids.push(out["id"].clone());
    }
    ids.dedup();
    assert_eq!(ids.len(), 4, "each call records its own event: {ids:?}");
}

// -- evaluate argument parsing -----------------------------------------------------------

#[test]
fn evaluate_should_refuse_an_unknown_traversal_or_tier_instead_of_answering_another_question() {
    let root = fixture("evaluate-args");
    let mut service = Service::open(&root).unwrap();
    let request = |traversal: Option<&str>, tiers: Option<&str>| Request::Evaluate {
        from: "go".into(),
        to: "login".into(),
        traversal: traversal.map(String::from),
        tiers: tiers.map(String::from),
        max_depth: None,
        time_budget_ms: None,
        scope: None,
        at_snapshot: false,
    };
    let (_, message) = err(&mut service, request(Some("sideways"), None));
    assert_eq!(
        message,
        "evaluate: unknown --traversal \"sideways\" (callees | callers)"
    );
    let (_, message) = err(&mut service, request(None, Some("fuzzy")));
    assert_eq!(
        message,
        "evaluate: unknown --tiers \"fuzzy\" (exact | exact,probable)"
    );
}

#[test]
fn evaluate_request_parse_should_fill_the_documented_defaults() {
    let args = EvaluateRequest {
        from: "a".into(),
        to: "b".into(),
        traversal: Some("callers".into()),
        tiers: None,
        max_depth: None,
        time_budget_ms: None,
        scope: Some("src".into()),
        at_snapshot: true,
    }
    .parse()
    .unwrap();
    assert_eq!(args.traversal, wire::Traversal::Callers);
    assert_eq!(args.tiers, evaluate::TierSelection::Exact);
    assert_eq!(args.max_depth, DEFAULT_EVALUATE_MAX_DEPTH);
    assert_eq!(args.time_budget_ms, DEFAULT_EVALUATE_TIME_BUDGET_MS);
    assert_eq!(args.scope.as_deref(), Some("src"));
    assert!(args.at_snapshot);

    let explicit = EvaluateRequest {
        from: "a".into(),
        to: "b".into(),
        traversal: None,
        tiers: None,
        max_depth: Some(2),
        time_budget_ms: Some(9),
        scope: None,
        at_snapshot: false,
    }
    .parse()
    .unwrap();
    assert_eq!(
        explicit.traversal,
        wire::Traversal::Callees,
        "callees by default"
    );
    assert_eq!((explicit.max_depth, explicit.time_budget_ms), (2, 9));
}

// -- status and graph -----------------------------------------------------------------

#[test]
fn status_should_say_the_graph_is_absent_until_one_is_built() {
    let root = fixture("status");
    let mut service = Service::open(&root).unwrap();
    let before = ok(&mut service, Request::Status {});
    assert_eq!(before["graph"], json!({"present": false}));
    assert_eq!(before["root"], root.display().to_string());
    assert!(before["index"]["open"]["base"].is_string(), "{before}");
    assert_eq!(before["watcher"]["graph_update_failures"], 0);

    ok(&mut service, Request::Graph { if_stale: false });
    let after = ok(&mut service, Request::Status {});
    assert_eq!(after["graph"]["present"], true);
    assert_eq!(after["graph"]["files"], 2);
    assert_eq!(after["graph"]["symbols"], 4);
}

#[test]
fn status_should_report_a_graph_file_it_cannot_open() {
    let root = fixture("status-broken-graph");
    let mut service = Service::open(&root).unwrap();
    let db = service.graph_db_path();
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(&db, b"not a sqlite database at all").unwrap();
    let out = ok(&mut service, Request::Status {});
    assert_eq!(out["graph"]["present"], true);
    assert!(
        out["graph"]["error"]
            .as_str()
            .is_some_and(|e| !e.is_empty()),
        "{out}"
    );
}

#[test]
fn graph_if_stale_should_keep_a_fresh_graph_and_say_so() {
    let root = fixture("graph-if-stale");
    let mut service = Service::open(&root).unwrap();
    let full = ok(&mut service, Request::Graph { if_stale: false });
    assert_eq!(
        full["build"],
        json!({"mode": "full", "reason": "requested"})
    );
    assert_eq!(full["files"], 2);

    let kept = ok(&mut service, Request::Graph { if_stale: true });
    assert_eq!(kept["build"], json!({"mode": "fresh"}));
    assert_eq!(kept["files"], 2);
    assert!(kept["phases"]["check_ms"].is_u64(), "{kept}");
}

// -- pure helpers -----------------------------------------------------------------------

#[test]
fn is_retrieval_op_should_hold_for_exactly_the_listed_ops() {
    for op in RETRIEVAL_OPS {
        assert!(is_retrieval_op(op), "{op}");
    }
    for op in ["ping", "publish", "status", "note", "map", "searc", ""] {
        assert!(!is_retrieval_op(op), "{op}");
    }
}

#[test]
fn regex_escape_keyword_should_escape_ascii_metacharacters_only() {
    assert_eq!(regex_escape_keyword("snake_case9"), "snake_case9");
    assert_eq!(regex_escape_keyword("a.b*c"), "a\\.b\\*c");
    assert_eq!(regex_escape_keyword("(x|y)"), "\\(x\\|y\\)");
    assert_eq!(
        regex_escape_keyword("café"),
        "café",
        "non-ASCII stays literal"
    );
}

#[test]
fn normalize_file_arg_should_make_a_path_repo_relative_with_forward_slashes() {
    let root = Path::new("/repo");
    assert_eq!(normalize_file_arg(root, "/repo/src/a.rs"), "src/a.rs");
    assert_eq!(normalize_file_arg(root, "/src/a.rs"), "src/a.rs");
    assert_eq!(normalize_file_arg(root, "src\\a.rs"), "src/a.rs");
    assert_eq!(normalize_file_arg(root, "src/a.rs"), "src/a.rs");
}

#[test]
fn merge_build_info_should_attach_a_build_only_when_there_is_one_and_an_object_to_hold_it() {
    let mut out = json!({"a": 1});
    merge_build_info(&mut out, None);
    assert_eq!(out, json!({"a": 1}));
    merge_build_info(&mut out, Some(json!({"build_ms": 3})));
    assert_eq!(out, json!({"a": 1, "graph_build": {"build_ms": 3}}));
    let mut scalar = json!(7);
    merge_build_info(&mut scalar, Some(json!({"build_ms": 3})));
    assert_eq!(scalar, json!(7));
}

#[test]
fn remove_sqlite_files_should_remove_the_database_and_both_sidecars_and_accept_their_absence() {
    let dir = tmpdir("sqlite-files");
    let db = dir.join("graph.db");
    for path in [
        db.clone(),
        sqlite_sidecar(&db, "-wal"),
        sqlite_sidecar(&db, "-shm"),
    ] {
        std::fs::write(&path, b"x").unwrap();
    }
    assert_eq!(sqlite_sidecar(&db, "-wal"), dir.join("graph.db-wal"));
    remove_sqlite_files(&db).unwrap();
    for name in ["graph.db", "graph.db-wal", "graph.db-shm"] {
        assert!(!dir.join(name).exists(), "{name}");
    }
    remove_sqlite_files(&db).unwrap();
    remove_sqlite_sidecars(&db).unwrap();
}

#[test]
fn remove_sqlite_files_should_report_a_path_it_cannot_remove() {
    let dir = tmpdir("sqlite-files-dir");
    let db = dir.join("graph.db");
    std::fs::create_dir_all(&db).unwrap();
    let error = remove_sqlite_files(&db).unwrap_err();
    assert!(
        error.starts_with(&format!("remove {}: ", db.display())),
        "{error}"
    );
    std::fs::remove_dir_all(&db).unwrap();
    std::fs::create_dir_all(sqlite_sidecar(&db, "-wal")).unwrap();
    let error = remove_sqlite_sidecars(&db).unwrap_err();
    assert!(error.contains("graph.db-wal"), "{error}");
}

#[test]
fn cosine_sim_should_be_zero_for_mismatched_empty_or_null_vectors() {
    assert!((cosine_sim(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    assert!(cosine_sim(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    assert!((cosine_sim(&[1.0, 0.0], &[-2.0, 0.0]) + 1.0).abs() < 1e-6);
    assert!(
        (cosine_sim(&[3.0, 4.0], &[6.0, 8.0]) - 1.0).abs() < 1e-6,
        "scale-free"
    );
    assert_eq!(cosine_sim(&[1.0], &[1.0, 2.0]), 0.0);
    assert_eq!(cosine_sim(&[], &[]), 0.0);
    assert_eq!(cosine_sim(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
}

#[test]
fn search_signal_words_should_split_alternatives_and_spaces_but_not_invent_regex_letters() {
    assert_eq!(
        search_signal_words("gain ledger|fooBar"),
        ["gain", "ledger", "foo", "bar"]
    );
    assert_eq!(search_signal_words("   "), Vec::<String>::new());
}

#[test]
fn derive_epistemics_should_name_each_cap_marker_the_result_carries() {
    let (e, w) = derive_epistemics("search", &json!({"truncated": true, "next_offset": 20}));
    assert_eq!(
        w.iter().map(|w| w.message.as_str()).collect::<Vec<_>>(),
        ["results truncated by row/byte cap; more exist — continue via next_offset"]
    );
    assert!(w.iter().all(|w| w.code == "RESULT_CAPPED"));
    assert!(e.lower_bound);
    assert!(e.basis.starts_with("text index; caps: "), "{}", e.basis);

    let (_, w) = derive_epistemics("uses", &json!({"truncated": true, "next_offset": null}));
    assert_eq!(w[0].message, "results truncated by an output cap");

    let (_, w) = derive_epistemics(
        "search",
        &json!({"truncated": true, "caps": ["row cap truncated results"]}),
    );
    assert_eq!(
        w.len(),
        1,
        "a named truncation cap is not repeated generically"
    );

    let (e, w) = derive_epistemics(
        "impact",
        &json!({"envelope": {"lower_bound": true, "unresolved_same_name": 2}}),
    );
    assert_eq!(
        w[0].message,
        "graph lower bound: 2 unresolved same-name call site(s) — edges beyond this answer may exist"
    );
    assert!(e.lower_bound);

    let (_, w) = derive_epistemics("impact", &json!({"envelope": {"lower_bound": true}}));
    assert_eq!(
        w[0].message,
        "graph lower bound: resolver could not close the world"
    );

    let (_, w) = derive_epistemics(
        "impact",
        &json!({"envelope": {"lower_bound": true, "caps": ["named cap"]}}),
    );
    assert_eq!(
        w.iter().map(|w| w.message.as_str()).collect::<Vec<_>>(),
        ["named cap"],
        "a named cap already explains the lower bound"
    );

    let (_, w) = derive_epistemics("resolve", &json!({"scan_capped": true}));
    assert_eq!(
        w[0].message,
        "fallback table scan hit its row cap; unscanned rows were never considered"
    );
}

#[test]
fn derive_epistemics_should_name_the_source_basis_staleness_and_confidence() {
    let (e, w) = derive_epistemics(
        "resolve",
        &json!({"basis": "tier 2", "confidence": "ranked", "graph_build": {"build_ms": 40}}),
    );
    assert!(w.is_empty());
    assert!(!e.lower_bound);
    assert!(!e.closed_world, "static analysis is never complete");
    assert!(
        e.basis
            .starts_with("text index + code graph; tier 2; static analysis cannot guarantee"),
        "{}",
        e.basis
    );
    assert_eq!(e.staleness_ms, Some(0), "rebuilt for this answer");
    assert_eq!(e.confidence.as_deref(), Some("ranked"));
    assert_eq!(e.extraction_limits.len(), 4);

    let (e, _) = derive_epistemics("targets", &json!({"envelope": {"confidence": "likely"}}));
    assert_eq!(e.confidence.as_deref(), Some("likely"));
    assert_eq!(e.staleness_ms, None, "never guessed");

    for (op, source) in [
        ("changes", "code graph + working-tree diff"),
        ("review_gate", "code graph + working-tree diff"),
        ("targets", "text index + code graph"),
        ("context", "code graph"),
    ] {
        let (e, _) = derive_epistemics(op, &json!({}));
        assert!(
            e.basis.starts_with(&format!("{source};")),
            "{op}: {}",
            e.basis
        );
    }
}

#[test]
fn fan_in_counts_should_count_incoming_calls_per_candidate_file() {
    let root = fixture("fan-in");
    let mut service = Service::open(&root).unwrap();
    service.ensure_graph().unwrap();
    let store = service.graph.as_ref().unwrap();
    assert!(fan_in_counts(store.conn(), &[]).is_empty());
    let counts = fan_in_counts(
        store.conn(),
        &["login.rs".to_string(), "caller.rs".to_string()],
    );
    assert_eq!(counts.get("login.rs"), Some(&1), "`go` calls `login`");
    assert_eq!(counts.get("caller.rs"), None, "no call reaches caller.rs");
}
