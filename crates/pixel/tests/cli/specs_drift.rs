// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `specs/` drift: the specs state contracts, and the binary is the only
//! source of truth for the parts of them a machine can read.
//!
//! - every subcommand of `pixel --help` has one row in `specs/README.md`'s
//!   command index, in `--help` order, and a row that names a spec names one
//!   that exists and mentions the command;
//! - every `` `pixel <name>` `` a spec quotes is a real command (the rule
//!   `docs_drift` applies to the other docs);
//! - the provider and event lists of `hooks.md`, the settings of `config.md`,
//!   the facets of `search-history.md` and the protocol version of `ops.md` are
//!   the ones the binary accepts.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::docs_drift::{
    documentable_commands, referenced_commands, repo_root, subcommands_in_help_order,
};
use crate::support::{neutral_cwd, pixel_command};

fn specs_dir() -> PathBuf {
    repo_root().join("specs")
}

fn spec(name: &str) -> String {
    let path = specs_dir().join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The rows of the `## Command index` table: command name, and the spec file
/// it names (`None` for `—`).
fn index_rows(readme: &str) -> Vec<(String, Option<String>)> {
    let table = readme
        .split("## Command index")
        .nth(1)
        .expect("specs/README.md has a `## Command index` section");
    table
        .lines()
        .filter_map(|line| line.strip_prefix("| `"))
        .filter_map(|rest| {
            let (name, rest) = rest.split_once("` | ")?;
            let cell = rest.split(" | ").next()?.trim();
            let file = cell
                .strip_prefix('`')
                .and_then(|c| c.strip_suffix('`'))
                .map(str::to_string);
            Some((name.to_string(), file))
        })
        .collect()
}

/// Why the index does not match `commands` (in `--help` order), if it does
/// not. `mentions` says whether a spec file exists and quotes `` `pixel name` ``.
fn index_problem(
    readme: &str,
    commands: &[String],
    mentions: impl Fn(&str, &str) -> Option<bool>,
) -> Option<String> {
    let rows = index_rows(readme);
    let names: Vec<&str> = rows.iter().map(|(name, _)| name.as_str()).collect();
    let wanted: Vec<&str> = commands.iter().map(String::as_str).collect();
    if names != wanted {
        let missing: Vec<&&str> = wanted.iter().filter(|c| !names.contains(c)).collect();
        let extra: Vec<&&str> = names.iter().filter(|c| !wanted.contains(c)).collect();
        return Some(format!(
            "the command index must list every `pixel --help` command once, in help order: \
             missing {missing:?}, unknown {extra:?}"
        ));
    }
    for (name, file) in rows {
        let Some(file) = file else { continue };
        match mentions(&file, &name) {
            None => return Some(format!("`{name}` names {file}, which does not exist")),
            Some(false) => {
                return Some(format!("{file} never mentions `pixel {name}`"));
            }
            Some(true) => {}
        }
    }
    None
}

fn on_disk(file: &str, command: &str) -> Option<bool> {
    let text = std::fs::read_to_string(specs_dir().join(file)).ok()?;
    Some(text.contains(&format!("`pixel {command}")))
}

#[test]
fn the_command_index_should_list_every_command_once_in_help_order() {
    let readme = spec("README.md");
    let problem = index_problem(&readme, &subcommands_in_help_order(), on_disk);
    assert_eq!(problem, None);
}

#[test]
fn a_command_missing_from_the_index_should_fail_the_check() {
    let readme = spec("README.md");
    let kept: String = readme
        .lines()
        .filter(|line| !line.starts_with("| `search-history` |"))
        .map(|line| format!("{line}\n"))
        .collect();
    assert_ne!(kept, readme, "the row this test removes is gone already");
    let problem = index_problem(&kept, &subcommands_in_help_order(), on_disk)
        .expect("a removed row must be reported");
    assert!(problem.contains("search-history"), "{problem}");
}

#[test]
fn a_row_naming_a_spec_that_does_not_mention_it_should_fail_the_check() {
    let readme = spec("README.md");
    let wrong = readme.replace("| `config` | `config.md` |", "| `config` | `metrics.md` |");
    assert_ne!(wrong, readme);
    let problem = index_problem(&wrong, &subcommands_in_help_order(), on_disk)
        .expect("a spec that never quotes the command must be reported");
    assert!(
        problem.contains("never mentions `pixel config`"),
        "{problem}"
    );
}

#[test]
fn every_spec_should_quote_only_real_commands() {
    let known = documentable_commands();
    let mut stale = Vec::new();
    let mut files = 0;
    for entry in std::fs::read_dir(specs_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        files += 1;
        let text = std::fs::read_to_string(&path).unwrap();
        for name in referenced_commands(&text) {
            if !known.contains(&name) {
                stale.push(format!("{}: `pixel {name}`", path.display()));
            }
        }
    }
    assert!(files >= 8, "specs/ lists too few files: {files}");
    assert!(stale.is_empty(), "{stale:#?}");
}

/// The backticked tokens of `text` that do not start with a dash.
fn tokens(text: &str) -> BTreeSet<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter(|token| !token.starts_with('-') && !token.is_empty())
        .map(str::to_string)
        .collect()
}

/// The `[possible values: a, b]` of the option `--flag` in `help`.
fn possible_values(help: &str, flag: &str) -> BTreeSet<String> {
    let line = help
        .lines()
        .find(|line| line.trim_start().starts_with(flag))
        .unwrap_or_else(|| panic!("no {flag} in help:\n{help}"));
    let list = line
        .split("[possible values: ")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .unwrap_or_else(|| panic!("{flag} lists no possible values: {line}"));
    list.split(", ").map(str::to_string).collect()
}

fn help_of(args: &[&str]) -> String {
    let out = pixel_command().args(args).arg("--help").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// The text of the bullet that opens with `marker`, from the end of the
/// marker to the first `stop` after it.
fn bullet_after(doc: &str, marker: &str, stop: &str) -> String {
    let rest = doc
        .split(marker)
        .nth(1)
        .unwrap_or_else(|| panic!("no bullet `{marker}`"));
    rest.split(stop).next().unwrap().to_string()
}

#[test]
fn hooks_should_list_the_providers_and_events_the_binary_accepts() {
    let hooks = spec("hooks.md");
    let help = help_of(&["run-hook", "task-event"]);
    let providers = tokens(&bullet_after(
        &hooks,
        "- `--provider` of `task-event`:",
        "(`",
    ));
    assert_eq!(providers, possible_values(&help, "--provider"));
    let events = tokens(&bullet_after(
        &hooks,
        "- `--event` of `task-event` (`TaskHookEvent`):",
        ".",
    ));
    assert_eq!(events, possible_values(&help, "--event"));
}

#[test]
fn config_should_list_the_settings_the_binary_accepts() {
    let out = pixel_command()
        .args(["config", "no-such-setting"])
        .current_dir(neutral_cwd())
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let list = stderr
        .split("the settings are ")
        .nth(1)
        .and_then(|rest| rest.split(" (").next())
        .unwrap_or_else(|| panic!("the refusal lists no settings: {stderr}"));
    let binary: BTreeSet<String> = list.split(", ").map(str::to_string).collect();
    let config = spec("config.md");
    let table = bullet_after(
        &config,
        "The flat settings `pixel config <key> <value>` writes are exactly four:",
        "An environment value",
    );
    let documented: BTreeSet<String> = table
        .lines()
        .filter_map(|line| line.strip_prefix("| `"))
        .filter_map(|rest| rest.split_once('`').map(|(key, _)| key.to_string()))
        .collect();
    assert_eq!(documented, binary);
}

#[test]
fn search_history_should_list_the_facets_the_binary_accepts() {
    let doc = spec("search-history.md");
    let listed = tokens(&bullet_after(&doc, "- `--facet`:", ". "));
    let facets = possible_values(&help_of(&["search-history"]), "--facet");
    assert_eq!(listed, facets);
}

#[test]
fn ops_should_name_the_protocol_version_of_the_daemon() {
    let ops = spec("ops.md");
    let tail = ops
        .split("`PROTOCOL_VERSION` (`pixel_daemon::api`, ")
        .nth(1)
        .expect(
            "ops.md names the version as `PROTOCOL_VERSION` (`pixel_daemon::api`, N on this head)",
        );
    let named: u64 = tail
        .split(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .expect("a number follows");
    assert_eq!(named, pixel_daemon::api::PROTOCOL_VERSION);
    assert!(
        ops.contains(pixel_proto::REQUEST_DEADLINE_FIELD),
        "ops.md does not name the deadline member"
    );
}
