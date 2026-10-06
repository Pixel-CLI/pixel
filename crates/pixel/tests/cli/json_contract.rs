// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! CLI output contract for `--json`.
//!
//! Agents parse `pixel … --json` stdout with a JSON parser. The contract
//! this file pins down is the one they rely on:
//!
//! - stdout is JSON and nothing else: one document per line (a single
//!   document for most commands, NDJSON for `search`), no prose, no notes;
//! - human-facing notes (graph build announcements, caveats) go to stderr;
//! - a failing command exits non-zero and writes the reason to stderr. Under
//!   `--json` stdout carries one failure envelope (`ok: false`, `error.code`,
//!   the same reason) so a parser reads the code instead of the prose; in
//!   human mode stdout stays empty, so a script that did not ask for JSON
//!   never gets a document.
//!
//! Each command runs against the in-process service (`PIXEL_DAEMON_AUTO_START=0`)
//! so the test does not depend on, or leave behind, a background daemon.

use std::path::Path;
use std::process::{Command, Output};

use crate::support::{Scratch, pixel_command};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn pixel(dir: &Path, args: &[&str]) -> Output {
    pixel_command()
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn fixture(tag: &str) -> Scratch {
    let dir = Scratch::for_test("pixel-json-contract", tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/login.rs"),
        "pub fn login_user(name: &str) -> bool {\n    !name.is_empty()\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/caller.rs"),
        "use crate::login::login_user;\npub fn go() { login_user(\"a\"); }\n",
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), ".pixel/\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "fixture"]);
    dir
}

/// The standard fixture plus a file with enough matches that `--context 20`
/// makes the enriched page outgrow a small `PIXEL_OUTPUT_CAP_BYTES` long
/// before the daemon's own 64 KiB byte cap.
fn fixture_with_many_matches(tag: &str) -> Scratch {
    let dir = fixture(tag);
    let terms: Vec<String> = (1..=200)
        .map(|n| format!("// the line {n} mentions the search term"))
        .collect();
    std::fs::write(dir.join("src/terms.rs"), format!("{}\n", terms.join("\n"))).unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "terms"]);
    dir
}

/// `pixel search-content the . --limit <limit> --json` against `dir` under a
/// stdout cap, optionally with `--context <context>`.
fn capped_search(dir: &Path, cap: usize, limit: usize, context: Option<usize>) -> Output {
    let cap = cap.to_string();
    let limit = limit.to_string();
    let mut cmd = pixel_command();
    cmd.args([
        "search-content",
        "the",
        ".",
        "--limit",
        limit.as_str(),
        "--json",
    ])
    .current_dir(dir)
    .env("PIXEL_OUTPUT_CAP_BYTES", cap.as_str());
    if let Some(context) = context {
        cmd.args(["--context", &context.to_string()]);
    }
    cmd.output().unwrap()
}

/// Every non-empty stdout line must parse as a JSON value on its own.
/// Returns the parsed documents so callers can assert on content.
fn parse_stdout_lines(out: &Output, what: &str) -> Vec<serde_json::Value> {
    let stdout = String::from_utf8(out.stdout.clone())
        .unwrap_or_else(|e| panic!("{what}: stdout is not UTF-8: {e}"));
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|e| {
                panic!(
                    "{what}: stdout line is not JSON ({e}):\n{line}\n--- stderr:\n{}",
                    String::from_utf8_lossy(&out.stderr)
                )
            })
        })
        .collect()
}

#[test]
fn json_commands_emit_only_json_on_stdout() {
    let dir = fixture("ok");

    // (argv, expected top-level keys on the single document)
    let single_doc: &[(&[&str], &[&str])] = &[
        (&["status", ".", "--json"], &[]),
        (&["find-symbol", "login_user", ".", "--json"], &[]),
        (&["impact", "login_user", ".", "--json"], &["epistemics"]),
        (
            &[
                "scope-task",
                "fix login_user",
                ".",
                "--json",
                "--no-manifest",
            ],
            &["targets", "epistemics"],
        ),
        (&["repo-state", ".", "--json"], &["head", "branch"]),
    ];
    for (argv, keys) in single_doc {
        let out = pixel(&dir, argv);
        let what = argv.join(" ");
        assert!(
            out.status.success(),
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let docs = parse_stdout_lines(&out, &what);
        assert_eq!(
            docs.len(),
            1,
            "{what}: expected exactly one JSON document, got {docs:?}"
        );
        assert!(docs[0].is_object(), "{what}: top-level must be an object");
        for k in *keys {
            assert!(
                docs[0].get(k).is_some(),
                "{what}: missing key {k:?} in {}",
                docs[0]
            );
        }
    }

    // `search --json` is NDJSON: one match object per line, then the final
    // page-metadata line, which is the only one without a `path`.
    let out = pixel(&dir, &["search-content", "login_user", ".", "--json"]);
    assert!(
        out.status.success(),
        "search: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let docs = parse_stdout_lines(&out, "search --json");
    assert!(!docs.is_empty(), "search must find the fixture symbol");
    let (meta, matches) = docs.split_last().unwrap();
    assert!(
        meta.get("truncated").is_some(),
        "the last line is the page metadata: {meta}"
    );
    assert!(meta.get("path").is_none(), "{meta}");
    for d in matches {
        assert!(d.get("path").is_some(), "unexpected line: {d}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A failure under `--json` answers with the failure envelope: `ok: false`,
/// `error.code`, the same reason stderr carries, exit 1. The code is what an
/// agent branches on — `NOT_FOUND` means "widen the query", `INVALID_INPUT`
/// means "the call itself is wrong" — and it comes from the daemon's own
/// classifier, the one that put a code on the wire for this message. Human
/// mode is unchanged: same failure, no `--json`, stdout empty.
#[test]
fn failing_json_command_answers_with_a_failure_envelope() {
    let dir = fixture("fail");

    // A name that resolves to nothing: `resolve_symbol` answers
    // "no symbol named …", which classifies as NOT_FOUND.
    let out = pixel(&dir, &["impact", "no_such_symbol_anywhere", ".", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let docs = parse_stdout_lines(&out, "impact unknown");
    assert_eq!(docs.len(), 1, "{docs:?}");
    assert_eq!(docs[0]["ok"], false, "{docs:?}");
    assert_eq!(docs[0]["op"], "impact", "{docs:?}");
    assert_eq!(docs[0]["error"]["code"], "NOT_FOUND", "{docs:?}");
    let reason = docs[0]["error"]["message"]
        .as_str()
        .expect("failure envelope carries a message")
        .to_string();
    assert!(reason.contains("no_such_symbol_anywhere"), "{reason}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with("pixel: "), "{stderr}");
    assert!(
        stderr.contains(&reason),
        "stderr keeps the reason: {stderr}"
    );

    // A malformed regex is the request's fault, not a missing thing: the
    // envelope still answers, with the default code.
    let out = pixel(&dir, &["search-content", "(", ".", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let docs = parse_stdout_lines(&out, "malformed regex");
    assert_eq!(docs.len(), 1, "{docs:?}");
    assert_eq!(docs[0]["ok"], false, "{docs:?}");
    assert_eq!(docs[0]["error"]["code"], "INVALID_INPUT", "{docs:?}");

    // Human mode is untouched by the contract change: the same two failures
    // without `--json` keep stdout empty and the reason on stderr.
    let out = pixel(&dir, &["impact", "no_such_symbol_anywhere", "."]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&reason),
        "{out:?}"
    );
    let out = pixel(&dir, &["search-content", "(", "."]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");

    // A command that owns stdout keeps it: the statusline is read by a shell
    // prompt, so a failure there must not become a JSON document. The same
    // failure without `--statusline` answers with the envelope.
    let missing = dir.join("not-a-repo").display().to_string();
    let out = pixel(&dir, &["status", &missing, "--statusline", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let out = pixel(&dir, &["status", &missing, "--json"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let docs = parse_stdout_lines(&out, "status bad path");
    assert_eq!(docs[0]["ok"], false, "{docs:?}");
    assert_eq!(docs[0]["error"]["code"], "INVALID_INPUT", "{docs:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A capped `search --json` page must describe itself: the last NDJSON line
/// carries the page state (`truncated`, `next_offset`) and the envelope's
/// `epistemics`/`warnings`, because a page cut short is otherwise
/// byte-for-byte a complete answer. The cap counts the `--context` text the
/// CLI adds *after* the daemon's own byte cap — the 154 KB page measured
/// under an 8 KB cap.
#[test]
fn search_json_page_ends_with_the_state_of_the_page_it_printed() {
    let dir = fixture_with_many_matches("search-page-meta");
    let cap = 8192;

    // `--context 20` makes each match ~2 KB, so an 8 KB cap cuts the page long
    // before the daemon's 64 KB byte cap: only the trailer can say so.
    let out = capped_search(&dir, cap, 300, Some(20));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.len() <= cap + 1,
        "stdout must respect PIXEL_OUTPUT_CAP_BYTES ({cap}): {} bytes",
        out.stdout.len()
    );
    let docs = parse_stdout_lines(&out, "capped search --json --context 20");
    let (meta, matches) = docs.split_last().expect("the metadata line at least");
    assert!(
        meta.get("path").is_none(),
        "the last line is metadata: {meta}"
    );
    assert!(!matches.is_empty(), "the cap leaves room for matches");
    assert_eq!(meta["truncated"], true, "{meta}");
    assert_eq!(
        meta["next_offset"].as_u64(),
        Some(matches.len() as u64),
        "the page resumes at the first match it did not print: {meta}"
    );
    assert!(meta["epistemics"].is_object(), "{meta}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("stdout cap (PIXEL_OUTPUT_CAP_BYTES)"),
        "the cut is named on stderr: {stderr}"
    );

    // Without `--context` the rows are small enough to fill the cap tightly:
    // the reserved metadata line stays inside the cap too.
    let tight = capped_search(&dir, cap, 300, None);
    assert!(tight.status.success(), "{tight:?}");
    assert!(
        tight.stdout.len() <= cap + 1,
        "the reserved trailer must stay inside the cap: {} bytes",
        tight.stdout.len()
    );
    let docs = parse_stdout_lines(&tight, "capped search --json");
    let (meta, matches) = docs.split_last().unwrap();
    assert_eq!(meta["truncated"], true, "{meta}");
    assert_eq!(
        meta["next_offset"].as_u64(),
        Some(matches.len() as u64),
        "{meta}"
    );

    // The same page without a cap is complete, and says so: the control that
    // the daemon was not the one truncating the pages above.
    let full = capped_search(&dir, 0, 300, Some(20));
    assert!(full.status.success(), "{full:?}");
    assert!(full.stdout.len() > cap, "the cap really cut something");
    let docs = parse_stdout_lines(&full, "uncapped search --json --context 20");
    let (meta, matches) = docs.split_last().unwrap();
    assert_eq!(matches.len(), 200, "every match is in the page");
    assert_eq!(meta["truncated"], false, "{meta}");
    assert!(meta["next_offset"].is_null(), "{meta}");

    // A page the daemon itself capped keeps the daemon's resume offset, and
    // the stderr line names the daemon's row cap rather than the stdout cap.
    let limited = capped_search(&dir, cap, 5, None);
    assert!(limited.status.success(), "{limited:?}");
    let docs = parse_stdout_lines(&limited, "row-capped search --json");
    let (meta, matches) = docs.split_last().unwrap();
    assert_eq!(matches.len(), 5, "{docs:?}");
    assert_eq!(meta["truncated"], true, "{meta}");
    assert_eq!(meta["next_offset"].as_u64(), Some(5), "{meta}");
    let stderr = String::from_utf8_lossy(&limited.stderr);
    assert!(
        stderr.contains("row limit 5"),
        "the daemon's cap is named on stderr: {stderr}"
    );
    assert!(
        !stderr.contains("stdout cap"),
        "the stdout cap did not fire: {stderr}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Human `search` output keeps its rendering and its stderr warning: the
/// stdout cap cuts between rows, and the note names both how many rows were
/// written and the offset that resumes the page.
#[test]
fn capped_search_human_output_names_the_stdout_cap() {
    let dir = fixture_with_many_matches("search-page-human");
    let cap = 8192usize;
    let cap_arg = cap.to_string();
    let out = pixel_command()
        .args([
            "search-content",
            "the",
            ".",
            "--limit",
            "300",
            "--context",
            "20",
        ])
        .current_dir(&dir)
        .env("PIXEL_OUTPUT_CAP_BYTES", cap_arg.as_str())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(
        out.stdout.len() <= cap + 1,
        "human output respects the cap too: {} bytes",
        out.stdout.len()
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("--- "),
        "the context block rendering is unchanged: {}",
        text.chars().take(80).collect::<String>()
    );
    assert!(!text.contains("\"path\":"), "human mode stays human");
    let blocks = text.matches("--- ").count();
    assert!(blocks > 0, "some matches fit under the cap: {text}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("wrote {blocks} of ")),
        "the note names how many rows were written ({blocks}): {stderr}"
    );
    assert!(
        stderr.contains("stdout cap (PIXEL_OUTPUT_CAP_BYTES)"),
        "and which cap cut the page: {stderr}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// `prepare-repo` says where its time went: CI reads `.timings` to tell a
/// reused index from a rebuilt one and to name the slow graph phase, and a
/// human gets the same answer on one line.
#[test]
fn prepare_repo_reports_where_the_time_went() {
    let dir = fixture("timings");
    // Every fixture commits the same tree, so the shard cache under the
    // shared test HOME may already hold this base: its own cache keeps the
    // first open a build from git.
    let cache = Scratch::for_test("pixel-json-contract", "timings-cache");
    let pixel = |dir: &Path, args: &[&str]| {
        pixel_command()
            .args(args)
            .current_dir(dir)
            .env("XDG_CACHE_HOME", cache.as_os_str())
            .output()
            .unwrap()
    };
    let out = pixel(&dir, &["prepare-repo", ".", "--json", "--no-daemon"]);
    assert!(out.status.success(), "{out:?}");
    let docs = parse_stdout_lines(&out, "prepare-repo --json");
    let timings = &docs[0]["timings"];
    assert_eq!(timings["index"]["base"], "built_from_git", "{timings}");
    assert!(timings["total_ms"].is_u64(), "{timings}");
    assert!(
        timings["graph"]["phases"]["extract_ms"].is_u64(),
        "{timings}"
    );
    assert!(
        timings["graph"]["phases"]["publish_ms"].is_u64(),
        "{timings}"
    );
    assert_eq!(
        timings["graph"]["build"],
        serde_json::json!({"mode": "full", "reason": "missing"}),
        "{timings}"
    );
    for moved in ["phases", "build"] {
        assert!(
            docs[0]["graph"].get(moved).is_none(),
            "{moved}: moved, not copied"
        );
    }
    assert!(docs[0]["index"].get("open").is_none(), "moved, not copied");

    let out = pixel(&dir, &["prepare-repo", ".", "--no-daemon"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.lines().any(|l| l.starts_with("timings: total ")
            && l.contains("(base reused)")
            && l.contains("(fresh)")),
        "the second run reuses the base and the graph it built: {text}"
    );

    let out = pixel(
        &dir,
        &[
            "prepare-repo",
            ".",
            "--json",
            "--no-daemon",
            "--rebuild-graph",
        ],
    );
    assert!(out.status.success(), "{out:?}");
    let docs = parse_stdout_lines(&out, "prepare-repo --rebuild-graph");
    assert_eq!(
        docs[0]["timings"]["graph"]["build"],
        serde_json::json!({"mode": "full", "reason": "requested"})
    );
    assert!(
        docs[0]["graph"]["symbols"].as_u64() > Some(0),
        "{}",
        docs[0]
    );
}

/// A repo with a big untracked tree (a `vendor/bundle`) is the everyday
/// case that used to break the contract: `status`/`ready` embedded the full
/// dirty list, blew the 256 KB cap, and the whole answer degraded to a
/// `{partial: "..."}` wrapper — `jq .index` stopped working on a command
/// whose job is only to say "index and graph are ready". The freshness
/// answers must stay small (a count, never the list), and the commands that
/// legitimately return the list must be cut structurally: valid JSON, every
/// scalar field intact, the list shortened and the cut named.
#[test]
fn big_untracked_tree_keeps_json_answers_structured() {
    let dir = fixture("bigdirty");
    for i in 0..3000 {
        let d = dir.join(format!("vendor/bundle/gems/g{}", i / 100));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(format!("f{i}.txt")), "x").unwrap();
    }

    for args in [
        &["status", ".", "--json"][..],
        &["prepare-repo", ".", "--json", "--no-daemon"][..],
    ] {
        let out = pixel(&dir, args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        let docs = parse_stdout_lines(&out, &format!("{args:?}"));
        assert_eq!(docs.len(), 1);
        let doc = &docs[0];
        assert!(
            out.stdout.len() < 4096,
            "{args:?}: freshness answer must not scale with the dirty tree ({} bytes)",
            out.stdout.len()
        );
        assert!(doc.get("truncated").is_none(), "{args:?}: {doc}");
        assert!(doc["index"].is_object(), "{args:?}: {doc}");
        let dirty_count = if args[0] == "status" {
            assert!(doc["snapshot"].get("dirty").is_none(), "{args:?}: {doc}");
            doc["snapshot"]["dirty_count"].as_u64()
        } else {
            assert!(
                doc.get("status").is_none(),
                "prepare-repo must not embed status"
            );
            doc["dirty_count"].as_u64()
        };
        assert_eq!(dirty_count, Some(3000), "{args:?}: {doc}");
    }

    // Graph/retrieval answers are computed AGAINST a tree state, they do
    // not report it: the snapshot names HEAD/branch and counts the dirty
    // paths. Before this, `symbol`/`resolve` on a CI checkout with an
    // untracked `vendor/bundle` weighed 238 KB each, all of it path list.
    for args in [
        &["find-symbol", "login_user", ".", "--json"][..],
        &["find-code", "login user", ".", "--json"][..],
        &["impact", "login_user", ".", "--json"][..],
        &[
            "who-calls",
            "login_user",
            ".",
            "--role",
            "callers",
            "--json",
        ][..],
        &["what-changed", ".", "--json"][..],
    ] {
        let out = pixel(&dir, args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        let docs = parse_stdout_lines(&out, &format!("{args:?}"));
        assert_eq!(docs.len(), 1, "{args:?}");
        let doc = &docs[0];
        assert!(
            out.stdout.len() < 4096,
            "{args:?}: graph answer must not scale with the dirty tree ({} bytes)",
            out.stdout.len()
        );
        // `truncated` here would be the output-cap wrapper (`uses` carries
        // its own pagination `truncated: false`, which is fine).
        assert_ne!(doc["truncated"], true, "{args:?}: {doc}");
        assert!(doc.get("cap_bytes").is_none(), "{args:?}: {doc}");
        assert!(
            doc["snapshot"].get("dirty").is_none(),
            "{args:?}: snapshot must not enumerate dirty paths: {doc}"
        );
        assert_eq!(
            doc["snapshot"]["dirty_count"].as_u64(),
            Some(3000),
            "{args:?}: {doc}"
        );
        assert!(doc["snapshot"]["head"].is_string(), "{args:?}: {doc}");
    }

    // `inspect` owns the list: under a small cap it is shortened, not
    // replaced by a textual wrapper.
    let out = pixel_command()
        .args(["repo-state", ".", "--json"])
        .current_dir(&dir)
        .env("PIXEL_OUTPUT_CAP_BYTES", "4096")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.len() <= 4097, "{}", out.stdout.len());
    let doc = &parse_stdout_lines(&out, "inspect capped")[0];
    assert_eq!(doc["truncated"], true);
    assert_eq!(doc["cap_bytes"], 4096);
    assert!(
        doc.get("partial").is_none(),
        "structural cut, not wrapper: {doc}"
    );
    assert_eq!(doc["branch"].as_str().map(str::is_empty), Some(false));
    let cut = &doc["truncated_arrays"][0];
    assert_eq!(cut["path"], "dirty");
    assert_eq!(cut["total"], 3000);
    assert_eq!(
        cut["kept"].as_u64(),
        doc["dirty"].as_array().map(|a| a.len() as u64)
    );
    assert!(cut["kept"].as_u64().unwrap() > 0);

    // `0` lifts the cap: the full list comes back and nothing is flagged.
    let out = pixel_command()
        .args(["repo-state", ".", "--json"])
        .current_dir(&dir)
        .env("PIXEL_OUTPUT_CAP_BYTES", "0")
        .output()
        .unwrap();
    let doc = &parse_stdout_lines(&out, "inspect uncapped")[0];
    assert!(doc.get("truncated").is_none(), "{doc}");
    assert_eq!(doc["dirty"].as_array().map(Vec::len), Some(3000));

    std::fs::remove_dir_all(&dir).ok();
}

/// `review-gate` is a retrieval-class answer: one JSON document, an
/// array of findings each carrying its witness, epistemics and snapshot in
/// the envelope, and no secret echoed back in the finding's text.
#[test]
fn review_gate_json_contract_is_an_enveloped_finding_list() {
    let dir = fixture("review-gate");
    // Change `login_user` and append a credential-shaped line in the same
    // file. `src/caller.rs` — which calls `login_user` — is untouched, so
    // the divergence rule must anchor at the changed site and name the
    // untouched reader's location in its evidence, never list the reader's
    // file as the finding's own.
    std::fs::write(
        dir.join("src/login.rs"),
        "pub fn login_user(name: &str) -> bool {\n    name.len() > 0\n}\nconst LEAK: &str = \"ghp_1234567890abcdef\";\n",
    )
    .unwrap();

    let out = pixel(&dir, &["review-gate", ".", "--json"][..]);
    assert!(out.status.success(), "review-gate: {out:?}");
    let doc = &parse_stdout_lines(&out, "review-gate")[0];
    assert!(
        out.stdout.len() < 4096,
        "review-gate output must stay bounded"
    );
    // The CLI prints the envelope's `result` payload, envelope metadata
    // folded in: the caps array (empty here), the graph build, and the
    // freshest signal the graph side can attest.
    assert!(doc["snapshot"]["head"].is_string(), "{doc}");

    let findings = doc["findings"].as_array().expect("findings array");
    let divergence: Vec<&serde_json::Value> = findings
        .iter()
        .filter(|f| f["rule"] == "producer-reader-divergence")
        .collect();
    assert_eq!(divergence.len(), 1, "{doc}");
    assert_eq!(divergence[0]["file"], "src/login.rs", "{doc}");
    assert_eq!(divergence[0]["line"], 1, "{doc}");
    assert_eq!(divergence[0]["severity"], "MEDIUM", "{doc}");
    assert!(
        divergence[0]["evidence"]
            .as_str()
            .unwrap_or_default()
            .contains("src/caller.rs:2"),
        "the untouched reader's site is named in the evidence: {doc}"
    );

    let secret: Vec<&serde_json::Value> = findings
        .iter()
        .filter(|f| f["rule"] == "possible-secret")
        .collect();
    assert_eq!(secret.len(), 1, "{doc}");
    assert_eq!(secret[0]["severity"], "CRITICAL", "{doc}");
    assert!(
        !doc.to_string().contains("ghp_"),
        "the JSON response must not echo the matched credential"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// `repo-state` is the Phase-1 freshness answer: the tracked-clean file list
/// is the bulk of it on a clean tree and no consumer reads it. The default
/// answer keeps the exact counts, `--files` keeps restricting them, and
/// `--include-clean` keeps the full (capped) form reachable.
#[test]
fn repo_state_should_hide_the_clean_list_unless_include_clean_is_passed() {
    let dir = fixture("repo-state-compact");
    // 2 tracked-clean files (caller.rs, .gitignore), 1 dirty (login.rs).
    std::fs::write(
        dir.join("src/login.rs"),
        "pub fn login_user(name: &str) -> bool {\n    name.is_empty()\n}\n",
    )
    .unwrap();

    let out = pixel(&dir, &["repo-state", ".", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let doc = &parse_stdout_lines(&out, "repo-state compact")[0];
    assert!(doc.get("clean").is_none(), "compact answer: {doc}");
    assert!(doc.get("clean_list_cap").is_none(), "compact answer: {doc}");
    assert!(
        doc.get("clean_list_truncated").is_none(),
        "compact answer: {doc}"
    );
    assert_eq!(doc["clean_count"], 2, "{doc}");
    assert_eq!(doc["dirty_count"], 1, "{doc}");
    assert!(
        doc["dirty"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["path"] == "src/login.rs"),
        "the dirty list stays: {doc}"
    );

    // `--include-clean` restores the list, the cap and the truncation flag.
    let out = pixel(&dir, &["repo-state", ".", "--json", "--include-clean"]);
    assert!(out.status.success(), "{out:?}");
    let doc = &parse_stdout_lines(&out, "repo-state --include-clean")[0];
    let clean = doc["clean"].as_array().unwrap();
    assert_eq!(clean.len(), 2, "{doc}");
    assert!(clean.iter().any(|p| p == "src/caller.rs"), "{doc}");
    assert_eq!(doc["clean_count"], 2);
    assert_eq!(doc["clean_list_cap"], 200);
    assert_eq!(doc["clean_list_truncated"], false);

    // `--files` restricts both lists and both counts; the compact form keeps
    // the filtered `clean_count` without the list.
    let out = pixel(
        &dir,
        &[
            "repo-state",
            ".",
            "--json",
            "--files",
            "src/login.rs",
            "--files",
            "src/caller.rs",
        ],
    );
    assert!(out.status.success(), "{out:?}");
    let doc = &parse_stdout_lines(&out, "repo-state --files")[0];
    assert!(doc.get("clean").is_none(), "{doc}");
    assert_eq!(doc["clean_count"], 1, "{doc}");
    assert_eq!(doc["dirty_count"], 1, "{doc}");

    std::fs::remove_dir_all(&dir).ok();
}

/// `CLEAN_LIST_CAP` bounds the opt-in list: 200 of 201 paths, the exact
/// `clean_count`, and the truncation flag naming the cut.
#[test]
fn repo_state_should_cap_the_include_clean_list_at_200_paths() {
    let dir = Scratch::for_test("pixel-json-contract", "repo-state-clean-cap");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    for i in 0..201 {
        std::fs::write(
            dir.join(format!("src/f{i}.rs")),
            format!("pub fn f{i}() -> u32 {{ {i} }}\n"),
        )
        .unwrap();
    }
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "201 files"]);

    let out = pixel(&dir, &["repo-state", ".", "--json", "--include-clean"]);
    assert!(out.status.success(), "{out:?}");
    let doc = &parse_stdout_lines(&out, "repo-state clean cap")[0];
    assert_eq!(doc["clean_count"], 201, "{doc}");
    assert_eq!(doc["clean_list_cap"], 200);
    assert_eq!(doc["clean_list_truncated"], true);
    assert_eq!(doc["clean"].as_array().map(Vec::len), Some(200));

    // The default answer stays compact on the same tree.
    let out = pixel(&dir, &["repo-state", ".", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let doc = &parse_stdout_lines(&out, "repo-state clean cap compact")[0];
    assert!(doc.get("clean").is_none(), "{doc}");
    assert_eq!(doc["clean_count"], 201, "{doc}");

    std::fs::remove_dir_all(&dir).ok();
}

/// The statusline shows `indexed/total` commits only when the history index
/// knows about commits: a repository without any must not print `0/0`.
#[test]
fn statusline_reports_the_commit_fraction_only_when_there_are_commits() {
    let dir = fixture("statusline-commits");
    let indexed = pixel(&dir, &["build-index", "--history", "."]);
    assert!(indexed.status.success(), "{indexed:?}");
    let out = pixel(&dir, &["status", ".", "--statusline"]);
    assert!(out.status.success(), "{out:?}");
    let line = String::from_utf8_lossy(&out.stdout);
    assert!(line.contains(" 1/1"), "one commit indexed of one: {line}");

    let empty = Scratch::for_test("pixel-json-contract", "statusline-empty");
    std::fs::write(empty.join(".gitignore"), ".pixel/\n").unwrap();
    git(&empty, &["init", "-q"]);
    let _ = pixel(&empty, &["build-index", "--history", "."]);
    let out = pixel(&empty, &["status", ".", "--statusline"]);
    assert!(out.status.success(), "{out:?}");
    let line = String::from_utf8_lossy(&out.stdout);
    assert!(!line.contains("0/0"), "{line}");
}

/// `dig-history --show` is the follow-up every `dig-history` answer names:
/// it prints the file at the commit, and without `--file` it refuses under
/// the command's current name, so the agent can correct the call it typed.
#[test]
fn dig_history_show_prints_the_file_and_requires_file() {
    let dir = fixture("dig-history-show");
    let shown = pixel(
        &dir,
        &[
            "dig-history",
            "--show",
            "HEAD",
            "--file",
            "src/login.rs",
            "--json",
            ".",
        ],
    );
    assert!(shown.status.success(), "{shown:?}");
    let doc: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(
        doc["content"],
        "pub fn login_user(name: &str) -> bool {\n    !name.is_empty()\n}\n"
    );
    assert_eq!(doc["parent_fallback"], false);

    let missing = pixel(&dir, &["dig-history", "--show", "HEAD", "."]);
    assert!(!missing.status.success(), "{missing:?}");
    assert!(missing.stdout.is_empty(), "{missing:?}");
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(
        stderr.contains("dig-history --show requires --file"),
        "{stderr}"
    );
}

/// `pixel plan` renders the daemon's findings: JSON carries each finding
/// and the verify flag, compact prints `file:line label [SEVERITY]`, and an
/// unknown query fails instead of printing an empty plan.
#[test]
fn plan_renders_daemon_findings_as_json_and_compact() {
    let dir = fixture("plan");
    let out = pixel(&dir, &["plan", "--query", "dead-code", "--json", "."]);
    assert!(out.status.success(), "{out:?}");
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["verify"], true, "{doc}");
    let findings = doc["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{doc}");
    assert_eq!(findings[0]["file"], "src/caller.rs");
    assert_eq!(findings[0]["severity"], "low");
    assert!(
        findings[0]["label"]
            .as_str()
            .unwrap()
            .starts_with("No callers found for function `go`"),
        "{doc}"
    );

    let compact = pixel(
        &dir,
        &[
            "plan",
            "--query",
            "dead-code",
            "--format",
            "compact",
            "--no-verify",
            ".",
        ],
    );
    assert!(compact.status.success(), "{compact:?}");
    let text = String::from_utf8(compact.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "{text}");
    assert!(
        lines[0].starts_with("src/caller.rs:2 No callers found"),
        "{text}"
    );
    assert!(lines[0].ends_with("[LOW]"), "{text}");

    let unknown = pixel(&dir, &["plan", "--query", "everything", "."]);
    assert!(!unknown.status.success(), "{unknown:?}");
    assert!(unknown.stdout.is_empty(), "{unknown:?}");
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("unknown query 'everything'"),
        "{unknown:?}"
    );
}

/// `pixel plan` turns the daemon's `prereqs` evidence into blocking gates:
/// an env read names the variable, an auth-gated file names the saved
/// `auth`-tagged replay flow, gates render above the numbered list, and
/// `--no-gates` omits them.
#[test]
fn plan_lists_verification_gates_and_honors_no_gates() {
    let dir = fixture("plan-prereqs");
    std::fs::write(
        dir.join("src/secrets.rs"),
        "pub fn unused() {\n    auth();\n    let k = std::env::var(\"STRIPE_SECRET_KEY\").unwrap();\n    let _ = k;\n}\nfn auth() {}\n",
    )
    .unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "secrets"]);

    // A saved `auth`-tagged flow the auth gate can name.
    let flows = dir.join("flows");
    std::fs::create_dir_all(&flows).unwrap();
    std::fs::write(
        flows.join("client-login.json"),
        "{\"name\":\"client-login\",\"title\":\"t\",\"description\":\"d\",\"tags\":[\"auth\"],\"steps\":[],\"created_unix\":1,\"revised_unix\":1}",
    )
    .unwrap();

    let out = pixel_command()
        .args(["plan", "--query", "dead-code", "--json", "."])
        .current_dir(&*dir)
        .env("PIXEL_FLOW_DIR", &flows)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let gates = doc["gates"].as_array().unwrap();
    let labels: Vec<&str> = gates.iter().filter_map(|g| g["label"].as_str()).collect();
    assert!(
        labels.iter().any(|l| l.contains("STRIPE_SECRET_KEY")),
        "{doc}"
    );
    assert!(
        labels.iter().any(|l| l.contains("client-login")),
        "auth gate must name the saved flow: {doc}"
    );
    assert_eq!(gates[0]["kind"], "prereq", "{doc}");
    assert_eq!(gates[0]["blocking"], true, "{doc}");

    // Markdown: gates as a bullet block before the numbered findings, and
    // the verify item says the gates come first.
    let md = pixel_command()
        .args(["plan", "--query", "dead-code", "."])
        .current_dir(&*dir)
        .env("PIXEL_FLOW_DIR", &flows)
        .output()
        .unwrap();
    assert!(md.status.success(), "{md:?}");
    let text = String::from_utf8(md.stdout).unwrap();
    let gates_at = text.find("Prerequisites — verification gates:");
    let list_at = text.find("1. [ ]");
    assert!(gates_at.is_some(), "{text}");
    assert!(gates_at < list_at, "gates render before findings: {text}");
    assert!(text.contains("(gates above first)"), "{text}");

    // Gates are tracked items: --status numbers them, --done marks them.
    let status = pixel(&dir, &["plan", "--status"]);
    let shown = String::from_utf8(status.stdout).unwrap();
    assert!(shown.contains("Gate: auth-gated code"), "{shown}");
    let done = pixel(&dir, &["plan", "--done", "1"]);
    assert!(done.status.success(), "{done:?}");

    // --done 1 marks gate 1 done; --status confirms the persistence and
    // that other gates stay undone. Without this round-trip the test
    // would pass on a no-op --done.
    let after = pixel(&dir, &["plan", "--status"]);
    let after_text = String::from_utf8(after.stdout).unwrap();
    let gate_lines: Vec<&str> = after_text.lines().filter(|l| l.contains("Gate:")).collect();
    assert!(
        gate_lines
            .iter()
            .any(|l| l.starts_with("1. [x]") && l.contains("auth-gated code")),
        "gate 1 must be done after --done 1: {after_text}"
    );
    assert!(
        gate_lines.iter().filter(|l| l.contains("[x]")).count() == 1,
        "exactly one gate is done: {after_text}"
    );

    // --no-gates drops the block entirely.
    let none = pixel(&dir, &["plan", "--query", "dead-code", "--no-gates", "."]);
    assert!(none.status.success(), "{none:?}");
    let text = String::from_utf8(none.stdout).unwrap();
    assert!(!text.contains("Prerequisites"), "{text}");
    assert!(!text.contains("Gate:"), "{text}");
}

/// Provider and DB detections each become their own gate label end-to-end.
/// The auth/env gate test above covers two of the four kinds; this one
/// pins the wording for the remaining two so a refactor of `gates_of`
/// cannot silently drop them.
#[test]
fn plan_lists_provider_and_db_gates_end_to_end() {
    let dir = fixture("plan-prereqs-providers");
    std::fs::write(
        dir.join("src/billing.ts"),
        "import Stripe from 'stripe';\n\
         import { sql } from 'drizzle-orm';\n\
         export function charge() { return Stripe; }\n\
         export function query() { return sql; }\n",
    )
    .unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "billing"]);

    let out = pixel_command()
        .args(["plan", "--query", "dead-code", "--json", "."])
        .current_dir(&*dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let gates = doc["gates"].as_array().unwrap();
    let labels: Vec<&str> = gates.iter().filter_map(|g| g["label"].as_str()).collect();

    // Provider gate names the SDK and the env-key prefix the spec promises.
    assert!(
        labels
            .iter()
            .any(|l| l.starts_with("Gate: Stripe integration")),
        "missing provider gate: {doc}"
    );
    assert!(
        labels.iter().any(|l| l.contains("STRIPE_* keys")),
        "provider gate must name the env-key prefix: {doc}"
    );

    // DB gate names the driver and the spec's "reproduce with real data"
    // wording. The driver name is taken from the import spec.
    assert!(
        labels
            .iter()
            .any(|l| l.starts_with("Gate: database-backed state")),
        "missing db gate: {doc}"
    );
    assert!(
        labels.iter().any(|l| l.contains("drizzle-orm")),
        "db gate must name the driver: {doc}"
    );
    assert!(
        labels
            .iter()
            .any(|l| l.contains("reproduce with real data before fixing")),
        "db gate must use the spec wording: {doc}"
    );

    // Every gate is `kind: prereq` and `blocking: true`.
    for g in gates {
        assert_eq!(g["kind"], "prereq", "{g}");
        assert_eq!(g["blocking"], true, "{g}");
    }
}

/// `--no-gates` is a render toggle for the gate block; it must refuse to
/// combine with the state-flag trio (`--status`/`--done`/`--undone`/`--prune`)
/// because those flags take a state-only path that never renders gates. A
/// silent accept used to be possible before the clap `conflicts_with_all`
/// lists were updated to include `no_gates`; pin the behaviour so a future
/// refactor cannot drop the conflict.
#[test]
fn plan_rejects_no_gates_combined_with_state_flags() {
    let out = pixel_command()
        .args(["plan", "--status", "--no-gates"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "must fail: {out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot be used with") && stderr.contains("--no-gates"),
        "stderr must name the conflict: {stderr}"
    );

    for args in [
        &["plan", "--done", "1", "--no-gates"][..],
        &["plan", "--undone", "1", "--no-gates"][..],
        &["plan", "--prune", "--no-gates"][..],
    ] {
        let out = pixel_command().args(args).output().unwrap();
        assert!(!out.status.success(), "must fail: {out:?} {args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("cannot be used with"),
            "stderr must name the conflict for {args:?}: {stderr}"
        );
    }
}

/// `search-content` takes the ripgrep flags agents pass by habit instead of
/// rejecting them: in the recorded demo runs each `--glob` usage error cost
/// the agent a turn. `-l` lists files, `-g` filters with `.gitignore` rules
/// and `!` excludes, `-t` selects by type, `-F` matches literally, `-n` is
/// accepted.
#[test]
fn search_content_takes_ripgreps_glob_type_files_and_literal_flags() {
    let dir = fixture("search-rg-flags");
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(
        dir.join("tests/login_test.rs"),
        "fn t() { login_user(\"x\"); }\n",
    )
    .unwrap();
    std::fs::write(dir.join("NOTES.md"), "login_user is the entry point\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "more"]);
    let files = |args: &[&str]| -> Vec<String> {
        let mut full = vec!["search-content"];
        full.extend_from_slice(args);
        let out = pixel(&dir, &full);
        assert!(out.status.success(), "{args:?}: {out:?}");
        let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(ToString::to_string)
            .collect();
        lines.sort();
        lines
    };
    assert_eq!(
        files(&["login_user", ".", "-l"]),
        [
            "NOTES.md",
            "src/caller.rs",
            "src/login.rs",
            "tests/login_test.rs"
        ],
        "-l prints each file once"
    );
    assert_eq!(
        files(&["login_user", ".", "-l", "--glob", "!**/tests/**"]),
        ["NOTES.md", "src/caller.rs", "src/login.rs"]
    );
    assert_eq!(
        files(&["login_user", ".", "-l", "-t", "rust", "-g", "!tests"]),
        ["src/caller.rs", "src/login.rs"]
    );
    assert_eq!(
        files(&["login_user(", ".", "-F", "-n", "-l", "-g", "src/*.rs"]),
        ["src/caller.rs", "src/login.rs"],
        "-F: `(` is literal"
    );
    // -l output is newline-terminated lines, and nothing at all without a match.
    let listed = pixel(
        &dir,
        &["search-content", "login_user", "src/login.rs", "-l"],
    );
    assert_eq!(String::from_utf8_lossy(&listed.stdout), "src/login.rs\n");
    let none = pixel(&dir, &["search-content", "no_such_needle_xyz", ".", "-l"]);
    assert!(none.status.success(), "{none:?}");
    assert!(none.stdout.is_empty(), "{none:?}");
    // -l still says when the page it listed was cut, and only then.
    let cut = pixel(
        &dir,
        &["search-content", "login_user", ".", "-l", "--limit", "1"],
    );
    assert!(
        String::from_utf8_lossy(&cut.stderr).contains("results truncated"),
        "{cut:?}"
    );
    let whole = pixel(&dir, &["search-content", "login_user", ".", "-l"]);
    assert!(
        !String::from_utf8_lossy(&whole.stderr).contains("results truncated"),
        "{whole:?}"
    );
    // Without -F the same pattern is an unclosed group, so -F is doing the work.
    let regex = pixel(&dir, &["search-content", "login_user(", "."]);
    assert!(!regex.status.success(), "{regex:?}");
    let unknown = pixel(&dir, &["search-content", "login_user", ".", "-t", "cobol"]);
    assert!(!unknown.status.success(), "{unknown:?}");
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("unknown --type 'cobol'"),
        "{unknown:?}"
    );
}

/// The row cap is stated once in prose mode: the `⚠ results truncated`
/// line names it, and the epistemics note that follows must not restate
/// the same cap — two stderr lines saying "truncated" read as two
/// problems. `--json` keeps the cap inside `epistemics.basis`: there that
/// field is the only place a non-reading consumer finds it.
#[test]
fn search_names_a_truncation_cap_once_in_prose() {
    let dir = fixture("search-note-once");
    let cut = pixel(
        &dir,
        &["search-content", "login_user", ".", "-l", "--limit", "1"],
    );
    assert!(cut.status.success(), "{cut:?}");
    let stderr = String::from_utf8_lossy(&cut.stderr);
    assert!(
        stderr.contains("results truncated"),
        "the warning names the cut: {stderr}"
    );
    assert_eq!(
        stderr.matches("row limit").count(),
        1,
        "the row cap is stated once: {stderr}"
    );
    let json = pixel(
        &dir,
        &[
            "search-content",
            "login_user",
            ".",
            "--limit",
            "1",
            "--json",
        ],
    );
    assert!(json.status.success(), "{json:?}");
    let docs = parse_stdout_lines(&json, "json search");
    let (meta, _) = docs.split_last().unwrap();
    assert!(
        meta["epistemics"]["basis"]
            .as_str()
            .is_some_and(|b| b.contains("row limit")),
        "{meta}"
    );
    // A clean page names no cap, so the note must not print at all: with
    // the guard's `&&` chain widened, every search would grow the line.
    let whole = pixel(&dir, &["search-content", "login_user", ".", "-l"]);
    assert!(whole.status.success(), "{whole:?}");
    assert!(
        !String::from_utf8_lossy(&whole.stderr).contains("bounded result"),
        "{whole:?}"
    );
}

/// When the stdout cap cuts the page, the warning names the stdout cap and
/// the daemon's own caps stay in the bounded-result note: the two lines
/// name different bounds, and dropping the daemon's row cap from the note
/// would lose the only place prose names it.
#[test]
fn the_stdout_cap_warning_leaves_the_daemon_caps_in_the_note() {
    let dir = fixture_with_many_matches("search-note-stdout-cap");
    let out = pixel_command()
        .args(["search-content", "the", ".", "--context", "20"])
        .current_dir(&dir)
        .env("PIXEL_OUTPUT_CAP_BYTES", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("stdout cap"),
        "the stdout-cap warning fires: {stderr}"
    );
    assert_eq!(
        stderr.matches("row limit").count(),
        1,
        "the daemon's row cap survives in the note: {stderr}"
    );
}

/// `--limit` counts the matching lines a `-g`/`-t` search prints, not the
/// index rows read before the filter: `--limit 1 -g 'tests/*'` used to ask
/// the index for one row, drop it (it was `NOTES.md`), and print nothing
/// while `tests/login_test.rs` matched. The daemon filters before paging, so
/// `next_offset` counts kept matches and names the next one, never a replay.
#[test]
fn a_filtered_limit_counts_kept_matches_and_resumes_past_dropped_rows() {
    let dir = fixture("search-filter-limit");
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::write(
        dir.join("tests/login_test.rs"),
        "fn t() { login_user(\"x\"); }\n",
    )
    .unwrap();
    std::fs::write(dir.join("NOTES.md"), "login_user is the entry point\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "more"]);
    let page = |args: &[&str]| -> Vec<serde_json::Value> {
        let mut full = vec!["search-content", "login_user", ".", "--json"];
        full.extend_from_slice(args);
        let out = pixel(&dir, &full);
        assert!(out.status.success(), "{args:?}: {out:?}");
        parse_stdout_lines(&out, "filtered search")
    };

    let docs = page(&["--limit", "1", "-g", "tests/*"]);
    let (meta, matches) = docs.split_last().unwrap();
    assert_eq!(matches.len(), 1, "{docs:?}");
    assert_eq!(matches[0]["path"], "tests/login_test.rs");
    assert_eq!(meta["truncated"], false, "the only kept match: {meta}");
    assert!(meta["next_offset"].is_null(), "{meta}");

    // Paging one kept line at a time walks exactly the filtered answer:
    // every `next_offset` skips the rows the filter dropped, none replays.
    let filter = ["-t", "rust", "-g", "src/*"];
    let key = |m: &serde_json::Value| (m["path"].to_string(), m["line"].to_string());
    let docs = page(&filter);
    let whole: Vec<_> = docs[..docs.len() - 1].iter().map(key).collect();
    assert!(whole.len() > 1, "{docs:?}");
    let mut walked = Vec::new();
    let mut offset = "0".to_string();
    for _ in 0..whole.len() + 1 {
        let mut args = filter.to_vec();
        args.extend_from_slice(&["--limit", "1", "--offset", &offset]);
        let docs = page(&args);
        let (meta, matches) = docs.split_last().unwrap();
        assert_eq!(matches.len(), 1, "{docs:?}");
        walked.push(key(&matches[0]));
        match meta["next_offset"].as_u64() {
            Some(next) => {
                assert_eq!(meta["truncated"], true, "{meta}");
                offset = next.to_string();
            }
            None => break,
        }
    }
    assert_eq!(
        walked, whole,
        "one page per kept line, in order, none twice"
    );
}

/// A filter narrows the answer, it does not lengthen the page: without
/// `--limit`, `-g` gets the daemon's default row limit, as an unfiltered
/// search does.
#[test]
fn a_filtered_search_without_a_limit_keeps_the_default_page_length() {
    let dir = fixture_with_many_matches("search-filter-default");
    let run = |args: &[&str]| {
        let mut full = vec!["search-content", "the", ".", "--json"];
        full.extend_from_slice(args);
        let out = pixel(&dir, &full);
        assert!(out.status.success(), "{args:?}: {out:?}");
        parse_stdout_lines(&out, "default page")
    };
    let plain = run(&[]);
    let filtered = run(&["-g", "src/*"]);
    assert_eq!(plain.len() - 1, 100, "the daemon default");
    assert_eq!(filtered.len() - 1, plain.len() - 1);
    assert_eq!(filtered.last().unwrap()["truncated"], true);
}

/// `-l` holds the stdout byte cap like every other search output, cutting
/// between paths, never inside one, and says so on stderr.
#[test]
fn files_with_matches_stop_at_the_stdout_cap_between_paths() {
    let dir = fixture("search-files-cap");
    std::fs::write(dir.join("NOTES.md"), "login_user is the entry point\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "notes"]);
    let run = |cap: &str| {
        pixel_command()
            .args(["search-content", "login_user", ".", "-l"])
            .current_dir(&dir)
            .env("PIXEL_OUTPUT_CAP_BYTES", cap)
            .output()
            .unwrap()
    };
    let whole = run("0");
    let all = String::from_utf8(whole.stdout).unwrap();
    let paths: Vec<&str> = all.lines().collect();
    assert_eq!(paths.len(), 3, "{all}");
    // Room for the first path and its newline, one byte short of the second.
    let cap = paths[0].len() + 1 + paths[1].len();
    let cut = run(&cap.to_string());
    assert_eq!(
        String::from_utf8_lossy(&cut.stdout),
        format!("{}\n", paths[0]),
        "{cut:?}"
    );
    let stderr = String::from_utf8_lossy(&cut.stderr);
    assert!(stderr.contains("stdout cap"), "{stderr}");
    assert!(stderr.contains("wrote 1 paths"), "{stderr}");
    // The offset it names starts on the first path the cap held back.
    let offset = stderr
        .split("--offset ")
        .nth(1)
        .and_then(|rest| rest.split('.').next())
        .unwrap_or_else(|| panic!("a resume offset: {stderr}"));
    let resumed = pixel_command()
        .args([
            "search-content",
            "login_user",
            ".",
            "-l",
            "--offset",
            offset,
        ])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&resumed.stdout).lines().next(),
        Some(paths[1]),
        "{resumed:?}"
    );
    // Exactly the bytes of two paths fits both.
    let fits = run(&(cap + 1).to_string());
    assert_eq!(
        String::from_utf8_lossy(&fits.stdout).lines().count(),
        2,
        "{fits:?}"
    );
}

/// `pack-context` takes the symbol an agent actually has in hand. An agent
/// that just read `renderStories` in a file calls
/// `pixel pack-context renderStories` — the same `uid_or_name` protocol
/// `impact` accepts — and only a fully-qualified uid pins one symbol among
/// same-named ones. When nothing matches, the error names the recovery
/// (`pixel find-symbol`) instead of only restating the guess, so the next
/// call is the one that answers.
#[test]
fn pack_context_resolves_a_bare_name_and_names_the_recovery_when_it_fails() {
    let dir = fixture("pack-context-name");

    // Bare name, unique in the fixture: the same packet the uid form returns.
    let out = pixel(&dir, &["pack-context", "login_user", ".", "--json"]);
    let what = "pack-context bare name";
    assert!(
        out.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let docs = parse_stdout_lines(&out, what);
    assert_eq!(docs.len(), 1, "{docs:?}");
    assert_eq!(docs[0]["symbol"]["name"], "login_user", "{docs:?}");
    assert!(
        docs[0]["symbol"]["uid"]
            .as_str()
            .is_some_and(|uid| uid.contains('#')),
        "the packet names the uid a caller can pin next time: {docs:?}"
    );

    // A name that resolves to nothing: NOT_FOUND, with the lookup that
    // answers on stderr and in the envelope.
    let out = pixel(&dir, &["pack-context", "no_such_symbol_anywhere", "."]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("pixel find-symbol no_such_symbol_anywhere"),
        "{out:?}"
    );
}
