// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Codex's own app-server, asked rather than impersonated.
//!
//! A readiness probe has to know whether the hooks installed for a workspace
//! are enabled and trusted *by Codex*. The tempting shortcut is to read
//! `config.toml`, recompute the hash of each hook definition and compare it
//! with the stored one — and that shortcut is wrong twice over. The trust
//! rules live in the installed server, not in the file, and a hash written by
//! anything else is a value Codex never agreed to. So this module does what
//! the reference does: it spawns `codex app-server --stdio`, lets *that*
//! process compute the current hash for every hook, and asks the same server
//! to write the state it will accept.
//!
//! Deliberately absent: any write to `config.toml`, any hashing, and any
//! inference of trust from a value already on disk.
//!
//! The wire protocol is two requests over newline-delimited JSON on the
//! child's stdin and stdout. `initialize` (id 1) is answered with a message
//! carrying `id: 1`, which releases the `initialized` notification and the
//! caller's own request (id 2). A message carrying `error` fails the
//! exchange; a line that is not valid JSON is a notification this module does
//! not model and is skipped, exactly as the reference skips it.

use std::{
    io::{self, Read, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    time::{Duration, Instant},
};

use serde_json::{Map, Value, json};

/// The id of the `initialize` request in both the reference and here.
const INITIALIZE_ID: u64 = 1;

/// The id of the request whose answer carries the result the caller wants.
const ANSWER_ID: u64 = 2;

/// The reference caps the hook exchange at 15 s whatever timeout the caller
/// allowed. The server is local: one that has not answered in that long is
/// wedged, not slow.
const HOOKS_EXCHANGE_CAP: Duration = Duration::from_secs(15);

/// The most of one stdout line that is accumulated. The reference keeps an
/// unbounded line buffer; a server that streams a megabyte without a newline
/// must not grow this process's memory with it, so a line past this is
/// dropped whole and the reader resyncs at the next newline. A real
/// `hooks/list` answer is a few kilobytes.
const MAX_LINE_BYTES: usize = 1 << 20;

/// How long the child is given to leave after `SIGTERM`, matching the
/// reference's 500 ms grace before it escalates.
const EXIT_GRACE: Duration = Duration::from_millis(500);

/// How often a teardown checks whether the child has exited.
const EXIT_POLL: Duration = Duration::from_millis(20);

// --- what the server reports -------------------------------------------------

/// One hook as the app-server reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hook {
    pub(crate) enabled: bool,
    /// Codex's own word for the trust state: `trusted`, `managed`, or
    /// something still needing review.
    pub(crate) trust_status: String,
    /// The key `hooks.state` is written under. Absent for a hook the server
    /// does not persist state for.
    pub(crate) key: Option<String>,
    /// The hash of the hook's *current* definition, computed by the installed
    /// server — never a value read back from a config file.
    pub(crate) current_hash: Option<String>,
}

/// One row of a `hooks/list` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HookEntry {
    pub(crate) errors: Vec<Value>,
    pub(crate) warnings: Vec<Value>,
    pub(crate) hooks: Vec<Hook>,
}

/// How `config/batchWrite` merges an edit into the user's config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MergeStrategy {
    /// Merge the new value into what is already there (`hooks.state`).
    Upsert,
    /// Replace whatever is there (`projects."…".trust_level`).
    Replace,
}

impl MergeStrategy {
    /// The spelling the server expects on the wire.
    fn as_str(self) -> &'static str {
        match self {
            Self::Upsert => "upsert",
            Self::Replace => "replace",
        }
    }
}

/// The first of `names` present in `object` *and* a string, as an owned value.
///
/// The app-server spells some fields both ways (`current_hash`/`currentHash`,
/// `trustStatus`/`trust_status`): the reference accepts either for the hash
/// and only camelCase for the status, and accepting both for both costs
/// nothing and cannot invent a value neither spelling carried.
///
/// A present-but-null spelling falls through to the next one, matching the
/// reference's `??`: `find_map` yields only a value that really is a string,
/// so `current_hash: null` beside `currentHash: "h"` resolves to `"h"` rather
/// than to nothing.
fn string_field(object: &Map<String, Value>, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(Value::as_str))
        .map(str::to_string)
}

/// The array at `name`, or an empty one when the field is absent or is not an
/// array.
fn array_at(object: &Map<String, Value>, name: &str) -> Vec<Value> {
    object
        .get(name)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// One hook from a `hooks/list` hook object, or `None` when `enabled` is not
/// a boolean — a hook whose enabled state is unknown is skipped rather than
/// assumed either way.
fn parse_hook(value: &Value) -> Option<Hook> {
    let object = value.as_object()?;
    Some(Hook {
        enabled: object.get("enabled")?.as_bool()?,
        trust_status: string_field(object, &["trustStatus", "trust_status"]).unwrap_or_default(),
        key: string_field(object, &["key"]),
        current_hash: string_field(object, &["current_hash", "currentHash"]),
    })
}

/// Parse one `hooks/list` `result.data` array. Rows that are not objects are
/// skipped rather than guessed at, and so are a row's hooks that are not
/// objects or that carry no boolean `enabled`.
pub(crate) fn parse_hook_entries(data: &Value) -> Vec<HookEntry> {
    let Some(rows) = data.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(Value::as_object)
        .map(|row| HookEntry {
            errors: array_at(row, "errors"),
            warnings: array_at(row, "warnings"),
            hooks: row
                .get("hooks")
                .and_then(Value::as_array)
                .map(|hooks| hooks.iter().filter_map(parse_hook).collect())
                .unwrap_or_default(),
        })
        .collect()
}

/// The `hooks.state` value that trusts every hook still needing review, as
/// `key -> {"trusted_hash": <hash>}`.
///
/// Only a hook that is enabled and not already `trusted` or `managed` is in
/// the map: an entry for a hook Codex already trusts would be redundant. The
/// hash used is always the *current* one the server computed, never a stored
/// value. `None` when such a hook has no `key` or no `current_hash` — the
/// reference refuses the whole write rather than vouch for a hook it cannot
/// name, and so does this. `None` too when two such hooks share a key, which
/// is the reference's own count check: writing one entry for two hooks would
/// trust a hook under a hash the server computed for a different one. An
/// empty map means there is nothing to approve, which is a success the caller
/// can distinguish from the refusal.
pub(crate) fn hook_state(entries: &[HookEntry]) -> Option<Value> {
    let mut state = Map::new();
    let mut counted = 0_usize;
    for hook in entries.iter().flat_map(|entry| entry.hooks.iter()) {
        if !hook.enabled || matches!(hook.trust_status.as_str(), "trusted" | "managed") {
            continue;
        }
        let key = hook.key.as_deref()?;
        let hash = hook.current_hash.as_deref()?;
        counted += 1;
        state.insert(key.to_string(), json!({ "trusted_hash": hash }));
    }
    (state.len() == counted).then_some(Value::Object(state))
}

/// The key path Codex reads a workspace's trust from. The path is quoted the
/// way Codex's TOML spells it, which is what keeps a path with spaces or dots
/// a single key rather than a chain of them.
pub(crate) fn trust_key_path(workspace: &str) -> String {
    format!("projects.\"{workspace}\".trust_level")
}

// --- what this module sends --------------------------------------------------

/// The reference's `initialize`, the first request on every connection.
fn initialize_message() -> Value {
    json!({
        "id": INITIALIZE_ID,
        "method": "initialize",
        "params": { "clientInfo": { "name": "recording-readiness", "version": "1" } },
    })
}

/// The notification that releases the connection to accept the real request.
fn initialized_message() -> Value {
    json!({ "method": "initialized", "params": {} })
}

/// `hooks/list` for one workspace. Codex takes a list of cwds; this probe
/// only ever asks about the workspace it was pointed at.
fn hooks_list_message(workspace: &Path) -> Value {
    json!({
        "id": ANSWER_ID,
        "method": "hooks/list",
        "params": { "cwds": [workspace.to_string_lossy()] },
    })
}

/// One `config/batchWrite` edit, in the shape the reference's own hook
/// approval uses: the edit inside `edits`, and `filePath`/`expectedVersion`
/// explicitly null so the server picks the user config.
fn batch_write_message(key_path: &str, value: &Value, merge: MergeStrategy) -> Value {
    json!({
        "id": ANSWER_ID,
        "method": "config/batchWrite",
        "params": {
            "edits": [{
                "keyPath": key_path,
                "value": value,
                "mergeStrategy": merge.as_str(),
            }],
            "filePath": null,
            "expectedVersion": null,
            "reloadUserConfig": true,
        },
    })
}

// --- the exchange ------------------------------------------------------------

/// What one parsed server message means for the handshake in progress.
#[derive(Debug, PartialEq, Eq)]
enum Reaction {
    /// The server failed the exchange; the string is the report.
    Failed(String),
    /// The answer to `initialize`: send `initialized` and the follow-up.
    Handshake,
    /// The answer to the caller's own request.
    Answered,
    /// A notification, or an answer to a request this connection never sent.
    Other,
}

/// Classify one message. The request id is the only thing that distinguishes
/// the three phases, so it is a pure function with its own tests: the
/// reference has this logic tangled into its read loop.
fn react(message: &Value, answer_id: u64) -> Reaction {
    if let Some(error) = message.get("error") {
        return Reaction::Failed(format!("codex app-server error: {error}"));
    }
    match message.get("id").and_then(Value::as_u64) {
        Some(id) if id == INITIALIZE_ID => Reaction::Handshake,
        Some(id) if id == answer_id => Reaction::Answered,
        _ => Reaction::Other,
    }
}

/// The two directions of a newline-delimited JSON connection, behind a trait
/// so the exchange can be driven from a script in a test without a process.
trait Transport {
    /// The next line, `Ok(None)` when the server closed its output.
    fn read_line(&mut self) -> Result<Option<Vec<u8>>, String>;
    /// Write one message and its terminating newline.
    fn write_message(&mut self, message: &Value) -> Result<(), String>;
}

/// Run the shared handshake: `initialize`, then, on its answer, `initialized`
/// and `follow_up`; the answer to `follow_up` goes to `on_answer`.
///
/// One loop serves both callers because the phases are identical and only the
/// second request differs — the reference copies the loop twice.
fn converse(
    transport: &mut dyn Transport,
    follow_up: &Value,
    on_answer: &mut dyn FnMut(&Value) -> Result<(), String>,
) -> Result<(), String> {
    transport.write_message(&initialize_message())?;
    loop {
        let Some(line) = transport.read_line()? else {
            return Err("codex app-server closed its output before answering".to_string());
        };
        // A line that is not JSON, or JSON that is not a message this
        // connection asked for, is skipped exactly as the reference skips it.
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        match react(&message, ANSWER_ID) {
            Reaction::Failed(why) => return Err(why),
            Reaction::Handshake => {
                transport.write_message(&initialized_message())?;
                transport.write_message(follow_up)?;
            }
            Reaction::Answered => return on_answer(&message),
            Reaction::Other => {}
        }
    }
}

/// The hook rows of a `hooks/list` answer, or `None` when the answer does not
/// carry `result.data` as an array — the reference's own condition for
/// recognising the answer.
fn entries_for(answer: &Value) -> Option<Vec<HookEntry>> {
    answer
        .get("result")?
        .get("data")
        .filter(|data| data.is_array())
        .map(parse_hook_entries)
}

/// The budget for a hooks exchange: the caller's, capped the way the
/// reference caps it.
fn hooks_budget(timeout: Duration) -> Duration {
    timeout.min(HOOKS_EXCHANGE_CAP)
}

// --- the child process -------------------------------------------------------

/// One end of the app-server's pipes as a message stream.
///
/// Generic over the writer so a test can hand it a `Vec<u8>` and read back
/// exactly what was sent; the real session hands it the child's stdin.
struct Wire<W: Write> {
    writer: W,
    lines: Receiver<Vec<u8>>,
    /// The instant the whole exchange must be finished by, fixed when the
    /// wire is created.
    ///
    /// A budget held as a per-read duration is handed to every read, so each
    /// line the server emits starts the wait over: a server that streams
    /// notifications and never answers id 2 would be waited on forever,
    /// although the contract is that the budget bounds the exchange. Holding
    /// the deadline instead makes every read wait only for what is left of it.
    deadline: Instant,
    /// The caller's budget, for the timeout report.
    budget: Duration,
}

impl<W: Write> Wire<W> {
    /// A wire whose exchange must finish within `budget` of this call.
    fn new(writer: W, lines: Receiver<Vec<u8>>, budget: Duration) -> Self {
        Self {
            writer,
            lines,
            deadline: Instant::now() + budget,
            budget,
        }
    }

    /// What a read reports once the exchange is out of time.
    fn timeout_report(&self) -> String {
        let budget = self.budget;
        format!("codex app-server did not answer within {budget:?}")
    }
}

impl<W: Write> Transport for Wire<W> {
    fn read_line(&mut self) -> Result<Option<Vec<u8>>, String> {
        // Only what is left before the deadline, never a fresh budget: a
        // server that keeps saying something must still not outlast it.
        let left = self.deadline.saturating_duration_since(Instant::now());
        match self.lines.recv_timeout(left) {
            Ok(line) => Ok(Some(line)),
            Err(RecvTimeoutError::Timeout) => Err(self.timeout_report()),
            Err(RecvTimeoutError::Disconnected) => Ok(None),
        }
    }

    fn write_message(&mut self, message: &Value) -> Result<(), String> {
        let mut line = serde_json::to_vec(message).map_err(|e| format!("encode a request: {e}"))?;
        line.push(b'\n');
        self.writer
            .write_all(&line)
            .map_err(|e| format!("write to codex app-server: {e}"))
    }
}

/// Split a byte stream into newline-terminated messages and send them on.
///
/// Reads are chunked, so one read is not one message: the partial line is
/// kept across reads, up to [`MAX_LINE_BYTES`], past which the line is
/// dropped and the remainder discarded until the next newline. The trailing
/// buffer at end of stream is sent even without its newline, so an answer
/// the server forgot to terminate is not lost.
fn drain(mut stdout: impl Read, sender: &Sender<Vec<u8>>) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut overflowing = false;
    let mut chunk = [0u8; 8_192];
    loop {
        let read = match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            // An interrupted read is not a closed pipe.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        for &byte in &chunk[..read] {
            if byte == b'\n' {
                if overflowing {
                    // The tail of a line too long to parse: drop it and
                    // resync at the next newline.
                    overflowing = false;
                } else {
                    // A gone receiver means the session was dropped, which
                    // kills the child and closes this stream; the send is
                    // allowed to fail rather than ending the thread early.
                    let _ = sender.send(std::mem::take(&mut buffer));
                }
            } else if overflowing {
                continue;
            } else {
                buffer.push(byte);
                if buffer.len() > MAX_LINE_BYTES {
                    buffer.clear();
                    overflowing = true;
                }
            }
        }
    }
    if !overflowing && !buffer.is_empty() {
        let _ = sender.send(buffer);
    }
}

/// A live `codex app-server --stdio`.
///
/// Its lifetime is the exchange's: every path out of the request helpers
/// drops the session, and dropping it terminates and reaps the child, so no
/// error path can leave a `codex` behind.
struct Session {
    wire: Wire<ChildStdin>,
    child: Child,
}

impl Session {
    /// Spawn the server in `workspace`.
    ///
    /// The child never inherits a banned credential: `ANTHROPIC_API_KEY` is
    /// removed from its environment, as everywhere else in this tree a
    /// subprocess is started.
    #[cfg_attr(test, mutants::skip)] // runs the real `codex`; the wire it exposes is tested through `Wire` and `drain`
    fn spawn(workspace: &Path, budget: Duration) -> Result<Self, String> {
        let mut child = Command::new("codex")
            .args(["app-server", "--stdio"])
            .current_dir(workspace)
            .env_remove("ANTHROPIC_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn `codex app-server`: {e}"))?;
        let writer = child
            .stdin
            .take()
            .ok_or("codex app-server exposed no stdin")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("codex app-server exposed no stdout")?;
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || drain(stdout, &sender));
        Ok(Self {
            wire: Wire::new(writer, lines, budget),
            child,
        })
    }
}

impl Drop for Session {
    /// Terminate, then reap. `SIGTERM` first so a server with state to flush
    /// exits cleanly, `SIGKILL` after [`EXIT_GRACE`] if it has not, and a
    /// blocking `wait` last so the pid is never left a zombie.
    #[cfg_attr(test, mutants::skip)] // signals and reaps a real child; there is no state to assert on
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none()
            && let Ok(pid) = libc::pid_t::try_from(self.child.id())
        {
            // SAFETY: `pid` is this session's child, not yet reaped; SIGTERM
            // is the documented way to ask an app-server to stop.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        let deadline = Instant::now() + EXIT_GRACE;
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(EXIT_POLL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --- the two requests --------------------------------------------------------

/// Ask the app-server for the workspace's hooks, each with the hash the
/// installed app-server computed for the hook's *current* definition.
///
/// The exchange is capped at [`HOOKS_EXCHANGE_CAP`] even when the caller
/// allows more, as the reference caps it.
#[cfg_attr(test, mutants::skip)] // spawns the real `codex`; the exchange it drives is tested through `Wire`
pub(crate) fn hooks_list(workspace: &Path, timeout: Duration) -> Result<Vec<HookEntry>, String> {
    let mut session = Session::spawn(workspace, hooks_budget(timeout))?;
    let follow_up = hooks_list_message(workspace);
    let mut entries = None;
    converse(&mut session.wire, &follow_up, &mut |answer| {
        entries = entries_for(answer);
        Ok(())
    })?;
    entries.ok_or_else(|| "codex app-server answered hooks/list without a data array".to_string())
}

/// Write one `config/batchWrite` edit through Codex's own config RPC.
///
/// The value is written by the server, which is the point: a probe that
/// edited `config.toml` itself would be asserting a trust Codex never
/// granted. `timeout` bounds the exchange; unlike [`hooks_list`] there is no
/// reference cap to honour, because the reference sets no timer here at all.
#[cfg_attr(test, mutants::skip)] // spawns the real `codex`; the exchange it drives is tested through `Wire`
pub(crate) fn config_batch_write(
    workspace: &Path,
    key_path: String,
    value: Value,
    merge_strategy: MergeStrategy,
    timeout: Duration,
) -> Result<(), String> {
    let mut session = Session::spawn(workspace, timeout)?;
    let follow_up = batch_write_message(&key_path, &value, merge_strategy);
    converse(&mut session.wire, &follow_up, &mut |_answer| Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer that always fails, for the encode/write error paths.
    struct Failing;

    impl Write for Failing {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("nope"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A reader that yields one byte per call, so a line arriving in many
    /// reads is really assembled from many reads.
    struct Bytewise {
        bytes: Vec<u8>,
        offset: usize,
    }

    impl Read for Bytewise {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.offset >= self.bytes.len() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.bytes[self.offset];
            self.offset += 1;
            Ok(1)
        }
    }

    /// A reader that yields a scripted sequence of read outcomes, so the
    /// drain loop's error handling can be driven without a real pipe. Each
    /// step is consumed once; the stream is closed after the last one.
    struct Scripted {
        steps: std::vec::IntoIter<io::Result<Vec<u8>>>,
    }

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.steps.next() {
                None => Ok(0),
                Some(Ok(bytes)) => {
                    let n = bytes.len().min(buf.len());
                    buf[..n].copy_from_slice(&bytes[..n]);
                    Ok(n)
                }
                Some(Err(e)) => Err(e),
            }
        }
    }

    /// A `Scripted` reader over `steps`, closed after the last one.
    fn scripted(steps: Vec<io::Result<Vec<u8>>>) -> Scripted {
        Scripted {
            steps: steps.into_iter(),
        }
    }

    /// A wire over a `Vec<u8>` writer and a channel, with nothing spawned.
    fn wire(budget: Duration) -> Wire<Vec<u8>> {
        let (_sender, lines) = mpsc::channel();
        Wire::new(Vec::new(), lines, budget)
    }

    /// Drain `stdout` into `lines`, then drop the sender so a reader sees the
    /// stream closed.
    fn drained(stdout: impl Read) -> Receiver<Vec<u8>> {
        let (sender, lines) = mpsc::channel();
        drain(stdout, &sender);
        drop(sender);
        lines
    }

    /// A transport driven from an explicit script, so `converse` is tested
    /// without a process and without the wire's own clock. A `None` from the
    /// script is the server closing its output.
    struct ScriptedTransport {
        incoming: std::collections::VecDeque<Vec<u8>>,
        written: Vec<u8>,
    }

    impl ScriptedTransport {
        fn new(incoming: &[&str]) -> Self {
            Self {
                incoming: incoming
                    .iter()
                    .map(|line| line.as_bytes().to_vec())
                    .collect(),
                written: Vec::new(),
            }
        }

        /// Everything written, parsed back into messages.
        fn sent(&self) -> Vec<Value> {
            String::from_utf8(self.written.clone())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    impl Transport for ScriptedTransport {
        fn read_line(&mut self) -> Result<Option<Vec<u8>>, String> {
            Ok(self.incoming.pop_front())
        }

        fn write_message(&mut self, message: &Value) -> Result<(), String> {
            let mut line = serde_json::to_vec(message).unwrap();
            line.push(b'\n');
            self.written.extend_from_slice(&line);
            Ok(())
        }
    }

    /// A cap-sized line of `b'x'` followed by a newline and then `b"ok"`.
    fn over_and_after_cap(x_bytes: usize) -> Vec<u8> {
        let mut bytes = vec![b'x'; x_bytes];
        bytes.extend_from_slice(b"\nok\n");
        bytes
    }

    fn hook(enabled: bool, trust: &str, key: Option<&str>, hash: Option<&str>) -> Hook {
        Hook {
            enabled,
            trust_status: trust.to_string(),
            key: key.map(str::to_string),
            current_hash: hash.map(str::to_string),
        }
    }

    fn entry(hooks: Vec<Hook>) -> HookEntry {
        HookEntry {
            errors: Vec::new(),
            warnings: Vec::new(),
            hooks,
        }
    }

    #[test]
    fn parse_hook_entries_accepts_both_spellings_of_every_optional_field() {
        let data = json!([{
            "errors": [{ "message": "e" }],
            "warnings": ["w"],
            "hooks": [
                { "enabled": true, "trustStatus": "untrusted", "key": "a", "current_hash": "h1" },
                { "enabled": false, "trust_status": "trusted", "key": "b", "currentHash": "h2" }
            ]
        }]);
        let entries = parse_hook_entries(&data);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].errors, vec![json!({ "message": "e" })]);
        assert_eq!(entries[0].warnings, vec![json!("w")]);
        assert_eq!(
            entries[0].hooks,
            vec![
                hook(true, "untrusted", Some("a"), Some("h1")),
                hook(false, "trusted", Some("b"), Some("h2")),
            ]
        );
    }

    #[test]
    fn a_null_spelling_falls_through_to_the_other_one() {
        // The reference's `??`: a present-but-null `current_hash` beside a real
        // `currentHash` resolves to the real one rather than to nothing.
        let data = json!([{
            "hooks": [{ "enabled": true, "current_hash": null, "currentHash": "h2" }]
        }]);
        let entries = parse_hook_entries(&data);
        assert_eq!(entries[0].hooks, vec![hook(true, "", None, Some("h2"))]);
    }

    #[test]
    fn parse_hook_entries_skips_rows_and_hooks_it_cannot_read() {
        let data = json!([
            7,
            "row",
            { "hooks": "not an array" },
            { "hooks": [{ "trustStatus": "trusted" }, { "enabled": "yes" }, { "enabled": true }] }
        ]);
        let entries = parse_hook_entries(&data);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], entry(Vec::new()));
        assert_eq!(entries[1].hooks, vec![hook(true, "", None, None)]);
        assert_eq!(parse_hook_entries(&json!({})), Vec::new());
    }

    #[test]
    fn hook_state_names_enabled_untrusted_hooks_by_their_current_hash() {
        let entries = [entry(vec![hook(true, "untrusted", Some("k"), Some("h1"))])];
        assert_eq!(
            hook_state(&entries),
            Some(json!({ "k": { "trusted_hash": "h1" } }))
        );
    }

    #[test]
    fn hook_state_leaves_out_disabled_and_already_trusted_hooks() {
        let entries = [entry(vec![
            hook(true, "trusted", Some("a"), Some("h")),
            hook(true, "managed", Some("b"), Some("h")),
            hook(false, "untrusted", Some("c"), Some("h")),
            hook(true, "untrusted", Some("d"), Some("h")),
        ])];
        assert_eq!(
            hook_state(&entries),
            Some(json!({ "d": { "trusted_hash": "h" } }))
        );
    }

    #[test]
    fn hook_state_refuses_a_hook_it_cannot_name_or_hash() {
        let no_key = [entry(vec![hook(true, "untrusted", None, Some("h"))])];
        assert_eq!(hook_state(&no_key), None);
        let no_hash = [entry(vec![hook(true, "untrusted", Some("k"), None)])];
        assert_eq!(hook_state(&no_hash), None);
    }

    #[test]
    fn hook_state_refuses_two_hooks_that_share_a_key() {
        // One entry cannot carry two hashes: keeping the second would trust
        // the first hook at a hash the server computed for the other.
        let shared = [entry(vec![
            hook(true, "untrusted", Some("k"), Some("h1")),
            hook(true, "untrusted", Some("k"), Some("h2")),
        ])];
        assert_eq!(hook_state(&shared), None);
    }

    #[test]
    fn hook_state_is_an_empty_map_when_nothing_needs_approval() {
        let trusted = [entry(vec![hook(true, "trusted", Some("k"), Some("h"))])];
        assert_eq!(hook_state(&trusted), Some(json!({})));
        assert_eq!(hook_state(&[]), Some(json!({})));
    }

    #[test]
    fn trust_key_path_quotes_the_workspace() {
        assert_eq!(
            trust_key_path("/Users/me/facebook-clone-codex"),
            "projects.\"/Users/me/facebook-clone-codex\".trust_level"
        );
    }

    #[test]
    fn merge_strategy_renders_the_wire_spelling() {
        assert_eq!(MergeStrategy::Upsert.as_str(), "upsert");
        assert_eq!(MergeStrategy::Replace.as_str(), "replace");
    }

    #[test]
    fn initialize_and_initialized_are_the_reference_messages() {
        assert_eq!(
            initialize_message(),
            json!({
                "id": 1,
                "method": "initialize",
                "params": { "clientInfo": { "name": "recording-readiness", "version": "1" } },
            })
        );
        assert_eq!(
            initialized_message(),
            json!({ "method": "initialized", "params": {} })
        );
    }

    #[test]
    fn hooks_list_message_asks_for_the_workspace() {
        assert_eq!(
            hooks_list_message(Path::new("/w/x")),
            json!({ "id": 2, "method": "hooks/list", "params": { "cwds": ["/w/x"] } })
        );
    }

    #[test]
    fn batch_write_message_carries_one_edit_and_the_user_config_flags() {
        let value = json!({ "k": { "trusted_hash": "h" } });
        assert_eq!(
            batch_write_message("hooks.state", &value, MergeStrategy::Upsert),
            json!({
                "id": 2,
                "method": "config/batchWrite",
                "params": {
                    "edits": [{
                        "keyPath": "hooks.state",
                        "value": value.clone(),
                        "mergeStrategy": "upsert",
                    }],
                    "filePath": null,
                    "expectedVersion": null,
                    "reloadUserConfig": true,
                },
            })
        );
        assert_eq!(
            batch_write_message(
                "projects.\"/w\".trust_level",
                &json!("trusted"),
                MergeStrategy::Replace
            )["params"]["edits"][0]["mergeStrategy"],
            json!("replace")
        );
    }

    #[test]
    fn hooks_budget_caps_the_callers_timeout_at_fifteen_seconds() {
        assert_eq!(
            hooks_budget(Duration::from_secs(60)),
            Duration::from_secs(15)
        );
        assert_eq!(hooks_budget(Duration::from_secs(2)), Duration::from_secs(2));
    }

    #[test]
    fn react_classifies_by_id_and_reports_an_error_first() {
        assert_eq!(react(&json!({ "id": 1 }), 2), Reaction::Handshake);
        assert_eq!(react(&json!({ "id": 2 }), 2), Reaction::Answered);
        assert_eq!(react(&json!({ "id": 7 }), 2), Reaction::Other);
        assert_eq!(react(&json!({ "method": "noise" }), 2), Reaction::Other);
        assert_eq!(
            react(&json!({ "id": 2, "error": "boom" }), 2),
            Reaction::Failed("codex app-server error: \"boom\"".to_string())
        );
        assert_eq!(
            react(&json!({ "id": 1, "error": "boom" }), 2),
            Reaction::Failed("codex app-server error: \"boom\"".to_string())
        );
    }

    #[test]
    fn entries_for_reads_only_an_array_under_result_data() {
        assert_eq!(
            entries_for(&json!({ "result": { "data": [] } })),
            Some(Vec::new())
        );
        assert_eq!(entries_for(&json!({ "result": { "data": {} } })), None);
        assert_eq!(entries_for(&json!({ "result": {} })), None);
        assert_eq!(entries_for(&json!({})), None);
    }

    #[test]
    fn converse_runs_the_handshake_then_hands_over_the_answer() {
        let mut transport =
            ScriptedTransport::new(&["{\"id\":1}", "{\"id\":2,\"result\":{\"data\":[]}}"]);
        let follow_up = hooks_list_message(Path::new("/w"));
        let mut answer = None;
        converse(&mut transport, &follow_up, &mut |message| {
            answer = Some(message.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            transport.sent(),
            vec![initialize_message(), initialized_message(), follow_up]
        );
        assert_eq!(answer, Some(json!({ "id": 2, "result": { "data": [] } })));
    }

    #[test]
    fn converse_skips_noise_and_fails_on_an_error_message() {
        let mut transport = ScriptedTransport::new(&[
            "not json",
            "{\"method\":\"noise\"}",
            "{\"id\":1}",
            "{\"id\":2,\"error\":{\"message\":\"nope\"}}",
        ]);
        let error = converse(&mut transport, &json!({ "id": 2 }), &mut |_| Ok(())).unwrap_err();
        assert_eq!(
            error,
            "codex app-server error: {\"message\":\"nope\"}".to_string()
        );
    }

    #[test]
    fn converse_fails_when_the_server_closes_without_answering() {
        let mut transport = ScriptedTransport::new(&[]);
        let error = converse(&mut transport, &json!({ "id": 2 }), &mut |_| Ok(())).unwrap_err();
        assert_eq!(
            error,
            "codex app-server closed its output before answering".to_string()
        );
    }

    #[test]
    fn wire_writes_one_newline_terminated_message() {
        let mut transport = wire(Duration::from_secs(1));
        transport.write_message(&json!({ "id": 1 })).unwrap();
        assert_eq!(
            String::from_utf8(transport.writer).unwrap(),
            "{\"id\":1}\n".to_string()
        );
    }

    #[test]
    fn wire_reports_a_failed_write() {
        let mut transport = Wire::new(Failing, mpsc::channel().1, Duration::from_secs(1));
        assert_eq!(
            transport.write_message(&json!(1)).unwrap_err(),
            "write to codex app-server: nope".to_string()
        );
    }

    #[test]
    fn wire_reads_queued_lines_then_end_of_stream() {
        let (sender, lines) = mpsc::channel();
        sender.send(b"one".to_vec()).unwrap();
        let mut transport = Wire::new(Vec::new(), lines, Duration::from_secs(1));
        assert_eq!(transport.read_line().unwrap(), Some(b"one".to_vec()));
        drop(sender);
        assert_eq!(transport.read_line().unwrap(), None);
    }

    #[test]
    fn wire_waits_for_a_line_that_arrives_inside_its_budget() {
        // The deadline is in the future, not merely spent: a wire that read
        // only what was already queued would call a slow answer a timeout.
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let _ = sender.send(b"late".to_vec());
        });
        let mut transport = Wire::new(Vec::new(), lines, Duration::from_secs(5));
        assert_eq!(transport.read_line().unwrap(), Some(b"late".to_vec()));
    }

    #[test]
    fn a_wire_that_only_hears_noise_still_gives_up_at_its_deadline() {
        // The case a per-line timer never ends on: the server emits a line
        // more often than the budget and never answers, so every read would
        // start the wait over. The deadline is fixed when the wire is made,
        // so the exchange ends `budget` after that however many lines arrived
        // inside it. The reads are driven under the test's own cap, so a wire
        // that never gives up fails the assertion in two seconds instead of
        // hanging the suite for a mutation's whole timeout.
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(5);
            while Instant::now() < until && sender.send(b"{\"method\":\"noise\"}".to_vec()).is_ok()
            {
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let mut transport = Wire::new(Vec::new(), lines, Duration::from_millis(300));
        let stop = Instant::now() + Duration::from_secs(2);
        let mut reported = None;
        while Instant::now() < stop {
            if let Err(error) = transport.read_line() {
                reported = Some(error);
                break;
            }
        }
        assert_eq!(
            reported.as_deref(),
            Some("codex app-server did not answer within 300ms")
        );
    }

    #[test]
    fn wire_reports_a_timeout_with_the_budget() {
        // The sender is held for the test's lifetime, so the wait really is
        // the budget rather than a stream that closed.
        let (_sender, lines) = mpsc::channel();
        let mut transport = Wire::new(Vec::new(), lines, Duration::from_millis(1));
        assert_eq!(
            transport.read_line().unwrap_err(),
            "codex app-server did not answer within 1ms".to_string()
        );
    }

    #[test]
    fn drain_assembles_lines_split_across_reads() {
        let lines = drained(Bytewise {
            bytes: b"{\"a\":1}\n{\"b\":2}\n".to_vec(),
            offset: 0,
        });
        assert_eq!(lines.recv().unwrap(), b"{\"a\":1}".to_vec());
        assert_eq!(lines.recv().unwrap(), b"{\"b\":2}".to_vec());
        assert!(lines.recv().is_err());
    }

    #[test]
    fn drain_keeps_a_final_line_without_a_newline() {
        let lines = drained(io::Cursor::new(b"last".to_vec()));
        assert_eq!(lines.recv().unwrap(), b"last".to_vec());
        assert!(lines.recv().is_err());
    }

    #[test]
    fn drain_drops_a_line_past_the_cap_and_resyncs() {
        let lines = drained(io::Cursor::new(over_and_after_cap(MAX_LINE_BYTES + 1)));
        assert_eq!(lines.recv().unwrap(), b"ok".to_vec());
        assert!(lines.recv().is_err());
    }

    #[test]
    fn drain_keeps_a_line_of_exactly_the_cap() {
        let lines = drained(io::Cursor::new(over_and_after_cap(MAX_LINE_BYTES)));
        let first = lines.recv().unwrap();
        assert_eq!(first.len(), MAX_LINE_BYTES);
        assert_eq!(lines.recv().unwrap(), b"ok".to_vec());
        assert!(lines.recv().is_err());
    }

    #[test]
    fn drain_resumes_after_an_interrupted_read() {
        let lines = drained(scripted(vec![
            Err(io::Error::from(io::ErrorKind::Interrupted)),
            Ok(b"{\"a\":1}\n".to_vec()),
        ]));
        assert_eq!(lines.recv().unwrap(), b"{\"a\":1}".to_vec());
        assert!(lines.recv().is_err());
    }

    #[test]
    fn drain_ends_at_a_read_error_that_is_not_interrupted() {
        let lines = drained(scripted(vec![
            Ok(b"first\n".to_vec()),
            Err(io::Error::from(io::ErrorKind::Other)),
            Ok(b"second\n".to_vec()),
        ]));
        assert_eq!(lines.recv().unwrap(), b"first".to_vec());
        assert!(lines.recv().is_err());
    }
}
