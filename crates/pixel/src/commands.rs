// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! What each command does (`ARCHITECTURE.md`, "Command surface"): build the
//! op, route it, and turn the answer into [`Done`] — the JSON result, the
//! text for a person and the exit status.
//!
//! The ops with a wire form go through [`client::execute`]; `review`,
//! `call-path`, `list-signatures` and `rename` are in-process calls on the
//! graph (`specs/ops.md`: "Not wire ops").

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pixel_daemon::{Request, Service};
use pixel_graph::analysis::concept;
use pixel_graph::analysis::resolve::{Resolved, resolve};
use pixel_graph::analysis::review::{Severity, review};
use pixel_graph::analysis::trace::trace;
use pixel_graph::rename::{self, Role, Skipped};
use pixel_graph::store::Store;
use pixel_graph::tree;
use pixel_index::history::FactsStore;
use pixel_index::history::ingest::{IngestOptions, ingest_until_fresh_within};
use pixel_metrics::{InProcessReason, ServeRoute, ServeStep};
use pixel_model::{Symbol, SymbolKind};
use serde_json::{Value, json};

use crate::cli::{Command, DaemonCmd, FailOn};
use crate::client::{self, Probe};
use crate::{operation, render, serve_trace};

/// The wall-clock cap of `prepare --history`.
const HISTORY_PREPARE_CAP: Duration = Duration::from_secs(600);

/// The largest file `pack-context` reads.
const PACK_FILE_CAP: u64 = 2 * 1024 * 1024;

/// How long an in-process graph refresh waits for another writer.
const GRAPH_LOCK_WAIT: Duration = Duration::from_secs(5);

/// How long `daemon start` waits for the new daemon to answer.
const DAEMON_START_WAIT: Duration = Duration::from_secs(5);

/// What a command produced.
pub struct Done {
    /// The result, as `--json` prints it.
    pub value: Value,
    /// The text a person reads on stdout.
    pub text: String,
    /// Lines for stderr, before the metrics block.
    pub notes: Vec<String>,
    /// The exit status when it is not 0 (`review --fail-on`).
    pub exit: i32,
    /// Whether `--json` was asked.
    pub json: bool,
}

impl Done {
    pub(crate) fn new(value: Value, text: String, json: bool) -> Done {
        operation::observe(&value);
        Done {
            value,
            text,
            notes: Vec::new(),
            exit: 0,
            json,
        }
    }

    pub(crate) fn note(mut self, note: Option<String>) -> Done {
        self.notes.extend(note);
        self
    }
}

/// One op asked of every repository of the workspace the repository at `path`
/// holds, the answers kept apart per repository.
fn across_workspace(
    path: &Path,
    op: &dyn Fn() -> Request,
    no_daemon: bool,
    json: bool,
    render: &dyn Fn(&Value) -> String,
) -> Result<Done, String> {
    let root = crate::workspace_cmd::root_of(path);
    let results = crate::workspace_cmd::fan_out(&root, op, &|member, request| {
        call(member, request, no_daemon)
    })?;
    Ok(crate::workspace_cmd::fan_out_done(&results, json, render))
}

/// Runs a command. `no_daemon` answers in this process.
pub fn run(command: &Command, no_daemon: bool) -> Result<Done, String> {
    match command {
        Command::Prepare {
            path,
            history,
            json,
        } => prepare(path, *history, *json),
        Command::Status { path, json } => {
            let data = call(path, Request::Status {}, no_daemon)?;
            Ok(Done::new(data.clone(), render::status(&data), *json))
        }
        Command::SearchContent {
            pattern,
            paths,
            globs,
            types,
            files_only,
            limit,
            offset,
            json,
        } => search_content(
            pattern,
            paths,
            globs,
            types,
            *files_only,
            *limit,
            *offset,
            *json,
            no_daemon,
        ),
        Command::SearchMeaning {
            query,
            path,
            limit,
            download,
            json,
        } => crate::meaning_cmd::run(query, path, *limit, *download, *json, no_daemon),
        Command::FindCode {
            phrase,
            path,
            limit,
            json,
        } => find_code(phrase, path, *limit, *json, no_daemon),
        Command::FindSymbol { name, path, json } => {
            let data = call(
                path,
                Request::Symbol {
                    name: name.clone(),
                    stale_ok: false,
                },
                no_daemon,
            )?;
            Ok(Done::new(data.clone(), render::symbols(&data), *json))
        }
        Command::WhoCalls {
            name,
            path,
            workspace,
            callees,
            offset,
            json,
        } => {
            let op = || Request::Callers {
                stale_ok: false,
                uid_or_name: name.clone(),
                role: if *callees { "callees" } else { "callers" }.to_owned(),
                offset: Some(*offset),
            };
            if *workspace {
                return across_workspace(path, &op, no_daemon, *json, &render::callers);
            }
            let data = call(path, op(), no_daemon)?;
            Ok(Done::new(data.clone(), render::callers(&data), *json))
        }
        Command::Impact {
            name,
            path,
            workspace,
            direction,
            depth,
            json,
        } => {
            let op = || Request::Impact {
                stale_ok: false,
                uid_or_name: name.clone(),
                direction: direction.as_str().to_owned(),
                depth: *depth,
            };
            if *workspace {
                return across_workspace(path, &op, no_daemon, *json, &render::impact);
            }
            let data = call(path, op(), no_daemon)?;
            Ok(Done::new(data.clone(), render::impact(&data), *json))
        }
        Command::Workspace { cmd } => crate::workspace_cmd::run(cmd),
        Command::Index { cmd } => crate::index_cmd::run(cmd),
        Command::WhatChanged {
            path,
            base,
            tests,
            offset,
            json,
        } => {
            let op = Request::Changes {
                base: base.clone(),
                offset: Some(*offset),
                include_tests: *tests,
            };
            let data = call(path, op, no_daemon)?;
            Ok(Done::new(data.clone(), render::changes(&data), *json))
        }
        Command::Review {
            path,
            base,
            fail_on,
            json,
        } => review_command(path, base.as_deref(), *fail_on, *json),
        Command::CallPath {
            from,
            to,
            path,
            depth,
            json,
        } => call_path(from, to, path, *depth, *json),
        Command::ListSignatures { file, path, json } => list_signatures(file, path, *json),
        Command::PackContext {
            name,
            path,
            budget,
            json,
        } => pack_context(name, path, *budget, *json),
        Command::RepoMap {
            path,
            markdown,
            json,
        } => {
            let data = call(
                path,
                Request::Map {
                    markdown: *markdown,
                },
                no_daemon,
            )?;
            Ok(Done::new(data.clone(), render::repo_map(&data), *json))
        }
        Command::SearchHistory {
            query,
            path,
            facet,
            limit,
            read_only,
            json,
        } => {
            let op = Request::History {
                query: query.clone(),
                facet: facet.clone(),
                limit: *limit,
                read_only: *read_only,
            };
            let data = call(path, op, no_daemon)?;
            Ok(Done::new(data.clone(), render::history(&data), *json))
        }
        Command::Rename {
            name,
            to,
            path,
            apply,
            include_likely,
            json,
        } => rename_command(name, to, path, *apply, *include_likely, *json),
        Command::Brief {
            prompt,
            path,
            session,
            host,
            parts,
            without,
            json,
        } => brief(
            &BriefAsk {
                prompt,
                path,
                session: session.as_deref(),
                host,
                parts: *parts,
                without,
            },
            *json,
            no_daemon,
        ),
        Command::Daemon { cmd } => daemon(cmd),
        Command::Doctor {
            path,
            fix,
            fail_on,
            only,
            skip,
            shell,
            list,
            json,
        } => crate::doctor_cmd::run(&crate::doctor_cmd::Request {
            path,
            fix: *fix,
            fail_on: match fail_on {
                crate::cli::DoctorFailOn::Yellow => pixel_setup::install::CheckStatus::Yellow,
                crate::cli::DoctorFailOn::Red => pixel_setup::install::CheckStatus::Red,
            },
            only,
            skip,
            shell: shell.as_deref(),
            list: *list,
            json: *json,
        }),
        Command::Config { cmd: Some(cmd), .. } => crate::config_cmd::classify_run(cmd),
        Command::Classify(options) => {
            crate::classify::run(options.clone())?;
            Ok(Done::new(json!({}), String::new(), false))
        }
        Command::Config {
            cmd: None,
            key,
            value,
            global,
            unset,
            path,
            json,
        } => crate::config_cmd::run(&crate::config_cmd::Request {
            key: key.as_deref(),
            value: value.as_deref(),
            global: *global,
            unset: *unset,
            path,
            json: *json,
        }),
        Command::Install {
            source,
            name,
            allow_unverified,
            json,
        } => crate::plugin_cmd::install(source, name.as_deref(), *allow_unverified, *json),
        Command::Uninstall { name } => crate::plugin_cmd::uninstall(name),
        Command::Plugin { cmd } => crate::plugin_cmd::run(cmd),
        Command::Metrics { since, path, json } => {
            crate::metrics_cmd::run(&crate::metrics_cmd::Request {
                since,
                path,
                json: *json,
            })
        }
        Command::Setup {
            agent,
            json,
            print,
            yes,
            remove,
            dry_run,
            binary_path,
        } => crate::setup_cmd::setup(&crate::setup_cmd::Request {
            agents: agent,
            json: *json,
            print: *print,
            yes: *yes,
            remove: *remove,
            dry_run: *dry_run,
            binary_path: binary_path.as_deref(),
        }),
        // `run::run` answers a hook before it reaches here; this arm only
        // keeps the match total.
        Command::Hook { .. } => Ok(Done::new(json!({}), String::new(), false)),
    }
}

/// What `pixel brief` asks for.
struct BriefAsk<'a> {
    prompt: &'a str,
    path: &'a Path,
    session: Option<&'a str>,
    host: &'a str,
    parts: u8,
    without: &'a [String],
}

/// `pixel brief`: ask the daemon for the prompt's brief, record the decision in
/// the journal and print the block. `brief: off` in the settings, or
/// `PIXEL_BRIEF=0`, computes none.
fn brief(ask: &BriefAsk, json: bool, no_daemon: bool) -> Result<Done, String> {
    let BriefAsk {
        prompt,
        path,
        session,
        host,
        parts,
        without,
    } = *ask;
    if crate::settings::brief_off(client::discover_root(path).ok().as_deref()) {
        let value = json!({"text": null, "declined": "disabled", "paths": []});
        return Ok(Done::new(value, String::new(), json).note(Some(
            "no brief: switched off (`pixel config brief`, or PIXEL_BRIEF)".to_owned(),
        )));
    }
    let root = client::discover_root(path)?;
    let data = call(
        path,
        Request::Brief {
            prompt: prompt.to_owned(),
            session: session.unwrap_or_default().to_owned(),
            host: host.to_owned(),
            parts,
            without: without.to_vec(),
        },
        no_daemon,
    )?;
    journal(&root, &data["record"]);
    let text = data["text"].as_str().map(|text| format!("{text}\n"));
    let note = if text.is_none() {
        Some(format!("no brief: {}", render::brief_reason(&data)))
    } else {
        None
    };
    Ok(Done::new(data.clone(), text.unwrap_or_default(), json).note(note))
}

/// Appends a hook decision to `.pixel/journal.jsonl`, best effort: the journal
/// never costs the command. A repository without `.pixel/` gets none.
fn journal(root: &Path, record: &Value) {
    let Some(path) = pixel_metrics::journal::path_for(root) else {
        return;
    };
    let mut line = record.clone();
    line["provenance"] = serde_json::to_value(crate::run::provenance("brief")).unwrap_or_default();
    let _ =
        pixel_metrics::journal::append(&path, &line.to_string(), pixel_metrics::journal::MAX_LINES);
}

/// Routes `op` and returns its result; an ambiguity failure lists the
/// candidates so the caller can pass a uid.
fn call(path: &Path, op: Request, no_daemon: bool) -> Result<Value, String> {
    let response = client::execute(path, op, no_daemon)?;
    if !response.ok
        && let Some(error) = &response.error
        && let Some(candidates) = error
            .ambiguity
            .as_ref()
            .and_then(|a| a.get("candidates"))
            .and_then(Value::as_array)
    {
        let mut message = error.message.clone();
        for candidate in candidates {
            message.push_str(&format!(
                "\n  {}  ({}:{})",
                candidate.get("uid").and_then(Value::as_str).unwrap_or(""),
                candidate.get("file").and_then(Value::as_str).unwrap_or(""),
                candidate.get("line").and_then(Value::as_u64).unwrap_or(0)
            ));
        }
        return Err(message);
    }
    client::unwrap_response(response)
}

/// A path argument mapped to the repository root it lies in and, when it
/// names something below the root, the root-relative prefix.
fn scope_of(path: &Path) -> Result<(PathBuf, Option<String>), String> {
    let root = client::discover_root(path)?;
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let prefix = canonical
        .strip_prefix(&root)
        .ok()
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .filter(|relative| !relative.is_empty());
    Ok((root, prefix))
}

#[allow(clippy::too_many_arguments)] // one parameter per flag of the command
fn search_content(
    pattern: &str,
    paths: &[PathBuf],
    globs: &[String],
    types: &[String],
    files_only: bool,
    limit: Option<usize>,
    offset: usize,
    json: bool,
    no_daemon: bool,
) -> Result<Done, String> {
    // Group the arguments by repository: one request per root, with the
    // prefixes it was asked to cover.
    let mut by_root: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    let mut whole: Vec<PathBuf> = Vec::new();
    for path in paths {
        let (root, prefix) = scope_of(path)?;
        match prefix {
            Some(prefix) => by_root.entry(root).or_default().push(prefix),
            None => whole.push(root),
        }
    }
    // An argument that is a whole root covers it, whatever else was asked.
    for root in whole {
        by_root.insert(root, Vec::new());
    }
    let mut matches: Vec<Value> = Vec::new();
    let mut next_offset = None;
    for (root, prefixes) in by_root {
        let op = Request::Search {
            stale_ok: false,
            pattern: pattern.to_owned(),
            json,
            limit,
            offset: Some(offset),
            paths: (!prefixes.is_empty()).then_some(prefixes),
            scope: None,
            globs: globs.to_vec(),
            types: types.to_vec(),
        };
        let data = call(&root, op, no_daemon)?;
        matches.extend(data["matches"].as_array().cloned().unwrap_or_default());
        next_offset = data["next_offset"].as_u64().or(next_offset);
    }
    let value = json!({
        "matches": matches,
        "offset": offset,
        "next_offset": next_offset,
        "truncated": next_offset.is_some(),
    });
    let text = render::search(&value, files_only);
    let note = render::search_note(&value);
    Ok(Done::new(value, text, json).note(note))
}

/// Opens the graph for an in-process command: refreshed from the working
/// tree, then read.
fn open_graph(path: &Path) -> Result<(PathBuf, Store), String> {
    let root = client::discover_root(path)?;
    let clock = Instant::now();
    pixel_graph::refresh(&root, Some(GRAPH_LOCK_WAIT), None).map_err(|err| err.to_string())?;
    let store = Store::open(&root).map_err(|err| err.to_string())?;
    let mut step = ServeStep::in_process(InProcessReason::NotRouted);
    step.route = ServeRoute::InProcess;
    step.open_ms = Some(serve_trace::millis_since(clock));
    serve_trace::record(step);
    Ok((root, store))
}

/// The one symbol `token` names; several are an error listing them.
fn one(store: &Store, token: &str) -> Result<Symbol, String> {
    match resolve(store, token).map_err(|err| err.to_string())? {
        Resolved::One(symbol) => Ok(symbol),
        Resolved::None => Err(format!("no symbol named {token:?}")),
        Resolved::Many(found) => {
            let mut message = format!("{token:?} names {} symbols; pass a uid", found.len());
            for symbol in found.iter().take(20) {
                message.push_str(&format!(
                    "\n  {}  ({}:{})",
                    symbol.uid, symbol.file, symbol.line
                ));
            }
            Err(message)
        }
    }
}

fn severity_threshold(fail_on: FailOn) -> Severity {
    match fail_on {
        FailOn::Blocker => Severity::Critical,
        FailOn::Concern => Severity::High,
        FailOn::Suggestion => Severity::Medium,
    }
}

fn review_command(
    path: &Path,
    base: Option<&str>,
    fail_on: Option<FailOn>,
    json: bool,
) -> Result<Done, String> {
    let (root, store) = open_graph(path)?;
    let diff = match base {
        Some(base) => tree::diff_against(&root, base),
        None => tree::diff(&root),
    }
    .map_err(|err| err.to_string())?;
    let report = review(&store, &root, &diff).map_err(|err| err.to_string())?;
    let mut text = String::new();
    for finding in &report.findings {
        let place = match (&finding.file, finding.line) {
            (Some(file), Some(line)) => format!(" {file}:{line}"),
            (Some(file), None) => format!(" {file}"),
            _ => String::new(),
        };
        text.push_str(&format!(
            "[{}] {}{place}: {}\n    {}\n",
            finding.severity.label(),
            finding.rule,
            finding.evidence,
            finding.fix_hint
        ));
    }
    if text.is_empty() {
        text.push_str("no findings\n");
    }
    let value = json!({
        "findings": report.findings.iter().map(|f| json!({
            "rule": f.rule,
            "severity": f.severity.label(),
            "file": f.file,
            "line": f.line,
            "evidence": f.evidence,
            "fix_hint": f.fix_hint,
        })).collect::<Vec<_>>(),
        "caps": report.caps,
    });
    let mut done = Done::new(value, text, json);
    done.notes
        .extend(report.caps.iter().map(|cap| format!("⚠ {cap}")));
    if let Some(fail_on) = fail_on {
        let threshold = severity_threshold(fail_on);
        if report.findings.iter().any(|f| f.severity >= threshold) {
            done.exit = 1;
        }
    }
    Ok(done)
}

fn call_path(from: &str, to: &str, path: &Path, depth: usize, json: bool) -> Result<Done, String> {
    let (_root, store) = open_graph(path)?;
    let (start, end) = (one(&store, from)?, one(&store, to)?);
    let hops = trace(&store, &start, &end, depth).map_err(|err| err.to_string())?;
    let Some(hops) = hops else {
        let value = json!({"from": start.uid, "to": end.uid, "path": null});
        return Ok(Done::new(
            value,
            format!(
                "no chain of resolved calls from {} to {} within {depth} calls\n",
                start.name, end.name
            ),
            json,
        ));
    };
    let mut text = format!("{} ({}:{})\n", start.name, start.file, start.line);
    for hop in &hops {
        let tag = if hop.tier == pixel_model::Tier::Likely {
            " [likely]"
        } else {
            ""
        };
        text.push_str(&format!(
            "  → {} ({}:{}) called at line {}{tag}\n",
            hop.symbol.name, hop.symbol.file, hop.symbol.line, hop.line
        ));
    }
    let value = json!({
        "from": start.uid,
        "to": end.uid,
        "path": hops.iter().map(|hop| json!({
            "uid": hop.symbol.uid,
            "name": hop.symbol.name,
            "file": hop.symbol.file,
            "line": hop.symbol.line,
            "called_at": hop.line,
            "tier": match hop.tier { pixel_model::Tier::Proved => "proved", _ => "likely" },
        })).collect::<Vec<_>>(),
    });
    Ok(Done::new(value, text, json))
}

/// `file` as the index names it: relative to the root, whether given
/// relative to the working directory or to the root itself.
fn root_relative(root: &Path, file: &str) -> String {
    let given = Path::new(file);
    let absolute = if given.is_absolute() {
        given.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| given.to_path_buf(), |cwd| cwd.join(given))
    };
    match absolute
        .canonicalize()
        .ok()
        .and_then(|p| p.strip_prefix(root).ok().map(Path::to_path_buf))
    {
        Some(relative) => relative.to_string_lossy().replace('\\', "/"),
        None => file.trim_start_matches("./").to_owned(),
    }
}

fn list_signatures(file: &str, path: &Path, json: bool) -> Result<Done, String> {
    let (root, store) = open_graph(path)?;
    let relative = root_relative(&root, file);
    let symbols: Vec<Symbol> = store
        .symbols_in(&relative)
        .map_err(|err| err.to_string())?
        .into_iter()
        .filter(|s| !matches!(s.kind, SymbolKind::File | SymbolKind::Package))
        .collect();
    if symbols.is_empty() {
        return Err(format!("no symbols indexed for {relative}"));
    }
    let rows: Vec<Value> = symbols
        .iter()
        .map(|s| json!({"line": s.line, "end_line": s.end_line, "kind": s.kind.to_string(), "name": s.name, "signature": s.signature, "file": s.file}))
        .collect();
    let text = render::signatures(&relative, &rows);
    Ok(Done::new(
        json!({"file": relative, "symbols": rows}),
        text,
        json,
    ))
}

/// Rows `find-code` reads from the text index before ranking them.
const FIND_CODE_ROWS: usize = 400;

/// The most leads `find-code` prints.
const FIND_CODE_MAX: usize = 100;

fn find_code(
    phrase: &str,
    path: &Path,
    limit: usize,
    json: bool,
    no_daemon: bool,
) -> Result<Done, String> {
    let words = concept::words(phrase);
    let pattern = concept::pattern(&words)
        .ok_or_else(|| "find-code needs at least one word of two characters or more".to_owned())?;
    let data = call(
        path,
        Request::Search {
            stale_ok: false,
            pattern,
            json: false,
            limit: Some(FIND_CODE_ROWS),
            offset: Some(0),
            paths: None,
            scope: None,
            globs: Vec::new(),
            types: Vec::new(),
        },
        no_daemon,
    )?;
    let hits = data["matches"].as_array().cloned().unwrap_or_default();
    let capped = data["next_offset"].as_u64().is_some();
    let mut leads = concept::rank(
        phrase,
        hits.iter().filter_map(|row| {
            Some((
                row["path"].as_str()?.to_owned(),
                u32::try_from(row["line"].as_u64()?).ok()?,
                row["text"].as_str()?.to_owned(),
            ))
        }),
        limit.clamp(1, FIND_CODE_MAX),
    );
    // The owner is a courtesy: no graph, no owner, the lead stands.
    if let Ok((_root, store)) = open_graph(path) {
        let _ = concept::with_owners(&store, &mut leads);
    }
    let rows: Vec<Value> = leads
        .iter()
        .map(|lead| {
            json!({
                "path": lead.path,
                "line": lead.line,
                "kind": lead.kind.as_str(),
                "owner": lead.owner,
                "text": lead.text,
                "score": lead.score,
            })
        })
        .collect();
    let mut text = String::new();
    for lead in &leads {
        text.push_str(&format!(
            "{}:{} {}",
            lead.path,
            lead.line,
            lead.kind.as_str()
        ));
        if let Some(owner) = &lead.owner {
            text.push_str(&format!(" in {owner}"));
        }
        text.push_str(&format!(" — {}\n", lead.text));
    }
    if leads.is_empty() {
        text.push_str("no line carries those words\n");
    }
    let value = json!({"phrase": phrase, "leads": rows, "capped": capped});
    let note = capped.then(|| {
        format!("⚠ the text index stopped at {FIND_CODE_ROWS} matching lines; narrow the phrase")
    });
    Ok(Done::new(value, text, json).note(note))
}

fn pack_context(name: &str, path: &Path, budget: usize, json: bool) -> Result<Done, String> {
    let (root, store) = open_graph(path)?;
    let symbol = one(&store, name)?;
    let file = pixel_io::repo_path::confine(&root, &symbol.file).map_err(|err| err.to_string())?;
    let text = pixel_io::nofollow::read_text_capped(&file, PACK_FILE_CAP)
        .map_err(|err| err.to_string())?;
    let packed = pixel_graph::analysis::context::pack(&symbol, &text, budget).ok_or_else(|| {
        format!(
            "{} no longer holds {} at line {}: run `pixel prepare`",
            symbol.file, symbol.name, symbol.line
        )
    })?;
    let value = json!({
        "symbol": {"uid": symbol.uid, "name": symbol.name, "kind": symbol.kind.to_string(), "file": symbol.file, "line": symbol.line, "end_line": symbol.end_line},
        "start_line": packed.start_line,
        "end_line": packed.end_line,
        "cut_lines": packed.cut_lines,
        "tokens": packed.tokens,
        "budget_tokens": budget,
        "text": packed.render(),
    });
    let text = format!(
        "{}:{}-{} {} ({})\n{}",
        symbol.file,
        packed.start_line,
        packed.end_line,
        symbol.name,
        symbol.kind,
        packed.render()
    );
    Ok(Done::new(value, text, json))
}

fn rename_command(
    name: &str,
    to: &str,
    path: &Path,
    apply: bool,
    include_likely: bool,
    json: bool,
) -> Result<Done, String> {
    let (root, store) = open_graph(path)?;
    let symbol = one(&store, name)?;
    let plan =
        rename::plan(&store, &root, &symbol, to, include_likely).map_err(|err| err.to_string())?;
    let mut text = String::new();
    for edit in &plan.edits {
        let role = match edit.role {
            Role::Definition => "definition",
            Role::Call => "call",
            Role::Import => "import",
        };
        text.push_str(&format!(
            "{}:{} {role}: {} → {}\n",
            edit.file, edit.line, plan.from, plan.to
        ));
    }
    let mut notes = Vec::new();
    for skip in &plan.skipped {
        let why = match &skip.reason {
            Skipped::Likely => "only probably this symbol (use --include-likely)".to_owned(),
            Skipped::NotWritten => "the name is not written on the line (an alias?)".to_owned(),
            Skipped::Ambiguous(count) => format!("the name is written {count} times on the line"),
            Skipped::Unparsed => "the file could not be parsed".to_owned(),
        };
        notes.push(format!("skipped {}:{}: {why}", skip.file, skip.line));
    }
    if plan.open_sites > 0 {
        notes.push(format!(
            "⚠ {} unresolved site(s) call a function of this name; the rename does not see them",
            plan.open_sites
        ));
    }
    let written = if apply {
        rename::apply(&store, &root, &plan).map_err(|err| err.to_string())?
    } else {
        notes.push("dry run: pass --apply to write".to_owned());
        Vec::new()
    };
    let value = json!({
        "symbol": symbol.uid,
        "from": plan.from,
        "to": plan.to,
        "applied": apply,
        "edits": plan.edits.iter().map(|e| json!({"file": e.file, "line": e.line})).collect::<Vec<_>>(),
        "skipped": plan.skipped.len(),
        "open_sites": plan.open_sites,
        "written": written,
    });
    let mut done = Done::new(value, text, json);
    done.notes = notes;
    Ok(done)
}

fn prepare(path: &Path, history: bool, json: bool) -> Result<Done, String> {
    let root = client::discover_root(path)?;
    let clock = Instant::now();
    let service = Service::open(&root).map_err(|err| err.to_string())?;
    let index_ms = serve_trace::millis_since(clock);
    drop(service);
    let clock = Instant::now();
    let graph =
        pixel_graph::refresh(&root, Some(GRAPH_LOCK_WAIT), None).map_err(|err| err.to_string())?;
    let graph_ms = serve_trace::millis_since(clock);
    let mut history_value = Value::Null;
    if history {
        let clock = Instant::now();
        let mut store = FactsStore::open(&root).map_err(|err| err.to_string())?;
        let report =
            ingest_until_fresh_within(&mut store, &IngestOptions::default(), HISTORY_PREPARE_CAP)
                .map_err(|err| err.to_string())?;
        history_value =
            json!({"fresh": report.fresh, "elapsed_ms": serve_trace::millis_since(clock)});
    }
    let value = json!({
        "root": root.display().to_string(),
        "index_ms": index_ms,
        "graph": {
            "files": graph.files,
            "extracted": graph.extracted,
            "dropped": graph.dropped,
            "stale": graph.stale,
            "dirty": graph.dirty,
            "elapsed_ms": graph_ms,
        },
        "history": history_value,
    });
    let mut text = format!(
        "indexed {} (text index {index_ms} ms; graph {} files, {} extracted, {graph_ms} ms)\n",
        root.display(),
        graph.files,
        graph.extracted
    );
    if graph.stale > 0 {
        text.push_str(&format!(
            "{} file(s) left for the next run: the extraction deadline passed\n",
            graph.stale
        ));
    }
    if history {
        text.push_str("history ingested\n");
    }
    Ok(Done::new(value, text, json))
}

fn daemon(cmd: &DaemonCmd) -> Result<Done, String> {
    match cmd {
        DaemonCmd::Start { path, foreground } => {
            let root = client::discover_root(path)?;
            if *foreground {
                pixel_daemon::run(&root).map_err(|err| err.to_string())?;
                return Ok(Done::new(json!({"stopped": true}), String::new(), false));
            }
            match client::probe(&root) {
                Probe::Current | Probe::Newer => {
                    return Ok(Done::new(
                        json!({"running": true}),
                        format!("daemon already running for {}\n", root.display()),
                        false,
                    ));
                }
                Probe::Absent | Probe::Stale => {}
            }
            client::spawn_daemon(&root).map_err(|err| format!("cannot start the daemon: {err}"))?;
            let deadline = Instant::now() + DAEMON_START_WAIT;
            while Instant::now() < deadline {
                if client::probe(&root) == Probe::Current {
                    return Ok(Done::new(
                        json!({"running": true}),
                        format!("daemon started for {}\n", root.display()),
                        false,
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err("the daemon did not answer within 5 s".to_owned())
        }
        DaemonCmd::Stop { path } => {
            let root = client::discover_root(path)?;
            let response = client::request_if_running(&root, &Request::Shutdown);
            let text = if response.is_some() {
                format!("daemon stopped for {}\n", root.display())
            } else {
                format!("no daemon running for {}\n", root.display())
            };
            Ok(Done::new(
                json!({"stopped": response.is_some()}),
                text,
                false,
            ))
        }
        DaemonCmd::Status { path } => {
            let root = client::discover_root(path)?;
            let probe = client::probe(&root);
            let (running, label) = match probe {
                Probe::Current => (true, "running"),
                Probe::Newer => (true, "running (a newer pixel)"),
                Probe::Stale => (
                    true,
                    "running (another version; the next command replaces it)",
                ),
                Probe::Absent => (false, "not running"),
            };
            Ok(Done::new(
                json!({"running": running, "state": label}),
                format!("daemon {label} for {}\n", root.display()),
                false,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_review_threshold_maps_each_word_to_a_severity() {
        assert_eq!(severity_threshold(FailOn::Blocker), Severity::Critical);
        assert_eq!(severity_threshold(FailOn::Concern), Severity::High);
        assert_eq!(severity_threshold(FailOn::Suggestion), Severity::Medium);
    }

    #[test]
    fn a_file_is_named_from_the_root_whether_given_from_the_cwd_or_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "x").unwrap();
        let absolute = root.join("src/a.rs");
        assert_eq!(root_relative(&root, absolute.to_str().unwrap()), "src/a.rs");
        // Not under the working directory: taken as already root-relative.
        assert_eq!(root_relative(&root, "src/gone.rs"), "src/gone.rs");
        assert_eq!(root_relative(&root, "./src/gone.rs"), "src/gone.rs");
    }

    #[test]
    fn a_path_below_the_root_is_a_prefix_and_the_root_itself_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let git = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(git.success());
        std::fs::create_dir_all(root.join("src")).unwrap();
        assert_eq!(scope_of(&root).unwrap(), (root.clone(), None));
        assert_eq!(
            scope_of(&root.join("src")).unwrap(),
            (root, Some("src".to_owned()))
        );
    }
}
