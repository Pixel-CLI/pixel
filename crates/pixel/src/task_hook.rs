// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Host hook normalization and response envelopes for durable task gates.

use std::io::Write;
use std::iter::Peekable;
use std::path::Path;
use std::str::Chars;
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use crate::brief_parts::{self, BriefPart, Limits};

// Native hooks have a ten-second host deadline; leave time for stdin, late
// first-part recovery, and later parts to observe its done marker.
const DECISION_TIMEOUT: Duration = Duration::from_secs(8);
const PI_DECISION_TIMEOUT: Duration = Duration::from_secs(6);
const fn decision_timeout(provider: TaskProvider) -> Duration {
    if matches!(provider, TaskProvider::Pi) {
        PI_DECISION_TIMEOUT
    } else {
        DECISION_TIMEOUT
    }
}
// The brief's longest window plus the time to print it stays inside that decision.
const _: () = assert!(
    pixel_setup::config::BRIEF_WINDOW_MAX_MS + 1_500 <= PI_DECISION_TIMEOUT.as_millis() as u64
);
// The probes' ceiling (5 s after the prompt) leaves a second of the decision to print the merged brief.
const _: () = assert!(5_000 + 1_000 <= PI_DECISION_TIMEOUT.as_millis() as u64);
/// What of the decision window printing the answer needs: the brief and its
/// probes end that much before the decision does (the second the probes'
/// ceiling above leaves).
const PRINT_RESERVE: Duration = Duration::from_millis(1_000);

/// The instant a decision that starts at `now` must have its answer ready by:
/// the host's decision window less [`PRINT_RESERVE`].
fn answer_by(provider: TaskProvider, now: std::time::Instant) -> std::time::Instant {
    now + decision_timeout(provider).saturating_sub(PRINT_RESERVE)
}
// A stdin its host leaves open is waited on before the decision starts: both
// together stay inside Pi's eight-second deadline, the tighter of the two.
const _: () =
    assert!(crate::hook_input::STDIN_WAIT.as_millis() + PI_DECISION_TIMEOUT.as_millis() < 8_000);
// Native hooks keep the decision and late-first-part fallback under ten seconds.
const _: () = assert!(
    crate::hook_input::STDIN_WAIT.as_millis()
        + DECISION_TIMEOUT.as_millis()
        + LATE_GRACE.as_millis()
        + ORDERING_SLACK.as_millis()
        < (pixel_setup::config::HOOK_TIMEOUT * 1_000) as u128
);
const _: () = assert!(
    crate::hook_input::STDIN_WAIT.as_millis()
        + PI_DECISION_TIMEOUT.as_millis()
        + LATE_GRACE.as_millis()
        + ORDERING_SLACK.as_millis()
        < 8_000
);
/// Payload field of Pixel's own Pi package (`extensions/pixel-brief.ts`): the
/// event carries a prompt or a session start for the evidence brief alone. The
/// ledger stays out of it (no task is bound, no gate or enforcement applies),
/// so only the brief, its read-feedback line, the daemon warm-start and the
/// metrics `systemMessage` run. A Pi host that drives the task lifecycle
/// itself does not send it and keeps `TaskProvider::Pi`'s ledger-only events.
const BRIEF_ONLY: &str = "brief_only";
const UNAVAILABLE: &str = "Pixel task state is unavailable. Reads and recovery remain available; edits and completion require a working task ledger.";

/// Hosts with a verified task lifecycle adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum TaskProvider {
    Claude,
    Codex,
    Pi,
    Devin,
    Gemini,
    Antigravity,
}

impl TaskProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::Devin => "devin",
            Self::Gemini => "gemini",
            Self::Antigravity => "antigravity",
        }
    }

    /// The host's name for the prompt-submission event: Gemini calls it
    /// `BeforeAgent`; Antigravity's closest event is `PreInvocation`, gated
    /// on `invocationNum == 0`. The rest share Claude's `UserPromptSubmit`.
    fn prompt_event_name(self) -> &'static str {
        match self {
            Self::Gemini => "BeforeAgent",
            Self::Antigravity => "PreInvocation",
            _ => "UserPromptSubmit",
        }
    }
}

/// Host lifecycle boundaries translated to the shared task bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum TaskHookEvent {
    SessionStart,
    PromptSubmit,
    PreToolUse,
    PostToolUse,
    ToolFailure,
    Stop,
    SessionEnd,
    Interrupt,
    SubagentStart,
    SubagentStop,
    ModelResponse,
    UserBash,
}

impl TaskHookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session-start",
            Self::PromptSubmit => "prompt-submit",
            Self::PreToolUse => "pre-tool-use",
            Self::PostToolUse => "post-tool-use",
            Self::ToolFailure => "tool-failure",
            Self::Stop => "stop",
            Self::SessionEnd => "session-end",
            Self::Interrupt => "interrupt",
            Self::SubagentStart => "subagent-start",
            Self::SubagentStop => "subagent-stop",
            Self::ModelResponse => "model-response",
            Self::UserBash => "user-bash",
        }
    }

    fn host_name(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::PromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::ToolFailure => "PostToolUseFailure",
            Self::Stop => "Stop",
            Self::SessionEnd => "SessionEnd",
            Self::Interrupt => "Interrupt",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::ModelResponse => "ModelResponse",
            Self::UserBash => "UserBash",
        }
    }
}

fn string<'a>(payload: &'a Value, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| payload.get(name)?.as_str().filter(|s| !s.is_empty()))
}

/// The keys a host names its session by, for the brief's own state: Claude,
/// Codex, Devin and Pi's package send `session_id`; Antigravity sends only
/// `conversationId`, so without it the read feedback, the part cache and the
/// region dedupe of an Antigravity brief had no session at all.
const BRIEF_SESSION_KEYS: &[&str] = &["session_id", "sessionId", "conversationId"];

/// The session a brief belongs to (see [`BRIEF_SESSION_KEYS`]). The task
/// ledger keeps `session_id` / `sessionId` only: Antigravity registers no
/// lifecycle hook, and binding its conversations is not this lookup's call.
fn brief_session(payload: &Value) -> Option<&str> {
    string(payload, BRIEF_SESSION_KEYS)
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn tool_label(value: &str) -> Option<&str> {
    (!value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:/".contains(&byte)))
    .then_some(value)
}

fn bounded_count(value: &Value, maximum: u64) -> Option<u64> {
    value.as_u64().filter(|count| *count <= maximum)
}

fn usage(payload: &Value) -> Value {
    let counters = ["input", "output", "cache_read", "cache_write"]
        .into_iter()
        .filter_map(|key| {
            bounded_count(&payload["usage"][key], 1_000_000_000_000)
                .map(|count| (key.to_string(), Value::from(count)))
        })
        .collect::<serde_json::Map<_, _>>();
    if counters.is_empty() {
        Value::Null
    } else {
        Value::Object(counters)
    }
}

fn read_flags(args: &[String], allowed: &[&str], prefixes: &[&str]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .all(|arg| {
            !arg.starts_with('-')
                || allowed.contains(&arg.as_str())
                || prefixes.iter().any(|prefix| arg.starts_with(prefix))
        })
}

fn git_read(args: &[String]) -> bool {
    args.first().is_some_and(|op| {
        matches!(
            op.as_str(),
            "status" | "diff" | "log" | "show" | "rev-parse" | "ls-files"
        )
    }) && read_flags(
        &args[1..],
        &[
            "-h",
            "--help",
            "-s",
            "--short",
            "-b",
            "--branch",
            "--porcelain",
            "--porcelain=v1",
            "--porcelain=v2",
            "-z",
            "-v",
            "-t",
            "-m",
            "-o",
            "-d",
            "-c",
            "-u",
            "--cached",
            "--staged",
            "--others",
            "--exclude-standard",
            "--modified",
            "--deleted",
            "--unmerged",
            "--stage",
            "--check",
            "--stat",
            "--numstat",
            "--shortstat",
            "--name-only",
            "--name-status",
            "--summary",
            "--patch",
            "-p",
            "--no-patch",
            "--binary",
            "--raw",
            "--exit-code",
            "--quiet",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--no-index",
            "--oneline",
            "--graph",
            "--decorate",
            "--all",
            "--no-decorate",
            "--no-color",
            "--reverse",
            "--show-toplevel",
            "--show-prefix",
            "--git-dir",
            "--git-path",
            "--absolute-git-dir",
            "--verify",
            "--revs-only",
            "--is-inside-work-tree",
            "--abbrev-ref",
            "--symbolic-full-name",
            "--end-of-options",
        ],
        &[
            "--format=",
            "--pretty=",
            "--max-count=",
            "--since=",
            "--until=",
            "--color=",
            "--unified=",
        ],
    )
}

fn search_read(args: &[String]) -> bool {
    read_flags(
        args,
        &[
            "-h",
            "--help",
            "--version",
            "-n",
            "-i",
            "-v",
            "-l",
            "-L",
            "-c",
            "-q",
            "-w",
            "-x",
            "-s",
            "-F",
            "-E",
            "-e",
            "-f",
            "-g",
            "-r",
            "-R",
            "-H",
            "-I",
            "-a",
            "-o",
            "-U",
            "-z",
            "-0",
            "-A",
            "-B",
            "-C",
            "-m",
            "--files",
            "--hidden",
            "--no-ignore",
            "--no-config",
            "--json",
            "--line-number",
            "--ignore-case",
            "--fixed-strings",
            "--count",
            "--files-with-matches",
            "--files-without-match",
            "--glob",
            "--iglob",
            "--type",
            "--type-not",
            "--max-count",
            "--context",
            "--after-context",
            "--before-context",
            "--regexp",
            "--file",
            "--no-heading",
            "--heading",
        ],
        &[
            "--glob=",
            "--iglob=",
            "--type=",
            "--type-not=",
            "--max-count=",
            "--context=",
            "--after-context=",
            "--before-context=",
            "--regexp=",
            "--file=",
            "--color=",
        ],
    )
}

fn pixel_read_or_recovery(args: &[String]) -> bool {
    let Some((operation, rest)) = args.split_first() else {
        return false;
    };
    match operation.as_str() {
        "config" => {
            rest.is_empty()
                || rest.len() == 1 && matches!(rest[0].as_str(), "metrics" | "--help" | "-h")
        }
        "task-state" => rest.first().is_some_and(|operation| {
            matches!(
                operation.as_str(),
                "begin"
                    | "contract"
                    | "prepare"
                    | "verify"
                    | "review"
                    | "finish"
                    | "route"
                    | "cancel"
                    | "recover"
                    | "status"
                    | "events"
                    | "replay"
                    | "--help"
                    | "-h"
            )
        }),
        // A plugin's read commands stay reads: `pixel repo-state` resolves to
        // the git plugin when it is installed, and to a hint when it is not,
        // and neither writes. Which words and flags of them do write is the
        // plugin table's to say.
        name if pixel_plugin::read_only_invocation(name, rest) => true,
        _ => matches!(
            operation.as_str(),
            "status"
                | "doctor"
                | "build-index"
                | "scope-task"
                | "find-code"
                | "find-symbol"
                | "search-content"
                | "search-meaning"
                | "impact"
                | "pack-context"
                | "what-changed"
                | "list-areas"
                | "list-flows"
                | "who-calls"
                | "capabilities"
                | "--help"
                | "--version"
        ),
    }
}

/// Single-quoted input is literal, including inline contract JSON. Other
/// expansions, escapes, redirections and unsupported shell syntax stay gated.
fn task_argv(command: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    for character in command.chars() {
        if character == '\0' {
            return None;
        }
        if quote == Some('\'') {
            if character == '\'' {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        if matches!(character, '\n' | '\r' | '\\' | '$' | '`') {
            return None;
        }
        match quote {
            Some(mark) if character == mark => quote = None,
            Some(_) => current.push(character),
            None if matches!(character, '\'' | '"') => {
                quote = Some(character);
                started = true;
            }
            None if ";|&<>()*?[]{}~#".contains(character) => return None,
            None if matches!(character, ' ' | '\t') => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None if character.is_whitespace() => return None,
            None => {
                current.push(character);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        args.push(current);
    }
    Some(args)
}

fn sort_read(args: &[String]) -> bool {
    read_flags(
        args,
        &[
            "-r",
            "-n",
            "-u",
            "-f",
            "-b",
            "-d",
            "-g",
            "-h",
            "-M",
            "-V",
            "-s",
            "-z",
            "-c",
            "-C",
            "--reverse",
            "--numeric-sort",
            "--unique",
            "--ignore-case",
            "--stable",
            "--check",
            "--zero-terminated",
        ],
        &["--key=", "--field-separator="],
    )
}

fn uniq_read(args: &[String]) -> bool {
    let end = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let operands = args[..end]
        .iter()
        .filter(|arg| !arg.starts_with('-'))
        .count()
        + args.len().saturating_sub(end + 1);
    operands <= 1
        && read_flags(
            args,
            &[
                "-c",
                "-d",
                "-u",
                "-i",
                "-z",
                "--count",
                "--repeated",
                "--unique",
                "--ignore-case",
                "--zero-terminated",
            ],
            &["--skip-fields=", "--skip-chars=", "--check-chars="],
        )
}

/// `sed` reads when it never writes: no `-i`/`--in-place` in any spelling,
/// only the read-oriented flags the bounded-read route prescribes (`-n`,
/// `-e <script>`, `--expression <script>`). Script files supplied with
/// `-f`/`--file` are rejected outright: their contents cannot be inspected,
/// so a script file carrying `e …` or `w …` would otherwise slip through as
/// a read. An unknown flag or combined form like `-ni` stays a mutation, so
/// a destructive sed can never slip through as a read. Only the script
/// operands are scanned — file operands (`events.rs`, `worker.rs`) are not
/// scripts, so a bounded read like `sed -n '1,20p' events.rs` stays a read.
fn sed_read(args: &[String]) -> bool {
    read_flags(
        args,
        &[
            "-n",
            "-N",
            "-s",
            "-z",
            "-u",
            "-l",
            "-E",
            "-r",
            "--silent",
            "--quiet",
            "--regexp-extended",
            "--separate",
            "--null-data",
            "--unbuffered",
            "--line-length",
            "--posix",
            "--sandbox",
            "--debug",
            "--help",
            "--version",
        ],
        &["-e", "--expression", "--line-length="],
    ) && !sed_scripts(args).any(sed_script_writes)
}

/// The script texts sed will run: every `-e`/`--expression` value (including
/// the `--expression=<script>` and merged `-e<script>` forms), or, when none
/// is given, the first positional operand. File operands are not scripts —
/// sed decides which operand is the script, and the rest are input files.
/// `--line-length`/`-l` consume a separate value argument so it is not
/// mistaken for a positional — but only a number: BSD sed's `-l` takes no
/// value, so `sed -l 'w out' f` runs `w out` there, and a non-numeric operand
/// stays a script candidate. `--` ends the options only: the first operand
/// after it is still the script when none came before. A positional that
/// precedes every `-e` is scanned as well: BSD sed (and GNU sed under
/// `POSIXLY_CORRECT`) stops option parsing at the first operand, so
/// `sed 'w out' -e p f` runs `w out` there.
fn sed_scripts(args: &[String]) -> impl Iterator<Item = &str> {
    let mut scripts: Vec<&str> = Vec::new();
    let mut has_explicit = false;
    let mut positional_before_explicit = false;
    let mut first_positional: Option<&str> = None;
    let mut after_dd = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if after_dd {
            // `--` ends the options, not the script search: with no `-e` and
            // no earlier positional, GNU sed runs the first operand after it.
            if first_positional.is_none() {
                first_positional = Some(arg.as_str());
            }
            continue;
        }
        if arg == "--" {
            after_dd = true;
        } else if arg == "-e" || arg == "--expression" {
            has_explicit = true;
            if let Some(script) = iter.next() {
                scripts.push(script.as_str());
            }
        } else if let Some(rest) = arg.strip_prefix("--expression=") {
            has_explicit = true;
            scripts.push(rest);
        } else if let Some(rest) = arg.strip_prefix("-e") {
            if !rest.is_empty() {
                has_explicit = true;
                scripts.push(rest);
            }
        } else if arg == "-l" || arg == "--line-length" {
            if iter
                .as_slice()
                .first()
                .is_some_and(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
            {
                iter.next();
            }
        } else if !arg.starts_with('-') && first_positional.is_none() {
            first_positional = Some(arg.as_str());
            positional_before_explicit = !has_explicit;
        }
    }
    if (!has_explicit || positional_before_explicit)
        && let Some(script) = first_positional
    {
        scripts.push(script);
    }
    scripts.into_iter()
}

/// A sed script writes or executes when it carries an `e` command (execute the
/// shell), a `w`/`W` command (write the pattern space to a file), or a
/// substitution with an `e`/`w`/`W` flag. GNU sed takes the rest of the
/// command line as the `e` command or the `w`/`W` filename, so `east`,
/// `west` and `write` are writes too — recognition is by command and flag,
/// never by a following word boundary. Addresses (numbers, `$`, `/re/`,
/// `\cre`, including ranges) may precede the command letter, substitutions
/// may use any delimiter, and commands may be separated by `;`, braces or
/// newlines, so the script is tokenised rather than split on a fixed set of
/// characters.
fn sed_script_writes(script: &str) -> bool {
    let mut chars = script.chars().peekable();
    loop {
        if !skip_separators(&mut chars) || !skip_addresses(&mut chars) {
            return false;
        }
        match chars.peek().copied() {
            None => return false,
            Some('e' | 'w' | 'W') => return true,
            Some('s') => {
                chars.next();
                if substitution_writes(&mut chars) {
                    return true;
                }
            }
            Some(_) => {}
        }
        // The remainder of this command is argument text, not more commands.
        for c in chars.by_ref() {
            if matches!(c, ';' | '{' | '}' | '\n') {
                break;
            }
        }
    }
}

/// Consume command separators, whitespace and comments. Returns false only
/// when the script ends there, so the caller can stop scanning.
fn skip_separators(chars: &mut Peekable<Chars<'_>>) -> bool {
    loop {
        match chars.peek().copied() {
            None => return false,
            Some(';' | '{' | '}') => {
                chars.next();
            }
            Some('#') => {
                chars.next();
                for c in chars.by_ref() {
                    if matches!(c, '\n' | '\r') {
                        break;
                    }
                }
            }
            Some(c) if c.is_ascii_whitespace() => {
                chars.next();
            }
            Some(_) => return true,
        }
    }
}

/// Consume the address(es) that may precede a command letter: line numbers,
/// `$`, the comma of a range, `/re/` and `\cre`. GNU sed also accepts
/// whitespace, negation (`!`), step/offset range syntax (`~`, `+`, `/` after
/// a comma) and regex address flags (`I`, `M`) between the address and the
/// command, so those are consumed here too. Returns false only when the
/// script ends there, so the caller can stop scanning.
fn skip_addresses(chars: &mut Peekable<Chars<'_>>) -> bool {
    loop {
        match chars.peek().copied() {
            None => return false,
            Some('0'..='9' | '$' | ',') => {
                chars.next();
            }
            Some('/') => {
                chars.next();
                skip_delimited(chars, '/');
                // Regex address flags: /pat/I, /pat/M
                skip_regex_flags(chars);
            }
            Some('\\') => {
                chars.next();
                let Some(delim) = chars.next() else {
                    return false;
                };
                skip_delimited(chars, delim);
                skip_regex_flags(chars);
            }
            // Step/offset range syntax: 1~2, 1,~4, 1,+2, 1,/pat/
            Some('~' | '+') => {
                chars.next();
            }
            // Negation: 1!w out, $!e cmd
            Some('!') => {
                chars.next();
            }
            // Whitespace between address and command: 1 w out
            Some(c) if c.is_ascii_whitespace() => {
                chars.next();
            }
            Some(_) => return true,
        }
    }
}

/// Consume regex address flags (`I`, `M`) that may follow a `/re/` or `\cre`
/// address. GNU sed accepts them in any combination and order.
fn skip_regex_flags(chars: &mut Peekable<Chars<'_>>) {
    while matches!(chars.peek(), Some('I' | 'M')) {
        chars.next();
    }
}

/// Consume through the next unescaped `delim`, honouring backslash escapes,
/// so a `\c` delimiter or an escaped delimiter inside a regexp does not end
/// the scan early.
fn skip_delimited(chars: &mut Peekable<Chars<'_>>, delim: char) {
    while let Some(next) = chars.next() {
        match next {
            '\\' => {
                chars.next();
            }
            _ if next == delim => break,
            _ => {}
        }
    }
}

/// Scan a substitution `s<delim>regexp<delim>replacement<delim>flags` whose
/// `s` has already been consumed. Returns true when the trailing flags carry
/// an `e` (execute) or `w`/`W` (write to a file) flag; GNU sed accepts them in
/// any flag position and takes the text after `w` as its filename, so
/// `s/a/b/e`, `s/.*/touch marker/ep`, `s/a/id/e;p` and `s/a/b/w out` are
/// writes alike.
fn substitution_writes(chars: &mut Peekable<Chars<'_>>) -> bool {
    // The delimiter is whatever follows the `s`; `\c` is also accepted.
    let Some(leading) = chars.next() else {
        return false;
    };
    let delim = if leading == '\\' {
        let Some(delim) = chars.next() else {
            return false;
        };
        delim
    } else {
        leading
    };
    // Skip the regular expression and the replacement, each through its
    // closing delimiter, honouring escaped delimiters inside them.
    skip_delimited(chars, delim);
    skip_delimited(chars, delim);
    // Any remaining text before a command boundary is the flag list; a
    // boundary is left in place so the next command still starts there.
    while let Some(&flag) = chars.peek() {
        match flag {
            'e' | 'w' | 'W' => return true,
            ';' | '{' | '}' | '\n' | '\r' => break,
            _ => {
                chars.next();
            }
        }
    }
    false
}

/// Environment markers set by a harness that loads Claude Code's configuration
/// rather than being Claude Code. Devin reads `~/.claude/settings.json` by
/// default (its `read_config_from.claude`) and runs the hook commands it finds
/// there unchanged, so `--provider claude` on a hook names the install, not
/// the host that invoked it. Such an entry must not act on Claude's behalf:
/// the host's own protocol carries the behavior, and the imported copy
/// double-runs beside it.
const IMPORTED_CLAUDE_CONFIG_MARKERS: &[&str] = &["DEVIN_PROJECT_DIR"];

/// The imported-config marker set in this process, if any.
pub(crate) fn imported_config_host() -> Option<&'static str> {
    IMPORTED_CLAUDE_CONFIG_MARKERS
        .iter()
        .copied()
        .find(|marker| std::env::var_os(marker).is_some())
}

/// Split only unquoted pipeline/sequence operators, retaining stdin provenance.
/// `None` when a quote is unbalanced or a segment is empty, which the caller
/// treats as a mutating command.
fn split_segments(text: &str) -> Option<Vec<(&str, bool)>> {
    let mut segments = Vec::new();
    let mut quote = None;
    let mut start = 0;
    let mut piped = false;
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if matches!(c, '\'' | '"') => quote = Some(c),
            None if matches!(c, '|' | ';' | '&' | '\n') => {
                let segment = text[start..index].trim();
                if segment.is_empty() {
                    return None;
                }
                segments.push((segment, piped));
                let doubled =
                    matches!(c, '|' | '&') && chars.peek().is_some_and(|(_, next)| *next == c);
                let end = if doubled { chars.next()?.0 } else { index };
                piped = c == '|' && !doubled;
                start = end + c.len_utf8();
            }
            None => {}
        }
    }
    if quote.is_some() || text[start..].trim().is_empty() {
        return None;
    }
    segments.push((text[start..].trim(), piped));
    Some(segments)
}

/// Proven reads, alone, piped or sequenced, and single-command recovery bypass
/// the edit gate. Every leaf of a sequence is judged, so `a; b` reads only
/// when both do; recovery stays single-command.
fn shell_mutates(command: &str) -> bool {
    let Some(segments) = split_segments(command) else {
        return true;
    };
    segments
        .iter()
        .any(|(segment, _)| shell_leaf_mutates(segment, segments.len() == 1))
}

fn shell_leaf_mutates(command: &str, recovery: bool) -> bool {
    // Discarding diagnostics writes nothing; any other redirection stays gated.
    let command = command.strip_suffix(" 2>/dev/null").unwrap_or(command);
    let Some(words) = task_argv(command) else {
        return true;
    };
    let mut words = words.as_slice();
    if words.first().is_some_and(|s| basename(s) == "rtk") {
        words = &words[1..];
        if words.first().is_some_and(|s| s == "proxy") {
            words = &words[1..];
        }
    }
    let Some((program, args)) = words.split_first() else {
        return false;
    };
    match basename(program) {
        "pixel" | "pixel-dev" => {
            !pixel_read_or_recovery(args)
                || !recovery
                    && args.first().is_some_and(|operation| {
                        matches!(operation.as_str(), "task-state" | "doctor" | "build-index")
                    })
        }
        "git" => !git_read(args),
        "rg" | "grep" => !search_read(args),
        "sed" => !sed_read(args),
        "sort" => !sort_read(args),
        "uniq" => !uniq_read(args),
        "pwd" | "true" | "false" | "cat" | "head" | "tail" | "wc" | "ls" | "read" | "cd" | "nl"
        | "echo" => false,
        _ => true,
    }
}

fn mutation(tool: &str, input: &Value) -> bool {
    match tool {
        "Bash" | "bash" | "shell" | "local_shell" | "unified_exec" | "exec_command" => {
            string(input, &["command", "cmd"]).is_none_or(shell_mutates)
        }
        "pixel" | "pixel_project" => !matches!(
            string(input, &["action"]),
            Some(
                "scope_task"
                    | "list_areas"
                    | "search_content"
                    | "find_code"
                    | "impact"
                    | "pack_context"
                    | "what_changed"
                    | "review_changes"
            )
        ),
        "Read" | "read" | "Glob" | "glob" | "Grep" | "grep" | "WebSearch" | "web_search"
        | "WebFetch" | "web_fetch" | "AskUserQuestion" | "ls" | "find" | "list_dir"
        | "grep_search" | "file_search" | "view_file" => false,
        // Claude's delegation and bookkeeping tools change no file themselves;
        // a subagent's own tool calls reach this gate with the parent session.
        "Agent" | "Task" | "TodoWrite" | "ToolSearch" => false,
        _ => true,
    }
}

/// Normalized inputs contain no raw tool arguments, output, or user shell text.
///
/// The bridge receives nullable real host IDs (`event_id`, `session_id`,
/// `turn_id`, `tool_use_id`, `agent_id`, `parent_tool_use_id`), `branch_id`,
/// `tool_name`, `mutation`, `cancelled`, nullable `success`, `input_digest`,
/// `changed_paths`, `coverage`, typed `usage`/`duration_ms`, `response_id`,
/// safe `request_tools` labels, and the objective only on prompt-submit.
/// A missing host ID is never replaced with a fabricated call identity.
pub(crate) fn normalize(event: TaskHookEvent, payload: &Value) -> Value {
    let input = payload
        .get("tool_input")
        .or_else(|| payload.get("input"))
        .unwrap_or(&Value::Null);
    let tool = string(payload, &["tool_name", "toolName"]).unwrap_or("");
    let session = string(payload, &["session_id", "sessionId"]);
    let turn = string(payload, &["turn_id", "turnId"]);
    let call = string(payload, &["tool_use_id", "toolCallId"]);
    let explicit_event = string(payload, &["event_id", "eventId", "uuid"]);
    let event_id = explicit_event.map(ToString::to_string).or_else(|| {
        Some(format!(
            "{}:{}:{}:{}",
            session?,
            turn.unwrap_or(""),
            event.as_str(),
            call?
        ))
    });
    let cancelled = event == TaskHookEvent::Interrupt
        || payload.get("cancelled").and_then(Value::as_bool) == Some(true)
        || payload.get("is_interrupt").and_then(Value::as_bool) == Some(true)
        || matches!(
            string(payload, &["stop_reason", "stopReason"]),
            Some("aborted" | "error")
        );
    let success = match event {
        TaskHookEvent::ToolFailure => Some(false),
        TaskHookEvent::PostToolUse => {
            payload.get("success").and_then(Value::as_bool).or_else(|| {
                payload
                    .get("isError")
                    .and_then(Value::as_bool)
                    .map(|error| !error)
            })
        }
        _ => None,
    };
    let paths: Vec<&str> = string(input, &["file_path", "path", "notebook_path"])
        .into_iter()
        .collect();
    let request_ids: Vec<&str> = payload
        .get("request_ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|id| !id.is_empty())
        .collect();
    let coverage_complete = event == TaskHookEvent::ModelResponse
        && payload.get("coverage_complete").and_then(Value::as_bool) == Some(true)
        && payload
            .get("request_ids")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.len() == request_ids.len());
    let request_tools = request_ids
        .iter()
        .filter_map(|id| {
            let label = tool_label(payload["request_tools"][*id].as_str()?)?;
            Some(((*id).to_string(), Value::from(label)))
        })
        .collect::<serde_json::Map<_, _>>();
    json!({
        "event_id": event_id,
        "session_id": session,
        "turn_id": turn,
        "tool_use_id": call,
        "agent_id": string(payload, &["agent_id", "agentId"]),
        "parent_tool_use_id": string(payload, &["parent_tool_use_id", "parentToolUseId"]),
        "branch_id": string(payload, &["branch_id", "branchId"]),
        "task_id": string(payload, &["task_id"]),
        "attempt_id": string(payload, &["attempt_id"]),
        "binding_session_id": string(payload, &["binding_session_id"]),
        "branch_unbound": payload.get("branch_unbound").and_then(Value::as_bool) == Some(true),
        "parent_span_id": string(payload, &["parent_span_id"]),
        "prompt": if event == TaskHookEvent::PromptSubmit { string(payload, &["prompt"]) } else { None },
        "tool_name": tool_label(tool).unwrap_or(""),
        "mutation": mutation(tool, input),
        "cancelled": cancelled,
        "success": success,
        "input_digest": format!("{:016x}", xxhash_rust::xxh3::xxh3_64(input.to_string().as_bytes())),
        "changed_paths": paths,
        "request_ids": request_ids,
        "request_tools": request_tools,
        "response_id": string(payload, &["response_id"]).filter(|id| id.len() <= 512 && !id.chars().any(char::is_control)),
        "usage": usage(payload),
        "duration_ms": bounded_count(&payload["duration_ms"], 604_800_000),
        "coverage_complete": coverage_complete,
        "coverage": { "native_hooks": true, "model_requests_complete": coverage_complete, "call_identity": call.is_some() },
    })
}

fn envelope(provider: TaskProvider, event: TaskHookEvent, decision: &Value) -> Value {
    if provider == TaskProvider::Pi {
        return decision.clone();
    }
    let action = decision
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or("observe");
    let reason = decision
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or(UNAVAILABLE);
    match (event, action) {
        (TaskHookEvent::PreToolUse, "deny") => json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason
        }}),
        (TaskHookEvent::Stop | TaskHookEvent::SubagentStop, "continue") => {
            json!({"decision": "block", "reason": reason})
        }
        (TaskHookEvent::Stop | TaskHookEvent::SubagentStop, "deny") => {
            json!({"continue": false, "stopReason": reason})
        }
        (_, _) if decision.get("context").and_then(Value::as_str).is_some() => {
            // Antigravity injects steps into the trajectory, not a
            // hookSpecificOutput block; every other provider shares the
            // Claude contract, with Gemini naming the event BeforeAgent.
            if provider == TaskProvider::Antigravity && event == TaskHookEvent::PromptSubmit {
                return json!({"injectSteps": [{"ephemeralMessage": decision["context"]}]});
            }
            let event_name = if event == TaskHookEvent::PromptSubmit {
                provider.prompt_event_name()
            } else {
                event.host_name()
            };
            json!({"hookSpecificOutput": {
                "hookEventName": event_name, "additionalContext": decision["context"]
            }})
        }
        _ => json!({}),
    }
}

/// The decision when the ledger cannot answer. It never blocks more than an
/// answering ledger would: an unenforced session only observes, as in
/// `task_bridge::handle_hook`; an enforced one keeps edits and completion gated.
fn unavailable(event: TaskHookEvent, payload: Option<&Value>, enforced: bool) -> Value {
    let blocks = enforced
        && match event {
            TaskHookEvent::PreToolUse => payload.is_none_or(|p| {
                let normalized = normalize(event, p);
                normalized["mutation"] == true
            }),
            TaskHookEvent::Stop | TaskHookEvent::SubagentStop => true,
            _ => false,
        };
    json!({"decision": if blocks { "deny" } else { "observe" }, "reason": UNAVAILABLE, "coverage": "unavailable"})
}

fn payload_cwd(provider: TaskProvider, payload: &Value) -> std::path::PathBuf {
    if provider == TaskProvider::Antigravity {
        let cwd = string(payload, &["cwd"]).filter(|path| !path.is_empty());
        if let Some(root) = cwd.and_then(indexed_antigravity_root) {
            return root;
        }
        if let Some(root) = payload["workspacePaths"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|path| !path.is_empty())
            .find_map(indexed_antigravity_root)
        {
            return root;
        }
        if let Some(workspace) = payload["workspacePaths"]
            .as_array()
            .and_then(|paths| paths.first())
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
        {
            return std::path::PathBuf::from(workspace);
        }
    }
    string(payload, &["cwd"]).map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        std::path::PathBuf::from,
    )
}

/// Antigravity can report several workspaces; prefer one Pixel can brief.
fn indexed_antigravity_root(path: &str) -> Option<std::path::PathBuf> {
    let root = crate::discover_root(Path::new(path)).ok()?;
    crate::execution_brief::chain::Gate::read(&root)
        .indexed
        .then_some(root)
}

/// Whether the unanswered decision would have been enforced. Unreadable input
/// and an undiscoverable repository count as enforced.
fn enforcement_applies(provider: TaskProvider, payload: Option<&Value>) -> bool {
    let Some(payload) = payload else {
        return true;
    };
    crate::discover_root(&payload_cwd(provider, payload))
        .ok()
        .is_none_or(|root| {
            crate::task_bridge::fallback_enforced(
                &root,
                provider.as_str(),
                string(payload, &["session_id", "sessionId"]),
            )
        })
}

fn process(
    provider: TaskProvider,
    event: TaskHookEvent,
    raw: &str,
    part: Option<BriefPart>,
) -> Value {
    let Ok(payload) = serde_json::from_str::<Value>(raw) else {
        return envelope(provider, event, &unavailable(event, None, true));
    };
    let first_of_many = part
        .filter(|part| part.index == 1 && part.count > 1 && event == TaskHookEvent::PromptSubmit);
    // The brief runs beside the ledger: it needs neither, and a slow ledger
    // must not use up its window.
    let parts = brief_budget(provider, &payload, part);
    let brief = start_brief(provider, event, &payload, parts);
    forget_sent(provider, event, &payload);
    if let Some(root) = session_start_root(provider, event, &payload) {
        // A start is a nicety: whatever it came to, the hook goes on.
        let _ = crate::execution_brief::autostart::warm(&root);
    }
    let ledger = || {
        // Pixel's Pi package asked for the brief alone: no task is bound and
        // nothing is gated, so the ledger has nothing to answer.
        if brief_only(provider, &payload) {
            return json!({"decision": "observe"});
        }
        handle_at(&payload_cwd(provider, &payload), provider, event, &payload).unwrap_or_else(
            |_| {
                unavailable(
                    event,
                    Some(&payload),
                    enforcement_applies(provider, Some(&payload)),
                )
            },
        )
    };
    let (decision, text) = if let Some(part) = first_of_many {
        // The other parts are waiting for the brief: publish it as soon as it
        // is finished, not after the ledger has answered too.
        std::thread::scope(|scope| {
            let published = scope.spawn(|| publish_first_part(provider, &payload, brief, part));
            let decision = ledger();
            (decision, published.join().unwrap_or(None))
        })
    } else {
        let decision = ledger();
        (decision, finish_brief(provider, event, &payload, brief))
    };
    envelope(provider, event, &with_context(decision, text))
}

/// Hook-sized parts the brief of this event may fill. A hook serving slice
/// K of N gets N. Without `--part` the one hook prints the whole brief: one
/// part, except for Pixel's Pi package ([`BRIEF_ONLY`]), which has no per-hook
/// cap on a message and gets the configured `brief_parts` (repository layer,
/// then global, 3 by default), as `pixel brief --parts 3` gave it before.
fn brief_budget(provider: TaskProvider, payload: &Value, part: Option<BriefPart>) -> u8 {
    if let Some(part) = part {
        return part.count;
    }
    if !brief_only(provider, payload) {
        return 1;
    }
    let root = crate::discover_root(&payload_cwd(provider, payload)).ok();
    crate::config_cmd::brief_parts(root.as_deref())
}

/// Whether a prompt-submit `output` carries a brief, the condition for the
/// 🟩 `systemMessage`. Pi's envelope is the bare decision, so its brief is the
/// `context`, and only a [`BRIEF_ONLY`] payload counts: a ledger note of a Pi
/// host that drives the lifecycle itself is not a brief.
fn delivered_brief(provider: TaskProvider, raw: Option<&str>, output: &Value) -> bool {
    if provider == TaskProvider::Antigravity {
        return output.get("injectSteps").is_some();
    }
    if provider != TaskProvider::Pi {
        return output.get("hookSpecificOutput").is_some();
    }
    output.get("context").is_some_and(Value::is_string)
        && raw
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .is_some_and(|payload| brief_only(provider, &payload))
}

fn should_relay_metrics(
    provider: TaskProvider,
    event: TaskHookEvent,
    part: Option<BriefPart>,
    recovered_head: bool,
    raw: Option<&str>,
    output: &Value,
) -> bool {
    event == TaskHookEvent::PromptSubmit
        && (part.is_none_or(|part| part.index == 1) || recovered_head)
        && delivered_brief(provider, raw, output)
}

/// Where the first part stores the brief of this prompt and the others read
/// it, when the payload names a prompt and a repository.
fn part_path(provider: TaskProvider, payload: &Value) -> Option<std::path::PathBuf> {
    let prompt = brief_prompt(provider, TaskHookEvent::PromptSubmit, payload)?;
    let root = crate::discover_root(&payload_cwd(provider, payload)).ok()?;
    Some(brief_parts::cache_path(
        &root,
        brief_session(payload),
        &prompt,
    ))
}

/// What one part of a split brief may carry on `provider`: Codex measures an
/// injected context in bytes, the others in characters.
fn part_limits(provider: TaskProvider) -> Limits {
    if provider == TaskProvider::Codex {
        Limits::BYTES
    } else {
        Limits::CHARS
    }
}

/// The finished brief of a prompt-submit, grown to the size
/// `PIXEL_BRIEF_TEST_PAD_PARTS` asks for in a debug build.
#[cfg(debug_assertions)]
fn forced_oversize(provider: TaskProvider, text: Option<String>) -> Option<String> {
    let parts = std::env::var(brief_parts::TEST_PAD_ENV)
        .ok()
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|parts| (1..=pixel_setup::config::BRIEF_PARTS_MAX).contains(parts));
    match parts {
        Some(parts) => Some(brief_parts::pad_to_parts(
            &text.unwrap_or_default(),
            parts,
            part_limits(provider),
        )),
        None => text,
    }
}

/// A release build has no such variable.
#[cfg(not(debug_assertions))]
fn forced_oversize(_: TaskProvider, text: Option<String>) -> Option<String> {
    text
}

/// The brief text of `event`, once the worker has answered or the window is
/// out; `None` when the event carries no brief or nothing answered. A prompt
/// the brief saw is written to the read-feedback log either way.
fn finish_brief(
    provider: TaskProvider,
    event: TaskHookEvent,
    payload: &Value,
    brief: Option<crate::execution_brief::chain::Pending>,
) -> Option<String> {
    let finished = brief.map(crate::execution_brief::chain::Pending::finish_with_record);
    let Some(prompt) = brief_prompt(provider, event, payload) else {
        return finished.and_then(|finished| finished.text);
    };
    let text = forced_oversize(provider, finished.as_ref().and_then(|f| f.text.clone()));
    note_prompt(
        provider,
        payload,
        &prompt,
        text.as_deref(),
        finished.as_ref(),
    );
    text
}

/// Write the `prompt` line of the read-feedback log: what the brief handed
/// this prompt, joined to the decision log by the typed text's hash. Best
/// effort, and nothing without a session id to join the later tool calls by.
fn note_prompt(
    provider: TaskProvider,
    payload: &Value,
    prompt: &str,
    text: Option<&str>,
    finished: Option<&crate::execution_brief::chain::Finished>,
) {
    use crate::execution_brief::{decision_log, feedback};
    let Some(session) = brief_session(payload) else {
        return;
    };
    let Some(path) = crate::discover_root(&payload_cwd(provider, payload))
        .ok()
        .and_then(|root| feedback::path_for(&root))
    else {
        return;
    };
    let digest = decision_log::sha256_hex(prompt);
    let no_paths: &[String] = &[];
    let line = feedback::prompt_line(
        pixel_task::now_ms(),
        &feedback::Prompt {
            session,
            provider: provider.as_str(),
            prompt: &digest,
            decision: finished.map(|finished| finished.record.sha256.as_str()),
            gate: finished.map_or("none", |finished| finished.record.gate),
            kind: finished.and_then(|finished| finished.record.kind),
            injected: finished.map_or(no_paths, |finished| finished.paths.as_slice()),
            bytes: text.map_or(0, str::len),
        },
    );
    let _ = decision_log::append(&path, &line, feedback::MAX_LINES);
}

/// The bytes of brief a prompt-submit response carries: the
/// `hookSpecificOutput.additionalContext` of the wrapped envelopes, the
/// `ephemeralMessage` Antigravity's `injectSteps` carries, or the bare
/// decision's `context` Pi's package reads ([`BRIEF_ONLY`] payloads only, as
/// in [`delivered_brief`]: a ledger note is not a brief).
fn context_bytes(provider: TaskProvider, payload: &Value, output: &Value) -> usize {
    let text = match provider {
        TaskProvider::Pi if !brief_only(provider, payload) => return 0,
        TaskProvider::Pi => output["context"].as_str(),
        TaskProvider::Antigravity => output["injectSteps"][0]["ephemeralMessage"].as_str(),
        _ => output["hookSpecificOutput"]["additionalContext"].as_str(),
    };
    text.map_or(0, str::len)
}

/// Write the read-feedback line of a hook that has already answered the host:
/// what a prompt-submit part printed (`output`, `None` when it printed nothing
/// because the brief did not arrive), a tool call and the repository files it
/// named, or the stop that closes a turn. Best effort: nothing here can reach
/// the host.
fn note_event(
    provider: TaskProvider,
    event: TaskHookEvent,
    part: Option<BriefPart>,
    raw: Option<&str>,
    output: Option<&Value>,
) {
    use crate::execution_brief::{decision_log, feedback};
    if !matches!(
        event,
        TaskHookEvent::PromptSubmit
            | TaskHookEvent::PostToolUse
            | TaskHookEvent::ToolFailure
            | TaskHookEvent::Stop
    ) {
        return;
    }
    let Some(payload) = raw.and_then(|raw| serde_json::from_str::<Value>(raw).ok()) else {
        return;
    };
    let Some(session) = brief_session(&payload) else {
        return;
    };
    let cwd = payload_cwd(provider, &payload);
    let Ok(root) = crate::discover_root(&cwd) else {
        return;
    };
    let Some(path) = feedback::path_for(&root) else {
        return;
    };
    let ts = pixel_task::now_ms();
    let provider_name = provider.as_str();
    let _ = match event {
        TaskHookEvent::PromptSubmit => {
            let Some(prompt) = brief_prompt(provider, event, &payload) else {
                return;
            };
            let (index, count) = part.map_or((1, 1), |part| (part.index, part.count));
            let line = feedback::part_line(
                ts,
                session,
                provider_name,
                &decision_log::sha256_hex(&prompt),
                (index, count),
                (
                    output.map_or(0, |output| context_bytes(provider, &payload, output)),
                    output.is_none(),
                ),
            );
            decision_log::append(&path, &line, feedback::MAX_LINES)
        }
        TaskHookEvent::PostToolUse | TaskHookEvent::ToolFailure => {
            let tool = string(&payload, &["tool_name", "toolName"])
                .and_then(tool_label)
                .unwrap_or("");
            let input = payload
                .get("tool_input")
                .or_else(|| payload.get("input"))
                .unwrap_or(&Value::Null);
            let line = feedback::tool_line(
                ts,
                session,
                provider_name,
                tool,
                mutation(tool, input),
                &feedback::tool_targets(&root, &cwd, input),
            );
            decision_log::append(&path, &line, feedback::MAX_LINES)
        }
        _ => decision_log::append_with(&path, feedback::MAX_LINES, |existing| {
            feedback::stop_lines(existing, ts, session, provider_name)
        }),
    };
}

/// Whether the 🟩 prompt-submit line joins the response: the `metrics`
/// configuration for the payload's repository, vetoed by `PIXEL_METRICS=0`.
fn metrics_on(provider: TaskProvider, raw: Option<&str>) -> bool {
    if std::env::var_os("PIXEL_METRICS").is_some_and(|v| v == "0") {
        return false;
    }
    let root = raw
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|payload| crate::discover_root(&payload_cwd(provider, &payload)).ok());
    crate::config_cmd::metrics_enabled(root.as_deref())
}

/// Part 1 of a split brief: finish the brief, store it whole for the other
/// parts (an empty one too, so they do not wait for nothing) and return the
/// first slice for this hook to print.
fn publish_first_part(
    provider: TaskProvider,
    payload: &Value,
    brief: Option<crate::execution_brief::chain::Pending>,
    part: BriefPart,
) -> Option<String> {
    let text = finish_brief(provider, TaskHookEvent::PromptSubmit, payload, brief);
    if let Some(path) = part_path(provider, payload) {
        // Best effort: parts that find nothing print nothing.
        let _ = brief_parts::store(&path, text.as_deref().unwrap_or(""), SystemTime::now());
    }
    text.map(|text| brief_parts::slice(&text, part, part_limits(provider)).to_owned())
        .filter(|slice| !slice.is_empty())
}

/// How long part 1 waits, once its bound has passed, for the brief its worker
/// is storing. It is half the shared part wait slack.
const LATE_GRACE: Duration = Duration::from_millis(brief_parts::WAIT_SLACK.as_millis() as u64 / 2);
/// A quarter of the one-second part wait slack remains after late recovery so
/// later parts can observe the first part's done marker.
const ORDERING_SLACK: Duration =
    Duration::from_millis((brief_parts::WAIT_SLACK.as_millis() / 4) as u64);

/// How long a part after the first waits for the first part's brief: the
/// bound part 1 itself is given ([`decision_timeout`]) and a little more to
/// store, print and release it, so a part never prints before part 1 has
/// finished or given up. It stays under the host's deadline.
fn follow_bound(provider: TaskProvider) -> Duration {
    decision_timeout(provider) + LATE_GRACE + ORDERING_SLACK
}

/// Part 1 of a split brief whose worker missed the bound: the first slice of
/// the brief the worker stored meanwhile, waiting at most [`LATE_GRACE`] for it.
/// `None` for any other hook, or when no brief is stored in time.
fn late_first_part(
    provider: TaskProvider,
    event: TaskHookEvent,
    raw: &str,
    part: Option<BriefPart>,
) -> Option<Value> {
    let part = part.filter(|part| part.index == 1 && part.count > 1)?;
    if event != TaskHookEvent::PromptSubmit {
        return None;
    }
    let payload = serde_json::from_str::<Value>(raw).ok()?;
    let path = part_path(provider, &payload)?;
    let text = brief_parts::wait_stored(&path, LATE_GRACE, brief_parts::POLL)?;
    let slice = brief_parts::slice(&text, part, part_limits(provider));
    (!slice.is_empty()).then(|| {
        envelope(
            provider,
            event,
            &json!({"decision": "observe", "context": slice}),
        )
    })
}

/// Clear a prior identical prompt's split cache before starting part 1's
/// bounded worker, so a timeout cannot recover stale context.
fn forget_first_part_cache(
    provider: TaskProvider,
    event: TaskHookEvent,
    raw: &str,
    part: Option<BriefPart>,
) {
    if !part.is_some_and(|part| part.index == 1 && part.count > 1)
        || event != TaskHookEvent::PromptSubmit
    {
        return;
    }
    let Ok(payload) = serde_json::from_str::<Value>(raw) else {
        return;
    };
    if let Some(path) = part_path(provider, &payload) {
        brief_parts::forget(&path);
    }
}

fn context_in_output(provider: TaskProvider, output: &Value) -> Option<&str> {
    match provider {
        TaskProvider::Pi => output.get("context")?.as_str(),
        TaskProvider::Antigravity => output
            .get("injectSteps")?
            .as_array()?
            .first()?
            .get("ephemeralMessage")?
            .as_str(),
        _ => output
            .get("hookSpecificOutput")?
            .get("additionalContext")?
            .as_str(),
    }
}

fn delivered_first_slice(
    provider: TaskProvider,
    raw: &str,
    part: BriefPart,
    output: &Value,
) -> bool {
    if part.index != 1 || part.count < 2 {
        return false;
    }
    let Some(context) = context_in_output(provider, output) else {
        return false;
    };
    let Some(payload) = serde_json::from_str::<Value>(raw).ok() else {
        return false;
    };
    let Some(path) = part_path(provider, &payload) else {
        return false;
    };
    let Some(text) = brief_parts::wait_stored(&path, Duration::ZERO, brief_parts::POLL) else {
        return false;
    };
    context == brief_parts::slice(&text, part, part_limits(provider)) && !context.is_empty()
}

/// A part after the first prints its stored slice, or supplies the head when
/// part 1 explicitly finished without delivering it. The boolean is true when
/// the returned output contains that recovered head.
fn follow_with_status(
    provider: TaskProvider,
    raw: &str,
    part: BriefPart,
    bound: Duration,
) -> Option<(Value, bool)> {
    let payload = serde_json::from_str::<Value>(raw).ok()?;
    let path = part_path(provider, &payload)?;
    let stored = brief_parts::wait_for_with_status(&path, bound, brief_parts::POLL)?;
    let recovered_head = stored.missing_head;
    let index = if recovered_head {
        part.index.checked_sub(1)?
    } else {
        part.index
    };
    let slice_part = BriefPart {
        index,
        count: part.count,
    };
    let slice = brief_parts::slice(&stored.text, slice_part, part_limits(provider));
    let delivered_head = recovered_head && index == 1 && !slice.is_empty();
    let output = if slice.is_empty() {
        json!({})
    } else {
        envelope(
            provider,
            TaskHookEvent::PromptSubmit,
            &json!({"decision": "observe", "context": slice}),
        )
    };
    Some((output, delivered_head))
}

/// A part after the first: print its stored slice, or supply the head when
/// part 1 explicitly finished without delivering it.
#[cfg(test)]
fn follow(provider: TaskProvider, raw: &str, part: BriefPart, bound: Duration) -> Option<Value> {
    follow_with_status(provider, raw, part, bound).map(|(output, _)| output)
}

/// Whether `payload` is Pixel's Pi package asking for the brief alone
/// ([`BRIEF_ONLY`]). Never true for another provider.
fn brief_only(provider: TaskProvider, payload: &Value) -> bool {
    provider == TaskProvider::Pi && payload.get(BRIEF_ONLY).and_then(Value::as_bool) == Some(true)
}

/// Whether `provider` takes the evidence brief from this payload: every
/// prompt-hook host, and Pi only when its package asked for the brief alone.
fn briefed(provider: TaskProvider, payload: &Value) -> bool {
    provider != TaskProvider::Pi || brief_only(provider, payload)
}

fn brief_prompt(provider: TaskProvider, event: TaskHookEvent, payload: &Value) -> Option<String> {
    if !briefed(provider, payload) || event != TaskHookEvent::PromptSubmit {
        return None;
    }
    match provider {
        TaskProvider::Antigravity => {
            if payload.get("invocationNum").and_then(Value::as_u64) != Some(0) {
                return None;
            }
            antigravity_prompt(payload)
        }
        _ => string(payload, &["prompt"]).map(str::to_string),
    }
}

/// A session the host starts over (`SessionStart` with source `compact` or
/// `clear`) no longer holds what the packs sent it: forget what was sent, so
/// the next pack carries it again. A `resume` keeps the same context, so it
/// keeps the record too.
fn forget_sent(provider: TaskProvider, event: TaskHookEvent, payload: &Value) {
    if event != TaskHookEvent::SessionStart
        || !matches!(string(payload, &["source"]), Some("compact" | "clear"))
    {
        return;
    }
    let Some(session) = brief_session(payload) else {
        return;
    };
    if let Ok(root) = crate::discover_root(&payload_cwd(provider, payload)) {
        crate::execution_brief::sent::reset(&root, session);
    }
}

/// The repository whose daemon a session starting should keep warm for the
/// brief: the briefed providers' `SessionStart`, in a discoverable repository.
fn session_start_root(
    provider: TaskProvider,
    event: TaskHookEvent,
    payload: &Value,
) -> Option<std::path::PathBuf> {
    if !briefed(provider, payload) || event != TaskHookEvent::SessionStart {
        return None;
    }
    crate::discover_root(&payload_cwd(provider, payload)).ok()
}

/// Start the evidence brief for a prompt of a briefed host (Pi only when its
/// package sent [`BRIEF_ONLY`]); every other event and a prompt outside a
/// repository start none.
fn start_brief(
    provider: TaskProvider,
    event: TaskHookEvent,
    payload: &Value,
    parts: u8,
) -> Option<crate::execution_brief::chain::Pending> {
    let prompt = brief_prompt(provider, event, payload)?;
    let root = crate::discover_root(&payload_cwd(provider, payload)).ok()?;
    // The files the session is working on: its transcript's recent tool
    // calls and the dirty files of the tree. Only the path is taken here; the
    // transcript is read, for paths alone, inside the brief's own window.
    let session = crate::execution_brief::chain::Session {
        id: brief_session(payload).map(str::to_string),
        transcript: string(payload, &["transcript_path", "transcriptPath"])
            .map(std::path::PathBuf::from),
        until: crate::hook_input::answer_by(),
    };
    crate::execution_brief::chain::start_in(&prompt, &root, parts, session)
}

/// The latest user turn in the transcript `transcriptPath` points at. The
/// file is JSONL; a turn is a user-role entry or an explicit Antigravity
/// `USER_INPUT`, with text in `content`/`text` or a text-parts array.
/// Tolerant on purpose: a transcript shape the reader cannot parse yields
/// no brief, never a hook failure.
fn antigravity_prompt(payload: &Value) -> Option<String> {
    let path = string(payload, &["transcriptPath", "transcript_path"])?;
    let text = std::fs::read_to_string(path).ok()?;
    let mut last: Option<String> = None;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let role = entry["role"].as_str().or_else(|| entry["type"].as_str());
        let explicit_user_input = entry["type"].as_str() == Some("USER_INPUT")
            && entry["source"].as_str() == Some("USER_EXPLICIT");
        if !explicit_user_input && role != Some("user") && role != Some("human") {
            continue;
        }
        let content = entry["content"].as_str().map_or_else(
            || {
                entry["text"].as_str().map(str::to_string).or_else(|| {
                    entry["content"].as_array().map(|parts| {
                        parts
                            .iter()
                            .filter_map(|part| part["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                })
            },
            |text| Some(text.to_string()),
        );
        if let Some(content) = content
            .map(|text| antigravity_user_request(&text).to_string())
            .filter(|text| !text.trim().is_empty())
        {
            last = Some(content);
        }
    }
    last
}

/// Devin and Antigravity draw neither `systemMessage` nor a hook's output
/// (#1031), so the prompt-submit metrics line reaches their user only when
/// the model repeats it: the brief (Devin's `additionalContext`,
/// Antigravity's injected `ephemeralMessage`) ends asking for that.
fn ask_model_to_relay(output: &mut Value, line: &str) {
    let pointer = if output.get("injectSteps").is_some() {
        "/injectSteps/0/ephemeralMessage"
    } else {
        "/hookSpecificOutput/additionalContext"
    };
    if let Some(context) = output.pointer_mut(pointer)
        && let Some(brief) = context.as_str()
    {
        *context = json!(format!(
            "{brief}\n\nEnd your answer with this line, verbatim: {line}"
        ));
    }
}

/// What the user typed in an Antigravity turn. Antigravity CLI 1.4 stores a
/// turn as `<USER_REQUEST>…</USER_REQUEST>`, then its own
/// `<ADDITIONAL_METADATA>` (the local time) and, after a model switch, a
/// `<USER_SETTINGS_CHANGE>` block; briefing the whole text looked up
/// `USER_REQUEST`, `Gemini` and the time as symbols. Text without the
/// wrapper is returned as is.
fn antigravity_user_request(text: &str) -> &str {
    text.trim_start()
        .strip_prefix("<USER_REQUEST>")
        .and_then(|rest| rest.split_once("</USER_REQUEST>"))
        .map_or(text, |(request, _)| request.trim())
}

/// `decision` with the brief as its `context`, which the envelope delivers as
/// the host's additional context. A decision that already carries a context
/// keeps it.
#[cfg(test)]
fn with_brief(decision: Value, brief: Option<crate::execution_brief::chain::Pending>) -> Value {
    if decision.get("context").is_some() {
        return decision;
    }
    with_context(
        decision,
        brief.and_then(crate::execution_brief::chain::Pending::finish),
    )
}

/// `decision` with `text` as its `context` unless it carries one already.
fn with_context(mut decision: Value, text: Option<String>) -> Value {
    if decision.get("context").is_none()
        && let Some(text) = text
    {
        decision["context"] = Value::String(text);
    }
    decision
}

fn handle_at(
    cwd: &Path,
    provider: TaskProvider,
    event: TaskHookEvent,
    payload: &Value,
) -> Result<Value, String> {
    let root = crate::discover_root(cwd).map_err(|e| e.to_string())?;
    crate::task_commands::handle_hook(
        &root,
        provider.as_str(),
        event.as_str(),
        &normalize(event, payload),
    )
}

/// The decision `evaluate` returns within `timeout`, or `None` when the worker
/// could not start or did not answer in time (it keeps running until `run()`
/// exits).
fn decision_within(
    timeout: Duration,
    evaluate: impl FnOnce() -> Value + Send + 'static,
) -> Option<Value> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("pixel-task-gate".into())
        .spawn(move || {
            let _ = sender.send(evaluate());
        })
        .ok()?;
    receiver.recv_timeout(timeout).ok()
}

/// Part 1's decision, falling back to the stored first slice after a timeout.
fn first_part_decision(
    provider: TaskProvider,
    event: TaskHookEvent,
    raw: &str,
    part: Option<BriefPart>,
    timeout: Duration,
    evaluate: impl FnOnce() -> Value + Send + 'static,
    enforced: impl FnOnce(Option<&Value>) -> bool,
) -> Value {
    forget_first_part_cache(provider, event, raw, part);
    decision_within(timeout, evaluate)
        .or_else(|| late_first_part(provider, event, raw, part))
        .unwrap_or_else(|| unavailable_envelope(provider, event, raw, enforced))
}

/// Write part 1's response before releasing the later parts waiting on it.
fn emit_first_part<W: Write>(
    writer: &mut W,
    output: Option<&Value>,
    path: Option<&Path>,
    delivered_head: bool,
) {
    if let Some(output) = output {
        let _ = writeln!(writer, "{output}");
        let _ = writer.flush();
    }
    if let Some(path) = path {
        if delivered_head {
            brief_parts::mark_done(path);
        } else {
            brief_parts::mark_done_missing_head(path);
        }
    }
}

fn bounded_decision(
    provider: TaskProvider,
    event: TaskHookEvent,
    raw: &str,
    timeout: Duration,
    evaluate: impl FnOnce() -> Value + Send + 'static,
    enforced: impl FnOnce(Option<&Value>) -> bool,
) -> Value {
    if let Some(decision) = decision_within(timeout, evaluate) {
        return decision;
    }
    unavailable_envelope(provider, event, raw, enforced)
}

/// The envelope of a decision that did not arrive in time.
fn unavailable_envelope(
    provider: TaskProvider,
    event: TaskHookEvent,
    raw: &str,
    enforced: impl FnOnce(Option<&Value>) -> bool,
) -> Value {
    let payload = serde_json::from_str(raw).ok();
    let enforced = enforced(payload.as_ref());
    envelope(
        provider,
        event,
        &unavailable(event, payload.as_ref(), enforced),
    )
}

/// Read one bounded host event, dispatch it, and emit only the host's schema.
///
/// `part` is the slice of a split prompt-submit brief this hook serves (see
/// [`brief_parts`]); on another event it means nothing and is ignored. The
/// process exits 0 afterwards, after a panic too
/// ([`crate::hook_input::exit_0_after`]).
pub fn run(provider: TaskProvider, event: TaskHookEvent, part: Option<BriefPart>) -> ! {
    crate::hook_input::exit_0_after(|| respond(provider, event, part))
}

/// The body of [`run`]: read, decide, print the one response, log the event.
fn respond(provider: TaskProvider, event: TaskHookEvent, part: Option<BriefPart>) {
    let started = std::time::Instant::now();
    if provider == TaskProvider::Claude && imported_config_host().is_some() {
        return;
    }
    let raw = crate::hook_input::read_hook_payload();
    let journal_payload = raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    crate::usage_journal::bind_hook(
        provider.as_str(),
        journal_payload.as_ref().and_then(brief_session),
    );
    crate::hook_input::test_panic("before-response");
    // One instant for every stage that works for this answer; what is left of
    // the decision window after it is what printing the answer needs.
    crate::hook_input::fix_answer_by(answer_by(provider, std::time::Instant::now()));
    let mut recovered_head = false;
    let mut output = if let Some(raw) = &raw {
        let later = part.filter(|part| part.index > 1 && event == TaskHookEvent::PromptSubmit);
        if let Some(part) = later {
            // Nothing to say is nothing printed: a hook that cannot get its
            // slice in time never holds the prompt.
            follow_with_status(provider, raw, part, follow_bound(provider)).map(
                |(output, recovered)| {
                    recovered_head = recovered;
                    output
                },
            )
        } else {
            let input = raw.clone();
            let evaluate = move || process(provider, event, &input, part);
            let enforced = |payload: Option<&Value>| enforcement_applies(provider, payload);
            if part.is_some_and(|part| part.index == 1 && part.count > 1)
                && event == TaskHookEvent::PromptSubmit
            {
                // Part 1 of a split brief that reached its bound just as the
                // worker stored the brief prints the first slice rather than
                // nothing, or the head of the brief and the 🟩 notice are
                // lost while parts 2 and 3 print the tail.
                Some(first_part_decision(
                    provider,
                    event,
                    raw,
                    part,
                    decision_timeout(provider),
                    evaluate,
                    enforced,
                ))
            } else {
                Some(bounded_decision(
                    provider,
                    event,
                    raw,
                    decision_timeout(provider),
                    evaluate,
                    enforced,
                ))
            }
        }
    } else {
        Some(envelope(provider, event, &unavailable(event, None, true)))
    };
    // A hook has no terminal: the metrics line a person would read on stderr
    // rides the response as `systemMessage`, which the host surfaces as a
    // warning next to the prompt. Only a delivered brief earns one — a `{}`
    // abstention should not light up every prompt.
    if event == TaskHookEvent::PromptSubmit
        && let Some(output) = &mut output
        && should_relay_metrics(
            provider,
            event,
            part,
            recovered_head,
            raw.as_deref(),
            output,
        )
        && metrics_on(provider, raw.as_deref())
    {
        let line = format!(
            "🟩 pixel prompt-submit ❀ {:.1}ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        if matches!(provider, TaskProvider::Devin | TaskProvider::Antigravity) {
            ask_model_to_relay(output, &line);
        }
        // Antigravity's envelope is `injectSteps` alone: no top-level key.
        if provider != TaskProvider::Antigravity {
            output["systemMessage"] = json!(line);
        }
    }
    let first_of_many = part.is_some_and(|part| {
        part.index == 1 && part.count > 1 && event == TaskHookEvent::PromptSubmit
    });
    if first_of_many {
        let path = raw
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|payload| part_path(provider, &payload));
        let mut stdout = std::io::stdout().lock();
        let delivered_head =
            raw.as_deref()
                .zip(output.as_ref())
                .zip(part)
                .is_some_and(|((raw, output), part)| {
                    delivered_first_slice(provider, raw, part, output)
                });
        emit_first_part(
            &mut stdout,
            output.as_ref(),
            path.as_deref(),
            delivered_head,
        );
    } else if let Some(output) = &output {
        // Flush the only response before exit terminates any stalled evaluator.
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{output}");
        let _ = stdout.flush();
    }
    crate::hook_input::test_panic("after-response");
    note_event(provider, event, part, raw.as_deref(), output.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn the_answer_deadline_should_leave_the_print_reserve_of_each_hosts_decision_window() {
        let now = std::time::Instant::now();
        for provider in [TaskProvider::Claude, TaskProvider::Codex, TaskProvider::Pi] {
            assert_eq!(
                answer_by(provider, now) + PRINT_RESERVE,
                now + decision_timeout(provider),
                "{provider:?}"
            );
        }
        assert_eq!(
            answer_by(TaskProvider::Pi, now),
            now + Duration::from_secs(5)
        );
    }

    /// A Claude-argued `task-event` inside a host that imports Claude's
    /// configuration must exit without acting: the marker names the real host.
    #[test]
    fn imported_config_host_is_the_marker_set_in_the_process() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let saved = std::env::var_os("DEVIN_PROJECT_DIR");
        // SAFETY: DEVIN_PROJECT_DIR is only changed under ENV_LOCK.
        unsafe { std::env::set_var("DEVIN_PROJECT_DIR", "/tmp/devin-repo") };
        assert_eq!(imported_config_host(), Some("DEVIN_PROJECT_DIR"));
        // SAFETY: same lock as above.
        unsafe { std::env::remove_var("DEVIN_PROJECT_DIR") };
        assert_eq!(imported_config_host(), None);
        if let Some(restored) = saved {
            // SAFETY: same lock as above.
            unsafe { std::env::set_var("DEVIN_PROJECT_DIR", restored) };
        }
    }

    /// A single `|` marks the NEXT segment as piped; `&&`, `||` and a lone
    /// `&` are separators that carry no such flag. Quotes protect operators.
    #[test]
    fn split_segments_marks_pipes_quotes_and_doubles() {
        assert_eq!(
            split_segments("a | b"),
            Some(vec![("a", false), ("b", true)])
        );
        assert_eq!(
            split_segments("a && b"),
            Some(vec![("a", false), ("b", false)])
        );
        assert_eq!(
            split_segments("a & b"),
            Some(vec![("a", false), ("b", false)])
        );
        assert_eq!(
            split_segments("a || b"),
            Some(vec![("a", false), ("b", false)])
        );
        assert_eq!(
            split_segments("echo 'a|b' && git status"),
            Some(vec![("echo 'a|b'", false), ("git status", false)])
        );
    }

    #[test]
    fn contextual_envelopes_should_name_every_native_event_exactly() {
        for (provider, name) in [
            (TaskProvider::Claude, "claude"),
            (TaskProvider::Codex, "codex"),
            (TaskProvider::Pi, "pi"),
        ] {
            assert_eq!(provider.as_str(), name);
        }
        for (event, cli, host) in [
            (TaskHookEvent::SessionStart, "session-start", "SessionStart"),
            (
                TaskHookEvent::PromptSubmit,
                "prompt-submit",
                "UserPromptSubmit",
            ),
            (TaskHookEvent::PreToolUse, "pre-tool-use", "PreToolUse"),
            (TaskHookEvent::PostToolUse, "post-tool-use", "PostToolUse"),
            (
                TaskHookEvent::ToolFailure,
                "tool-failure",
                "PostToolUseFailure",
            ),
            (TaskHookEvent::Stop, "stop", "Stop"),
            (TaskHookEvent::SessionEnd, "session-end", "SessionEnd"),
            (TaskHookEvent::Interrupt, "interrupt", "Interrupt"),
            (
                TaskHookEvent::SubagentStart,
                "subagent-start",
                "SubagentStart",
            ),
            (TaskHookEvent::SubagentStop, "subagent-stop", "SubagentStop"),
            (
                TaskHookEvent::ModelResponse,
                "model-response",
                "ModelResponse",
            ),
            (TaskHookEvent::UserBash, "user-bash", "UserBash"),
        ] {
            assert_eq!(event.as_str(), cli);
            for provider in [TaskProvider::Claude, TaskProvider::Codex] {
                assert_eq!(
                    envelope(provider, event, &json!({"context":"prepare task-1"})),
                    json!({"hookSpecificOutput":{"hookEventName":host,"additionalContext":"prepare task-1"}})
                );
                assert_eq!(envelope(provider, event, &json!({"context":42})), json!({}));
            }
        }
    }

    #[test]
    fn normalization_should_preserve_each_cancellation_and_success_signal() {
        for (event, payload) in [
            (TaskHookEvent::Interrupt, json!({})),
            (TaskHookEvent::Stop, json!({"cancelled":true})),
            (TaskHookEvent::Stop, json!({"is_interrupt":true})),
            (TaskHookEvent::Stop, json!({"stop_reason":"aborted"})),
            (TaskHookEvent::Stop, json!({"stopReason":"error"})),
        ] {
            assert_eq!(
                normalize(event, &payload)["cancelled"],
                true,
                "{event:?}: {payload}"
            );
        }
        assert_eq!(
            normalize(TaskHookEvent::Stop, &json!({}))["cancelled"],
            false
        );
        for (payload, expected) in [
            (json!({"isError":true}), json!(false)),
            (json!({"isError":false}), json!(true)),
            (json!({"success":false,"isError":false}), json!(false)),
            (json!({"success":true,"isError":true}), json!(true)),
            (json!({}), Value::Null),
        ] {
            assert_eq!(
                normalize(TaskHookEvent::PostToolUse, &payload)["success"],
                expected
            );
        }
        let response = json!({"request_ids":["c1"],"coverage_complete":false});
        assert_eq!(
            normalize(TaskHookEvent::ModelResponse, &response)["coverage_complete"],
            false
        );
        assert_eq!(
            normalize(
                TaskHookEvent::Stop,
                &json!({"session_id":"","sessionId":"fallback"})
            )["session_id"],
            "fallback"
        );
    }

    #[test]
    fn argv_and_uniq_boundaries_should_preserve_literals_without_accepting_outputs() {
        for (command, expected) in [
            ("rg \"needle text\" \"\"", vec!["rg", "needle text", ""]),
            (
                "rg\t'quoted $literal' src",
                vec!["rg", "quoted $literal", "src"],
            ),
            ("rg \"x'y\" src", vec!["rg", "x'y", "src"]),
            ("rg 'a\nb'", vec!["rg", "a\nb"]),
        ] {
            assert_eq!(
                task_argv(command),
                Some(expected.into_iter().map(String::from).collect()),
                "{command}"
            );
            assert!(!shell_mutates(command), "{command}");
        }
        for command in [
            "rg \0",
            "rg '\0'",
            "rg \u{000b}",
            "rg > out",
            "rg unquoted*",
            "rg \"unfinished",
        ] {
            assert_eq!(task_argv(command), None, "{command:?}");
        }
        for args in [
            vec!["input", "output"],
            vec!["--", "input", "output"],
            vec!["input", "--", "output"],
            vec!["-c", "-d", "input", "output"],
        ] {
            assert!(!uniq_read(
                &args.into_iter().map(String::from).collect::<Vec<_>>()
            ));
        }
        for command in [
            "uniq input",
            "uniq -c -d input",
            "uniq input --",
            "uniq -- input",
            "rg x | pixel find-code x",
        ] {
            assert!(!shell_mutates(command), "{command}");
        }
        assert!(shell_mutates("uniq --unknown"));
    }

    #[test]
    fn deadline_should_deny_edits_and_completion_but_preserve_proven_reads() {
        for provider in [TaskProvider::Claude, TaskProvider::Codex, TaskProvider::Pi] {
            for (event, payload) in [
                (TaskHookEvent::PreToolUse, json!({"tool_name":"Edit"})),
                (TaskHookEvent::PreToolUse, json!({"tool_name":"Read"})),
                (TaskHookEvent::Stop, json!({})),
            ] {
                let (release, wait) = std::sync::mpsc::channel();
                let output = bounded_decision(
                    provider,
                    event,
                    &payload.to_string(),
                    Duration::from_millis(1),
                    move || {
                        let _ = wait.recv_timeout(Duration::from_secs(1));
                        json!({"incorrect":"late result"})
                    },
                    |seen| seen == Some(&payload),
                );
                let _ = release.send(());
                assert_eq!(
                    output,
                    envelope(provider, event, &unavailable(event, Some(&payload), true))
                );
            }
        }
        assert_eq!(
            bounded_decision(
                TaskProvider::Codex,
                TaskHookEvent::PreToolUse,
                "{}",
                Duration::from_secs(1),
                || json!({"decision":"ready"}),
                |_| true
            ),
            json!({"decision":"ready"})
        );
    }

    #[test]
    fn normalize_should_preserve_host_identity_without_tool_secrets() {
        let input = json!({"session_id":"s", "turn_id":"t", "tool_use_id":"c", "tool_name":"Write", "tool_input":{"file_path":"src/a.rs","content":"secret-value"},"tool_response":"private-output"});
        let result = normalize(TaskHookEvent::PreToolUse, &input);
        assert_eq!(result["event_id"], "s:t:pre-tool-use:c");
        assert_eq!(result["mutation"], true);
        assert_eq!(result["changed_paths"], json!(["src/a.rs"]));
        assert_eq!(result["agent_id"], Value::Null);
        assert!(!result.to_string().contains("secret-value"));
        assert!(!result.to_string().contains("private-output"));
        assert_eq!(
            normalize(TaskHookEvent::PostToolUse, &input)["success"],
            Value::Null
        );
    }

    #[test]
    fn normalization_should_not_invent_ids_or_treat_errors_as_success() {
        let result = normalize(TaskHookEvent::ToolFailure, &json!({"is_interrupt":true}));
        assert_eq!(result["event_id"], Value::Null);
        assert_eq!(result["success"], false);
        assert_eq!(result["cancelled"], true);
        assert_eq!(result["coverage"]["model_requests_complete"], false);
    }

    #[test]
    fn telemetry_should_keep_only_bounded_counters_and_safe_request_labels() {
        let event = json!({
            "request_ids":["call"], "request_tools":{"call":"mcp__pixel.find-code", "extra":"secret-label"},
            "response_id":"response-7", "duration_ms":123,
            "usage":{"input":100,"output":0,"cache_read":4,"cache_write":2,"secret":"private-value"},
            "tool_name":"mcp__pixel.find-code"
        });
        let normalized = normalize(TaskHookEvent::ModelResponse, &event);
        assert_eq!(
            normalized["request_tools"],
            json!({"call":"mcp__pixel.find-code"})
        );
        assert_eq!(normalized["tool_name"], "mcp__pixel.find-code");
        assert_eq!(normalized["response_id"], "response-7");
        assert_eq!(normalized["duration_ms"], 123);
        assert_eq!(
            normalized["usage"],
            json!({"input":100,"output":0,"cache_read":4,"cache_write":2})
        );
        assert!(!normalized.to_string().contains("private-value"));
        assert!(!normalized.to_string().contains("secret-label"));
        let invalid = normalize(
            TaskHookEvent::ModelResponse,
            &json!({
                "request_ids":["call"],"request_tools":{"call":"tool secret text"},"tool_name":"tool\nprivate",
                "response_id":"response\nprivate", "duration_ms":604_800_001_u64,
                "usage":{"input":-1,"output":"10","cache_read":1_000_000_000_001_u64,"cache_write":0.5}
            }),
        );
        assert_eq!(invalid["request_tools"], json!({}));
        assert_eq!(invalid["tool_name"], "");
        for key in ["usage", "response_id", "duration_ms"] {
            assert_eq!(invalid[key], Value::Null, "{key}");
        }
    }

    /// `sed_script_writes` fires only on a write/exec command at a command
    /// boundary (`w out`, `W x`, `e cmd`, `1e touch marker`), a substitution
    /// with an `e`/`w`/`W` flag (`s/a/b/w out`, `s/.*/x/ep`), and the
    /// newline-separated, comment-hidden and inline-`--expression` forms that
    /// carry one (`p\ne touch marker`, `#x\nw out`). GNU sed takes the rest of
    /// the line as the `e` command or `w`/`W` filename, so `west`, `write` and
    /// `east` are writes too. Bounded reads (`1,20p`, `s/a/b/`, `d`, `p`, `a
    /// label`) and unterminated or comment-stripped scripts stay reads.
    #[test]
    fn sed_script_writes_should_flag_only_write_or_exec_commands() {
        for script in [
            "w out",
            "W tmp",
            "e rm -rf x",
            "s/a/b/w out",
            "1,5p;w out",
            "1,5{w out}",
            "s/a/b/; w out",
            "1e touch marker",
            "1,5e touch marker",
            "$e rm -rf x",
            "2w out",
            "/pat/e touch marker",
            "s@a@b@w out",
            "s@a@b@e touch marker",
            "s/a/b/e",
            "w",
            "1 e touch marker",
            "1~2w out",
            "1,+3e touch x",
            "1!w out",
            "$!w out",
            "/a/Iw out",
            "1 w out",
            "\\#foo#w out",
            "\\#\\b#w out",
            "west",
            "write",
            "east",
            "p\ne touch marker",
            "1,20p\nw out",
            "#x\nw out",
            "s/.*/touch marker/ep",
            "s/a/id/e;p",
            "s/a\\/b/c/w out",
            "s/a/b/west",
            // Address modifiers before command: whitespace, negation, step/offset, regex flags
            "1 w out",
            "1!w out",
            "1~2w out",
            "1,+2w out",
            "1,~4w out",
            "/x/Iw out",
            "/x/Mw out",
            "/x/!e cmd",
            "1!e touch marker",
        ] {
            assert!(sed_script_writes(script), "{script}");
        }
        for script in [
            "1,20p",
            "1,20p\n2,30q",
            "s/a/b/",
            "s/a b/c/",
            "s@a@b@",
            "s@a@b@g",
            "1,20p;2,30q",
            "433,472p",
            "145,184p",
            "d",
            "p",
            "n",
            "a label",
            "# ; w out",
            "s/a\\/b/wout",
            "s/a/b/;q west",
            "s/a/b/\nq west",
            "\\#foo",
            "\\#foo#p",
            "# w out\n1,20p",
            "\\",
            // Address modifiers with read commands
            "1 p",
            "1!p",
            "1~2p",
            "1,+2p",
            "/x/Ip",
            "/x/Mp",
            "1,20p\n2,30q",
            // Comment with embedded ; must not expose w as a command
            "p; # x; w out\nq",
        ] {
            assert!(!sed_script_writes(script), "{script}");
        }
    }

    #[test]
    fn unavailable_should_block_edits_but_preserve_reads_and_recovery() {
        for tool in ["Write", "Edit", "apply_patch", "write", "edit"] {
            let decision = unavailable(
                TaskHookEvent::PreToolUse,
                Some(&json!({"tool_name":tool})),
                true,
            );
            assert_eq!(decision["decision"], "deny", "{tool}");
        }
        for command in [
            "rtk proxy pixel task-state status abc",
            "pixel doctor . --fix",
            "git diff",
            "cat src/a.rs",
            "pixel repo-state --json",
            "pixel review-changes",
            "pixel list-branches .",
            "pixel recall search 'auth'",
            "pixel commit-history --json",
            "pixel dig-history --phrase x",
        ] {
            assert!(!shell_mutates(command), "{command}");
        }
        for command in [
            "python3 mutate.py",
            "cat x > y",
            "pixel commit --files x",
            "git checkout x",
            "rtk proxy rm x",
            "pixel list-branches . --fetch",
            "pixel recall index",
            "pixel list-errors gc",
            "pixel list-errors run -- make",
            "pixel recall --limit 5 index",
            "pixel recall setup",
            "pixel plan-rollback --apply abc --file a.rs",
            "pixel new-branch topic --request-id 1",
        ] {
            assert!(shell_mutates(command), "{command}");
        }
        assert_eq!(
            unavailable(
                TaskHookEvent::PreToolUse,
                Some(&json!({"tool_name":"Read"})),
                true
            )["decision"],
            "observe"
        );
        assert_eq!(
            unavailable(TaskHookEvent::Stop, Some(&json!({})), true)["decision"],
            "deny"
        );
    }

    #[test]
    fn discovery_aliases_should_remain_reads_when_task_state_is_unavailable() {
        for tool in [
            "ls",
            "find",
            "list_dir",
            "grep_search",
            "file_search",
            "view_file",
        ] {
            let payload = json!({"tool_name":tool,"tool_input":{"path":"src/lib.rs"}});
            assert_eq!(
                normalize(TaskHookEvent::PreToolUse, &payload)["mutation"],
                false,
                "{tool}"
            );
            assert_eq!(
                unavailable(TaskHookEvent::PreToolUse, Some(&payload), true)["decision"],
                "observe",
                "{tool}"
            );
        }
    }

    fn assert_sed_denied(command: &str) {
        assert!(shell_mutates(command), "{command}");
        assert_eq!(
            unavailable(
                TaskHookEvent::PreToolUse,
                Some(&json!({"tool_name":"Bash","tool_input":{"command":command}})),
                true
            )["decision"],
            "deny",
            "{command}"
        );
    }

    #[test]
    fn sed_line_length_should_consume_only_a_numeric_value() {
        // BSD sed's `-l` takes no value: the next operand is the script it
        // runs, so swallowing it as a line length hid a write on macOS.
        for command in [
            "sed -n -l 'w out' file.rs",
            "sed -l 'e touch marker' file.rs",
            "sed -n --line-length 'w out' file.rs",
        ] {
            assert_sed_denied(command);
        }
        // A numeric value is still consumed, so the script after it is read.
        assert!(!shell_mutates("sed -n -l 72 '1,20p' west.rs"));
    }

    #[test]
    fn sed_positional_before_expression_should_be_scanned_as_a_script() {
        // BSD sed stops option parsing at the first operand: there the
        // operand before `-e` is the script and `-e p` are input files.
        for command in [
            "sed 'w out' -e p file.rs",
            "sed -n 'e touch marker' --expression=p file.rs",
            "sed events.rs -e '1,20p'",
        ] {
            assert_sed_denied(command);
        }
        // Operands after the first `-e` are files on every sed.
        assert!(!shell_mutates("sed -n -e '1,20p' west.rs events.rs"));
        assert!(!shell_mutates("sed -n '1,20p' -e '2,30p' west.rs"));
    }

    #[test]
    fn write_flags_config_setters_and_unknown_tools_should_require_preparation() {
        for command in [
            "git diff --output=src/a.rs",
            "git diff --output src/a.rs",
            "git show --ext-diff",
            "rg --pre /runner/mutator pattern",
            "rg --pre=/runner/mutator pattern",
            "rg --hostname-bin /runner/mutator pattern",
            "pixel config setup",
            "pixel task-state evaluate --suite external.json",
            "sed -i 's/a/b/' file.rs",
            "sed -ni 's/a/b/' file.rs",
            "sed --in-place 's/a/b/' file.rs",
            "sed -e 's/a/b/' -e 'w out' file.rs",
            "sed -n 's/a/b/w out' file.rs",
            "sed 'e rm -rf x' file.rs",
            "sed '1e touch marker' file.rs",
            "sed 's@a@b@w out' file.rs",
            "sed -f script.sed file.rs",
            "sed --file=script.sed file.rs",
            "sed --expression='w out' file.rs",
            "sed -e'w out' file.rs",
            "sed 'west' file.rs",
            "sed 'p\ne touch marker' file.rs",
            "sed 's/a/b/ep' file.rs",
            // Address modifiers before write/exec commands
            "sed -n '1 w out' file.rs",
            "sed -n '1!w out' file.rs",
            "sed -n '1~2w out' file.rs",
            "sed -n '1,+2w out' file.rs",
            "sed -n '/x/Iw out' file.rs",
            "sed -n '/x/Mw out' file.rs",
            "sed -n '1!e touch marker' file.rs",
            // -l consumes its value arg, so the script after it is still scanned
            "sed -n -l 72 'w out' file.rs",
            "sed -n --line-length 72 'w out' file.rs",
            // `--` ends the options: the first operand after it is the script
            "sed -- 'e touch marker' file.rs",
            "sed -n -- 'w out' file.rs",
        ] {
            assert!(shell_mutates(command), "{command}");
            assert_eq!(
                unavailable(
                    TaskHookEvent::PreToolUse,
                    Some(&json!({"tool_name":"Bash","tool_input":{"command":command}})),
                    true
                )["decision"],
                "deny",
                "{command}"
            );
        }
        for command in [
            "sed -n '1,40p' eval/README.md",
            "sed -n '1,20p' website/content/docs.md",
            "sed -n '1p' /tmp/w.txt",
            "git diff --name-only HEAD",
            "git status --porcelain=v1",
            "git diff -- --output=notes",
            "rg -n -F needle src",
            "rg --glob='*.rs' needle",
            "sed -n '433,472p' crates/pixel/src/code_search.rs",
            "rtk sed -n '145,184p' crates/pixel/src/main.rs",
            "sed -n '1,20p' file.rs | sort | uniq",
            "sed --expression='1,20p' file.rs",
            "sed -e'1,40p' file.rs",
            // File operands starting with e/w/W must not be misclassified as writes
            "sed -n '1,20p' events.rs",
            "sed -n '1,20p' worker.rs",
            "sed -n '1,20p' web/index.js",
            "sed -n '1,20p' examples/x.rs",
            "sed -n '1,20p' /tmp/w.rs",
            "sed -n '1,20p' west.rs",
            "sed -e '1,20p' -e '2,30p' events.rs",
            "sed --expression='1,20p' worker.rs",
            // Address modifiers with read commands
            "sed -n '1 p' file.rs",
            "sed -n '1!p' file.rs",
            "sed -n '1~2p' file.rs",
            "sed -n '1,+2p' file.rs",
            "sed -n '/x/Ip' file.rs",
            "sed -n '/x/Mp' file.rs",
            // -l consumes its value, so the script after it is still the script
            "sed -n -l 72 '1,20p' file.rs",
            "sed -n --line-length 72 '1,20p' file.rs",
            // after `--`, only the first operand is the script; the rest are files
            "sed -n -- '1,20p' west.rs",
            "sed -n '1,20p' -- west.rs",
            "sed -n -e '1,20p' -- west.rs",
            "rg -- --pre",
            "pixel config",
            "pixel config metrics",
            "pixel task-state prepare task-1",
            "pixel task-state verify task-1",
            "pixel task-state recover task-1",
            "pixel task-state contract task-1 --file contract.json",
            "pixel task-state status task-1 --json",
            "pixel task-state contract task-1 --definition '{}'",
        ] {
            assert!(!shell_mutates(command), "{command}");
        }
        for tool in ["mcp__custom__edit", "customTool", ""] {
            assert!(mutation(tool, &json!({})), "{tool}");
            assert_eq!(
                unavailable(
                    TaskHookEvent::PreToolUse,
                    Some(&json!({"tool_name":tool})),
                    true
                )["decision"],
                "deny",
                "{tool}"
            );
        }
        assert!(mutation("pixel", &json!({"action":"new_mutator"})));
    }

    #[test]
    fn read_pipelines_and_literal_contract_json_should_preserve_recovery() {
        for command in [
            "rg needle src | sort | uniq",
            "git diff --name-only | sort -u",
            "rg 'x|y' src | uniq -c",
            "cat source.txt | uniq -- -",
            "rg needle || cat source.txt",
            "rg needle; cat source.txt",
        ] {
            assert!(!shell_mutates(command), "{command}");
        }
        for command in [
            "rg needle | sort -o source.txt",
            "rg needle | sort --output=source.txt",
            "rg needle | uniq - source.txt",
            "rg needle | uniq -- - source.txt",
            "rg needle | tee source.txt",
            "rg needle | pixel task-state prepare task-1",
            "rg needle; pixel task-state prepare task-1",
            "rg needle || rm source.txt",
            "cat source.txt && sed -i s/a/b/ source.txt",
            "rg needle | cat > source.txt",
            "rg $(touch source.txt) | sort",
            "rg \"$(touch source.txt)\" | sort",
            "pixel task-state contract task-1 --definition $(cat secret)",
            "pixel task-state contract task-1 --definition \"$(cat secret)\"",
        ] {
            assert!(shell_mutates(command), "{command}");
        }
        let definition =
            json!({"checks":[{"argv":["/bin/sh","-c","test \"$(cat source.txt)\" = original"]}]})
                .to_string();
        let command =
            format!("pixel task-state contract task-1 --definition '{definition}' --json");
        assert!(!shell_mutates(&command));
        assert_eq!(task_argv(&command).unwrap()[5], definition);
    }

    #[test]
    fn envelopes_should_use_supported_provider_specific_decisions() {
        for provider in [TaskProvider::Claude, TaskProvider::Codex] {
            let denied = envelope(
                provider,
                TaskHookEvent::PreToolUse,
                &json!({"decision":"deny","reason":"prepare first"}),
            );
            assert_eq!(
                denied,
                json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"prepare first"}})
            );
            assert_eq!(
                envelope(
                    provider,
                    TaskHookEvent::Stop,
                    &json!({"decision":"continue","reason":"verify"})
                ),
                json!({"decision":"block","reason":"verify"})
            );
            assert_eq!(
                envelope(
                    provider,
                    TaskHookEvent::Stop,
                    &json!({"decision":"deny","reason":"unverified"})
                ),
                json!({"continue":false,"stopReason":"unverified"})
            );
            assert_eq!(
                envelope(
                    provider,
                    TaskHookEvent::PreToolUse,
                    &json!({"decision":"allow"})
                ),
                json!({})
            );
        }
        let decision = json!({"decision":"deny","reason":"verify"});
        assert_eq!(
            envelope(TaskProvider::Pi, TaskHookEvent::Stop, &decision),
            decision
        );
    }

    #[test]
    fn model_response_coverage_should_require_a_complete_real_id_list() {
        let event = json!({"request_ids":["c1","c2"],"coverage_complete":true});
        let normalized = normalize(TaskHookEvent::ModelResponse, &event);
        assert_eq!(normalized["request_ids"], json!(["c1", "c2"]));
        assert_eq!(normalized["coverage_complete"], true);
        assert_eq!(
            normalize(TaskHookEvent::PreToolUse, &event)["coverage_complete"],
            false
        );
        for payload in [
            json!({"coverage_complete":true}),
            json!({"request_ids":["c1",null],"coverage_complete":true}),
            json!({"request_ids":[""],"coverage_complete":true}),
        ] {
            assert_eq!(
                normalize(TaskHookEvent::ModelResponse, &payload)["coverage_complete"],
                false
            );
        }
        assert!(mutation("pixel_project", &json!({"action":"commit"})));
        assert!(!mutation(
            "pixel_project",
            &json!({"action":"review_changes"})
        ));
    }

    #[test]
    fn read_sequences_should_not_count_as_mutations() {
        for command in [
            "cd /repo && cat README.md",
            "cd /repo && ls eval eval/scenarios 2>/dev/null; head -n 40 eval/README.md",
            "nl -ba src/a.rs | head -n 40",
            "echo start; cat src/a.rs",
            "rg needle src && wc -l src/a.rs",
        ] {
            assert!(!shell_mutates(command), "{command}");
        }
        for command in [
            "cat src/a.rs 2>/tmp/err",
            "cat src/a.rs > /dev/null 2>/dev/null",
            "cat src/a.rs 2>/dev/null > out",
            "cd /repo && rm -rf target",
            "echo x; python3 mutate.py",
            // `sed` classification belongs to the sed parser of #649.
            "cd /repo && sed -i s/a/b/ src/a.rs",
        ] {
            assert!(shell_mutates(command), "{command}");
        }
    }

    #[test]
    fn delegation_and_bookkeeping_tools_should_not_count_as_mutations() {
        for tool in ["Agent", "Task", "TodoWrite", "ToolSearch"] {
            let payload = json!({"tool_name":tool,"tool_input":{"prompt":"edit src/a.rs"}});
            assert_eq!(
                normalize(TaskHookEvent::PreToolUse, &payload)["mutation"],
                false,
                "{tool}"
            );
            assert_eq!(
                unavailable(TaskHookEvent::PreToolUse, Some(&payload), true)["decision"],
                "observe",
                "{tool}"
            );
        }
        for tool in ["agent", "NotebookEdit", "MultiEdit"] {
            assert!(mutation(tool, &json!({})), "{tool}");
        }
    }

    #[test]
    fn an_unenforced_session_should_only_observe_when_the_ledger_is_unavailable() {
        for (event, payload) in [
            (TaskHookEvent::PreToolUse, json!({"tool_name":"Edit"})),
            (
                TaskHookEvent::PreToolUse,
                json!({"tool_name":"Bash","tool_input":{"command":"rm x"}}),
            ),
            (TaskHookEvent::Stop, json!({})),
            (TaskHookEvent::SubagentStop, json!({})),
        ] {
            assert_eq!(
                unavailable(event, Some(&payload), false)["decision"],
                "observe"
            );
            assert_eq!(unavailable(event, Some(&payload), true)["decision"], "deny");
            for provider in [TaskProvider::Claude, TaskProvider::Codex] {
                assert_eq!(
                    bounded_decision(
                        provider,
                        event,
                        &payload.to_string(),
                        Duration::from_millis(1),
                        || {
                            std::thread::sleep(Duration::from_millis(200));
                            json!({})
                        },
                        |_| false,
                    ),
                    json!({}),
                    "{event:?}"
                );
            }
        }
    }

    #[test]
    fn enforcement_should_be_assumed_without_a_payload_or_a_repository() {
        assert!(enforcement_applies(TaskProvider::Claude, None));
        let missing =
            std::env::temp_dir().join(format!("pixel-no-such-dir-{}", std::process::id()));
        assert!(enforcement_applies(
            TaskProvider::Claude,
            Some(&json!({"cwd": missing, "session_id":"s"}))
        ));
    }

    #[test]
    fn malformed_input_should_not_open_the_mutation_or_completion_gate() {
        for provider in [TaskProvider::Claude, TaskProvider::Codex] {
            assert_eq!(
                process(provider, TaskHookEvent::PreToolUse, "not json", None)["hookSpecificOutput"]
                    ["permissionDecision"],
                "deny"
            );
            assert_eq!(
                process(provider, TaskHookEvent::Stop, "not json", None)["continue"],
                false
            );
            assert_eq!(
                process(provider, TaskHookEvent::PostToolUse, "not json", None),
                json!({})
            );
        }
    }

    /// A source that answers one file for every search, so a brief exists.
    struct OneFile;

    impl crate::execution_brief::chain::Evidence for OneFile {
        fn files_with(
            &self,
            _: &str,
            _: std::time::Instant,
        ) -> Result<crate::execution_brief::chain::Found, String> {
            Ok(crate::execution_brief::chain::Found {
                hits: vec![crate::execution_brief::chain::FileHit {
                    path: "src/a.ts".into(),
                    line: 2,
                }],
                capped: false,
            })
        }
        fn concept(
            &self,
            _: &str,
            _: std::time::Instant,
        ) -> Result<crate::execution_brief::chain::Found, String> {
            Err("unused".into())
        }
        fn symbols(
            &self,
            _: &str,
            _: std::time::Instant,
        ) -> Result<Vec<crate::execution_brief::chain::SymbolHit>, String> {
            Err("unused".into())
        }
        fn callers(
            &self,
            _: &str,
            _: std::time::Instant,
        ) -> Result<Vec<crate::execution_brief::chain::CallerHit>, String> {
            Err("unused".into())
        }
    }

    fn pending_brief() -> crate::execution_brief::chain::Pending {
        crate::execution_brief::chain::start_with(
            "where is `fetchUser` used",
            crate::execution_brief::chain::Gate {
                enabled: true,
                indexed: true,
            },
            Duration::from_secs(5),
            |_| Box::new(OneFile),
            |_, _| None,
        )
        .expect("a code prompt in an open gate starts a brief")
    }

    #[test]
    fn a_brief_should_become_the_context_the_envelope_delivers_to_the_host() {
        let decision = with_brief(
            json!({"decision":"observe","coverage":"partial"}),
            Some(pending_brief()),
        );
        let context = decision["context"].as_str().unwrap().to_string();
        assert!(context.starts_with("[PIXEL:BRIEF]\n"), "{context}");
        assert!(context.contains("\nfiles: src/a.ts:2\n"), "{context}");
        for provider in [TaskProvider::Claude, TaskProvider::Codex] {
            assert_eq!(
                envelope(provider, TaskHookEvent::PromptSubmit, &decision),
                json!({"hookSpecificOutput":{
                    "hookEventName":"UserPromptSubmit","additionalContext":context
                }})
            );
        }
        assert_eq!(
            envelope(
                TaskProvider::Antigravity,
                TaskHookEvent::PromptSubmit,
                &decision
            ),
            json!({"injectSteps":[{"ephemeralMessage":context}]})
        );
    }

    #[test]
    fn a_decision_should_keep_its_own_context_and_stay_unchanged_without_a_brief() {
        let own = json!({"decision":"observe","context":"ledger note"});
        assert_eq!(with_brief(own.clone(), Some(pending_brief())), own);
        let plain = json!({"decision":"observe","coverage":"partial"});
        assert_eq!(with_brief(plain.clone(), None), plain);
        assert_eq!(
            envelope(TaskProvider::Claude, TaskHookEvent::PromptSubmit, &plain),
            json!({})
        );
    }

    #[test]
    fn a_brief_should_start_only_for_a_claude_or_codex_prompt_in_an_indexed_repository() {
        let root = std::env::temp_dir().join(format!("pixel-hook-brief-{}", std::process::id()));
        let shard_dir = root.join(pixel_index::index::SHARD_DIR);
        std::fs::create_dir_all(&shard_dir).unwrap();
        std::fs::write(shard_dir.join(pixel_index::index::SHARD_FILE), b"x").unwrap();
        let root = root.canonicalize().unwrap();
        let prompt = json!({"prompt":"callers of `fetchUser`","cwd":root});
        for provider in [TaskProvider::Claude, TaskProvider::Codex] {
            assert!(start_brief(provider, TaskHookEvent::PromptSubmit, &prompt, 1).is_some());
            for event in [
                TaskHookEvent::PreToolUse,
                TaskHookEvent::PostToolUse,
                TaskHookEvent::Stop,
                TaskHookEvent::SessionStart,
            ] {
                assert!(
                    start_brief(provider, event, &prompt, 1).is_none(),
                    "{event:?}"
                );
            }
        }
        assert!(start_brief(TaskProvider::Pi, TaskHookEvent::PromptSubmit, &prompt, 1).is_none());
        let no_prompt = json!({"cwd":root});
        assert!(
            start_brief(
                TaskProvider::Claude,
                TaskHookEvent::PromptSubmit,
                &no_prompt,
                1
            )
            .is_none()
        );
        let elsewhere = json!({"prompt":"callers of `fetchUser`","cwd":root.join("missing")});
        assert!(
            start_brief(
                TaskProvider::Claude,
                TaskHookEvent::PromptSubmit,
                &elsewhere,
                1
            )
            .is_none()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn indexed_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("pixel-hook-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let shard_dir = root.join(pixel_index::index::SHARD_DIR);
        std::fs::create_dir_all(&shard_dir).unwrap();
        std::fs::write(shard_dir.join(pixel_index::index::SHARD_FILE), b"x").unwrap();
        root.canonicalize().unwrap()
    }

    #[test]
    fn pi_should_be_briefed_only_when_its_package_asks_for_the_brief_alone() {
        let prompt = "callers of `fetchUser`";
        let marked = json!({"prompt": prompt, "brief_only": true});
        let plain = json!({"prompt": prompt});
        assert!(brief_only(TaskProvider::Pi, &marked));
        assert!(!brief_only(TaskProvider::Pi, &plain));
        assert_eq!(
            brief_prompt(TaskProvider::Pi, TaskHookEvent::PromptSubmit, &marked).as_deref(),
            Some(prompt)
        );
        assert_eq!(
            brief_prompt(TaskProvider::Pi, TaskHookEvent::PromptSubmit, &plain),
            None
        );
        for marker in [json!(false), json!("true"), json!(1), Value::Null] {
            let payload = json!({"prompt": prompt, "brief_only": marker});
            assert!(!brief_only(TaskProvider::Pi, &payload), "{marker}");
            assert_eq!(
                brief_prompt(TaskProvider::Pi, TaskHookEvent::PromptSubmit, &payload),
                None,
                "{marker}"
            );
        }
        for event in [TaskHookEvent::PreToolUse, TaskHookEvent::Stop] {
            assert_eq!(brief_prompt(TaskProvider::Pi, event, &marked), None);
        }
        // The marker is Pi's: no other provider turns its ledger off with it.
        for provider in [
            TaskProvider::Claude,
            TaskProvider::Codex,
            TaskProvider::Devin,
            TaskProvider::Gemini,
            TaskProvider::Antigravity,
        ] {
            assert!(!brief_only(provider, &marked), "{provider:?}");
        }
    }

    #[test]
    fn a_pi_brief_should_start_and_a_pi_session_should_warm_only_on_the_brief_only_marker() {
        let root = indexed_root("pi-brief-only");
        let marked = json!({"prompt": "callers of `fetchUser`", "cwd": root, "brief_only": true});
        let plain = json!({"prompt": "callers of `fetchUser`", "cwd": root});
        assert!(start_brief(TaskProvider::Pi, TaskHookEvent::PromptSubmit, &marked, 1).is_some());
        assert!(start_brief(TaskProvider::Pi, TaskHookEvent::PromptSubmit, &plain, 1).is_none());
        assert_eq!(
            session_start_root(TaskProvider::Pi, TaskHookEvent::SessionStart, &marked),
            Some(root.clone())
        );
        assert_eq!(
            session_start_root(TaskProvider::Pi, TaskHookEvent::SessionStart, &plain),
            None
        );
        assert_eq!(
            session_start_root(TaskProvider::Pi, TaskHookEvent::PromptSubmit, &marked),
            None
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_brief_only_pi_event_should_bind_no_task_and_answer_observe() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let root = indexed_root("pi-no-ledger");
        for (event, extra) in [
            (
                TaskHookEvent::PromptSubmit,
                json!({"prompt": "where is `fetchUser` used"}),
            ),
            (TaskHookEvent::SessionStart, json!({"source": "startup"})),
        ] {
            let mut payload = json!({
                "session_id": "pi-session", "cwd": root, "brief_only": true,
            });
            payload
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let output = process(TaskProvider::Pi, event, &payload.to_string(), None);
            assert_eq!(output["decision"], "observe", "{event:?}: {output}");
            assert!(output.get("coverage").is_none(), "{event:?}: {output}");
            assert!(output.get("hookSpecificOutput").is_none(), "{event:?}");
        }
        assert!(
            !root.join(".pixel/tasks").exists(),
            "the ledger bound or recorded something for a brief-only event"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_brief_budget_should_be_one_part_unless_a_hook_part_or_the_pi_package_says_more() {
        let root = indexed_root("pi-budget");
        let marked = json!({"cwd": root, "brief_only": true});
        let plain = json!({"cwd": root});
        std::fs::write(root.join(".pixel/config.yaml"), "brief_parts: 4\n").unwrap();
        assert_eq!(brief_budget(TaskProvider::Pi, &marked, None), 4);
        assert_eq!(brief_budget(TaskProvider::Pi, &plain, None), 1);
        assert_eq!(brief_budget(TaskProvider::Claude, &marked, None), 1);
        assert_eq!(brief_budget(TaskProvider::Claude, &plain, None), 1);
        for provider in [TaskProvider::Pi, TaskProvider::Claude] {
            assert_eq!(brief_budget(provider, &marked, Some(part(2, 3))), 3);
            assert_eq!(brief_budget(provider, &plain, Some(part(1, 2))), 2);
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_metrics_line_should_follow_a_delivered_brief_only() {
        let marked = json!({"brief_only": true}).to_string();
        let plain = json!({}).to_string();
        let pi_brief = json!({"decision": "observe", "context": "[PIXEL:BRIEF]"});
        assert!(delivered_brief(TaskProvider::Pi, Some(&marked), &pi_brief));
        // Not Pi's package: a ledger note of a host driving the lifecycle.
        assert!(!delivered_brief(TaskProvider::Pi, Some(&plain), &pi_brief));
        assert!(!delivered_brief(TaskProvider::Pi, None, &pi_brief));
        assert!(!delivered_brief(
            TaskProvider::Pi,
            Some("not json"),
            &pi_brief
        ));
        for output in [
            json!({"decision": "observe"}),
            json!({"decision": "observe", "context": 42}),
            json!({"hookSpecificOutput": {"additionalContext": "x"}}),
        ] {
            assert!(!delivered_brief(TaskProvider::Pi, Some(&marked), &output));
        }
        let claude_brief = envelope(
            TaskProvider::Claude,
            TaskHookEvent::PromptSubmit,
            &json!({"decision": "observe", "context": "[PIXEL:BRIEF]"}),
        );
        assert!(delivered_brief(
            TaskProvider::Claude,
            Some(&plain),
            &claude_brief
        ));
        assert!(!delivered_brief(
            TaskProvider::Claude,
            Some(&plain),
            &json!({})
        ));
        assert!(!delivered_brief(
            TaskProvider::Claude,
            Some(&marked),
            &json!({"decision": "observe", "context": "bare"})
        ));
        let agy_brief = envelope(
            TaskProvider::Antigravity,
            TaskHookEvent::PromptSubmit,
            &json!({"decision": "observe", "context": "[PIXEL:BRIEF]"}),
        );
        assert!(delivered_brief(
            TaskProvider::Antigravity,
            Some(&plain),
            &agy_brief
        ));
        assert!(!delivered_brief(
            TaskProvider::Antigravity,
            Some(&plain),
            &claude_brief
        ));
        assert!(!delivered_brief(
            TaskProvider::Antigravity,
            Some(&plain),
            &json!({})
        ));
    }

    #[test]
    fn ask_model_to_relay_should_end_the_brief_each_host_injects() {
        let ask = "\n\nEnd your answer with this line, verbatim: 🟩 L";
        let mut devin = json!({"hookSpecificOutput": {"additionalContext": "B"}});
        ask_model_to_relay(&mut devin, "🟩 L");
        assert_eq!(
            devin,
            json!({"hookSpecificOutput": {"additionalContext": format!("B{ask}")}})
        );
        let mut agy = json!({"injectSteps": [{"ephemeralMessage": "B"}]});
        ask_model_to_relay(&mut agy, "🟩 L");
        assert_eq!(
            agy,
            json!({"injectSteps": [{"ephemeralMessage": format!("B{ask}")}]})
        );
        let mut none = json!({"decision": "observe"});
        ask_model_to_relay(&mut none, "🟩 L");
        assert_eq!(none, json!({"decision": "observe"}));
    }

    #[test]
    fn the_feedback_part_should_count_the_brief_bytes_each_envelope_carries() {
        let marked = json!({"brief_only": true});
        let plain = json!({});
        let pi_brief = json!({"decision": "observe", "context": "[PIXEL:BRIEF]"});
        let wrapped = json!({"hookSpecificOutput": {"additionalContext": "abc"}});
        assert_eq!(context_bytes(TaskProvider::Pi, &marked, &pi_brief), 13);
        // A ledger note of a Pi host that drives the lifecycle is not a brief.
        assert_eq!(context_bytes(TaskProvider::Pi, &plain, &pi_brief), 0);
        assert_eq!(context_bytes(TaskProvider::Pi, &marked, &wrapped), 0);
        assert_eq!(context_bytes(TaskProvider::Claude, &plain, &wrapped), 3);
        assert_eq!(context_bytes(TaskProvider::Codex, &marked, &wrapped), 3);
        assert_eq!(context_bytes(TaskProvider::Claude, &marked, &pi_brief), 0);
        let injected = json!({"injectSteps": [{"ephemeralMessage": "[PIXEL:BRIEF]x"}]});
        assert_eq!(
            context_bytes(TaskProvider::Antigravity, &plain, &injected),
            14
        );
        assert_eq!(
            context_bytes(TaskProvider::Antigravity, &plain, &wrapped),
            0
        );
        assert_eq!(context_bytes(TaskProvider::Claude, &plain, &injected), 0);
    }

    #[test]
    fn the_brief_session_should_be_antigravity_s_conversation_when_no_session_id_is_sent() {
        let conversation = json!({"conversationId": "c-1", "workspacePaths": ["/r"]});
        assert_eq!(brief_session(&conversation), Some("c-1"));
        let both = json!({"session_id": "s-1", "conversationId": "c-1"});
        assert_eq!(brief_session(&both), Some("s-1"));
        assert_eq!(brief_session(&json!({"sessionId": "s-2"})), Some("s-2"));
        assert_eq!(brief_session(&json!({"conversationId": ""})), None);
        assert_eq!(brief_session(&json!({})), None);
    }

    #[test]
    fn a_session_start_should_name_its_repository_for_the_briefed_providers_only() {
        let root = std::env::temp_dir().join(format!("pixel-hook-warm-{}", std::process::id()));
        let shard_dir = root.join(pixel_index::index::SHARD_DIR);
        std::fs::create_dir_all(&shard_dir).unwrap();
        std::fs::write(shard_dir.join(pixel_index::index::SHARD_FILE), b"x").unwrap();
        let root = root.canonicalize().unwrap();
        let payload = json!({"cwd":root});
        for provider in [
            TaskProvider::Claude,
            TaskProvider::Codex,
            TaskProvider::Antigravity,
        ] {
            assert_eq!(
                session_start_root(provider, TaskHookEvent::SessionStart, &payload),
                Some(root.clone()),
                "{provider:?}"
            );
            for event in [
                TaskHookEvent::PromptSubmit,
                TaskHookEvent::PreToolUse,
                TaskHookEvent::PostToolUse,
                TaskHookEvent::Stop,
            ] {
                assert_eq!(
                    session_start_root(provider, event, &payload),
                    None,
                    "{provider:?} {event:?}"
                );
            }
        }
        assert_eq!(
            session_start_root(TaskProvider::Pi, TaskHookEvent::SessionStart, &payload),
            None
        );
        let elsewhere = json!({"cwd":root.join("missing")});
        assert_eq!(
            session_start_root(
                TaskProvider::Claude,
                TaskHookEvent::SessionStart,
                &elsewhere
            ),
            None
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn antigravity_prompt_should_parse_user_text_and_ignore_other_roles() {
        let path = std::env::temp_dir().join(format!(
            "pixel-antigravity-transcript-{}",
            std::process::id()
        ));
        for (content, field, expected) in [
            (json!("string prompt"), "content", "string prompt"),
            (json!("text prompt"), "text", "text prompt"),
            (
                json!([{"text":"first part"}, {"image":"ignored"}, {"text":"second part"}]),
                "content",
                "first part\nsecond part",
            ),
            // Antigravity CLI 1.4.2's USER_EXPLICIT turn, verbatim shape.
            (
                json!(
                    [
                        "<USER_REQUEST>",
                        "what breaks if I rename renderPosts",
                        "</USER_REQUEST>",
                        "<ADDITIONAL_METADATA>",
                        "The current local time is: 2026-10-20T19:50:30+02:00.",
                        "</ADDITIONAL_METADATA>",
                        "<USER_SETTINGS_CHANGE>",
                        "The user changed setting `Model Selection` from None to Gemini 3.8 Flash (High).",
                        "</USER_SETTINGS_CHANGE>",
                    ]
                    .join("\n")
                ),
                "content",
                "what breaks if I rename renderPosts",
            ),
        ] {
            let mut user = json!({"role":"user"});
            user[field] = content;
            std::fs::write(
                &path,
                [
                    user.to_string(),
                    json!({"role":"assistant","content":"assistant text"}).to_string(),
                ]
                .join("\n"),
            )
            .unwrap();
            assert_eq!(
                antigravity_prompt(&json!({"transcriptPath":path})).as_deref(),
                Some(expected)
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn antigravity_user_request_should_keep_only_the_typed_request() {
        assert_eq!(
            antigravity_user_request(
                "<USER_REQUEST>\nfix it\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nt\n</ADDITIONAL_METADATA>"
            ),
            "fix it"
        );
        assert_eq!(
            antigravity_user_request("<USER_REQUEST>\n\n</USER_REQUEST>"),
            ""
        );
        for plain in [
            "plain prompt",
            "<USER_REQUEST>\nunclosed",
            "quote <USER_REQUEST>x</USER_REQUEST> mid-text",
        ] {
            assert_eq!(antigravity_user_request(plain), plain, "{plain}");
        }
    }

    #[test]
    fn antigravity_workspace_should_skip_an_unindexed_first_root() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent = std::env::temp_dir().join(format!(
            "pixel-antigravity-workspaces-{}-{unique}",
            std::process::id()
        ));
        let unindexed = parent.join("first");
        let indexed = parent.join("second");
        let explicit = parent.join("cwd");
        for root in [&unindexed, &indexed, &explicit] {
            std::fs::create_dir_all(root.join(".git")).unwrap();
        }
        for root in [&indexed, &explicit] {
            let shard = root
                .join(pixel_index::index::SHARD_DIR)
                .join(pixel_index::index::SHARD_FILE);
            std::fs::create_dir_all(shard.parent().unwrap()).unwrap();
            std::fs::write(shard, b"fixture").unwrap();
        }

        let payload = json!({
            "workspacePaths": [unindexed.clone(), indexed.clone()],
        });
        assert_eq!(
            payload_cwd(TaskProvider::Antigravity, &payload),
            indexed.canonicalize().unwrap()
        );
        let payload = json!({
            "cwd": explicit.clone(),
            "workspacePaths": [indexed.clone(), unindexed],
        });
        assert_eq!(
            payload_cwd(TaskProvider::Antigravity, &payload),
            explicit.canonicalize().unwrap()
        );

        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn antigravity_native_user_input_should_inject_a_brief_from_its_workspace() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "pixel-antigravity-native-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/fetch.ts"), "function fetchUser() {}\n").unwrap();
        let initialized = Command::new("git")
            .current_dir(&root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["init", "-q"])
            .status()
            .expect("git is installed for the fixture");
        assert!(initialized.success());
        let staged = Command::new("git")
            .current_dir(&root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["add", "."])
            .status()
            .expect("git is installed for the fixture");
        assert!(staged.success());
        let committed = Command::new("git")
            .current_dir(&root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "-c",
                "user.name=Pixel Test",
                "-c",
                "user.email=pixel-test@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ])
            .status()
            .expect("git is installed for the fixture");
        assert!(committed.success());
        let root = root.canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".pixel")).unwrap();
        std::fs::write(
            root.join(".pixel/config.yaml"),
            "daemon_auto_start: false\n",
        )
        .unwrap();
        pixel_index::index::build(&root, &pixel_index::TrigramExtractor).unwrap();
        let graph = root
            .join(pixel_index::index::SHARD_DIR)
            .join(pixel_daemon::api::GRAPH_DB_FILE);
        std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
        pixel_graph::build::build_graph(&root, &graph).unwrap();
        let transcript = root.join("transcript.jsonl");
        std::fs::write(
            &transcript,
            json!({
                "source": "USER_EXPLICIT",
                "type": "USER_INPUT",
                "content": "Where is `fetchUser` defined?"
            })
            .to_string(),
        )
        .unwrap();

        let payload = json!({
            "invocationNum": 0,
            "transcriptPath": transcript,
            "workspacePaths": [root.clone()],
        });
        assert_eq!(
            brief_prompt(
                TaskProvider::Antigravity,
                TaskHookEvent::PromptSubmit,
                &payload
            )
            .as_deref(),
            Some("Where is `fetchUser` defined?")
        );
        assert_eq!(payload_cwd(TaskProvider::Antigravity, &payload), root);
        let gate = crate::execution_brief::chain::Gate::read(&root);
        assert!(gate.enabled && gate.indexed, "{gate:?}");
        let brief = start_brief(
            TaskProvider::Antigravity,
            TaskHookEvent::PromptSubmit,
            &payload,
            1,
        )
        .expect("the native prompt starts a brief from the indexed workspace");
        let context = finish_brief(
            TaskProvider::Antigravity,
            TaskHookEvent::PromptSubmit,
            &payload,
            Some(brief),
        )
        .expect("the indexed fixture returns read-only evidence");
        let decision = with_context(json!({"decision":"observe"}), Some(context));
        let output = envelope(
            TaskProvider::Antigravity,
            TaskHookEvent::PromptSubmit,
            &decision,
        );
        let message = output["injectSteps"][0]["ephemeralMessage"]
            .as_str()
            .unwrap_or_else(|| {
                panic!("Antigravity receives its brief through injectSteps: {output}")
            });
        assert!(message.starts_with("[PIXEL:BRIEF]\n"), "{message}");
        assert!(message.contains("src/fetch.ts:1"), "{message}");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prompt_dispatch_should_gate_antigravity_and_accept_gemini_before_agent() {
        let path =
            std::env::temp_dir().join(format!("pixel-antigravity-dispatch-{}", std::process::id()));
        std::fs::write(
            &path,
            json!({"role":"user","content":"where is fetchUser defined?"}).to_string(),
        )
        .unwrap();
        let antigravity = json!({"invocationNum":0,"transcriptPath":path});
        assert_eq!(
            brief_prompt(
                TaskProvider::Antigravity,
                TaskHookEvent::PromptSubmit,
                &antigravity
            )
            .as_deref(),
            Some("where is fetchUser defined?")
        );
        assert!(
            brief_prompt(
                TaskProvider::Antigravity,
                TaskHookEvent::PromptSubmit,
                &json!({"invocationNum":1,"transcriptPath":path})
            )
            .is_none()
        );
        assert_eq!(
            brief_prompt(
                TaskProvider::Gemini,
                TaskHookEvent::PromptSubmit,
                &json!({"prompt":"where is fetchUser defined?"})
            )
            .as_deref(),
            Some("where is fetchUser defined?")
        );
        assert!(
            brief_prompt(
                TaskProvider::Gemini,
                TaskHookEvent::SessionStart,
                &json!({"prompt":"where is fetchUser defined?"})
            )
            .is_none()
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn gemini_and_antigravity_should_receive_their_prompt_context_envelopes() {
        let decision = json!({"decision":"observe","context":"[PIXEL:BRIEF]\nfiles: src/a.ts:2"});
        assert_eq!(
            envelope(TaskProvider::Gemini, TaskHookEvent::PromptSubmit, &decision),
            json!({"hookSpecificOutput":{
                "hookEventName":"BeforeAgent",
                "additionalContext":"[PIXEL:BRIEF]\nfiles: src/a.ts:2"
            }})
        );
        assert_eq!(
            envelope(
                TaskProvider::Antigravity,
                TaskHookEvent::PromptSubmit,
                &decision
            ),
            json!({"injectSteps":[{"ephemeralMessage":"[PIXEL:BRIEF]\nfiles: src/a.ts:2"}]})
        );
    }

    /// A repository (a `.git` directory is enough to be discovered) and the
    /// prompt-submit payload a host would send from it.
    fn parts_repo(tag: &str) -> (std::path::PathBuf, String) {
        let root = std::env::temp_dir().join(format!(
            "pixel-task-hook-parts-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let root = root.canonicalize().unwrap();
        let raw = json!({
            "session_id": "session-9",
            "prompt": "where is `fetchUser` used",
            "cwd": root,
            "hook_event_name": "UserPromptSubmit",
        })
        .to_string();
        (root, raw)
    }

    fn part(index: u8, count: u8) -> BriefPart {
        BriefPart { index, count }
    }

    fn brief_path(root: &Path, raw: &str) -> std::path::PathBuf {
        let payload: Value = serde_json::from_str(raw).unwrap();
        part_path(TaskProvider::Claude, &payload).unwrap_or_else(|| root.join("no-path"))
    }

    /// A brief of `lines` lines of 100 characters, line 1 `[PIXEL:BRIEF]`.
    fn big_brief(lines: usize) -> String {
        let mut text = String::from("[PIXEL:BRIEF]\n");
        for n in 1..lines {
            text.push_str(&format!("{n:04} {}\n", "z".repeat(94)));
        }
        text
    }

    fn context_of(output: &Value) -> &str {
        output["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
    }

    #[test]
    fn a_later_part_prints_its_slice_in_the_hook_envelope() {
        let (root, raw) = parts_repo("slice");
        let brief = big_brief(250);
        brief_parts::store(&brief_path(&root, &raw), &brief, SystemTime::now()).unwrap();
        let mut joined = String::new();
        for index in 1..=3 {
            let output =
                follow(TaskProvider::Claude, &raw, part(index, 3), Duration::ZERO).unwrap();
            assert_eq!(
                output["hookSpecificOutput"]["hookEventName"],
                "UserPromptSubmit"
            );
            assert_eq!(output.as_object().unwrap().len(), 1, "{output}");
            assert!(context_of(&output).chars().count() <= 9_800);
            joined.push_str(context_of(&output));
        }
        assert_eq!(joined, brief, "the parts together are the brief");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_later_part_waits_for_the_first_and_prints_what_arrives() {
        let (root, raw) = parts_repo("waits");
        let path = brief_path(&root, &raw);
        let brief = big_brief(150);
        let writer = {
            let (path, brief) = (path, brief.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                brief_parts::store(&path, &brief, SystemTime::now()).unwrap();
                brief_parts::mark_done(&path);
            })
        };
        let started = std::time::Instant::now();
        let output = follow(
            TaskProvider::Claude,
            &raw,
            part(2, 3),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "{:?}",
            started.elapsed()
        );
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            context_of(&output),
            brief_parts::slice(&brief, part(2, 3), Limits::CHARS)
        );
        writer.join().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_later_part_that_times_out_prints_nothing() {
        let (root, raw) = parts_repo("timeout");
        let started = std::time::Instant::now();
        assert_eq!(
            follow(
                TaskProvider::Claude,
                &raw,
                part(2, 3),
                Duration::from_millis(100)
            ),
            None
        );
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert!(started.elapsed() < Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn native_hooks_get_more_decision_time_while_pi_stays_inside_its_host_deadline() {
        assert_eq!(decision_timeout(TaskProvider::Pi), Duration::from_secs(6));
        for provider in [
            TaskProvider::Claude,
            TaskProvider::Codex,
            TaskProvider::Devin,
            TaskProvider::Gemini,
            TaskProvider::Antigravity,
        ] {
            assert_eq!(
                decision_timeout(provider),
                Duration::from_secs(8),
                "{provider:?}"
            );
            assert!(follow_bound(provider) < Duration::from_secs(9));
        }
        assert!(
            crate::hook_input::STDIN_WAIT + follow_bound(TaskProvider::Pi) < Duration::from_secs(8)
        );
        assert!(
            crate::hook_input::STDIN_WAIT + follow_bound(TaskProvider::Claude)
                < Duration::from_secs(10)
        );
    }

    #[test]
    fn a_later_part_with_no_slice_prints_an_empty_object_and_with_no_prompt_nothing() {
        let (root, raw) = parts_repo("empty");
        let path = brief_path(&root, &raw);
        // The brief is one piece long: parts 2 and 3 have nothing.
        brief_parts::store(&path, "[PIXEL:BRIEF]\nshort\n", SystemTime::now()).unwrap();
        for index in [2, 3] {
            assert_eq!(
                follow(TaskProvider::Claude, &raw, part(index, 3), Duration::ZERO),
                Some(json!({}))
            );
        }
        // No brief at all (stored empty): the same, and without waiting.
        brief_parts::store(&path, "", SystemTime::now()).unwrap();
        brief_parts::mark_done(&path);
        let started = std::time::Instant::now();
        assert_eq!(
            follow(
                TaskProvider::Claude,
                &raw,
                part(2, 3),
                Duration::from_secs(30)
            ),
            Some(json!({}))
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        // A payload that names no prompt, no repository or no JSON waits for nothing.
        let started = std::time::Instant::now();
        for bad in [
            "not json".to_owned(),
            json!({"session_id": "s", "cwd": root}).to_string(),
            json!({"session_id": "s", "prompt": "p", "cwd": root.join("absent")}).to_string(),
        ] {
            assert_eq!(
                follow(
                    TaskProvider::Claude,
                    &bad,
                    part(2, 3),
                    Duration::from_secs(30)
                ),
                None,
                "{bad}"
            );
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Another session, another prompt or another repository never reads the
    /// stored brief of this one.
    #[test]
    fn the_parts_of_one_prompt_never_read_the_brief_of_another() {
        let (root, raw) = parts_repo("keyed");
        brief_parts::store(&brief_path(&root, &raw), &big_brief(150), SystemTime::now()).unwrap();
        let mut other_session: Value = serde_json::from_str(&raw).unwrap();
        other_session["session_id"] = json!("session-10");
        let mut other_prompt: Value = serde_json::from_str(&raw).unwrap();
        other_prompt["prompt"] = json!("where is `fetchOrder` used");
        for payload in [other_session, other_prompt] {
            assert_eq!(
                follow(
                    TaskProvider::Claude,
                    &payload.to_string(),
                    part(2, 3),
                    Duration::ZERO
                ),
                None,
                "{payload}"
            );
        }
        assert!(follow(TaskProvider::Claude, &raw, part(2, 3), Duration::ZERO).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_timed_out_first_part_should_emit_its_late_slice_before_releasing_later_parts() {
        let (root, raw) = parts_repo("late-first");
        let path = brief_path(&root, &raw);
        let brief = format!("{}\n", "a".repeat(9_000)).repeat(2);
        let later_raw = raw.clone();
        let (later_tx, later_rx) = std::sync::mpsc::channel();
        let later = std::thread::spawn(move || {
            let result = follow(
                TaskProvider::Claude,
                &later_raw,
                part(2, 3),
                Duration::from_secs(2),
            );
            let _ = later_tx.send(result);
        });
        let worker_path = path.clone();
        let worker_brief = brief.clone();
        let late = first_part_decision(
            TaskProvider::Claude,
            TaskHookEvent::PromptSubmit,
            &raw,
            Some(part(1, 3)),
            Duration::from_millis(20),
            move || {
                std::thread::sleep(Duration::from_millis(150));
                brief_parts::store(&worker_path, &worker_brief, SystemTime::now()).unwrap();
                json!({"decision": "observe", "context": "worker finished after timeout"})
            },
            |_| false,
        );
        assert_eq!(context_of(&late), format!("{}\n", "a".repeat(9_000)));
        let delivered_head = delivered_first_slice(TaskProvider::Claude, &raw, part(1, 3), &late);
        assert!(delivered_head);
        // The later hook has the stored brief but cannot pass part 1 until
        // part 1's response is flushed and its done marker appears.
        std::thread::sleep(Duration::from_millis(40));
        assert!(
            matches!(
                later_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "later part escaped before part 1"
        );
        let mut response = Vec::new();
        emit_first_part(&mut response, Some(&late), Some(&path), delivered_head);
        assert_eq!(response, format!("{late}\n").as_bytes());
        assert!(path.with_extension("done").is_file());
        assert_eq!(
            context_of(
                &later_rx
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap()
                    .unwrap()
            ),
            format!("{}\n", "a".repeat(9_000))
        );
        later.join().unwrap();
        // Only split prompt-submit part 1 uses the fallback.
        for (event, part) in [
            (TaskHookEvent::PromptSubmit, Some(part(2, 3))),
            (TaskHookEvent::PromptSubmit, Some(part(1, 1))),
            (TaskHookEvent::PromptSubmit, None),
            (TaskHookEvent::Stop, Some(part(1, 3))),
        ] {
            assert!(
                late_first_part(TaskProvider::Claude, event, &raw, part).is_none(),
                "{event:?} {part:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_timed_out_first_part_does_not_recover_a_previous_identical_prompt_cache() {
        let (root, raw) = parts_repo("stale-first-part");
        let path = brief_path(&root, &raw);
        brief_parts::store(&path, "stale cached brief\n", SystemTime::now()).unwrap();

        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let output = first_part_decision(
            TaskProvider::Claude,
            TaskHookEvent::PromptSubmit,
            &raw,
            Some(part(1, 3)),
            Duration::from_millis(20),
            move || {
                std::thread::sleep(Duration::from_millis(150));
                let _ = finished_tx.send(());
                json!({"decision": "observe", "context": "fresh worker finished"})
            },
            |_| false,
        );

        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            !output.to_string().contains("stale cached brief"),
            "a timed-out hook must not serve the previous identical prompt's cache"
        );
        assert!(
            !path.exists(),
            "cache was invalidated before the worker started"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_later_part_should_supply_the_head_only_after_an_explicit_missing_head_marker() {
        let (root, raw) = parts_repo("missing-head-fallback");
        let path = brief_path(&root, &raw);
        let brief = format!("{}\n", "a".repeat(9_000)).repeat(2);
        brief_parts::mark_done_missing_head(&path);
        brief_parts::store(&path, &brief, SystemTime::now()).unwrap();

        let (part_two, recovered_head) =
            follow_with_status(TaskProvider::Claude, &raw, part(2, 3), Duration::ZERO).unwrap();
        assert_eq!(context_of(&part_two), format!("{}\n", "a".repeat(9_000)));
        assert!(recovered_head);
        assert!(should_relay_metrics(
            TaskProvider::Claude,
            TaskHookEvent::PromptSubmit,
            Some(part(2, 3)),
            recovered_head,
            Some(&raw),
            &part_two,
        ));

        let (part_three, recovered_head) =
            follow_with_status(TaskProvider::Claude, &raw, part(3, 3), Duration::ZERO).unwrap();
        assert_eq!(context_of(&part_three), format!("{}\n", "a".repeat(9_000)));
        assert!(
            !recovered_head,
            "part 3 carries the second slice, not the head"
        );

        brief_parts::forget(&path);
        brief_parts::store(&path, &brief, SystemTime::now()).unwrap();
        brief_parts::mark_done(&path);
        let (part_two, recovered_head) =
            follow_with_status(TaskProvider::Claude, &raw, part(2, 3), Duration::ZERO).unwrap();
        assert_eq!(context_of(&part_two), format!("{}\n", "a".repeat(9_000)));
        assert!(
            !recovered_head,
            "the legacy empty marker keeps the old slice map"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn codex_parts_are_cut_by_bytes_and_the_others_by_characters() {
        let (root, raw) = parts_repo("codex");
        // Four lines of 9,001 characters, 18,002 bytes each.
        let brief = format!("{}\n", "é".repeat(9_000)).repeat(4);
        brief_parts::store(&brief_path(&root, &raw), &brief, SystemTime::now()).unwrap();
        let printed = |provider, count: u8| -> Vec<String> {
            (1..=count)
                .map(|index| {
                    follow(provider, &raw, part(index, count), Duration::ZERO)
                        .map(|output| context_of(&output).to_owned())
                        .unwrap()
                })
                .collect()
        };
        let by_chars = printed(TaskProvider::Claude, 4);
        assert!(
            by_chars
                .iter()
                .all(|slice| slice == &format!("{}\n", "é".repeat(9_000)))
        );
        // Codex counts bytes: 4,900 characters fill a part, so a line is two.
        let by_bytes = printed(TaskProvider::Codex, 8);
        assert_eq!(by_bytes.concat(), brief);
        assert!(
            by_bytes.iter().all(|slice| slice.len() <= 9_800),
            "{:?}",
            by_bytes.iter().map(String::len).collect::<Vec<_>>()
        );
        assert_eq!(by_bytes[0].chars().count(), 4_900);
        assert_eq!(part_limits(TaskProvider::Codex), Limits::BYTES);
        for provider in [
            TaskProvider::Claude,
            TaskProvider::Gemini,
            TaskProvider::Antigravity,
            TaskProvider::Devin,
            TaskProvider::Pi,
        ] {
            assert_eq!(part_limits(provider), Limits::CHARS, "{provider:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_first_part_stores_the_whole_brief_and_prints_its_own_slice() {
        // finish_brief reads the debug-only pad variable another test sets.
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let (root, raw) = parts_repo("first");
        let payload: Value = serde_json::from_str(&raw).unwrap();
        let path = brief_path(&root, &raw);
        let printed = publish_first_part(
            TaskProvider::Claude,
            &payload,
            Some(pending_brief()),
            part(1, 3),
        );
        let stored = brief_parts::wait_for(&path, Duration::ZERO, brief_parts::POLL).unwrap();
        assert!(stored.starts_with("[PIXEL:BRIEF]"), "{stored}");
        assert_eq!(
            printed.as_deref(),
            Some(stored.as_str()),
            "a short brief is all in part 1"
        );
        for index in [2, 3] {
            assert_eq!(
                follow(TaskProvider::Claude, &raw, part(index, 3), Duration::ZERO),
                Some(json!({}))
            );
        }

        // No brief for this prompt: part 1 stores an empty one so the others
        // do not wait, and prints nothing.
        brief_parts::forget(&path);
        assert_eq!(
            publish_first_part(TaskProvider::Claude, &payload, None, part(1, 3)),
            None
        );
        assert_eq!(
            brief_parts::wait_for(&path, Duration::ZERO, brief_parts::POLL).as_deref(),
            Some("")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_first_part_outside_a_repository_prints_and_stores_nothing() {
        // finish_brief reads the debug-only pad variable another test sets.
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let payload = json!({"session_id": "s", "prompt": "where is `fetchUser` used", "cwd": "/nonexistent-dir-for-pixel"});
        assert!(part_path(TaskProvider::Claude, &payload).is_none());
        assert_eq!(
            publish_first_part(
                TaskProvider::Claude,
                &payload,
                Some(pending_brief()),
                part(1, 3)
            )
            .map(|slice| slice.starts_with("[PIXEL:BRIEF]")),
            Some(true)
        );
    }

    /// The debug-only variable grows a brief to the parts it asks for; absent
    /// or out of range it leaves the brief alone.
    #[test]
    #[cfg(debug_assertions)]
    fn the_test_pad_variable_makes_the_brief_exactly_as_many_parts_as_asked() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let saved = std::env::var_os(brief_parts::TEST_PAD_ENV);
        let set = |value: Option<&str>| match value {
            // SAFETY: the variable is only changed under ENV_LOCK.
            Some(value) => unsafe { std::env::set_var(brief_parts::TEST_PAD_ENV, value) },
            // SAFETY: same lock as above.
            None => unsafe { std::env::remove_var(brief_parts::TEST_PAD_ENV) },
        };
        set(None);
        assert_eq!(forced_oversize(TaskProvider::Claude, None), None);
        assert_eq!(
            forced_oversize(TaskProvider::Claude, Some("brief".into())),
            Some("brief".into())
        );
        for ignored in ["0", "9", "-1", "many", ""] {
            set(Some(ignored));
            assert_eq!(
                forced_oversize(TaskProvider::Claude, None),
                None,
                "{ignored:?}"
            );
        }
        for provider in [TaskProvider::Claude, TaskProvider::Codex] {
            set(Some("3"));
            let padded = forced_oversize(provider, Some("[PIXEL:BRIEF]\nsmall\n".into())).unwrap();
            let pieces = brief_parts::split(&padded, part_limits(provider));
            assert_eq!(pieces.len(), 3, "{provider:?}");
            assert!(pieces[0].starts_with("[PIXEL:BRIEF]\nsmall\n"));
            for (k, piece) in (1..=3).zip(&pieces) {
                assert!(piece.ends_with(&brief_parts::marker(k, 3)), "{piece:?}");
            }
            // Even a prompt with no brief gets an oversized one.
            assert_eq!(
                brief_parts::split(
                    &forced_oversize(provider, None).unwrap(),
                    part_limits(provider)
                )
                .len(),
                3
            );
        }
        match saved {
            // SAFETY: same lock as above.
            Some(value) => unsafe { std::env::set_var(brief_parts::TEST_PAD_ENV, value) },
            // SAFETY: same lock as above.
            None => unsafe { std::env::remove_var(brief_parts::TEST_PAD_ENV) },
        }
    }

    #[test]
    fn a_brief_is_added_to_a_decision_unless_it_has_a_context() {
        let plain = json!({"decision": "observe"});
        assert_eq!(
            with_context(plain.clone(), Some("brief".into())),
            json!({"decision": "observe", "context": "brief"})
        );
        assert_eq!(with_context(plain.clone(), None), plain);
        let own = json!({"decision": "observe", "context": "own"});
        assert_eq!(with_context(own.clone(), Some("brief".into())), own);
    }

    /// Any JSON value a few levels deep, its strings sometimes the tool
    /// names and shell commands the gate parses.
    fn any_value() -> impl proptest::strategy::Strategy<Value = Value> {
        use proptest::prelude::*;
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::from),
            any::<i64>().prop_map(Value::from),
            any::<f64>().prop_map(|f| json!(f)),
            any::<String>().prop_map(Value::from),
            prop::sample::select(vec!["Edit", "Write", "Bash", "shell", "exec_command"])
                .prop_map(Value::from),
            "(sed|git|rg|sort|uniq|pixel|cat) [-a-z ='/|;&>$(){}]{0,40}".prop_map(Value::from),
        ];
        leaf.prop_recursive(3, 32, 6, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..6).prop_map(Value::from),
                prop::collection::btree_map(any::<String>(), inner, 0..6)
                    .prop_map(|map| Value::Object(map.into_iter().collect())),
            ]
        })
    }

    /// A payload shaped like a host's, every field of any shape.
    fn any_payload() -> impl proptest::strategy::Strategy<Value = Value> {
        use proptest::prelude::*;
        let fields = [
            "tool_name",
            "tool_input",
            "session_id",
            "turn_id",
            "tool_use_id",
            "cwd",
            "prompt",
            "success",
            "stop_reason",
            "cancelled",
            "usage",
        ];
        (
            prop::collection::vec(prop::option::of(any_value()), fields.len()),
            any_value(),
        )
            .prop_map(move |(values, other)| {
                let mut payload: serde_json::Map<String, Value> = fields
                    .iter()
                    .zip(values)
                    .filter_map(|(key, value)| Some(((*key).to_owned(), value?)))
                    .collect();
                payload.insert("other".to_owned(), other);
                Value::Object(payload)
            })
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 512,
            failure_persistence: None,
            ..proptest::test_runner::Config::default()
        })]

        /// The gate's parsing of a host payload never panics, whatever the
        /// payload (a panic there is a hook that answers nothing), and what
        /// any decision becomes on the host's stdout is one JSON object on
        /// one line (pixel-mvp's C-32).
        #[test]
        fn any_payload_should_normalize_and_any_decision_should_be_one_json_line(
            payload in proptest::prelude::prop_oneof![any_payload(), any_value()],
            // The ledger decides in objects (`task_bridge::handle_hook`,
            // [`unavailable`]); Pi gets that object as it is.
            decision in proptest::prelude::prop_oneof![
                any_payload(),
                proptest::sample::select(vec![
                    json!({"decision": "deny", "reason": "r"}),
                    json!({"decision": "continue", "reason": "r"}),
                    json!({"decision": "observe", "context": "c"}),
                    json!({"decision": "deny"}),
                ]),
            ],
        ) {
            let normalized = normalize(TaskHookEvent::PreToolUse, &payload);
            proptest::prop_assert!(normalized.is_object(), "{}", normalized);
            for provider in <TaskProvider as clap::ValueEnum>::value_variants() {
                for event in <TaskHookEvent as clap::ValueEnum>::value_variants() {
                    let normalized = normalize(*event, &payload);
                    proptest::prop_assert!(normalized.is_object(), "{}", normalized);
                    for answer in [
                        envelope(*provider, *event, &decision),
                        envelope(*provider, *event, &unavailable(*event, Some(&payload), true)),
                    ] {
                        let line = answer.to_string();
                        proptest::prop_assert!(!line.contains('\n'), "{}", line);
                        let parsed: Value = serde_json::from_str(&line).unwrap();
                        proptest::prop_assert!(parsed.is_object(), "{}", line);
                    }
                }
            }
        }

        /// Without enforcement, a ledger that cannot answer blocks nothing,
        /// whatever the payload: every host but Pi gets an empty object, and
        /// Pi an observation. Only an enforced session is gated.
        #[test]
        fn an_unenforced_session_should_never_be_blocked_by_an_unavailable_ledger(
            payload in proptest::prelude::prop_oneof![any_payload(), any_value()],
        ) {
            for provider in <TaskProvider as clap::ValueEnum>::value_variants() {
                for event in <TaskHookEvent as clap::ValueEnum>::value_variants() {
                    let decision = unavailable(*event, Some(&payload), false);
                    proptest::prop_assert_eq!(&decision["decision"], "observe");
                    let answer = envelope(*provider, *event, &decision);
                    if *provider == TaskProvider::Pi {
                        proptest::prop_assert_eq!(&answer, &decision);
                    } else {
                        proptest::prop_assert_eq!(&answer, &json!({}));
                    }
                }
            }
        }
    }

    #[test]
    fn audited_context_shapes_should_read_each_native_host_and_reject_wrong_types() {
        assert_eq!(
            context_in_output(TaskProvider::Pi, &json!({"context":"pi"})),
            Some("pi")
        );
        assert_eq!(
            context_in_output(
                TaskProvider::Antigravity,
                &json!({"injectSteps":[{"ephemeralMessage":"agy"}]})
            ),
            Some("agy")
        );
        assert_eq!(
            context_in_output(
                TaskProvider::Claude,
                &json!({"hookSpecificOutput":{"additionalContext":"claude"}})
            ),
            Some("claude")
        );
        assert_eq!(
            context_in_output(TaskProvider::Pi, &json!({"context":false})),
            None
        );
        assert_eq!(
            context_in_output(TaskProvider::Antigravity, &json!({"injectSteps":[]})),
            None
        );
    }

    #[test]
    fn audited_metrics_should_follow_only_prompt_delivery_and_the_first_or_recovered_part() {
        let output = json!({"hookSpecificOutput":{"additionalContext":"brief"}});
        for (event, index, recovered, expected) in [
            (TaskHookEvent::PromptSubmit, 1, false, true),
            (TaskHookEvent::PromptSubmit, 2, false, false),
            (TaskHookEvent::PromptSubmit, 2, true, true),
            (TaskHookEvent::SessionStart, 1, false, false),
        ] {
            assert_eq!(
                should_relay_metrics(
                    TaskProvider::Claude,
                    event,
                    Some(part(index, 3)),
                    recovered,
                    None,
                    &output
                ),
                expected
            );
        }
    }

    #[test]
    fn audited_follow_bound_should_include_both_late_and_ordering_grace() {
        assert_eq!(LATE_GRACE, Duration::from_millis(500));
        assert_eq!(ORDERING_SLACK, Duration::from_millis(250));
        for provider in [TaskProvider::Claude, TaskProvider::Codex, TaskProvider::Pi] {
            assert_eq!(
                follow_bound(provider).checked_sub(decision_timeout(provider)),
                Some(Duration::from_millis(750))
            );
        }
    }

    #[test]
    fn audited_first_slice_detection_should_require_the_matching_nonempty_cached_head() {
        let (root, raw) = parts_repo("audited-delivery");
        let path = brief_path(&root, &raw);
        brief_parts::store(&path, "head", SystemTime::now()).unwrap();
        let output = json!({"hookSpecificOutput":{"additionalContext":"head"}});
        assert!(delivered_first_slice(
            TaskProvider::Claude,
            &raw,
            part(1, 2),
            &output
        ));
        for part in [part(2, 2), part(1, 1)] {
            assert!(!delivered_first_slice(
                TaskProvider::Claude,
                &raw,
                part,
                &output
            ));
        }
        assert!(!delivered_first_slice(
            TaskProvider::Claude,
            &raw,
            part(1, 2),
            &json!({"hookSpecificOutput":{"additionalContext":"wrong"}})
        ));
        assert!(!delivered_first_slice(
            TaskProvider::Claude,
            "broken",
            part(1, 2),
            &output
        ));
        brief_parts::store(&path, "", SystemTime::now()).unwrap();
        assert!(!delivered_first_slice(
            TaskProvider::Claude,
            &raw,
            part(1, 2),
            &json!({"hookSpecificOutput":{"additionalContext":""}})
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn audited_cache_forgetting_should_require_a_split_first_prompt_part() {
        let (root, raw) = parts_repo("audited-forget");
        let path = brief_path(&root, &raw);
        for (event, part, forgotten) in [
            (TaskHookEvent::SessionStart, part(1, 2), false),
            (TaskHookEvent::PromptSubmit, part(2, 2), false),
            (TaskHookEvent::PromptSubmit, part(1, 1), false),
            (TaskHookEvent::PromptSubmit, part(1, 2), true),
        ] {
            brief_parts::store(&path, "cached", SystemTime::now()).unwrap();
            forget_first_part_cache(TaskProvider::Claude, event, &raw, Some(part));
            assert_eq!(path.exists(), !forgotten);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
