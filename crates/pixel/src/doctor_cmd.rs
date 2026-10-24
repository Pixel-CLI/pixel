// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel doctor`: the install checks of `pixel-setup` plus what only this binary
//! can answer about a repository: the daemon, and how fresh the text index,
//! the graph and the history are. `--fix` runs each distinct repair once and
//! judges it by re-running the checks it targeted.

use std::path::{Path, PathBuf};

use pixel_graph::store::Store;
use pixel_setup::doctor::{
    CHECKS, CheckStatus, DoctorCheckDetail, DoctorOptions, DoctorReport, Runner, doctor,
    judge_repair, render_catalogue, render_repairs, repair_plan, run_repair, split_home_repairs,
};
use serde_json::{Value, json};

use crate::client::{self, Probe};
use crate::commands::Done;

/// What `pixel doctor` was asked.
pub(crate) struct Request<'a> {
    pub path: &'a Path,
    pub fix: bool,
    pub fail_on: CheckStatus,
    pub only: &'a [String],
    pub skip: &'a [String],
    pub shell: Option<&'a str>,
    pub list: bool,
    pub json: bool,
}

/// The text index's commit, as `.pixel/state.json` records it: the delta's when
/// there is one, else the base shard's.
fn index_commit(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(pixel_io::sidecar::dir(root).join("state.json")).ok()?;
    let state: Value = serde_json::from_str(&text).ok()?;
    state["delta_oid"]
        .as_str()
        .or_else(|| state["base_oid"].as_str())
        .map(str::to_owned)
}

/// HEAD of the work tree at `root`, `None` before the first commit.
fn head(root: &Path) -> Option<String> {
    pixel_graph::tree::discover(root)
        .ok()
        .and_then(|repository| repository.commit().map(str::to_owned))
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// `daemon.health`: whether a daemon of this build answers.
fn daemon_health(root: &Path) -> Result<(CheckStatus, DoctorCheckDetail), String> {
    let (status, summary) = match client::probe(root) {
        Probe::Current => (CheckStatus::Green, "a daemon of this version answers"),
        Probe::Absent => (
            CheckStatus::Yellow,
            "no daemon is running; the next command starts one",
        ),
        Probe::Stale => (
            CheckStatus::Yellow,
            "a daemon of another version answers; the next command replaces it",
        ),
        Probe::Newer => (
            CheckStatus::Yellow,
            "a daemon of a newer protocol answers; it was started by a newer pixel",
        ),
    };
    Ok((
        status,
        DoctorCheckDetail {
            summary: summary.into(),
            detail: None,
        },
    ))
}

/// `index.freshness`: the text index against HEAD.
fn index_freshness(root: &Path) -> Result<(CheckStatus, DoctorCheckDetail), String> {
    let detail = |summary: String| DoctorCheckDetail {
        summary,
        detail: None,
    };
    let Some(indexed) = index_commit(root) else {
        return Ok((CheckStatus::Yellow, detail("no text index yet".into())));
    };
    match head(root) {
        Some(head) if head != indexed => Ok((
            CheckStatus::Yellow,
            detail(format!(
                "the text index is at {} but HEAD is {}",
                short(&indexed),
                short(&head)
            )),
        )),
        _ => Ok((
            CheckStatus::Green,
            detail(format!("the text index is at {}", short(&indexed))),
        )),
    }
}

/// `graph.freshness`: the graph's commit against HEAD.
fn graph_freshness(root: &Path) -> Result<(CheckStatus, DoctorCheckDetail), String> {
    use pixel_model::Graph;
    let detail = |summary: String| DoctorCheckDetail {
        summary,
        detail: None,
    };
    let store = match Store::open(root) {
        Ok(store) => store,
        Err(pixel_graph::store::StoreError::NoIndex) => {
            return Ok((CheckStatus::Yellow, detail("no code graph yet".into())));
        }
        Err(error) => return Err(error.to_string()),
    };
    let provenance = store.provenance().map_err(|error| error.to_string())?;
    let built = provenance.commit.unwrap_or_default();
    match head(root) {
        Some(head) if head != built => Ok((
            CheckStatus::Yellow,
            detail(format!(
                "the graph is at {} but HEAD is {}",
                short(&built),
                short(&head)
            )),
        )),
        _ => Ok((
            CheckStatus::Green,
            detail(format!("the graph is at {}", short(&built))),
        )),
    }
}

/// `history.freshness`: whether a history db was ever built. Not building it
/// is not a finding: history is built by the first history query.
fn history_freshness(root: &Path) -> Result<(CheckStatus, DoctorCheckDetail), String> {
    let built = pixel_io::sidecar::dir(root).join("history.db").is_file();
    Ok((
        CheckStatus::Green,
        DoctorCheckDetail {
            summary: if built {
                "the history db is built".into()
            } else {
                "the history db is built by the first history query".into()
            },
            detail: None,
        },
    ))
}

/// The checks of one repository, on `runner`.
fn live_checks(runner: &mut Runner<'_>, root: &Path) {
    runner.check_status("daemon.health", || daemon_health(root));
    runner.check_status("index.freshness", || index_freshness(root));
    runner.check_status("graph.freshness", || graph_freshness(root));
    runner.check_status("history.freshness", || history_freshness(root));
}

fn report(options: &DoctorOptions, root: Option<&Path>) -> Result<DoctorReport, String> {
    doctor(options, |runner| {
        if let Some(root) = root {
            live_checks(runner, root);
        }
    })
    .map_err(|error| error.to_string())
}

/// Runs `pixel doctor`.
pub(crate) fn run(request: &Request) -> Result<Done, String> {
    if request.list {
        return Ok(Done::new(
            json!(
                CHECKS
                    .iter()
                    .map(|spec| json!({"id": spec.id, "fix": spec.fix}))
                    .collect::<Vec<_>>()
            ),
            render_catalogue(CHECKS),
            request.json,
        ));
    }
    let root: Option<PathBuf> = client::discover_root(request.path).ok();
    let options = DoctorOptions {
        repo_root: root.clone(),
        shell: request.shell.map(str::to_owned),
        only: request.only.to_vec(),
        skip: request.skip.to_vec(),
        ..DoctorOptions::default()
    };
    let mut report = report(&options, root.as_deref())?;
    let mut repairs = String::new();
    let mut outcomes = Vec::new();
    if request.fix {
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let side_build = pixel_setup::config::is_side_build(&exe);
        let (run_now, _left) = split_home_repairs(repair_plan(&report), side_build);
        let runs: Vec<_> = run_now
            .into_iter()
            .map(|repair| {
                let result = run_repair(&exe, &repair);
                (repair, result)
            })
            .collect();
        if !runs.is_empty() {
            report = self::report(&options, root.as_deref())?;
        }
        outcomes = runs
            .into_iter()
            .map(|(repair, result)| judge_repair(repair, result, &report))
            .collect();
        repairs = render_repairs(&outcomes);
    }
    let mut value = serde_json::to_value(&report).map_err(|error| error.to_string())?;
    if request.fix {
        value["repairs"] = serde_json::to_value(&outcomes).map_err(|error| error.to_string())?;
    }
    let mut done = Done::new(value, format!("{repairs}{report}"), request.json);
    if report.fails(request.fail_on) {
        done.exit = 1;
    }
    Ok(done)
}
