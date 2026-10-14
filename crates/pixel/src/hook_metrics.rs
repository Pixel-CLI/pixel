// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel run-hook metrics` — the PostToolUse relay that replays a pixel
//! invocation's finalized 🟩 metrics line to hosts whose tool results drop
//! stderr (Codex) or keep it user-invisible (Claude's Bash result). The hook
//! reads the tool-call payload, matches the shell command's pixel invocation
//! to the newest finalized action record for the effective cwd, and re-emits
//! its metrics line in the provider's hook-response contract. Every miss —
//! malformed payload, no pixel call, suppressed or missing metrics, an
//! ambiguous overlap — is a silent exit 0: the relay is advisory and must
//! never turn a tool call into a failure.
//!
//! Restored verbatim from the implementation retired in `14feba45` (the
//! `guard.rs` cluster), narrowed to the metrics path.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The hook payload's host. Named `Provider` to keep the restored code
/// byte-identical to the version the registered command lines carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Provider {
    Claude,
    Codex,
    Devin,
    /// Antigravity's `toolCall` payload is normalized to the shared shape.
    Antigravity,
    /// Cursor's `postToolUse`: flat `tool_output` payload, snake_case
    /// `additional_context` response.
    Cursor,
    /// Copilot CLI: camelCase `toolName`/`toolArgs`/`toolResult` payload,
    /// top-level `additionalContext` response.
    Copilot,
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
    // Copilot CLI sends camelCase fields; normalize to the shared
    // snake_case shape the rest of the guard reads.
    if provider == Provider::Copilot {
        if payload.get("tool_name").is_none()
            && let Some(value) = payload.get("toolName").cloned()
        {
            payload["tool_name"] = value;
        }
        if payload.get("tool_input").is_none()
            && let Some(value) = payload.get("toolArgs").cloned()
        {
            // The CLI hook docs describe `toolArgs` as a JSON string; the
            // SDK sends the object itself. Accept both.
            payload["tool_input"] = match value.as_str().and_then(|s| {
                let parsed: Result<Value, _> = serde_json::from_str(s);
                parsed.ok()
            }) {
                Some(parsed) => parsed,
                None => value,
            };
        }
        return payload;
    }
    if provider == Provider::Antigravity
        && let Some(call) = payload.get("toolCall")
    {
        let tool = call.get("name").cloned().unwrap_or(Value::Null);
        let input = call.get("args").cloned().unwrap_or(Value::Null);
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
        // Cursor sends `cwd: ""` on Shell calls and puts the repo in
        // `workspace_roots` instead (observed on cursor-agent 2026.10.01);
        // Antigravity's PreInvocation payload names only `workspacePaths`.
        .or_else(|| {
            ["workspace_roots", "workspacePaths"]
                .into_iter()
                .find_map(|key| payload.get(key)?.as_array()?.first()?.as_str())
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
    // Cursor's composer-mode shell tool.
    "Shell",
    "shell",
    "local_shell",
    "unified_exec",
    "exec",
    "run_command",
    "container.exec",
    "functions.shell",
];

/// `pixel run-hook metrics` — PostToolUse relay whose dedupe drops a 🟩
/// block already in the tool result. Codex's exec layer merges the
/// invocation's stderr into the tool result it records and shows, so the
/// relay is the fallback for the rare host whose tool result drops it,
/// not the primary path: the 🟩 line reaches a Codex user once via the
/// tool result and the relay no-ops on its dedupe. The hook reads the
/// payload, matches the invocation to its finalized action record, and
/// replays that record's line as `additionalContext`. Any miss is a
/// silent exit: the relay is advisory and must never turn a tool call
/// into a failure.
#[cfg_attr(test, mutants::skip)] // stdin + process::exit boundary; every decision lives in `metrics_hook_line`
pub fn run_metrics_hook(provider: Option<Provider>) -> ! {
    // An imported Claude entry running beside Devin's own relay would emit
    // the Claude contract (`systemMessage`) into a host that never asked for
    // it and double the native `--provider devin` relay's output.
    if imported_claude_entry(provider) {
        std::process::exit(0);
    }
    let Some(input) = crate::hook_input::read_hook_payload() else {
        std::process::exit(0);
    };
    let Ok(payload) = serde_json::from_str::<Value>(&input) else {
        std::process::exit(0);
    };
    // Antigravity's PostToolUse carries a camelCase `toolCall` payload; the
    // same provider-boundary normalization the guard path applies lets the
    // relay read it (a miss is the documented silent exit below).
    let payload = match provider {
        Some(p) => provider_payload(p, payload),
        None => payload,
    };
    if let Some(response) = metrics_hook_response(provider, &payload) {
        print!("{response}");
    }
    std::process::exit(0);
}

/// Whether the host already put the invocation's 🟩 box in the tool result.
fn result_carries_metrics_box(payload: &Value) -> bool {
    // `tool_response` is the Claude/Codex/Devin field; Cursor's postToolUse
    // sends the same idea as `tool_output` (a JSON-stringified result);
    // Copilot nests it under `toolResult.textResultForLlm` — the serialized
    // contains-check reaches the nested text field.
    ["tool_response", "tool_output", "toolResult"]
        .iter()
        .any(|key| {
            payload
                .get(key)
                .is_some_and(|r| r.to_string().contains("🟩 pixel"))
        })
}

/// The provider-shaped envelope for a replayed metrics line: Cursor's
/// postToolUse contract is flat snake_case `additional_context` and
/// Copilot's is flat camelCase `additionalContext`; every other host takes
/// the Claude-style advisory shape.
fn metrics_relay_response(provider: Option<Provider>, line: &str) -> Value {
    match provider {
        Some(Provider::Cursor) => serde_json::json!({"additional_context": line}),
        Some(Provider::Copilot) => serde_json::json!({"additionalContext": line}),
        _ => post_tool_use_advisory(line),
    }
}

/// The hook's answer. A box missing from the result is replayed as
/// `additionalContext` for every provider. A box already in the result is a
/// duplicate for the model, so it is dropped, except for Claude Code: its
/// Bash result carries stderr, which the user never sees, so the finalized
/// box goes out as `systemMessage` only (user-visible, not model context).
/// Devin and Codex document no `systemMessage` for this event and keep the
/// silent dedupe.
fn metrics_hook_response(provider: Option<Provider>, payload: &Value) -> Option<Value> {
    if !result_carries_metrics_box(payload) {
        return metrics_hook_line(payload).map(|line| metrics_relay_response(provider, &line));
    }
    (provider == Some(Provider::Claude))
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
// Skip: the overlap guard's `other_start < latest_end` clause is dead — `latest`
// is the newest same-command record, so `other_start <= latest_start < latest_end`
// always holds for real durations; its `<`->`<=` flip only differs for a
// zero-duration newest record, which the action log never emits.
#[cfg_attr(test, mutants::skip)]
fn metrics_record_line(payload: &Value) -> Option<String> {
    let tool = payload.get("tool_name")?.as_str()?;
    if !METRICS_SHELL_TOOLS.contains(&tool) {
        return None;
    }
    let tool_input = payload.get("tool_input")?;
    let command = tool_command_text(tool_input)?;
    // Resolve the working directory the way enforcement does, so an empty
    // `cwd` (Cursor sends `cwd: ""`) falls back to `workspace_roots` and the
    // workspace's `.pixel` is still found for the metrics lookup.
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
        })
        .collect();
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
    // `~/.pixel/config.json`; serialize tests that repoint it through the
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
    fn cursor_metrics_relay_resolves_the_workspace_from_empty_cwd() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("cursor-empty-cwd");
        // The documented Cursor shell payload: `cwd: ""` with the repository
        // in `workspace_roots`, the hook process running outside the
        // workspace. The record lookup must still find the workspace's
        // action log, or the relay never fires for Cursor.
        let payload = serde_json::json!({
            "tool_name": "Shell",
            "tool_input": { "command": "pixel impact src/login.rs" },
            "cwd": "",
            "workspace_roots": [fixture.root],
        });
        let line =
            metrics_hook_line(&payload).expect("the workspace resolves through workspace_roots");
        assert!(line.starts_with("🟩 pixel impact ❀"), "{line}");
        // Without workspace_roots and with an empty cwd there is no
        // workspace to look up from the fixture dir: nothing to relay.
        let payload = serde_json::json!({
            "tool_name": "Shell",
            "tool_input": { "command": "pixel impact src/login.rs" },
            "cwd": "",
        });
        assert_eq!(metrics_hook_line(&payload), None);
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
    fn metrics_hook_response_dedupes_except_for_claude_users() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("response");
        let plain = fixture.payload(serde_json::json!("pixel impact src/login.rs"));
        let line = metrics_hook_line(&plain).expect("seeded record");
        let mut shown = plain.clone();
        shown["tool_response"] = serde_json::json!({"stdout": "", "stderr": line});
        // Box missing: the model context replay, for every provider.
        let advisory = post_tool_use_advisory(&line);
        for provider in [
            None,
            Some(Provider::Claude),
            Some(Provider::Devin),
            Some(Provider::Codex),
        ] {
            assert_eq!(
                metrics_hook_response(provider, &plain),
                Some(advisory.clone())
            );
        }
        // Box shown: only Claude answers, with the system message alone.
        assert_eq!(
            metrics_hook_response(Some(Provider::Claude), &shown),
            Some(serde_json::json!({"systemMessage": line}))
        );
        for provider in [
            None,
            Some(Provider::Devin),
            Some(Provider::Cursor),
            Some(Provider::Cursor),
        ] {
            assert_eq!(metrics_hook_response(provider, &shown), None);
        }
        // Shown, but no record matches: nothing to finalize.
        shown["tool_input"] = serde_json::json!({"command": "pixel impact other.rs"});
        assert_eq!(metrics_hook_response(Some(Provider::Claude), &shown), None);
    }

    #[test]
    fn metrics_relay_response_switches_on_the_cursor_contract() {
        let advisory = post_tool_use_advisory("line");
        assert_eq!(
            metrics_relay_response(Some(Provider::Cursor), "line"),
            serde_json::json!({"additional_context": "line"})
        );
        // Every non-Cursor provider — including the None fallback — keeps
        // the Claude-style advisory envelope; a flipped comparison here
        // would hand Cursor a shape it cannot read and vice versa.
        for provider in [
            None,
            Some(Provider::Claude),
            Some(Provider::Codex),
            Some(Provider::Devin),
        ] {
            assert_eq!(metrics_relay_response(provider, "line"), advisory);
        }
    }

    #[test]
    fn cursor_metrics_response_uses_the_flat_additional_context_field() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("cursor");
        let mut payload = fixture.payload(serde_json::json!("pixel impact src/login.rs"));
        payload["tool_name"] = serde_json::json!("Shell");
        let line = metrics_hook_line(&payload).expect("seeded record");
        assert_eq!(
            metrics_hook_response(Some(Provider::Cursor), &payload),
            Some(serde_json::json!({"additional_context": line}))
        );
        // The box inside Cursor's `tool_output` dedupes like `tool_response`.
        payload["tool_output"] = serde_json::json!(line);
        assert_eq!(
            metrics_hook_response(Some(Provider::Cursor), &payload),
            None
        );
    }

    /// Cursor sends `cwd: ""` on Shell calls and names the repo only in
    /// `workspace_roots`; the metrics relay must still resolve the workspace
    /// and find the matching record instead of silently returning no line.
    #[test]
    fn cursor_metrics_lookup_falls_back_to_workspace_roots_on_empty_cwd() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let fixture = MetricsFixture::new("cursor-roots");
        let payload = serde_json::json!({
            "tool_name": "Shell",
            "tool_input": { "command": "pixel impact src/login.rs" },
            "cwd": "",
            "workspace_roots": [fixture.root.display().to_string()],
        });
        let line = metrics_hook_line(&payload)
            .expect("an empty cwd still resolves the workspace via workspace_roots");
        assert!(line.starts_with("🟩 pixel impact"), "{line}");
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
            fixture.root.join(".pixel/config.json"),
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
