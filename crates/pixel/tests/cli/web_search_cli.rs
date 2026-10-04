// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel web-search`: the parse and dispatch contract — query argument,
//! `--limit`, `--json`, and the `marker`/`epistemics`/`snapshot` envelope —
//! exercised end to end against a loopback SearXNG stub. A configured SearXNG
//! is the only provider, so these tests never touch the real DuckDuckGo or
//! Wikipedia endpoints (the unit tests in `web_search.rs` pin that rule on a
//! fetch seam; the test below checks that `run` passes the variable through).

use std::io::{Read, Write};
use std::net::TcpListener;

use crate::support::pixel_command;

/// Serve `body` as a one-shot `200 OK` HTTP response on an ephemeral
/// loopback port; returns the base URL (`http://127.0.0.1:<port>`).
/// `connections` bounds the accept loop so a client that drops early cannot
/// hang the thread; a client that never connects leaves a detached thread
/// blocked on `accept`, which the test process exit reaps.
fn http_stub(body: &'static str, connections: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for _ in 0..connections {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            // Drain the request head; the body is irrelevant.
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[test]
fn web_search_json_reports_hits_with_epistemics_and_snapshot() {
    let base = http_stub(
        r#"{"results":[{"title":"JEV","url":"https://example.test/jev","content":"Joint Embedded Validator","engine":"stub"}]}"#,
        1,
    );
    let out = pixel_command()
        .args(["web-search", "JEV", "--limit", "1", "--json"])
        .env("PIXEL_WEB_SEARCH_URL", &base)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON document");
    assert_eq!(doc["query"], "JEV");
    assert_eq!(doc["marker"], "complete");
    assert_eq!(doc["hits"][0]["url"], "https://example.test/jev");
    assert_eq!(doc["hits"][0]["engine"], "searxng");
    assert_eq!(doc["epistemics"]["closed_world"], false);
    assert_eq!(doc["epistemics"]["basis"], "web search");
    assert_eq!(doc["snapshot"]["limit"], 1);
    assert_eq!(doc["snapshot"]["providers"], serde_json::json!(["searxng"]));
}

#[test]
fn web_search_with_searxng_set_answers_from_it_alone_under_the_limit() {
    // One hit for a limit of 8 used to be topped up from DuckDuckGo and
    // Wikipedia; with the user's own instance configured it no longer is.
    let base = http_stub(
        r#"{"results":[{"title":"JEV","url":"https://example.test/jev","content":"Joint Embedded Validator"}]}"#,
        1,
    );
    let out = pixel_command()
        .args(["web-search", "JEV", "--limit", "8", "--json"])
        .env("PIXEL_WEB_SEARCH_URL", &base)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON document");
    assert_eq!(doc["snapshot"]["providers"], serde_json::json!(["searxng"]));
    assert_eq!(
        doc["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["url"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["https://example.test/jev"]
    );
}

#[test]
fn web_search_requires_a_query_argument() {
    let out = pixel_command().args(["web-search"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("<QUERY>") || stderr.contains("required"),
        "{stderr}"
    );
}

#[test]
fn web_search_rejects_a_non_numeric_limit() {
    let out = pixel_command()
        .args(["web-search", "jev", "--limit", "abc"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}
