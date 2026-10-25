// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The step beside `probes/cand-j-callers/probe`. The graph cases carry the
//! script's own output (run with a fake `pixel` answering the same graph) as
//! `expected`; the semantic cases carry the python tests' assertions, on
//! repositories the graph knows nothing about.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::super::graph::testing::FakeGraph;
use super::super::testing::{repo, run};
use super::semantic::{Limits, tracked_source_within};
use super::*;

/// One graph case: what the daemon knows and what the script printed.
struct Case {
    prompt: &'static str,
    brief: &'static str,
    graph: &'static str,
    files: &'static [(&'static str, &'static str)],
    expected: &'static str,
}

/// The daemon of a case: `symbol` by name, `uses` by `uid|role`, 20 a page.
fn answer(graph: &Value, request: &Value) -> Value {
    match request["op"].as_str() {
        Some("symbol") => {
            let name = request["name"].as_str().unwrap_or_default();
            let symbols: Vec<&Value> = graph["symbols"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|symbol| symbol["name"] == name)
                .collect();
            json!({"symbols": symbols})
        }
        Some("uses") => {
            let key = format!(
                "{}|{}",
                request["uid_or_name"].as_str().unwrap_or_default(),
                request["role"].as_str().unwrap_or_default()
            );
            let offset = request["offset"].as_u64().unwrap_or(0) as usize;
            let rows = graph["edges"][&key].as_array().cloned().unwrap_or_default();
            let page: Vec<Value> = rows.into_iter().skip(offset).take(20).collect();
            json!({"edges": page, "edge_limit": 20})
        }
        _ => json!({}),
    }
}

/// What the step prints for `case`, and the requests the daemon saw.
fn ask(case: &Case) -> (String, Vec<Value>) {
    let dir = repo(case.files);
    let graph: Value = serde_json::from_str(case.graph).unwrap();
    let daemon = FakeGraph::serve(dir.path(), move |request| answer(&graph, request));
    let out = run(&STEP, dir.path(), case.prompt, case.brief);
    (out, daemon.requests())
}

fn same_as_the_script(case: &Case) {
    let (out, _) = ask(case);
    assert_eq!(out, case.expected, "\n{out}");
}

/// What the step prints when no graph is there.
fn alone(prompt: &str, files: &[(&str, &str)]) -> String {
    let dir = repo(files);
    run(&STEP, dir.path(), prompt, "")
}

const MAIN: Case = Case {
    prompt: "who calls load_config and which tests cover it",
    brief: r#"[PIXEL:BRIEF]
### tests/unrelated.rs:1-5 — a thing (test)
1| fn nothing() {}
### tests/config.rs:1-5 — cover (test)
1| fn loads_the_defaults() { load_config() }
### tests matching other words
nothing here
### tests matching load_config
load_config is here
### src/other.rs:3-9 — other_thing definition (function)
3| pub fn other_thing() {
### src/lib.rs:5-9 — load_config definition (function)
5| pub fn load_config() {
"#,
    graph: r#"{"symbols": [{"uid": "U#load_config", "name": "load_config", "kind": "function", "path": "src/lib.rs", "start_line": 5, "end_line": 9, "qualified": "load_config", "sig": ""}, {"uid": "U#run", "name": "run", "kind": "function", "path": "src/cli.rs", "start_line": 3, "end_line": 6, "qualified": "run", "sig": ""}, {"uid": "U#main", "name": "main", "kind": "function", "path": "src/main.rs", "start_line": 3, "end_line": 7, "qualified": "main", "sig": ""}, {"uid": "U#long_caller", "name": "long_caller", "kind": "function", "path": "src/long.rs", "start_line": 1, "end_line": 55, "qualified": "long_caller", "sig": ""}, {"uid": "U#loads_the_defaults", "name": "loads_the_defaults", "kind": "function", "path": "tests/config.rs", "start_line": 2, "end_line": 5, "qualified": "loads_the_defaults", "sig": ""}, {"uid": "U#read_file", "name": "read_file", "kind": "function", "path": "src/fs.rs", "start_line": 1, "end_line": 3, "qualified": "read_file", "sig": ""}], "edges": {"U#load_config|callers": [{"symbol": {"uid": "U#loads_the_defaults", "name": "loads_the_defaults", "kind": "function", "path": "tests/config.rs", "start_line": 2, "end_line": 5, "qualified": "loads_the_defaults", "sig": ""}, "tier": "exact", "site_line": 3}, {"symbol": {"uid": "U#run", "name": "run", "kind": "function", "path": "src/cli.rs", "start_line": 3, "end_line": 6, "qualified": "run", "sig": ""}, "tier": "exact", "site_line": 4}, {"symbol": {"uid": "U#long_caller", "name": "long_caller", "kind": "function", "path": "src/long.rs", "start_line": 1, "end_line": 55, "qualified": "long_caller", "sig": ""}, "tier": "exact", "site_line": 50}], "U#run|callers": [{"symbol": {"uid": "U#main", "name": "main", "kind": "function", "path": "src/main.rs", "start_line": 3, "end_line": 7, "qualified": "main", "sig": ""}, "tier": "exact", "site_line": 5}], "U#long_caller|callers": [], "U#loads_the_defaults|callers": [], "U#load_config|callees": [{"symbol": {"uid": "U#read_file", "name": "read_file", "kind": "function", "path": "src/fs.rs", "start_line": 1, "end_line": 3, "qualified": "read_file", "sig": ""}, "tier": "exact", "site_line": 7}]}}"#,
    files: &[
        ("src/lib.rs", r#"use std::env;

/// Reads the settings. The directory comes from PIXEL_CONFIG_DIR, else
/// from PIXEL_HOME_FALLBACK.
pub fn load_config(path: &str) -> Config {
    let dir = env::var("PIXEL_CONFIG_DIR").unwrap_or_default();
    let text = read_file(&dir, path);
    parse(text)
}

pub fn other_thing() -> u32 {
    7
}
"#),
        ("src/cli.rs", r#"use crate::lib::load_config;

fn run(args: &[String]) -> Config {
    let config = load_config(&args[0]);
    config
}
"#),
        ("src/main.rs", r#"mod cli;

fn main() {
    let args = collect();
    let config = cli::run(&args);
    show(config);
}
"#),
        ("src/long.rs", r#"fn long_caller() {
    step1();
    step2();
    step3();
    step4();
    step5();
    step6();
    step7();
    step8();
    step9();
    step10();
    step11();
    step12();
    step13();
    step14();
    step15();
    step16();
    step17();
    step18();
    step19();
    step20();
    step21();
    step22();
    step23();
    step24();
    step25();
    step26();
    step27();
    step28();
    step29();
    step30();
    step31();
    step32();
    step33();
    step34();
    step35();
    step36();
    step37();
    step38();
    step39();
    step40();
    step41();
    step42();
    step43();
    step44();
    step45();
    step46();
    step47();
    step48();
    load_config("x");
    after1();
    after2();
    after3();
    after4();
}
"#),
        ("src/raw.rs", r#"pub fn load_config_raw(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
"#),
        ("src/fs.rs", r#"pub fn read_file(dir: &str, path: &str) -> String {
    std::fs::read_to_string(format!("{dir}/{path}")).unwrap_or_default()
}

#[test]
fn reads_what_it_wrote() {
    let text = read_file("/tmp", "x");
    assert!(text.is_empty());
}
"#),
        ("tests/config.rs", r#"#[test]
fn loads_the_defaults() {
    let config = load_config("none");
    assert!(config.is_default());
}
"#),
        ("tests/env.rs", r#"#[test]
fn honours_the_directory() {
    std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
    let config = load_config("a");
    assert_eq!(config.dir(), "/tmp/x");
}
"#),
    ],
    expected: r#"drop: ^(?:tests/unrelated\.rs:1\-5|tests\ matching\ other\ words|src/other\.rs:3\-9)
### src/cli.rs:3-6 — run calls load_config
3| fn run(args: &[String]) -> Config {
4|     let config = load_config(&args[0]);
5|     config
6| }
### src/main.rs:3-4 — main (declaration)
3| fn main() {
4|     let args = collect();
### src/main.rs:4-6 — main calls run
4|     let args = collect();
5|     let config = cli::run(&args);
6|     show(config);
### src/long.rs:42-55 — long_caller calls load_config
42|     step41();
43|     step42();
44|     step43();
45|     step44();
46|     step45();
47|     step46();
48|     step47();
49|     step48();
50|     load_config("x");
51|     after1();
52|     after2();
53|     after3();
54|     after4();
55| }
### tests/config.rs:2-5 — loads_the_defaults a test that calls load_config
2| fn loads_the_defaults() {
3|     let config = load_config("none");
4|     assert!(config.is_default());
5| }
### src/raw.rs:1-3 — load_config_raw (helper named after load_config)
1| pub fn load_config_raw(path: &str) -> String {
2|     std::fs::read_to_string(path).unwrap_or_default()
3| }
### src/fs.rs:5-9 — a unit test beside fs.rs, which load_config calls into
5| #[test]
6| fn reads_what_it_wrote() {
7|     let text = read_file("/tmp", "x");
8|     assert!(text.is_empty());
9| }
### tests/env.rs:1-6 — a test that sets PIXEL_CONFIG_DIR
1| #[test]
2| fn honours_the_directory() {
3|     std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
4|     let config = load_config("a");
5|     assert_eq!(config.dir(), "/tmp/x");
6| }
"#,
};

#[test]
fn graph_case_main_prints_what_the_script_prints() {
    same_as_the_script(&MAIN);
}

const NO_BRIEF: Case = Case {
    prompt: "who calls load_config and which tests cover it",
    brief: "",
    graph: r#"{"symbols": [{"uid": "U#load_config", "name": "load_config", "kind": "function", "path": "src/lib.rs", "start_line": 5, "end_line": 9, "qualified": "load_config", "sig": ""}, {"uid": "U#run", "name": "run", "kind": "function", "path": "src/cli.rs", "start_line": 3, "end_line": 6, "qualified": "run", "sig": ""}, {"uid": "U#main", "name": "main", "kind": "function", "path": "src/main.rs", "start_line": 3, "end_line": 7, "qualified": "main", "sig": ""}, {"uid": "U#long_caller", "name": "long_caller", "kind": "function", "path": "src/long.rs", "start_line": 1, "end_line": 55, "qualified": "long_caller", "sig": ""}, {"uid": "U#loads_the_defaults", "name": "loads_the_defaults", "kind": "function", "path": "tests/config.rs", "start_line": 2, "end_line": 5, "qualified": "loads_the_defaults", "sig": ""}, {"uid": "U#read_file", "name": "read_file", "kind": "function", "path": "src/fs.rs", "start_line": 1, "end_line": 3, "qualified": "read_file", "sig": ""}], "edges": {"U#load_config|callers": [{"symbol": {"uid": "U#loads_the_defaults", "name": "loads_the_defaults", "kind": "function", "path": "tests/config.rs", "start_line": 2, "end_line": 5, "qualified": "loads_the_defaults", "sig": ""}, "tier": "exact", "site_line": 3}, {"symbol": {"uid": "U#run", "name": "run", "kind": "function", "path": "src/cli.rs", "start_line": 3, "end_line": 6, "qualified": "run", "sig": ""}, "tier": "exact", "site_line": 4}, {"symbol": {"uid": "U#long_caller", "name": "long_caller", "kind": "function", "path": "src/long.rs", "start_line": 1, "end_line": 55, "qualified": "long_caller", "sig": ""}, "tier": "exact", "site_line": 50}], "U#run|callers": [{"symbol": {"uid": "U#main", "name": "main", "kind": "function", "path": "src/main.rs", "start_line": 3, "end_line": 7, "qualified": "main", "sig": ""}, "tier": "exact", "site_line": 5}], "U#long_caller|callers": [], "U#loads_the_defaults|callers": [], "U#load_config|callees": [{"symbol": {"uid": "U#read_file", "name": "read_file", "kind": "function", "path": "src/fs.rs", "start_line": 1, "end_line": 3, "qualified": "read_file", "sig": ""}, "tier": "exact", "site_line": 7}]}}"#,
    files: &[
        ("src/lib.rs", r#"use std::env;

/// Reads the settings. The directory comes from PIXEL_CONFIG_DIR, else
/// from PIXEL_HOME_FALLBACK.
pub fn load_config(path: &str) -> Config {
    let dir = env::var("PIXEL_CONFIG_DIR").unwrap_or_default();
    let text = read_file(&dir, path);
    parse(text)
}

pub fn other_thing() -> u32 {
    7
}
"#),
        ("src/cli.rs", r#"use crate::lib::load_config;

fn run(args: &[String]) -> Config {
    let config = load_config(&args[0]);
    config
}
"#),
        ("src/main.rs", r#"mod cli;

fn main() {
    let args = collect();
    let config = cli::run(&args);
    show(config);
}
"#),
        ("src/long.rs", r#"fn long_caller() {
    step1();
    step2();
    step3();
    step4();
    step5();
    step6();
    step7();
    step8();
    step9();
    step10();
    step11();
    step12();
    step13();
    step14();
    step15();
    step16();
    step17();
    step18();
    step19();
    step20();
    step21();
    step22();
    step23();
    step24();
    step25();
    step26();
    step27();
    step28();
    step29();
    step30();
    step31();
    step32();
    step33();
    step34();
    step35();
    step36();
    step37();
    step38();
    step39();
    step40();
    step41();
    step42();
    step43();
    step44();
    step45();
    step46();
    step47();
    step48();
    load_config("x");
    after1();
    after2();
    after3();
    after4();
}
"#),
        ("src/raw.rs", r#"pub fn load_config_raw(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
"#),
        ("src/fs.rs", r#"pub fn read_file(dir: &str, path: &str) -> String {
    std::fs::read_to_string(format!("{dir}/{path}")).unwrap_or_default()
}

#[test]
fn reads_what_it_wrote() {
    let text = read_file("/tmp", "x");
    assert!(text.is_empty());
}
"#),
        ("tests/config.rs", r#"#[test]
fn loads_the_defaults() {
    let config = load_config("none");
    assert!(config.is_default());
}
"#),
        ("tests/env.rs", r#"#[test]
fn honours_the_directory() {
    std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
    let config = load_config("a");
    assert_eq!(config.dir(), "/tmp/x");
}
"#),
    ],
    expected: r#"### src/cli.rs:3-6 — run calls load_config
3| fn run(args: &[String]) -> Config {
4|     let config = load_config(&args[0]);
5|     config
6| }
### src/main.rs:3-4 — main (declaration)
3| fn main() {
4|     let args = collect();
### src/main.rs:4-6 — main calls run
4|     let args = collect();
5|     let config = cli::run(&args);
6|     show(config);
### src/long.rs:42-55 — long_caller calls load_config
42|     step41();
43|     step42();
44|     step43();
45|     step44();
46|     step45();
47|     step46();
48|     step47();
49|     step48();
50|     load_config("x");
51|     after1();
52|     after2();
53|     after3();
54|     after4();
55| }
### tests/config.rs:2-5 — loads_the_defaults a test that calls load_config
2| fn loads_the_defaults() {
3|     let config = load_config("none");
4|     assert!(config.is_default());
5| }
### src/raw.rs:1-3 — load_config_raw (helper named after load_config)
1| pub fn load_config_raw(path: &str) -> String {
2|     std::fs::read_to_string(path).unwrap_or_default()
3| }
### src/fs.rs:5-9 — a unit test beside fs.rs, which load_config calls into
5| #[test]
6| fn reads_what_it_wrote() {
7|     let text = read_file("/tmp", "x");
8|     assert!(text.is_empty());
9| }
### tests/env.rs:1-6 — a test that sets PIXEL_CONFIG_DIR
1| #[test]
2| fn honours_the_directory() {
3|     std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
4|     let config = load_config("a");
5|     assert_eq!(config.dir(), "/tmp/x");
6| }
"#,
};

#[test]
fn graph_case_no_brief_prints_what_the_script_prints() {
    same_as_the_script(&NO_BRIEF);
}

const MANY_CALLERS: Case = Case {
    prompt: "who calls load_config and which tests cover it",
    brief: "",
    graph: r#"{"symbols": [{"uid": "U#load_config", "name": "load_config", "kind": "function", "path": "src/lib.rs", "start_line": 5, "end_line": 9, "qualified": "load_config", "sig": ""}, {"uid": "U#c8", "name": "c8", "kind": "function", "path": "src/c8.rs", "start_line": 1, "end_line": 3, "qualified": "c8", "sig": ""}, {"uid": "U#c7", "name": "c7", "kind": "function", "path": "src/c7.rs", "start_line": 1, "end_line": 3, "qualified": "c7", "sig": ""}, {"uid": "U#c6", "name": "c6", "kind": "function", "path": "src/c6.rs", "start_line": 1, "end_line": 3, "qualified": "c6", "sig": ""}, {"uid": "U#c5", "name": "c5", "kind": "function", "path": "src/c5.rs", "start_line": 1, "end_line": 3, "qualified": "c5", "sig": ""}, {"uid": "U#c4", "name": "c4", "kind": "function", "path": "src/c4.rs", "start_line": 1, "end_line": 3, "qualified": "c4", "sig": ""}, {"uid": "U#c3", "name": "c3", "kind": "function", "path": "src/c3.rs", "start_line": 1, "end_line": 3, "qualified": "c3", "sig": ""}, {"uid": "U#c2", "name": "c2", "kind": "function", "path": "src/c2.rs", "start_line": 1, "end_line": 3, "qualified": "c2", "sig": ""}, {"uid": "U#c1", "name": "c1", "kind": "function", "path": "src/c1.rs", "start_line": 1, "end_line": 3, "qualified": "c1", "sig": ""}, {"uid": "U#u1", "name": "u1", "kind": "function", "path": "src/u1.rs", "start_line": 1, "end_line": 3, "qualified": "u1", "sig": ""}, {"uid": "U#u2", "name": "u2", "kind": "function", "path": "src/u2.rs", "start_line": 1, "end_line": 3, "qualified": "u2", "sig": ""}, {"uid": "U#u3", "name": "u3", "kind": "function", "path": "src/u3.rs", "start_line": 1, "end_line": 3, "qualified": "u3", "sig": ""}, {"uid": "U#u4", "name": "u4", "kind": "function", "path": "src/u4.rs", "start_line": 1, "end_line": 3, "qualified": "u4", "sig": ""}, {"uid": "U#u5", "name": "u5", "kind": "function", "path": "src/u5.rs", "start_line": 1, "end_line": 3, "qualified": "u5", "sig": ""}], "edges": {"U#load_config|callers": [{"symbol": {"uid": "U#c8", "name": "c8", "kind": "function", "path": "src/c8.rs", "start_line": 1, "end_line": 3, "qualified": "c8", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#c7", "name": "c7", "kind": "function", "path": "src/c7.rs", "start_line": 1, "end_line": 3, "qualified": "c7", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#c6", "name": "c6", "kind": "function", "path": "src/c6.rs", "start_line": 1, "end_line": 3, "qualified": "c6", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#c5", "name": "c5", "kind": "function", "path": "src/c5.rs", "start_line": 1, "end_line": 3, "qualified": "c5", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#c4", "name": "c4", "kind": "function", "path": "src/c4.rs", "start_line": 1, "end_line": 3, "qualified": "c4", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#c3", "name": "c3", "kind": "function", "path": "src/c3.rs", "start_line": 1, "end_line": 3, "qualified": "c3", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#c2", "name": "c2", "kind": "function", "path": "src/c2.rs", "start_line": 1, "end_line": 3, "qualified": "c2", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#c1", "name": "c1", "kind": "function", "path": "src/c1.rs", "start_line": 1, "end_line": 3, "qualified": "c1", "sig": ""}, "tier": "exact", "site_line": 2}], "U#load_config|callees": [], "U#c8|callers": [], "U#c7|callers": [], "U#c6|callers": [], "U#c5|callers": [], "U#c4|callers": [], "U#c3|callers": [], "U#c2|callers": [], "U#c1|callers": [{"symbol": {"uid": "U#u1", "name": "u1", "kind": "function", "path": "src/u1.rs", "start_line": 1, "end_line": 3, "qualified": "u1", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#u2", "name": "u2", "kind": "function", "path": "src/u2.rs", "start_line": 1, "end_line": 3, "qualified": "u2", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#u3", "name": "u3", "kind": "function", "path": "src/u3.rs", "start_line": 1, "end_line": 3, "qualified": "u3", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#u4", "name": "u4", "kind": "function", "path": "src/u4.rs", "start_line": 1, "end_line": 3, "qualified": "u4", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#u5", "name": "u5", "kind": "function", "path": "src/u5.rs", "start_line": 1, "end_line": 3, "qualified": "u5", "sig": ""}, "tier": "exact", "site_line": 2}]}}"#,
    files: &[
        ("src/lib.rs", r#"use std::env;

/// Reads the settings. The directory comes from PIXEL_CONFIG_DIR, else
/// from PIXEL_HOME_FALLBACK.
pub fn load_config(path: &str) -> Config {
    let dir = env::var("PIXEL_CONFIG_DIR").unwrap_or_default();
    let text = read_file(&dir, path);
    parse(text)
}

pub fn other_thing() -> u32 {
    7
}
"#),
        ("src/cli.rs", r#"use crate::lib::load_config;

fn run(args: &[String]) -> Config {
    let config = load_config(&args[0]);
    config
}
"#),
        ("src/main.rs", r#"mod cli;

fn main() {
    let args = collect();
    let config = cli::run(&args);
    show(config);
}
"#),
        ("src/long.rs", r#"fn long_caller() {
    step1();
    step2();
    step3();
    step4();
    step5();
    step6();
    step7();
    step8();
    step9();
    step10();
    step11();
    step12();
    step13();
    step14();
    step15();
    step16();
    step17();
    step18();
    step19();
    step20();
    step21();
    step22();
    step23();
    step24();
    step25();
    step26();
    step27();
    step28();
    step29();
    step30();
    step31();
    step32();
    step33();
    step34();
    step35();
    step36();
    step37();
    step38();
    step39();
    step40();
    step41();
    step42();
    step43();
    step44();
    step45();
    step46();
    step47();
    step48();
    load_config("x");
    after1();
    after2();
    after3();
    after4();
}
"#),
        ("src/raw.rs", r#"pub fn load_config_raw(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
"#),
        ("src/fs.rs", r#"pub fn read_file(dir: &str, path: &str) -> String {
    std::fs::read_to_string(format!("{dir}/{path}")).unwrap_or_default()
}

#[test]
fn reads_what_it_wrote() {
    let text = read_file("/tmp", "x");
    assert!(text.is_empty());
}
"#),
        ("tests/config.rs", r#"#[test]
fn loads_the_defaults() {
    let config = load_config("none");
    assert!(config.is_default());
}
"#),
        ("tests/env.rs", r#"#[test]
fn honours_the_directory() {
    std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
    let config = load_config("a");
    assert_eq!(config.dir(), "/tmp/x");
}
"#),
        ("src/c8.rs", r#"fn c8() {
    load_config("8");
}
"#),
        ("src/c7.rs", r#"fn c7() {
    load_config("7");
}
"#),
        ("src/c6.rs", r#"fn c6() {
    load_config("6");
}
"#),
        ("src/c5.rs", r#"fn c5() {
    load_config("5");
}
"#),
        ("src/c4.rs", r#"fn c4() {
    load_config("4");
}
"#),
        ("src/c3.rs", r#"fn c3() {
    load_config("3");
}
"#),
        ("src/c2.rs", r#"fn c2() {
    load_config("2");
}
"#),
        ("src/c1.rs", r#"fn c1() {
    load_config("1");
}
"#),
        ("src/u1.rs", r#"fn u1() {
    c1();
}
"#),
        ("src/u2.rs", r#"fn u2() {
    c1();
}
"#),
        ("src/u3.rs", r#"fn u3() {
    c1();
}
"#),
        ("src/u4.rs", r#"fn u4() {
    c1();
}
"#),
        ("src/u5.rs", r#"fn u5() {
    c1();
}
"#),
    ],
    expected: r#"### src/c1.rs:1-3 — c1 calls load_config
1| fn c1() {
2|     load_config("1");
3| }
### src/u1.rs:1-3 — u1 calls c1
1| fn u1() {
2|     c1();
3| }
### src/u2.rs:1-3 — u2 calls c1
1| fn u2() {
2|     c1();
3| }
### src/u3.rs:1-3 — u3 calls c1
1| fn u3() {
2|     c1();
3| }
### src/c2.rs:1-3 — c2 calls load_config
1| fn c2() {
2|     load_config("2");
3| }
### src/c3.rs:1-3 — c3 calls load_config
1| fn c3() {
2|     load_config("3");
3| }
### src/c4.rs:1-3 — c4 calls load_config
1| fn c4() {
2|     load_config("4");
3| }
### src/c5.rs:1-3 — c5 calls load_config
1| fn c5() {
2|     load_config("5");
3| }
### src/c6.rs:1-3 — c6 calls load_config
1| fn c6() {
2|     load_config("6");
3| }
### src/raw.rs:1-3 — load_config_raw (helper named after load_config)
1| pub fn load_config_raw(path: &str) -> String {
2|     std::fs::read_to_string(path).unwrap_or_default()
3| }
### tests/env.rs:1-6 — a test that sets PIXEL_CONFIG_DIR
1| #[test]
2| fn honours_the_directory() {
3|     std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
4|     let config = load_config("a");
5|     assert_eq!(config.dir(), "/tmp/x");
6| }
"#,
};

#[test]
fn graph_case_many_callers_prints_what_the_script_prints() {
    same_as_the_script(&MANY_CALLERS);
}

const BODY_CAP: Case = Case {
    prompt: "who calls load_config and which tests cover it",
    brief: "",
    graph: r#"{"symbols": [{"uid": "U#load_config", "name": "load_config", "kind": "function", "path": "src/lib.rs", "start_line": 5, "end_line": 9, "qualified": "load_config", "sig": ""}, {"uid": "U#a40", "name": "a40", "kind": "function", "path": "src/a40.rs", "start_line": 1, "end_line": 40, "qualified": "a40", "sig": ""}, {"uid": "U#b41", "name": "b41", "kind": "function", "path": "src/b41.rs", "start_line": 1, "end_line": 41, "qualified": "b41", "sig": ""}], "edges": {"U#load_config|callers": [{"symbol": {"uid": "U#a40", "name": "a40", "kind": "function", "path": "src/a40.rs", "start_line": 1, "end_line": 40, "qualified": "a40", "sig": ""}, "tier": "exact", "site_line": 39}, {"symbol": {"uid": "U#b41", "name": "b41", "kind": "function", "path": "src/b41.rs", "start_line": 1, "end_line": 41, "qualified": "b41", "sig": ""}, "tier": "exact", "site_line": 2}], "U#a40|callers": [], "U#b41|callers": [], "U#load_config|callees": []}}"#,
    files: &[
        ("src/lib.rs", r#"use std::env;

/// Reads the settings. The directory comes from PIXEL_CONFIG_DIR, else
/// from PIXEL_HOME_FALLBACK.
pub fn load_config(path: &str) -> Config {
    let dir = env::var("PIXEL_CONFIG_DIR").unwrap_or_default();
    let text = read_file(&dir, path);
    parse(text)
}

pub fn other_thing() -> u32 {
    7
}
"#),
        ("src/cli.rs", r#"use crate::lib::load_config;

fn run(args: &[String]) -> Config {
    let config = load_config(&args[0]);
    config
}
"#),
        ("src/main.rs", r#"mod cli;

fn main() {
    let args = collect();
    let config = cli::run(&args);
    show(config);
}
"#),
        ("src/long.rs", r#"fn long_caller() {
    step1();
    step2();
    step3();
    step4();
    step5();
    step6();
    step7();
    step8();
    step9();
    step10();
    step11();
    step12();
    step13();
    step14();
    step15();
    step16();
    step17();
    step18();
    step19();
    step20();
    step21();
    step22();
    step23();
    step24();
    step25();
    step26();
    step27();
    step28();
    step29();
    step30();
    step31();
    step32();
    step33();
    step34();
    step35();
    step36();
    step37();
    step38();
    step39();
    step40();
    step41();
    step42();
    step43();
    step44();
    step45();
    step46();
    step47();
    step48();
    load_config("x");
    after1();
    after2();
    after3();
    after4();
}
"#),
        ("src/raw.rs", r#"pub fn load_config_raw(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
"#),
        ("src/fs.rs", r#"pub fn read_file(dir: &str, path: &str) -> String {
    std::fs::read_to_string(format!("{dir}/{path}")).unwrap_or_default()
}

#[test]
fn reads_what_it_wrote() {
    let text = read_file("/tmp", "x");
    assert!(text.is_empty());
}
"#),
        ("tests/config.rs", r#"#[test]
fn loads_the_defaults() {
    let config = load_config("none");
    assert!(config.is_default());
}
"#),
        ("tests/env.rs", r#"#[test]
fn honours_the_directory() {
    std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
    let config = load_config("a");
    assert_eq!(config.dir(), "/tmp/x");
}
"#),
        ("src/a40.rs", r#"fn a40() {
    s0();
    s1();
    s2();
    s3();
    s4();
    s5();
    s6();
    s7();
    s8();
    s9();
    s10();
    s11();
    s12();
    s13();
    s14();
    s15();
    s16();
    s17();
    s18();
    s19();
    s20();
    s21();
    s22();
    s23();
    s24();
    s25();
    s26();
    s27();
    s28();
    s29();
    s30();
    s31();
    s32();
    s33();
    s34();
    s35();
    s36();
    s37();
    load_config("a");
}
"#),
        ("src/b41.rs", r#"fn b41() {
    load_config("b");
    s0();
    s1();
    s2();
    s3();
    s4();
    s5();
    s6();
    s7();
    s8();
    s9();
    s10();
    s11();
    s12();
    s13();
    s14();
    s15();
    s16();
    s17();
    s18();
    s19();
    s20();
    s21();
    s22();
    s23();
    s24();
    s25();
    s26();
    s27();
    s28();
    s29();
    s30();
    s31();
    s32();
    s33();
    s34();
    s35();
    s36();
    s37();
    s38();
}
"#),
    ],
    expected: r#"### src/a40.rs:1-40 — a40 calls load_config
1| fn a40() {
2|     s0();
3|     s1();
4|     s2();
5|     s3();
6|     s4();
7|     s5();
8|     s6();
9|     s7();
10|     s8();
11|     s9();
12|     s10();
13|     s11();
14|     s12();
15|     s13();
16|     s14();
17|     s15();
18|     s16();
19|     s17();
20|     s18();
21|     s19();
22|     s20();
23|     s21();
24|     s22();
25|     s23();
26|     s24();
27|     s25();
28|     s26();
29|     s27();
30|     s28();
31|     s29();
32|     s30();
33|     s31();
34|     s32();
35|     s33();
36|     s34();
37|     s35();
38|     s36();
39|     s37();
40|     load_config("a");
### src/b41.rs:1-10 — b41 calls load_config
1| fn b41() {
2|     load_config("b");
3|     s0();
4|     s1();
5|     s2();
6|     s3();
7|     s4();
8|     s5();
9|     s6();
10|     s7();
### src/raw.rs:1-3 — load_config_raw (helper named after load_config)
1| pub fn load_config_raw(path: &str) -> String {
2|     std::fs::read_to_string(path).unwrap_or_default()
3| }
### tests/env.rs:1-6 — a test that sets PIXEL_CONFIG_DIR
1| #[test]
2| fn honours_the_directory() {
3|     std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
4|     let config = load_config("a");
5|     assert_eq!(config.dir(), "/tmp/x");
6| }
"#,
};

#[test]
fn graph_case_body_cap_prints_what_the_script_prints() {
    same_as_the_script(&BODY_CAP);
}

const SAME_CALLER_TWICE: Case = Case {
    prompt: "who calls load_config and which tests cover it",
    brief: "",
    graph: r#"{"symbols": [{"uid": "U#load_config", "name": "load_config", "kind": "function", "path": "src/lib.rs", "start_line": 5, "end_line": 9, "qualified": "load_config", "sig": ""}, {"uid": "U#run", "name": "run", "kind": "function", "path": "src/cli.rs", "start_line": 3, "end_line": 6, "qualified": "run", "sig": ""}, {"uid": "U#main", "name": "main", "kind": "function", "path": "src/main.rs", "start_line": 3, "end_line": 7, "qualified": "main", "sig": ""}, {"uid": "U#long_caller", "name": "long_caller", "kind": "function", "path": "src/long.rs", "start_line": 1, "end_line": 55, "qualified": "long_caller", "sig": ""}, {"uid": "U#loads_the_defaults", "name": "loads_the_defaults", "kind": "function", "path": "tests/config.rs", "start_line": 2, "end_line": 5, "qualified": "loads_the_defaults", "sig": ""}, {"uid": "U#read_file", "name": "read_file", "kind": "function", "path": "src/fs.rs", "start_line": 1, "end_line": 3, "qualified": "read_file", "sig": ""}], "edges": {"U#load_config|callers": [{"symbol": {"uid": "U#run", "name": "run", "kind": "function", "path": "src/cli.rs", "start_line": 3, "end_line": 6, "qualified": "run", "sig": ""}, "tier": "exact", "site_line": 4}, {"symbol": {"uid": "U#run", "name": "run", "kind": "function", "path": "src/cli.rs", "start_line": 3, "end_line": 6, "qualified": "run", "sig": ""}, "tier": "exact", "site_line": 4}], "U#run|callers": [{"symbol": {"uid": "U#main", "name": "main", "kind": "function", "path": "src/main.rs", "start_line": 3, "end_line": 7, "qualified": "main", "sig": ""}, "tier": "exact", "site_line": 5}], "U#long_caller|callers": [], "U#loads_the_defaults|callers": [], "U#load_config|callees": [{"symbol": {"uid": "U#read_file", "name": "read_file", "kind": "function", "path": "src/fs.rs", "start_line": 1, "end_line": 3, "qualified": "read_file", "sig": ""}, "tier": "exact", "site_line": 7}]}}"#,
    files: &[
        ("src/lib.rs", r#"use std::env;

/// Reads the settings. The directory comes from PIXEL_CONFIG_DIR, else
/// from PIXEL_HOME_FALLBACK.
pub fn load_config(path: &str) -> Config {
    let dir = env::var("PIXEL_CONFIG_DIR").unwrap_or_default();
    let text = read_file(&dir, path);
    parse(text)
}

pub fn other_thing() -> u32 {
    7
}
"#),
        ("src/cli.rs", r#"use crate::lib::load_config;

fn run(args: &[String]) -> Config {
    let config = load_config(&args[0]);
    config
}
"#),
        ("src/main.rs", r#"mod cli;

fn main() {
    let args = collect();
    let config = cli::run(&args);
    show(config);
}
"#),
        ("src/long.rs", r#"fn long_caller() {
    step1();
    step2();
    step3();
    step4();
    step5();
    step6();
    step7();
    step8();
    step9();
    step10();
    step11();
    step12();
    step13();
    step14();
    step15();
    step16();
    step17();
    step18();
    step19();
    step20();
    step21();
    step22();
    step23();
    step24();
    step25();
    step26();
    step27();
    step28();
    step29();
    step30();
    step31();
    step32();
    step33();
    step34();
    step35();
    step36();
    step37();
    step38();
    step39();
    step40();
    step41();
    step42();
    step43();
    step44();
    step45();
    step46();
    step47();
    step48();
    load_config("x");
    after1();
    after2();
    after3();
    after4();
}
"#),
        ("src/raw.rs", r#"pub fn load_config_raw(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
"#),
        ("src/fs.rs", r#"pub fn read_file(dir: &str, path: &str) -> String {
    std::fs::read_to_string(format!("{dir}/{path}")).unwrap_or_default()
}

#[test]
fn reads_what_it_wrote() {
    let text = read_file("/tmp", "x");
    assert!(text.is_empty());
}
"#),
        ("tests/config.rs", r#"#[test]
fn loads_the_defaults() {
    let config = load_config("none");
    assert!(config.is_default());
}
"#),
        ("tests/env.rs", r#"#[test]
fn honours_the_directory() {
    std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
    let config = load_config("a");
    assert_eq!(config.dir(), "/tmp/x");
}
"#),
    ],
    expected: r#"### src/cli.rs:3-6 — run calls load_config
3| fn run(args: &[String]) -> Config {
4|     let config = load_config(&args[0]);
5|     config
6| }
### src/main.rs:3-4 — main (declaration)
3| fn main() {
4|     let args = collect();
### src/main.rs:4-6 — main calls run
4|     let args = collect();
5|     let config = cli::run(&args);
6|     show(config);
### src/raw.rs:1-3 — load_config_raw (helper named after load_config)
1| pub fn load_config_raw(path: &str) -> String {
2|     std::fs::read_to_string(path).unwrap_or_default()
3| }
### src/fs.rs:5-9 — a unit test beside fs.rs, which load_config calls into
5| #[test]
6| fn reads_what_it_wrote() {
7|     let text = read_file("/tmp", "x");
8|     assert!(text.is_empty());
9| }
### tests/env.rs:1-6 — a test that sets PIXEL_CONFIG_DIR
1| #[test]
2| fn honours_the_directory() {
3|     std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
4|     let config = load_config("a");
5|     assert_eq!(config.dir(), "/tmp/x");
6| }
"#,
};

#[test]
fn graph_case_same_caller_twice_prints_what_the_script_prints() {
    same_as_the_script(&SAME_CALLER_TWICE);
}

const SHOWN_DEFINITION_WINS: Case = Case {
    prompt: "who calls load_config and which tests cover it",
    brief: r#"[PIXEL:BRIEF]
### src/lib.rs:5-9 — load_config definition (function)
5| x
"#,
    graph: r#"{"symbols": [{"uid": "U#load_config", "name": "load_config", "kind": "function", "path": "src/lib.rs", "start_line": 5, "end_line": 9, "qualified": "load_config", "sig": ""}, {"uid": "U#alt", "name": "load_config", "kind": "function", "path": "src/alt.rs", "start_line": 1, "end_line": 3, "qualified": "load_config", "sig": ""}, {"uid": "U#run", "name": "run", "kind": "function", "path": "src/cli.rs", "start_line": 3, "end_line": 6, "qualified": "run", "sig": ""}], "edges": {"U#load_config|callers": [{"symbol": {"uid": "U#run", "name": "run", "kind": "function", "path": "src/cli.rs", "start_line": 3, "end_line": 6, "qualified": "run", "sig": ""}, "tier": "exact", "site_line": 4}], "U#alt|callers": [], "U#run|callers": [], "U#alt|callees": [], "U#load_config|callees": []}}"#,
    files: &[
        ("src/lib.rs", r#"use std::env;

/// Reads the settings. The directory comes from PIXEL_CONFIG_DIR, else
/// from PIXEL_HOME_FALLBACK.
pub fn load_config(path: &str) -> Config {
    let dir = env::var("PIXEL_CONFIG_DIR").unwrap_or_default();
    let text = read_file(&dir, path);
    parse(text)
}

pub fn other_thing() -> u32 {
    7
}
"#),
        ("src/cli.rs", r#"use crate::lib::load_config;

fn run(args: &[String]) -> Config {
    let config = load_config(&args[0]);
    config
}
"#),
        ("src/main.rs", r#"mod cli;

fn main() {
    let args = collect();
    let config = cli::run(&args);
    show(config);
}
"#),
        ("src/long.rs", r#"fn long_caller() {
    step1();
    step2();
    step3();
    step4();
    step5();
    step6();
    step7();
    step8();
    step9();
    step10();
    step11();
    step12();
    step13();
    step14();
    step15();
    step16();
    step17();
    step18();
    step19();
    step20();
    step21();
    step22();
    step23();
    step24();
    step25();
    step26();
    step27();
    step28();
    step29();
    step30();
    step31();
    step32();
    step33();
    step34();
    step35();
    step36();
    step37();
    step38();
    step39();
    step40();
    step41();
    step42();
    step43();
    step44();
    step45();
    step46();
    step47();
    step48();
    load_config("x");
    after1();
    after2();
    after3();
    after4();
}
"#),
        ("src/raw.rs", r#"pub fn load_config_raw(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
"#),
        ("src/fs.rs", r#"pub fn read_file(dir: &str, path: &str) -> String {
    std::fs::read_to_string(format!("{dir}/{path}")).unwrap_or_default()
}

#[test]
fn reads_what_it_wrote() {
    let text = read_file("/tmp", "x");
    assert!(text.is_empty());
}
"#),
        ("tests/config.rs", r#"#[test]
fn loads_the_defaults() {
    let config = load_config("none");
    assert!(config.is_default());
}
"#),
        ("tests/env.rs", r#"#[test]
fn honours_the_directory() {
    std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
    let config = load_config("a");
    assert_eq!(config.dir(), "/tmp/x");
}
"#),
        ("src/alt.rs", r#"pub fn load_config() {
    1;
}
"#),
    ],
    expected: r#"### src/cli.rs:3-6 — run calls load_config
3| fn run(args: &[String]) -> Config {
4|     let config = load_config(&args[0]);
5|     config
6| }
### src/raw.rs:1-3 — load_config_raw (helper named after load_config)
1| pub fn load_config_raw(path: &str) -> String {
2|     std::fs::read_to_string(path).unwrap_or_default()
3| }
### tests/env.rs:1-6 — a test that sets PIXEL_CONFIG_DIR
1| #[test]
2| fn honours_the_directory() {
3|     std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
4|     let config = load_config("a");
5|     assert_eq!(config.dir(), "/tmp/x");
6| }
"#,
};

#[test]
fn graph_case_shown_definition_wins_prints_what_the_script_prints() {
    same_as_the_script(&SHOWN_DEFINITION_WINS);
}

const FIRST_IN_PATH_ORDER: Case = Case {
    prompt: "who calls load_config and which tests cover it",
    brief: "",
    graph: r#"{"symbols": [{"uid": "U#load_config", "name": "load_config", "kind": "function", "path": "src/lib.rs", "start_line": 5, "end_line": 9, "qualified": "load_config", "sig": ""}, {"uid": "U#alt", "name": "load_config", "kind": "function", "path": "src/alt.rs", "start_line": 1, "end_line": 3, "qualified": "load_config", "sig": ""}, {"uid": "U#run", "name": "run", "kind": "function", "path": "src/cli.rs", "start_line": 3, "end_line": 6, "qualified": "run", "sig": ""}], "edges": {"U#load_config|callers": [{"symbol": {"uid": "U#run", "name": "run", "kind": "function", "path": "src/cli.rs", "start_line": 3, "end_line": 6, "qualified": "run", "sig": ""}, "tier": "exact", "site_line": 4}], "U#alt|callers": [], "U#run|callers": [], "U#alt|callees": [], "U#load_config|callees": []}}"#,
    files: &[
        ("src/lib.rs", r#"use std::env;

/// Reads the settings. The directory comes from PIXEL_CONFIG_DIR, else
/// from PIXEL_HOME_FALLBACK.
pub fn load_config(path: &str) -> Config {
    let dir = env::var("PIXEL_CONFIG_DIR").unwrap_or_default();
    let text = read_file(&dir, path);
    parse(text)
}

pub fn other_thing() -> u32 {
    7
}
"#),
        ("src/cli.rs", r#"use crate::lib::load_config;

fn run(args: &[String]) -> Config {
    let config = load_config(&args[0]);
    config
}
"#),
        ("src/main.rs", r#"mod cli;

fn main() {
    let args = collect();
    let config = cli::run(&args);
    show(config);
}
"#),
        ("src/long.rs", r#"fn long_caller() {
    step1();
    step2();
    step3();
    step4();
    step5();
    step6();
    step7();
    step8();
    step9();
    step10();
    step11();
    step12();
    step13();
    step14();
    step15();
    step16();
    step17();
    step18();
    step19();
    step20();
    step21();
    step22();
    step23();
    step24();
    step25();
    step26();
    step27();
    step28();
    step29();
    step30();
    step31();
    step32();
    step33();
    step34();
    step35();
    step36();
    step37();
    step38();
    step39();
    step40();
    step41();
    step42();
    step43();
    step44();
    step45();
    step46();
    step47();
    step48();
    load_config("x");
    after1();
    after2();
    after3();
    after4();
}
"#),
        ("src/raw.rs", r#"pub fn load_config_raw(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
"#),
        ("src/fs.rs", r#"pub fn read_file(dir: &str, path: &str) -> String {
    std::fs::read_to_string(format!("{dir}/{path}")).unwrap_or_default()
}

#[test]
fn reads_what_it_wrote() {
    let text = read_file("/tmp", "x");
    assert!(text.is_empty());
}
"#),
        ("tests/config.rs", r#"#[test]
fn loads_the_defaults() {
    let config = load_config("none");
    assert!(config.is_default());
}
"#),
        ("tests/env.rs", r#"#[test]
fn honours_the_directory() {
    std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
    let config = load_config("a");
    assert_eq!(config.dir(), "/tmp/x");
}
"#),
        ("src/alt.rs", r#"pub fn load_config() {
    1;
}
"#),
    ],
    expected: r#"### src/raw.rs:1-3 — load_config_raw (helper named after load_config)
1| pub fn load_config_raw(path: &str) -> String {
2|     std::fs::read_to_string(path).unwrap_or_default()
3| }
"#,
};

#[test]
fn graph_case_first_in_path_order_prints_what_the_script_prints() {
    same_as_the_script(&FIRST_IN_PATH_ORDER);
}

const STRUCT_IS_NOT_A_FUNCTION: Case = Case {
    prompt: "who calls load_config and which tests cover it",
    brief: "",
    graph: r#"{"symbols": [{"uid": "U#load_config", "name": "load_config", "kind": "struct", "path": "src/lib.rs", "start_line": 5, "end_line": 9, "qualified": "load_config", "sig": ""}], "edges": {"U#load_config|callers": [{"symbol": {"uid": "U#loads_the_defaults", "name": "loads_the_defaults", "kind": "function", "path": "tests/config.rs", "start_line": 2, "end_line": 5, "qualified": "loads_the_defaults", "sig": ""}, "tier": "exact", "site_line": 3}, {"symbol": {"uid": "U#run", "name": "run", "kind": "function", "path": "src/cli.rs", "start_line": 3, "end_line": 6, "qualified": "run", "sig": ""}, "tier": "exact", "site_line": 4}, {"symbol": {"uid": "U#long_caller", "name": "long_caller", "kind": "function", "path": "src/long.rs", "start_line": 1, "end_line": 55, "qualified": "long_caller", "sig": ""}, "tier": "exact", "site_line": 50}], "U#run|callers": [{"symbol": {"uid": "U#main", "name": "main", "kind": "function", "path": "src/main.rs", "start_line": 3, "end_line": 7, "qualified": "main", "sig": ""}, "tier": "exact", "site_line": 5}], "U#long_caller|callers": [], "U#loads_the_defaults|callers": [], "U#load_config|callees": [{"symbol": {"uid": "U#read_file", "name": "read_file", "kind": "function", "path": "src/fs.rs", "start_line": 1, "end_line": 3, "qualified": "read_file", "sig": ""}, "tier": "exact", "site_line": 7}]}}"#,
    files: &[
        ("src/lib.rs", r#"use std::env;

/// Reads the settings. The directory comes from PIXEL_CONFIG_DIR, else
/// from PIXEL_HOME_FALLBACK.
pub fn load_config(path: &str) -> Config {
    let dir = env::var("PIXEL_CONFIG_DIR").unwrap_or_default();
    let text = read_file(&dir, path);
    parse(text)
}

pub fn other_thing() -> u32 {
    7
}
"#),
        ("src/cli.rs", r#"use crate::lib::load_config;

fn run(args: &[String]) -> Config {
    let config = load_config(&args[0]);
    config
}
"#),
        ("src/main.rs", r#"mod cli;

fn main() {
    let args = collect();
    let config = cli::run(&args);
    show(config);
}
"#),
        ("src/long.rs", r#"fn long_caller() {
    step1();
    step2();
    step3();
    step4();
    step5();
    step6();
    step7();
    step8();
    step9();
    step10();
    step11();
    step12();
    step13();
    step14();
    step15();
    step16();
    step17();
    step18();
    step19();
    step20();
    step21();
    step22();
    step23();
    step24();
    step25();
    step26();
    step27();
    step28();
    step29();
    step30();
    step31();
    step32();
    step33();
    step34();
    step35();
    step36();
    step37();
    step38();
    step39();
    step40();
    step41();
    step42();
    step43();
    step44();
    step45();
    step46();
    step47();
    step48();
    load_config("x");
    after1();
    after2();
    after3();
    after4();
}
"#),
        ("src/raw.rs", r#"pub fn load_config_raw(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
"#),
        ("src/fs.rs", r#"pub fn read_file(dir: &str, path: &str) -> String {
    std::fs::read_to_string(format!("{dir}/{path}")).unwrap_or_default()
}

#[test]
fn reads_what_it_wrote() {
    let text = read_file("/tmp", "x");
    assert!(text.is_empty());
}
"#),
        ("tests/config.rs", r#"#[test]
fn loads_the_defaults() {
    let config = load_config("none");
    assert!(config.is_default());
}
"#),
        ("tests/env.rs", r#"#[test]
fn honours_the_directory() {
    std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
    let config = load_config("a");
    assert_eq!(config.dir(), "/tmp/x");
}
"#),
    ],
    expected: r#""#,
};

#[test]
fn graph_case_struct_is_not_a_function_prints_what_the_script_prints() {
    same_as_the_script(&STRUCT_IS_NOT_A_FUNCTION);
}

const NOTHING_TO_SHOW: Case = Case {
    prompt: "who calls load_config and which tests cover it",
    brief: r#"[PIXEL:BRIEF]
### tests/unrelated.rs:1-5 — a thing (test)
1| fn nothing() {}
### tests/config.rs:1-5 — cover (test)
1| fn loads_the_defaults() { load_config() }
### tests matching other words
nothing here
### tests matching load_config
load_config is here
### src/other.rs:3-9 — other_thing definition (function)
3| pub fn other_thing() {
### src/lib.rs:5-9 — load_config definition (function)
5| pub fn load_config() {
"#,
    graph: r#"{"symbols": [{"uid": "U#load_config", "name": "load_config", "kind": "function", "path": "src/lib.rs", "start_line": 5, "end_line": 9, "qualified": "load_config", "sig": ""}], "edges": {"U#load_config|callers": [], "U#load_config|callees": []}}"#,
    files: &[
        ("src/lib.rs", r#"use std::env;

/// Reads the settings. The directory comes from dir, else
/// from home.
pub fn load_config(path: &str) -> Config {
    let dir = env::var("dir").unwrap_or_default();
    let text = read_file(&dir, path);
    parse(text)
}

pub fn other_thing() -> u32 {
    7
}
"#),
    ],
    expected: r#""#,
};

#[test]
fn graph_case_nothing_to_show_prints_what_the_script_prints() {
    same_as_the_script(&NOTHING_TO_SHOW);
}

const PAGED_CALLERS: Case = Case {
    prompt: "who calls load_config and which tests cover it",
    brief: "",
    graph: r#"{"symbols": [{"uid": "U#load_config", "name": "load_config", "kind": "function", "path": "src/lib.rs", "start_line": 5, "end_line": 9, "qualified": "load_config", "sig": ""}, {"uid": "U#p0", "name": "p0", "kind": "function", "path": "src/z00.rs", "start_line": 1, "end_line": 3, "qualified": "p0", "sig": ""}, {"uid": "U#p1", "name": "p1", "kind": "function", "path": "src/z01.rs", "start_line": 1, "end_line": 3, "qualified": "p1", "sig": ""}, {"uid": "U#p2", "name": "p2", "kind": "function", "path": "src/z02.rs", "start_line": 1, "end_line": 3, "qualified": "p2", "sig": ""}, {"uid": "U#p3", "name": "p3", "kind": "function", "path": "src/z03.rs", "start_line": 1, "end_line": 3, "qualified": "p3", "sig": ""}, {"uid": "U#p4", "name": "p4", "kind": "function", "path": "src/z04.rs", "start_line": 1, "end_line": 3, "qualified": "p4", "sig": ""}, {"uid": "U#p5", "name": "p5", "kind": "function", "path": "src/z05.rs", "start_line": 1, "end_line": 3, "qualified": "p5", "sig": ""}, {"uid": "U#p6", "name": "p6", "kind": "function", "path": "src/z06.rs", "start_line": 1, "end_line": 3, "qualified": "p6", "sig": ""}, {"uid": "U#p7", "name": "p7", "kind": "function", "path": "src/z07.rs", "start_line": 1, "end_line": 3, "qualified": "p7", "sig": ""}, {"uid": "U#p8", "name": "p8", "kind": "function", "path": "src/z08.rs", "start_line": 1, "end_line": 3, "qualified": "p8", "sig": ""}, {"uid": "U#p9", "name": "p9", "kind": "function", "path": "src/z09.rs", "start_line": 1, "end_line": 3, "qualified": "p9", "sig": ""}, {"uid": "U#p10", "name": "p10", "kind": "function", "path": "src/z10.rs", "start_line": 1, "end_line": 3, "qualified": "p10", "sig": ""}, {"uid": "U#p11", "name": "p11", "kind": "function", "path": "src/z11.rs", "start_line": 1, "end_line": 3, "qualified": "p11", "sig": ""}, {"uid": "U#p12", "name": "p12", "kind": "function", "path": "src/z12.rs", "start_line": 1, "end_line": 3, "qualified": "p12", "sig": ""}, {"uid": "U#p13", "name": "p13", "kind": "function", "path": "src/z13.rs", "start_line": 1, "end_line": 3, "qualified": "p13", "sig": ""}, {"uid": "U#p14", "name": "p14", "kind": "function", "path": "src/z14.rs", "start_line": 1, "end_line": 3, "qualified": "p14", "sig": ""}, {"uid": "U#p15", "name": "p15", "kind": "function", "path": "src/z15.rs", "start_line": 1, "end_line": 3, "qualified": "p15", "sig": ""}, {"uid": "U#p16", "name": "p16", "kind": "function", "path": "src/z16.rs", "start_line": 1, "end_line": 3, "qualified": "p16", "sig": ""}, {"uid": "U#p17", "name": "p17", "kind": "function", "path": "src/z17.rs", "start_line": 1, "end_line": 3, "qualified": "p17", "sig": ""}, {"uid": "U#p18", "name": "p18", "kind": "function", "path": "src/z18.rs", "start_line": 1, "end_line": 3, "qualified": "p18", "sig": ""}, {"uid": "U#p19", "name": "p19", "kind": "function", "path": "src/z19.rs", "start_line": 1, "end_line": 3, "qualified": "p19", "sig": ""}, {"uid": "U#p20", "name": "p20", "kind": "function", "path": "src/a20.rs", "start_line": 1, "end_line": 3, "qualified": "p20", "sig": ""}, {"uid": "U#p21", "name": "p21", "kind": "function", "path": "src/a21.rs", "start_line": 1, "end_line": 3, "qualified": "p21", "sig": ""}, {"uid": "U#p22", "name": "p22", "kind": "function", "path": "src/a22.rs", "start_line": 1, "end_line": 3, "qualified": "p22", "sig": ""}, {"uid": "U#p23", "name": "p23", "kind": "function", "path": "src/a23.rs", "start_line": 1, "end_line": 3, "qualified": "p23", "sig": ""}, {"uid": "U#p24", "name": "p24", "kind": "function", "path": "src/a24.rs", "start_line": 1, "end_line": 3, "qualified": "p24", "sig": ""}], "edges": {"U#load_config|callers": [{"symbol": {"uid": "U#p0", "name": "p0", "kind": "function", "path": "src/z00.rs", "start_line": 1, "end_line": 3, "qualified": "p0", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p1", "name": "p1", "kind": "function", "path": "src/z01.rs", "start_line": 1, "end_line": 3, "qualified": "p1", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p2", "name": "p2", "kind": "function", "path": "src/z02.rs", "start_line": 1, "end_line": 3, "qualified": "p2", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p3", "name": "p3", "kind": "function", "path": "src/z03.rs", "start_line": 1, "end_line": 3, "qualified": "p3", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p4", "name": "p4", "kind": "function", "path": "src/z04.rs", "start_line": 1, "end_line": 3, "qualified": "p4", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p5", "name": "p5", "kind": "function", "path": "src/z05.rs", "start_line": 1, "end_line": 3, "qualified": "p5", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p6", "name": "p6", "kind": "function", "path": "src/z06.rs", "start_line": 1, "end_line": 3, "qualified": "p6", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p7", "name": "p7", "kind": "function", "path": "src/z07.rs", "start_line": 1, "end_line": 3, "qualified": "p7", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p8", "name": "p8", "kind": "function", "path": "src/z08.rs", "start_line": 1, "end_line": 3, "qualified": "p8", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p9", "name": "p9", "kind": "function", "path": "src/z09.rs", "start_line": 1, "end_line": 3, "qualified": "p9", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p10", "name": "p10", "kind": "function", "path": "src/z10.rs", "start_line": 1, "end_line": 3, "qualified": "p10", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p11", "name": "p11", "kind": "function", "path": "src/z11.rs", "start_line": 1, "end_line": 3, "qualified": "p11", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p12", "name": "p12", "kind": "function", "path": "src/z12.rs", "start_line": 1, "end_line": 3, "qualified": "p12", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p13", "name": "p13", "kind": "function", "path": "src/z13.rs", "start_line": 1, "end_line": 3, "qualified": "p13", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p14", "name": "p14", "kind": "function", "path": "src/z14.rs", "start_line": 1, "end_line": 3, "qualified": "p14", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p15", "name": "p15", "kind": "function", "path": "src/z15.rs", "start_line": 1, "end_line": 3, "qualified": "p15", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p16", "name": "p16", "kind": "function", "path": "src/z16.rs", "start_line": 1, "end_line": 3, "qualified": "p16", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p17", "name": "p17", "kind": "function", "path": "src/z17.rs", "start_line": 1, "end_line": 3, "qualified": "p17", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p18", "name": "p18", "kind": "function", "path": "src/z18.rs", "start_line": 1, "end_line": 3, "qualified": "p18", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p19", "name": "p19", "kind": "function", "path": "src/z19.rs", "start_line": 1, "end_line": 3, "qualified": "p19", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p20", "name": "p20", "kind": "function", "path": "src/a20.rs", "start_line": 1, "end_line": 3, "qualified": "p20", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p21", "name": "p21", "kind": "function", "path": "src/a21.rs", "start_line": 1, "end_line": 3, "qualified": "p21", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p22", "name": "p22", "kind": "function", "path": "src/a22.rs", "start_line": 1, "end_line": 3, "qualified": "p22", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p23", "name": "p23", "kind": "function", "path": "src/a23.rs", "start_line": 1, "end_line": 3, "qualified": "p23", "sig": ""}, "tier": "exact", "site_line": 2}, {"symbol": {"uid": "U#p24", "name": "p24", "kind": "function", "path": "src/a24.rs", "start_line": 1, "end_line": 3, "qualified": "p24", "sig": ""}, "tier": "exact", "site_line": 2}], "U#load_config|callees": [], "U#p0|callers": [], "U#p1|callers": [], "U#p2|callers": [], "U#p3|callers": [], "U#p4|callers": [], "U#p5|callers": [], "U#p6|callers": [], "U#p7|callers": [], "U#p8|callers": [], "U#p9|callers": [], "U#p10|callers": [], "U#p11|callers": [], "U#p12|callers": [], "U#p13|callers": [], "U#p14|callers": [], "U#p15|callers": [], "U#p16|callers": [], "U#p17|callers": [], "U#p18|callers": [], "U#p19|callers": [], "U#p20|callers": [], "U#p21|callers": [], "U#p22|callers": [], "U#p23|callers": [], "U#p24|callers": []}}"#,
    files: &[
        ("src/lib.rs", r#"use std::env;

/// Reads the settings. The directory comes from PIXEL_CONFIG_DIR, else
/// from PIXEL_HOME_FALLBACK.
pub fn load_config(path: &str) -> Config {
    let dir = env::var("PIXEL_CONFIG_DIR").unwrap_or_default();
    let text = read_file(&dir, path);
    parse(text)
}

pub fn other_thing() -> u32 {
    7
}
"#),
        ("src/cli.rs", r#"use crate::lib::load_config;

fn run(args: &[String]) -> Config {
    let config = load_config(&args[0]);
    config
}
"#),
        ("src/main.rs", r#"mod cli;

fn main() {
    let args = collect();
    let config = cli::run(&args);
    show(config);
}
"#),
        ("src/long.rs", r#"fn long_caller() {
    step1();
    step2();
    step3();
    step4();
    step5();
    step6();
    step7();
    step8();
    step9();
    step10();
    step11();
    step12();
    step13();
    step14();
    step15();
    step16();
    step17();
    step18();
    step19();
    step20();
    step21();
    step22();
    step23();
    step24();
    step25();
    step26();
    step27();
    step28();
    step29();
    step30();
    step31();
    step32();
    step33();
    step34();
    step35();
    step36();
    step37();
    step38();
    step39();
    step40();
    step41();
    step42();
    step43();
    step44();
    step45();
    step46();
    step47();
    step48();
    load_config("x");
    after1();
    after2();
    after3();
    after4();
}
"#),
        ("src/raw.rs", r#"pub fn load_config_raw(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
"#),
        ("src/fs.rs", r#"pub fn read_file(dir: &str, path: &str) -> String {
    std::fs::read_to_string(format!("{dir}/{path}")).unwrap_or_default()
}

#[test]
fn reads_what_it_wrote() {
    let text = read_file("/tmp", "x");
    assert!(text.is_empty());
}
"#),
        ("tests/config.rs", r#"#[test]
fn loads_the_defaults() {
    let config = load_config("none");
    assert!(config.is_default());
}
"#),
        ("tests/env.rs", r#"#[test]
fn honours_the_directory() {
    std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
    let config = load_config("a");
    assert_eq!(config.dir(), "/tmp/x");
}
"#),
        ("src/z00.rs", r#"fn p0() {
    load_config("0");
}
"#),
        ("src/z01.rs", r#"fn p1() {
    load_config("1");
}
"#),
        ("src/z02.rs", r#"fn p2() {
    load_config("2");
}
"#),
        ("src/z03.rs", r#"fn p3() {
    load_config("3");
}
"#),
        ("src/z04.rs", r#"fn p4() {
    load_config("4");
}
"#),
        ("src/z05.rs", r#"fn p5() {
    load_config("5");
}
"#),
        ("src/z06.rs", r#"fn p6() {
    load_config("6");
}
"#),
        ("src/z07.rs", r#"fn p7() {
    load_config("7");
}
"#),
        ("src/z08.rs", r#"fn p8() {
    load_config("8");
}
"#),
        ("src/z09.rs", r#"fn p9() {
    load_config("9");
}
"#),
        ("src/z10.rs", r#"fn p10() {
    load_config("10");
}
"#),
        ("src/z11.rs", r#"fn p11() {
    load_config("11");
}
"#),
        ("src/z12.rs", r#"fn p12() {
    load_config("12");
}
"#),
        ("src/z13.rs", r#"fn p13() {
    load_config("13");
}
"#),
        ("src/z14.rs", r#"fn p14() {
    load_config("14");
}
"#),
        ("src/z15.rs", r#"fn p15() {
    load_config("15");
}
"#),
        ("src/z16.rs", r#"fn p16() {
    load_config("16");
}
"#),
        ("src/z17.rs", r#"fn p17() {
    load_config("17");
}
"#),
        ("src/z18.rs", r#"fn p18() {
    load_config("18");
}
"#),
        ("src/z19.rs", r#"fn p19() {
    load_config("19");
}
"#),
        ("src/a20.rs", r#"fn p20() {
    load_config("20");
}
"#),
        ("src/a21.rs", r#"fn p21() {
    load_config("21");
}
"#),
        ("src/a22.rs", r#"fn p22() {
    load_config("22");
}
"#),
        ("src/a23.rs", r#"fn p23() {
    load_config("23");
}
"#),
        ("src/a24.rs", r#"fn p24() {
    load_config("24");
}
"#),
    ],
    expected: r#"### src/a20.rs:1-3 — p20 calls load_config
1| fn p20() {
2|     load_config("20");
3| }
### src/a21.rs:1-3 — p21 calls load_config
1| fn p21() {
2|     load_config("21");
3| }
### src/a22.rs:1-3 — p22 calls load_config
1| fn p22() {
2|     load_config("22");
3| }
### src/a23.rs:1-3 — p23 calls load_config
1| fn p23() {
2|     load_config("23");
3| }
### src/a24.rs:1-3 — p24 calls load_config
1| fn p24() {
2|     load_config("24");
3| }
### src/z00.rs:1-3 — p0 calls load_config
1| fn p0() {
2|     load_config("0");
3| }
### src/raw.rs:1-3 — load_config_raw (helper named after load_config)
1| pub fn load_config_raw(path: &str) -> String {
2|     std::fs::read_to_string(path).unwrap_or_default()
3| }
### tests/env.rs:1-6 — a test that sets PIXEL_CONFIG_DIR
1| #[test]
2| fn honours_the_directory() {
3|     std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
4|     let config = load_config("a");
5|     assert_eq!(config.dir(), "/tmp/x");
6| }
"#,
};

#[test]
fn graph_case_paged_callers_prints_what_the_script_prints() {
    same_as_the_script(&PAGED_CALLERS);
}

const FIRST_RESOLVABLE_WORD: Case = Case {
    prompt: "who calls `Config` or the helper load_config",
    brief: "",
    graph: r#"{"symbols": [{"uid": "U#load_config", "name": "load_config", "kind": "function", "path": "src/lib.rs", "start_line": 5, "end_line": 9, "qualified": "load_config", "sig": ""}, {"uid": "U#run", "name": "run", "kind": "function", "path": "src/cli.rs", "start_line": 3, "end_line": 6, "qualified": "run", "sig": ""}, {"uid": "U#main", "name": "main", "kind": "function", "path": "src/main.rs", "start_line": 3, "end_line": 7, "qualified": "main", "sig": ""}, {"uid": "U#long_caller", "name": "long_caller", "kind": "function", "path": "src/long.rs", "start_line": 1, "end_line": 55, "qualified": "long_caller", "sig": ""}, {"uid": "U#loads_the_defaults", "name": "loads_the_defaults", "kind": "function", "path": "tests/config.rs", "start_line": 2, "end_line": 5, "qualified": "loads_the_defaults", "sig": ""}, {"uid": "U#read_file", "name": "read_file", "kind": "function", "path": "src/fs.rs", "start_line": 1, "end_line": 3, "qualified": "read_file", "sig": ""}], "edges": {"U#load_config|callers": [{"symbol": {"uid": "U#loads_the_defaults", "name": "loads_the_defaults", "kind": "function", "path": "tests/config.rs", "start_line": 2, "end_line": 5, "qualified": "loads_the_defaults", "sig": ""}, "tier": "exact", "site_line": 3}, {"symbol": {"uid": "U#run", "name": "run", "kind": "function", "path": "src/cli.rs", "start_line": 3, "end_line": 6, "qualified": "run", "sig": ""}, "tier": "exact", "site_line": 4}, {"symbol": {"uid": "U#long_caller", "name": "long_caller", "kind": "function", "path": "src/long.rs", "start_line": 1, "end_line": 55, "qualified": "long_caller", "sig": ""}, "tier": "exact", "site_line": 50}], "U#run|callers": [{"symbol": {"uid": "U#main", "name": "main", "kind": "function", "path": "src/main.rs", "start_line": 3, "end_line": 7, "qualified": "main", "sig": ""}, "tier": "exact", "site_line": 5}], "U#long_caller|callers": [], "U#loads_the_defaults|callers": [], "U#load_config|callees": [{"symbol": {"uid": "U#read_file", "name": "read_file", "kind": "function", "path": "src/fs.rs", "start_line": 1, "end_line": 3, "qualified": "read_file", "sig": ""}, "tier": "exact", "site_line": 7}]}}"#,
    files: &[
        ("src/lib.rs", r#"use std::env;

/// Reads the settings. The directory comes from PIXEL_CONFIG_DIR, else
/// from PIXEL_HOME_FALLBACK.
pub fn load_config(path: &str) -> Config {
    let dir = env::var("PIXEL_CONFIG_DIR").unwrap_or_default();
    let text = read_file(&dir, path);
    parse(text)
}

pub fn other_thing() -> u32 {
    7
}
"#),
        ("src/cli.rs", r#"use crate::lib::load_config;

fn run(args: &[String]) -> Config {
    let config = load_config(&args[0]);
    config
}
"#),
        ("src/main.rs", r#"mod cli;

fn main() {
    let args = collect();
    let config = cli::run(&args);
    show(config);
}
"#),
        ("src/long.rs", r#"fn long_caller() {
    step1();
    step2();
    step3();
    step4();
    step5();
    step6();
    step7();
    step8();
    step9();
    step10();
    step11();
    step12();
    step13();
    step14();
    step15();
    step16();
    step17();
    step18();
    step19();
    step20();
    step21();
    step22();
    step23();
    step24();
    step25();
    step26();
    step27();
    step28();
    step29();
    step30();
    step31();
    step32();
    step33();
    step34();
    step35();
    step36();
    step37();
    step38();
    step39();
    step40();
    step41();
    step42();
    step43();
    step44();
    step45();
    step46();
    step47();
    step48();
    load_config("x");
    after1();
    after2();
    after3();
    after4();
}
"#),
        ("src/raw.rs", r#"pub fn load_config_raw(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
"#),
        ("src/fs.rs", r#"pub fn read_file(dir: &str, path: &str) -> String {
    std::fs::read_to_string(format!("{dir}/{path}")).unwrap_or_default()
}

#[test]
fn reads_what_it_wrote() {
    let text = read_file("/tmp", "x");
    assert!(text.is_empty());
}
"#),
        ("tests/config.rs", r#"#[test]
fn loads_the_defaults() {
    let config = load_config("none");
    assert!(config.is_default());
}
"#),
        ("tests/env.rs", r#"#[test]
fn honours_the_directory() {
    std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
    let config = load_config("a");
    assert_eq!(config.dir(), "/tmp/x");
}
"#),
    ],
    expected: r#"### src/cli.rs:3-6 — run calls load_config
3| fn run(args: &[String]) -> Config {
4|     let config = load_config(&args[0]);
5|     config
6| }
### src/main.rs:3-4 — main (declaration)
3| fn main() {
4|     let args = collect();
### src/main.rs:4-6 — main calls run
4|     let args = collect();
5|     let config = cli::run(&args);
6|     show(config);
### src/long.rs:42-55 — long_caller calls load_config
42|     step41();
43|     step42();
44|     step43();
45|     step44();
46|     step45();
47|     step46();
48|     step47();
49|     step48();
50|     load_config("x");
51|     after1();
52|     after2();
53|     after3();
54|     after4();
55| }
### tests/config.rs:2-5 — loads_the_defaults a test that calls load_config
2| fn loads_the_defaults() {
3|     let config = load_config("none");
4|     assert!(config.is_default());
5| }
### src/raw.rs:1-3 — load_config_raw (helper named after load_config)
1| pub fn load_config_raw(path: &str) -> String {
2|     std::fs::read_to_string(path).unwrap_or_default()
3| }
### src/fs.rs:5-9 — a unit test beside fs.rs, which load_config calls into
5| #[test]
6| fn reads_what_it_wrote() {
7|     let text = read_file("/tmp", "x");
8|     assert!(text.is_empty());
9| }
### tests/env.rs:1-6 — a test that sets PIXEL_CONFIG_DIR
1| #[test]
2| fn honours_the_directory() {
3|     std::env::set_var("PIXEL_CONFIG_DIR", "/tmp/x");
4|     let config = load_config("a");
5|     assert_eq!(config.dir(), "/tmp/x");
6| }
"#,
};

#[test]
fn graph_case_first_resolvable_word_prints_what_the_script_prints() {
    same_as_the_script(&FIRST_RESOLVABLE_WORD);
}

#[test]
fn a_second_page_of_callers_is_read_before_the_first_six_are_chosen() {
    let (out, requests) = ask(&PAGED_CALLERS);
    let offsets: Vec<u64> = requests
        .iter()
        .filter(|request| request["op"] == "uses" && request["role"] == "callers" && request["uid_or_name"] == "U#load_config")
        .map(|request| request["offset"].as_u64().unwrap_or(0))
        .collect();
    assert_eq!(offsets, [0, 20], "two pages, then a short one ends it");
    assert!(out.starts_with("### src/a20.rs:1-3 — p20 calls load_config\n"), "{out}");
}

#[test]
fn the_graph_is_asked_with_the_stale_flag_and_nothing_else_writes() {
    let (_, requests) = ask(&MAIN);
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|request| request["stale_ok"] == true), "{requests:?}");
}

#[test]
fn a_graph_symbol_keeps_the_source_fallbacks_out() {
    let files = [
        ("src/ids.ts", "export const makeElementId = () => nanoid();\n"),
        ("src/canvas.ts", "import { makeElementId } from './ids';\nconst shape = { id: makeElementId() };\n"),
    ];
    let dir = repo(&files);
    let symbol = json!({"uid": "sym-1", "name": "makeElementId", "kind": "function", "path": "src/ids.ts",
                        "start_line": 1, "end_line": 1, "qualified": "makeElementId", "sig": ""});
    let _daemon = FakeGraph::serve(dir.path(), move |request| match request["op"].as_str() {
        Some("symbol") => json!({"symbols": [symbol.clone()]}),
        _ => json!({"edges": [], "edge_limit": 20}),
    });
    let out = run(&STEP, dir.path(), "Which modules call the ID generator makeElementId?", "");
    assert!(!out.contains("ID factory makeElementId"), "{out}");
}

#[test]
fn the_step_applies_to_the_caller_phrasings_and_not_to_other_scope_questions() {
    for prompt in [
        "if I change how req.get reads headers, which parts of lib call it",
        "Which library methods invoke req.get or its header-reading alias?",
        "Which parts of the request module call req.get?",
        "Which modules call randomId?",
        "Which code calls the request parser?",
        "Show callers of the function that creates fresh drawing element IDs.",
        "Locate RequestContext.push and the internal entry points that invoke it.",
    ] {
        assert!(STEP.applies(None, prompt, false), "{prompt}");
        assert!(STEP.applies(None, prompt, true), "a closed gate still runs it: {prompt}");
    }
    for prompt in [
        "Which parts of my vacation did I change?",
        "Which parts of my vacation should call the driver?",
        "Which parts of lib explain request parsing?",
        "Which parts of the repository are impacted?",
    ] {
        assert!(!STEP.applies(None, prompt, false), "{prompt}");
    }
    assert!(!STEP.applies(Some("overview"), "who calls load_config", false));
    assert!(!STEP.applies(Some("review"), "who calls load_config", false));
    assert!(!STEP.applies(Some("history"), "who calls load_config", false));
}

#[test]
fn semantic_id_factory_reports_definition_and_id_assignment_callers() {
    let out = alone(
        "Which modules call the element ID generator?",
        &[
        ("src/ids.ts", r#"export const makeId = () => nanoid();
"#),
        ("src/copy.ts", r#"import { makeId } from './ids';
const copy = { id: makeId() };
"#),
        ("src/store.ts", r#"import { makeId as createId } from './ids';
return { id: createId() };
"#),
    ],
    );
    assert!(out.contains("src/ids.ts"), "{out}");
    assert!(out.contains("src/copy.ts"), "{out}");
    assert!(out.contains("src/store.ts"), "{out}");
    assert!(out.contains("id: makeId()"), "{out}");
}

#[test]
fn semantic_id_factory_resolves_typescript_alias_and_barrel_export() {
    let out = alone(
        "Which modules call the element ID generator?",
        &[
        ("tsconfig.json", r#"{"compilerOptions":{"baseUrl":".","paths":{"@core/*":["src/*"]}}}"#),
        ("src/random.ts", r#"export const makeElementId = () => nanoid();
"#),
        ("src/index.ts", r#"export * from './random';
"#),
        ("src/canvas.ts", r#"import { makeElementId as createId } from '@core/index';
const element = { id: createId() };
"#),
    ],
    );
    assert!(out.contains("src/random.ts"), "{out}");
    assert!(out.contains("src/canvas.ts"), "{out}");
    assert!(out.contains("id: createId()"), "{out}");
}

#[test]
fn semantic_id_factory_follows_renamed_barrel_reexport() {
    let out = alone(
        "Which modules call the element ID generator?",
        &[
        ("src/random.ts", r#"export const randomId = () => Math.random();
"#),
        ("src/index.ts", r#"export { randomId as makeId } from './random';
"#),
        ("src/canvas.ts", r#"import { makeId } from './index';
export const createElement = () => ({ id: makeId() });
"#),
    ],
    );
    assert!(out.contains("src/random.ts"), "{out}");
    assert!(out.contains("src/canvas.ts"), "{out}");
    assert!(out.contains("id: makeId()"), "{out}");
}

#[test]
fn semantic_id_factory_reexport_cache_does_not_poison_cyclic_barrel() {
    let out = alone(
        "Which modules call the element ID generator?",
        &[
        ("src/random.ts", r#"export const randomId = () => Math.random();
"#),
        ("src/index.ts", r#"export * from './cycle-a';
export { randomId as makeId } from './random';
"#),
        ("src/cycle-a.ts", r#"export * from './cycle-b';
"#),
        ("src/cycle-b.ts", r#"export * from './cycle-a';
"#),
        ("src/canvas-positive.ts", r#"import { makeId } from './index';
export const createElement = () => ({ id: makeId() });
"#),
        ("src/canvas-negative.ts", r#"import { makeId } from './cycle-b';
export const createElement = () => ({ id: makeId() });
"#),
    ],
    );
    assert!(out.contains("src/canvas-positive.ts"), "{out}");
    assert!(!out.contains("src/canvas-negative.ts"), "{out}");
}

#[test]
fn semantic_two_hop_renamed_reexport_prefilter_keeps_only_bound_consumer() {
    let out = alone(
        "Which modules call the element ID generator?",
        &[
        ("src/random.ts", r#"export const randomId = () => Math.random();
"#),
        ("src/index.ts", r#"export { randomId as makeId } from './random';
"#),
        ("src/public.ts", r#"export { makeId as createId } from './index';
"#),
        ("src/consumer.ts", r#"import { createId } from './public';
export const createElement = () => ({ id: createId() });
"#),
        ("src/unrelated.ts", r#"export const createId = () => 'not-the-factory';
"#),
        ("src/wrong-consumer.ts", r#"import { createId } from './unrelated';
export const unrelated = () => ({ id: createId() });
"#),
        ("src/comment-decoy.ts", r#"// import { createId } from './public';
export const commentOnly = () => ({ id: createId() });
"#),
    ],
    );
    assert!(out.contains("src/consumer.ts"), "{out}");
    assert!(!out.contains("src/wrong-consumer.ts"), "{out}");
    assert!(!out.contains("src/comment-decoy.ts"), "{out}");
}

#[test]
fn semantic_reexport_text_in_comment_or_string_does_not_bind_factory_call() {
    let out = alone(
        "Which modules call the element ID generator?",
        &[
        ("src/ids.ts", r#"export const makeElementId = () => nanoid();
"#),
        ("src/barrel.ts", r#"const docs = "export { makeElementId } from './ids';";
// export { makeElementId } from './ids';
"#),
        ("src/consumer.ts", r#"import { makeElementId } from './barrel';
export const createElement = () => ({ id: makeElementId() });
"#),
        ("src/star-barrel.ts", r#"const docs = "export * from './ids';";
"#),
        ("src/star-consumer.ts", r#"import { makeElementId } from './star-barrel';
export const createElement = () => ({ id: makeElementId() });
"#),
    ],
    );
    assert!(!out.contains("src/consumer.ts"), "{out}");
    assert!(!out.contains("src/star-consumer.ts"), "{out}");
}

#[test]
fn semantic_fake_factory_export_text_does_not_bind_factory_import() {
    let out = alone(
        "Which modules call the element ID generator?",
        &[
        ("src/ids.ts", r#"const makeElementId = () => nanoid();
const docs = "export const makeElementId = () => fake(); export { makeElementId };";
"#),
        ("src/consumer.ts", r#"import { makeElementId } from './ids';
export const createElement = () => ({ id: makeElementId() });
"#),
    ],
    );
    assert!(!out.contains("src/consumer.ts"), "{out}");
}

#[test]
fn semantic_factory_local_export_alias_is_kept_by_prefilter() {
    let out = alone(
        "Which modules call the element ID generator?",
        &[
        ("src/random.ts", r#"const randomId = () => Math.random();
export { randomId, randomId as makeId };
"#),
        ("src/consumer.ts", r#"import { makeId } from './random';
export const createElement = () => ({ id: makeId() });
"#),
    ],
    );
    assert!(out.contains("src/consumer.ts"), "{out}");
}

#[test]
fn semantic_shadowed_import_is_not_counted_as_factory_call() {
    let out = alone(
        "Which modules call the element ID generator?",
        &[
        ("src/ids.ts", r#"export const makeElementId = () => nanoid();
"#),
        ("src/canvas.ts", r#"import { makeElementId } from './ids';
function render() { const makeElementId = () => fallback(); return { id: makeElementId() }; }
"#),
    ],
    );
    assert_eq!(out, "");
}

#[test]
fn semantic_id_factory_reference_in_comment_or_string_is_not_a_callsite() {
    let out = alone(
        "Which modules call the element ID generator?",
        &[
        ("src/ids.ts", r#"export const makeElementId = () => nanoid();
"#),
        ("src/canvas.ts", r#"import { makeElementId } from './ids';
// makeElementId() creates an element
const note = 'makeElementId()';
"#),
    ],
    );
    assert_eq!(out, "");
}

#[test]
fn semantic_import_spelled_only_in_comment_does_not_bind_factory_call() {
    let out = alone(
        "Which modules call the element ID generator?",
        &[
        ("src/ids.ts", r#"export const makeElementId = () => nanoid();
"#),
        ("src/canvas.ts", r#"/* import { makeElementId } from './ids'; */
const shape = { id: makeElementId() };
"#),
    ],
    );
    assert_eq!(out, "");
}

#[test]
fn semantic_explicit_member_reports_same_object_header_method_callers() {
    let out = alone(
        "Which parts of lib call req.get?",
        &[
        ("lib/request.js", r#"req.get =
  req.header = function header(name) {
  return this.headers[name];
};
req.range = function range(size) {
  return this.get('Range');
};
req.protocol = function protocol() {
  return this.get('X-Forwarded-Proto');
};
"#),
        ("examples/demo.js", r#"req.get('Content-Type');
"#),
    ],
    );
    assert!(out.contains("lib/request.js"), "{out}");
    assert!(out.contains("this.get('Range')"), "{out}");
    assert!(out.contains("this.get('X-Forwarded-Proto')"), "{out}");
    assert!(!out.contains("examples/demo.js"), "{out}");
}

#[test]
fn semantic_request_context_push_binds_factory_receiver_and_method_owner() {
    let out = alone(
        "Which code calls the request context when it is pushed?",
        &[
        ("src/ctx.py", r#"class _Globals:
    def push(self):
        pass

class AppContext:
    """A request context merged into the app context."""
    def push(self):
        self.activate_request()
"#),
        ("src/app.py", r#"class App:
    def request_context(self, env):
        return AppContext(env)

    def wsgi_app(self, env):
        ctx = self.request_context(env)
        ctx.push()
"#),
    ],
    );
    assert!(out.contains("AppContext.push"), "{out}");
    assert!(out.contains("wsgi_app"), "{out}");
    assert!(out.contains("ctx.push()"), "{out}");
    assert!(!out.contains("_Globals.push"), "{out}");
}

#[test]
fn semantic_show_callers_of_fresh_element_id_factory() {
    let out = alone(
        "Show callers of the function that creates fresh drawing element IDs.",
        &[
        ("packages/common/src/random.ts", r#"export const randomId = () => Math.random().toString(36);
"#),
        ("packages/element/src/newElement.ts", r#"import { randomId } from '../../common/src/random';
export const createElement = () => ({ id: randomId() });
"#),
    ],
    );
    assert!(out.contains("ID factory randomId"), "{out}");
    assert!(out.contains("packages/element/src/newElement.ts"), "{out}");
    assert!(out.contains("id: randomId()"), "{out}");
}

#[test]
fn semantic_locate_request_context_alias_and_internal_push_callers() {
    let out = alone(
        "Locate RequestContext.push and the internal entry points that invoke it.",
        &[
        ("src/flask/ctx.py", r#"class AppContext:
    """An app context contains request information.\n    Merged with RequestContext; the alias will be removed."""
    def push(self):
        self.activate_request()

    def __enter__(self):
        self.push()
        return self

def __getattr__(name):
    if name == "RequestContext":
        return AppContext
    raise AttributeError(name)
"#),
        ("src/flask/app.py", r#"class Flask:
    def request_context(self, environ) -> AppContext:
        return AppContext(environ)

    def wsgi_app(self, environ):
        ctx = self.request_context(environ)
        ctx.push()
"#),
    ],
    );
    assert!(out.contains("RequestContext"), "{out}");
    assert!(out.contains("RequestContext alias resolves to merged AppContext"), "{out}");
    assert!(out.contains("AppContext.push"), "{out}");
    assert!(out.contains("__enter__"), "{out}");
    assert!(out.contains("wsgi_app"), "{out}");
    assert!(out.contains("ctx.push()"), "{out}");
}

#[test]
fn semantic_generic_workflow_id_question_abstains() {
    let out = alone(
        "Can you generate an ID for this workflow ticket?",
        &[
        ("src/ids.ts", r#"export const makeId = () => nanoid();
"#),
        ("src/copy.ts", r#"const copy = { id: makeId() };
"#),
    ],
    );
    assert_eq!(out, "");
}

#[test]
fn semantic_element_lookup_name_is_not_a_random_id_factory() {
    let out = alone(
        "Which modules call the element ID generator?",
        &[
        ("src/dom.ts", r#"export const getElementById = (id) => document.getElementById(id);
export const unrelated = () => {
  const random = Math.random();
  return random;
};
"#),
        ("src/use.ts", r#"const element = getElementById('canvas');
"#),
    ],
    );
    assert_eq!(out, "");
}

#[test]
fn semantic_same_named_id_factories_in_different_modules_are_ambiguous() {
    let out = alone(
        "Which modules call the element ID generator?",
        &[
        ("src/ids/element.ts", r#"export const makeElementId = () => nanoid();
"#),
        ("src/ids/copy.ts", r#"export const makeElementId = () => lookupElement();
"#),
        ("src/canvas.ts", r#"import { makeElementId } from './ids/element';
const shape = { id: makeElementId() };
"#),
        ("src/duplicate.ts", r#"import { makeElementId } from './ids/copy';
const duplicate = { id: makeElementId() };
"#),
    ],
    );
    assert_eq!(out, "");
}

#[test]
fn semantic_member_fallback_ignores_other_objects_this_method() {
    let out = alone(
        "Which parts of lib call req.get?",
        &[
        ("lib/request.js", r#"req.get = function get(name) { return headers[name]; };
req.range = function range(size) { return this.get('Range'); };
class OtherRequest {
  range() { return this.get('not a request header'); }
}
"#),
    ],
    );
    assert!(out.contains("this.get('Range')"), "{out}");
    assert!(!out.contains("not a request header"), "{out}");
}

#[test]
fn semantic_getter_helper_is_followed_only_with_receiver_binding_proof() {
    let out = alone(
        "Which parts of lib call req.get?",
        &[
        ("lib/request.js", r#"req.get = function get(name) { return headers[name]; };
function defineGetter(obj, name, getter) {
  Object.defineProperty(obj, name, { configurable: true, get: getter });
}
defineGetter(req, 'protocol', function protocol() {
  return this.get('X-Forwarded-Proto');
});
"#),
    ],
    );
    assert!(out.contains("this.get('X-Forwarded-Proto')"), "{out}");
    assert!(out.contains("getter calls this.get on the proven receiver"), "{out}");
}

#[test]
fn semantic_getter_call_does_not_trust_comment_or_string_helper_definition() {
    let out = alone(
        "Which parts of lib call req.get?",
        &[
        ("lib/request.js", r#"req.get = function get(name) { return headers[name]; };
/* function defineGetter(obj, name, getter) { Object.defineProperty(obj, name, { get: getter }); } */
const note = "function defineGetter(obj, name, getter) {";
defineGetter(req, 'protocol', function protocol() {
  return this.get('X-Forwarded-Proto');
});
"#),
    ],
    );
    assert!(!out.contains("X-Forwarded-Proto"), "{out}");
}

#[test]
fn semantic_id_scan_skips_irrelevant_source() {
    let out = alone(
        "Which modules call the element ID generator?",
        &[
        ("src/ids.ts", r#"export const makeElementId = () => nanoid();
"#),
        ("src/canvas.ts", r#"import { makeElementId } from './ids';
const element = { id: makeElementId() };
"#),
        ("src/noise.ts", r#"export const unrelated = () => true;
"#),
    ],
    );
    assert!(out.contains("src/canvas.ts"), "{out}");
    assert!(!out.contains("unrelated"), "{out}");
}

fn source_case(files: &[(&str, &str)]) -> tempfile::TempDir {
    repo(files)
}

#[test]
fn tracked_source_puts_the_paths_that_share_words_with_the_question_first() {
    let dir = source_case(&[
        ("aaa/noise.py", "def unrelated():\n    return True\n"),
        ("zzz/element_ids.py", "def make_element_id():\n    return uuid4()\n"),
    ]);
    let ctx = Ctx::new(dir.path(), "", "", Instant::now() + Duration::from_secs(10));
    let limits = Limits { files: 10, bytes: 1 << 20, scan: Duration::from_secs(5) };
    let (sources, partial) = tracked_source_within(&ctx, "Show callers of fresh drawing element IDs", &limits);
    assert!(!partial);
    assert_eq!(sources.order, ["zzz/element_ids.py", "aaa/noise.py"]);
}

#[test]
fn tracked_source_enforces_the_file_cap() {
    let dir = source_case(&[
        ("a.js", "const value = 1;\n"),
        ("b.js", "const value = 1;\n"),
        ("c.js", "const value = 1;\n"),
        ("d.js", "const value = 1;\n"),
    ]);
    let ctx = Ctx::new(dir.path(), "", "", Instant::now() + Duration::from_secs(10));
    let limits = Limits { files: 2, bytes: 1 << 20, scan: Duration::from_secs(5) };
    let (sources, partial) = tracked_source_within(&ctx, "", &limits);
    assert_eq!(sources.order, ["a.js", "b.js"]);
    assert!(partial);
}

#[test]
fn tracked_source_enforces_the_byte_cap() {
    let dir = source_case(&[("a.js", "abc\n"), ("b.js", "abc\n")]);
    let ctx = Ctx::new(dir.path(), "", "", Instant::now() + Duration::from_secs(10));
    let limits = Limits { files: 10, bytes: 5, scan: Duration::from_secs(5) };
    let (sources, partial) = tracked_source_within(&ctx, "", &limits);
    assert_eq!(sources.order, ["a.js"]);
    assert!(partial);
}

#[test]
fn tracked_source_with_the_clock_run_out_is_empty_and_partial() {
    let dir = source_case(&[("a.js", "const value = 1;\n")]);
    let ctx = Ctx::new(dir.path(), "", "", Instant::now());
    let limits = Limits { files: 10, bytes: 1 << 20, scan: Duration::from_secs(5) };
    let (sources, partial) = tracked_source_within(&ctx, "", &limits);
    assert!(sources.order.is_empty());
    assert!(partial);
}

#[test]
fn a_unit_is_found_around_a_line_and_capped() {
    let lines: Vec<String> = ["// doc", "pub fn go() {", "    one();", "    two();", "}", "", "fn other() {}"]
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(unit_around(&lines, 3, 22), Some((1, 5)), "the doc comment is part of the unit");
    assert_eq!(unit_around(&lines, 3, 3), Some((1, 3)), "capped");
    assert_eq!(unit_around(&lines, 6, 22), None, "a line after the unit closed");
}
