// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! Text for the answers: one compact form per op, built from the JSON result
//! the daemon returns (`--json` prints that result instead). A renderer reads
//! only fields the op documents and tolerates a missing one, so a newer
//! daemon's extra fields and an older one's gaps never break a command.

use std::fmt::Write as _;

use serde_json::Value;

/// The most bytes of text one command prints; past it the text is cut at a
/// line and says so.
pub const STDOUT_BYTE_CAP: usize = 256 * 1024;

fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn n(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn items<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
}

/// `text` cut at the last whole line within `cap` bytes, and whether it was.
pub fn cap_text(text: &str, cap: usize) -> (String, bool) {
    if text.len() <= cap {
        return (text.to_owned(), false);
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = text[..end].rfind('\n').map_or(0, |at| at + 1);
    (text[..cut].to_owned(), true)
}

/// `file:line text`, one match per line, as ripgrep prints them.
pub fn search(data: &Value, files_only: bool) -> String {
    let mut out = String::new();
    if files_only {
        let mut seen: Vec<&str> = Vec::new();
        for row in items(data, "matches") {
            let path = s(row, "path");
            if !seen.contains(&path) {
                seen.push(path);
                let _ = writeln!(out, "{path}");
            }
        }
        return out;
    }
    for row in items(data, "matches") {
        let _ = writeln!(
            out,
            "{}:{}:{}",
            s(row, "path"),
            n(row, "line"),
            s(row, "text")
        );
    }
    out
}

/// The note under a search page that was cut.
pub fn search_note(data: &Value) -> Option<String> {
    let next = data.get("next_offset").and_then(Value::as_u64)?;
    Some(format!(
        "⚠ results truncated: {} shown, next page --offset {next}",
        items(data, "matches").len()
    ))
}

fn symbol_line(symbol: &Value) -> String {
    format!(
        "{}:{} {} {}",
        s(symbol, "file"),
        n(symbol, "line"),
        s(symbol, "kind"),
        s(symbol, "name")
    )
}

pub fn symbols(data: &Value) -> String {
    let mut out = String::new();
    for symbol in items(data, "symbols") {
        let _ = writeln!(out, "{}", symbol_line(symbol));
        let signature = s(symbol, "signature").trim();
        if !signature.is_empty() {
            let _ = writeln!(out, "    {signature}");
        }
    }
    if data.get("truncated").and_then(Value::as_bool) == Some(true) {
        let _ = writeln!(
            out,
            "… {} symbols in all; narrow the name",
            n(data, "total")
        );
    }
    out
}

fn tier_tag(row: &Value) -> &'static str {
    match s(row, "tier") {
        "likely" => " [likely]",
        "open" => " [open]",
        _ => "",
    }
}

pub fn callers(data: &Value) -> String {
    let target = data.get("target").unwrap_or(&Value::Null);
    let role = s(data, "role");
    let mut out = format!(
        "{} ({}:{}) {role}: {}\n",
        s(target, "name"),
        s(target, "file"),
        n(target, "line"),
        n(data, "total")
    );
    for row in items(data, "items") {
        if role == "callees" {
            let _ = writeln!(
                out,
                "  {}:{} {} (called at line {}){}",
                s(row, "file"),
                n(row, "line"),
                s(row, "name"),
                n(row, "site_line"),
                tier_tag(row)
            );
        } else {
            // A caller is placed where it calls: its file, the line of the call.
            let _ = writeln!(
                out,
                "  {}:{} {}{}",
                s(row, "file"),
                n(row, "site_line"),
                s(row, "name"),
                tier_tag(row)
            );
        }
    }
    if let Some(next) = data.get("next_offset").and_then(Value::as_u64) {
        let _ = writeln!(out, "  … next page --offset {next}");
    }
    let open = n(data, "open_sites");
    if open > 0 {
        let _ = writeln!(
            out,
            "  lower bound: {open} unresolved site(s) call a function of this name"
        );
    }
    out
}

pub fn impact(data: &Value) -> String {
    let target = data.get("target").unwrap_or(&Value::Null);
    let risk = data.get("risk").and_then(Value::as_str);
    let mut out = format!(
        "{} ({}:{}) {}{}\n",
        s(target, "name"),
        s(target, "file"),
        n(target, "line"),
        s(data, "direction"),
        risk.map_or(String::new(), |risk| format!(", risk {risk}"))
    );
    let counts = items(data, "counts");
    let levels = items(data, "depths");
    let labels = ["will break", "likely affected", "may need tests"];
    for (depth, level) in levels.iter().enumerate() {
        let rows = level.as_array().map_or(&[][..], Vec::as_slice);
        let count = counts.get(depth).and_then(Value::as_u64).unwrap_or(0);
        if count == 0 {
            continue;
        }
        let _ = writeln!(
            out,
            "depth {} ({}): {count}",
            depth + 1,
            labels[depth.min(2)]
        );
        for row in rows {
            let _ = writeln!(
                out,
                "  {}:{} {}{}",
                s(row, "file"),
                n(row, "line"),
                s(row, "name"),
                tier_tag(row)
            );
        }
    }
    let referenced = items(data, "referenced_by");
    if !referenced.is_empty() {
        let _ = writeln!(
            out,
            "named without a call ({}):",
            n(data, "referenced_by_total")
        );
        for row in referenced {
            let _ = writeln!(
                out,
                "  {}:{} {}",
                s(row, "file"),
                n(row, "line"),
                s(row, "name")
            );
        }
    }
    let tests = items(data, "tests");
    if !tests.is_empty() {
        let _ = writeln!(out, "tests:");
        for test in tests {
            let _ = writeln!(out, "  {}", test.as_str().unwrap_or(""));
        }
    }
    let open = n(data, "open_sites");
    if open > 0 {
        let _ = writeln!(
            out,
            "lower bound: {open} unresolved site(s) call a function of this name"
        );
    }
    for cap in items(data, "caps") {
        let _ = writeln!(out, "⚠ {}", cap.as_str().unwrap_or(""));
    }
    out
}

pub fn changes(data: &Value) -> String {
    let mut out = String::new();
    for row in items(data, "symbols") {
        let marker = if row.get("signature_changed").and_then(Value::as_bool) == Some(true) {
            " [signature]"
        } else {
            ""
        };
        let _ = writeln!(out, "{}{marker}", symbol_line(row));
        for test in items(row, "tests") {
            let _ = writeln!(out, "    test {}", test.as_str().unwrap_or(""));
        }
    }
    if out.is_empty() {
        out.push_str("no symbols changed\n");
    }
    for path in items(data, "unindexed") {
        let _ = writeln!(out, "not indexed: {}", path.as_str().unwrap_or(""));
    }
    if let Some(next) = data.get("next_offset").and_then(Value::as_u64) {
        let _ = writeln!(out, "… next page --offset {next}");
    }
    out
}

pub fn repo_map(data: &Value) -> String {
    if let Some(markdown) = data.get("markdown").and_then(Value::as_str) {
        return markdown.to_owned();
    }
    let mut out = String::new();
    for file in items(data, "files") {
        let _ = writeln!(out, "{}", s(file, "path"));
        for symbol in items(file, "symbols") {
            let _ = writeln!(out, "  {} {}", s(symbol, "kind"), s(symbol, "name"));
        }
    }
    if data.get("truncated").and_then(Value::as_bool) == Some(true) {
        let _ = writeln!(out, "… {} files in all; the map is cut", n(data, "total"));
    }
    out
}

pub fn signatures(file: &str, symbols: &[Value]) -> String {
    let mut out = format!("{file}\n");
    for symbol in symbols {
        let _ = writeln!(
            out,
            "{:>5} {:<10} {}",
            n(symbol, "line"),
            s(symbol, "kind"),
            s(symbol, "signature").trim()
        );
    }
    out
}

pub fn history(data: &Value) -> String {
    if data.get("available").and_then(Value::as_bool) == Some(false) {
        return format!("history unavailable: {}\n", s(data, "reason"));
    }
    let mut out = String::new();
    for hit in items(data, "candidates") {
        let short: String = s(hit, "oid").chars().take(10).collect();
        let _ = writeln!(
            out,
            "{short} {} {} [{}]",
            s(hit, "at").get(..10).unwrap_or(s(hit, "at")),
            s(hit, "subject"),
            s(hit, "kind")
        );
        if let Some(path) = hit.get("path").and_then(Value::as_str) {
            let _ = writeln!(out, "    {path}");
        }
        if let Some(snippet) = hit.get("snippet").and_then(Value::as_str) {
            for line in snippet.lines().take(6) {
                let _ = writeln!(out, "    {line}");
            }
        }
    }
    if out.is_empty() {
        out.push_str("no commits match\n");
    }
    out
}

/// Why a prompt got no brief, from the op's result: the decline, else the
/// gate's reason.
pub fn brief_reason(data: &Value) -> String {
    if let Some(declined) = data.get("declined").and_then(Value::as_str) {
        return declined.replace('_', " ");
    }
    let record = data.get("record").unwrap_or(&Value::Null);
    match record.get("reason").and_then(Value::as_str) {
        Some(reason) => reason.to_owned(),
        None => format!(
            "gate {}, {} of {} operations answered",
            s(record, "gate"),
            record.get("answered").and_then(Value::as_u64).unwrap_or(0),
            record.get("ops").and_then(Value::as_u64).unwrap_or(0)
        ),
    }
}

pub fn status(data: &Value) -> String {
    let index = data.get("index").unwrap_or(&Value::Null);
    let graph = data.get("graph").unwrap_or(&Value::Null);
    let history = data.get("history").unwrap_or(&Value::Null);
    let mut out = format!("pixel {} at {}\n", s(data, "version"), s(data, "root"));
    let _ = writeln!(
        out,
        "index:   {} base + {} delta + {} overlay files, covers {}",
        n(index, "base_files"),
        n(index, "delta_files"),
        n(index, "overlay_files"),
        index
            .get("covered_head")
            .and_then(Value::as_str)
            .map_or("no commit", |head| head.get(..10).unwrap_or(head))
    );
    if graph.get("present").and_then(Value::as_bool) == Some(true) {
        let _ = writeln!(
            out,
            "graph:   {} files, extractor {}{}",
            n(graph, "files"),
            s(graph, "extractor"),
            if graph.get("stale").and_then(Value::as_bool) == Some(true) {
                ", behind the working tree"
            } else {
                ""
            }
        );
    } else {
        let _ = writeln!(out, "graph:   not built (run `pixel prepare`)");
    }
    let _ = writeln!(
        out,
        "history: {}",
        if history.get("present").and_then(Value::as_bool) == Some(true) {
            "present"
        } else {
            "not built (the first history query builds it)"
        }
    );
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_search_prints_ripgrep_lines_and_a_page_note() {
        let data = json!({
            "matches": [
                {"path": "a.rs", "line": 3, "text": "fn run() {}"},
                {"path": "a.rs", "line": 9, "text": "run();"},
                {"path": "b.rs", "line": 1, "text": "run"},
            ],
            "next_offset": 3,
        });
        assert_eq!(
            search(&data, false),
            "a.rs:3:fn run() {}\na.rs:9:run();\nb.rs:1:run\n"
        );
        assert_eq!(search(&data, true), "a.rs\nb.rs\n");
        assert_eq!(
            search_note(&data).as_deref(),
            Some("⚠ results truncated: 3 shown, next page --offset 3")
        );
        assert_eq!(search_note(&json!({"matches": []})), None);
    }

    #[test]
    fn symbols_show_their_place_and_their_signature() {
        let data = json!({"symbols": [
            {"file": "a.rs", "line": 2, "kind": "function", "name": "run", "signature": "pub fn run() {"},
        ], "total": 1});
        assert_eq!(symbols(&data), "a.rs:2 function run\n    pub fn run() {\n");
        let cut = json!({"symbols": [], "total": 90, "truncated": true});
        assert_eq!(symbols(&cut), "… 90 symbols in all; narrow the name\n");
    }

    #[test]
    fn callers_name_the_tier_of_a_guess_and_the_lower_bound() {
        let data = json!({
            "target": {"name": "run", "file": "a.rs", "line": 1},
            "role": "callers",
            "total": 2,
            "items": [
                {"file": "b.rs", "line": 3, "site_line": 4, "name": "go", "tier": "proved"},
                {"file": "c.rs", "line": 6, "site_line": 7, "name": "try_it", "tier": "likely"},
            ],
            "next_offset": 50,
            "open_sites": 3,
        });
        assert_eq!(
            callers(&data),
            "run (a.rs:1) callers: 2\n  b.rs:4 go\n  c.rs:7 try_it [likely]\n  … next page --offset 50\n  lower bound: 3 unresolved site(s) call a function of this name\n"
        );
        let callees = json!({
            "target": {"name": "go", "file": "b.rs", "line": 3},
            "role": "callees",
            "total": 1,
            "items": [{"file": "a.rs", "line": 1, "site_line": 4, "name": "run", "tier": "proved"}],
        });
        assert_eq!(
            super::callers(&callees),
            "go (b.rs:3) callees: 1\n  a.rs:1 run (called at line 4)\n"
        );
    }

    #[test]
    fn an_impact_lists_the_depths_that_have_items_the_tests_and_the_caps() {
        let data = json!({
            "target": {"name": "run", "file": "a.rs", "line": 1},
            "direction": "upstream",
            "risk": "LOW",
            "counts": [1, 0, 1],
            "depths": [
                [{"file": "b.rs", "line": 3, "name": "go", "tier": "proved"}],
                [],
                [{"file": "t.rs", "line": 2, "name": "t", "tier": "likely"}],
            ],
            "referenced_by": [{"file": "r.rs", "line": 5, "name": "register"}],
            "referenced_by_total": 1,
            "tests": ["tests/it.rs"],
            "open_sites": 0,
            "caps": ["depth 1 listed 1 of 2 items"],
        });
        let text = impact(&data);
        assert!(
            text.starts_with("run (a.rs:1) upstream, risk LOW\n"),
            "{text}"
        );
        assert!(
            text.contains("depth 1 (will break): 1\n  b.rs:3 go\n"),
            "{text}"
        );
        assert!(!text.contains("depth 2"), "{text}");
        assert!(
            text.contains("depth 3 (may need tests): 1\n  t.rs:2 t [likely]\n"),
            "{text}"
        );
        assert!(
            text.contains("named without a call (1):\n  r.rs:5 register\n"),
            "{text}"
        );
        assert!(text.contains("tests:\n  tests/it.rs\n"), "{text}");
        assert!(text.ends_with("⚠ depth 1 listed 1 of 2 items\n"), "{text}");
    }

    #[test]
    fn changes_flag_a_signature_and_name_the_tests_or_say_nothing_changed() {
        let data = json!({"symbols": [
            {"file": "a.rs", "line": 1, "kind": "function", "name": "run",
             "signature_changed": true, "tests": ["tests/it.rs"]},
        ], "unindexed": ["new.rs"]});
        assert_eq!(
            changes(&data),
            "a.rs:1 function run [signature]\n    test tests/it.rs\nnot indexed: new.rs\n"
        );
        assert_eq!(changes(&json!({"symbols": []})), "no symbols changed\n");
    }

    #[test]
    fn a_map_lists_files_and_symbols_or_the_markdown_the_daemon_made() {
        let data = json!({"files": [
            {"path": "a.rs", "symbols": [{"kind": "function", "name": "run"}]},
        ], "total": 1, "truncated": false});
        assert_eq!(repo_map(&data), "a.rs\n  function run\n");
        assert_eq!(repo_map(&json!({"markdown": "## a.rs\n"})), "## a.rs\n");
    }

    #[test]
    fn history_says_when_it_is_unavailable_and_prints_hits_otherwise() {
        let unavailable = json!({"available": false, "reason": "no history db"});
        assert_eq!(
            history(&unavailable),
            "history unavailable: no history db\n"
        );
        let data = json!({"available": true, "candidates": [
            {"oid": "0123456789abcdef", "at": "2026-10-01T10:00:00Z", "subject": "Add run",
             "kind": "message", "path": "a.rs", "snippet": "+fn run() {}\n+}"},
        ]});
        assert_eq!(
            history(&data),
            "0123456789 2026-10-01 Add run [message]\n    a.rs\n    +fn run() {}\n    +}\n"
        );
        assert_eq!(
            history(&json!({"available": true, "candidates": []})),
            "no commits match\n"
        );
    }

    #[test]
    fn status_reads_the_three_stores() {
        let data = json!({
            "version": "0.1.0", "root": "/r",
            "index": {"base_files": 5, "delta_files": 1, "overlay_files": 2, "covered_head": "0123456789abcdef"},
            "graph": {"present": true, "files": 4, "extractor": "rust.1", "stale": true},
            "history": {"present": false},
        });
        assert_eq!(
            status(&data),
            "pixel 0.1.0 at /r\nindex:   5 base + 1 delta + 2 overlay files, covers 0123456789\ngraph:   4 files, extractor rust.1, behind the working tree\nhistory: not built (the first history query builds it)\n"
        );
        assert!(
            status(&json!({"graph": {"present": false}}))
                .contains("not built (run `pixel prepare`)")
        );
    }

    #[test]
    fn text_is_cut_at_a_line_and_says_so_only_when_it_is_cut() {
        assert_eq!(cap_text("a\nb\n", 100), ("a\nb\n".to_owned(), false));
        assert_eq!(cap_text("aa\nbb\ncc\n", 7), ("aa\nbb\n".to_owned(), true));
        assert_eq!(
            cap_text("é\né\n", 2),
            (String::new(), true),
            "never inside a character"
        );
        assert_eq!(cap_text("abc", 0), (String::new(), true));
    }

    #[test]
    fn signatures_are_aligned_by_line() {
        let rows = [json!({"line": 3, "kind": "function", "signature": "pub fn run() {"})];
        assert_eq!(
            signatures("a.rs", &rows),
            "a.rs\n    3 function   pub fn run() {\n"
        );
    }
}
