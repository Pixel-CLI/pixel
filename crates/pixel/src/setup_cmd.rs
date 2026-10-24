// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel setup`: register Pixel with the agents on this machine, or take it all
//! back out (`specs/setup-migration.md`). At a terminal with no `--agent`,
//! `--print`, `--json` or `--yes`, it asks one question per agent
//! (`pixel_setup::setup::wizard`); anywhere else it sets up every agent found.

use std::io::IsTerminal;
use std::path::Path;

use pixel_setup::install::{
    CheckStatus, InstallOptions, InstallReport, InstallSelection, install_selected,
};
use pixel_setup::receipt::Snapshot;
use pixel_setup::setup::AgentTarget;
use pixel_setup::uninstall::{UninstallOptions, uninstall};

use crate::commands::Done;

/// What `pixel setup` was asked to do.
pub(crate) struct Request<'a> {
    pub agents: &'a [AgentTarget],
    pub json: bool,
    /// Report what would be written and write nothing.
    pub print: bool,
    /// Do not ask which agents to set up.
    pub yes: bool,
    /// Take back everything `pixel setup` wrote.
    pub remove: bool,
    /// With `remove`: report and change nothing.
    pub dry_run: bool,
    /// With `remove`: the binary to remove (default: the running one).
    pub binary_path: Option<&'a Path>,
}

fn icon(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Green => "🟩",
        CheckStatus::Yellow => "🟨",
        CheckStatus::Red => "🟥",
    }
}

/// The steps of `report` as a person reads them.
fn render(report: &InstallReport) -> String {
    let mut text = String::new();
    for step in &report.steps {
        text.push_str(&format!(
            "{} {}: {}\n",
            icon(step.status),
            step.id,
            step.summary
        ));
        if step.status != CheckStatus::Green
            && let Some(detail) = &step.detail
        {
            text.push_str(&format!("   {detail}\n"));
        }
    }
    text.push_str(&format!(
        "{} green, {} yellow, {} red{}\n",
        report.summary.green,
        report.summary.yellow,
        report.summary.red,
        if report.dry_run {
            " (nothing written)"
        } else {
            ""
        }
    ));
    text
}

fn done(report: &InstallReport, as_json: bool) -> Result<Done, String> {
    let value = serde_json::to_value(report).map_err(|error| error.to_string())?;
    let mut done = Done::new(value, render(report), as_json);
    if !report.ok {
        done.exit = 1;
    }
    Ok(done)
}

/// Whether the run asks which agents to set up: a person is at a terminal and
/// nothing on the command line already answered or silenced the question.
fn is_guided(request: &Request, terminal: bool) -> bool {
    terminal && request.agents.is_empty() && !request.print && !request.json && !request.yes
}

/// The agents a person picks at the terminal.
fn ask_agents() -> Result<Vec<AgentTarget>, String> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or("HOME is not set; pass --agent to name the agents")?;
    pixel_setup::setup::wizard::choose(
        &mut std::io::stdin().lock(),
        &mut std::io::stderr(),
        &home,
        pixel_setup::setup::detect().map(AgentTarget::from_detected),
    )
    .map_err(|error| error.to_string())
}

/// Runs `pixel setup` or `pixel setup --remove`.
pub(crate) fn setup(request: &Request) -> Result<Done, String> {
    if request.remove {
        let report = uninstall(&UninstallOptions {
            binary_path: request.binary_path.map(Path::to_path_buf),
            running_binary: std::env::current_exe().ok(),
            dry_run: request.dry_run,
            ..UninstallOptions::default()
        })
        .map_err(|error| error.to_string())?;
        return done(&report, request.json);
    }
    let options = InstallOptions {
        dry_run: request.print,
        ..InstallOptions::default()
    };
    let guided = is_guided(
        request,
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
    );
    let agents = if guided {
        ask_agents()?
    } else {
        request.agents.to_vec()
    };
    if guided && agents.is_empty() {
        return Ok(Done::new(
            serde_json::json!({"ok": true, "agents": []}),
            "no agent chosen; nothing written\n".to_owned(),
            false,
        ));
    }
    let selection = (!agents.is_empty()).then_some(InstallSelection {
        agents,
        hooks: true,
    });
    let snapshot = Snapshot::begin(&options).map_err(|error| error.to_string())?;
    let report =
        install_selected(&options, selection.as_ref()).map_err(|error| error.to_string())?;
    if let Some(snapshot) = snapshot {
        snapshot.finish().map_err(|error| error.to_string())?;
    }
    done(&report, request.json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(agents: &[AgentTarget]) -> Request<'_> {
        Request {
            agents,
            json: false,
            print: false,
            yes: false,
            remove: false,
            dry_run: false,
            binary_path: None,
        }
    }

    #[test]
    fn a_terminal_run_with_nothing_answered_should_be_guided() {
        assert!(is_guided(&request(&[]), true));
    }

    #[test]
    fn anything_that_answers_or_silences_the_question_should_skip_it() {
        assert!(!is_guided(&request(&[]), false));
        assert!(!is_guided(&request(&[AgentTarget::Pi]), true));
        assert!(!is_guided(
            &Request {
                print: true,
                ..request(&[])
            },
            true
        ));
        assert!(!is_guided(
            &Request {
                json: true,
                ..request(&[])
            },
            true
        ));
        assert!(!is_guided(
            &Request {
                yes: true,
                ..request(&[])
            },
            true
        ));
    }
}
