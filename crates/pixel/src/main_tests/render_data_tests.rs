// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use super::*;

#[test]
fn the_install_banner_color_follows_no_color() {
    assert!(banner_color(None));
    assert!(banner_color(Some(std::ffi::OsStr::new(""))));
    assert!(!banner_color(Some(std::ffi::OsStr::new("1"))));
}

#[test]
fn the_install_start_banner_stays_on_human_terminal_output() {
    assert!(should_render_install_banner(false, true));
    assert!(!should_render_install_banner(true, true));
    assert!(!should_render_install_banner(false, false));
}

fn big() -> Value {
    json!({"matches": (0..200).map(|i| json!({"path": format!("src/file_{i}.rs"), "line": i, "text": "é".repeat(20)})).collect::<Vec<_>>()})
}

#[test]
fn under_cap_is_untouched() {
    let d = json!({"a": 1});
    let r = render_data(&d, true, 1024);
    assert_eq!(r.text, "{\"a\":1}\n");
    assert!(!r.truncated);
    assert_eq!(
        serde_json::from_str::<Value>(&render_data(&d, false, 1024).text).unwrap(),
        d
    );
}

/// The reason this matters: agents call `pixel … --json` and parse
/// stdout, then `jq '.index'` / `.graph` on the result. A response that
/// is over the cap only because ONE list is long (a `snapshot.dirty`
/// full of untracked `vendor/bundle` paths) must keep its shape: the
/// scalar fields stay addressable, only the list is shortened, and the
/// document says which list was cut and how much survived.
#[test]
fn json_mode_truncation_shortens_the_largest_array_and_keeps_structure() {
    let d = json!({
        "index": {"base_files": 238, "commit_oid": "abc"},
        "snapshot": {"head": "abc", "branch": "main",
                      "dirty": (0..500).map(|i| format!("vendor/bundle/gems/g{i}/lib/x.rb")).collect::<Vec<_>>()},
    });
    let full = serde_json::to_string(&d).unwrap().len();
    let cap = full / 3;
    let r = render_data(&d, true, cap);
    assert!(r.truncated);
    assert!(r.text.len() <= cap + 1, "{} > cap {cap}", r.text.len());
    let v: Value = serde_json::from_str(&r.text).expect("stdout must remain one JSON document");
    assert_eq!(v["truncated"], true);
    assert_eq!(v["cap_bytes"], cap);
    assert_eq!(
        v["index"]["base_files"], 238,
        "untouched fields must survive"
    );
    assert_eq!(v["snapshot"]["branch"], "main");
    let dirty = v["snapshot"]["dirty"].as_array().unwrap();
    assert!(
        !dirty.is_empty() && dirty.len() < 500,
        "kept {}",
        dirty.len()
    );
    assert_eq!(
        dirty[0], "vendor/bundle/gems/g0/lib/x.rb",
        "prefix, not a sample"
    );
    let cuts = v["truncated_arrays"].as_array().unwrap();
    assert_eq!(cuts.len(), 1);
    assert_eq!(cuts[0]["path"], "snapshot.dirty");
    assert_eq!(cuts[0]["total"], 500);
    assert_eq!(cuts[0]["kept"], dirty.len());
    assert!(
        v.get("partial").is_none(),
        "no textual wrapper when structure fits"
    );
    assert_eq!(r.text.matches('\n').count(), 1, "single NDJSON-safe line");
}

/// Nested arrays: the cut lands on the array that actually carries the
/// bytes, not blindly on the top-level one, and the path names it.
#[test]
fn structural_truncation_targets_nested_array_by_size() {
    let d = json!({"groups": [
        {"name": "small", "items": ["a", "b"]},
        {"name": "huge", "items": (0..2000).map(|i| format!("item-{i:05}")).collect::<Vec<_>>()},
    ]});
    let r = render_data(&d, true, 2000);
    let v: Value = serde_json::from_str(&r.text).unwrap();
    assert_eq!(
        v["groups"].as_array().unwrap().len(),
        2,
        "outer array intact"
    );
    assert_eq!(v["groups"][0]["items"].as_array().unwrap().len(), 2);
    assert!(v["groups"][1]["items"].as_array().unwrap().len() < 2000);
    assert_eq!(v["truncated_arrays"][0]["path"], "groups[1].items");
    assert!(r.text.len() <= 2001);
}

/// When the bulk is not an array (one huge string) structural trimming
/// cannot help; the textual wrapper must still be one valid document
/// that says it was cut.
#[test]
fn json_mode_falls_back_to_wrapper_when_no_array_can_be_cut() {
    let d = json!({"blob": "x".repeat(5000)});
    let r = render_data(&d, true, 500);
    assert!(r.truncated);
    let v: Value = serde_json::from_str(&r.text).expect("stdout must remain one JSON document");
    assert_eq!(v["truncated"], true);
    assert_eq!(v["cap_bytes"], 500);
    let partial = v["partial"].as_str().unwrap();
    assert!(partial.len() <= 500);
    assert!(partial.starts_with("{\"blob\":\""));
    assert!(v["note"].as_str().unwrap().contains("TRUNCATED"));
    assert_eq!(r.text.matches('\n').count(), 1, "single NDJSON-safe line");
}

#[test]
fn json_mode_array_of_objects_is_cut_structurally() {
    let r = render_data(&big(), true, 500);
    let v: Value = serde_json::from_str(&r.text).unwrap();
    assert_eq!(v["truncated"], true);
    assert!(v["matches"].is_array());
    assert_eq!(v["truncated_arrays"][0]["path"], "matches");
    assert_eq!(v["truncated_arrays"][0]["total"], 200);
}

#[test]
fn human_mode_truncation_keeps_visible_note() {
    let r = render_data(&big(), false, 500);
    assert!(r.truncated);
    assert!(r.text.contains("⚠ OUTPUT TRUNCATED AT 500 BYTES"));
    assert!(serde_json::from_str::<Value>(&r.text).is_err());
}

/// Multi-byte text near the cap: the cut must land on a char boundary
/// so the partial string is valid UTF-8 and serializable.
#[test]
fn truncation_respects_char_boundaries() {
    let d = json!({"t": "é".repeat(1000)});
    for cap in 100..140 {
        let out = render_data(&d, true, cap).text;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["partial"].as_str().unwrap().len() <= cap);
    }
}

/// `array_at_mut` must round-trip every path shape `largest_array`
/// emits, otherwise a cut silently targets nothing.
#[test]
fn array_path_round_trip() {
    let mut d = json!({"a": {"b": [[1, 2, 3], {"c": [4, 5]}]}, "d": [6]});
    let (path, len, _) = largest_array(&d, "").unwrap();
    assert_eq!(
        path, "a.b",
        "21 bytes / 2 elems: 11 removable, beats a.b[0]'s 5"
    );
    assert_eq!(len, 2);
    assert_eq!(array_at_mut(&mut d, "a.b[0]").unwrap().len(), 3);
    assert_eq!(array_at_mut(&mut d, "a.b[1].c").unwrap().len(), 2);
    assert_eq!(array_at_mut(&mut d, "d").unwrap().len(), 1);
    assert!(array_at_mut(&mut d, "a.b[5]").is_none());
}

/// `PIXEL_OUTPUT_CAP_BYTES=0` is the documented escape hatch for a
/// consumer that wants the whole document; anything unparsable must
/// keep the safety net rather than silently disabling it.
#[test]
fn output_cap_env_parsing() {
    assert_eq!(parse_output_cap(None), STDOUT_BYTE_CAP);
    assert_eq!(parse_output_cap(Some("0")), usize::MAX);
    assert_eq!(parse_output_cap(Some(" 4096 ")), 4096);
    assert_eq!(parse_output_cap(Some("lots")), STDOUT_BYTE_CAP);
    assert_eq!(parse_output_cap(Some("")), STDOUT_BYTE_CAP);
}

/// `status`/`ready` are freshness answers: the dirty LIST is what let an
/// untracked vendor tree blow the cap, the COUNT is all they need.
/// The stderr line is how an agent tells a 1 s incremental update from
/// a 100 s rebuild of the whole tree; the two must not read the same.
#[test]
fn graph_build_notice_distinguishes_incremental_from_full() {
    let incremental =
        json!({"incremental": true, "changed_files": 2, "removed_files": 0, "build_ms": 1200});
    assert_eq!(
        graph_build_notice(&incremental),
        "updated graph.db for 2 changed file(s) (1200 ms)"
    );
    let with_removed =
        json!({"incremental": true, "changed_files": 1, "removed_files": 1, "build_ms": 40});
    assert_eq!(
        graph_build_notice(&with_removed),
        "updated graph.db for 1 changed file(s), 1 removed (40 ms)"
    );
    let first = json!({"incremental": false, "reason": "missing", "build_ms": 36000});
    assert_eq!(
        graph_build_notice(&first),
        "built graph.db on first use (36000 ms)"
    );
    let threshold = json!({"incremental": false, "reason": "threshold", "build_ms": 5});
    assert!(graph_build_notice(&threshold).contains("PIXEL_GRAPH_INCREMENTAL_MAX_PCT"));
    // Older daemon without the field: still the first-use wording.
    assert_eq!(
        graph_build_notice(&json!({"build_ms": 7})),
        "built graph.db on first use (7 ms)"
    );
}

#[test]
fn compact_snapshot_replaces_dirty_list_with_count() {
    let mut d = json!({"index": {"base_files": 1},
        "snapshot": {"head": "abc", "branch": "main", "dirty": ["a", "b", "c"]}});
    compact_snapshot(&mut d);
    assert_eq!(d["snapshot"]["dirty_count"], 3);
    assert!(d["snapshot"].get("dirty").is_none());
    assert_eq!(d["snapshot"]["head"], "abc");
    assert_eq!(d["index"]["base_files"], 1);
    // No snapshot (older daemon / in-process service without one): no-op.
    let mut bare = json!({"index": {}});
    compact_snapshot(&mut bare);
    assert_eq!(bare, json!({"index": {}}));
}

#[test]
fn compact_repo_state_drops_the_clean_list_and_keeps_the_count() {
    let mut d = json!({"root": "/repo", "head": "abc", "branch": "main",
        "dirty": [], "dirty_count": 0,
        "clean": ["a", "b"], "clean_count": 2,
        "clean_list_truncated": false, "clean_list_cap": 200});
    compact_repo_state(&mut d);
    assert!(d.get("clean").is_none(), "{d}");
    assert!(d.get("clean_list_truncated").is_none(), "{d}");
    assert!(d.get("clean_list_cap").is_none(), "{d}");
    assert_eq!(d["clean_count"], 2);
    assert_eq!(d["head"], "abc");
    // A non-object (never produced today): no-op, no panic.
    let mut bare = json!([]);
    compact_repo_state(&mut bare);
    assert_eq!(bare, json!([]));
}

#[test]
fn a_find_code_match_line_shows_a_route_handler_only_when_recorded() {
    let route = json!({
        "path": "config/routes.rb", "start_line": 3, "kind": "route", "score": 1.0,
        "raw": "POST /admin/orders",
        "detail": "admin/orders#create (Admin::OrdersController#create)",
    });
    assert_eq!(
        resolve_match_line(&route),
        "config/routes.rb:3 (route, score: 1.00) POST /admin/orders → admin/orders#create (Admin::OrdersController#create)\n"
    );
    let plain = json!({
        "path": "src/a.ts", "start_line": 7, "kind": "string", "score": 0.5,
        "raw": "hello world again", "detail": "",
    });
    assert_eq!(
        resolve_match_line(&plain),
        "src/a.ts:7 (string, score: 0.50) hello world again\n"
    );
    // Another kind's detail is bookkeeping, never printed as a handler.
    let component = json!({
        "path": "src/b.tsx", "start_line": 2, "kind": "component", "score": 0.9,
        "raw": "Button", "detail": "component",
    });
    assert_eq!(
        resolve_match_line(&component),
        "src/b.tsx:2 (component, score: 0.90) Button\n"
    );
}
