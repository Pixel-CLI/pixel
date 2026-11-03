// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use super::*;

fn argv(args: &[&str]) -> Vec<String> {
    std::iter::once("pixel")
        .chain(args.iter().copied())
        .map(String::from)
        .collect()
}

/// The clap error `run` would get for `args`, from the real definition.
/// Parsed on a thread with a 4 MiB stack — the derived command tree
/// overflows the 2 MiB a test thread gets, the same reason
/// `validate_cli_syntax` spawns one.
fn parse_error(args: &[String]) -> clap::Error {
    let args = args.to_vec();
    std::thread::Builder::new()
        .stack_size(4 * 1024 * 1024)
        .spawn(move || {
            Cli::command()
                .try_get_matches_from(args)
                .expect_err("these argvs do not parse")
        })
        .unwrap()
        .join()
        .unwrap()
}

/// A global flag before the verb used to move `run-hook` off `argv[1]`, and
/// the hook then exited 2, which Claude Code reads as a blocking error.
#[test]
fn a_hook_parse_error_fails_open_whatever_precedes_the_verb() {
    for args in [
        &["run-hook", "--bogus"][..],
        &["--metrics=off", "run-hook", "--bogus"],
        &["--metrics", "off", "run-hook", "--bogus"],
        &["--metrics", "off", "hook", "prompt-submit"],
        &["-v", "run-hook", "task-event"],
        &[
            "--metrics=off",
            "run-hook",
            "metrics",
            "--provider",
            "newer-host",
        ],
    ] {
        let argv = argv(args);
        assert!(fails_open(&argv, &parse_error(&argv)), "{args:?}");
    }
}

/// Only the subcommand position counts: a `run-hook` or `hook` word later
/// in another command's argv, or a help request, keeps clap's exit.
#[test]
fn a_non_hook_parse_error_or_a_help_request_does_not_fail_open() {
    for args in [
        &["status", "hook", "--newer-flag"][..],
        &["--metrics", "off", "status", "--newer-flag", "run-hook"],
        &["--metrics", "run-hook", "status", "--newer-flag"],
        &["run-hook", "--help"],
    ] {
        let argv = argv(args);
        assert!(!fails_open(&argv, &parse_error(&argv)), "{args:?}");
    }
}

#[test]
fn subcommand_token_skips_options_and_the_value_of_metrics() {
    let cases: [(&[&str], Option<&str>); 6] = [
        (&["run-hook", "task-event"], Some("run-hook")),
        (&["--metrics", "off", "status"], Some("status")),
        (&["--metrics=off", "hook"], Some("hook")),
        (&["-v", "--newer", "doctor"], Some("doctor")),
        (&["--metrics"], None),
        (&[], None),
    ];
    for (args, expected) in cases {
        assert_eq!(subcommand_token(&argv(args)), expected, "{args:?}");
    }
}
