// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `ARCHITECTURE.md` against the tree (`.agents/rules/architecture-doc.md`):
//! the command table against `pixel --help`, the crate table against the
//! workspace and each crate's `[dependencies]`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The rows of the table under `## <heading>`, each as its cells.
fn table(doc: &str, heading: &str) -> Vec<Vec<String>> {
    let mut lines = doc.lines();
    let marker = format!("## {heading}");
    assert!(
        lines.any(|line| line.trim() == marker),
        "no `{marker}` in ARCHITECTURE.md"
    );
    lines
        .skip_while(|line| !line.starts_with('|'))
        .take_while(|line| line.starts_with('|'))
        .skip(2) // header and separator
        .map(|line| {
            line.trim()
                .trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().to_owned())
                .collect()
        })
        .collect()
}

fn name_of(cell: &str) -> String {
    cell.trim_matches('`').to_owned()
}

#[test]
fn the_command_table_is_the_help_in_order() {
    let doc = std::fs::read_to_string(workspace_root().join("ARCHITECTURE.md")).unwrap();
    let documented: Vec<String> = table(&doc, "Command surface")
        .iter()
        .map(|row| name_of(&row[0]))
        .collect();
    let help = Command::new(env!("CARGO_BIN_EXE_pixel"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&help.stdout).into_owned();
    let listed: Vec<String> = help
        .lines()
        .skip_while(|line| *line != "Commands:")
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| *name != "help")
        .map(str::to_owned)
        .collect();
    assert_eq!(documented, listed);
}

fn crate_dependencies(manifest: &str) -> BTreeSet<String> {
    let parsed: toml_edit::DocumentMut = manifest.parse().unwrap();
    parsed
        .get("dependencies")
        .and_then(|deps| deps.as_table())
        .map(|deps| {
            deps.iter()
                .map(|(name, _)| name.to_owned())
                .filter(|name| name.starts_with("pixel-"))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn the_crate_table_is_the_workspace_and_its_dependencies() {
    let root = workspace_root();
    let doc = std::fs::read_to_string(root.join("ARCHITECTURE.md")).unwrap();
    let workspace: toml_edit::DocumentMut = std::fs::read_to_string(root.join("Cargo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    let members: BTreeSet<String> = workspace["workspace"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| {
            member
                .as_str()
                .unwrap()
                .trim_start_matches("crates/")
                .to_owned()
        })
        .collect();
    let rows = table(&doc, "Crates");
    let documented: BTreeSet<String> = rows.iter().map(|row| name_of(&row[0])).collect();
    assert_eq!(documented, members, "one row per workspace member");
    for row in &rows {
        let name = name_of(&row[0]);
        let manifest =
            std::fs::read_to_string(root.join("crates").join(&name).join("Cargo.toml")).unwrap();
        let actual = crate_dependencies(&manifest);
        let cell = row[2].split("(dev:").next().unwrap_or("");
        let stated: BTreeSet<String> = cell
            .split(',')
            .map(|dep| dep.trim().trim_matches('`').to_owned())
            .filter(|dep| dep.starts_with("pixel-"))
            .collect();
        assert_eq!(stated, actual, "`Depends on` of {name}");
    }
}

/// The command words of `pixel --help`.
fn help_commands() -> BTreeSet<String> {
    let help = Command::new(env!("CARGO_BIN_EXE_pixel"))
        .arg("--help")
        .output()
        .unwrap();
    String::from_utf8_lossy(&help.stdout)
        .lines()
        .skip_while(|line| *line != "Commands:")
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_skill_names_only_commands_that_exist_and_fits_its_budget() {
    let body = pixel_setup::skill::PIXEL_SKILL
        .split_once("---\n\n")
        .map(|(_, body)| body)
        .unwrap();
    assert!(
        body.len() <= 2_048,
        "the skill body is {} bytes",
        body.len()
    );
    let commands = help_commands();
    let mut named = BTreeSet::new();
    for span in body.split('`').skip(1).step_by(2) {
        let mut words = span.split_whitespace();
        if words.next() == Some("pixel")
            && let Some(command) = words
                .next()
                .filter(|word| word.chars().all(|c| c.is_ascii_lowercase() || c == '-'))
        {
            named.insert(command.to_owned());
        }
    }
    assert!(named.len() >= 8, "{named:?}");
    for command in &named {
        assert!(
            commands.contains(command),
            "the skill names `pixel {command}`"
        );
    }
}
