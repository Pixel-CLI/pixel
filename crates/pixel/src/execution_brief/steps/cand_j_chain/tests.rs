// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The step on a repository whose graph a fake daemon answers: a chain
//! `start_job -> build -> make` where `make` constructs the `Widget`.

use serde_json::{Value, json};

use super::super::graph::testing::{FakeGraph, sym};
use super::super::testing::{headers, repo, run};
use super::*;

const ENTRY: &str = "pub fn start_job(text: &str) -> Widget {
    let parsed = parse_text(text);
    let built = build(parsed);
    let extra = size_part(&built);
    drop(extra);
    built
}
";
const BUILD: &str = "pub fn build(parsed: Parsed) -> Widget {
    let sized = parsed.len();
    make(sized)
}
";
const WIDGET: &str = "pub struct Widget {
    pub size: u32,
}

pub fn make(size: u32) -> Widget {
    Widget { size }
}
";
const PARSE: &str = "pub fn parse_text(text: &str) -> Parsed {
    let lines = text.lines();
    let count = lines.count();
    Parsed { count }
}
";
const SIZE: &str = "pub fn size_part(widget: &Widget) -> u32 {
    widget.size
}
";
const PROMPT: &str = "trace how text becomes a Widget from start_job through build";

fn files() -> [(&'static str, &'static str); 5] {
    [
        ("src/entry.rs", ENTRY),
        ("src/build.rs", BUILD),
        ("src/widget.rs", WIDGET),
        ("src/parse.rs", PARSE),
        ("src/size.rs", SIZE),
    ]
}

const START: &str = "src/entry.rs#start_job#function";
const BUILD_UID: &str = "src/build.rs#build#function";
const MAKE: &str = "src/widget.rs#make#function";
const PARSE_UID: &str = "src/parse.rs#parse_text#function";
const SIZE_UID: &str = "src/size.rs#size_part#function";

/// Every symbol of the fixture by name.
fn symbol(name: &str) -> Option<Value> {
    Some(match name {
        "start_job" => sym(START, name, "function", "src/entry.rs", (1, 7)),
        "build" => sym(BUILD_UID, name, "function", "src/build.rs", (1, 4)),
        "make" => sym(MAKE, name, "function", "src/widget.rs", (5, 7)),
        "Widget" => sym("src/widget.rs#Widget#struct", name, "struct", "src/widget.rs", (1, 3)),
        "parse_text" => sym(PARSE_UID, name, "function", "src/parse.rs", (1, 5)),
        "size_part" => {
            let mut row = sym(SIZE_UID, name, "function", "src/size.rs", (1, 3));
            row["sig"] = json!("pub fn size_part(widget: &Widget) -> u32");
            row
        }
        _ => return None,
    })
}

fn edge(name: &str, site: u32) -> Value {
    json!({"symbol": symbol(name), "tier": "exact", "site_line": site})
}

fn answer(request: &Value) -> Value {
    match request["op"].as_str() {
        Some("symbol") => {
            let name = request["name"].as_str().unwrap_or_default();
            json!({"symbols": symbol(name).into_iter().collect::<Vec<_>>()})
        }
        Some("trace") => {
            if request["from"] != START || request["to"] != MAKE {
                return json!({"found": false, "hops": []});
            }
            json!({"found": true, "hops": [
                {"uid": START, "name": "start_job", "path": "src/entry.rs", "line": 1},
                {"uid": BUILD_UID, "name": "build", "path": "src/build.rs", "line": 1},
                {"uid": MAKE, "name": "make", "path": "src/widget.rs", "line": 5}
            ]})
        }
        Some("uses") => {
            assert_eq!(request["role"], "callees");
            let edges = match request["uid_or_name"].as_str() {
                Some(START) => vec![edge("build", 3), edge("parse_text", 2), edge("size_part", 4)],
                Some(BUILD_UID) => vec![edge("make", 3)],
                _ => vec![],
            };
            json!({"edges": edges, "edge_limit": 20})
        }
        _ => json!({}),
    }
}

fn ask(root: &std::path::Path, prompt: &str, brief: &str) -> String {
    run(&STEP, root, prompt, brief)
}

#[test]
fn the_step_applies_to_a_trace_question_and_not_to_an_overview_or_a_review() {
    assert!(STEP.applies(Some("flow"), PROMPT, false));
    assert!(STEP.applies(None, "Walk me through the hook", true), "a closed gate still runs it");
    assert!(STEP.applies(None, "from main to Config", true));
    assert!(!STEP.applies(Some("overview"), PROMPT, false));
    assert!(!STEP.applies(Some("review"), PROMPT, false));
    assert!(!STEP.applies(Some("history"), PROMPT, false));
    assert!(!STEP.applies(None, "what does this repository do", true));
}

#[test]
fn the_chain_is_shown_hop_by_hop_with_what_the_question_names() {
    let dir = repo(&files());
    let daemon = FakeGraph::serve(dir.path(), answer);
    let brief = "[PIXEL:BRIEF]\n### src/old.rs:1-3 — start_job definition (function)\n1| x\n";
    let out = ask(dir.path(), PROMPT, brief);
    assert_eq!(
        headers(&out),
        [
            "### call trace, entry to target",
            "### src/entry.rs:1-7 — start_job calls build",
            "### src/build.rs:1-4 — build calls make",
            "### src/widget.rs:5-7 — make (constructor of Widget), head",
            "### src/size.rs:1-3 — size_part receives the Widget",
            "### src/parse.rs:1-5 — parse_text (called by start_job, named by the question)",
        ]
    );
    assert!(
        out.starts_with(
            "drop: ^(?:src/old\\.rs:1\\-3\\ —\\ start_job\\ definition\\ \\(function\\))$\n\
             ### call trace, entry to target\n\
             chain start_job (src/entry.rs:1) -> build (src/build.rs:1) -> make (src/widget.rs:5)\n\
             ### src/entry.rs:1-7 — start_job calls build\n\
             1| pub fn start_job(text: &str) -> Widget {\n"
        ),
        "{out}"
    );
    assert!(out.contains("\n6|     built\n7| }\n### src/build.rs:1-4"), "{out}");
    assert!(out.ends_with("5| }\n"), "the last line of parse_text, then a newline: {out}");
    // The graph was asked for the entry (the word after `from`), not the first function.
    let traces: Vec<_> = daemon
        .requests()
        .into_iter()
        .filter(|request| request["op"] == "trace")
        .collect();
    assert_eq!(traces.len(), 1, "one constructor, one call-path");
    assert_eq!(traces[0]["from"], START);
}

#[test]
fn a_brief_header_the_step_prints_itself_is_not_dropped() {
    let dir = repo(&files());
    let _daemon = FakeGraph::serve(dir.path(), answer);
    let brief = "[PIXEL:BRIEF]\n### src/entry.rs:1-7 — start_job calls build\n1| x\n";
    let out = ask(dir.path(), PROMPT, brief);
    assert!(out.starts_with("### call trace, entry to target\n"), "{out}");
    assert!(!out.contains("drop:"), "{out}");
}

#[test]
fn the_entry_is_the_word_after_from_whatever_comes_first() {
    let dir = repo(&files());
    let daemon = FakeGraph::serve(dir.path(), answer);
    // `build` is named first, `start_job` after `from`.
    let out = ask(dir.path(), "trace how build makes a Widget from start_job", "");
    assert!(out.contains("chain start_job ("), "{out}");
    let from: Vec<_> = daemon
        .requests()
        .into_iter()
        .filter(|request| request["op"] == "trace")
        .map(|request| request["from"].clone())
        .collect();
    assert_eq!(from, [json!(START)]);
}

#[test]
fn nothing_is_printed_without_a_function_a_type_or_a_chain() {
    let dir = repo(&files());
    let _daemon = FakeGraph::serve(dir.path(), answer);
    assert_eq!(ask(dir.path(), "trace how start_job runs", ""), "", "no type");
    assert_eq!(ask(dir.path(), "trace how a Widget lives", ""), "", "no function");
    assert_eq!(ask(dir.path(), "trace make through Widget", ""), "", "no chain");
}

#[test]
fn nothing_is_printed_without_a_daemon() {
    let dir = repo(&files());
    assert_eq!(ask(dir.path(), PROMPT, ""), "");
}

#[test]
fn the_shortest_chain_among_the_constructors_wins() {
    let widget = format!("{WIDGET}\npub fn spare(size: u32) -> Widget {{\n    make(size)\n}}\n");
    let dir = repo(&[
        ("src/entry.rs", ENTRY),
        ("src/build.rs", BUILD),
        ("src/widget.rs", &widget),
    ]);
    let daemon = FakeGraph::serve(dir.path(), |request| {
        match (request["op"].as_str(), request["name"].as_str()) {
            (Some("symbol"), Some("spare")) => json!({"symbols": [
                sym("src/widget.rs#spare#function", "spare", "function", "src/widget.rs", (9, 11))
            ]}),
            (Some("trace"), _) if request["to"] == "src/widget.rs#spare#function" => json!({
                "found": true, "hops": [
                    {"uid": START, "name": "start_job", "path": "src/entry.rs", "line": 1},
                    {"uid": BUILD_UID, "name": "build", "path": "src/build.rs", "line": 1},
                    {"uid": MAKE, "name": "make", "path": "src/widget.rs", "line": 5},
                    {"uid": "src/widget.rs#spare#function", "name": "spare", "path": "src/widget.rs", "line": 9}
                ]}),
            _ => answer(request),
        }
    });
    let out = ask(dir.path(), PROMPT, "");
    assert!(
        out.contains("chain start_job (src/entry.rs:1) -> build (src/build.rs:1) -> make (src/widget.rs:5)\n"),
        "the three-hop chain beats the four-hop one: {out}"
    );
    assert_eq!(
        daemon
            .requests()
            .iter()
            .filter(|request| request["op"] == "trace")
            .count(),
        2,
        "both constructors were tried"
    );
}
