// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The edit-time events of `pixel hook` (`specs/hooks.md`): the card before an
//! edit, the stale callers after it, and the check at Stop. All three are off
//! unless `edit_hooks` is on, answer only through a daemon that is already
//! running (starting one in the background when none is, and saying nothing
//! this time), and give up after [`DEADLINE`]: a late card is dropped, never
//! waited for.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use pixel_daemon::Request;
use pixel_metrics::journal::feedback;
use pixel_proto::EditPhase;
use pixel_setup::edit_input::{EditTarget, targets};
use pixel_setup::{Event, attach_line, block_envelope, context_envelope};
use serde_json::{Value, json};

use super::{Hook, emit, human_bytes};
use crate::client;

/// How long an edit hook waits for the daemon.
const DEADLINE: Duration = Duration::from_millis(300);

/// What the daemon said about one file or about Stop.
struct Verdict {
    path: Option<String>,
    result: Value,
}

impl Verdict {
    fn text(&self) -> Option<&str> {
        self.result["text"].as_str().filter(|text| !text.is_empty())
    }

    fn count(&self) -> u64 {
        self.result["count"].as_u64().unwrap_or(0)
    }
}

impl Hook {
    /// Before an edit tool runs.
    pub(super) fn pre_edit(&self) {
        self.edit_event(EditPhase::Before);
    }

    /// After an edit tool ran.
    pub(super) fn post_edit(&self) {
        self.edit_event(EditPhase::After);
    }

    /// The repository files the call writes and what they will hold.
    fn edit_targets(&self, phase: EditPhase) -> Vec<(String, Option<String>)> {
        let Some(tool) = self.payload.tool.as_deref() else {
            return Vec::new();
        };
        let Ok(root) = self.root.canonicalize() else {
            return Vec::new();
        };
        let cwd = self
            .payload
            .dirs
            .first()
            .map_or_else(|| root.clone(), std::path::PathBuf::from);
        let relative = |path: &str| feedback::repository_file(&root, &cwd, path);
        let read = |path: &str| {
            if phase == EditPhase::After {
                return None;
            }
            let bytes = pixel_graph::edit::read(&root, &relative(path)?).ok()??;
            String::from_utf8(bytes).ok()
        };
        targets(tool, &self.payload.tool_input, &read)
            .into_iter()
            .filter_map(|EditTarget { path, proposed }| Some((relative(&path)?, proposed)))
            .collect()
    }

    /// Asks the running daemon, within [`DEADLINE`]. `None`: no daemon (one is
    /// started for the next event), or it did not answer in time.
    fn ask(&self, request: Request) -> Option<Value> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let root = self.root.clone();
        let spawned = std::thread::Builder::new()
            .name("pixel-hook-edit".into())
            .spawn(move || {
                let answer = client::request_if_running(&root, &request);
                let _ = sender.send(answer);
            });
        if spawned.is_err() {
            return None;
        }
        match receiver.recv_timeout(DEADLINE) {
            Ok(Some(response)) => client::unwrap_response(response).ok(),
            Ok(None) => {
                client::warm(&self.root);
                None
            }
            Err(_) => None,
        }
    }

    fn edit_event(&self, phase: EditPhase) {
        if !crate::settings::edit_hooks_on(Some(&self.root)) {
            return;
        }
        let Some(session) = self.session() else {
            return;
        };
        let mut verdicts = Vec::new();
        for (path, proposed) in self.edit_targets(phase) {
            let request = Request::EditCheck {
                path: path.clone(),
                proposed,
                phase,
                session: session.to_owned(),
            };
            let result = self
                .ask(request)
                .unwrap_or_else(|| json!({"text": null, "silent": "no_daemon", "count": 0}));
            verdicts.push(Verdict {
                path: Some(path),
                result,
            });
        }
        let kind = match phase {
            EditPhase::Before => "edit",
            EditPhase::After => "stale",
        };
        let event = match phase {
            EditPhase::Before => Event::PreEdit,
            EditPhase::After => Event::PostEdit,
        };
        self.note_checks(kind, &verdicts);
        let texts: Vec<&str> = verdicts.iter().filter_map(Verdict::text).collect();
        if texts.is_empty() {
            return;
        }
        let text = texts.join("\n\n");
        let count: u64 = verdicts.iter().map(Verdict::count).sum();
        let mut output = context_envelope(self.host, event, &text);
        if crate::settings::metrics_on(Some(&self.root)) {
            let what = match phase {
                EditPhase::Before => format!("{count} callers"),
                EditPhase::After => format!("{count} stale sites"),
            };
            let line = format!(
                "🟩 pixel {} ❀ {what} ❀ {} ❀ {} ms",
                if phase == EditPhase::Before {
                    "edit"
                } else {
                    "stale"
                },
                human_bytes(text.len()),
                self.started.elapsed().as_millis()
            );
            attach_line(self.host, &mut output, &line);
        }
        emit(&output);
    }

    /// The check at Stop: block once while callers of a changed shape are
    /// still untouched. Never under a stop a stop hook already caused.
    pub(super) fn stop_check(&self) {
        if !crate::settings::edit_hooks_on(Some(&self.root)) || self.payload.stop_active {
            return;
        }
        let Some(session) = self.session() else {
            return;
        };
        let result = self
            .ask(Request::StopCheck {
                session: session.to_owned(),
            })
            .unwrap_or_else(|| json!({"text": null, "silent": "no_daemon", "count": 0}));
        let verdict = Verdict { path: None, result };
        self.note_checks("guard", std::slice::from_ref(&verdict));
        let Some(reason) = verdict.text() else {
            return;
        };
        let Some(mut output) = block_envelope(self.host, reason) else {
            return;
        };
        if crate::settings::metrics_on(Some(&self.root)) {
            let line = format!(
                "🟩 pixel stop ❀ {} stale sites ❀ {} ms",
                verdict.count(),
                self.started.elapsed().as_millis()
            );
            attach_line(self.host, &mut output, &line);
        }
        emit(&output);
    }

    /// Writes one journal line per verdict.
    fn note_checks(&self, kind: &str, verdicts: &[Verdict]) {
        let Some(session) = self.session() else {
            return;
        };
        if verdicts.is_empty() {
            return;
        }
        let provenance = self.provenance();
        let ts = pixel_metrics::now_ms().unsigned_abs();
        let lines: Vec<String> = verdicts
            .iter()
            .map(|verdict| {
                feedback::check_line(
                    &provenance,
                    ts,
                    session,
                    self.host.as_str(),
                    kind,
                    verdict
                        .path
                        .as_deref()
                        .map(Path::new)
                        .and_then(Path::to_str),
                    &verdict.result,
                )
            })
            .collect();
        self.journal(|_| lines);
    }
}
