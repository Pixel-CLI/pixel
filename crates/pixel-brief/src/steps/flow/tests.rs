// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use std::time::{Duration, Instant};

use super::*;
use crate::chain::{CallerHit, Evidence, Found, RichFound};
use crate::steps::testing::{Repo, texts};
use crate::steps::{Steps, blocks};

/// A repository with the graph answers a flow question reads.
struct Graph {
    repo: Repo,
    symbols: Vec<SymbolHit>,
    callees: BTreeMap<String, Vec<FlowNode>>,
    path: Option<Vec<PathHop>>,
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
    fn callers(&self, target: &str, deadline: Instant) -> Result<Vec<CallerHit>, String> {
        self.repo.callers(target, deadline)
    }
    fn flow_tree(
        &self,
        uid: &str,
        _depth: usize,
        _limit: usize,
        _deadline: Instant,
    ) -> Result<Vec<FlowNode>, String> {
        Ok(self.callees.get(uid).cloned().unwrap_or_default())
    }
    fn call_path(
        &self,
        _from: &str,
        _to: &str,
        _deadline: Instant,
    ) -> Result<Option<Vec<PathHop>>, String> {
        Ok(self.path.clone())
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

fn symbol(path: &str, name: &str, kind: &str, start: u64, end: u64) -> SymbolHit {
    SymbolHit {
        uid: format!("{path}#{name}#{kind}"),
        name: name.to_string(),
        kind: kind.to_string(),
        path: path.to_string(),
        start_line: start,
        end_line: end,
    }
}

fn callee(of: &SymbolHit) -> FlowNode {
    FlowNode {
        depth: 1,
        parent: None,
        uid: of.uid.clone(),
        name: of.name.clone(),
        kind: of.kind.clone(),
        path: of.path.clone(),
        start: of.start_line,
        end: of.end_line,
    }
}

fn hop(of: &SymbolHit, call_line: u64) -> PathHop {
    PathHop {
        symbol: of.clone(),
        call_line,
        likely: false,
    }
}

fn ask(graph: &Graph, typed: &str, kind: QuestionKind) -> crate::steps::Outcome {
    let deadline = Instant::now() + Duration::from_secs(5);
    blocks(typed, kind, Steps::default(), &[], graph, deadline)
}

const SESSION: &str = "pub struct Session {\n    pub id: u32,\n}\n\nimpl Session {\n    pub fn open(id: u32) -> Session {\n        Session { id }\n    }\n}\n";
const SERVER: &str = "pub fn handle(req: Request) -> u32 {\n    let user = authenticate(&req);\n    let session = start_session(user);\n    respond(session)\n}\n";
const AUTH: &str = "pub fn start_session(user: u32) -> Session {\n    Session::open(user)\n}\n";
const REPLY: &str = "pub fn respond(session: Session) -> u32 {\n    session.id\n}\n";
const COOKIE: &str = "pub fn session_cookie(id: u32) -> String {\n    format!(\"sid={id}\")\n}\n";

/// `handle` -> `start_session` -> `Session::open`, with three more callees
/// of `handle`.
fn chain_graph(path: bool) -> Graph {
    let handle = symbol("src/server.rs", "handle", "function", 1, 5);
    let start = symbol("src/auth.rs", "start_session", "function", 1, 3);
    let open = symbol("src/session.rs", "open", "function", 6, 8);
    let authenticate = symbol("src/server.rs", "authenticate", "function", 7, 9);
    let respond = symbol("src/reply.rs", "respond", "function", 1, 3);
    let cookie = symbol("src/cookie.rs", "session_cookie", "function", 1, 3);
    let callees = BTreeMap::from([(
        handle.uid.clone(),
        [&authenticate, &start, &cookie, &respond]
            .into_iter()
            .map(callee)
            .collect(),
    )]);
    Graph {
        repo: Repo::new(&[
            ("src/session.rs", SESSION),
            ("src/server.rs", SERVER),
            ("src/auth.rs", AUTH),
            ("src/reply.rs", REPLY),
            ("src/cookie.rs", COOKIE),
        ]),
        path: path.then(|| vec![hop(&handle, 0), hop(&start, 3), hop(&open, 2)]),
        symbols: vec![
            symbol("src/session.rs", "Session", "struct", 1, 3),
            handle,
            start,
            open,
            authenticate,
            respond,
            cookie,
        ],
        callees,
    }
}

const TRACE: &str = "trace how a request becomes a Session, from handle through to the constructor";

#[test]
fn a_trace_shows_each_hop_the_constructor_and_the_callees_that_take_or_name_the_target() {
    let out = ask(&chain_graph(true), TRACE, QuestionKind::Flow);
    assert!(out.rescuing);
    let heads: Vec<&str> = out.lead.iter().map(Block::head).collect();
    assert_eq!(
        heads,
        [
            "call trace, entry to target",
            "src/server.rs:1-5 — handle calls start_session",
            "src/auth.rs:1-3 — start_session calls open",
            "src/session.rs:6-9 — open (constructor of Session), head",
            "src/reply.rs:1-3 — respond receives the Session",
            "src/cookie.rs:1-3 — session_cookie (called by handle, named by the question)",
        ]
    );
    let shown = texts(&out.lead);
    assert_eq!(
        shown[0],
        "### call trace, entry to target\n\
         chain handle (src/server.rs:1) -> start_session (src/auth.rs:1) -> open (src/session.rs:6)"
    );
    assert_eq!(
        shown[2],
        "### src/auth.rs:1-3 — start_session calls open\n\
         1| pub fn start_session(user: u32) -> Session {\n\
         2|     Session::open(user)\n\
         3| }"
    );
    // The pack's own call trace gives way; its other blocks stay.
    assert!(out.drops.drops_head("call trace for handle"));
    assert!(
        !out.drops
            .drops_head("src/server.rs:1-5 — handle definition (function)")
    );
}

#[test]
fn a_call_matched_by_name_is_marked_in_the_chain() {
    let mut graph = chain_graph(true);
    if let Some(path) = graph.path.as_mut() {
        path[1].likely = true;
    }
    let out = ask(&graph, TRACE, QuestionKind::Flow);
    assert_eq!(
        texts(&out.lead)[0],
        "### call trace, entry to target\n\
         chain handle (src/server.rs:1) ~> start_session (src/auth.rs:1) -> open (src/session.rs:6)\n\
         (~> is a call matched by name, not proved)"
    );
}

#[test]
fn a_long_hop_is_shown_at_its_call_site_not_whole() {
    let mut graph = chain_graph(true);
    let body: String = (0..120).map(|n| format!("    step_{n}();\n")).collect();
    let long = format!("pub fn handle(req: Request) -> u32 {{\n{body}    start_session(1)\n}}\n");
    graph.repo = Repo::new(&[
        ("src/session.rs", SESSION),
        ("src/server.rs", long.as_str()),
        ("src/auth.rs", AUTH),
    ]);
    graph.symbols[1].end_line = 123;
    if let Some(path) = graph.path.as_mut() {
        path[0].symbol.end_line = 123;
        path[1].call_line = 122;
    }
    let out = ask(&graph, TRACE, QuestionKind::Flow);
    let heads: Vec<&str> = out.lead.iter().map(Block::head).collect();
    assert_eq!(
        &heads[1..4],
        [
            "src/server.rs:1-1 — handle (declaration)",
            "src/server.rs:120-123 — handle calls start_session",
            "src/auth.rs:1-3 — start_session calls open",
        ]
    );
}

#[test]
fn no_path_to_a_constructor_leaves_the_pack_alone() {
    assert!(
        ask(&chain_graph(false), TRACE, QuestionKind::Flow)
            .lead
            .is_empty()
    );
}

#[test]
fn only_a_trace_question_with_an_entry_and_a_type_runs_and_the_flag_turns_it_off() {
    let graph = chain_graph(true);
    for prompt in [
        "what does handle do with a Session",
        "trace how a request becomes a Session",
        "trace what handle does",
    ] {
        assert!(
            ask(&graph, prompt, QuestionKind::Flow).lead.is_empty(),
            "{prompt}"
        );
    }
    assert!(ask(&graph, TRACE, QuestionKind::History).lead.is_empty());
    let off = Steps::default()
        .without(&["cand-j-chain".to_string()])
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    assert!(
        blocks(TRACE, QuestionKind::Flow, off, &[], &graph, deadline)
            .lead
            .is_empty()
    );
}

const CLIENT: &str = "pub struct Client;\n\nimpl Client {\n    pub fn fetch(&self) {\n        fetch_headers();\n        retry_request();\n        log_line();\n        parse_body();\n    }\n}\n";
const HEADERS: &str = "pub fn fetch_headers() {\n    set(\"accept\");\n}\n";
const RETRY: &str = "pub fn retry_request() {\n    backoff_delay();\n}\n\npub fn backoff_delay() {\n    sleep(10);\n}\n";
const SMALL: &str = "pub fn small() {\n    x();\n}\n";

fn rank_graph() -> Graph {
    let fetch = symbol("src/client.rs", "fetch", "method", 4, 9);
    let headers = symbol("src/headers.rs", "fetch_headers", "function", 1, 3);
    let retry = symbol("src/retry.rs", "retry_request", "function", 1, 3);
    let backoff = symbol("src/retry.rs", "backoff_delay", "function", 5, 7);
    let log = symbol("src/log.rs", "log_line", "function", 1, 3);
    let body = symbol("src/body.rs", "parse_body", "function", 1, 3);
    let callees = BTreeMap::from([
        (
            fetch.uid.clone(),
            [&headers, &retry, &log, &body]
                .into_iter()
                .map(callee)
                .collect(),
        ),
        (retry.uid.clone(), vec![callee(&backoff)]),
    ]);
    Graph {
        repo: Repo::new(&[
            ("src/client.rs", CLIENT),
            ("src/headers.rs", HEADERS),
            ("src/retry.rs", RETRY),
            ("src/log.rs", SMALL),
            ("src/body.rs", SMALL),
            ("src/other.rs", "pub fn fetch() {}\n"),
        ]),
        path: None,
        symbols: vec![
            symbol("src/other.rs", "fetch", "function", 1, 1),
            fetch,
            headers,
            retry,
            backoff,
            log,
            body,
        ],
        callees,
    }
}

#[test]
fn a_named_entry_shows_its_callees_ranked_by_the_questions_words_down_three_levels() {
    let out = ask(
        &rank_graph(),
        "walk through how `client.fetch` retries a request",
        QuestionKind::Flow,
    );
    assert!(out.rescuing);
    let heads: Vec<&str> = out.lead.iter().map(Block::head).collect();
    assert_eq!(
        heads,
        [
            "call trace, ranked by the question",
            "src/client.rs:4-9 — fetch definition (method)",
            "src/headers.rs:1-3 — fetch_headers definition (function)",
            "src/retry.rs:1-3 — retry_request definition (function)",
            "src/retry.rs:5-7 — backoff_delay definition (function)",
        ]
    );
    assert_eq!(
        texts(&out.lead)[0],
        "### call trace, ranked by the question\n\
         entry fetch — src/client.rs:4-9\n\
         \x20 calls fetch_headers — src/headers.rs:1-3\n\
         \x20 calls retry_request — src/retry.rs:1-3\n\
         \x20   calls backoff_delay — src/retry.rs:5-7"
    );
    assert!(out.drops.drops_head("call graph facts for fetch"));
}

#[test]
fn an_entry_without_callees_or_a_name_in_code_form_adds_nothing() {
    let mut graph = rank_graph();
    graph.callees.clear();
    assert!(
        ask(
            &graph,
            "walk through how `client.fetch` retries a request",
            QuestionKind::Flow
        )
        .lead
        .is_empty()
    );
    assert!(
        ask(
            &rank_graph(),
            "walk through how the client retries a request",
            QuestionKind::Flow
        )
        .lead
        .is_empty()
    );
    assert!(
        ask(
            &rank_graph(),
            "walk through how `client.fetch` retries a request",
            QuestionKind::Lookup
        )
        .lead
        .is_empty()
    );
}

#[test]
fn a_callee_under_a_test_path_never_ranks() {
    let mut graph = rank_graph();
    let helper = symbol("tests/retry_request.rs", "retry_request", "function", 1, 3);
    let fetch_uid = graph.symbols[1].uid.clone();
    graph.callees.insert(fetch_uid, vec![callee(&helper)]);
    graph.repo = Repo::new(&[("src/client.rs", CLIENT), ("tests/retry_request.rs", RETRY)]);
    assert!(
        ask(
            &graph,
            "walk through how `client.fetch` retries a request",
            QuestionKind::Flow
        )
        .lead
        .is_empty()
    );
}

#[test]
fn words_stem_one_inflection_and_a_declaration_names_its_function() {
    assert_eq!(stem_chain("Requests"), "request");
    assert_eq!(stem_chain("opened"), "open");
    assert_eq!(stem_chain("goes"), "goes");
    assert_eq!(
        DECL_NAME
            .captures("    pub async fn open(id: u32) -> Session {")
            .map(|caps| caps[1].to_string()),
        Some("open".to_string())
    );
    assert!(DECL_NAME.captures("let open = 1;").is_none());
}
