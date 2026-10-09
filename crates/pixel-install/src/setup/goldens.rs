// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The golden files under `tests/setup/.agents/`.
//!
//! One file per agent per scope, each one **exactly** what `pixel setup` writes
//! into that agent's instruction file, so the whole matrix is readable in one
//! diff instead of inferred from the code. Two scopes, because they are two
//! layouts: a machine has `~/.claude/CLAUDE.md`, a repository has `CLAUDE.md`.
//!
//! The comparison is made against an in-process render-and-apply, never through
//! the command line: the `--selected-agents` / `--selected-features` /
//! `--dummy-apply` flags that drive the wizard from a script are removed before
//! the release, and this test has to outlive them.
//!
//! Regenerate after an intentional wording change with
//! `PIXEL_SETUP_UPDATE_GOLDENS=1 cargo test -p pixel-install --lib setup::goldens`,
//! then read the diff: a golden that moves when it should not is the test doing
//! its job.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::agent::{AgentTarget, Scope};
use super::feature::{ConfigValue, Feature, Writes};
use super::files::{BLOCK_END, BLOCK_START};
use super::plan::SetupPlan;

/// The repository root, from this crate's manifest: the goldens live in the
/// repository, not under `target/`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/pixel-install has two parents")
        .to_path_buf()
}

/// Where the goldens live, relative to the repository root.
fn golden_dir() -> PathBuf {
    repo_root().join("tests").join("setup").join(".agents")
}

/// The golden path of one agent in one scope.
fn golden_path(agent: AgentTarget, scope: Scope) -> PathBuf {
    let folder = match scope {
        Scope::Global => "global",
        Scope::Repository => "repository",
    };
    let name = agent.name().to_ascii_lowercase().replace(' ', "-");
    golden_dir().join(folder).join(format!("{name}.md"))
}

/// The features a wizard run starts from: the catalog defaults the scope can
/// honour.
fn default_features(scope: Scope) -> Vec<Feature> {
    Feature::ALL
        .iter()
        .copied()
        .filter(|feature| {
            feature.default_selected() && !(feature.repo_local_only() && scope != Scope::Repository)
        })
        .collect()
}

/// What one agent's setup writes, in a scratch directory: the path it wrote,
/// relative to the base, and the file's content.
fn apply_one(agent: AgentTarget, scope: Scope) -> Vec<(String, String)> {
    let scratch = scratch_dir(agent, scope);
    let plan = SetupPlan::new(&scratch, scope, &[agent], &default_features(scope))
        .expect("a single agent resolves to a plan");
    let written = plan
        .apply()
        .expect("a writable scratch directory accepts the block");
    let files = written
        .iter()
        .map(|path| {
            let content = std::fs::read_to_string(path).expect("the apply wrote this file");
            (
                path.strip_prefix(&scratch)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/"),
                content,
            )
        })
        .collect();
    std::fs::remove_dir_all(&scratch).ok();
    files
}

/// A scratch directory of its own: two cases of the same test would otherwise
/// write over each other under `cargo test`, which shares one process.
fn scratch_dir(agent: AgentTarget, scope: Scope) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pixel-setup-golden-{}-{}-{scope:?}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_nanos(),
        agent.index()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

/// Write `content` to `path`, or compare the file that is already there.
///
/// Comparing is the default: a golden that updates itself is a golden nobody
/// reads. `PIXEL_SETUP_UPDATE_GOLDENS=1` is the deliberate override, and the
/// diff it produces is reviewed like any other.
fn assert_or_write(path: &Path, content: &str, what: &str) {
    if std::env::var_os("PIXEL_SETUP_UPDATE_GOLDENS").is_some() {
        std::fs::create_dir_all(path.parent().expect("a golden has a parent"))
            .expect("create the golden directory");
        std::fs::write(path, content).expect("write the golden");
        return;
    }
    let golden = std::fs::read_to_string(path).unwrap_or_else(|err| {
        panic!(
            "missing golden {} for {what}: {err}\n\
             regenerate with PIXEL_SETUP_UPDATE_GOLDENS=1 \
             cargo test -p pixel-install --lib setup::goldens",
            path.display()
        )
    });
    assert_eq!(golden, content, "golden {} for {what}", path.display());
}

#[test]
fn every_agent_matches_its_golden_in_every_scope_that_writes_a_file() {
    let mut with_file = 0;
    let mut without_file = Vec::new();
    for scope in [Scope::Global, Scope::Repository] {
        for agent in AgentTarget::ALL {
            let files = apply_one(agent, scope);
            let golden = golden_path(agent, scope);
            match files.as_slice() {
                [(path, content)] => {
                    assert_or_write(
                        &golden,
                        content,
                        &format!("{} in {scope:?} at {path}", agent.name()),
                    );
                    with_file += 1;
                }
                [] => without_file.push(format!("{} in {scope:?}", agent.name())),
                other => panic!(
                    "{} writes {} files in {scope:?}: {other:?}",
                    agent.name(),
                    other.len()
                ),
            }
            if std::env::var_os("PIXEL_SETUP_UPDATE_GOLDENS").is_none() && !files.is_empty() {
                assert!(
                    golden.exists(),
                    "{} has no golden at {golden:?}",
                    agent.name()
                );
            }
        }
    }
    assert_eq!(
        with_file, 14,
        "five agents machine-wide (the others report instead), nine per repository"
    );
    assert_eq!(
        without_file,
        vec![
            "Devin in Global".to_string(),
            "Cursor in Global".to_string(),
            "Antigravity in Global".to_string(),
            "AGENTS.md in Global".to_string(),
        ],
        "an agent pixel writes no file for is reported, never silently skipped"
    );
}

#[test]
fn a_golden_is_the_block_itself_with_the_users_text_left_alone() {
    for scope in [Scope::Global, Scope::Repository] {
        for agent in AgentTarget::ALL {
            let Ok(golden) = std::fs::read_to_string(golden_path(agent, scope)) else {
                continue;
            };
            assert!(
                golden.starts_with(BLOCK_START) && golden.ends_with(&format!("{BLOCK_END}\n")),
                "the {} golden in {scope:?} is the block and nothing else: {golden:?}",
                agent.name()
            );
            assert!(
                golden.contains("## Pixel\n"),
                "{} golden in {scope:?} lost the baseline",
                agent.name()
            );
            assert_eq!(
                golden.matches(BLOCK_START).count(),
                1,
                "{} golden in {scope:?} holds more than one block",
                agent.name()
            );
        }
    }
}

#[test]
fn two_agents_reading_the_same_file_get_identical_bytes() {
    let mut by_path: BTreeMap<String, (String, String)> = BTreeMap::new();
    for agent in AgentTarget::ALL {
        for (path, content) in apply_one(agent, Scope::Repository) {
            match by_path.get(&path) {
                Some((owner, previous)) => assert_eq!(
                    &content,
                    previous,
                    "{path} differs between {owner} and {}: a file two agents read \
                     must not depend on which one was selected",
                    agent.name()
                ),
                None => {
                    by_path.insert(path, (agent.name().to_string(), content));
                }
            }
        }
    }
    assert_eq!(
        by_path.keys().collect::<Vec<_>>(),
        vec!["AGENTS.md", "CLAUDE.md", "GEMINI.md"],
        "the repository scope writes three files, whatever is selected"
    );
}

#[test]
fn the_configuration_golden_matches_the_keys_the_default_selection_writes() {
    let keys: Vec<(&[&str], String)> = Feature::ALL
        .iter()
        .filter(|feature| feature.default_selected())
        .filter_map(|feature| match feature.writes() {
            Writes::Config(key, value) => Some((key, render(key, value))),
            _ => None,
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            (&["brief"][..], "brief: true".to_string()),
            (&["metrics"][..], "metrics: \"on\"".to_string()),
            (
                &["daemon_auto_start"][..],
                "daemon_auto_start: true".to_string()
            ),
        ],
        "the golden is written from this list: a feature added here has to reach it"
    );

    let golden = golden_dir().join("config.yaml");
    let expected = format!(
        "# Generated by `cargo test -p pixel-install --lib setup::goldens`.\n\
         # The global settings `pixel setup` writes for its default features.\n{}\n",
        keys.iter()
            .map(|(_key, rendered)| format!("{rendered}\n"))
            .collect::<Vec<_>>()
            .concat()
    );
    assert_or_write(&golden, &expected, "the default global configuration");
}

/// A configuration key and value as the YAML document would show them.
fn render(key: &[&str], value: ConfigValue) -> String {
    let rendered = match value {
        ConfigValue::Bool(on) => on.to_string(),
        ConfigValue::Text(text) => format!("\"{text}\""),
    };
    format!("{}: {rendered}", key.join("."))
}
