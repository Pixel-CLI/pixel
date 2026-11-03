// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `flow-rank` (issues #906, #1198): a walkthrough question is answered by
//! the callees that carry its words, not by a depth-first prefix of the
//! entry's calls.
//!
//! Ported from `probes/flow-rank/probe`. The entry is the one the question
//! names in code form (`client.fetch`, `handle_event`) when the graph defines
//! it, else the one the brief's trace names. Every callee of the entry is
//! read (`uses` pages), ranked by the question's words (generic words weigh
//! little, tests are left out, an exact edge weighs a little more), and the
//! best six are followed one level at a time down to depth 3 (from a named
//! entry, a level where fewer than two callees carry the question's words
//! keeps its best three: a narrow chain is the path). The entry is shown in
//! full with each ranked body. Four walkthroughs the graph cannot follow are
//! answered from the source instead: a key chord or a named undo/redo
//! shortcut, an application-to-router request handoff, and a hook event
//! literal with the output it ends in. The step replaces the brief's own flow
//! blocks. Prints nothing without a daemon.

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::sync::LazyLock;

use pixel_plugin::ProbeMode;
use regex::Regex;

use super::graph::{Role, Sym};
use super::text::{head, re, suffix};
use super::{Ctx, Step};

pub(super) const STEP: Step = Step {
    name: "flow-rank",
    when_kinds: &["flow"],
    skip_kinds: &[],
    // A walkthrough the brief did not classify as flow (a "rationale" or
    // unjudged prompt) still gets its chain.
    when_prompt: Some(
        r"(?i)(\bwalk (me )?through\b.*\b(hook|prompt|router|request|press|key|event|response|code|function|method)\b|\b(hook|prompt|router|request|press|key|event|response|code|function|method)\b.*\bwalk (me )?through\b|\b(trace|walk|flow|happens|path)\b.{0,220}\b(hook|prompt)\b|\b(trace|walk|flow|happens|path)\b.{0,120}\b(undo|redo)\b.{0,100}\b(keyboard shortcut|shortcut)\b|\b(how does|walk|trace|flow|happens|path)\b.{0,160}\b(application|app)\b.{0,50}\bcallback\b.{0,180}\brequest\b.{0,160}\b(router|routing|route)\b.{0,160}\b(completion|response)\b.{0,50}\bcallback\b)",
    ),
    when_gate_closed: true,
    budget_ms: 1500,
    mode: ProbeMode::Lead,
    run,
};

/// Question words that say little about the code.
const GENERIC: &[&str] = &[
    "element", "elements", "scene", "file", "files", "data", "state", "value", "type",
];
/// Question words of the frame of a walkthrough.
const NOISE: &[&str] = &[
    "walk", "through", "happens", "happen", "someone", "until", "from", "shows", "when", "onto",
    "that", "file", "with", "what", "into",
];
/// Words that never help to order the source files.
const PATH_STOP: &[&str] = &[
    "walk", "trace", "flow", "happens", "path", "show", "find", "tell", "explain",
];
/// Lines of the entry shown.
const ENTRY_LINES: i64 = 160;
/// Lines of a ranked callee shown.
const BODY_LINES: i64 = 130;
/// Ranked callees shown.
const MAX_RANKED: usize = 12;
/// A callee scoring at most this is not ranked.
const MIN_SCORE: f64 = 1.0;
/// Offset past which a symbol's call edges are not paged.
const MAX_EDGE_OFFSET: usize = 200;
/// Source files read for the source walkthroughs, and their listing prefix.
const SOURCE_FILES: usize = 3000;
/// Bytes of source read for the source walkthroughs, in all.
const SOURCE_SCAN_BYTES: usize = 24_000_000;
/// A source file longer than this is not scanned.
const SOURCE_FILE_BYTES: u64 = 600_000;
/// Wall time of the source scan.
const SOURCE_SCAN_MS: u64 = 950;
/// Characters of a source walkthrough's output at most.
const SOURCE_OUT_CHARS: usize = 5200;
/// A file read whole to quote the entry or a callee.
const WHOLE_FILE_BYTES: u64 = 16_000_000;
const SOURCE_SUFFIXES: &[&str] = &[
    ".ts", ".tsx", ".js", ".jsx", ".py", ".rs", ".go", ".java", ".kt", ".swift",
];


static WORD3: LazyLock<Regex> = LazyLock::new(|| re(r"[A-Za-z][A-Za-z0-9]*"));
static WORD4: LazyLock<Regex> = LazyLock::new(|| re(r"[A-Za-z]{4,}"));
static WALKS: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b(walk|trace|flow|happens|path)\b"));
static CAMEL: LazyLock<Regex> = LazyLock::new(|| re(r"([a-z0-9])([A-Z])"));
static ALNUM: LazyLock<Regex> = LazyLock::new(|| re(r"[a-z0-9]+"));
static CHORD: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(ctrl|control|cmd|command|meta)\s*\+\s*([a-z0-9])\b"));
static UNDO_REDO: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b(undo|redo)\b"));
static SHORTCUT: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(keyboard\s+shortcut|shortcut)\b"));
static MODIFIER: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)ctrl|control|meta|command|cmd"));
static BINDING: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?:export\s+)?(?:const|let|var|function|class)\s+([\w$]+)"));
static KEY_TEST: LazyLock<Regex> = LazyLock::new(|| re(r"\bkeyTest\s*:"));
static EXECUTES: LazyLock<Regex> = LazyLock::new(|| re(r"executeAction\s*\(|\.perform\s*\("));
static REDO_WORD: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\bredo\b"));
static UNDO_WORD: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\bundo\b"));
static FLOW_INTENT: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(walk|trace|flow|happens|path|how does)\b"));
static REQUEST_WORD: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\brequest\b"));
static ROUTING_WORD: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b(router|routing|route)\b"));
static CALLBACK_WORD: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\bcallback\b"));
static APP_CALLBACK: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(application|app)\b.{0,50}\bcallback\b"));
static COMPLETION_WORD: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(response|completion)\b"));
static ROUTER_WORD: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\brouter\b"));
static RESPONSE_WORD: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\bresponse\b"));
static ROUTER_HANDLE: LazyLock<Regex> = LazyLock::new(|| re(r"\.router\.handle\s*\("));
static APP_FUNCTION: LazyLock<Regex> = LazyLock::new(|| re(r"\bapp\s*=\s*function\s*\("));
static APP_HANDLE: LazyLock<Regex> = LazyLock::new(|| re(r"\bapp\.handle\s*\("));
static HOOK_WORD: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\bhook\b"));
static BRIEF_WORD: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\bbrief\b"));
static PROMPT_WORD: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\bprompt\b"));
static SUBMITTED_WORD: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(submitted|submission)\b"));
static OUTPUT_WORD: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(print|printed|prints|write|written|output|delivers?)\b"));
static TEXT_WORD: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b(text|context)\b"));
static SENT_WORD: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b(sent|send|returned|return|back)\b"));
static EVENT_NAME: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b([a-z][a-z0-9]*[-_][a-z][a-z0-9]*)\b"));
static EVENT_MAPPING: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?:Self::|::)([A-Z][A-Za-z0-9_]*)\s*=>\s*['"]([^'"]+)['"]"#));
static ACTIVE_VARIANT: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?:Self::|::)([A-Z][A-Za-z0-9_]*)\s*=>"));
static CAPITALIZED: LazyLock<Regex> = LazyLock::new(|| re(r"[A-Z][a-z0-9]*"));
static RUN_FN: LazyLock<Regex> = LazyLock::new(|| re(r"\bfn\s+run\s*\("));
static EVENT_TYPE: LazyLock<Regex> =
    LazyLock::new(|| re(r"\bevent\s*:\s*([A-Z][A-Za-z0-9_]*)"));
static ANY_FN: LazyLock<Regex> = LazyLock::new(|| re(r"\bfn\s+"));
static PROCESSES_EVENT: LazyLock<Regex> =
    LazyLock::new(|| re(r"\bprocess\w*\s*\([^;\n]*\bevent\b[^;\n]*\)"));
static BRIEF_CALL: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\b[A-Za-z_]\w*brief\w*\s*\("));
static DECLARES: LazyLock<Regex> = LazyLock::new(|| re(r"\b(fn|def|function)\s+"));
static READ_CALL: LazyLock<Regex> = LazyLock::new(|| re(r"read\w*\s*\("));
static EVENT_WORD: LazyLock<Regex> = LazyLock::new(|| re(r"\bevent\b"));
static BACKTICKED: LazyLock<Regex> = LazyLock::new(|| re(r"`([A-Za-z_][\w.]*)`"));
static DOTTED: LazyLock<Regex> = LazyLock::new(|| re(r"\b[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)+\b"));
static CODE_FORM: LazyLock<Regex> =
    LazyLock::new(|| re(r"\b[a-z][a-z0-9]*(?:_[a-z0-9]+)+\b|\b[a-z]+[A-Z]\w*\b"));
static BRIEF_ENTRY: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?m)^entry (\S+) — (\S+?):(\d+)-(\d+)"));

fn run(ctx: &Ctx) -> String {
    let scan = Scan {
        ctx,
        rows: OnceCell::new(),
    };
    let source = shortcut_flow(ctx, &scan)
        .or_else(|| request_flow(ctx, &scan))
        .or_else(|| event_flow(ctx, &scan));
    if let Some(text) = source {
        return format!("drop: .\n{text}\n");
    }
    ranked_flow(ctx).unwrap_or_default()
}

/// A tracked source file read whole: its path and lines.
struct Row {
    path: String,
    lines: Vec<String>,
}

/// The source files the walkthroughs of the source read, scanned once.
struct Scan<'a, 'c> {
    ctx: &'a Ctx<'c>,
    rows: OnceCell<Vec<Row>>,
}

impl Scan<'_, '_> {
    fn rows(&self) -> &[Row] {
        self.rows.get_or_init(|| source_rows(self.ctx))
    }

    /// The rows that are not tests or specs.
    fn code(&self) -> impl Iterator<Item = &Row> {
        self.rows().iter().filter(|row| {
            let path = row.path.to_lowercase();
            !path.contains("test") && !path.contains("spec")
        })
    }
}


/// The tracked source paths ordered by how many words of the prompt their
/// names carry (camel case split), before the bounded read.
fn prioritized_source_paths<'a>(files: &'a [String], prompt: &str) -> Vec<&'a str> {
    let mut terms: BTreeSet<String> = WORD3
        .find_iter(prompt)
        .map(|word| word.as_str().to_lowercase())
        .filter(|word| word.chars().count() > 2)
        .filter(|word| !NOISE.contains(&word.as_str()) && !GENERIC.contains(&word.as_str()))
        .filter(|word| !PATH_STOP.contains(&word.as_str()))
        .collect();
    if WALKS.is_match(prompt) {
        // A traced path may need its top-level caller even when no prompt
        // term appears in that conventional entry filename.
        terms.insert("main".to_owned());
    }
    let overlap = |path: &str| {
        let spaced = CAMEL.replace_all(path, "$1 $2").to_lowercase();
        let tokens: HashSet<&str> = ALNUM.find_iter(&spaced).map(|token| token.as_str()).collect();
        terms.iter().filter(|term| tokens.contains(term.as_str())).count()
    };
    let mut paths: Vec<(usize, &str)> = files
        .iter()
        .take(SOURCE_FILES + 1)
        .map(String::as_str)
        .filter(|path| SOURCE_SUFFIXES.contains(&suffix(path).to_lowercase().as_str()))
        .map(|path| (overlap(path), path))
        .collect();
    paths.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    paths.into_iter().map(|(_, path)| path).collect()
}

/// The source files read for the walkthroughs of the source: most relevant
/// first, within the byte and time bounds.
fn source_rows(ctx: &Ctx) -> Vec<Row> {
    let Some(files) = ctx.files() else {
        return Vec::new();
    };
    let started = std::time::Instant::now();
    let mut rows = Vec::new();
    let mut total = 0;
    for path in prioritized_source_paths(files, ctx.prompt)
        .into_iter()
        .take(SOURCE_FILES + 1)
    {
        if started.elapsed().as_millis() >= u128::from(SOURCE_SCAN_MS) || ctx.timed_out() {
            break;
        }
        if path.starts_with('/') || path.split('/').any(|part| part == "..") {
            continue;
        }
        let Some(text) = ctx.read(path, SOURCE_FILE_BYTES) else {
            continue;
        };
        if total + text.len() > SOURCE_SCAN_BYTES {
            break;
        }
        total += text.len();
        rows.push(Row {
            path: path.to_owned(),
            lines: text.lines().map(ToString::to_string).collect(),
        });
    }
    rows
}

/// `line[:200]` numbered `n`.
fn numbered_line(n: usize, line: &str) -> String {
    format!("{n}| {}", head(line, 200))
}

/// The lines `lo..hi` (0-based, end exclusive) of `lines`, numbered from `lo + 1`.
fn numbered_range(lines: &[String], lo: usize, hi: usize) -> Vec<String> {
    (lo..hi.min(lines.len()))
        .filter_map(|at| lines.get(at).map(|line| numbered_line(at + 1, line)))
        .collect()
}

/// The first `SOURCE_OUT_CHARS` characters of a source walkthrough.
fn clip(out: &[String]) -> String {
    head(&out.join("\n"), SOURCE_OUT_CHARS).to_owned()
}

fn is_comment(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("//") || line.starts_with('#') || line.starts_with('*')
}

/// The key handler found: its rows and the name it dispatches.
struct Action<'r> {
    row: &'r Row,
    lo: usize,
    hi: usize,
    name: String,
    body: String,
}

/// A key chord or a named undo/redo shortcut resolved to its source action,
/// the dispatcher that runs it and the handler it reaches.
fn shortcut_flow(ctx: &Ctx, scan: &Scan) -> Option<String> {
    if !WALKS.is_match(ctx.prompt) {
        return None;
    }
    let chord = CHORD.captures(ctx.prompt);
    let operation = UNDO_REDO.captures(ctx.prompt);
    let named_shortcut = operation.is_some() && SHORTCUT.is_match(ctx.prompt);
    if chord.is_none() && !named_shortcut {
        return None;
    }
    let key = chord
        .as_ref()
        .and_then(|caps| caps.get(2))
        .map(|key| key.as_str().to_uppercase());
    let key_forms = key.as_ref().map(|key| {
        let (upper, lower) = (regex::escape(key), regex::escape(&key.to_lowercase()));
        re(&format!(
            r#"(?i)KEYS\.{upper}\b|event\.key\s*=+\s*['"]{lower}['"]|key\s*=+\s*['"]{lower}['"]"#
        ))
    });
    let expected_name = operation.as_ref().and_then(|caps| caps.get(1)).map(|op| {
        re(&format!(
            r#"(?i)name\s*:\s*['"]{}['"]"#,
            regex::escape(&op.as_str().to_lowercase())
        ))
    });
    let mut action: Option<Action> = None;
    'rows: for row in scan.code() {
        let lines = &row.lines;
        for (at, line) in lines.iter().enumerate() {
            if is_comment(line) {
                continue;
            }
            let (start, end);
            if let Some(forms) = &key_forms {
                if !forms.is_match(line) {
                    continue;
                }
                let context = lines[at.saturating_sub(4)..lines.len().min(at + 5)].join("\n");
                if !MODIFIER.is_match(&context) {
                    continue;
                }
                start = (at.saturating_sub(29)..=at)
                    .rev()
                    .find(|j| BINDING.is_match(&lines[*j]))
                    .unwrap_or(at.saturating_sub(12));
                end = lines.len().min(at + 13);
            } else {
                if !KEY_TEST.is_match(line) {
                    continue;
                }
                start = (at.saturating_sub(39)..=at)
                    .rev()
                    .find(|j| BINDING.is_match(&lines[*j]))
                    .unwrap_or(at.saturating_sub(20));
                end = lines.len().min(at + 8);
                let candidate = lines[start..end].join("\n");
                if !expected_name.as_ref().is_some_and(|name| name.is_match(&candidate)) {
                    continue;
                }
            }
            let joined = lines[start..end].join("\n");
            if !UNDO_REDO.is_match(&joined) {
                continue;
            }
            let name = BINDING
                .captures(&lines[start])
                .and_then(|caps| caps.get(1))
                .map_or("action", |name| name.as_str())
                .to_owned();
            action = Some(Action {
                row,
                lo: start + 1,
                hi: lines.len().min(at + 13),
                name,
                body: joined,
            });
            break 'rows;
        }
    }
    let action = action?;

    let method = match &operation {
        Some(caps) => caps[1].to_lowercase(),
        None if REDO_WORD.is_match(&action.body) && !UNDO_WORD.is_match(&action.body) => {
            "redo".to_owned()
        }
        None => "undo".to_owned(),
    };
    let label = match (&chord, &key) {
        (Some(caps), Some(key)) => format!("{}-{key}", &caps[1]),
        _ => format!("{method} keyboard shortcut (key from source)"),
    };
    let mut out = vec![
        format!("### shortcut flow: {label} action {}", action.name),
        format!(
            "### {}:{}-{} — key action {}",
            action.row.path, action.lo, action.hi, action.name
        ),
    ];
    out.extend(numbered_range(&action.row.lines, action.lo - 1, action.hi));

    let declaration = re(&format!(
        r"^\s*(?:(?:public|private|protected|async|static)\s+)*(?:function\s+|def\s+)?{}\s*\(",
        regex::escape(&method)
    ));
    let handler = scan.code().find_map(|row| {
        row.lines.iter().enumerate().find_map(|(at, line)| {
            let before = line.split_once(method.as_str()).map_or(line.as_str(), |(b, _)| b);
            (declaration.is_match(line) && !before.contains('.')).then_some((row, at))
        })
    });
    let dispatch = scan.code().find_map(|row| {
        row.lines.iter().enumerate().find_map(|(at, line)| {
            if !line.contains("keyTest") {
                return None;
            }
            let region = &row.lines[at.saturating_sub(15)..row.lines.len().min(at + 65)];
            let executes = region.iter().any(|x| EXECUTES.is_match(x));
            if !(region.iter().any(|x| x.contains("actions")) && executes) {
                return None;
            }
            let execute_at = (at..row.lines.len().min(at + 65)).find(|j| EXECUTES.is_match(&row.lines[*j]));
            Some((row, at, execute_at))
        })
    });
    if let Some((row, key_at, execute_at)) = dispatch {
        let first = (key_at.saturating_sub(8), row.lines.len().min(key_at + 8));
        let second = execute_at.map_or(first, |at| {
            (at.saturating_sub(3), row.lines.len().min(at + 4))
        });
        let indices: BTreeSet<usize> = (first.0..first.1).chain(second.0..second.1).collect();
        if let (Some(low), Some(high)) = (indices.first(), indices.last()) {
            out.push(format!(
                "### {}:{}-{} — action dispatcher selection and execution",
                row.path,
                low + 1,
                high + 1
            ));
            out.extend(
                indices
                    .iter()
                    .filter_map(|at| row.lines.get(*at).map(|line| numbered_line(at + 1, line))),
            );
        }
    }
    if let Some((row, at)) = handler {
        let (lo, hi) = (at.saturating_sub(8), row.lines.len().min(at + 9));
        out.push(format!("### {}:{}-{hi} — {method} handler", row.path, lo + 1));
        out.extend(numbered_range(&row.lines, lo, hi));
    }
    Some(clip(&out))
}

/// The application-to-router handoff of a request/response walkthrough.
fn request_flow(ctx: &Ctx, scan: &Scan) -> Option<String> {
    let prompt = ctx.prompt;
    let flow_intent = FLOW_INTENT.is_match(prompt);
    let request_trace = flow_intent
        && REQUEST_WORD.is_match(prompt)
        && ROUTING_WORD.is_match(prompt)
        && CALLBACK_WORD.find_iter(prompt).count() >= 2
        && APP_CALLBACK.is_match(prompt)
        && COMPLETION_WORD.is_match(prompt);
    let legacy_trace = flow_intent
        && REQUEST_WORD.is_match(prompt)
        && ROUTER_WORD.is_match(prompt)
        && RESPONSE_WORD.is_match(prompt);
    if !(request_trace || legacy_trace) {
        return None;
    }
    let (row, lo, hi) = scan.code().find_map(|row| {
        row.lines.iter().enumerate().find_map(|(at, line)| {
            (!is_comment(line) && ROUTER_HANDLE.is_match(line))
                .then(|| (row, at.saturating_sub(18), row.lines.len().min(at + 5)))
        })
    })?;
    let mut out = vec![
        "### request flow: app delegates request/response to router".to_owned(),
        format!("### {}:{}-{hi} — app-to-router handoff", row.path, lo + 1),
    ];
    out.extend(numbered_range(&row.lines, lo, hi));
    for candidate in scan.code() {
        let body = &candidate.lines;
        let entry = body.iter().enumerate().find(|(at, line)| {
            APP_FUNCTION.is_match(line)
                && body[*at..body.len().min(at + 8)]
                    .iter()
                    .any(|item| APP_HANDLE.is_match(item))
        });
        if let Some((at, _)) = entry {
            let (start, end) = (at.saturating_sub(5), body.len().min(at + 9));
            out.push(format!(
                "### {}:{}-{end} — application request entry",
                candidate.path,
                start + 1
            ));
            out.extend(numbered_range(body, start, end));
            break;
        }
    }
    Some(clip(&out))
}

/// Blanks Rust comments and string literals, keeping every other character
/// where it was, so a match in `source` can be tested against the active code.
fn rust_code_mask(source: &str) -> String {
    #[derive(PartialEq)]
    enum State {
        Code,
        Line,
        Block,
        Raw,
        Str,
    }
    let chars: Vec<char> = source.chars().collect();
    let mut out = chars.clone();
    let starts = |at: usize, text: &str| {
        text.chars()
            .enumerate()
            .all(|(k, ch)| chars.get(at + k) == Some(&ch))
    };
    let blank = |out: &mut Vec<char>, from: usize, to: usize| {
        for slot in out.iter_mut().take(to).skip(from) {
            *slot = ' ';
        }
    };
    let (mut at, mut state, mut escaped, mut depth) = (0, State::Code, false, 0_u32);
    let mut raw_end = String::new();
    while at < chars.len() {
        match state {
            State::Code => {
                if starts(at, "//") {
                    blank(&mut out, at, at + 2);
                    at += 2;
                    state = State::Line;
                    continue;
                }
                if starts(at, "/*") {
                    blank(&mut out, at, at + 2);
                    at += 2;
                    (state, depth) = (State::Block, 1);
                    continue;
                }
                let boundary = at == 0
                    || !(chars.get(at - 1).is_some_and(|ch| ch.is_alphanumeric()) || chars[at - 1] == '_');
                if boundary
                    && let Some(open) = raw_string_start(&chars, at)
                {
                    blank(&mut out, at, open.end);
                    raw_end = format!("\"{}", "#".repeat(open.hashes));
                    at = open.end;
                    state = State::Raw;
                    continue;
                }
                if chars[at] == '"' {
                    blank(&mut out, at, at + 1);
                    at += 1;
                    (state, escaped) = (State::Str, false);
                    continue;
                }
            }
            State::Line => {
                if chars[at] == '\n' {
                    state = State::Code;
                } else {
                    out[at] = ' ';
                }
            }
            State::Block => {
                if starts(at, "/*") {
                    blank(&mut out, at, at + 2);
                    depth += 1;
                    at += 2;
                    continue;
                }
                if starts(at, "*/") {
                    blank(&mut out, at, at + 2);
                    depth -= 1;
                    at += 2;
                    if depth == 0 {
                        state = State::Code;
                    }
                    continue;
                }
                if chars[at] != '\n' {
                    out[at] = ' ';
                }
            }
            State::Raw => {
                if starts(at, &raw_end) {
                    let width = raw_end.chars().count();
                    blank(&mut out, at, at + width);
                    at += width;
                    state = State::Code;
                    continue;
                }
                if chars[at] != '\n' {
                    out[at] = ' ';
                }
            }
            State::Str => {
                if chars[at] == '\n' {
                    state = State::Code;
                    at += 1;
                    continue;
                }
                if escaped {
                    out[at] = ' ';
                    escaped = false;
                    at += 1;
                    continue;
                }
                if chars[at] == '\\' {
                    out[at] = ' ';
                    escaped = true;
                    at += 1;
                    continue;
                }
                if chars[at] == '"' {
                    state = State::Code;
                }
                out[at] = ' ';
            }
        }
        at += 1;
    }
    out.into_iter().collect()
}

/// The opening of a raw string literal at `at`: `r"`, `br"`, `r#"`, ...
struct RawOpen {
    /// Where the literal's text starts.
    end: usize,
    hashes: usize,
}

fn raw_string_start(chars: &[char], at: usize) -> Option<RawOpen> {
    let mut pos = at;
    if chars.get(pos) == Some(&'b') {
        pos += 1;
    }
    if chars.get(pos) != Some(&'r') {
        return None;
    }
    pos += 1;
    let hashes = chars[pos..].iter().take_while(|ch| **ch == '#').count();
    pos += hashes;
    (chars.get(pos) == Some(&'"')).then_some(RawOpen {
        end: pos + 1,
        hashes,
    })
}

/// The 0-based character offset of byte offset `at` of `text`.
fn char_offset(text: &str, at: usize) -> usize {
    text.get(..at).map_or(0, |before| before.chars().count())
}

/// A hook event literal resolved to its typed variant, the runner that
/// receives it and the output it ends in.
fn event_flow(ctx: &Ctx, scan: &Scan) -> Option<String> {
    let prompt = ctx.prompt;
    let hook = HOOK_WORD.is_match(prompt);
    let submitted_prompt = PROMPT_WORD.is_match(prompt) && SUBMITTED_WORD.is_match(prompt);
    let context_delivery =
        submitted_prompt && TEXT_WORD.is_match(prompt) && SENT_WORD.is_match(prompt);
    if !hook
        || !(BRIEF_WORD.is_match(prompt) || submitted_prompt)
        || !(OUTPUT_WORD.is_match(prompt) || context_delivery)
    {
        return None;
    }
    let event_match = EVENT_NAME.captures(prompt);
    let semantic_event = event_match.is_none()
        && hook
        && PROMPT_WORD.is_match(prompt)
        && SUBMITTED_WORD.is_match(prompt)
        && TEXT_WORD.is_match(prompt);
    if event_match.is_none() && !semantic_event {
        return None;
    }
    let typed_literal = event_match
        .as_ref()
        .map_or_else(String::new, |caps| caps[1].to_lowercase().replace('_', "-"));
    let event_variant = event_match.as_ref().map_or_else(String::new, |caps| {
        caps[1]
            .split(['-', '_'])
            .map(|part| {
                let mut chars = part.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_uppercase().chain(chars).collect()
                })
            })
            .collect()
    });
    let names_submit = |variant: &str| {
        let parts: Vec<String> = CAPITALIZED
            .find_iter(variant)
            .map(|part| part.as_str().to_lowercase())
            .collect();
        parts.iter().any(|part| part == "prompt") && parts.iter().any(|part| part == "submit")
    };
    let wanted = |variant: &str, literal: &str| {
        if event_match.is_some() {
            variant == event_variant && literal == typed_literal
        } else {
            names_submit(variant)
        }
    };

    // The row, line, variant and literal of the mapping the question names.
    let mut event: Option<(&Row, usize, String, String)> = None;
    for row in scan.code() {
        let has_candidate = row.lines.iter().any(|line| {
            EVENT_MAPPING
                .captures(line)
                .is_some_and(|caps| wanted(&caps[1], &caps[2]))
        });
        if !has_candidate {
            continue;
        }
        let masked = rust_code_mask(&row.lines.join("\n"));
        for (at, (line, active)) in row.lines.iter().zip(masked.lines()).enumerate() {
            let Some(mapping) = EVENT_MAPPING.captures(line) else {
                continue;
            };
            let (Some(whole), Some(variant), Some(literal)) =
                (mapping.get(0), mapping.get(1), mapping.get(2))
            else {
                continue;
            };
            let Some(live) = ACTIVE_VARIANT.captures(active) else {
                continue;
            };
            let (Some(live_whole), Some(live_variant)) = (live.get(0), live.get(1)) else {
                continue;
            };
            if live_variant.as_str() != variant.as_str()
                || char_offset(active, live_whole.start()) != char_offset(line, whole.start())
                || !wanted(variant.as_str(), literal.as_str())
            {
                continue;
            }
            event = Some((
                row,
                at,
                variant.as_str().to_owned(),
                literal.as_str().to_owned(),
            ));
            break;
        }
        if event.is_some() {
            break;
        }
    }
    let (event_row, event_at, variant, event_literal) = event?;
    let lines = &event_row.lines;
    let run_start = lines.iter().position(|line| RUN_FN.is_match(line))?;
    let sink = lines.iter().position(|line| line.contains("writeln!(stdout"))?;
    if sink < run_start || sink - run_start > 180 {
        return None;
    }
    let event_type = EVENT_TYPE.captures(&lines[run_start])?[1].to_owned();
    let enum_re = re(&format!(r"\benum\s+{}\b", regex::escape(&event_type)));
    let enum_start = lines.iter().position(|line| enum_re.is_match(line))?;
    let variant_re = re(&format!(r"\b{}\b", regex::escape(&variant)));
    if !lines[enum_start..lines.len().min(enum_start + 40)]
        .iter()
        .any(|line| variant_re.is_match(line))
    {
        return None;
    }
    let consumer = (run_start..sink)
        .find(|at| !ANY_FN.is_match(&lines[*at]) && PROCESSES_EVENT.is_match(&lines[*at]))?;
    let enum_window = (enum_start, lines.len().min((event_at + 4).max(enum_start + 4)));
    let path = &event_row.path;
    let mut out = vec![
        format!("### event flow: {event_literal} -> {variant} -> hook output"),
        format!(
            "### {path}:{}-{} — typed event and host-name mapping",
            enum_window.0 + 1,
            enum_window.1
        ),
    ];
    out.extend(numbered_range(lines, enum_window.0, enum_window.1));
    let signature_hi = lines.len().min(run_start + 3);
    out.push(format!(
        "### {path}:{}-{signature_hi} — runner receives typed event",
        run_start + 1
    ));
    out.extend(numbered_range(lines, run_start, signature_hi));
    let (lo, hi) = (run_start.max(consumer.saturating_sub(3)), (sink + 1).min(consumer + 4));
    out.push(format!("### {path}:{}-{hi} — runner passes event to processing", lo + 1));
    out.extend(numbered_range(lines, lo, hi));
    let brief_calls = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| BRIEF_CALL.is_match(line) && !DECLARES.is_match(line))
        .map(|(at, _)| at);
    for at in brief_calls.take(2) {
        let (lo, hi) = (at.saturating_sub(3), lines.len().min(at + 5));
        out.push(format!("### {path}:{}-{hi} — brief processing call chain", lo + 1));
        out.extend(numbered_range(lines, lo, hi));
    }
    if let Some(read) = (run_start..sink).find(|at| READ_CALL.is_match(&lines[*at])) {
        let (lo, hi) = (run_start.max(read.saturating_sub(3)), (sink + 1).min(read + 5));
        out.push(format!("### {path}:{}-{hi} — hook reads the incoming event", lo + 1));
        out.extend(numbered_range(lines, lo, hi));
    }
    let (lo, hi) = (run_start.max(sink.saturating_sub(5)), lines.len().min(sink + 4));
    out.push(format!("### {path}:{}-{hi} — hook writes and flushes output", lo + 1));
    out.extend(numbered_range(lines, lo, hi));

    let module = std::path::Path::new(path)
        .file_stem()
        .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
    let call = re(&format!(r"\b{}::run\s*\(", regex::escape(&module)));
    let args = re(&format!(
        r"(?s)\b{}::run\s*\(([^)]{{0,400}})\)",
        regex::escape(&module)
    ));
    'search: for row in scan.code().filter(|row| row.path != *path) {
        for (at, line) in row.lines.iter().enumerate() {
            if !call.is_match(line) {
                continue;
            }
            let (lo, hi) = (at.saturating_sub(3), row.lines.len().min(at + 4));
            let invocation = row.lines[lo..hi].join("\n");
            let passes = args
                .captures(&invocation)
                .is_some_and(|caps| EVENT_WORD.is_match(&caps[1]));
            if !passes {
                continue;
            }
            out.push(format!(
                "### {}:{}-{hi} — command passes event to hook runner",
                row.path,
                lo + 1
            ));
            out.extend(numbered_range(&row.lines, lo, hi));
            break 'search;
        }
    }
    Some(clip(&out))
}

/// The entry a walkthrough names in code form (`client.fetch`,
/// `handle_event`, `app.mount`): the first such name the graph defines. A
/// dotted name's leading parts pick among same-named symbols (a path that
/// holds them), then a function over a method, then the shortest path; tests
/// never.
fn named_entry(ctx: &Ctx) -> Option<Sym> {
    let mut names: Vec<&str> = BACKTICKED
        .captures_iter(ctx.prompt)
        .filter_map(|caps| caps.get(1))
        .map(|name| name.as_str())
        .collect();
    names.extend(DOTTED.find_iter(ctx.prompt).map(|name| name.as_str()));
    names.extend(CODE_FORM.find_iter(ctx.prompt).map(|name| name.as_str()));
    let mut seen = HashSet::new();
    for full in names.into_iter().filter(|name| seen.insert(*name)) {
        if ctx.timed_out() {
            return None;
        }
        let parts: Vec<&str> = full.split('.').collect();
        let (last, leading) = parts.split_last()?;
        let mut rows: Vec<Sym> = ctx
            .symbols_named(last)
            .into_iter()
            .filter(|sym| sym.name.rsplit('.').next() == Some(*last))
            .filter(|sym| !sym.path.to_lowercase().contains("test"))
            .collect();
        let held = |sym: &Sym| {
            leading
                .iter()
                .filter(|part| sym.path.to_lowercase().contains(&part.to_lowercase()))
                .count()
        };
        rows.sort_by_key(|sym| {
            (
                std::cmp::Reverse(held(sym)),
                sym.kind != "function",
                sym.path.chars().count(),
                sym.path.clone(),
            )
        });
        if let Some(entry) = rows.into_iter().next() {
            return Some(Sym {
                name: (*last).to_owned(),
                ..entry
            });
        }
    }
    None
}

/// A callee of the walk, with how it was reached.
struct Reached {
    sym: Sym,
    tier: String,
    depth: usize,
}

fn ranked_flow(ctx: &Ctx) -> Option<String> {
    let named = named_entry(ctx);
    let entry = match &named {
        Some(entry) => entry.clone(),
        None => {
            let caps = BRIEF_ENTRY.captures(ctx.brief)?;
            if !ctx.brief.contains("### call trace") {
                return None;
            }
            let (name, path) = (caps[1].to_owned(), caps[2].to_owned());
            let (lo, hi): (i64, i64) = (caps[3].parse().ok()?, caps[4].parse().ok()?);
            let found = ctx
                .symbols_named(&name)
                .into_iter()
                .find(|sym| sym.path == path && sym.start_line == lo)?;
            Sym {
                name,
                path,
                start_line: lo,
                end_line: hi,
                ..found
            }
        }
    };
    let mut terms: Vec<String> = Vec::new();
    for word in WORD4.find_iter(ctx.prompt) {
        let lower = word.as_str().to_lowercase();
        if !terms.contains(&lower) && !NOISE.contains(&lower.as_str()) {
            terms.push(lower);
        }
    }
    let mut pool: BTreeMap<String, Reached> = BTreeMap::new();
    let score = |reached: &Reached| {
        let (name, path) = (reached.sym.name.to_lowercase(), reached.sym.path.to_lowercase());
        let weight = |term: &str| if GENERIC.contains(&term) { 0.35 } else { 1.2 };
        let in_name = terms
            .iter()
            .filter(|term| name.contains(term.as_str()))
            .fold(0.0, |sum, term| sum + weight(term));
        let in_path = terms
            .iter()
            .filter(|term| path.contains(term.as_str()))
            .fold(0.0, |sum, term| sum + 0.3 * weight(term));
        let mut score = in_name + in_path;
        score += if reached.tier == "exact" { 0.3 } else { 0.0 };
        if path.contains("test") || path.contains("polyfill") {
            score -= 9.0;
        }
        score - 0.25 * (f64::from(u32::try_from(reached.depth).unwrap_or(u32::MAX)) - 1.0)
    };
    let scored = |pool: &BTreeMap<String, Reached>| -> Vec<(f64, String)> {
        let mut all: Vec<(f64, String)> = pool
            .values()
            .map(|reached| (score(reached), reached.sym.uid.clone()))
            .collect();
        all.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });
        all
    };

    let mut frontier = vec![entry.uid.clone()];
    let mut kept: HashSet<String> = HashSet::new();
    for depth in 1..=3 {
        for uid in frontier.iter().take(6) {
            for edge in ctx.call_edges(uid, Role::Callees, MAX_EDGE_OFFSET) {
                if edge.symbol.uid != entry.uid && !pool.contains_key(&edge.symbol.uid) {
                    pool.insert(
                        edge.symbol.uid.clone(),
                        Reached {
                            sym: edge.symbol,
                            tier: edge.tier,
                            depth,
                        },
                    );
                }
            }
            if ctx.timed_out() {
                return None;
            }
        }
        let level: Vec<(f64, String)> = scored(&pool)
            .into_iter()
            .filter(|(score, uid)| pool[uid].depth == depth && *score > -1.0)
            .collect();
        let passing = level.iter().filter(|(score, _)| *score > MIN_SCORE).count();
        // From an entry the question names, a narrow chain is the path
        // itself: keep its best non-test callees even when their names do
        // not carry the question's words (`client.fetch` calls `send`, then
        // `Pool.open`).
        let chosen: Vec<&(f64, String)> = if named.is_none() || passing >= 2 {
            level.iter().filter(|(score, _)| *score > MIN_SCORE).collect()
        } else {
            level.iter().take(3).collect()
        };
        frontier = chosen.iter().map(|(_, uid)| uid.clone()).collect();
        kept.extend(frontier.iter().cloned());
    }
    let ranked: Vec<&Reached> = scored(&pool)
        .into_iter()
        .filter(|(score, uid)| *score > MIN_SCORE || kept.contains(uid))
        .take(MAX_RANKED)
        .map(|(_, uid)| &pool[&uid])
        .collect();
    if ranked.is_empty() {
        return None;
    }

    let mut text = String::from("drop: .\n### call trace, ranked by the question\n");
    let (name, path, lo, hi) = (&entry.name, &entry.path, entry.start_line, entry.end_line);
    let _ = writeln!(text, "entry {name} — {path}:{lo}-{hi}");
    for item in &ranked {
        let sym = &item.sym;
        let _ = writeln!(
            text,
            "{}calls {} — {}:{}-{}",
            "  ".repeat(item.depth),
            sym.name,
            sym.path,
            sym.start_line,
            sym.end_line
        );
    }
    let (body, last) = numbered(ctx, path, lo, hi, ENTRY_LINES);
    let _ = writeln!(text, "### {path}:{lo}-{last} — {name} definition (method)");
    let _ = writeln!(text, "{}", body.join("\n"));
    if last < hi {
        let _ = writeln!(text, "… {name} continues past line {last}");
    }
    for item in &ranked {
        let sym = &item.sym;
        let (body, last) = numbered(ctx, &sym.path, sym.start_line, sym.end_line, BODY_LINES);
        let _ = writeln!(
            text,
            "### {}:{}-{last} — {} definition ({})",
            sym.path, sym.start_line, sym.name, sym.kind
        );
        let _ = writeln!(text, "{}", body.join("\n"));
    }
    Some(text)
}

/// The numbered lines `lo..=hi` of `path`, at most `cap` of them, and the
/// last line number meant (`lo` when the file cannot be read).
fn numbered(ctx: &Ctx, path: &str, lo: i64, hi: i64, cap: i64) -> (Vec<String>, i64) {
    let Some(text) = ctx.read(path, WHOLE_FILE_BYTES) else {
        return (Vec::new(), lo);
    };
    let lines: Vec<&str> = text.lines().collect();
    let count = i64::try_from(lines.len()).unwrap_or(i64::MAX);
    let hi = hi.min(lo + cap - 1).min(count);
    let rows = (lo..=hi)
        .filter_map(|n| {
            let at = usize::try_from(n - 1).ok()?;
            Some(numbered_line(usize::try_from(n).ok()?, lines.get(at)?))
        })
        .collect();
    (rows, hi)
}

#[cfg(test)]
mod tests;
