// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use super::{Cli, READ_ONLY_COMMANDS, read_only_invocation};
use clap::CommandFactory;

#[test]
fn every_read_only_label_is_a_real_command() {
    // Building the full clap command overflows a test thread's default
    // 2 MiB stack; give the builder room.
    let check = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            let cli = Cli::command();
            let real: Vec<&str> = cli.get_subcommands().map(clap::Command::get_name).collect();
            for label in READ_ONLY_COMMANDS {
                assert!(
                    real.contains(label),
                    "{label} is not a subcommand name; the allow-list entry is dead"
                );
            }
        })
        .unwrap();
    check.join().unwrap();
}

fn read_only(args: &[&str]) -> bool {
    let args: Vec<String> = args.iter().map(ToString::to_string).collect();
    let check = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let matches = Cli::command()
                .try_get_matches_from(
                    std::iter::once("pixel").chain(args.iter().map(std::string::String::as_str)),
                )
                .unwrap();
            read_only_invocation(&matches)
        })
        .unwrap();
    check.join().unwrap()
}

#[test]
fn read_only_should_accept_retrieval_and_refuse_state_changers() {
    assert!(read_only(&["search-content", "pattern"]));
    assert!(read_only(&["impact", "symbol"]));
    assert!(read_only(&["repo-map", "."]));
    // Every one of these re-run would mutate state: the gate must keep
    // the update question away from them.
    for args in [
        &["setup"][..],
        &["install", "github:acme/tool"],
        &["uninstall", "tool"],
        &["config", "classify-engine", "local"],
        &["scope-task", "--clear"],
        &["build-index", "."],
        &["doctor"],
        &["self-update"],
        &["run-hook", "guard"],
        &["rename", "a", "b"],
    ] {
        assert!(!read_only(args), "{args:?} must not relaunch");
    }
}
