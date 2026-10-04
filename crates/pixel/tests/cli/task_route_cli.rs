// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Real CLI routing against a bounded fake classifier; no global environment mutation.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pixel_task::{Store, Task, TrajectoryEvent};
use serde_json::{Value, json};

use super::support::{Scratch, git, pixel_command};

struct Classifier {
    base: String,
    count: Arc<AtomicUsize>,
    requests: mpsc::Receiver<Value>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Classifier {
    fn start(delay: Duration, illegal: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let server_count = count.clone();
        let server_stopped = stopped.clone();
        let (sender, requests) = mpsc::channel();
        let (ready, started) = mpsc::sync_channel(0);
        let thread = std::thread::spawn(move || {
            ready.send(()).unwrap();
            while !server_stopped.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        // Park for far less than the 15ms warm-probe budget,
                        // without busy-spinning or relying on TCP for shutdown.
                        // Scheduling can still delay a wall-clock-bounded client.
                        std::thread::park_timeout(Duration::from_micros(100));
                        continue;
                    }
                    Err(error) => panic!("classifier fixture accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).unwrap_or(0) == 0 {
                    continue; // The warm probe opens a connection without an HTTP request.
                }
                assert!(first.starts_with("POST /v1/systemone HTTP/1.1"), "{first}");
                let mut length = None;
                loop {
                    let mut line = String::new();
                    assert_ne!(
                        reader.read_line(&mut line).unwrap(),
                        0,
                        "classifier request ended before the headers were complete"
                    );
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = Some(value.trim().parse::<usize>().unwrap());
                    }
                }
                let mut body = vec![0; length.expect("JSON request has content length")];
                reader.read_exact(&mut body).unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                server_count.fetch_add(1, Ordering::SeqCst);
                sender.send(request.clone()).unwrap();
                let probabilities: serde_json::Map<String, Value> = if illegal {
                    serde_json::Map::from_iter([("finish".into(), json!(1.0))])
                } else {
                    request["questions"]["q1"]["criteria"]
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(|label| {
                            (
                                label.clone(),
                                json!(if label == "prepare" { 0.9 } else { 0.01 }),
                            )
                        })
                        .collect()
                };
                let body = json!({"answers":{"q1":{"type":"choice","probabilities":probabilities,"confidence":0.9}}}).to_string();
                std::thread::sleep(delay);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        started
            .recv_timeout(Duration::from_secs(5))
            .expect("classifier fixture must become ready");
        Self {
            base,
            count,
            requests,
            stopped,
            thread: Some(thread),
        }
    }

    fn next_request(&self) -> Value {
        self.requests
            .recv_timeout(Duration::from_secs(5))
            .expect("classifier request must arrive")
    }
}

impl Drop for Classifier {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        let thread = self.thread.take().unwrap();
        // Unpark cannot fail, and its token survives a race before park_timeout.
        thread.thread().unpark();
        thread.join().unwrap();
    }
}

fn fixture(tag: &str, base: &str, enabled: bool, engine: &str) -> (Scratch, Scratch, Task) {
    let root = Scratch::for_test("task-route", tag);
    let home = Scratch::for_test("task-route-home", tag);
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("source.txt"), "before\n").unwrap();
    std::fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "initial"]);
    std::fs::create_dir_all(home.join(".pixel")).unwrap();
    std::fs::write(home.join(".pixel/config.json"), json!({"classify":{"enabled":enabled,"engine":engine,"ollaya":{"base":base,"argv":["/usr/bin/false"]}}}).to_string()).unwrap();
    let contract = serde_json::from_value(json!({"objective":"fix source","checks":[{"id":"check","argv":["/usr/bin/true"]}],"criteria":[{"id":"acceptance","description":"source is fixed","checks":["check"]}]})).unwrap();
    let task = Store::open(&root)
        .unwrap()
        .begin(contract, "pi", Some("route-test"), "begin")
        .unwrap();
    (root, home, task)
}

fn command(root: &Path, home: &Path, task: &Task, policy: &str) -> Command {
    let mut command = pixel_command();
    command
        .current_dir(root)
        .env("HOME", home)
        .env("PIXEL_METRICS", "0")
        .env("PIXEL_TASK_POLICY", policy)
        .env_remove("PIXEL_TASK_CONTRACT")
        .env_remove("PIXEL_TASK_TELEMETRY_PATH")
        .args(["task", "route", &task.task_id, "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn decode(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn events(root: &Path, task: &Task, kind: &str) -> Vec<TrajectoryEvent> {
    Store::open(root)
        .unwrap()
        .events(&task.task_id)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "observation" && event.data["kind"] == kind)
        .collect()
}

fn assert_fallback(value: &Value) {
    assert!(value["classifier"].is_null());
    assert!(value["frame"]["classifier"].is_null());
    assert_eq!(value["ranked_routes"], value["decision"]["eligible_routes"]);
    assert_eq!(value["recommended"], value["ranked_routes"][0]);
    assert_eq!(value["decision"]["allowed"], false);
}

#[test]
fn task_route_should_skip_disabled_remote_and_nonclassifier_arms() {
    let server = Classifier::start(Duration::ZERO, false);
    for (tag, enabled, engine, policy) in [
        ("disabled", false, "local", "gates_classifier"),
        ("remote", true, "remote", "gates_classifier"),
        ("gates", true, "local", "gates"),
    ] {
        let (root, home, task) = fixture(tag, &server.base, enabled, engine);
        assert_fallback(&decode(
            command(&root, &home, &task, policy).output().unwrap(),
        ));
        assert_eq!(events(&root, &task, "route_attempt").len(), 0);
        assert_eq!(events(&root, &task, "route").len(), 1);
    }
    assert_eq!(server.count.load(Ordering::SeqCst), 0);
}

#[test]
fn task_route_should_cache_the_prediction_and_reclassify_changed_source() {
    let server = Classifier::start(Duration::ZERO, false);
    let (root, home, task) = fixture("cache", &server.base, true, "local");
    let first = decode(
        command(&root, &home, &task, "gates_classifier")
            .output()
            .unwrap(),
    );
    assert!(
        !first["classifier"].is_null(),
        "warm classifier fixture must produce a prediction before awaiting its request; route={first}; attempts={:?}",
        events(&root, &task, "route_attempt")
    );
    let request = server.next_request();
    assert_eq!(request["state"], "fix source");
    assert_eq!(request["model"], "winnow:e4b");
    assert_eq!(first["recommended"], "prepare");
    assert_eq!(first["classifier"]["model"], "winnow:e4b");
    assert_eq!(first["decision"]["allowed"], false);
    assert_eq!(
        first["frame"]["classifier"]["ranked_routes"],
        first["ranked_routes"]
    );
    let mut ranked: Vec<String> = serde_json::from_value(first["ranked_routes"].clone()).unwrap();
    let mut eligible: Vec<String> =
        serde_json::from_value(first["decision"]["eligible_routes"].clone()).unwrap();
    ranked.sort();
    eligible.sort();
    assert_eq!(ranked, eligible);
    assert_eq!(
        decode(
            command(&root, &home, &task, "gates_classifier")
                .output()
                .unwrap()
        ),
        first
    );
    assert_eq!(server.count.load(Ordering::SeqCst), 1);
    assert_eq!(events(&root, &task, "route_attempt").len(), 1);
    assert_eq!(events(&root, &task, "route").len(), 1);
    let telemetry = events(&root, &task, "telemetry");
    assert_eq!(telemetry.len(), 1);
    assert_eq!(telemetry[0].data["data"]["actor"], "classifier");
    assert!(telemetry[0].data["data"]["duration_ms"].is_u64());
    std::fs::write(root.join("source.txt"), "changed\n").unwrap();
    let changed = decode(
        command(&root, &home, &task, "gates_classifier")
            .output()
            .unwrap(),
    );
    assert_ne!(
        changed["frame"]["current_source_id"],
        first["frame"]["current_source_id"]
    );
    assert_eq!(server.count.load(Ordering::SeqCst), 2);
    assert_eq!(events(&root, &task, "route_attempt").len(), 2);
}

#[test]
fn task_route_should_recover_frozen_classifier_telemetry_without_reinference() {
    let server = Classifier::start(Duration::from_millis(25), false);
    let (root, home, task) = fixture("export-retry", &server.base, true, "local");
    let sink = home.join("telemetry.jsonl");
    std::fs::create_dir(&sink).unwrap();
    let run = || {
        command(&root, &home, &task, "gates_classifier")
            .env("PIXEL_TASK_TELEMETRY_PATH", &sink)
            .output()
            .unwrap()
    };
    assert!(!run().status.success());
    assert_eq!(server.count.load(Ordering::SeqCst), 1);
    assert!(events(&root, &task, "route").is_empty());
    let recorded = events(&root, &task, "telemetry");
    assert_eq!(recorded.len(), 1);
    let frozen = recorded[0].data["data"].clone();
    assert_eq!(frozen["actor"], "classifier");
    assert!(frozen["duration_ms"].as_u64().unwrap() >= 25);

    std::fs::remove_dir(&sink).unwrap();
    let recovered = decode(run());
    assert_fallback(&recovered);
    let read_sink = || {
        std::fs::read_to_string(&sink)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(read_sink(), vec![frozen.clone()]);
    assert_eq!(events(&root, &task, "route").len(), 1);

    // Replaying a cached route must retry export too, without changing duration.
    std::fs::remove_file(&sink).unwrap();
    std::fs::create_dir(&sink).unwrap();
    assert!(!run().status.success());
    std::fs::remove_dir(&sink).unwrap();
    assert_eq!(decode(run()), recovered);
    assert_eq!(decode(run()), recovered);
    assert_eq!(read_sink(), vec![frozen.clone(), frozen]);
    assert_eq!(events(&root, &task, "telemetry").len(), 1);
    assert_eq!(events(&root, &task, "route_attempt").len(), 1);
    assert_eq!(server.count.load(Ordering::SeqCst), 1);
}

#[test]
fn task_route_should_give_inference_its_full_budget_after_the_reservation() {
    // The 300ms classify budget must cover inference alone, not the
    // reservation's fsyncs before the call (task_route.rs): the fixture
    // answers just inside the budget, and a route that charges reservation
    // time against it falls back instead of predicting — exactly what a
    // loaded CI runner disk made shards of #703/#704 do at 700ms, and this
    // canary catches at 280ms where the margin is thin on purpose.
    let server = Classifier::start(Duration::from_millis(280), false);
    let (root, home, task) = fixture("inference-budget", &server.base, true, "local");
    let result = decode(
        command(&root, &home, &task, "gates_classifier")
            .output()
            .unwrap(),
    );
    assert!(
        !result["classifier"].is_null(),
        "a 280ms answer must fit the 300ms inference budget: {result}"
    );
    assert_eq!(result["recommended"], "prepare");
    assert_eq!(server.count.load(Ordering::SeqCst), 1);
}

#[test]
fn task_route_should_share_one_inflight_prediction_between_processes() {
    // The route allows the classifier 300ms minus what elapsed before the call
    // (task_route.rs), so the fixture delay must leave runner-load margin
    // inside that budget or the first process falls back and the test flakes
    // (shard 1 of run 37213453475). 60ms still spans a second process spawn;
    // if it misses, the lock and cache still yield one shared request.
    let server = Classifier::start(Duration::from_millis(60), false);
    let (root, home, task) = fixture("concurrent", &server.base, true, "local");
    let first = command(&root, &home, &task, "gates_classifier")
        .spawn()
        .unwrap();
    server.next_request();
    let second = command(&root, &home, &task, "gates_classifier")
        .spawn()
        .unwrap();
    let first = decode(first.wait_with_output().unwrap());
    let second = decode(second.wait_with_output().unwrap());
    assert_eq!(first["recommended"], "prepare");
    assert_eq!(first, second);
    assert_eq!(server.count.load(Ordering::SeqCst), 1);
    assert_eq!(events(&root, &task, "route_attempt").len(), 1);
    assert_eq!(events(&root, &task, "route").len(), 1);
}

#[test]
fn task_route_should_preserve_spent_reservation_after_the_caller_is_killed() {
    let server = Classifier::start(Duration::from_millis(700), false);
    let (root, home, task) = fixture("interrupted", &server.base, true, "local");
    let mut first = command(&root, &home, &task, "gates_classifier")
        .spawn()
        .unwrap();
    server.next_request();
    first.kill().unwrap();
    first.wait().unwrap();
    assert_eq!(events(&root, &task, "route_attempt").len(), 1);
    assert_eq!(events(&root, &task, "route").len(), 0);
    let recovered = decode(
        command(&root, &home, &task, "gates_classifier")
            .output()
            .unwrap(),
    );
    assert_fallback(&recovered);
    assert_eq!(
        decode(
            command(&root, &home, &task, "gates_classifier")
                .output()
                .unwrap()
        ),
        recovered
    );
    assert_eq!(server.count.load(Ordering::SeqCst), 1);
    assert_eq!(events(&root, &task, "route_attempt").len(), 1);
}

#[test]
fn task_route_should_bound_slow_inference_and_reject_illegal_predictions() {
    for (tag, delay, illegal) in [
        ("timeout", Duration::from_millis(700), false),
        ("illegal", Duration::ZERO, true),
    ] {
        let server = Classifier::start(delay, illegal);
        let (root, home, task) = fixture(tag, &server.base, true, "local");
        let started = Instant::now();
        let result = decode(
            command(&root, &home, &task, "gates_classifier")
                .output()
                .unwrap(),
        );
        assert_fallback(&result);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(server.count.load(Ordering::SeqCst), 1);
        assert_eq!(events(&root, &task, "route_attempt").len(), 1);
        assert_eq!(events(&root, &task, "telemetry").len(), 1);
        assert_eq!(
            decode(
                command(&root, &home, &task, "gates_classifier")
                    .output()
                    .unwrap()
            ),
            result
        );
        assert_eq!(server.count.load(Ordering::SeqCst), 1);
    }
}
