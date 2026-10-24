// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! One invocation from argv to exit status (`specs/metrics.md`): parse, run
//! the command, print the answer, then write the action-log line and the 🟩
//! block. The accounting is never skipped; `--metrics=off`, `PIXEL_METRICS=0`
//! and a disabled sidecar only silence the block.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{CommandFactory, FromArgMatches};
use pixel_metrics::{ActionEvent, ActionLog, OperationMetrics, Provenance};
use serde_json::json;

use crate::cli::Cli;
use crate::commands::{self, Done};
use crate::{operation, render, serve_trace};

/// Bumped when the shape of `Provenance` changes.
const PROVENANCE_SCHEMA: u32 = 1;

/// The optional sequential round-trip policy, in milliseconds.
const ROUND_TRIP_ENV: &str = "PIXEL_METRICS_ROUND_TRIP_MS";

/// Who is running: the producing build, the invocation, and nothing else —
/// an ordinary shell has no inferred host.
pub(crate) fn provenance(source: &str) -> Provenance {
    Provenance {
        schema_version: PROVENANCE_SCHEMA,
        pixel_version: env!("CARGO_PKG_VERSION").to_owned(),
        build_commit: env!("PIXEL_BUILD_COMMIT").to_owned(),
        build_dirty: env!("PIXEL_GIT_DIRTY").parse().ok(),
        source: source.to_owned(),
        invocation_id: pixel_metrics::invocation_id(),
        provider: None,
        session: None,
    }
}

pub(crate) fn action(command: &str, args: String, source: &str) -> ActionEvent {
    let provenance = provenance(source);
    let mut event = ActionEvent::new(command, args);
    event.invocation_id = Some(provenance.invocation_id.clone());
    event.provenance = Some(provenance);
    event
}

/// The arguments as the action log records them. A secret on the command
/// line (`config remote-key <preset> <key>`) is masked before it reaches any
/// sink: the log is plain text under `.pixel/`.
pub fn logged_args(args: &[String]) -> String {
    let secret_from = args
        .windows(2)
        .position(|pair| pair[0] == "config" && pair[1] == "remote-key")
        .map(|at| at + 3);
    // `install https://user:token@host/…` carries the credential of a private
    // repository in its URL. Only the command word counts: `search-content
    // install` is a query.
    let with_program: Vec<String> = std::iter::once(String::new())
        .chain(args.iter().cloned())
        .collect();
    let source_from = pixel_plugin::command_word(&with_program)
        .filter(|(_, word)| *word == "install")
        .map(|(at, _)| at);
    args.iter()
        .enumerate()
        .map(|(at, arg)| {
            if secret_from.is_some_and(|from| at >= from) && !arg.starts_with("--") {
                "<redacted>".to_owned()
            } else if source_from.is_some_and(|from| at >= from) {
                pixel_plugin::redact_source(arg)
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The path a command operates on, for the action log's directory: the
/// innermost subcommand's `path`/`paths` argument, `.` otherwise.
fn operation_path(matches: &clap::ArgMatches) -> Option<PathBuf> {
    if let Some((_, nested)) = matches.subcommand()
        && let Some(path) = operation_path(nested)
    {
        return Some(path);
    }
    if let Ok(Some(path)) = matches.try_get_one::<PathBuf>("path") {
        return Some(path.clone());
    }
    matches
        .try_get_many::<PathBuf>("paths")
        .ok()
        .flatten()
        .and_then(|mut paths| paths.next().cloned())
}

/// Whether the parsed invocation asked for JSON.
fn json_requested(matches: &clap::ArgMatches) -> bool {
    if let Some((_, nested)) = matches.subcommand() {
        return json_requested(nested);
    }
    matches.try_get_one::<bool>("json").ok().flatten() == Some(&true)
}

/// The failure a `--json` caller reads on stdout: the same envelope shape a
/// success has, with no result.
fn failure_json(command: &str, message: &str) -> String {
    json!({"ok": false, "op": command, "error": {"message": message}}).to_string()
}

fn print_answer(done: &Done) {
    if done.json {
        let pretty = serde_json::to_string_pretty(&done.value).unwrap_or_default();
        operation::print(format_args!("{pretty}\n"));
    } else {
        let (text, cut) = render::cap_text(&done.text, render::STDOUT_BYTE_CAP);
        operation::print(format_args!("{text}"));
        if cut {
            operation::print_error(format_args!(
                "⚠ output cut at {} bytes; narrow the query or page with --offset\n",
                render::STDOUT_BYTE_CAP
            ));
        }
    }
}

pub fn run() -> ExitCode {
    let started = Instant::now();
    let argv: Vec<String> = std::env::args().collect();
    let matches = match Cli::command().try_get_matches() {
        Ok(matches) => matches,
        // A hook never blocks its host: a flag this binary does not know is
        // silence, not exit status 2.
        Err(error) if error.use_stderr() && argv.get(1).is_some_and(|word| word == "hook") => {
            let _ = error.print();
            return ExitCode::SUCCESS;
        }
        Err(error) => match crate::plugin_cmd::dispatch(&error, &argv) {
            Some(code) => return ExitCode::from(u8::try_from(code).unwrap_or(1)),
            None => error.exit(),
        },
    };
    if let Ok(Cli {
        command: crate::cli::Command::Hook { host, event, part },
        ..
    }) = Cli::from_arg_matches(&matches)
    {
        crate::hook::run(host, event, part);
    }
    let command_label = matches.subcommand_name().unwrap_or("unknown").to_owned();
    let path = operation_path(&matches).unwrap_or_else(|| PathBuf::from("."));
    let json = json_requested(&matches);
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };
    let root = crate::client::discover_root(&path)
        .or_else(|_| crate::client::discover_root(Path::new(".")));
    let foreground_daemon = matches!(
        &cli.command,
        crate::cli::Command::Daemon {
            cmd: crate::cli::DaemonCmd::Start {
                foreground: true,
                ..
            }
        }
    );
    let live = !foreground_daemon
        && cli.metrics != "off"
        && crate::settings::metrics_on(root.as_ref().ok().map(PathBuf::as_path));
    operation::begin(root.as_deref().unwrap_or(Path::new(".")));
    let mut logger = match (&root, foreground_daemon) {
        (Ok(root), false) => ActionLog::spawn_for_root(root),
        _ => ActionLog::noop(),
    };

    let result = commands::run(&cli.command, cli.no_daemon);
    let (outcome, exit) = match &result {
        Ok(done) => {
            print_answer(done);
            for note in &done.notes {
                operation::print_error(format_args!("{note}\n"));
            }
            (Ok(()), done.exit)
        }
        Err(message) => {
            if json && operation::stdout_bytes() == 0 {
                operation::print(format_args!("{}\n", failure_json(&command_label, message)));
            }
            operation::print_error(format_args!("pixel: {message}\n"));
            (Err(message.clone()), 1)
        }
    };
    let trailer = outcome
        .as_ref()
        .err()
        .filter(|_| live)
        .map(|message| format!("pixel: {message}\n"));
    let _ = std::io::stdout().flush();
    let elapsed = started.elapsed();
    let logged_result = match (&outcome, exit) {
        (Ok(()), code) if code != 0 => Err(format!("{command_label} exited {code}")),
        _ => outcome.clone(),
    };
    let mut event =
        action(&command_label, logged_args(&argv[1..]), "cli").with_result(&logged_result, elapsed);
    event.serve = serve_trace::take();
    if !foreground_daemon {
        let output_bytes =
            operation::output_bytes() + trailer.as_ref().map_or(0, |text| text.len() as u64);
        let mut metrics = match operation::evidence(&command_label, outcome.is_ok()) {
            Ok(evidence) => OperationMetrics::new(elapsed, output_bytes, Some(evidence)),
            Err(gap) => OperationMetrics::new(elapsed, output_bytes, None).with_comparison_gap(gap),
        };
        if let Some(round_trip_ms) = std::env::var(ROUND_TRIP_ENV)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
        {
            metrics = metrics.with_round_trip_ms(round_trip_ms);
        }
        metrics.output_scope = Some("cli-rendered-streams".to_owned());
        metrics.answer_bytes = Some(operation::stdout_bytes());
        event = event.with_metrics(metrics);
    }
    if live && let Some(line) = event.finalize_metrics_line() {
        // A blank record keeps transcript UIs from attaching the block to
        // the command's output; its bytes are accounted for.
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "\n{line}");
        if let Some(trailer) = &trailer {
            let _ = stderr.write_all(trailer.as_bytes());
        }
    }
    logger.log(event);
    // The record must be on disk before exit: `pixel metrics` and the next
    // invocation read it back.
    logger.finish_flush();
    if exit == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(u8::try_from(exit).unwrap_or(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_remote_key_is_masked_in_the_logged_arguments() {
        assert_eq!(
            logged_args(&args(&["config", "remote-key", "openai", "sk-secret"])),
            "config remote-key openai <redacted>"
        );
        assert_eq!(
            logged_args(&args(&["config", "remote-key", "openai", "-", "--json"])),
            "config remote-key openai <redacted> --json"
        );
    }

    #[test]
    fn other_arguments_are_logged_as_given() {
        assert_eq!(
            logged_args(&args(&["search-content", "remote-key", "src"])),
            "search-content remote-key src"
        );
        assert_eq!(logged_args(&[]), "");
    }

    #[test]
    fn the_path_of_an_operation_is_the_innermost_path_argument() {
        let matches = Cli::command()
            .try_get_matches_from(["pixel", "daemon", "status", "/tmp/x"])
            .unwrap();
        assert_eq!(operation_path(&matches), Some(PathBuf::from("/tmp/x")));
        let matches = Cli::command()
            .try_get_matches_from(["pixel", "search-content", "p", "a", "b"])
            .unwrap();
        assert_eq!(operation_path(&matches), Some(PathBuf::from("a")));
        let matches = Cli::command()
            .try_get_matches_from(["pixel", "status"])
            .unwrap();
        assert_eq!(operation_path(&matches), Some(PathBuf::from(".")));
    }

    #[test]
    fn json_is_requested_by_the_innermost_flag_only() {
        let on = Cli::command()
            .try_get_matches_from(["pixel", "status", "--json"])
            .unwrap();
        let off = Cli::command()
            .try_get_matches_from(["pixel", "status"])
            .unwrap();
        assert!(json_requested(&on));
        assert!(!json_requested(&off));
    }

    #[test]
    fn a_json_failure_names_the_command_and_the_message() {
        let parsed: serde_json::Value =
            serde_json::from_str(&failure_json("symbol", "no symbol named \"x\"")).unwrap();
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["op"], "symbol");
        assert_eq!(parsed["error"]["message"], "no symbol named \"x\"");
    }

    #[test]
    fn provenance_names_this_build_and_a_fresh_invocation() {
        let first = provenance("cli");
        let second = provenance("cli");
        assert_eq!(first.pixel_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(first.source, "cli");
        assert_ne!(first.invocation_id, second.invocation_id);
    }
}
