// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use clap::CommandFactory;

use super::*;
use crate::cli::Cli;

#[test]
fn spec_should_carry_every_label_with_its_criterion() {
    let spec = spec("add a flag").unwrap();
    assert_eq!(spec.labels.len(), INTENTS.len());
    for kind in INTENTS {
        assert_eq!(spec.criteria[kind.label], kind.criterion, "{}", kind.label);
    }
}

#[test]
fn ops_for_should_map_each_intent_to_its_documented_ops() {
    let ops = |label| ops_for(label).unwrap();
    assert_eq!(ops("bugfix")[0], "pixel search-history \"<text>\"");
    assert_eq!(ops("feature")[0], "pixel find-symbol \"<name>\"");
    assert_eq!(ops("review"), ["pixel review", "pixel what-changed"]);
    assert_eq!(ops("ops"), ["pixel status", "pixel doctor"]);
    assert!(ops_for("chit-chat").is_none());
}

#[test]
fn every_op_names_a_command_the_binary_has() {
    let commands: Vec<String> = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            Cli::command()
                .get_subcommands()
                .map(|command| command.get_name().to_owned())
                .collect()
        })
        .unwrap()
        .join()
        .unwrap();
    for kind in INTENTS {
        assert!(!kind.ops.is_empty(), "{}", kind.label);
        for op in kind.ops {
            let command = op.split_whitespace().nth(1).unwrap();
            assert!(commands.iter().any(|name| name == command), "{op}");
        }
    }
}

#[test]
fn the_labels_are_the_ones_the_spec_ships() {
    assert_eq!(
        labels(),
        [
            "bugfix",
            "feature",
            "refactor",
            "investigate",
            "question",
            "review",
            "ops"
        ]
    );
}
