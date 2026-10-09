// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Integration: the `facts.relevance` block of the `targets_facts` op over a
//! real fixture repository, and its agreement with the in-process reader
//! (`relevance_on`) that a hook without a daemon uses.

use std::path::{Path, PathBuf};
use std::process::Command;

use pixel_daemon::relevance::relevance_on;
use pixel_daemon::{Request, Response, Service};
use pixel_graph::GraphStore;
use pixel_index::TrigramExtractor;
use pixel_index::indexset::IndexSet;
use pixel_proto::Relevance;
use serde_json::Value;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn fixture(tag: &str, files: &[(String, String)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("gpx-relevance-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    for (path, body) in files {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, body).unwrap();
    }
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "fixture"]);
    root.canonicalize().unwrap()
}

fn install_repo(tag: &str) -> PathBuf {
    let files = [
        (
            "crates/install/src/claude_settings.rs",
            "// install must handle existing settings\n\
             pub fn merge_claude_settings(existing: &str) -> String {\n    existing.to_string()\n}\n",
        ),
        (
            "crates/install/src/hooks.rs",
            "pub fn install_hooks() {}\n// settings for hooks\n",
        ),
        (
            "docs/manual-setup.md",
            "To install by hand, edit the settings file.\n",
        ),
        ("src/auth.rs", "// login flow\npub fn check() {}\n"),
        (".env", "install=1\nTOKEN=hunter2 install\n"),
    ];
    let owned: Vec<(String, String)> = files
        .iter()
        .map(|(path, body)| ((*path).to_owned(), (*body).to_owned()))
        .collect();
    fixture(tag, &owned)
}

/// A service whose graph is built and fresh, which `targets_facts` requires.
fn ready(root: &Path) -> Service {
    let mut service = Service::open(root).unwrap();
    let built = service.handle(Request::Targets {
        task: "build the graph".to_owned(),
        limit: Some(1),
        max_tier: None,
        precision: false,
        regions: false,
    });
    assert!(built.ok, "fixture graph build: {built:?}");
    service
}

fn facts(service: &mut Service, task: &str) -> Response {
    let response = service.handle(Request::TargetsFacts {
        task: task.to_owned(),
        limit: Some(10),
    });
    assert!(response.ok, "targets_facts: {:?}", response.error);
    assert_eq!(response.data()["status"], "available", "{response:?}");
    response
}

fn relevance_of(response: &Response) -> Relevance {
    serde_json::from_value(response.data()["facts"]["relevance"].clone())
        .unwrap_or_else(|error| panic!("facts.relevance: {error}: {:?}", response.data()))
}

fn envelope_caps(response: &Response) -> Vec<String> {
    response.data()["facts"]["envelope"]["caps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|cap| cap.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn targets_facts_should_carry_the_relevance_block_and_repeat_it_identically() {
    let root = install_repo("carries");
    let mut service = ready(&root);
    let task = "how does install handle existing Claude settings";

    let first = facts(&mut service, task);
    let second = facts(&mut service, task);

    assert_eq!(first.data(), second.data(), "same inputs, same facts");
    assert_eq!(first.data()["inputs"]["algorithm_version"], 2);
    let relevance = relevance_of(&first);
    assert_eq!(relevance.files_considered, 5);
    assert!(relevance.graph);
    let counts: Vec<(&str, usize)> = relevance
        .keywords
        .iter()
        .map(|row| (row.keyword.as_str(), row.content_files))
        .collect();
    assert_eq!(
        counts,
        [
            ("does", 0),
            ("install", 2),
            ("handle", 1),
            ("existing", 1),
            ("claude", 0),
            ("settings", 3),
        ]
    );
    assert_eq!(
        relevance.cofiles[0].path,
        "crates/install/src/claude_settings.rs"
    );
    assert_eq!(relevance.cofiles[0].line, Some(1));
    assert!(
        !first.data()["facts"]["relevance"]
            .to_string()
            .contains("hunter2"),
        "a credential-shaped file's lines never reach the block"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn targets_facts_should_report_exactly_what_the_in_process_reader_reports() {
    let root = install_repo("agrees");
    let mut service = ready(&root);
    let index = IndexSet::open_or_build(&root, Box::new(TrigramExtractor)).unwrap();
    let graph = GraphStore::open_read_only(&service.graph_db_path()).unwrap();

    for task in [
        "how does install handle existing Claude settings",
        "la connexion ne marche pas",
        "quantum flux capacitor",
        "claude setting hooks",
        // `login` is also a synonym of `connexion`, and `auth` of `login`:
        // the ranking probes synonyms the relevance block must not depend on.
        "auth connexion",
    ] {
        let from_daemon = relevance_of(&facts(&mut service, task));
        let in_process = relevance_on(&index, Some(&graph), task).unwrap();
        assert_eq!(from_daemon, in_process, "task: {task}");
    }
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn targets_facts_should_borrow_a_synonym_for_a_french_keyword() {
    let root = install_repo("french");
    let mut service = ready(&root);

    let relevance = relevance_of(&facts(&mut service, "la connexion ne marche pas"));

    let row = &relevance.keywords[0];
    assert_eq!(row.keyword, "connexion");
    assert_eq!(row.via_expansion.as_deref(), Some("login"));
    assert_eq!(row.content_files, 1);
    assert_eq!(relevance.cofiles[0].path, "src/auth.rs");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn targets_facts_should_name_a_truncated_probe_in_the_block_and_in_the_response() {
    // 12 files x 100 lines = 1,200 matches of "needle", past the probe cap.
    let files: Vec<(String, String)> = (0..12)
        .map(|file| {
            (
                format!("f{file:02}.rs"),
                (0..100).map(|n| format!("// needle {n}\n")).collect(),
            )
        })
        .collect();
    let root = fixture("capped", &files);
    let mut service = ready(&root);

    let response = facts(&mut service, "needle probe");

    let cap = "content probe truncated at 1000 matches for keyword 'needle'; \
               files beyond the cap carry no content signal";
    let relevance = relevance_of(&response);
    let needle = &relevance.keywords[0];
    assert_eq!(needle.keyword, "needle");
    assert!(needle.truncated, "{needle:?}");
    assert_eq!(
        needle.content_files, 10,
        "a path-ordered prefix of the 12 files"
    );
    assert_eq!(relevance.caps, [cap]);
    assert_eq!(
        envelope_caps(&response)
            .iter()
            .filter(|named| named.as_str() == cap)
            .count(),
        1,
        "the ranking and the block name the same cap once"
    );
    assert_eq!(response.data()["facts"]["envelope"]["lower_bound"], true);
    let epistemics = response.epistemics.as_ref().unwrap();
    assert!(
        epistemics.lower_bound && !epistemics.closed_world,
        "{epistemics:?}"
    );
    assert!(epistemics.basis.contains(cap), "{}", epistemics.basis);
    assert!(
        response
            .warnings
            .iter()
            .any(|warning| warning.code == "RESULT_CAPPED" && warning.message == cap),
        "{:?}",
        response.warnings
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn targets_facts_should_name_a_task_longer_than_the_keyword_list_once() {
    let root = install_repo("long");
    let mut service = ready(&root);
    let task = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike";

    let response = facts(&mut service, task);

    let sentence = "task keywords truncated at 12; later task words contributed no signal";
    assert_eq!(relevance_of(&response).caps, [sentence]);
    let named = envelope_caps(&response);
    assert_eq!(
        named
            .iter()
            .filter(|cap| cap.starts_with("task keywords truncated"))
            .collect::<Vec<_>>(),
        [sentence],
        "the ranking's sentence and the block's are one sentence: {named:?}"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn targets_facts_should_leave_the_envelope_unchanged_when_the_block_has_no_cap() {
    let root = install_repo("quiet");
    let mut service = ready(&root);

    let response = facts(&mut service, "install hooks");

    assert!(
        relevance_of(&response).caps.len() == 1,
        "only the hidden .env"
    );
    let response = facts(&mut service, "pad hooks");
    assert!(relevance_of(&response).caps.is_empty());
    assert!(
        !envelope_caps(&response)
            .iter()
            .any(|cap| cap.contains("credential")),
        "{:?}",
        envelope_caps(&response)
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn targets_should_not_carry_a_relevance_block_outside_fact_mode() {
    let root = install_repo("plain");
    let mut service = ready(&root);

    let response = service.handle(Request::Targets {
        task: "install hooks".to_owned(),
        limit: Some(10),
        max_tier: None,
        precision: false,
        regions: false,
    });

    assert!(response.ok, "{:?}", response.error);
    let data: &Value = response.data();
    assert!(data.get("relevance").is_none(), "{data}");
    std::fs::remove_dir_all(&root).ok();
}
