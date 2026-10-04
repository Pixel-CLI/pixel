// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `env_read` concepts: the literal environment-variable names Rust code
//! reads, extracted, stored with their owner, resolved by name, and kept
//! current by an incremental update.

use std::path::{Path, PathBuf};

use pixel_graph::build::{build_graph, update_file};
use pixel_graph::concept::{ConceptKind, extract_concepts};
use pixel_graph::concept_resolve::{ConceptMatch, ResolveOptions, Tier, resolve};
use pixel_graph::store::GraphStore;

/// A default value that must never reach an `env_read` concept.
const SENTINEL: &str = "sentinel default value never stored";

fn source() -> String {
    [
        "use std::env;",                                                       // 1
        "fn config(key: &str) {",                                              // 2
        &format!("    let a = std::env::var(\"PIXEL_FLOW_DIR\").unwrap_or_else(|_| \"{SENTINEL}\".into());"), // 3
        "    let b = env::var_os(\"HOME\");",                                  // 4
        "    let c = env!(\"CARGO_PKG_VERSION\");",                            // 5
        "    let d = option_env!(\"PIXEL_BUILD_SHA\");",                       // 6
        "    let e = env::var(\"_LEADING_UNDERSCORE\");",                      // 7
        "    let f = std::env::var(format!(\"PIXEL_{key}\"));",                // 8
        "    let name = \"PIXEL_INDIRECT\";",                                  // 9
        "    let g = env::var(name);",                                         // 10
        "    std::env::set_var(\"PIXEL_WRITTEN\", \"1\");",                    // 11
        "    let h = env::var(\"has space\");",                                // 12
        "    let i = env::var(\"9LIVES\");",                                   // 13
        "    let j = myenv::var(\"NOT_ENV_PATH\");",                           // 14
        "    let k = env::args(\"NOT_A_READ\");",                              // 15
        "    let l = concat!(\"v\", env!(\"PIXEL_NESTED\"), option_env!(\"PIXEL_OPT\"));", // 16
        "    let m = concat!(stringify!(\"NOT_ENV_MACRO\"), env, \"NOT_AFTER_ENV\");", // 17
        "}",                                                                   // 18
        "",
    ]
    .join("\n")
}

fn env_reads(path: &str, content: &str) -> Vec<(String, String, u32)> {
    extract_concepts(path, content.as_bytes())
        .into_iter()
        .filter(|c| c.kind == ConceptKind::EnvRead)
        .map(|c| (c.raw, c.detail, c.start_line))
        .collect()
}

#[test]
fn extraction_should_keep_only_literal_reads_by_name() {
    let reads = env_reads("src/config.rs", &source());
    assert_eq!(
        reads,
        vec![
            ("PIXEL_FLOW_DIR".into(), "runtime read (env::var)".into(), 3),
            ("HOME".into(), "runtime read (env::var_os)".into(), 4),
            (
                "CARGO_PKG_VERSION".into(),
                "build-time read (env!)".into(),
                5
            ),
            (
                "PIXEL_BUILD_SHA".into(),
                "build-time read (option_env!)".into(),
                6
            ),
            (
                "_LEADING_UNDERSCORE".into(),
                "runtime read (env::var)".into(),
                7
            ),
            ("PIXEL_NESTED".into(), "build-time read (env!)".into(), 16),
            (
                "PIXEL_OPT".into(),
                "build-time read (option_env!)".into(),
                16
            ),
        ],
        "computed names, writes, non-names and other paths stay out"
    );
}

#[test]
fn no_concept_should_carry_a_default_value_as_an_env_read() {
    let concepts = extract_concepts("src/config.rs", source().as_bytes());
    for c in concepts.iter().filter(|c| c.kind == ConceptKind::EnvRead) {
        for field in [&c.raw, &c.norm, &c.detail] {
            assert!(!field.contains("sentinel"), "{c:?}");
        }
    }
    assert_eq!(
        ConceptKind::parse(ConceptKind::EnvRead.as_str()),
        ConceptKind::EnvRead
    );
    assert_eq!(ConceptKind::EnvRead.as_str(), "env_read");
}

fn tmpdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pixel-env-read-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Two functions reading `PIXEL_FLOW_DIR`, on lines 2 and 5.
fn lib_source(key: &str) -> String {
    [
        "pub fn flow_dir() -> Option<String> {",
        &format!("    std::env::var(\"{key}\").ok()"),
        "}",
        "pub fn flow_dir_os() -> bool {",
        &format!("    std::env::var_os(\"{key}\").is_some()"),
        "}",
        "",
    ]
    .join("\n")
}

/// The `env_read` matches naming exactly `phrase`: the fuzzy tiers may add
/// reads of a neighbouring name, which are not reads of this one.
fn env_matches(db: &Path, phrase: &str) -> Vec<(String, u32, Option<String>)> {
    let store = GraphStore::open(db).unwrap();
    let outcome = resolve(&store, phrase, &ResolveOptions::default()).unwrap();
    let mut found: Vec<(String, u32, Option<String>)> = outcome
        .matches
        .iter()
        .filter(|m: &&ConceptMatch| m.kind == ConceptKind::EnvRead && m.raw == phrase)
        .map(|m| (m.path.clone(), m.start_line, m.owner.clone()))
        .collect();
    found.sort();
    found
}

#[test]
fn resolve_should_land_on_every_read_with_its_owner_and_follow_an_edit() {
    let root = tmpdir("resolve");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), lib_source("PIXEL_FLOW_DIR")).unwrap();
    let db = tmpdir("db").join("graph.db");
    build_graph(&root, &db).unwrap();

    let expected = vec![
        ("src/lib.rs".to_string(), 2, Some("flow_dir".to_string())),
        ("src/lib.rs".to_string(), 5, Some("flow_dir_os".to_string())),
    ];
    assert_eq!(env_matches(&db, "PIXEL_FLOW_DIR"), expected);
    {
        let store = GraphStore::open(&db).unwrap();
        let outcome = resolve(&store, "pixel_flow_dir env", &ResolveOptions::default()).unwrap();
        assert_eq!(
            outcome.tier,
            Some(Tier::T1),
            "the head noun `env` directs to env reads"
        );
        assert_eq!(outcome.matches.len(), 2, "{:?}", outcome.matches);
    }

    // Rename the key and update the one file: the old name is gone, the new
    // one is found, exactly as a full rebuild would have it.
    std::fs::write(root.join("src/lib.rs"), lib_source("PIXEL_FLOW_HOME")).unwrap();
    update_file(&root, &db, "src/lib.rs").unwrap();
    assert_eq!(env_matches(&db, "PIXEL_FLOW_DIR"), Vec::new());
    let incremental = env_matches(&db, "PIXEL_FLOW_HOME");
    assert_eq!(incremental, expected);
    let rebuilt = tmpdir("db-full").join("graph.db");
    build_graph(&root, &rebuilt).unwrap();
    assert_eq!(env_matches(&rebuilt, "PIXEL_FLOW_HOME"), incremental);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_all_caps_name_should_reach_its_reads_before_a_lowercase_function() {
    let root = tmpdir("caps");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let source = [
        "pub fn codex_home() -> Option<std::ffi::OsString> {",
        "    std::env::var_os(\"CODEX_HOME\")",
        "}",
        "",
    ]
    .join("\n");
    std::fs::write(root.join("src/lib.rs"), source).unwrap();
    let db = tmpdir("db-caps").join("graph.db");
    build_graph(&root, &db).unwrap();
    assert_eq!(
        env_matches(&db, "CODEX_HOME"),
        vec![("src/lib.rs".to_string(), 2, Some("codex_home".to_string()))]
    );
    let store = GraphStore::open(&db).unwrap();
    let lowercase = resolve(&store, "codex_home", &ResolveOptions::default()).unwrap();
    assert_eq!(
        lowercase.tier,
        Some(Tier::Ident),
        "a lowercase query still finds the function"
    );
    let mixed = resolve(&store, "Codex_Home", &ResolveOptions::default()).unwrap();
    assert_eq!(
        mixed.tier,
        Some(Tier::Ident),
        "a mixed-case query retries in lowercase"
    );
    let _ = std::fs::remove_dir_all(&root);
}
