// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `pixel hook <event> --host <host> [--part K/N]`: the one entry point every
//! host's registered hook runs (`specs/hosts.md`, `specs/hooks.md`).
//!
//! It reads the host payload on stdin, answers the event, and prints exactly
//! one JSON envelope on stdout or nothing. It always exits 0: a hook that
//! fails, times out or panics is silence, never a blocked prompt. Edit-time
//! events (`pre-edit`, `post-edit`) belong to the edit hooks, which ship off by
//! default and are not answered here yet.

mod edit;
mod input;
mod parts;
mod pending;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use pixel_brief::parts::{BriefPart, Limits, slice};
use pixel_daemon::Request;
use pixel_io::sidecar;
use pixel_metrics::journal::{self, feedback};
use pixel_setup::{
    Event, Host, Payload, attach_line, context_envelope, footers, last_user_request, normalize,
    relay_response,
};
use serde_json::{Value, json};

use crate::client;

/// Environment markers set by a harness that loads Claude Code's configuration
/// rather than being Claude Code. Devin reads `~/.claude/settings.json` by
/// default and runs the hook commands it finds there unchanged, so `--host
/// claude` on a hook names the install, not the host that invoked it. Such an
/// entry must not act on Claude's behalf: the host's own registration carries
/// the behaviour, and the imported copy double-runs beside it.
const IMPORTED_CLAUDE_CONFIG_MARKERS: &[&str] = &["DEVIN_PROJECT_DIR"];

/// Time kept back from the host's deadline for printing the envelope and
/// releasing the parts waiting on it.
const HOLD_BACK: Duration = Duration::from_millis(1_500);

/// What a part after the first waits beyond the bound part 1 gives itself,
/// so it never prints before part 1 has finished or given up.
const FOLLOW_SLACK: Duration = Duration::from_millis(750);

/// An Antigravity transcript is read up to this many bytes.
const TRANSCRIPT_BYTES: u64 = 8 * 1_024 * 1_024;

/// Runs the hook and exits 0.
pub(crate) fn run(host: Host, event: Event, part: Option<BriefPart>) -> ! {
    input::exit_0_after(|| respond(host, event, part, Instant::now()))
}

/// One event of one host: everything a hook needs to answer it.
struct Hook {
    host: Host,
    root: PathBuf,
    payload: Payload,
    /// The payload as the host sent it: read for a tool result's metrics block
    /// and Antigravity's step numbers, never journaled.
    raw: Value,
    part: Option<BriefPart>,
    started: Instant,
}

fn respond(host: Host, event: Event, part: Option<BriefPart>, started: Instant) {
    if host == Host::Claude && imported_config_host().is_some() {
        return;
    }
    let Some(value) = input::read_hook_payload()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .filter(Value::is_object)
    else {
        return;
    };
    input::test_panic("before-response");
    let payload = normalize(host, &value);
    let Some(root) = hook_root(&payload) else {
        return;
    };
    let hook = Hook {
        host,
        root,
        payload,
        raw: value,
        part,
        started,
    };
    match event {
        Event::SessionStart => hook.session_start(),
        Event::PromptSubmit => hook.prompt_submit(),
        Event::PostTool => hook.post_tool(),
        Event::Stop => hook.stop(),
        Event::PreEdit => hook.pre_edit(),
        Event::PostEdit => hook.post_edit(),
    }
    input::test_panic("after-response");
}

/// The imported-config marker set in this process, if any.
fn imported_config_host() -> Option<&'static str> {
    IMPORTED_CLAUDE_CONFIG_MARKERS
        .iter()
        .copied()
        .find(|marker| std::env::var_os(marker).is_some())
}

/// The repository the event is about: the first directory the host names
/// that is in one, preferring one PIXEL has a graph for (Antigravity reports
/// several workspaces); the process's own directory when it names none.
fn hook_root(payload: &Payload) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = payload.dirs.iter().map(PathBuf::from).collect();
    if dirs.is_empty() {
        dirs.extend(std::env::current_dir().ok());
    }
    let roots: Vec<PathBuf> = dirs
        .iter()
        .filter_map(|dir| client::discover_root(dir).ok())
        .collect();
    roots
        .iter()
        .find(|root| sidecar::dir(root).join(pixel_graph::store::DB).is_file())
        .or(roots.first())
        .cloned()
}

fn emit(output: &Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{output}");
    let _ = stdout.flush();
}

impl Hook {
    fn session(&self) -> Option<&str> {
        self.payload.session.as_deref()
    }

    /// The deadline of the host, less what printing needs, from the moment
    /// this hook started.
    fn bound(&self) -> Duration {
        self.host.deadline().saturating_sub(HOLD_BACK)
    }

    fn limits(&self) -> Limits {
        if self.host.counts_bytes() {
            Limits::BYTES
        } else {
            Limits::CHARS
        }
    }

    /// The journal provenance of this hook: the build, the host, and the
    /// hashed session.
    fn provenance(&self) -> Value {
        let mut provenance = crate::run::provenance("hook");
        provenance.provider = Some(self.host.as_str().to_owned());
        provenance.session = self.session().map(journal::session_key);
        serde_json::to_value(provenance).unwrap_or_default()
    }

    /// Adds `lines` to the journal; best effort, and nothing without a
    /// `.pixel` directory.
    fn journal(&self, derive: impl FnOnce(&str) -> Vec<String>) {
        if let Some(path) = journal::path_for(&self.root) {
            let _ = journal::append_with(&path, journal::MAX_LINES, derive);
        }
    }

    fn session_start(&self) {
        if matches!(self.payload.source.as_deref(), Some("clear" | "compact"))
            && let Some(session) = self.session()
        {
            pixel_brief::sent::reset(&self.root, session);
            pixel_daemon::edit::session::reset(&self.root, session);
        }
        // A start is a nicety: whatever it came to, the hook goes on.
        client::warm(&self.root);
    }

    /// The prompt this event briefs, when it is a turn's first.
    fn prompt(&self) -> Option<String> {
        if !self.payload.first_invocation {
            return None;
        }
        if self.host == Host::Antigravity {
            let transcript = self.payload.transcript.as_deref()?;
            let file = pixel_io::nofollow::open_read(Path::new(transcript)).ok()?;
            let mut text = String::new();
            std::io::Read::read_to_string(
                &mut std::io::Read::take(file, TRANSCRIPT_BYTES),
                &mut text,
            )
            .ok()?;
            return last_user_request(&text);
        }
        self.payload
            .prompt
            .clone()
            .filter(|prompt| !prompt.trim().is_empty())
    }

    fn prompt_submit(&self) {
        if crate::settings::brief_off(Some(&self.root)) {
            return;
        }
        let Some(prompt) = self.prompt() else {
            return;
        };
        let store = parts::cache_path(&self.root, self.session(), &prompt);
        match self.part {
            Some(part) if part.index > 1 => self.later_part(&store, part),
            Some(part) if part.count > 1 => self.first_of_many(&store, &prompt, part),
            _ => self.whole(&prompt),
        }
    }

    /// The brief of `prompt` over `parts` hooks, or `None` when the daemon
    /// did not answer within the bound or the prompt gets none.
    fn brief(&self, prompt: &str, parts: u8) -> Option<Brief> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let request = Request::Brief {
            prompt: prompt.to_owned(),
            session: self.session().unwrap_or_default().to_owned(),
            host: self.host.as_str().to_owned(),
            parts,
            without: Vec::new(),
        };
        let root = self.root.clone();
        std::thread::Builder::new()
            .name("pixel-hook-brief".into())
            .spawn(move || {
                let answer =
                    client::execute(&root, request, false).and_then(client::unwrap_response);
                let _ = sender.send(answer);
            })
            .ok()?;
        let wait = self.bound().saturating_sub(self.started.elapsed());
        let data = receiver.recv_timeout(wait).ok()?.ok()?;
        Some(Brief {
            text: data["text"].as_str().map(str::to_owned),
            paths: data["paths"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            record: data["record"].clone(),
        })
    }

    /// The envelope that delivers `text`, with the 🟩 line when metrics are on.
    fn envelope(&self, text: &str, brief_bytes: usize) -> Value {
        let mut output = context_envelope(self.host, Event::PromptSubmit, text);
        if crate::settings::metrics_on(Some(&self.root)) {
            let ms = self.started.elapsed().as_millis();
            let line = format!("🟩 pixel brief ❀ {} ❀ {ms} ms", human_bytes(brief_bytes));
            attach_line(self.host, &mut output, &line);
        }
        output
    }

    /// The single hook that prints the whole brief. A host with no per-hook
    /// cap on what it injects (pi, OpenCode) gets the room of the default
    /// split in that one message.
    fn whole(&self, prompt: &str) {
        let parts = if self.host.bare_decision() {
            pixel_brief::parts::PARTS_DEFAULT
        } else {
            1
        };
        let brief = self.brief(prompt, parts);
        let text = brief.as_ref().and_then(|brief| brief.text.as_deref());
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            emit(&self.envelope(text, text.len()));
        }
        self.note_prompt(prompt, brief.as_ref(), text.map(str::len));
    }

    /// Part 1 of a split brief: compute it, store it whole for the other
    /// parts (an empty one too, so they do not wait for nothing), print the
    /// first slice, and only then release the others.
    fn first_of_many(&self, store: &Path, prompt: &str, part: BriefPart) {
        parts::forget(store);
        let brief = self.brief(prompt, part.count);
        let text = pad(
            brief.as_ref().and_then(|brief| brief.text.clone()),
            self.limits(),
        );
        let _ = parts::store(
            store,
            text.as_deref().unwrap_or(""),
            std::time::SystemTime::now(),
        );
        let first = text
            .as_deref()
            .map(|text| slice(text, part, self.limits()))
            .filter(|slice| !slice.is_empty());
        if let Some(first) = first {
            emit(&self.envelope(first, first.len()));
            parts::mark_done(store);
        } else {
            parts::mark_done_missing_head(store);
        }
        self.note_prompt(prompt, brief.as_ref(), text.as_deref().map(str::len));
        self.note_part(prompt, part, first.map(str::len), false);
    }

    /// A part after the first: its stored slice, or the head part 1 could not
    /// deliver. A hook that cannot get its slice in time prints nothing.
    fn later_part(&self, store: &Path, part: BriefPart) {
        let bound = self.bound() + FOLLOW_SLACK;
        let waited = bound.saturating_sub(self.started.elapsed());
        let Some(stored) = parts::wait_for_with_status(store, waited, parts::POLL) else {
            return;
        };
        let index = if stored.missing_head {
            part.index.saturating_sub(1)
        } else {
            part.index
        };
        let slice = slice(
            &stored.text,
            BriefPart {
                index,
                count: part.count,
            },
            self.limits(),
        );
        if !slice.is_empty() {
            emit(&self.envelope(slice, slice.len()));
        }
        if let Some(prompt) = self.prompt() {
            self.note_part(
                &prompt,
                part,
                (!slice.is_empty()).then_some(slice.len()),
                false,
            );
        }
    }

    /// Writes the brief decision and the `prompt` line of the journal.
    fn note_prompt(&self, prompt: &str, brief: Option<&Brief>, bytes: Option<usize>) {
        let provenance = self.provenance();
        let ts = pixel_metrics::now_ms().unsigned_abs();
        let digest = journal::sha256_hex(prompt);
        let record = brief
            .map(|brief| &brief.record)
            .filter(|record| record.is_object());
        let injected: &[String] = brief.map_or(&[], |brief| brief.paths.as_slice());
        let lines = |_: &str| {
            let mut lines = Vec::new();
            if let Some(record) = record {
                let mut record = record.clone();
                record["provenance"] = provenance.clone();
                lines.push(record.to_string());
            }
            if let Some(session) = self.session() {
                lines.push(feedback::prompt_line(
                    &provenance,
                    ts,
                    &feedback::Prompt {
                        session,
                        provider: self.host.as_str(),
                        prompt: &digest,
                        decision: record.and_then(|record| record["sha256"].as_str()),
                        gate: record
                            .and_then(|record| record["gate"].as_str())
                            .unwrap_or("none"),
                        kind: record.and_then(|record| record["kind"].as_str()),
                        injected,
                        bytes: bytes.unwrap_or(0),
                    },
                ));
            }
            lines
        };
        self.journal(lines);
    }

    /// Writes the `part` line: what hook part `part` printed.
    fn note_part(&self, prompt: &str, part: BriefPart, printed: Option<usize>, late: bool) {
        let Some(session) = self.session() else {
            return;
        };
        let provenance = self.provenance();
        let line = feedback::part_line(
            &provenance,
            pixel_metrics::now_ms().unsigned_abs(),
            session,
            self.host.as_str(),
            &journal::sha256_hex(prompt),
            (part.index, part.count),
            (printed.unwrap_or(0), late || printed.is_none()),
        );
        self.journal(|_| vec![line]);
    }

    /// The fast path of every tool call: a `tool` line naming the repository
    /// files the call named, and whether it changed any. No daemon.
    fn post_tool(&self) {
        let (Some(session), Some(tool)) = (self.session(), self.payload.tool.as_deref()) else {
            return;
        };
        let cwd = self
            .payload
            .dirs
            .first()
            .map_or_else(|| self.root.clone(), PathBuf::from);
        let targets = feedback::tool_targets(&self.root, &cwd, &self.payload.tool_input);
        let line = feedback::tool_line(
            &self.provenance(),
            pixel_metrics::now_ms().unsigned_abs(),
            session,
            self.host.as_str(),
            tool,
            changes_files(tool, &self.payload.tool_input),
            &targets,
        );
        self.journal(|_| vec![line]);
        self.relay();
    }

    /// The metrics relay: a tool result that carries the finalized 🟩 block of
    /// an `pixel` call has it shown to the user where the host draws one
    /// (`pixel_setup::relay_response`); Antigravity parks it for the end of the
    /// invocation.
    fn relay(&self) {
        if !crate::settings::metrics_on(Some(&self.root)) {
            return;
        }
        let blocks = footers(&self.raw);
        if self.host == Host::Antigravity {
            if let (Some(block), Some(home)) = (blocks.last(), std::env::var_os("HOME")) {
                pending::store(&self.raw, Path::new(&home), block);
            }
        } else if let Some(output) = relay_response(self.host, &blocks) {
            emit(&output);
        }
    }

    /// The end of a turn: the `stop` line and, when its prompt is still open,
    /// the `turn` summary. At the end of an Antigravity invocation, the blocks
    /// its tool calls parked.
    fn stop(&self) {
        if self.host == Host::Antigravity
            && crate::settings::metrics_on(Some(&self.root))
            && let Some(home) = std::env::var_os("HOME")
        {
            let steps: Vec<Value> = pending::take(&self.raw, Path::new(&home))
                .into_iter()
                .map(|line| json!({"ephemeralMessage": line}))
                .collect();
            if !steps.is_empty() {
                emit(&json!({"injectSteps": steps}));
            }
        }
        let Some(session) = self.session() else {
            return;
        };
        let provenance = self.provenance();
        let ts = pixel_metrics::now_ms().unsigned_abs();
        let host = self.host.as_str();
        self.journal(|existing| feedback::stop_lines(&provenance, existing, ts, session, host));
        self.stop_check();
    }
}

/// What the daemon's brief came to.
struct Brief {
    text: Option<String>,
    paths: Vec<String>,
    record: Value,
}

/// Tools that write a file by themselves, under the names the hosts use.
const EDIT_TOOLS: &[&str] = &[
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    "apply_patch",
    "edit",
    "write",
    "str_replace_editor",
    "replace",
    "write_file",
    "edit_file",
    "create_file",
];

/// Whether a tool call changed a file: an edit tool, or a shell command with
/// a redirect or a writing command. Conservative in neither direction: this
/// only labels a journal line.
fn changes_files(tool: &str, input: &Value) -> bool {
    if EDIT_TOOLS.contains(&tool) {
        return true;
    }
    let command = ["command", "cmd"]
        .iter()
        .find_map(|key| input.get(key).and_then(Value::as_str));
    command.is_some_and(shell_writes)
}

/// Whether a shell command line writes a file: an output redirect to
/// something other than `/dev/null` or a file descriptor, or a command that
/// writes in place.
fn shell_writes(command: &str) -> bool {
    const WRITERS: &[&str] = &[
        "tee", "sed -i", "mv", "cp", "rm", "touch", "patch", "install", "dd", "truncate",
    ];
    let redirects = command.match_indices('>').any(|(at, _)| {
        let rest = command[at + 1..]
            .trim_start_matches(['>', '|'])
            .trim_start();
        let before = command[..at].chars().next_back();
        !(rest.starts_with("/dev/null") || rest.starts_with('&') || before == Some('<'))
    });
    redirects
        || command
            .split(|ch: char| "|;&\n".contains(ch))
            .map(str::trim_start)
            .any(|segment| {
                WRITERS.iter().any(|writer| {
                    segment
                        .strip_prefix(writer)
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
                }) || segment.starts_with("git apply")
                    || segment.starts_with("git checkout")
                    || segment.starts_with("git restore")
            })
}

/// `bytes` as the 🟩 line writes sizes.
fn human_bytes(bytes: usize) -> String {
    if bytes < 1_000 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f64 / 1_000.0)
    }
}

/// The text of a split brief; a debug build grows it to the number of parts
/// `PIXEL_BRIEF_TEST_PAD_PARTS` asks for, so the CLI tests reach an oversized
/// brief without a prompt that produces one.
#[cfg(debug_assertions)]
fn pad(text: Option<String>, limits: Limits) -> Option<String> {
    let parts = std::env::var(parts::TEST_PAD_ENV)
        .ok()
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|parts| (1..=pixel_brief::parts::PARTS_MAX).contains(parts));
    match parts {
        Some(parts) => Some(parts::pad_to_parts(
            &text.unwrap_or_default(),
            parts,
            limits,
        )),
        None => text,
    }
}

/// A release build has no such variable.
#[cfg(not(debug_assertions))]
fn pad(text: Option<String>, _limits: Limits) -> Option<String> {
    text
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_edit_tool_changes_a_file_whatever_its_input() {
        for tool in [
            "Edit",
            "Write",
            "MultiEdit",
            "NotebookEdit",
            "apply_patch",
            "write_file",
        ] {
            assert!(changes_files(tool, &Value::Null), "{tool}");
        }
        for tool in ["Read", "Grep", "Glob", "WebFetch", "mcp__pixel__impact"] {
            assert!(!changes_files(tool, &Value::Null), "{tool}");
        }
    }

    #[test]
    fn a_shell_call_changes_a_file_when_it_redirects_or_writes_in_place() {
        for command in [
            "echo hi > out.txt",
            "echo hi >> out.txt",
            "cat a | tee b",
            "sed -i 's/a/b/' src/a.rs",
            "cd src && mv a.rs b.rs",
            "git apply fix.patch",
            "make; rm -rf target",
            "git checkout -- src/a.rs",
        ] {
            assert!(
                changes_files("Bash", &json!({ "command": command })),
                "{command}"
            );
        }
        for command in [
            "cat src/a.rs",
            "cargo test 2>&1 | tail",
            "cargo build 2>/dev/null",
            "git status",
            "ls -la; git diff",
            "cat a | cmp - b",
        ] {
            assert!(
                !changes_files("Bash", &json!({ "command": command })),
                "{command}"
            );
        }
        assert!(changes_files("shell", &json!({ "cmd": "touch x" })));
        assert!(!changes_files("Bash", &json!({})));
    }

    #[test]
    fn sizes_are_written_in_bytes_below_a_thousand_and_in_kilobytes_above() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1_000), "1.0 KB");
        assert_eq!(human_bytes(2_149), "2.1 KB");
    }

    #[test]
    fn the_brief_and_its_printing_fit_every_hosts_deadline() {
        for host in Host::ALL {
            // stdin's one second, the brief's longest window, then printing.
            let longest = input::STDIN_WAIT
                + Duration::from_millis(pixel_brief::chain::WINDOW_MAX_MS)
                + Duration::from_millis(500);
            assert!(longest <= host.deadline(), "{host:?}");
            assert!(host.deadline() - HOLD_BACK + FOLLOW_SLACK < host.deadline());
        }
    }
}
