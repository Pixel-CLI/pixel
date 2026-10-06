// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `gitpixel sniper` — one-look error capture queries. Thin dispatch over
//! `pixel_session::query` plus the
//! generic one-record ingest path (`report`) the JS adapters shell to.

use std::io::Read;
use std::path::PathBuf;

use clap::Subcommand;
use pixel_session::store::{Store, now_ms, resolve_project_root};
use pixel_session::types::{ReportEnvelope, Surface};
use pixel_session::{format, query, run};

#[derive(Subcommand)]
pub enum SniperCmd {
    /// Newest errors, compact one-liners + `cursor:` footer.
    Last {
        /// How many errors to show.
        #[arg(short = 'n', long = "count", default_value_t = 10)]
        n: i64,
        /// Only this capture surface (e.g. browser-rejection, vitest, tsc).
        #[arg(long)]
        surface: Option<String>,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Errors newer than a cursor (footer of every listing), or --ts 5m.
    Since {
        /// Last seen error id.
        cursor: Option<i64>,
        /// Time window instead of a cursor: 30s, 5m, 2h, 1d.
        #[arg(long)]
        ts: Option<String>,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Full detail for one error id: frames, values, run fingerprint, ±30s events.
    Show {
        id: i64,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Substring search over stored errors.
    Query {
        text: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// "Was my edit applied?" — recent HMR/reload/dep-optimize events.
    Hmr {
        /// Only hmr updates touching this file path fragment.
        #[arg(long)]
        file: Option<String>,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Latest run fingerprint; --diff compares against the previous run.
    Env {
        #[arg(long)]
        diff: bool,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Latest test signal: the newest vitest / Minitest / RSpec failure
    /// record against the newest test-pass event.
    Test {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Print the current cursor (highest error id).
    Cursor {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Apply retention now; --vacuum compacts the database file.
    Gc {
        #[arg(long)]
        vacuum: bool,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Ingest one JSON record ("-" = stdin). Errors by default; use
    /// {"type":"event",...} or {"type":"run",...} for lifecycle records.
    Report {
        /// File to read, or "-" for stdin.
        #[arg(default_value = "-")]
        input: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Wrap a command: tee its output live, mirror its exit code, and on
    /// failure record structured errors. tsc is parsed per TS code; Minitest
    /// and RSpec output (detected from the output, so `bundle exec rails
    /// test` and `bundle exec rspec` both work) gives one record per failing
    /// test (kind failure|error, class, name, file, line, message,
    /// project-only backtrace, rerun command) plus a `summary` record;
    /// rubocop gives one `lint` record per offense. Anything else gets a
    /// generic tail record + full output in raw_fallbacks. A green Minitest
    /// or RSpec run records a `test-pass` event with the run counters.
    Run {
        /// Name for the records (defaults to the command).
        #[arg(long)]
        label: Option<String>,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// The command and its arguments (after --).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
}

fn open_store(repo: &std::path::Path) -> Result<Store, String> {
    let root = resolve_project_root(repo).map_err(|e| e.to_string())?;
    Store::open(&root).map_err(|e| e.to_string())
}

fn emit<T: serde::Serialize>(
    value: &T,
    json: bool,
    pretty: impl FnOnce(&T) -> String,
) -> Result<(), String> {
    let text = if json {
        let mut s = serde_json::to_string(value).map_err(|e| e.to_string())?;
        s.push('\n');
        s
    } else {
        pretty(value)
    };
    print!("{text}");
    Ok(())
}

fn parse_surface(raw: Option<String>) -> Result<Option<Surface>, String> {
    match raw {
        None => Ok(None),
        Some(raw) => Surface::parse(&raw).map(Some).ok_or_else(|| {
            format!("unknown surface {raw:?} (e.g. browser-window, browser-rejection, vitest, tsc)")
        }),
    }
}

pub fn run_sniper(cmd: SniperCmd) -> Result<(), String> {
    match cmd {
        SniperCmd::Last {
            n,
            surface,
            repo,
            json,
        } => {
            let store = open_store(&repo)?;
            let surface = parse_surface(surface)?;
            let list = query::last(&store, n, surface).map_err(|e| e.to_string())?;
            emit(&list, json, |l| format::render_error_list(l, now_ms()))
        }
        SniperCmd::Since {
            cursor,
            ts,
            repo,
            json,
        } => {
            let store = open_store(&repo)?;
            let list = match (cursor, ts) {
                (Some(cursor), None) => query::since(&store, cursor),
                (None, Some(ts)) => {
                    let window = query::parse_duration_ms(&ts)
                        .ok_or_else(|| format!("bad duration {ts:?} (use 30s, 5m, 2h, 1d)"))?;
                    query::since_ts(&store, now_ms() - window)
                }
                _ => return Err("provide exactly one of <cursor> or --ts".into()),
            }
            .map_err(|e| e.to_string())?;
            emit(&list, json, |l| format::render_error_list(l, now_ms()))
        }
        SniperCmd::Show { id, repo, json } => {
            let store = open_store(&repo)?;
            match query::show(&store, id).map_err(|e| e.to_string())? {
                Some(result) => emit(&result, json, |r| format::render_show(r, now_ms())),
                None => Err(format!("no error with id {id}")),
            }
        }
        SniperCmd::Query { text, repo, json } => {
            let store = open_store(&repo)?;
            let list = query::search(&store, &text, 20).map_err(|e| e.to_string())?;
            emit(&list, json, |l| format::render_error_list(l, now_ms()))
        }
        SniperCmd::Hmr { file, repo, json } => {
            let store = open_store(&repo)?;
            let status = query::hmr(&store, file.as_deref()).map_err(|e| e.to_string())?;
            emit(&status, json, |s| format::render_hmr(s, now_ms()))
        }
        SniperCmd::Env { diff, repo, json } => {
            let store = open_store(&repo)?;
            let env = query::env(&store, diff).map_err(|e| e.to_string())?;
            emit(&env, json, format::render_env)
        }
        SniperCmd::Test { repo, json } => {
            let store = open_store(&repo)?;
            let status = query::test_status(&store).map_err(|e| e.to_string())?;
            emit(&status, json, |s| format::render_test(s, now_ms()))
        }
        SniperCmd::Cursor { repo, json } => {
            let store = open_store(&repo)?;
            let result = query::cursor(&store).map_err(|e| e.to_string())?;
            emit(&result, json, format::render_cursor)
        }
        SniperCmd::Gc { vacuum, repo, json } => {
            let store = open_store(&repo)?;
            let outcome = query::gc(&store, vacuum).map_err(|e| e.to_string())?;
            emit(&outcome, json, format::render_gc)
        }
        SniperCmd::Report { input, repo, json } => {
            let raw = if input == "-" {
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .map_err(|e| format!("read stdin: {e}"))?;
                buf
            } else {
                std::fs::read_to_string(&input).map_err(|e| format!("read {input}: {e}"))?
            };
            let value: serde_json::Value =
                serde_json::from_str(&raw).map_err(|e| format!("bad json: {e}"))?;
            let store = open_store(&repo)?;
            match ReportEnvelope::parse(value)? {
                ReportEnvelope::Error(input) => {
                    let recorded = store.record_error(&input).map_err(|e| e.to_string())?;
                    emit(&recorded, json, |r| {
                        format!(
                            "recorded #{}{}\n",
                            r.id,
                            if r.deduped {
                                format!(" (deduped, \u{d7}{})", r.count)
                            } else {
                                String::new()
                            }
                        )
                    })
                }
                ReportEnvelope::Event(input) => {
                    let id = store.record_event(&input).map_err(|e| e.to_string())?;
                    emit(&serde_json::json!({"event_id": id}), json, |_| {
                        format!("recorded event #{id}\n")
                    })
                }
                ReportEnvelope::Run(input) => {
                    store.record_run(&input).map_err(|e| e.to_string())?;
                    emit(&serde_json::json!({"run_id": input.run_id}), json, |_| {
                        format!("recorded run {}\n", input.run_id)
                    })
                }
            }
        }
        SniperCmd::Run { label, repo, cmd } => {
            let store = open_store(&repo)?;
            let code = run::run_wrapped(&store, label.as_deref(), &cmd)?;
            // Mirror the wrapped command's exit code exactly.
            std::process::exit(code);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_surface_name_parses_and_an_unknown_one_is_an_error() {
        assert_eq!(parse_surface(None), Ok(None));
        assert_eq!(
            parse_surface(Some("vitest".to_string())),
            Ok(Some(Surface::Vitest))
        );
        let err = parse_surface(Some("nope".to_string())).unwrap_err();
        assert!(err.contains("unknown surface \"nope\""), "{err}");
    }
}
