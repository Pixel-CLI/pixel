// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Host hook normalization and response envelopes for durable task gates.

use std::io::Write;
use std::iter::Peekable;
use std::path::Path;
use std::str::Chars;
use std::time::Duration;

use serde_json::{Value, json};

/// Hook payloads are bounded independently of the host's output limits (1 MiB).
/// One spelling shared with every other hook entry point, so the cap a host
/// is measured against cannot drift between them.
pub(crate) const MAX_INPUT: u64 = crate::hook_input::MAX_HOOK_INPUT;
// Return a decision before the managed native hooks' ten-second host deadline
// and Pi's eight-second subprocess deadline; a stalled worker dies with run().
const DECISION_TIMEOUT: Duration = Duration::from_secs(6);
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
                || rest.len() == 1
                    && matches!(rest[0].as_str(), "policy" | "metrics" | "--help" | "-h")
        }
        "task" | "task-state" => rest.first().is_some_and(|operation| {
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
                | "review-changes"
                | "repo-state"
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
fn imported_config_host() -> Option<&'static str> {
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
                        matches!(
                            operation.as_str(),
                            "task" | "task-state" | "doctor" | "build-index"
                        )
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

fn payload_cwd(payload: &Value) -> std::path::PathBuf {
    string(payload, &["cwd"]).map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        std::path::PathBuf::from,
    )
}

/// Whether the unanswered decision would have been enforced. Unreadable input
/// and an undiscoverable repository count as enforced.
fn enforcement_applies(provider: TaskProvider, payload: Option<&Value>) -> bool {
    let Some(payload) = payload else {
        return true;
    };
    crate::discover_root(&payload_cwd(payload))
        .ok()
        .is_none_or(|root| {
            crate::task_bridge::fallback_enforced(
                &root,
                provider.as_str(),
                string(payload, &["session_id", "sessionId"]),
            )
        })
}

fn process(provider: TaskProvider, event: TaskHookEvent, raw: &str) -> Value {
    let Ok(payload) = serde_json::from_str::<Value>(raw) else {
        return envelope(provider, event, &unavailable(event, None, true));
    };
    // The brief runs beside the ledger: it needs neither, and a slow ledger
    // must not use up its window.
    let brief = start_brief(provider, event, &payload);
    let decision =
        handle_at(&payload_cwd(&payload), provider, event, &payload).unwrap_or_else(|_| {
            unavailable(
                event,
                Some(&payload),
                enforcement_applies(provider, Some(&payload)),
            )
        });
    envelope(provider, event, &with_brief(decision, brief))
}

fn brief_prompt(provider: TaskProvider, event: TaskHookEvent, payload: &Value) -> Option<String> {
    if provider == TaskProvider::Pi || event != TaskHookEvent::PromptSubmit {
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

/// Start the evidence brief for a Claude or Codex prompt; Pi, every other
/// event and a prompt outside a repository start none.
fn start_brief(
    provider: TaskProvider,
    event: TaskHookEvent,
    payload: &Value,
) -> Option<crate::execution_brief::chain::Pending> {
    let prompt = brief_prompt(provider, event, payload)?;
    let root = crate::discover_root(&payload_cwd(payload)).ok()?;
    crate::execution_brief::chain::start(&prompt, &root)
}

/// The latest user turn in the transcript `transcriptPath` points at. The
/// file is JSONL; a turn is any entry whose role reads "user", its text the
/// `content`/`text` field (string or a parts array of `{"text": ...}`).
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
        if role != Some("user") && role != Some("human") {
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
        if let Some(content) = content.filter(|text| !text.trim().is_empty()) {
            last = Some(content);
        }
    }
    last
}

/// `decision` with the brief as its `context`, which the envelope delivers as
/// the host's additional context. A decision that already carries a context
/// keeps it.
fn with_brief(mut decision: Value, brief: Option<crate::execution_brief::chain::Pending>) -> Value {
    if decision.get("context").is_none()
        && let Some(text) = brief.and_then(crate::execution_brief::chain::Pending::finish)
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

fn bounded_decision(
    provider: TaskProvider,
    event: TaskHookEvent,
    raw: &str,
    timeout: Duration,
    evaluate: impl FnOnce() -> Value + Send + 'static,
    enforced: impl FnOnce(Option<&Value>) -> bool,
) -> Value {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    if std::thread::Builder::new()
        .name("pixel-task-gate".into())
        .spawn(move || {
            let _ = sender.send(evaluate());
        })
        .is_ok()
        && let Ok(decision) = receiver.recv_timeout(timeout)
    {
        return decision;
    }
    let payload = serde_json::from_str(raw).ok();
    let enforced = enforced(payload.as_ref());
    envelope(
        provider,
        event,
        &unavailable(event, payload.as_ref(), enforced),
    )
}

/// Read one bounded host event, dispatch it, and emit only the host's schema.
pub fn run(provider: TaskProvider, event: TaskHookEvent) -> ! {
    if provider == TaskProvider::Claude && imported_config_host().is_some() {
        std::process::exit(0);
    }
    let output =
        if let Some(raw) = crate::hook_input::read_bounded(&mut std::io::stdin(), MAX_INPUT) {
            let input = raw.clone();
            bounded_decision(
                provider,
                event,
                &raw,
                DECISION_TIMEOUT,
                move || process(provider, event, &input),
                |payload| enforcement_applies(provider, payload),
            )
        } else {
            envelope(provider, event, &unavailable(event, None, true))
        };
    // Flush the only response before exit terminates any stalled evaluator.
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{output}");
    let _ = stdout.flush();
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "rtk proxy pixel task status abc",
            "pixel doctor . --fix",
            "git diff",
            "cat src/a.rs",
        ] {
            assert!(!shell_mutates(command), "{command}");
        }
        for command in [
            "python3 mutate.py",
            "cat x > y",
            "pixel commit --files x",
            "git checkout x",
            "rtk proxy rm x",
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
            "pixel config policy off",
            "pixel config edit --repo",
            "pixel config setup",
            "pixel task evaluate --suite external.json",
            "pixel task reset session",
            "pixel task-state evaluate --suite external.json",
            "pixel task-state reset session",
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
            "pixel config policy",
            "pixel config metrics",
            "pixel task prepare task-1",
            "pixel task verify task-1",
            "pixel task recover task-1",
            "pixel task contract task-1 --file contract.json",
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
            "rg needle | pixel task prepare task-1",
            "rg needle | pixel task-state prepare task-1",
            "rg needle; pixel task prepare task-1",
            "rg needle || rm source.txt",
            "cat source.txt && sed -i s/a/b/ source.txt",
            "rg needle | cat > source.txt",
            "rg $(touch source.txt) | sort",
            "rg \"$(touch source.txt)\" | sort",
            "pixel task contract task-1 --definition $(cat secret)",
            "pixel task contract task-1 --definition \"$(cat secret)\"",
        ] {
            assert!(shell_mutates(command), "{command}");
        }
        let definition =
            json!({"checks":[{"argv":["/bin/sh","-c","test \"$(cat source.txt)\" = original"]}]})
                .to_string();
        let command = format!("pixel task contract task-1 --definition '{definition}' --json");
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
                process(provider, TaskHookEvent::PreToolUse, "not json")["hookSpecificOutput"]["permissionDecision"],
                "deny"
            );
            assert_eq!(
                process(provider, TaskHookEvent::Stop, "not json")["continue"],
                false
            );
            assert_eq!(
                process(provider, TaskHookEvent::PostToolUse, "not json"),
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
            assert!(start_brief(provider, TaskHookEvent::PromptSubmit, &prompt).is_some());
            for event in [
                TaskHookEvent::PreToolUse,
                TaskHookEvent::PostToolUse,
                TaskHookEvent::Stop,
                TaskHookEvent::SessionStart,
            ] {
                assert!(start_brief(provider, event, &prompt).is_none(), "{event:?}");
            }
        }
        assert!(start_brief(TaskProvider::Pi, TaskHookEvent::PromptSubmit, &prompt).is_none());
        let no_prompt = json!({"cwd":root});
        assert!(
            start_brief(
                TaskProvider::Claude,
                TaskHookEvent::PromptSubmit,
                &no_prompt
            )
            .is_none()
        );
        let elsewhere = json!({"prompt":"callers of `fetchUser`","cwd":root.join("missing")});
        assert!(
            start_brief(
                TaskProvider::Claude,
                TaskHookEvent::PromptSubmit,
                &elsewhere
            )
            .is_none()
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
}
