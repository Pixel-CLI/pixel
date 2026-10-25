// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The step on small repositories, with the script's own output as the
//! expectation: each `EXPECTED_*` constant is what `probes/cand-j-stop-cause/probe`
//! printed on the same files, prompt and brief.

use serde_json::{Value, json};

use super::super::graph::testing::{FakeGraph, sym};
use super::super::testing::{repo, run};
use super::*;

const WORKER: &str = r#"use std::sync::mpsc::Receiver;

pub struct Worker {
    queue: Receiver<Job>,
    done: bool,
}

impl Worker {
    pub fn new(queue: Receiver<Job>) -> Self {
        Self { queue, done: false }
    }

    /// Runs until the queue is empty.
    pub fn run(&mut self) {
        while !self.done {
            match self.queue.try_recv() {
                Ok(job) => job.execute(),
                Err(_) => {
                    self.halt();
                }
            }
        }
    }

    fn halt(&mut self) {
        // the worker stops as soon as the queue is empty
        self.done = true;
    }
}
"#;
const QUEUE: &str = r#"pub struct Job {
    pub id: u64,
}

impl Job {
    pub fn execute(&self) {
        println!("job {}", self.id);
    }
}

pub fn queue_is_empty(items: &[Job]) -> bool {
    items.is_empty()
}
"#;
const MAIN: &str = r#"mod worker;
mod queue;

fn main() {
    let (tx, rx) = channel();
    worker::Worker::new(rx).run();
}
"#;
const UTIL: &str = r#"pub fn add(a: u32, b: u32) -> u32 {
    a + b
}
"#;
const CONFIG: &str = r#"pub struct Config {
    pub name: String,
}
"#;
const NET: &str = r#"pub fn connect(host: &str) -> bool {
    !host.is_empty()
}
"#;
const TESTS: &str = r#"use crate::worker::Worker;

#[test]
fn the_worker_stops_when_the_queue_is_empty() {
    let (tx, rx) = channel();
    drop(tx);
    let mut worker = Worker::new(rx);
    worker.run();
    assert!(worker.is_done());
}

#[test]
fn the_worker_keeps_going_while_the_queue_has_jobs() {
    let (tx, rx) = channel();
    tx.send(Job { id: 1 }).unwrap();
    let mut worker = Worker::new(rx);
    worker.step();
    assert!(!worker.is_done());
}
"#;
const DOC: &str = r##"pub const HELP: &str = r#"
  --flag0  the worker stops when the queue is empty, flag 0
  --flag1  the worker stops when the queue is empty, flag 1
  --flag2  the worker stops when the queue is empty, flag 2
  --flag3  the worker stops when the queue is empty, flag 3
  --flag4  the worker stops when the queue is empty, flag 4
  --flag5  the worker stops when the queue is empty, flag 5
  --flag6  the worker stops when the queue is empty, flag 6
  --flag7  the worker stops when the queue is empty, flag 7
  --flag8  the worker stops when the queue is empty, flag 8
  --flag9  the worker stops when the queue is empty, flag 9
  --flag10  the worker stops when the queue is empty, flag 10
  --flag11  the worker stops when the queue is empty, flag 11
  --flag12  the worker stops when the queue is empty, flag 12
  --flag13  the worker stops when the queue is empty, flag 13
  --flag14  the worker stops when the queue is empty, flag 14
  --flag15  the worker stops when the queue is empty, flag 15
  --flag16  the worker stops when the queue is empty, flag 16
  --flag17  the worker stops when the queue is empty, flag 17
  --flag18  the worker stops when the queue is empty, flag 18
  --flag19  the worker stops when the queue is empty, flag 19
  --flag20  the worker stops when the queue is empty, flag 20
  --flag21  the worker stops when the queue is empty, flag 21
  --flag22  the worker stops when the queue is empty, flag 22
  --flag23  the worker stops when the queue is empty, flag 23
  --flag24  the worker stops when the queue is empty, flag 24
  --flag25  the worker stops when the queue is empty, flag 25
  --flag26  the worker stops when the queue is empty, flag 26
  --flag27  the worker stops when the queue is empty, flag 27
  --flag28  the worker stops when the queue is empty, flag 28
  --flag29  the worker stops when the queue is empty, flag 29
  --flag30  the worker stops when the queue is empty, flag 30
  --flag31  the worker stops when the queue is empty, flag 31
  --flag32  the worker stops when the queue is empty, flag 32
  --flag33  the worker stops when the queue is empty, flag 33
  --flag34  the worker stops when the queue is empty, flag 34
  --flag35  the worker stops when the queue is empty, flag 35
  --flag36  the worker stops when the queue is empty, flag 36
  --flag37  the worker stops when the queue is empty, flag 37
  --flag38  the worker stops when the queue is empty, flag 38
  --flag39  the worker stops when the queue is empty, flag 39
"#;

pub fn help() -> &'static str {
    HELP
}
"##;

const PROMPT: &str = "why does the worker stop when the queue is empty";

/// The ten files of the repository every case starts from.
fn files() -> Vec<(String, String)> {
    let mut all: Vec<(String, String)> = [
        ("src/worker.rs", WORKER),
        ("src/queue.rs", QUEUE),
        ("src/main.rs", MAIN),
        ("src/util.rs", UTIL),
        ("src/config.rs", CONFIG),
        ("src/net.rs", NET),
    ]
    .into_iter()
    .map(|(path, text)| (path.to_owned(), text.to_owned()))
    .collect();
    for i in 0..4 {
        all.push((
            format!("src/extra{i}.rs"),
            format!("pub fn extra{i}(x: u32) -> u32 {{\n    x + {i}\n}}\n"),
        ));
    }
    all
}

/// `files()` with `extra` added or replaced.
fn repo_with(extra: &[(&str, &str)]) -> tempfile::TempDir {
    let mut all = files();
    for (path, text) in extra {
        all.retain(|(known, _)| known != path);
        all.push(((*path).to_owned(), (*text).to_owned()));
    }
    let borrowed: Vec<(&str, &str)> = all.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    repo(&borrowed)
}

fn ask(dir: &tempfile::TempDir, prompt: &str, brief: &str) -> String {
    run(&STEP, dir.path(), prompt, brief)
}

const EXPECTED_BASIC: &str = r#"### src/worker.rs:2-29 — where the question's words meet (code)
3| pub struct Worker {
4|     queue: Receiver<Job>,
5|     done: bool,
6| }
8| impl Worker {
9|     pub fn new(queue: Receiver<Job>) -> Self {
10|         Self { queue, done: false }
11|     }
13|     /// Runs until the queue is empty.
14|     pub fn run(&mut self) {
15|         while !self.done {
16|             match self.queue.try_recv() {
17|                 Ok(job) => job.execute(),
18|                 Err(_) => {
19|                     self.halt();
20|                 }
21|             }
22|         }
23|     }
25|     fn halt(&mut self) {
26|         // the worker stops as soon as the queue is empty
27|         self.done = true;
28|     }
29| }
"#;
const EXPECTED_TESTS_ASKED: &str = r#"### src/worker.rs:2-29 — where the question's words meet (code)
3| pub struct Worker {
4|     queue: Receiver<Job>,
5|     done: bool,
6| }
8| impl Worker {
9|     pub fn new(queue: Receiver<Job>) -> Self {
10|         Self { queue, done: false }
11|     }
13|     /// Runs until the queue is empty.
14|     pub fn run(&mut self) {
15|         while !self.done {
16|             match self.queue.try_recv() {
17|                 Ok(job) => job.execute(),
18|                 Err(_) => {
19|                     self.halt();
20|                 }
21|             }
22|         }
23|     }
25|     fn halt(&mut self) {
26|         // the worker stops as soon as the queue is empty
27|         self.done = true;
28|     }
29| }
### tests/worker.rs:3-10 — where the question's words meet (test)
3| #[test]
4| fn the_worker_stops_when_the_queue_is_empty() {
5|     let (tx, rx) = channel();
6|     drop(tx);
7|     let mut worker = Worker::new(rx);
8|     worker.run();
9|     assert!(worker.is_done());
10| }
### tests/worker.rs:12-19 — where the question's words meet (test)
12| #[test]
13| fn the_worker_keeps_going_while_the_queue_has_jobs() {
14|     let (tx, rx) = channel();
15|     tx.send(Job { id: 1 }).unwrap();
16|     let mut worker = Worker::new(rx);
17|     worker.step();
18|     assert!(!worker.is_done());
19| }
"#;
const EXPECTED_BRIEF_DROP: &str = r#"drop: ^(?:src/main\.rs:1\-6|src/net\.rs:1\-3)
### src/worker.rs:2-29 — where the question's words meet (code)
3| pub struct Worker {
4|     queue: Receiver<Job>,
5|     done: bool,
6| }
8| impl Worker {
9|     pub fn new(queue: Receiver<Job>) -> Self {
10|         Self { queue, done: false }
11|     }
13|     /// Runs until the queue is empty.
14|     pub fn run(&mut self) {
15|         while !self.done {
16|             match self.queue.try_recv() {
17|                 Ok(job) => job.execute(),
18|                 Err(_) => {
19|                     self.halt();
20|                 }
21|             }
22|         }
23|     }
25|     fn halt(&mut self) {
26|         // the worker stops as soon as the queue is empty
27|         self.done = true;
28|     }
29| }
"#;
const EXPECTED_FORM_FEED: &str = r#"### src/worker.rs:2-31 — where the question's words meet (code)
3| pub struct Worker {
4|     queue: Receiver<Job>,
5|     done: bool,
6| }
8| impl Worker {
9|     pub fn new(queue: Receiver<Job>) -> Self {
10|         Self { queue, done: false }
11|     }
13|     /// Runs until the queue is empty.
14|     pub fn run(&mut self) {
15|         while !self.done {
16|             match self.queue.try_recv() {
17|                 Ok(job) => job.execute(),
18|                 Err(_) => {
19|                     self.halt();
20|                 }
21|             }
22|         }
23|     }
27|     fn halt(&mut self) {
28|         // the worker stops as soon as the queue is empty
29|         self.done = true;
30|     }
31| }
"#;
const EXPECTED_BRIEF_BOOST: &str = r#"drop: ^(?:src/worker\.rs:1\-30)
### src/worker.rs:2-29 — where the question's words meet (code)
3| pub struct Worker {
4|     queue: Receiver<Job>,
5|     done: bool,
6| }
8| impl Worker {
9|     pub fn new(queue: Receiver<Job>) -> Self {
10|         Self { queue, done: false }
11|     }
13|     /// Runs until the queue is empty.
14|     pub fn run(&mut self) {
15|         while !self.done {
16|             match self.queue.try_recv() {
17|                 Ok(job) => job.execute(),
18|                 Err(_) => {
19|                     self.halt();
20|                 }
21|             }
22|         }
23|     }
25|     fn halt(&mut self) {
26|         // the worker stops as soon as the queue is empty
27|         self.done = true;
28|     }
29| }
"#;
const EXPECTED_WITH_CALLER: &str = r#"### src/worker.rs:2-29 — where the question's words meet (code)
3| pub struct Worker {
4|     queue: Receiver<Job>,
5|     done: bool,
6| }
8| impl Worker {
9|     pub fn new(queue: Receiver<Job>) -> Self {
10|         Self { queue, done: false }
11|     }
13|     /// Runs until the queue is empty.
14|     pub fn run(&mut self) {
15|         while !self.done {
16|             match self.queue.try_recv() {
17|                 Ok(job) => job.execute(),
18|                 Err(_) => {
19|                     self.halt();
20|                 }
21|             }
22|         }
23|     }
25|     fn halt(&mut self) {
26|         // the worker stops as soon as the queue is empty
27|         self.done = true;
28|     }
29| }
### src/worker.rs:14-23 — where the question's words meet (code)
14|     pub fn run(&mut self) {
15|         while !self.done {
16|             match self.queue.try_recv() {
17|                 Ok(job) => job.execute(),
18|                 Err(_) => {
19|                     self.halt();
20|                 }
21|             }
22|         }
23|     }
"#;
const EXPECTED_TESTS_ASKED_ONLY_SOURCE: &str = r#"### src/worker.rs:2-29 — where the question's words meet (code)
3| pub struct Worker {
4|     queue: Receiver<Job>,
5|     done: bool,
6| }
8| impl Worker {
9|     pub fn new(queue: Receiver<Job>) -> Self {
10|         Self { queue, done: false }
11|     }
13|     /// Runs until the queue is empty.
14|     pub fn run(&mut self) {
15|         while !self.done {
16|             match self.queue.try_recv() {
17|                 Ok(job) => job.execute(),
18|                 Err(_) => {
19|                     self.halt();
20|                 }
21|             }
22|         }
23|     }
25|     fn halt(&mut self) {
26|         // the worker stops as soon as the queue is empty
27|         self.done = true;
28|     }
29| }
"#;

#[test]
fn the_window_where_the_words_meet_is_shown_with_its_numbers() {
    let dir = repo_with(&[]);
    assert_eq!(ask(&dir, PROMPT, ""), EXPECTED_BASIC);
}

#[test]
fn a_question_about_tests_adds_the_tests_that_carry_the_words() {
    let dir = repo_with(&[("tests/worker.rs", TESTS)]);
    let prompt = format!("{PROMPT}? is there a test that proves it");
    assert_eq!(ask(&dir, &prompt, ""), EXPECTED_TESTS_ASKED);
    assert_eq!(ask(&dir, PROMPT, ""), EXPECTED_TESTS_ASKED_ONLY_SOURCE);
}

#[test]
fn brief_blocks_that_are_not_definitions_are_dropped_by_their_range() {
    let dir = repo_with(&[]);
    let brief = "[PIXEL:BRIEF]\n### src/queue.rs:1-9 — Job definition\n1|pub struct Job {\n### src/main.rs:1-6 — entry\n1|mod worker;\n### src/net.rs:1-3 — unrelated\n1|pub fn connect\n";
    assert_eq!(ask(&dir, PROMPT, brief), EXPECTED_BRIEF_DROP);
}

#[test]
fn a_window_the_brief_already_shows_is_not_shown_again() {
    let dir = repo_with(&[]);
    for label in ["Worker definition", "it is named by the prompt"] {
        let brief = format!("[PIXEL:BRIEF]\n### src/worker.rs:2-29 — {label}\n2|x\n");
        assert_eq!(ask(&dir, PROMPT, &brief), "", "{label}");
    }
}

#[test]
fn a_window_the_brief_holds_weighs_more_and_its_block_is_dropped() {
    let dir = repo_with(&[]);
    let brief = "[PIXEL:BRIEF]\n### src/worker.rs:1-30 — worker code\n2|x\n";
    assert_eq!(ask(&dir, PROMPT, brief), EXPECTED_BRIEF_BOOST);
}

#[test]
fn a_multi_line_string_is_not_a_window() {
    let dir = repo_with(&[("src/help.rs", DOC)]);
    assert_eq!(ask(&dir, PROMPT, ""), EXPECTED_BASIC);
}

#[test]
fn lines_end_where_pythons_splitlines_ends_them() {
    let text = WORKER.replace("    fn halt", "\x0c\n    fn halt");
    let dir = repo_with(&[("src/worker.rs", &text)]);
    assert_eq!(ask(&dir, PROMPT, ""), EXPECTED_FORM_FEED);
}

#[test]
fn fewer_than_four_files_or_three_rare_words_say_nothing() {
    let two = repo(&[("src/worker.rs", WORKER), ("src/queue.rs", QUEUE)]);
    assert_eq!(ask(&two, PROMPT, ""), "");
    let dir = repo_with(&[]);
    assert_eq!(ask(&dir, "why does it stop", ""), "");
}

#[test]
fn the_first_caller_of_a_short_function_the_words_name_is_added() {
    let dir = repo_with(&[]);
    let halt = sym("W#halt", "halt", "method", "src/worker.rs", (25, 28));
    let caller = sym("W#run", "run", "method", "src/worker.rs", (14, 23));
    let graph = FakeGraph::serve(dir.path(), move |request| match request["op"].as_str() {
        Some("symbol") if request["name"] == "halt" => json!({ "symbols": [halt.clone()] }),
        Some("symbol") => json!({ "symbols": [] }),
        Some("uses") if request["uid_or_name"] == "W#halt" => {
            json!({ "edges": [{ "symbol": caller.clone(), "tier": "exact", "site_line": 19 }] })
        }
        _ => json!({ "edges": [] }),
    });
    assert_eq!(ask(&dir, PROMPT, ""), EXPECTED_WITH_CALLER);
    let asked: Vec<Value> = graph.requests();
    assert!(asked.iter().any(|request| request["op"] == "uses" && request["role"] == "callers"));
}

#[test]
fn a_function_longer_than_twelve_lines_or_unknown_to_the_graph_brings_no_caller() {
    let dir = repo_with(&[]);
    let long = sym("W#halt", "halt", "method", "src/worker.rs", (25, 40));
    let _graph = FakeGraph::serve(dir.path(), move |request| match request["op"].as_str() {
        Some("symbol") => json!({ "symbols": [long.clone()] }),
        _ => json!({ "edges": [] }),
    });
    assert_eq!(ask(&dir, PROMPT, ""), EXPECTED_BASIC);
}

fn lines(text: &str) -> Vec<String> {
    text.split('\n').map(ToString::to_string).collect()
}

fn unit(first: usize, last: usize, name: &str) -> Unit {
    Unit { first, last, name: name.to_owned() }
}

#[test]
fn the_tests_of_a_file_are_found_by_mark_by_name_or_by_file() {
    let text = lines(UNITS);
    assert_eq!(
        units("src/a.rs", &text),
        [unit(3, 8, "a_test"), unit(14, 21, "test_py"), unit(19, 21, "stops here")]
    );
    assert_eq!(
        units("tests/a.rs", &text),
        [
            unit(3, 8, "a_test"),
            unit(10, 12, "plain"),
            unit(14, 21, "test_py"),
            unit(19, 21, "stops here")
        ]
    );
}

const UNITS: &str = "use std::fmt;

/// docs
#[test]
fn a_test() {
    let x = \"}\";
    assert!(x.len() == 1); // }
}

fn plain() {
    one();
}

def test_py():
    assert 1

    assert 2

it('stops here', () => {
  expect(1);
});
";

#[test]
fn string_literals_that_span_lines_are_prose() {
    let text = lines("let a = r#\"x\ninside\n\"#;\nlet b = \"\"\"\nin\n\"\"\"\nafter");
    assert_eq!(prose_lines(&text), [true, true, true, true, true, true, false]);
}

#[test]
fn a_code_like_line_ends_a_statement_or_calls_something() {
    let got: Vec<bool> = ["", "// c", "foo(", "x;", "plain words", "  } ", "'q'", "#[test]", "a,"]
        .iter()
        .map(|line| code_like(line))
        .collect();
    assert_eq!(got, [false, false, true, true, false, true, false, false, true]);
}

#[test]
fn a_window_is_widened_to_whole_comments_and_braces() {
    let text = lines("// a\n// b\n// c\nfn x() {\n  y();\n}\n// d\nfn z() {\n  w();\n  if q {\n    r();\n  }\n}\ntail");
    assert_eq!(snap(&text, 2, 5), (1, 6));
    assert_eq!(snap(&text, 8, 9), (8, 13));
    assert_eq!(snap(&text, 1, 3), (1, 3));
    assert_eq!(snap(&text, 3, 4), (1, 6));
}

fn groups() -> Vec<Group> {
    let set = |words: &[&str]| words.iter().map(ToString::to_string).collect();
    vec![(1.0, set(&["stop", "halt"])), (2.0, set(&["queue"]))]
}

#[test]
fn a_window_is_cut_to_the_lines_with_the_words_and_two_around_them() {
    let mut text = vec!["a".to_owned(); 30];
    text.push("x".to_owned());
    assert_eq!(trim(&text, 1, 30, &groups()), (1, 30));
    let short = lines("a\nb\nqueue\nd\ne\nf\ng\nh\ni\nj");
    assert_eq!(trim(&short, 1, 10, &groups()), (1, 8));
}

#[test]
fn a_window_is_centred_on_the_weighted_words() {
    let mut text: Vec<String> = (1..=20).map(|n| format!("l{n}")).collect();
    text[9] = "queue stop".to_owned();
    text[10] = "queue".to_owned();
    assert_eq!(recentre(&text, 1, 8, &groups()), (1, 8));
    assert_eq!(recentre(&text, 1, 8, &[(1.0, ["zzz".to_owned()].into())]), (1, 8));
    assert_eq!(recentre(&text, 8, 15, &groups()), (6, 13));
    assert_eq!(recentre(&text, 13, 20, &groups()), (13, 20));
}
