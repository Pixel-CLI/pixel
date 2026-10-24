// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The daemon's side of the prompt brief (`specs/brief.md`): the evidence
//! `pixel-brief` asks for, read from the stores the daemon holds, and the
//! `Brief` op that runs the pipeline over it.

mod common;
pub(crate) mod evidence;
mod graph_reads;
pub mod probes;
pub mod relevance;

use std::sync::Arc;

use pixel_brief::chain::{self, Declined, Request, Session};
use pixel_brief::decision::Record;
use pixel_brief::steps::Steps;
use serde_json::{Value, json};

use crate::service::Shared;

use evidence::BriefEvidence;

/// Hosts that run the brief as a command a person waits on, not as a hook:
/// the shorter command window applies.
const COMMAND_HOSTS: [&str; 1] = ["cli"];

/// What a brief asks of the daemon, from the wire.
pub(crate) struct Ask<'a> {
    pub prompt: &'a str,
    pub session: &'a str,
    pub host: &'a str,
    pub parts: u8,
    /// Recipe steps switched off for this brief.
    pub without: &'a [String],
}

/// The environment variable that switches recipe steps off or back on
/// (`-name,+name`), read by whichever process computes the brief.
pub const STEPS_ENV: &str = "PIXEL_BRIEF_STEPS";

/// The steps that run: every step, then `PIXEL_BRIEF_STEPS`, then the brief's
/// own `without`. A spec or a name that is no step is ignored — the CLI
/// refuses it before it gets here — so a stale variable never costs a brief.
fn steps_for(ask: &Ask) -> Steps {
    let env = std::env::var(STEPS_ENV).unwrap_or_default();
    let steps = Steps::default().apply(&env).unwrap_or_default();
    steps.without(ask.without).unwrap_or(steps)
}

/// The plugin host whose brief probes run on a brief of `root`, unless
/// `PIXEL_BRIEF_PROBES` turns them off.
fn probe_host(root: &std::path::Path) -> Option<pixel_plugin::Host> {
    if !pixel_brief::probes::enabled(
        std::env::var(pixel_brief::probes::PROBES_ENV)
            .ok()
            .as_deref(),
    ) {
        return None;
    }
    Some(pixel_plugin::Host {
        home: std::env::var_os("HOME")?.into(),
        repo_root: Some(root.to_path_buf()),
        path: std::env::var_os("PATH").unwrap_or_default(),
        github_base: None,
    })
}

/// Run one brief over the stores of `shared`. `behind`: the graph lags the
/// working tree, so the evidence may predate the last edit.
///
/// The result is the op's value: the block (`null` when there is none), why
/// there is none (`declined`), the repository files the block names (`paths`)
/// and the journal's `brief` record, for the caller to append.
pub(crate) fn run(shared: &Arc<Shared>, behind: bool, ask: &Ask) -> Value {
    let request = Request {
        prompt: ask.prompt,
        root: &shared.root,
        command: COMMAND_HOSTS.contains(&ask.host),
        parts: ask.parts.max(1),
        window_ms: None,
        session: Some(Session {
            id: (!ask.session.is_empty()).then(|| ask.session.to_owned()),
            transcript: None,
        }),
        french: true,
        steps: steps_for(ask),
        probes: probe_host(&shared.root),
    };
    let evidence = Arc::clone(shared);
    match chain::start(&request, move |_| {
        Box::new(BriefEvidence::new(evidence, behind))
    }) {
        Ok(pending) => {
            let finished = pending.finish_with_record();
            answer(finished.text, None, &finished.paths, &finished.record)
        }
        Err(declined) => answer(
            None,
            Some(declined),
            &[],
            &chain::declined_record(declined, ask.prompt),
        ),
    }
}

fn answer(
    text: Option<String>,
    declined: Option<Declined>,
    paths: &[String],
    record: &Record,
) -> Value {
    json!({
        "text": text,
        "declined": declined.map(Declined::as_str),
        "paths": paths,
        "record": record.to_json(),
    })
}
