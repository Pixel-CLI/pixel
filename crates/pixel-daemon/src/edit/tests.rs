// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use std::fs;

use pixel_graph::store::Store;

use super::*;
use crate::testutil::{open, repo};

const MANIFEST: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n";
const RUN: &str = "pub fn run(x: u32) -> u32 {\n    x\n}\n";
const RUN_TWO: &str = "pub fn run(x: u32, y: u32) -> u32 {\n    x + y\n}\n";
const GO: &str = "use crate::a::run;\n\npub fn go() -> u32 {\n    run(1)\n}\n";

/// A crate whose `b::go` calls `a::run`, and an unrelated `c::other`.
fn fixture() -> (tempfile::TempDir, std::path::PathBuf, Store) {
    let (dir, root) = repo(&[
        ("Cargo.toml", MANIFEST),
        ("src/lib.rs", "pub mod a;\npub mod b;\npub mod c;\n"),
        ("src/a.rs", RUN),
        ("src/b.rs", GO),
        ("src/c.rs", "pub fn other() -> u32 {\n    7\n}\n"),
    ]);
    let (_, store) = open(&root);
    (dir, root, store)
}

fn text(answer: &Answer) -> &str {
    match answer {
        Answer::Inject { text, .. } => text,
        Answer::Silent(reason) => panic!("silent: {reason}"),
    }
}

#[test]
fn a_changed_shape_with_a_caller_should_card_once() {
    let (_dir, root, store) = fixture();
    let mut session = Session::default();
    let answer = before(&mut session, &store, &root, "src/a.rs", Some(RUN_TWO)).unwrap();
    let card = text(&answer);
    assert!(
        card.starts_with("[PIXEL:EDIT] `run` (src/a.rs) changes shape"),
        "{card}"
    );
    assert!(card.contains("pub fn run(x: u32) -> u32"), "{card}");
    assert!(card.contains("pub fn run(x: u32, y: u32) -> u32"), "{card}");
    assert!(card.contains("- src/b.rs:4 in go"), "{card}");
    assert!(card.len() <= text::BUDGET);
    assert!(matches!(answer, Answer::Inject { count: 1, .. }));

    let again = before(&mut session, &store, &root, "src/a.rs", Some(RUN_TWO)).unwrap();
    assert_eq!(again, Answer::Silent("already_sent"));
    let different = before(
        &mut session,
        &store,
        &root,
        "src/a.rs",
        Some("pub fn run(x: u64) -> u32 {\n    1\n}\n"),
    )
    .unwrap();
    assert!(text(&different).contains("x: u64"));
}

#[test]
fn a_body_only_edit_and_an_uncalled_symbol_should_be_silent_with_their_reasons() {
    let (_dir, root, store) = fixture();
    let mut session = Session::default();
    let body = before(
        &mut session,
        &store,
        &root,
        "src/a.rs",
        Some("pub fn run(x: u32) -> u32 {\n    x + 1\n}\n"),
    )
    .unwrap();
    assert_eq!(body, Answer::Silent("no_shape_change"));
    let uncalled = before(
        &mut session,
        &store,
        &root,
        "src/c.rs",
        Some("pub fn other(a: u8) -> u32 {\n    7\n}\n"),
    )
    .unwrap();
    assert_eq!(uncalled, Answer::Silent("no_external_callers"));
}

#[test]
fn without_the_new_text_before_should_only_record_the_baseline() {
    let (_dir, root, store) = fixture();
    let mut session = Session::default();
    let answer = before(&mut session, &store, &root, "src/a.rs", None).unwrap();
    assert_eq!(answer, Answer::Silent("no_proposed_text"));
    let rows = &session.baselines["src/a.rs"];
    assert!(
        rows.iter()
            .any(|row| row.name == "run" && row.shape == "pub fn run(x: u32) -> u32")
    );

    // A second call does not replace the baseline with a later generation.
    session.baselines.get_mut("src/a.rs").unwrap().clear();
    before(&mut session, &store, &root, "src/a.rs", None).unwrap();
    assert!(session.baselines["src/a.rs"].is_empty());
}

#[test]
fn a_removed_or_renamed_symbol_should_say_so_in_its_card() {
    let (_dir, root, store) = fixture();
    let mut removed = Session::default();
    let answer = before(
        &mut removed,
        &store,
        &root,
        "src/a.rs",
        Some("pub fn keep() {}\n"),
    )
    .unwrap();
    assert!(
        text(&answer).contains("`run` (src/a.rs) is removed"),
        "{}",
        text(&answer)
    );
    let mut renamed = Session::default();
    let answer = before(
        &mut renamed,
        &store,
        &root,
        "src/a.rs",
        Some("pub fn execute(x: u32) -> u32 {\n    x\n}\n"),
    )
    .unwrap();
    assert!(
        text(&answer).contains("renamed to `execute`"),
        "{}",
        text(&answer)
    );
}

#[test]
fn a_file_the_graph_does_not_hold_should_be_silent() {
    let (_dir, root, store) = fixture();
    let mut session = Session::default();
    let answer = before(
        &mut session,
        &store,
        &root,
        "src/new.rs",
        Some("pub fn x() {}\n"),
    )
    .unwrap();
    assert_eq!(answer, Answer::Silent("no_shape_change"));
    assert!(session.baselines.is_empty());
}

#[test]
fn after_an_edit_the_callers_not_yet_changed_should_be_pending_once() {
    let (_dir, root, store) = fixture();
    let mut session = Session::default();
    assert_eq!(
        before(&mut session, &store, &root, "src/a.rs", None).unwrap(),
        Answer::Silent("no_proposed_text")
    );
    fs::write(root.join("src/a.rs"), RUN_TWO).unwrap();

    let answer = after(&mut session, &store, &root, "src/a.rs").unwrap();
    let note = text(&answer);
    assert!(note.starts_with("[PIXEL:STALE-CALLERS]"), "{note}");
    assert!(note.contains("- src/b.rs:4 in go"), "{note}");
    assert!(
        note.contains("`pub fn run(x: u32, y: u32) -> u32`"),
        "{note}"
    );
    assert_eq!(session.pending.len(), 1);

    let repeat = after(&mut session, &store, &root, "src/a.rs").unwrap();
    assert_eq!(repeat, Answer::Silent("nothing_new"));
}

#[test]
fn after_with_no_baseline_or_no_change_should_be_silent() {
    let (_dir, root, store) = fixture();
    let mut session = Session::default();
    assert_eq!(
        after(&mut session, &store, &root, "src/a.rs").unwrap(),
        Answer::Silent("no_baseline")
    );
    before(&mut session, &store, &root, "src/a.rs", None).unwrap();
    assert_eq!(
        after(&mut session, &store, &root, "src/a.rs").unwrap(),
        Answer::Silent("no_shape_change")
    );
    fs::write(
        root.join("src/a.rs"),
        "pub fn run(x: u32) -> u32 {\n    x + 9\n}\n",
    )
    .unwrap();
    assert_eq!(
        after(&mut session, &store, &root, "src/a.rs").unwrap(),
        Answer::Silent("no_shape_change")
    );
}

#[test]
fn a_caller_the_agent_edits_should_leave_the_set_and_stay_out() {
    let (_dir, root, store) = fixture();
    let mut session = Session::default();
    before(&mut session, &store, &root, "src/a.rs", None).unwrap();
    fs::write(root.join("src/a.rs"), RUN_TWO).unwrap();
    after(&mut session, &store, &root, "src/a.rs").unwrap();
    assert_eq!(session.pending.len(), 1);

    fs::write(root.join("src/b.rs"), GO.replace("run(1)", "run(1, 2)")).unwrap();
    refresh(&mut session, &root);
    assert!(session.pending.is_empty());
    assert_eq!(stop(&mut session, &root), Answer::Silent("empty"));

    // Editing the symbol again does not put the fixed caller back.
    fs::write(
        root.join("src/a.rs"),
        "pub fn run(x: u32, y: u64) -> u32 {\n    x\n}\n",
    )
    .unwrap();
    let answer = after(&mut session, &store, &root, "src/a.rs").unwrap();
    assert_eq!(answer, Answer::Silent("nothing_new"));
    assert!(session.pending.is_empty());
}

#[test]
fn a_shape_that_comes_back_should_leave_the_set() {
    let (_dir, root, store) = fixture();
    let mut session = Session::default();
    before(&mut session, &store, &root, "src/a.rs", None).unwrap();
    fs::write(root.join("src/a.rs"), RUN_TWO).unwrap();
    after(&mut session, &store, &root, "src/a.rs").unwrap();
    fs::write(root.join("src/a.rs"), RUN).unwrap();
    assert_eq!(
        after(&mut session, &store, &root, "src/a.rs").unwrap(),
        Answer::Silent("no_shape_change")
    );
    assert!(session.pending.is_empty());
}

#[test]
fn a_removed_symbol_should_keep_its_callers_from_the_card_when_the_graph_forgot_it() {
    let (_dir, root, store) = fixture();
    let mut session = Session::default();
    before(&mut session, &store, &root, "src/a.rs", None).unwrap();
    // The card names the callers while the graph still holds the symbol.
    before(
        &mut session,
        &store,
        &root,
        "src/a.rs",
        Some("pub fn keep() {}\n"),
    )
    .unwrap();
    assert!(!session.candidates.is_empty());
    fs::write(root.join("src/a.rs"), "pub fn keep() {}\n").unwrap();
    // A graph that already republished the edit no longer has `run`.
    let (_, fresh) = {
        pixel_graph::refresh(&root, None, None).unwrap();
        ((), Store::open(&root).unwrap())
    };
    let answer = after(&mut session, &fresh, &root, "src/a.rs").unwrap();
    assert!(
        text(&answer).contains("removed or renamed"),
        "{}",
        text(&answer)
    );
    assert!(text(&answer).contains("src/b.rs:4"), "{}", text(&answer));
}

#[test]
fn stop_should_block_once_per_distinct_set() {
    let (_dir, root, store) = fixture();
    let mut session = Session::default();
    assert_eq!(stop(&mut session, &root), Answer::Silent("empty"));
    before(&mut session, &store, &root, "src/a.rs", None).unwrap();
    fs::write(root.join("src/a.rs"), RUN_TWO).unwrap();
    after(&mut session, &store, &root, "src/a.rs").unwrap();

    let first = stop(&mut session, &root);
    let reason = text(&first);
    assert!(reason.contains("still use the old shape"), "{reason}");
    assert!(reason.contains("state why they are fine"), "{reason}");
    assert!(matches!(first, Answer::Inject { count: 1, .. }));
    assert_eq!(stop(&mut session, &root), Answer::Silent("already_blocked"));

    // A different set blocks again.
    session.pending[0].sites.push(PendingSite {
        file: "src/c.rs".to_owned(),
        line: 1,
        holder: "other".to_owned(),
        hash: hash_of(&root, "src/c.rs").unwrap(),
    });
    assert!(matches!(
        stop(&mut session, &root),
        Answer::Inject { count: 2, .. }
    ));
}

#[test]
fn the_texts_should_stay_within_their_budget_and_say_what_was_cut() {
    let sites: Vec<PendingSite> = (0..16)
        .map(|n| PendingSite {
            file: format!("src/a_rather_long_module_name_{n}/and_a_nested_file_{n}.rs"),
            line: n + 1,
            holder: format!("holder_function_with_a_long_name_{n}"),
            hash: String::new(),
        })
        .collect();
    let pending: Vec<Pending> = (0..3)
        .map(|n| Pending {
            uid: format!("src/a.rs#run{n}#function"),
            name: format!("run{n}"),
            file: "src/a.rs".to_owned(),
            old: "pub fn run(x: u32) -> u32".to_owned(),
            new: Some("pub fn run(x: u32, y: u32) -> u32".to_owned()),
            sites: sites.clone(),
        })
        .collect();
    for body in [text::stale(&pending), text::block(&pending)] {
        assert!(body.len() <= text::BUDGET, "{}", body.len());
        assert!(body.contains("more lines cut"), "{body}");
        assert!(body.starts_with("[PIXEL:STALE-CALLERS]"));
    }
}

#[test]
fn an_answer_should_serialize_for_the_wire() {
    let inject = Answer::Inject {
        text: "hello".to_owned(),
        count: 2,
    };
    assert_eq!(
        inject.to_value("before"),
        json!({"phase": "before", "text": "hello", "count": 2, "bytes": 5})
    );
    assert_eq!(
        Answer::Silent("already_sent").to_value("after"),
        json!({"phase": "after", "text": null, "silent": "already_sent", "count": 0})
    );
}
