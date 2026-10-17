// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel run-hook metrics` — the PostToolUse relay that replays a pixel
//! invocation's finalized 🟩 metrics line through hosts' visible hook message
//! channels when shell tool results keep stderr out of the transcript. The hook
//! reads the tool-call payload, matches the shell command's pixel invocation
//! to a finalized action record for the effective cwd, and re-emits
//! its metrics line in the provider's hook-response contract. Every miss —
//! malformed payload, no pixel call, suppressed or missing metrics, an
//! ambiguous overlap — is a silent exit 0: the relay is advisory and must
//! never turn a tool call into a failure. Codex additionally requires the
//! record's exact identity header in this tool result and a unique match.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Antigravity's two hook events used by the deferred metrics relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum MetricsEvent {
    PostToolUse,
    PostInvocation,
}

/// The hook payload's host. Named `Provider` to keep the restored code
/// byte-identical to the version the registered command lines carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Provider {
    Claude,
    Codex,
    Devin,
    /// Pi's extension sends a Claude-shaped payload and shows `systemMessage`.
    Pi,
    /// Antigravity's `toolCall` payload is normalized to the shared shape.
    Antigravity,
}

/// Whether this hook invocation is an imported Claude entry running in a
/// foreign host: the `--provider claude` argument names the install, and the
/// importing marker names the real host. Such an entry must not act on
/// Claude's behalf — the host's own protocol carries the behavior, and the
/// imported copy double-runs beside it.
fn imported_claude_entry(provider: Option<Provider>) -> bool {
    matches!(provider, Some(Provider::Claude)) && crate::task_hook::imported_config_host().is_some()
}

/// guard that checks for bare `grep`.
///
/// Examples:
/// - `/usr/bin/grep`  → `grep`
/// - `/bin/cat`       → `cat`
/// - `command grep`   → `grep` (via token[1] promotion in callers)
/// - `builtin grep`   → `grep`
/// - `rtk grep`       → `grep` (rtk is a pass-through wrapper)
///
/// This only strips the path component — callers handle multi-token
/// wrappers (`command`, `builtin`, `rtk`) by removing the wrapper token
/// and calling `normalize_bin` on the next token.
fn normalize_bin(s: &str) -> &str {
    // Strip any directory prefix: /usr/bin/grep → grep
    if let Some(pos) = s.rfind('/') {
        return &s[pos + 1..];
    }
    s
}

/// Shell binaries that accept a `-c`-style flag followed by a script string.
const SHELL_WRAPPERS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];

/// Strip one layer of matching outer quotes.
fn strip_outer_quotes(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
    {
        return &s[1..s.len() - 1];
    }
    s
}

/// Unwrap a `bash -lc "<script>"` invocation down to `<script>`.
///
/// Without this, `bash -lc "grep -rn foo ."` reads as the binary `bash` and
/// the guard never sees the `grep` inside — a wrapper-evasion hole that
/// affected every harness, not just Codex. It matters most for Codex, which
/// routinely wraps every command as `["bash","-lc", ...]`.
fn unwrap_shell_c(cmd: &str) -> String {
    let trimmed = cmd.trim();
    let mut tokens = trimmed.split_whitespace();
    let Some(first) = tokens.next() else {
        return trimmed.to_string();
    };
    if !SHELL_WRAPPERS.contains(&normalize_bin(first)) {
        return trimmed.to_string();
    }
    let mut cursor = first.len();
    for tok in tokens {
        let Some(idx) = trimmed[cursor..].find(tok) else {
            break;
        };
        cursor += idx + tok.len();
        if !tok.starts_with('-') {
            // A non-flag token before any -c: not a `-c` invocation.
            break;
        }
        // `-c`, `-lc`, `-ic`, `-lic` … all mark the next token as the script.
        if tok.contains('c') {
            let rest = strip_outer_quotes(trimmed[cursor..].trim()).trim();
            return rest.to_string();
        }
    }
    trimmed.to_string()
}

/// Extract a shell command from a tool-input value that may be either a
/// plain string or an array of argv tokens, unwrapping any `sh -c` wrapper.
///
/// Claude/Gemini/Antigravity pass `command` as a string. Codex's `shell`
/// and `unified_exec` tools pass an argv array instead — Codex documents the
/// field as "a string or array of strings". Without the array arm, a payload
/// like `{"command":["bash","-lc","grep -rn foo ."]}` reads as an empty
/// string and the guard silently no-ops, letting raw grep through.
fn command_text(v: &Value) -> String {
    match v {
        Value::String(s) => unwrap_shell_c(s),
        Value::Array(items) => {
            let parts: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            if parts.is_empty() {
                return String::new();
            }
            // argv form: ["bash", "-lc", "<script>"] → "<script>"
            if SHELL_WRAPPERS.contains(&normalize_bin(parts[0]))
                && let Some(pos) = parts
                    .iter()
                    .position(|t| t.starts_with('-') && t.contains('c'))
                && let Some(script) = parts.get(pos + 1)
            {
                return unwrap_shell_c(script);
            }
            unwrap_shell_c(&parts.join(" "))
        }
        _ => String::new(),
    }
}
/// Normalize Antigravity's documented toolCall payload at the provider boundary.
fn provider_payload(provider: Provider, mut payload: Value) -> Value {
    if provider == Provider::Antigravity
        && let Some(call) = payload.get("toolCall")
    {
        let tool = call.get("name").cloned().unwrap_or(Value::Null);
        let mut input = call.get("args").cloned().unwrap_or(Value::Null);
        // Antigravity names the shell command `CommandLine`; normalize it at
        // the provider boundary before the shared metrics lookup.
        if input.get("command").is_none()
            && let Some(command) = input.get("CommandLine").cloned()
        {
            input["command"] = command;
        }
        let cwd = input
            .get("Cwd")
            .or_else(|| payload.get("workspacePaths")?.as_array()?.first())
            .cloned()
            .unwrap_or(Value::Null);
        payload["tool_name"] = tool;
        payload["tool_input"] = input;
        payload["cwd"] = cwd;
    }
    payload
}

fn provider_cwd(payload: &Value, input: &Value) -> Option<PathBuf> {
    let base = payload
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
        // Antigravity's PreInvocation payload names only `workspacePaths`.
        .or_else(|| {
            payload
                .get("workspacePaths")?
                .as_array()?
                .first()?
                .as_str()
                .map(PathBuf::from)
        })
        .or_else(|| std::env::current_dir().ok())?;
    Some(
        input
            .get("workdir")
            .or_else(|| input.get("cwd"))
            .or_else(|| input.get("Cwd"))
            .and_then(Value::as_str)
            .map_or_else(|| base.clone(), |path| base.join(path)),
    )
}
/// PostToolUse advisory: surface the blast-radius note to the model without a
/// permission decision (the edit already happened).
fn post_tool_use_advisory(note: &str) -> serde_json::Value {
    serde_json::json!({
        "systemMessage": note,
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": note
        }
    })
}

/// Shell-shaped tools whose `command`/`cmd` input can hold a `pixel` call.
const METRICS_SHELL_TOOLS: &[&str] = &[
    "Bash",
    "bash",
    "Shell",
    "shell",
    "local_shell",
    "unified_exec",
    "exec",
    "run_command",
    "container.exec",
    "functions.shell",
];

/// `pixel run-hook metrics` — relay finalized metrics lines through provider
/// hook contracts without turning a tool call into a failure.
#[cfg_attr(test, mutants::skip)] // stdin + process::exit boundary; every decision lives in `metrics_hook_line`
pub fn run_metrics_hook(provider: Option<Provider>, event: MetricsEvent) -> ! {
    crate::hook_input::exit_0_after(|| relay_metrics(provider, event))
}

/// The body of [`run_metrics_hook`]: at most one response on stdout.
#[cfg_attr(test, mutants::skip)] // stdin boundary; every decision lives in `metrics_hook_line`
fn relay_metrics(provider: Option<Provider>, event: MetricsEvent) {
    // An imported Claude entry running beside Devin's own relay would emit
    // the Claude contract (`systemMessage`) into a host that never asked for
    // it and double the native `--provider devin` relay's output.
    if imported_claude_entry(provider) {
        return;
    }
    let Some(input) = crate::hook_input::read_hook_payload() else {
        return;
    };
    crate::hook_input::test_panic("before-response");
    let Ok(payload) = serde_json::from_str::<Value>(&input) else {
        return;
    };
    if let Some(response) = metrics_hook_event_response(provider, event, &payload) {
        print!("{response}");
    }
}

fn metrics_hook_event_response(
    provider: Option<Provider>,
    event: MetricsEvent,
    payload: &Value,
) -> Option<Value> {
    if provider == Some(Provider::Antigravity) {
        return antigravity_metrics_hook_response(event, payload);
    }
    if event != MetricsEvent::PostToolUse {
        return None;
    }
    let payload =
        provider.map_or_else(|| payload.clone(), |p| provider_payload(p, payload.clone()));
    metrics_hook_response(provider, &payload)
}

fn antigravity_metrics_hook_response(event: MetricsEvent, payload: &Value) -> Option<Value> {
    match event {
        MetricsEvent::PostToolUse => {
            let payload = provider_payload(Provider::Antigravity, payload.clone());
            if let Some(line) = metrics_record_line(&payload) {
                let _ = store_antigravity_pending_metric(&payload, &line);
            }
            Some(serde_json::json!({}))
        }
        MetricsEvent::PostInvocation => {
            let lines = take_antigravity_pending_metrics(payload)?;
            (!lines.is_empty()).then(|| {
                serde_json::json!({
                    "injectSteps": lines.into_iter().map(|line| {
                        serde_json::json!({"ephemeralMessage": line})
                    }).collect::<Vec<_>>()
                })
            })
        }
    }
}

const ANTIGRAVITY_PENDING_METRICS_DIR: &str = ".pixel-pending-metrics";

fn antigravity_pending_metrics_dir(payload: &Value) -> Option<(PathBuf, &str)> {
    let conversation_id = payload.get("conversationId")?.as_str()?;
    if conversation_id.is_empty() {
        return None;
    }
    let artifact_path = payload.get("artifactDirectoryPath")?.as_str()?;
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let gemini_home = home.canonicalize().ok()?.join(".gemini");
    let artifact_path = artifact_path.strip_prefix("~/").map_or_else(
        || PathBuf::from(artifact_path),
        |relative| home.join(relative),
    );
    if !artifact_path.is_absolute() {
        return None;
    }
    let artifact_path = artifact_path.canonicalize().ok()?;
    if !artifact_path.starts_with(&gemini_home) {
        return None;
    }
    Some((
        artifact_path.join(ANTIGRAVITY_PENDING_METRICS_DIR),
        conversation_id,
    ))
}

fn store_antigravity_pending_metric(payload: &Value, line: &str) -> std::io::Result<()> {
    let Some(step_idx) = payload.get("stepIdx").and_then(Value::as_u64) else {
        return Ok(());
    };
    let Some((directory, conversation_id)) = antigravity_pending_metrics_dir(payload) else {
        return Ok(());
    };
    std::fs::create_dir_all(&directory)?;
    let pending = serde_json::json!({
        "conversationId": conversation_id,
        "stepIdx": step_idx,
        "line": line
    });
    std::fs::write(
        directory.join(format!("{step_idx}.json")),
        serde_json::to_vec(&pending)?,
    )
}

fn take_antigravity_pending_metrics(payload: &Value) -> Option<Vec<String>> {
    let conversation_id = payload.get("conversationId")?.as_str()?;
    let initial_num_steps = payload.get("initialNumSteps")?.as_u64()?;
    let (directory, _) = antigravity_pending_metrics_dir(payload)?;
    let entries = std::fs::read_dir(&directory).ok()?;
    let mut files = entries
        .filter_map(std::result::Result::ok)
        .collect::<Vec<_>>();
    files.sort_by_key(std::fs::DirEntry::file_name);
    let mut pending = Vec::new();
    for entry in files {
        let path = entry.path();
        let Some(step_idx) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
            .and_then(|name| name.parse::<u64>().ok())
        else {
            continue;
        };
        let record = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let Some(record) = record else {
            let _ = std::fs::remove_file(path);
            continue;
        };
        if record.get("conversationId").and_then(Value::as_str) != Some(conversation_id) {
            let _ = std::fs::remove_file(path);
            continue;
        }
        if step_idx < initial_num_steps {
            let _ = std::fs::remove_file(path);
            continue;
        }
        if record.get("stepIdx").and_then(Value::as_u64) != Some(step_idx) {
            let _ = std::fs::remove_file(path);
            continue;
        }
        if let Some(line) = record.get("line").and_then(Value::as_str) {
            pending.push((step_idx, line.to_owned(), path));
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
    pending.sort_by_key(|(step_idx, _, _)| *step_idx);
    let lines = pending
        .into_iter()
        .filter_map(|(_, line, path)| std::fs::remove_file(path).ok().map(|()| line))
        .collect::<Vec<_>>();
    let _ = std::fs::remove_dir(&directory);
    Some(lines)
}

/// Whether the host already put the invocation's 🟩 box in the tool result.
fn result_carries_metrics_box(payload: &Value) -> bool {
    payload
        .get("tool_response")
        .is_some_and(|r| r.to_string().contains("🟩 pixel"))
}

/// The provider-shaped envelope for a replayed metrics line: Codex uses its
/// documented `systemMessage` relay.
fn metrics_relay_response(provider: Option<Provider>, line: &str) -> Value {
    match provider {
        // Codex's shell result can carry stderr without surfacing the footer
        // in the transcript, so PostToolUse uses the documented visible field.
        // Pi's extension shows `systemMessage` to the user and never to the
        // model, which already reads the box in the bash result.
        Some(Provider::Codex | Provider::Pi) => serde_json::json!({"systemMessage": line}),
        _ => post_tool_use_advisory(line),
    }
}

/// The hook's answer. Outside Codex, a box missing from the result is replayed
/// through the provider's advisory channel. A box already in the result is a
/// duplicate for the model, so it is dropped, except for Claude Code: its
/// Bash result carries hidden stderr. Codex requires a unique matching header
/// from this result before using its user-visible `systemMessage`. Pi
/// relays like Codex: its extension shows the `systemMessage` to the user
/// whether or not the bash result holds the box.
fn metrics_hook_response(provider: Option<Provider>, payload: &Value) -> Option<Value> {
    if provider == Some(Provider::Codex) {
        // The log has no shared Codex tool-use ID. Only a footer in this
        // result can bind a finalized record to this particular call.
        let response = payload.get("tool_response")?;
        return metrics_record_line_with_result(payload, Some(response))
            .map(|line| serde_json::json!({"systemMessage": line}));
    }
    if !result_carries_metrics_box(payload) {
        return metrics_hook_line(payload).map(|line| metrics_relay_response(provider, &line));
    }
    matches!(
        provider,
        Some(Provider::Claude | Provider::Codex | Provider::Pi)
    )
    .then(|| metrics_record_line(payload))
    .flatten()
    .map(|line| serde_json::json!({"systemMessage": line}))
}

/// Resolve the payload to the metrics line of the invocation it describes,
/// or `None` when there is nothing to relay.
fn metrics_hook_line(payload: &Value) -> Option<String> {
    // A host that already put stderr in the tool result made the relay a
    // duplicate — leave the line where it is.
    if result_carries_metrics_box(payload) {
        return None;
    }
    metrics_record_line(payload)
}

/// The finalized record's line for the invocation, whether or not the tool
/// result already shows it.
fn metrics_record_line(payload: &Value) -> Option<String> {
    metrics_record_line_with_result(payload, None)
}

/// Whether a string leaf of the tool result contains the exact footer header.
fn result_has_metrics_header(result: &Value, header: &str) -> bool {
    match result {
        Value::String(text) => text.lines().any(|line| line.trim() == header),
        Value::Array(values) => values
            .iter()
            .any(|value| result_has_metrics_header(value, header)),
        Value::Object(values) => values
            .values()
            .any(|value| result_has_metrics_header(value, header)),
        _ => false,
    }
}

/// Match by command/cwd, optionally requiring a unique header from this result.
fn metrics_record_line_with_result(payload: &Value, response: Option<&Value>) -> Option<String> {
    let tool = payload.get("tool_name")?.as_str()?;
    if !METRICS_SHELL_TOOLS.contains(&tool) {
        return None;
    }
    let tool_input = payload.get("tool_input")?;
    let command = tool_command_text(tool_input)?;
    // Resolve the working directory the way enforcement does, so an empty
    // `cwd` falls back to the workspace paths and the workspace's `.pixel` is
    // still found for the metrics lookup.
    let payload_cwd = provider_cwd(payload, tool_input).unwrap_or_default();
    // A leading `cd dir &&` selects where the invocation actually ran:
    // the record's cwd is that effective directory, not the tool cwd.
    let (effective_cwd, effective_cmd) = strip_cd_prefix(&command, &payload_cwd);
    let tool_cwd = canonical(&effective_cwd);
    let invocation = pixel_invocation(effective_cmd)?;
    // Suppress exactly when the invocation suppressed its own footer: an
    // effective PIXEL_METRICS of 0 — the inherited env, overridden in
    // command order by inline `PIXEL_METRICS=` prefixes on the pixel call.
    if invocation
        .metrics_env
        .or_else(|| std::env::var("PIXEL_METRICS").ok())
        .as_deref()
        == Some("0")
    {
        return None;
    }
    if invocation.args.contains("--metrics=off") || invocation.args.contains("--metrics off") {
        return None;
    }
    let root = find_up(&tool_cwd, ".pixel")?;
    // The persistent opt-out applies to the relay exactly as to stderr.
    if !crate::config_cmd::metrics_enabled(Some(&root)) {
        return None;
    }
    let log = pixel_actionlog::ActionLog::path_for_root(&root);
    let events = pixel_actionlog::tail(&log, 100).ok()?;
    // Some providers pass a stable tool-use ID, but the action log currently
    // records its own invocation ID only. Without a shared ID, the newest
    // same-command record is not reliable when identical calls overlap. Use
    // the recorded start/duration interval to detect that ambiguity and omit
    // the relay rather than show another invocation's metrics.
    let matching: Vec<_> = events
        .iter()
        .rev()
        .filter(|event| {
            event.args == invocation.args
                && canonical(Path::new(&event.cwd)) == tool_cwd
                && event.metrics.is_some()
                && response.is_none_or(|result| {
                    event
                        .invocation_id
                        .as_deref()
                        .is_some_and(|id| !id.is_empty())
                        && pixel_actionlog::format_metrics_line(event).is_some_and(|line| {
                            result_has_metrics_header(
                                result,
                                line.lines().next().unwrap_or_default(),
                            )
                        })
                })
        })
        .collect();
    // A shortened ID can collide, and a compound result can contain more
    // than one footer. Neither identifies a unique invocation.
    if response.is_some() && matching.len() != 1 {
        return None;
    }
    let latest = matching.first()?;
    let latest_start = latest.ts_ms;
    let latest_end =
        latest_start.saturating_add(i64::try_from(latest.duration_ms).unwrap_or(i64::MAX));
    let overlaps_another = matching.iter().skip(1).any(|event| {
        let other_start = event.ts_ms;
        let other_end =
            other_start.saturating_add(i64::try_from(event.duration_ms).unwrap_or(i64::MAX));
        latest_start < other_end && other_start < latest_end
    });
    if overlaps_another {
        return None;
    }
    pixel_actionlog::format_metrics_line(latest)
}

/// The command string a shell tool was asked to run: a plain string, or the
/// argv array `local_shell`-shaped tools send, joined back to one line.
fn tool_command_text(tool_input: &Value) -> Option<String> {
    let value = tool_input
        .get("command")
        .or_else(|| tool_input.get("cmd"))?;
    match value {
        // `command_text` unwraps `bash -c` with the argv boundary intact —
        // joining first would fold $0 positionals into the script.
        Value::String(_) | Value::Array(_) => Some(command_text(value)),
        _ => None,
    }
}

/// What a compound command actually runs under `pixel`: the argument tail
/// plus the `PIXEL_METRICS=` value its env prefixes leave behind.
#[derive(Debug, PartialEq)]
struct PixelInvocation {
    args: String,
    metrics_env: Option<String>,
}

/// Split a command line at the separators a shell would honor — `&`, `;`,
/// `|` and newlines — but only outside quotes: `pixel search 'a|b'` is one
/// segment whose `|` belongs to the argument, not a pipeline.
fn shell_segments(command: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut quote: Option<char> = None;
    let mut start = 0;
    for (idx, c) in command.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if matches!(c, '&' | ';' | '|' | '\n') => {
                out.push(&command[start..idx]);
                start = idx + c.len_utf8();
            }
            None => {}
        }
    }
    out.push(&command[start..]);
    out
}

/// The first `pixel` invocation in a compound command (`cd x && pixel
/// impact y | head`, env prefixes, `export`, `bash -lc` wrappers).
fn pixel_invocation(command: &str) -> Option<PixelInvocation> {
    pixel_invocation_in(command, None)
}

fn pixel_invocation_in(command: &str, env: Option<String>) -> Option<PixelInvocation> {
    let mut exported = env;
    for segment in shell_segments(command) {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        // `export K=v; pixel f` reaches the pixel call ambiently — collect
        // it the way the shell would.
        if tokens
            .first()
            .is_some_and(|t| t.trim_matches(['\'', '"']) == "export")
        {
            for tok in tokens.iter().skip(1) {
                if let Some(v) = tok.trim_matches(['\'', '"']).strip_prefix("PIXEL_METRICS=") {
                    // `K='v'` leaves the opening quote on the value once the
                    // token's trailing quote is trimmed — `PIXEL_METRICS='0'`
                    // must compare equal to `PIXEL_METRICS=0`.
                    exported = Some(v.trim_start_matches(['\'', '"']).to_string());
                }
            }
            continue;
        }
        if let Some(inv) = pixel_invocation_in_tokens(&tokens, exported.clone()) {
            return Some(inv);
        }
    }
    None
}

fn pixel_invocation_in_tokens(tokens: &[&str], env: Option<String>) -> Option<PixelInvocation> {
    // Quote characters arrive attached to tokens (`bash -lc 'cd x && …'`);
    // strip them so the binary and its args match the recorded invocation.
    let tokens: Vec<&str> = tokens.iter().map(|t| t.trim_matches(['\'', '"'])).collect();
    // Skip env assignments, flags and launcher words to reach the binary.
    // A `for` keeps every mutation of the index update a wrong value, never
    // an unbounded loop.
    let mut i = 0;
    let mut metrics_env = env;
    for (j, tok) in tokens.iter().enumerate() {
        let prefix = (tok.contains('=') && !tok.starts_with('-'))
            || tok.starts_with('-')
            || matches!(*tok, "env" | "sudo" | "command" | "time" | "xargs" | "rtk");
        if !prefix {
            break;
        }
        if let Some(v) = tok.strip_prefix("PIXEL_METRICS=") {
            metrics_env = Some(v.trim_start_matches(['\'', '"']).to_string());
        }
        i = j + 1;
    }
    let bin = tokens.get(i)?;
    let base = bin.rsplit('/').next().unwrap_or(bin);
    if pixel_setup::config::PIXEL_EXECUTABLES.contains(&base) {
        return Some(PixelInvocation {
            args: tokens[i + 1..].join(" "),
            metrics_env,
        });
    }
    if matches!(base, "bash" | "sh" | "zsh" | "dash" | "fish") {
        // `bash -lc "…"`: the script follows the first flag carrying `c`,
        // and prefix assignments pass into it as its inherited env.
        for (j, tok) in tokens.iter().enumerate().skip(i + 1) {
            if !tok.starts_with('-') {
                break;
            }
            if tok.trim_start_matches('-').contains('c') {
                return bash_script_invocation(&tokens, j, metrics_env);
            }
        }
    }
    None
}

/// Re-parse everything after a shell's `-c` flag as its own command line:
/// for an argv array joined back into one line the script is every
/// remaining token, not just the next one.
#[cfg_attr(test, mutants::skip)] // tokens[j] is `-`-prefixed by the caller's guard, so `j` and `j+1` converge in the recursive flag-skip — the slice bound is unobservable
fn bash_script_invocation(
    tokens: &[&str],
    j: usize,
    env: Option<String>,
) -> Option<PixelInvocation> {
    pixel_invocation_in(&tokens[j + 1..].join(" "), env)
}
/// Strip a leading `cd <dir> && ` prefix from a command, returning the
/// effective cwd (original cwd + cd target) and the remaining body. If
/// there's no cd prefix, returns (original_cwd, original_cmd).
fn strip_cd_prefix<'a>(cmd: &'a str, cwd: &Path) -> (PathBuf, &'a str) {
    let trimmed = cmd.trim();
    if !trimmed.starts_with("cd ") {
        return (cwd.to_path_buf(), cmd);
    }
    // Find the first unquoted `&&` after the cd.
    let rest_after_cd = &trimmed[3..];
    let amp_idx = match find_unquoted_double_amp(rest_after_cd) {
        Some(i) => i,
        None => return (cwd.to_path_buf(), cmd),
    };
    let dir_str = rest_after_cd[..amp_idx].trim();
    // Strip quotes from the directory.
    let dir_str = dir_str.trim_matches(|c| c == '\'' || c == '"').trim();
    let new_cwd = if dir_str.starts_with('/') {
        PathBuf::from(dir_str)
    } else {
        cwd.join(dir_str)
    };
    let body = rest_after_cd[amp_idx + 2..].trim_start();
    // Return the body with a reference into the original string.
    // Find where body starts in the original cmd.
    let body_offset = cmd.len() - body.len();
    let body_ref = &cmd[body_offset..];
    (new_cwd, body_ref)
}

/// Find the byte index of the first `&&` outside single or double quotes.
fn find_unquoted_double_amp(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut prev_amp_at: Option<usize> = None;
    for (idx, c) in s.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '&' => {
                if let Some(first) = prev_amp_at {
                    return Some(first);
                }
                prev_amp_at = Some(idx);
                continue;
            }
            None => {}
        }
        prev_amp_at = None;
    }
    None
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Walk upward from `start` (or its parent, if `start` is a file) looking
/// for `rel` (a file or directory). Returns the directory containing it.
fn find_up(start: &Path, rel: impl AsRef<Path>) -> Option<PathBuf> {
    let rel = rel.as_ref();
    let mut dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        if dir.join(rel).exists() {
            return Some(dir);
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_unquoted_double_amp_reports_the_first_separator_outside_quotes() {
        assert_eq!(find_unquoted_double_amp(""), None);
        assert_eq!(find_unquoted_double_amp("cargo test"), None);
        assert_eq!(
            find_unquoted_double_amp("a & b"),
            None,
            "a lone `&` is not a separator"
        );
        assert_eq!(find_unquoted_double_amp("&& b"), Some(0));
        assert_eq!(find_unquoted_double_amp("a && b"), Some(2));
        assert_eq!(
            find_unquoted_double_amp("a && b && c"),
            Some(2),
            "first, not last"
        );
        assert_eq!(find_unquoted_double_amp("'a && b' && c"), Some(9));
        assert_eq!(find_unquoted_double_amp("\"x&&y\" && z"), Some(7));
        assert_eq!(find_unquoted_double_amp("'unterminated && quote"), None);
        assert_eq!(
            find_unquoted_double_amp("a &x& b"),
            None,
            "the ampersands must be adjacent"
        );
        // Byte index, not char index: `é` is two bytes.
        assert_eq!(find_unquoted_double_amp("é && x"), Some(3));
        assert_eq!(&"é && x"[3..5], "&&");
    }

    #[test]
    fn strip_cd_prefix_uses_the_unquoted_separator() {
        let cwd = Path::new("/repo");
        let (dir, body) = strip_cd_prefix("cd /tmp/x && cargo test", cwd);
        assert_eq!((dir.as_path(), body), (Path::new("/tmp/x"), "cargo test"));
        let (dir, body) = strip_cd_prefix("cd 'a && b' && ls", cwd);
        assert_eq!((dir.as_path(), body), (Path::new("/repo/a && b"), "ls"));
        let (dir, body) = strip_cd_prefix("cd sub", cwd);
        assert_eq!((dir.as_path(), body), (Path::new("/repo"), "cd sub"));
    }

    // ---- `pixel run-hook metrics` ----------------------------------------

    // HOME is process-global and the metrics layer consults
    // `~/.pixel/config.yaml`; serialize tests that repoint it through the
    // crate-wide `ENV_LOCK`.
    struct MetricsFixture {
        root: PathBuf,
        saved_home: Option<std::ffi::OsString>,
        saved_metrics: Option<std::ffi::OsString>,
    }

    impl MetricsFixture {
        /// A repo root holding `.pixel/` and one finalized `impact` action
        /// record, plus a fake HOME so no real global config leaks in and
        /// no inherited `PIXEL_METRICS` overrides the effective-env tests.
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pixel-metrics-hook-{}-{}-{}",
                name,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let root = dir.join("repo");
            std::fs::create_dir_all(root.join(".pixel")).unwrap();
            let root = canonical(&root);
            let home = dir.join("home");
            std::fs::create_dir_all(&home).unwrap();
            let saved_home = std::env::var_os("HOME");
            let saved_metrics = std::env::var_os("PIXEL_METRICS");
            // SAFETY: under crate::ENV_LOCK in tests only.
            unsafe {
                std::env::set_var("HOME", &home);
                std::env::remove_var("PIXEL_METRICS");
            }
            let fixture = Self {
                root,
                saved_home,
                saved_metrics,
            };
            fixture.record("impact", "impact src/login.rs");
            fixture
        }

        /// Append a finalized action record for `args` run in `cwd`.
        fn record(&self, command: &str, args: &str) {
            self.write_event(command, args, &self.root, None);
        }

        fn record_at(&self, command: &str, args: &str, cwd: &Path) {
            self.write_event(command, args, cwd, None);
        }

        fn record_interval(
            &self,
            command: &str,
            args: &str,
            invocation_id: &str,
            start_ms: i64,
            duration_ms: u64,
        ) {
            let mut event = pixel_actionlog::ActionEvent::new(command, args);
            event.cwd = self.root.display().to_string();
            event.invocation_id = Some(invocation_id.to_string());
            event.ts_ms = start_ms;
            event.duration_ms = duration_ms;
            event.metrics = Some(
                pixel_actionlog::OperationMetrics::new(
                    std::time::Duration::from_millis(4),
                    120,
                    None,
                )
                .with_comparison_gap(pixel_actionlog::ComparisonGap::NoPolicy),
            );
            self.append_event(event);
        }

        fn write_event(&self, command: &str, args: &str, cwd: &Path, id: Option<&str>) {
            let mut event = pixel_actionlog::ActionEvent::new(command, args);
            event.cwd = cwd.display().to_string();
            if let Some(id) = id {
                event.invocation_id = Some(id.to_string());
            }
            event.metrics = Some(
                pixel_actionlog::OperationMetrics::new(
                    std::time::Duration::from_millis(4),
                    120,
                    None,
                )
                .with_comparison_gap(pixel_actionlog::ComparisonGap::NoPolicy),
            );
            self.append_event(event);
        }

        fn append_event(&self, event: pixel_actionlog::ActionEvent) {
            use std::io::Write;
            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.root.join(".pixel/actions.jsonl"))
                .unwrap();
            writeln!(log, "{}", serde_json::to_string(&event).unwrap()).unwrap();
        }

        fn payload(&self, command: serde_json::Value) -> Value {
            serde_json::json!({
                "tool_name": "shell",
                "tool_input": { "command": command },
                "cwd": self.root.display().to_string(),
            })
        }
    }

    impl Drop for MetricsFixture {
        fn drop(&mut self) {
            // SAFETY: paired with the set_var/remove_var in new(); under the lock.
            unsafe {
                match self.saved_home.take() {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
                match self.saved_metrics.take() {
                    Some(v) => std::env::set_var("PIXEL_METRICS", v),
                    None => std::env::remove_var("PIXEL_METRICS"),
                }
            }
            let _ = std::fs::remove_dir_all(self.root.parent().unwrap());
        }
    }

    #[test]
    fn extract_pixel_args_finds_the_invocation_through_shell_noise() {
        for (command, want) in [
            ("pixel impact src/a.rs", "impact src/a.rs"),
            ("cd /r && pixel impact a | head", "impact a"),
            ("FOO=1 sudo pixel status .", "status ."),
            ("env PIXEL_X=2 ~/.local/bin/pixel plan", "plan"),
            ("bash -lc pixel repo-state .", "repo-state ."),
            ("bash -lc 'cd /r && pixel impact f'", "impact f"),
            ("xargs -0 pixel context", "context"),
            // `rtk` is the global shell wrapper every command in this
            // environment runs through; the relay must reach the pixel call
            // past it, or it goes silent on the one form agents actually use.
            ("rtk pixel status .", "status ."),
            ("cd /r && rtk pixel impact f", "impact f"),
            (
                "rtk pixel find-code composed guard foreign denial",
                "find-code composed guard foreign denial",
            ),
        ] {
            assert_eq!(
                pixel_invocation(command).map(|i| i.args).as_deref(),
                Some(want),
                "{command}"
            );
        }
        for command in [
            "grep pixel src/",
            "echo pixel",
            "ls; rm -rf pixel",
            "cd /r && cargo test",
        ] {
            assert_eq!(pixel_invocation(command), None, "{command}");
        }
    }

    #[test]
    fn pixel_invocation_keeps_separators_inside_quotes() {
        for (command, want) in [
            (
                "pixel search-content 'foo|bar' .",
                "search-content foo|bar .",
            ),
            ("pixel search-content \"a;b\" .", "search-content a;b ."),
            ("pixel search-content 'a&b' .", "search-content a&b ."),
            ("pixel plan\ntrue", "plan"),
            ("true; pixel repo-state .", "repo-state ."),
            ("true && pixel context f", "context f"),
        ] {
            assert_eq!(
                pixel_invocation(command).map(|i| i.args).as_deref(),
                Some(want),
                "{command}"
            );
        }
        // A quoted pixel word is still an argument, not an invocation.
        assert_eq!(pixel_invocation("echo 'pixel status'"), None);
        // A quote must close where it ends: if it ran to end-of-line, the
        // separator after it would hide a later pixel call.
        assert_eq!(
            pixel_invocation("echo 'quoted' ; pixel impact f")
                .map(|i| i.args)
                .as_deref(),
            Some("impact f")
        );
    }

    #[test]
    fn pixel_invocation_resolves_metrics_env_in_command_order() {
        for (command, want) in [
            ("PIXEL_METRICS=0 pixel f", Some("0")),
            ("PIXEL_METRICS=1 pixel f", Some("1")),
            ("PIXEL_METRICS='0' pixel f", Some("0")),
            ("PIXEL_METRICS=\"0\" pixel f", Some("0")),
            ("env PIXEL_METRICS=0 pixel f", Some("0")),
            ("export PIXEL_METRICS='0'; pixel f", Some("0")),
            ("PIXEL_METRICS=0 PIXEL_METRICS=1 pixel f", Some("1")),
            // Prefixes pass into a wrapper's script as its inherited env.
            ("PIXEL_METRICS=0 bash -lc 'pixel f'", Some("0")),
            // …and the script's own prefixes override them.
            (
                "PIXEL_METRICS=0 bash -lc 'PIXEL_METRICS=1 pixel f'",
                Some("1"),
            ),
            ("bash -lc 'PIXEL_METRICS=0 pixel f'", Some("0")),
            // `export` reaches a later segment's call ambiently.
            ("export PIXEL_METRICS=0; pixel f", Some("0")),
            // An assignment on another segment is not the pixel call's env.
            ("PIXEL_METRICS=0 true && pixel f", None),
            // Argument position is not an env assignment.
            ("pixel f PIXEL_METRICS=0", None),
        ] {
            assert_eq!(
                pixel_invocation(command).map(|i| i.metrics_env),
                Some(want.map(str::to_string)),
                "{command}"
            );
        }
    }

    #[test]
    fn metrics_hook_line_correlates_on_the_effective_cd_directory() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("cd");
        // The host ran the tool from /, but the command cd'd into the repo:
        // the record lives under the effective cwd, not the payload cwd.
        let payload = serde_json::json!({
            "tool_name": "shell",
            "tool_input": { "command": format!("cd {} && pixel impact src/login.rs", fixture.root.display()) },
            "cwd": "/",
        });
        let line = metrics_hook_line(&payload).expect("the cd selects the record's cwd");
        assert!(line.starts_with("🟩 pixel impact ❀"), "{line}");
    }

    #[test]
    fn metrics_hook_line_honors_the_effective_pixel_metrics_env() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("env");
        let saved = std::env::var_os("PIXEL_METRICS");
        // SAFETY: under crate::ENV_LOCK in tests only; restored below.
        unsafe { std::env::set_var("PIXEL_METRICS", "0") };
        // The invocation emitted no footer — there is nothing to relay.
        assert_eq!(
            metrics_hook_line(&fixture.payload(serde_json::json!("pixel impact src/login.rs"))),
            None
        );
        // An inline assignment overrides the inherited value in command order.
        assert!(
            metrics_hook_line(&fixture.payload(serde_json::json!(
                "PIXEL_METRICS=1 pixel impact src/login.rs"
            )))
            .is_some()
        );
        // SAFETY: paired restore under the same lock.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("PIXEL_METRICS", v),
                None => std::env::remove_var("PIXEL_METRICS"),
            }
        }
        // Prefixes reach the call through wrappers and export alike.
        for command in [
            "PIXEL_METRICS=0 bash -lc 'pixel impact src/login.rs'",
            "bash -lc 'PIXEL_METRICS=0 pixel impact src/login.rs'",
            "export PIXEL_METRICS=0; pixel impact src/login.rs",
        ] {
            assert_eq!(
                metrics_hook_line(&fixture.payload(serde_json::json!(command))),
                None,
                "{command}"
            );
        }
    }

    #[test]
    fn metrics_hook_line_relays_a_single_matching_record() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("single");
        let line =
            metrics_hook_line(&fixture.payload(serde_json::json!("pixel impact src/login.rs")))
                .unwrap();
        assert!(line.starts_with("🟩 pixel impact ❀"), "{line}");
    }

    #[test]
    fn metrics_hook_line_suppresses_overlapping_identical_records() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("overlap");
        let start = pixel_actionlog::now_ms().saturating_sub(60_000);
        fixture.record_interval("impact", "impact src/login.rs", "overlap-1", start, 10_000);
        fixture.record_interval(
            "impact",
            "impact src/login.rs",
            "overlap-2",
            start.saturating_add(5_000),
            10_000,
        );
        assert_eq!(
            metrics_hook_line(&fixture.payload(serde_json::json!("pixel impact src/login.rs"))),
            None
        );
    }

    #[test]
    fn metrics_hook_line_keeps_the_latest_non_overlapping_record() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("sequential");
        let start = pixel_actionlog::now_ms().saturating_sub(60_000);
        fixture.record_interval("impact", "impact src/login.rs", "x-000001", start, 1_000);
        fixture.record_interval(
            "impact",
            "impact src/login.rs",
            "x-000002",
            start.saturating_add(2_000),
            1_000,
        );
        let line =
            metrics_hook_line(&fixture.payload(serde_json::json!("pixel impact src/login.rs")))
                .unwrap();
        assert!(line.contains("#000002"), "{line}");
    }

    #[test]
    fn metrics_hook_line_keeps_a_record_that_only_touches_the_previous_interval() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("touch");
        let start = pixel_actionlog::now_ms().saturating_sub(60_000);
        // The second record ends exactly where the first starts: the
        // intervals share a boundary but never overlap, so the latest record
        // stays attributable and the relay must not be suppressed.
        fixture.record_interval("impact", "impact src/login.rs", "x-000001", start, 1_000);
        fixture.record_interval(
            "impact",
            "impact src/login.rs",
            "x-000002",
            start.saturating_add(1_000),
            1_000,
        );
        let line =
            metrics_hook_line(&fixture.payload(serde_json::json!("pixel impact src/login.rs")))
                .unwrap();
        assert!(line.contains("#000002"), "{line}");
    }

    #[test]
    fn codex_metrics_should_stay_silent_when_the_call_wrote_no_record() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("codex-missing-record");
        fixture.record_interval(
            "impact",
            "impact src/login.rs",
            "claude-123456",
            pixel_actionlog::now_ms().saturating_sub(60_000),
            1_000,
        );
        let mut payload = fixture.payload(serde_json::json!("pixel impact src/login.rs"));
        payload["tool_response"] = serde_json::json!({"output": "pixel: bad path src/login.rs"});
        assert_eq!(metrics_hook_response(Some(Provider::Codex), &payload), None);
    }

    #[test]
    fn codex_metrics_should_relay_the_result_record_instead_of_a_newer_call() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("codex-result-record");
        let mut payload = fixture.payload(serde_json::json!("pixel impact src/login.rs"));
        let line = metrics_record_line(&payload).unwrap();
        payload["tool_response"] =
            serde_json::json!({"content": [{"text": format!("output\n{line}\n")}]});
        fixture.record_interval(
            "impact",
            "impact src/login.rs",
            "another-abcdef",
            pixel_actionlog::now_ms().saturating_add(60_000),
            1_000,
        );
        let expected = Some(serde_json::json!({"systemMessage": line}));
        assert_eq!(
            metrics_hook_response(Some(Provider::Codex), &payload),
            expected
        );
        payload["tool_response"] =
            serde_json::json!({"output": "🟩 pixel impact ❀ 4.0ms ❀ #absent"});
        assert_eq!(metrics_hook_response(Some(Provider::Codex), &payload), None);
    }

    #[test]
    fn codex_metrics_should_reject_colliding_headers_and_multiple_matching_footers() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("codex-ambiguous");
        let mut payload = fixture.payload(serde_json::json!("pixel impact src/login.rs"));
        let initial = metrics_record_line(&payload).unwrap();
        let start = pixel_actionlog::now_ms().saturating_add(60_000);
        fixture.record_interval("impact", "impact src/login.rs", "one-123456", start, 1_000);
        let second = metrics_record_line(&payload).unwrap();
        payload["tool_response"] = serde_json::json!({"output": format!("{initial}\n{second}")});
        assert_eq!(metrics_hook_response(Some(Provider::Codex), &payload), None);
        fixture.record_interval(
            "impact",
            "impact src/login.rs",
            "two-123456",
            start + 2_000,
            1_000,
        );
        payload["tool_response"] = serde_json::json!({"output": second});
        assert_eq!(metrics_hook_response(Some(Provider::Codex), &payload), None);
    }

    #[test]
    fn codex_metrics_should_reject_a_legacy_record_without_an_invocation_id() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("codex-legacy");
        let mut event =
            pixel_actionlog::tail(&pixel_actionlog::ActionLog::path_for_root(&fixture.root), 1)
                .unwrap()
                .pop()
                .unwrap();
        for id in [None, Some(String::new())] {
            event.invocation_id = id;
            let line = pixel_actionlog::format_metrics_line(&event).unwrap();
            fixture.append_event(event.clone()); // append_event consumes it; the next case reuses it.
            let mut payload = fixture.payload(serde_json::json!("pixel impact src/login.rs"));
            payload["tool_response"] = serde_json::json!({"stderr": line});
            assert_eq!(metrics_hook_response(Some(Provider::Codex), &payload), None);
        }
    }

    #[test]
    fn codex_metrics_should_keep_effective_cwd_and_opt_out_checks() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("codex-cwd-optout");
        let mut payload = fixture.payload(serde_json::json!("pixel impact src/login.rs"));
        let line = metrics_record_line(&payload).unwrap();
        payload["cwd"] = serde_json::json!("/");
        payload["tool_input"]["command"] = serde_json::json!(format!(
            "cd {} && pixel impact src/login.rs",
            fixture.root.display()
        ));
        payload["tool_response"] = serde_json::json!({"stderr": line});
        assert_eq!(
            metrics_hook_response(Some(Provider::Codex), &payload),
            Some(serde_json::json!({"systemMessage": line}))
        );
        payload["tool_input"]["command"] = serde_json::json!(format!(
            "cd {} && PIXEL_METRICS=0 pixel impact src/login.rs",
            fixture.root.display()
        ));
        assert_eq!(metrics_hook_response(Some(Provider::Codex), &payload), None);
        payload["tool_input"]["command"] = serde_json::json!(format!(
            "cd {} && pixel impact src/login.rs",
            fixture.root.display()
        ));
        std::fs::write(
            fixture.root.join(".pixel/config.yaml"),
            "{\"metrics\": \"off\"}\n",
        )
        .unwrap();
        assert_eq!(metrics_hook_response(Some(Provider::Codex), &payload), None);
    }

    #[test]
    fn metrics_header_should_match_complete_string_lines_only() {
        let header = "🟩 pixel impact ❀ 4.0ms ❀ #123456";
        for result in [
            serde_json::json!(header),
            serde_json::json!({"stderr": format!("output\n {header} \nfooter")}),
            serde_json::json!([null, {"content": [{"text": header}]}]),
        ] {
            assert!(result_has_metrics_header(&result, header), "{result}");
        }
        for result in [
            serde_json::json!(format!("prefix {header}")),
            serde_json::json!(format!("{header}suffix")),
            serde_json::json!({header: "object key, not output"}),
            serde_json::json!([null, false, 42, ""]),
        ] {
            assert!(!result_has_metrics_header(&result, header), "{result}");
        }
    }

    #[test]
    fn metrics_hook_response_relays_visible_metrics_for_claude_and_codex_users() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("response");
        let plain = fixture.payload(serde_json::json!("pixel impact src/login.rs"));
        let line = metrics_hook_line(&plain).expect("seeded record");
        let mut shown = plain.clone();
        shown["tool_response"] = serde_json::json!({"stdout": "", "stderr": line});
        // Box missing: the model context replay, for every provider.
        let advisory = post_tool_use_advisory(&line);
        for provider in [None, Some(Provider::Claude), Some(Provider::Devin)] {
            assert_eq!(
                metrics_hook_response(provider, &plain),
                Some(advisory.clone())
            );
        }
        assert_eq!(
            metrics_hook_response(Some(Provider::Codex), &plain),
            None,
            "Codex needs invocation evidence from this tool result"
        );
        // A box in tool_result isn't sufficient for a user-visible result in
        // Claude or Codex; both use their documented systemMessage field.
        assert_eq!(
            metrics_hook_response(Some(Provider::Claude), &shown),
            Some(serde_json::json!({"systemMessage": line}))
        );
        assert_eq!(
            metrics_hook_response(Some(Provider::Codex), &shown),
            Some(serde_json::json!({"systemMessage": line}))
        );
        for provider in [None, Some(Provider::Devin)] {
            assert_eq!(metrics_hook_response(provider, &shown), None);
        }
        // Shown, but no record matches: nothing to finalize.
        shown["tool_input"] = serde_json::json!({"command": "pixel impact other.rs"});
        assert_eq!(metrics_hook_response(Some(Provider::Claude), &shown), None);
        assert_eq!(metrics_hook_response(Some(Provider::Codex), &shown), None);
    }

    /// Pi relays like Codex: always the user-only `systemMessage`, whether or
    /// not the bash result already holds the box, and never `additionalContext`
    /// (the model reads the box in the result itself).
    #[test]
    fn metrics_hook_response_relays_to_pi_as_a_system_message_only() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("pi-response");
        let plain = fixture.payload(serde_json::json!("pixel impact src/login.rs"));
        let line = metrics_hook_line(&plain).expect("seeded record");
        let mut shown = plain.clone();
        shown["tool_response"] = serde_json::json!({"stdout": line, "stderr": ""});
        let expected = Some(serde_json::json!({"systemMessage": line}));
        assert_eq!(metrics_hook_response(Some(Provider::Pi), &plain), expected);
        assert_eq!(metrics_hook_response(Some(Provider::Pi), &shown), expected);
        // Shown, but no record matches: nothing to finalize.
        shown["tool_input"] = serde_json::json!({"command": "pixel impact other.rs"});
        assert_eq!(metrics_hook_response(Some(Provider::Pi), &shown), None);
        assert_eq!(
            <Provider as clap::ValueEnum>::from_str("pi", false),
            Ok(Provider::Pi)
        );
        // Pi's lowercase tool name is a shell tool.
        let mut lower = plain;
        lower["tool_name"] = serde_json::json!("bash");
        assert_eq!(metrics_hook_response(Some(Provider::Pi), &lower), expected);
    }

    #[test]
    fn antigravity_post_tool_use_should_store_line_and_answer_empty_object() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("antigravity");
        let artifact_dir = fixture
            .root
            .parent()
            .unwrap()
            .join("home/.gemini/antigravity/conversation-a");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        let payload = provider_payload(
            Provider::Antigravity,
            serde_json::json!({
                "toolCall": {
                    "name": "run_command",
                    "args": {
                        "CommandLine": "pixel impact src/login.rs",
                        "Cwd": fixture.root.display().to_string()
                    }
                },
                "conversationId": "conversation-a",
                "artifactDirectoryPath": artifact_dir,
                "stepIdx": 4
            }),
        );
        assert_eq!(
            antigravity_metrics_hook_response(MetricsEvent::PostToolUse, &payload),
            Some(serde_json::json!({}))
        );
        let line = metrics_record_line(&payload).expect("the fixture has one matching record");
        assert!(line.starts_with("🟩 pixel impact ❀"), "{line}");
        assert_eq!(line.lines().count(), 1, "{line}");
        let pending_file = artifact_dir
            .join(ANTIGRAVITY_PENDING_METRICS_DIR)
            .join("4.json");
        assert!(pending_file.is_file(), "PostToolUse persisted the metric");
        let post_invocation = serde_json::json!({
            "conversationId": "conversation-a",
            "artifactDirectoryPath": artifact_dir,
            "initialNumSteps": 2
        });
        assert_eq!(
            antigravity_metrics_hook_response(MetricsEvent::PostInvocation, &post_invocation),
            Some(serde_json::json!({
                "injectSteps": [{"ephemeralMessage": line}]
            }))
        );
        assert!(!pending_file.exists(), "the delivered metric is consumed");
    }

    #[test]
    fn antigravity_pending_metrics_should_stay_conversation_scoped_and_clear_stale_steps() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("antigravity-correlation");
        fixture.record("impact", "impact src/other.rs");
        let home = fixture.root.parent().unwrap().join("home");
        let artifact = |conversation: &str| {
            let path = home.join(".gemini/antigravity").join(conversation);
            std::fs::create_dir_all(&path).unwrap();
            path
        };
        let tool_payload = |conversation: &str, artifact_dir: &Path, command: &str, step_idx| {
            provider_payload(
                Provider::Antigravity,
                serde_json::json!({
                    "toolCall": {"name": "run_command", "args": {
                        "CommandLine": command,
                        "Cwd": fixture.root.display().to_string()
                    }},
                    "conversationId": conversation,
                    "artifactDirectoryPath": artifact_dir,
                    "stepIdx": step_idx
                }),
            )
        };
        let artifact_a = artifact("conversation-a");
        let artifact_b = artifact("conversation-b");
        let payload_a = tool_payload(
            "conversation-a",
            &artifact_a,
            "pixel impact src/login.rs",
            4,
        );
        let payload_b = tool_payload(
            "conversation-b",
            &artifact_b,
            "pixel impact src/other.rs",
            4,
        );
        assert_eq!(
            antigravity_metrics_hook_response(MetricsEvent::PostToolUse, &payload_a),
            Some(serde_json::json!({}))
        );
        assert_eq!(
            antigravity_metrics_hook_response(MetricsEvent::PostToolUse, &payload_b),
            Some(serde_json::json!({}))
        );
        let line_a = metrics_record_line(&payload_a).unwrap();
        let line_b = metrics_record_line(&payload_b).unwrap();
        assert_ne!(line_a, line_b);

        let invocation_a = serde_json::json!({
            "conversationId": "conversation-a",
            "artifactDirectoryPath": artifact_a,
            "initialNumSteps": 1
        });
        assert_eq!(
            antigravity_metrics_hook_response(MetricsEvent::PostInvocation, &invocation_a),
            Some(serde_json::json!({
                "injectSteps": [{"ephemeralMessage": line_a}]
            }))
        );
        let pending_b = artifact_b
            .join(ANTIGRAVITY_PENDING_METRICS_DIR)
            .join("4.json");
        assert!(pending_b.is_file(), "consuming A leaves B pending");

        let invocation_b = serde_json::json!({
            "conversationId": "conversation-b",
            "artifactDirectoryPath": artifact_b,
            "initialNumSteps": 1
        });
        assert_eq!(
            antigravity_metrics_hook_response(MetricsEvent::PostInvocation, &invocation_b),
            Some(serde_json::json!({
                "injectSteps": [{"ephemeralMessage": line_b}]
            }))
        );
        assert!(!pending_b.exists(), "consuming B clears its state");

        let stale_payload = tool_payload(
            "conversation-a",
            &artifact_a,
            "pixel impact src/login.rs",
            5,
        );
        store_antigravity_pending_metric(&stale_payload, &line_a).unwrap();
        let stale_invocation = serde_json::json!({
            "conversationId": "conversation-a",
            "artifactDirectoryPath": artifact_a,
            "initialNumSteps": 6
        });
        assert_eq!(
            antigravity_metrics_hook_response(MetricsEvent::PostInvocation, &stale_invocation),
            None,
            "earlier-step metrics are not replayed into a later invocation"
        );
        assert!(
            !artifact_a.join(ANTIGRAVITY_PENDING_METRICS_DIR).exists(),
            "stale conversation state is cleaned"
        );
    }

    #[test]
    fn tool_command_text_accepts_string_and_argv_array_forms() {
        assert_eq!(
            tool_command_text(&serde_json::json!({"command": "pixel x"})),
            Some("pixel x".to_string())
        );
        assert_eq!(
            tool_command_text(&serde_json::json!({"cmd": "pixel y"})),
            Some("pixel y".to_string())
        );
        assert_eq!(
            tool_command_text(&serde_json::json!({"command": ["bash", "-lc", "pixel impact f"]})),
            Some("pixel impact f".to_string())
        );
        // argv keeps the `-c` script boundary: trailing elements are $0
        // positionals, not part of the command.
        assert_eq!(
            tool_command_text(
                &serde_json::json!({"command": ["bash", "-c", "pixel impact", "ignored"]})
            ),
            Some("pixel impact".to_string())
        );
        assert_eq!(tool_command_text(&serde_json::json!({})), None);
        assert_eq!(tool_command_text(&serde_json::json!({"command": 3})), None);
    }

    #[test]
    fn metrics_hook_line_replays_the_matching_finalized_record() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("replay");
        let line =
            metrics_hook_line(&fixture.payload(serde_json::json!("pixel impact src/login.rs")))
                .expect("the seeded record must relay");
        assert!(line.starts_with("🟩 pixel impact ❀"), "{line}");
        assert_eq!(line.lines().count(), 1, "{line}");
        assert!(!line.contains("unavailable"), "{line}");
    }

    #[test]
    fn metrics_hook_line_stays_silent_on_every_miss() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("miss");
        // Not a shell tool.
        assert_eq!(
            metrics_hook_line(&serde_json::json!({
                "tool_name": "Edit",
                "tool_input": {"command": "pixel impact src/login.rs"},
                "cwd": fixture.root.display().to_string(),
            })),
            None
        );
        // Host already delivered stderr in the tool result.
        assert_eq!(
            metrics_hook_line(&serde_json::json!({
                "tool_name": "shell",
                "tool_input": {"command": "pixel impact src/login.rs"},
                "tool_response": {"output": "…\n🟩 pixel impact ❀ 1ms"},
                "cwd": fixture.root.display().to_string(),
            })),
            None
        );
        // Per-invocation vetoes stay vetoes.
        assert_eq!(
            metrics_hook_line(&fixture.payload(serde_json::json!(
                "PIXEL_METRICS=0 pixel impact src/login.rs"
            ))),
            None
        );
        assert_eq!(
            metrics_hook_line(
                &fixture.payload(serde_json::json!("pixel impact src/login.rs --metrics=off"))
            ),
            None
        );
        // Either veto form alone must silence — the recorded args matching
        // the veto'd command must not leak a line back.
        fixture.record("impact", "impact src/login.rs --metrics=off");
        fixture.record("impact", "impact src/login.rs --metrics off");
        assert_eq!(
            metrics_hook_line(
                &fixture.payload(serde_json::json!("pixel impact src/login.rs --metrics=off"))
            ),
            None
        );
        assert_eq!(
            metrics_hook_line(
                &fixture.payload(serde_json::json!("pixel impact src/login.rs --metrics off"))
            ),
            None
        );
        // No pixel invocation at all.
        assert_eq!(
            metrics_hook_line(&fixture.payload(serde_json::json!("cargo test"))),
            None
        );
        // A record from another cwd must not be claimed as this call.
        let other = fixture.root.parent().unwrap().join("elsewhere");
        std::fs::create_dir_all(other.join(".pixel")).unwrap();
        let other = canonical(&other);
        fixture.record_at("foreign", "impact src/login.rs", &other);
        let line =
            metrics_hook_line(&fixture.payload(serde_json::json!("pixel impact src/login.rs")))
                .expect("this repo's own record still matches");
        assert!(
            line.starts_with("🟩 pixel impact ❀"),
            "a foreign-cwd record must not be claimed: {line}"
        );
    }

    #[test]
    fn metrics_hook_line_honors_the_persistent_opt_out() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("optout");
        std::fs::write(
            fixture.root.join(".pixel/config.yaml"),
            "{\"metrics\": \"off\"}\n",
        )
        .unwrap();
        assert_eq!(
            metrics_hook_line(&fixture.payload(serde_json::json!("pixel impact src/login.rs"))),
            None,
            "config off silences the relay exactly like stderr"
        );
    }
}
