// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::*;
use crate::chain::{Evidence, Found, RichFound};
use crate::kinds::FlowNode;
use crate::steps::testing::{Repo, texts};
use crate::steps::{Steps, blocks};

/// A repository with the graph answers a callers question reads.
struct Graph {
    repo: Repo,
    symbols: Vec<SymbolHit>,
    callers: BTreeMap<String, Vec<CallerHit>>,
    callees: BTreeMap<String, Vec<FlowNode>>,
}

impl Evidence for Graph {
    fn files_with(&self, anchor: &str, deadline: Instant) -> Result<Found, String> {
        self.repo.files_with(anchor, deadline)
    }
    fn concept(&self, phrase: &str, deadline: Instant) -> Result<Found, String> {
        self.repo.concept(phrase, deadline)
    }
    fn symbols(&self, name: &str, _deadline: Instant) -> Result<Vec<SymbolHit>, String> {
        Ok(self
            .symbols
            .iter()
            .filter(|s| s.name == name)
            .cloned()
            .collect())
    }
    fn callers(&self, target: &str, _deadline: Instant) -> Result<Vec<CallerHit>, String> {
        Ok(self.callers.get(target).cloned().unwrap_or_default())
    }
    fn flow_tree(
        &self,
        uid: &str,
        _depth: usize,
        _limit: usize,
        _d: Instant,
    ) -> Result<Vec<FlowNode>, String> {
        Ok(self.callees.get(uid).cloned().unwrap_or_default())
    }
    fn tracked_files(&self, deadline: Instant) -> Result<Vec<String>, String> {
        self.repo.tracked_files(deadline)
    }
    fn read_file(&self, path: &str, max: u64, deadline: Instant) -> Result<String, String> {
        self.repo.read_file(path, max, deadline)
    }
    fn grep(&self, pattern: &str, limit: usize, deadline: Instant) -> Result<RichFound, String> {
        self.repo.grep(pattern, limit, deadline)
    }
}

fn symbol(path: &str, name: &str, start: u64, end: u64) -> SymbolHit {
    SymbolHit {
        uid: format!("{path}#{name}#function"),
        name: name.to_string(),
        kind: "function".to_string(),
        path: path.to_string(),
        start_line: start,
        end_line: end,
    }
}

fn caller(path: &str, via: &str, line: u64) -> CallerHit {
    CallerHit {
        path: path.to_string(),
        via: via.to_string(),
        line,
    }
}

const LIMIT: &str = "/// Reads APP_LIMIT.\npub fn read_limit() -> u32 {\n    std::env::var(\"APP_LIMIT\").ok().and_then(|v| v.parse().ok()).unwrap_or(10)\n}\n\npub fn read_limit_raw(text: &str) -> u32 {\n    text.parse().unwrap_or(0)\n}\n";
const APP: &str =
    "use crate::limit::read_limit;\n\npub fn start() -> u32 {\n    read_limit() + 1\n}\n";
const MAIN: &str = "fn main() {\n    app::start();\n}\n";
const TEST: &str = "#[test]\nfn limit_from_env() {\n    std::env::set_var(\"APP_LIMIT\", \"3\");\n    assert_eq!(read_limit(), 3);\n}\n";

fn graph() -> Graph {
    let read_limit = symbol("src/limit.rs", "read_limit", 2, 4);
    let start = symbol("src/app.rs", "start", 3, 5);
    let callers = BTreeMap::from([
        (
            read_limit.uid.clone(),
            vec![
                caller("tests/limit.rs", "limit_from_env", 4),
                caller("src/app.rs", "start", 4),
            ],
        ),
        (start.uid.clone(), vec![caller("src/main.rs", "main", 2)]),
    ]);
    Graph {
        repo: Repo::new(&[
            ("src/limit.rs", LIMIT),
            ("src/app.rs", APP),
            ("src/main.rs", MAIN),
            ("tests/limit.rs", TEST),
        ]),
        symbols: vec![
            read_limit,
            start,
            symbol("src/main.rs", "main", 1, 3),
            symbol("tests/limit.rs", "limit_from_env", 2, 5),
        ],
        callers,
        callees: BTreeMap::new(),
    }
}

fn ask(graph: &Graph, typed: &str, kind: QuestionKind) -> crate::steps::Outcome {
    let deadline = Instant::now() + Duration::from_secs(5);
    blocks(typed, kind, Steps::default(), &[], graph, deadline)
}

#[test]
fn the_callers_their_callers_the_helpers_and_the_tests_that_set_the_environment_are_shown() {
    let out = ask(
        &graph(),
        "who calls read_limit and which tests cover it?",
        QuestionKind::Flow,
    );
    assert!(out.rescuing);
    assert_eq!(
        texts(&out.lead),
        vec![
            "### src/app.rs:3-5 — start calls read_limit\n\
             3| pub fn start() -> u32 {\n\
             4|     read_limit() + 1\n\
             5| }",
            "### src/main.rs:1-3 — main calls start\n\
             1| fn main() {\n\
             2|     app::start();\n\
             3| }",
            "### tests/limit.rs:2-5 — limit_from_env a test that calls read_limit\n\
             2| fn limit_from_env() {\n\
             3|     std::env::set_var(\"APP_LIMIT\", \"3\");\n\
             4|     assert_eq!(read_limit(), 3);\n\
             5| }",
            "### src/limit.rs:6-8 — read_limit_raw (helper named after read_limit)\n\
             6| pub fn read_limit_raw(text: &str) -> u32 {\n\
             7|     text.parse().unwrap_or(0)\n\
             8| }",
            "### tests/limit.rs:1-5 — a test that sets APP_LIMIT\n\
             1| #[test]\n\
             2| fn limit_from_env() {\n\
             3|     std::env::set_var(\"APP_LIMIT\", \"3\");\n\
             4|     assert_eq!(read_limit(), 3);\n\
             5| }",
        ]
    );
}

#[test]
fn the_rest_of_the_pack_loses_the_tests_and_definitions_of_other_functions() {
    let out = ask(
        &graph(),
        "who calls read_limit and which tests cover it?",
        QuestionKind::Flow,
    );
    let block = |head: &str, lines: &[&str]| {
        Block::new(
            head.to_string(),
            lines.iter().map(ToString::to_string).collect(),
        )
    };
    let unrelated = block(
        "tests/x.rs:1-3 — other_works (a test of other)",
        &["1| #[test]", "2| fn other_works() {}"],
    );
    let related = block(
        "tests/y.rs:1-3 — x (a test of read_limit)",
        &["1| assert_eq!(read_limit(), 3);"],
    );
    let sets = block(
        "tests/z.rs:1-3 — z (test)",
        &["1| set_var(\"APP_LIMIT\", \"1\");"],
    );
    let listing = block("tests matching read_limit", &["tests/x.rs:1 other"]);
    let bare_listing = block("tests matching other", &["tests/x.rs:1 other"]);
    let own = block(
        "src/limit.rs:2-4 — read_limit definition (function)",
        &["2| pub fn read_limit() {}"],
    );
    let other = block(
        "src/a.rs:1-3 — parse definition (function)",
        &["1| fn parse() {}"],
    );
    let usage = block(
        "src/b.rs:1-3 — read_limit usage in go",
        &["1| read_limit();"],
    );
    assert!(out.drops.drops(&unrelated));
    assert!(!out.drops.drops(&related));
    assert!(!out.drops.drops(&sets));
    assert!(out.drops.drops(&bare_listing));
    assert!(!out.drops.drops(&listing));
    assert!(!out.drops.drops(&own));
    assert!(out.drops.drops(&other));
    assert!(!out.drops.drops(&usage));
}

#[test]
fn a_long_caller_is_shown_as_a_window_around_the_call() {
    let body: String = (0..50).map(|n| format!("    step_{n}();\n")).collect();
    let big = format!("pub fn big() {{\n{body}    read_limit();\n{body}}}\n");
    let mut g = graph();
    g.repo = Repo::new(&[("src/limit.rs", LIMIT), ("src/big.rs", &big)]);
    g.symbols.push(symbol("src/big.rs", "big", 1, 103));
    g.callers.insert(
        g.symbols[0].uid.clone(),
        vec![caller("src/big.rs", "big", 52)],
    );
    let out = ask(&g, "who calls read_limit", QuestionKind::Flow);
    assert_eq!(
        out.lead[0].head(),
        "src/big.rs:44-60 — big calls read_limit"
    );
}

#[test]
fn a_callee_file_gets_a_unit_test_beside_it() {
    let mut g = graph();
    g.repo = Repo::new(&[
        ("src/limit.rs", LIMIT),
        (
            "src/parse.rs",
            "pub fn parse(t: &str) -> u32 {\n    t.len() as u32\n}\n\n#[test]\nfn parses_text() {\n    assert_eq!(parse(\"ab\"), 2);\n}\n",
        ),
    ]);
    g.callers.clear();
    g.callees.insert(
        g.symbols[0].uid.clone(),
        vec![FlowNode {
            depth: 1,
            parent: Some(0),
            uid: "src/parse.rs#parse#function".into(),
            name: "parse".into(),
            kind: "function".into(),
            path: "src/parse.rs".into(),
            start: 1,
            end: 3,
        }],
    );
    let out = ask(&g, "who calls read_limit", QuestionKind::Flow);
    let heads: Vec<&str> = out.lead.iter().map(Block::head).collect();
    assert!(
        heads.contains(
            &"src/parse.rs:5-8 — a unit test beside parse.rs, which read_limit calls into"
        ),
        "{heads:?}"
    );
}

#[test]
fn only_a_callers_question_about_a_function_the_graph_knows_runs() {
    let g = graph();
    for prompt in ["how does read_limit work", "who calls nothing_known_here"] {
        assert!(
            ask(&g, prompt, QuestionKind::Flow).lead.is_empty(),
            "{prompt}"
        );
    }
    for kind in [
        QuestionKind::Overview,
        QuestionKind::Review,
        QuestionKind::History,
    ] {
        assert!(
            ask(&g, "who calls read_limit", kind).lead.is_empty(),
            "{kind:?}"
        );
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let off = Steps::default()
        .without(&["cand-j-callers".to_string()])
        .unwrap();
    assert!(
        blocks(
            "who calls read_limit",
            QuestionKind::Flow,
            off,
            &[],
            &g,
            deadline
        )
        .lead
        .is_empty()
    );
}

#[test]
fn a_file_the_brief_shows_decides_between_two_functions_of_one_name() {
    let mut g = graph();
    g.symbols.push(symbol("src/other.rs", "read_limit", 1, 3));
    let deadline = Instant::now() + Duration::from_secs(5);
    let near = vec!["src/limit.rs".to_string()];
    let out = blocks(
        "who calls read_limit",
        QuestionKind::Flow,
        Steps::default(),
        &near,
        &g,
        deadline,
    );
    assert!(!out.lead.is_empty());
    // Without a hint, the path order picks `src/limit.rs` over `src/other.rs`
    // as well; with the hint on the other, the callers of that one are asked.
    let near = vec!["src/other.rs".to_string()];
    let out = blocks(
        "who calls read_limit",
        QuestionKind::Flow,
        Steps::default(),
        &near,
        &g,
        deadline,
    );
    assert!(out.lead.is_empty(), "no callers recorded for src/other.rs");
}

#[test]
fn a_declaration_is_found_behind_its_attributes_and_a_closing_brace_must_reach_the_line() {
    let lines: Vec<String> = TEST.lines().map(ToString::to_string).collect();
    assert_eq!(unit_around(&lines, 3, 22), Some((1, 5)));
    assert_eq!(unit_around(&lines, 3, 2), Some((1, 2)));
    let open: Vec<String> = "fn a() {\n    x();\n"
        .lines()
        .map(ToString::to_string)
        .collect();
    assert_eq!(unit_around(&open, 2, 22), None, "no closing brace");
    let past: Vec<String> = "fn a() {\n}\n\nlet x = 1;\n"
        .lines()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        unit_around(&past, 4, 22),
        None,
        "the declaration ended before the line"
    );
}
