// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Deterministic, bounded projection of `scope-task` evidence for agents.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

pub(crate) mod chain;
pub(crate) mod decision_log;
mod evidence;
pub(crate) mod intent;
pub(crate) mod relevance;
pub(crate) mod routes;

const MAX_CAPS: usize = 32;
const MAX_EVIDENCE_PER_TARGET: usize = 8;
const MAX_SYMBOLS_PER_TARGET: usize = 32;
const MAX_TARGETS: usize = 100;
const MAX_TEXT_CHARS: usize = 320;
const MAX_TASK_CHARS: usize = 4096;
const MAX_WORKSTREAMS: usize = 64;
const MAX_ROUTE_TASK_CHARS: usize = 240;

#[derive(Default)]
struct Workstream {
    area: String,
    tier: String,
    targets: Vec<Value>,
}

/// Convert the existing `Request::Targets` response into the versioned brief
/// consumed by orchestration callers. Dependency edges are deliberately not
/// inferred: this command has no complete static dependency graph.
pub fn from_scope_task(task: &str, data: &Value) -> Value {
    let mut caps = BTreeSet::new();
    collect_source_caps(data, &mut caps);
    let bounded_task = bounded_text(task, "task", MAX_TASK_CHARS, &mut caps);

    let mut targets: Vec<&Value> = match data.get("targets").and_then(Value::as_array) {
        Some(targets) => targets
            .iter()
            .filter(|target| {
                matches!(
                    target.get("tier").and_then(Value::as_str),
                    Some("P0" | "P1")
                )
            })
            .collect(),
        None => {
            caps.insert("scope-task response did not contain a targets array".to_string());
            Vec::new()
        }
    };

    if data
        .get("targets")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|item| item["tier"] == "P2"))
    {
        caps.insert("P2 targets omitted: the brief contract supports only P0 and P1".to_string());
    }

    targets.sort_by(|left, right| {
        tier_rank(left)
            .cmp(&tier_rank(right))
            .then_with(|| target_path(left).cmp(target_path(right)))
    });
    if targets.len() > MAX_TARGETS {
        targets.truncate(MAX_TARGETS);
        caps.insert(format!("execution brief targets capped at {MAX_TARGETS}"));
    }

    let mut groups: BTreeMap<(String, String), Workstream> = BTreeMap::new();
    for target in targets {
        let Some(path) = target_path_value(target) else {
            caps.insert("target without a path omitted".to_string());
            continue;
        };
        let tier = target["tier"].as_str().unwrap_or("P1").to_string();
        let area = repository_area(path);
        let key = (area.clone(), tier.clone());
        let entry = groups.entry(key).or_insert_with(|| Workstream {
            area,
            tier,
            targets: Vec::new(),
        });
        entry.targets.push(target_projection(target, &mut caps));
    }

    let mut workstreams = Vec::new();
    for (_, mut group) in groups {
        group
            .targets
            .sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
        let paths: Vec<Value> = group
            .targets
            .iter()
            .filter_map(|target| target["path"].as_str().map(Value::from))
            .collect();
        workstreams.push(json!({
            "id": format!("workstream:{}:{}", group.area, group.tier),
            "paths": paths,
            "tier": group.tier,
            "depends_on": [],
            "ownership": if group.tier == "P0" { "write" } else { "read" },
            "targets": group.targets,
        }));
    }
    // The groups come out ordered by area; order them P0 first before the
    // cap so a truncation drops read-only context, never a write workstream.
    workstreams.sort_by(|left, right| {
        tier_rank(left)
            .cmp(&tier_rank(right))
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    if workstreams.len() > MAX_WORKSTREAMS {
        workstreams.truncate(MAX_WORKSTREAMS);
        caps.insert(format!(
            "execution brief workstreams capped at {MAX_WORKSTREAMS}"
        ));
    }

    let mut caps: Vec<String> = caps.into_iter().collect();
    if caps.len() > MAX_CAPS {
        caps.truncate(MAX_CAPS);
        caps.push(format!("execution brief caps capped at {MAX_CAPS}"));
    }
    let mut validation = Vec::new();
    let has_p0 = workstreams
        .iter()
        .any(|workstream| workstream["tier"] == "P0");
    let has_p1 = workstreams
        .iter()
        .any(|workstream| workstream["tier"] == "P1");
    if has_p0 {
        validation
            .push("Run focused tests covering changed P0 behavior before integration.".to_string());
    }
    if has_p1 {
        validation.push(
            "Review P1 workstreams for callers and contracts, then run regression tests."
                .to_string(),
        );
    }
    if !has_p0 && has_p1 {
        validation
            .push("No P0 target was returned; validate the P1 context before editing.".to_string());
    }
    if workstreams.is_empty() {
        validation.push(
            "No P0/P1 targets were returned; inspect scope-task caps before acting.".to_string(),
        );
    }
    if data
        .get("targets")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|item| item["tier"] == "P2"))
    {
        validation.push(
            "P2 targets were omitted; validate any peripheral files separately if touched."
                .to_string(),
        );
    }
    if !caps.is_empty() {
        validation.push("Review uncertainty.caps before relying on this brief.".to_string());
    }

    // The typed text only (a pasted block never steers the search); a task
    // that asks nothing about code still gets a route from its whole text,
    // since `execution-brief` was asked for one.
    let request = retrieval_request(task);
    let route = retrieval_route(request.as_deref().unwrap_or(task));
    json!({
        "version": 1,
        "task": bounded_task,
        "asks_about_code": request.is_some(),
        "retrieval_route_text": pretty_retrieval_route(&route),
        "retrieval_route": route,
        "workstreams": workstreams,
        "uncertainty": {
            "closed_world": false,
            "lower_bound": true,
            "caps": caps,
            "unknown_dependencies": [
                "Static dependency edges are unavailable; every workstream depends_on list is intentionally empty."
            ],
        },
        "validation": validation,
    })
}

/// Produce one deterministic retrieval route from a task description. An
/// explicitly backticked identifier starts with exact search; other tasks
/// start with behavior search. A non-converging first call gets one alternate
/// Pixel query, then a bounded native fallback. Search warnings and call-count
/// notices are informational; only the result itself advances the route.
/// Commands are bare (`pixel`, `rg`, `sed`): the route must run where rtk is
/// not installed, and an installed rtk hook adds its own prefix.
pub fn retrieval_route(task: &str) -> Value {
    let task = truncate_chars(task.trim(), MAX_ROUTE_TASK_CHARS);
    let identifier = explicit_identifier(&task);
    let (subcommand, args) = match identifier.as_deref() {
        Some(identifier) => (
            "search-content",
            vec![
                "-F".to_string(),
                identifier.to_string(),
                "--fallback-query".to_string(),
                task.clone(),
                "--no-daemon".to_string(),
            ],
        ),
        None => ("find-code", vec![task.clone()]),
    };
    let first = route_command(subcommand, &args);
    let fallback_query = identifier
        .clone()
        .unwrap_or_else(|| bounded_native_query(&task));
    let native_fallback = format!(
        "rg -m 5 -n -F -- {} . | sed -n '1,20p'",
        shell_quote(&fallback_query)
    );
    let alternate = if identifier.is_some() {
        native_fallback.clone()
    } else {
        format!(
            "pixel find-code {}",
            shell_quote(&format!("{task} implementation and callers"))
        )
    };
    json!({
        "first_command": first,
        "first_operation": {"subcommand": subcommand, "args": args},
        "first_on_empty_or_irrelevant": alternate,
        "after_two_nonconverging_calls": native_fallback,
        "automatic_empty_fallback": identifier.is_some(),
        "read": "Read only a path returned by Pixel, in a maximum 40-line window around its line.",
        "validation": "After an edit, run the smallest relevant test for the changed behavior; read-only tasks need no test.",
        "progression": if identifier.is_some() {
            "An empty exact result automatically runs one task-aware find-code fallback in the same command; warnings or prior-call counts alone never trigger it. If that combined lookup is unusable, use the bounded native fallback."
        } else {
            "A warning or prior-call count is informational, not a no-hit. Advance only when this invocation returns no usable match, an unresolved result, or an irrelevant result."
        }
    })
}

/// Words that name code or ask a question about it. A prompt with none of
/// them and no identifier asks for something else (git, a release, a
/// discussion), and a retrieval route for it is noise a model learns to skip.
const CODE_WORDS: &[&str] = &[
    "api",
    "bug",
    "bugs",
    "call",
    "called",
    "callee",
    "callees",
    "caller",
    "callers",
    "calls",
    "class",
    "classes",
    "codebase",
    "constant",
    "crash",
    "crashes",
    "crate",
    "crates",
    "declaration",
    "declared",
    "defined",
    "definition",
    "endpoint",
    "enum",
    "error",
    "errors",
    "exception",
    "failing",
    "field",
    "fields",
    "function",
    "functions",
    "handler",
    "handlers",
    "hook",
    "hooks",
    "implement",
    "implementation",
    "implemented",
    "implements",
    "import",
    "imports",
    "interface",
    "method",
    "methods",
    "module",
    "modules",
    "panic",
    "panics",
    "parameter",
    "parser",
    "refactor",
    "regression",
    "rename",
    "schema",
    "signature",
    "struct",
    "structs",
    "symbol",
    "symbols",
    "test",
    "tests",
    "trait",
    "variable",
];

/// Openings of a question about how the code works or where something is.
const CODE_QUESTIONS: &[&str] = &[
    "explain ",
    "find the ",
    "fix ",
    "how does ",
    "how is ",
    "trace ",
    "what calls ",
    "what does ",
    "where are ",
    "where do ",
    "where does ",
    "where is ",
    "which file ",
    "which function ",
    "who calls ",
    "why does ",
];

/// Source extensions that make a token a file name.
const SOURCE_EXTENSIONS: &[&str] = &[
    "c", "cpp", "cs", "css", "ex", "exs", "go", "h", "hpp", "html", "java", "js", "json", "jsx",
    "kt", "lua", "md", "php", "py", "rb", "rs", "sh", "sql", "swift", "toml", "ts", "tsx", "vue",
    "yaml", "yml",
];

const PASTE_OPEN: &str = "<pasted_content";
const PASTE_CLOSE: &str = "</pasted_content";

/// The part of a prompt the user typed: pasted blocks (`<pasted_content …>`
/// to its closing tag) are someone else's text and never the task itself.
/// A removed block leaves a space, so the words on either side stay apart
/// (`where is<block>the parser` keeps its `where is ` opening).
pub(crate) fn typed_text(prompt: &str) -> String {
    let mut typed = String::new();
    let mut rest = prompt;
    while let Some(start) = find_tag(rest, PASTE_OPEN) {
        typed.push_str(&rest[..start]);
        let after = &rest[start + PASTE_OPEN.len()..];
        let Some(close) = find_tag(after, PASTE_CLOSE) else {
            return typed;
        };
        let tail = &after[close + PASTE_CLOSE.len()..];
        rest = tail.find('>').map_or("", |end| &tail[end + 1..]);
        if !rest.is_empty() {
            typed.push(' ');
        }
    }
    typed.push_str(rest);
    typed
}

/// The first `tag` that ends at a tag boundary (`>` or whitespace), so a
/// longer name such as `</pasted_contentious>` is not taken for it.
fn find_tag(haystack: &str, tag: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(found) = haystack[from..].find(tag) {
        let at = from + found;
        let next = haystack[at + tag.len()..].chars().next();
        if next.is_none_or(|ch| ch == '>' || ch.is_whitespace()) {
            return Some(at);
        }
        from = at + tag.len();
    }
    None
}

/// A token that can only be a name in code: `a::b`, `snake_case`, `camelCase`
/// or `PascalCase` with an inner capital, a path, or a file with a source
/// extension (in any case, so `README.MD` counts). Surrounding punctuation,
/// a sentence's closing period and backticks are not part of the token.
fn names_code(token: &str) -> bool {
    let token = token.trim_matches(|ch: char| "()[]{}<>,.;:!?\"'`".contains(ch));
    let inner = |separator: char| {
        token.split(separator).count() > 1
            && token
                .split(separator)
                .all(|part| part.chars().next().is_some_and(char::is_alphanumeric))
    };
    let camel = token
        .chars()
        .zip(token.chars().skip(1))
        .any(|(before, after)| before.is_lowercase() && after.is_uppercase());
    let extension = token.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && SOURCE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
    });
    token.contains("::") || inner('_') || inner('/') || camel || extension
}

/// How clearly the typed text of a prompt asks about code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// A backticked identifier or a code-shaped token (`a::b`, `snake_case`,
    /// `camelCase`, a path, a source file): no model needed to say so.
    Strong,
    /// Only a code word or a code-question opener matched: close enough to
    /// ask, ambiguous enough that a model verdict can help.
    Weak,
    /// Plain language with at least [`MIN_PROSE_KEYWORDS`] content words that
    /// is not a repository operation. Whether it is about this repository is
    /// not something its shape can say: the evidence decides.
    Prose,
}

impl Signal {
    /// The signal as the decision log spells it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Strong => "strong",
            Self::Weak => "weak",
            Self::Prose => "prose",
        }
    }
}

/// The typed text of `prompt` and how strongly it asks about code, or `None`
/// when it asks for something else (git and release requests, pasted chat
/// threads, discussion). Only the typed text counts; pasted blocks never
/// steer the search.
pub fn code_signal(prompt: &str) -> Option<(String, Signal)> {
    let typed = typed_text(prompt);
    let typed = typed.trim();
    if explicit_identifier(typed).is_some() || typed.split_whitespace().any(names_code) {
        return Some((typed.to_string(), Signal::Strong));
    }
    let lower = typed.to_lowercase();
    let weak = lower
        .split(|ch: char| !ch.is_alphanumeric())
        .any(|word| CODE_WORDS.contains(&word))
        || lower
            .split(['.', '?', '!', ':', ';', ',', '\n'])
            .map(str::trim_start)
            .any(|clause| {
                CODE_QUESTIONS
                    .iter()
                    .any(|opening| clause.starts_with(opening))
            });
    weak.then(|| (typed.to_string(), Signal::Weak))
}

/// The text to route when `prompt` asks about code, `None` when it asks for
/// something else. The typed text of [`code_signal`], whichever strength.
pub fn retrieval_request(prompt: &str) -> Option<String> {
    code_signal(prompt).map(|(typed, _)| typed)
}

/// Typed text longer than this many characters, over at least
/// [`PASTE_TAIL_LINES`] lines, is a prompt with an untagged paste in front of
/// the question: only its last paragraph is the task.
const PASTE_TAIL_CHARS: usize = 600;
const PASTE_TAIL_LINES: usize = 4;
/// Content words a plain-language prompt needs before the relevance gate is
/// asked: one word is a reply, not a task.
const MIN_PROSE_KEYWORDS: usize = 2;

/// Words that name a repository operation (git, a release, a deploy, CI)
/// in English and French, folded to ASCII as the tokenizer folds them, plus
/// the few function words a short prompt leaves behind when its language is
/// not detected. A prompt made only of these asks to operate the repository,
/// not to read it. Deliberately short: anything it misses meets the
/// relevance gate, which decides on evidence.
const OPS_WORDS: &[&str] = &[
    "amend",
    "back",
    "bascule",
    "basculer",
    "branch",
    "branche",
    "branches",
    "bump",
    "checkout",
    "cherry",
    "ci",
    "clone",
    "commit",
    "commite",
    "commiter",
    "commits",
    "deploie",
    "deployer",
    "deploy",
    "deployed",
    "fetch",
    "fusionne",
    "fusionner",
    "git",
    "go",
    "main",
    "master",
    "merge",
    "merged",
    "origin",
    "pick",
    "pousse",
    "pousser",
    "prod",
    "production",
    "publie",
    "publier",
    "publish",
    "pull",
    "push",
    "rebase",
    "release",
    "releases",
    "relance",
    "relancer",
    "remote",
    "reset",
    "revert",
    "ship",
    "squash",
    "stash",
    "staging",
    "sur",
    "switch",
    "tag",
    "tags",
    "tire",
    "upstream",
    "version",
    "versions",
    "vers",
];

/// Words that acknowledge, greet or judge the last answer. They are content
/// words to the tokenizer and carry nothing about the repository.
const CHATTER_WORDS: &[&str] = &[
    "awesome",
    "bonjour",
    "bravo",
    "cheers",
    "cool",
    "excellent",
    "fine",
    "genial",
    "good",
    "great",
    "hello",
    "looks",
    "merci",
    "nice",
    "okay",
    "parfait",
    "perfect",
    "salut",
    "sorry",
    "super",
    "sure",
    "thank",
    "thanks",
    "thx",
    "works",
    "worked",
    "yeah",
    "yep",
];

/// The part of the typed text that is the task: the last paragraph when the
/// text is long and spread over lines (an untagged paste before the
/// question), the whole text otherwise. Text with no blank line is one
/// paragraph and stays whole.
pub(crate) fn brief_task(typed: &str) -> &str {
    let typed = typed.trim();
    if typed.chars().count() > PASTE_TAIL_CHARS && typed.lines().count() >= PASTE_TAIL_LINES {
        last_paragraph(typed)
    } else {
        typed
    }
}

/// The text after the last blank line.
fn last_paragraph(text: &str) -> &str {
    let mut start = 0;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        offset += line.len();
        if line.trim().is_empty() {
            start = offset;
        }
    }
    text[start..].trim()
}

/// The content words of a task: the tokenizer's keywords without the
/// acknowledgements and greetings of a conversation.
fn brief_keywords(task: &str) -> Vec<String> {
    pixel_rank::tokenize_task(task)
        .map(|query| query.keywords)
        .unwrap_or_default()
        .into_iter()
        .filter(|word| !CHATTER_WORDS.contains(&word.as_str()))
        .collect()
}

/// A version number as the tokenizer leaves one: digits, or `v` and digits.
fn is_version_shaped(word: &str) -> bool {
    let digits = word.strip_prefix('v').unwrap_or(word);
    !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
}

/// Every keyword names an operation or a version: git, a release, a deploy.
fn is_ops_request(keywords: &[String]) -> bool {
    keywords
        .iter()
        .all(|word| OPS_WORDS.contains(&word.as_str()) || is_version_shaped(word))
}

/// The task of `prompt` and why it may deserve a brief, or `None` when it
/// cannot: [`code_signal`] first, and failing that [`Signal::Prose`] for
/// plain language with at least [`MIN_PROSE_KEYWORDS`] content words that is
/// not a repository operation. Only the typed text counts, and of a long
/// text with a paste in front only its last paragraph.
pub fn brief_signal(prompt: &str) -> Option<(String, Signal)> {
    if let Some(found) = code_signal(prompt) {
        return Some(found);
    }
    let typed = typed_text(prompt);
    let task = brief_task(&typed);
    let keywords = brief_keywords(task);
    (keywords.len() >= MIN_PROSE_KEYWORDS && !is_ops_request(&keywords))
        .then(|| (task.to_string(), Signal::Prose))
}

fn route_command(subcommand: &str, args: &[String]) -> String {
    format!(
        "pixel {subcommand} {}",
        args.iter()
            .map(|argument| match argument.as_str() {
                "-F" | "--fallback-query" | "--no-daemon" => argument.clone(),
                _ => shell_quote(argument),
            })
            .collect::<Vec<_>>()
            .join(" ")
    )
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetrievalOutcome {
    Usable,
    NoUsableResult,
    WarningOnly,
}

/// Resolve the next route step from an invocation's outcome. A warning by
/// itself never advances the route, regardless of the displayed call count.
#[cfg(test)]
pub(crate) fn next_retrieval_command(
    route: &Value,
    completed_pixel_calls: usize,
    outcome: RetrievalOutcome,
) -> Option<&str> {
    if outcome != RetrievalOutcome::NoUsableResult {
        return None;
    }
    match completed_pixel_calls {
        0 => route["first_command"].as_str(),
        1 => route["first_on_empty_or_irrelevant"].as_str(),
        _ => route["after_two_nonconverging_calls"].as_str(),
    }
}

/// Render an executable route without presenting task-context candidates as
/// recommendations or boundaries.
pub fn pretty_retrieval_route(route: &Value) -> String {
    if route["automatic_empty_fallback"].as_bool() == Some(true) {
        return format!(
            "[PIXEL:EXECUTION_ROUTE]\n1. Run: {}\n   An empty exact result automatically runs the task-aware find-code fallback once in the same command.\n2. Read: {}\n3. If the combined Pixel lookup is unusable, use: {}\n4. Validate: {}\n{}\n[/PIXEL:EXECUTION_ROUTE]",
            route["first_command"].as_str().unwrap_or(""),
            route["read"].as_str().unwrap_or(""),
            route["after_two_nonconverging_calls"]
                .as_str()
                .unwrap_or(""),
            route["validation"].as_str().unwrap_or(""),
            route["progression"].as_str().unwrap_or(""),
        );
    }
    format!(
        "[PIXEL:EXECUTION_ROUTE]\n1. Run: {}\n   If it returns no usable or relevant result, run exactly once: {}\n2. Read: {}\n3. If both Pixel calls do not converge, use: {}\n4. Validate: {}\n{}\n[/PIXEL:EXECUTION_ROUTE]",
        route["first_command"].as_str().unwrap_or(""),
        route["first_on_empty_or_irrelevant"].as_str().unwrap_or(""),
        route["read"].as_str().unwrap_or(""),
        route["after_two_nonconverging_calls"]
            .as_str()
            .unwrap_or(""),
        route["validation"].as_str().unwrap_or(""),
        route["progression"].as_str().unwrap_or(""),
    )
}

fn explicit_identifier(task: &str) -> Option<String> {
    let (_, tail) = task.split_once('`')?;
    let (identifier, _) = tail.split_once('`')?;
    let identifier = identifier.trim();
    (!identifier.is_empty()
        && identifier
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "_:-./".contains(ch)))
    .then(|| identifier.to_string())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Derive a bounded literal term for a native search fallback so the whole
/// task sentence is never handed to a fixed-string `rg`. Pick the longest
/// punctuation-free keyword; if the task has none, fall back to the task text
/// so the fixed-string search still targets a term that can match.
fn bounded_native_query(task: &str) -> String {
    task.split_whitespace()
        .map(|word| word.trim_matches(|ch: char| !ch.is_alphanumeric()))
        .max_by_key(|word| word.chars().count())
        .and_then(|word| (!word.is_empty()).then(|| word.to_string()))
        .unwrap_or_else(|| task.to_string())
}

fn truncate_chars(value: &str, max: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

/// Human-readable rendering that preserves the same bounded target details as
/// the JSON contract without making callers parse a second evidence shape.
pub fn pretty(brief: &Value) -> String {
    let mut output = String::new();
    output.push_str("execution brief v1\n");
    output.push_str(&format!(
        "task: {}\n",
        brief.get("task").and_then(Value::as_str).unwrap_or("")
    ));
    if let Some(workstreams) = brief.get("workstreams").and_then(Value::as_array) {
        for workstream in workstreams {
            output.push_str(&format!(
                "\n{} [{} / {}]\n",
                workstream.get("id").and_then(Value::as_str).unwrap_or("?"),
                workstream
                    .get("tier")
                    .and_then(Value::as_str)
                    .unwrap_or("?"),
                workstream
                    .get("ownership")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            ));
            if let Some(targets) = workstream.get("targets").and_then(Value::as_array) {
                for target in targets {
                    output.push_str(&format!(
                        "  {}\n",
                        target.get("path").and_then(Value::as_str).unwrap_or("?")
                    ));
                    if let Some(symbols) = target.get("symbols").and_then(Value::as_array) {
                        for symbol in symbols {
                            output.push_str(&format!(
                                "    symbol: {} {}\n",
                                symbol.get("kind").and_then(Value::as_str).unwrap_or("?"),
                                symbol.get("name").and_then(Value::as_str).unwrap_or("?")
                            ));
                        }
                    }
                    if let Some(evidence) = target.get("evidence").and_then(Value::as_array) {
                        for item in evidence {
                            output.push_str(&format!(
                                "    evidence [{}:{}]: {}\n",
                                item.get("keyword").and_then(Value::as_str).unwrap_or("?"),
                                item.get("line").and_then(Value::as_u64).unwrap_or(0),
                                item.get("text").and_then(Value::as_str).unwrap_or("")
                            ));
                        }
                    }
                    if let Some(reasons) = target.get("reasons").and_then(Value::as_array) {
                        for reason in reasons.iter().filter_map(Value::as_str) {
                            output.push_str(&format!("    reason: {reason}\n"));
                        }
                    }
                }
            }
        }
    }
    if let Some(uncertainty) = brief.get("uncertainty") {
        output.push_str("\nuncertainty:\n");
        output.push_str("  closed_world: false\n  lower_bound: true\n");
        if let Some(caps) = uncertainty.get("caps").and_then(Value::as_array) {
            for cap in caps.iter().filter_map(Value::as_str) {
                output.push_str(&format!("  cap: {cap}\n"));
            }
        }
        output.push_str("  dependencies: unknown (depends_on is empty)\n");
    }
    if let Some(validation) = brief.get("validation").and_then(Value::as_array) {
        output.push_str("\nvalidation:\n");
        for item in validation.iter().filter_map(Value::as_str) {
            output.push_str(&format!("  - {item}\n"));
        }
    }
    output
}

fn collect_source_caps(data: &Value, caps: &mut BTreeSet<String>) {
    if let Some(source_caps) = data["envelope"]["caps"].as_array() {
        for cap in source_caps.iter().filter_map(Value::as_str) {
            let bounded = bounded_source_cap(cap, caps);
            caps.insert(bounded);
        }
    }
    if let Some(warnings) = data["warnings"].as_array() {
        for warning in warnings {
            if let Some(message) = warning["message"].as_str() {
                let bounded = bounded_source_cap(message, caps);
                caps.insert(bounded);
            }
        }
    }
    if let Some(closed_world) = data["closed_world"].as_str() {
        let bounded = bounded_source_cap(closed_world, caps);
        caps.insert(bounded);
    }
}

fn bounded_source_cap(text: &str, caps: &mut BTreeSet<String>) -> String {
    bounded_text(text, "source cap", MAX_TEXT_CHARS, caps)
}

fn target_projection(target: &Value, caps: &mut BTreeSet<String>) -> Value {
    let symbols = target["symbols"]
        .as_array()
        .map(|items| {
            cap_items(items, MAX_SYMBOLS_PER_TARGET, "symbols", caps);
            bounded_items(items, MAX_SYMBOLS_PER_TARGET, |symbol| {
                json!({
                    "kind": symbol["kind"].as_str().unwrap_or("?"),
                    "name": symbol["name"].as_str().unwrap_or("?"),
                    "uid": symbol["uid"].as_str().unwrap_or("?"),
                    "line": symbol["line"].as_u64().unwrap_or(0),
                })
            })
        })
        .unwrap_or_default();
    let evidence = target["evidence"]
        .as_array()
        .map(|items| {
            cap_items(items, MAX_EVIDENCE_PER_TARGET, "evidence", caps);
            bounded_items(items, MAX_EVIDENCE_PER_TARGET, |item| {
                json!({
                    "keyword": item["keyword"].as_str().unwrap_or("?"),
                    "line": item["line"].as_u64().unwrap_or(0),
                    "text": bounded_text(
                        item["text"].as_str().unwrap_or(""),
                        "evidence text",
                        MAX_TEXT_CHARS,
                        caps,
                    ),
                })
            })
        })
        .unwrap_or_default();
    let reasons = target["reasons"]
        .as_array()
        .map(|items| {
            cap_items(items, MAX_EVIDENCE_PER_TARGET, "reasons", caps);
            bounded_items(items, MAX_EVIDENCE_PER_TARGET, |reason| {
                Value::from(bounded_text(
                    reason.as_str().unwrap_or(""),
                    "reason",
                    MAX_TEXT_CHARS,
                    caps,
                ))
            })
        })
        .unwrap_or_default();
    json!({
        "path": target_path_value(target).unwrap_or("?"),
        "symbols": symbols,
        "evidence": evidence,
        "reasons": reasons,
    })
}

fn bounded_items<T>(items: &[Value], limit: usize, map: impl FnMut(&Value) -> T) -> Vec<T> {
    items.iter().take(limit).map(map).collect()
}

fn cap_items(items: &[Value], limit: usize, label: &str, caps: &mut BTreeSet<String>) {
    if items.len() > limit {
        caps.insert(format!("{label} per target capped at {limit}"));
    }
}

fn bounded_text(text: &str, label: &str, limit: usize, caps: &mut BTreeSet<String>) -> String {
    let mut chars = text.chars();
    let bounded: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        caps.insert(format!("{label} capped at {limit} characters"));
    }
    bounded
}

fn repository_area(path: &str) -> String {
    path.rsplit_once('/').map_or_else(
        || ".".to_string(),
        |(parent, _)| {
            if parent.is_empty() {
                ".".to_string()
            } else {
                parent.to_string()
            }
        },
    )
}

fn target_path(target: &Value) -> &str {
    target["path"].as_str().unwrap_or("?")
}

fn target_path_value(target: &Value) -> Option<&str> {
    target["path"].as_str().filter(|path| !path.is_empty())
}

fn tier_rank(target: &Value) -> u8 {
    match target["tier"].as_str() {
        Some("P0") => 0,
        Some("P1") => 1,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(tier: &str, path: &str) -> Value {
        json!({"tier": tier, "path": path})
    }

    fn brief_of(targets: Vec<Value>) -> Value {
        from_scope_task("task", &json!({"targets": targets}))
    }

    fn caps_of(brief: &Value) -> Vec<String> {
        brief["uncertainty"]["caps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|cap| cap.as_str().unwrap().to_string())
            .collect()
    }

    fn target_count(brief: &Value) -> usize {
        brief["workstreams"]
            .as_array()
            .unwrap()
            .iter()
            .map(|workstream| workstream["targets"].as_array().unwrap().len())
            .sum()
    }

    #[test]
    fn targets_are_capped_at_one_hundred_and_the_cap_is_disclosed() {
        let make = |n: usize| {
            (0..n)
                .map(|i| target("P1", &format!("src/f{i:03}.rs")))
                .collect()
        };
        let exact = brief_of(make(MAX_TARGETS));
        assert_eq!(target_count(&exact), 100);
        assert!(caps_of(&exact).is_empty(), "{:?}", caps_of(&exact));
        let over = brief_of(make(MAX_TARGETS + 1));
        assert_eq!(target_count(&over), 100);
        assert_eq!(caps_of(&over), ["execution brief targets capped at 100"]);
    }

    #[test]
    fn a_workstream_cap_drops_read_context_before_any_write_workstream() {
        // 64 P1 areas named before the one P0 area: an area-ordered cap
        // would keep them all and lose the only write workstream.
        let mut targets: Vec<Value> = (0..MAX_WORKSTREAMS)
            .map(|i| target("P1", &format!("a{i:02}/x.rs")))
            .collect();
        targets.push(target("P0", "zz/edit.rs"));
        let brief = brief_of(targets);
        let workstreams = brief["workstreams"].as_array().unwrap();
        assert_eq!(workstreams.len(), 64);
        assert_eq!(workstreams[0]["id"], "workstream:zz:P0");
        assert_eq!(workstreams[0]["ownership"], "write");
        assert_eq!(
            caps_of(&brief),
            ["execution brief workstreams capped at 64"]
        );
        assert!(
            !brief["validation"].to_string().contains("No P0 target"),
            "{}",
            brief["validation"]
        );

        let exact: Vec<Value> = (0..MAX_WORKSTREAMS)
            .map(|i| target("P1", &format!("a{i:02}/x.rs")))
            .collect();
        let brief = brief_of(exact);
        assert_eq!(brief["workstreams"].as_array().unwrap().len(), 64);
        assert!(caps_of(&brief).is_empty());
    }

    #[test]
    fn per_target_lists_and_texts_are_cut_exactly_at_their_limits() {
        let symbols = |n: usize| -> Vec<Value> {
            (0..n)
                .map(|i| json!({"kind": "fn", "name": format!("s{i}")}))
                .collect()
        };
        let at_limit = brief_of(vec![json!({
            "tier": "P0", "path": "a.rs",
            "symbols": symbols(MAX_SYMBOLS_PER_TARGET),
            "evidence": [{"keyword": "k", "line": 1, "text": "x".repeat(MAX_TEXT_CHARS)}],
        })]);
        let projected = &at_limit["workstreams"][0]["targets"][0];
        assert_eq!(projected["symbols"].as_array().unwrap().len(), 32);
        assert_eq!(
            projected["evidence"][0]["text"].as_str().unwrap().len(),
            320
        );
        assert!(caps_of(&at_limit).is_empty(), "{:?}", caps_of(&at_limit));

        let over = brief_of(vec![json!({
            "tier": "P0", "path": "a.rs",
            "symbols": symbols(MAX_SYMBOLS_PER_TARGET + 1),
            "evidence": [{"keyword": "k", "line": 1, "text": "x".repeat(MAX_TEXT_CHARS + 1)}],
        })]);
        let projected = &over["workstreams"][0]["targets"][0];
        assert_eq!(projected["symbols"].as_array().unwrap().len(), 32);
        assert_eq!(
            projected["evidence"][0]["text"].as_str().unwrap().len(),
            320
        );
        assert_eq!(
            caps_of(&over),
            [
                "evidence text capped at 320 characters",
                "symbols per target capped at 32",
            ]
        );
    }

    #[test]
    fn more_than_thirty_two_caps_keep_thirty_two_plus_a_marker() {
        let source = |n: usize| -> Value {
            json!({
                "targets": [],
                "envelope": {"caps": (0..n).map(|i| format!("cap {i:02}")).collect::<Vec<_>>()},
            })
        };
        // The empty targets array adds no cap of its own.
        let exact = from_scope_task("t", &source(MAX_CAPS));
        assert_eq!(caps_of(&exact).len(), 32);
        let over = from_scope_task("t", &source(MAX_CAPS + 1));
        let caps = caps_of(&over);
        assert_eq!(caps.len(), 33);
        assert_eq!(caps[32], "execution brief caps capped at 32");
    }

    #[test]
    fn the_pretty_brief_carries_each_targets_reasons() {
        let brief = brief_of(vec![json!({
            "tier": "P0", "path": "src/login.rs", "reasons": ["defines login_user"],
        })]);
        let text = pretty(&brief);
        assert!(
            text.contains("  src/login.rs\n    reason: defines login_user\n"),
            "{text}"
        );
    }

    #[test]
    fn every_route_command_runs_without_rtk_installed() {
        for task in ["Trace callers of `Foo::bar`", "Why does the parser panic?"] {
            let route = retrieval_route(task);
            let rendered = pretty_retrieval_route(&route);
            for command in [
                &route["first_command"],
                &route["first_on_empty_or_irrelevant"],
                &route["after_two_nonconverging_calls"],
            ] {
                let command = command.as_str().unwrap();
                assert!(!command.contains("rtk"), "{command}");
                assert!(rendered.contains(command), "{rendered}");
            }
            assert!(!rendered.contains("rtk"), "{rendered}");
        }
    }

    #[test]
    fn explicit_identifier_starts_exact_and_empty_result_routes_to_find_code() {
        let route = retrieval_route("Trace callers of `Foo::bar`");
        assert_eq!(
            route["first_command"],
            "pixel search-content -F 'Foo::bar' --fallback-query 'Trace callers of `Foo::bar`' --no-daemon"
        );
        assert_eq!(
            route["first_operation"],
            json!({
                "subcommand": "search-content",
                "args": ["-F", "Foo::bar", "--fallback-query", "Trace callers of `Foo::bar`", "--no-daemon"]
            })
        );
        assert_eq!(route["automatic_empty_fallback"], true);
        assert_eq!(
            route["first_on_empty_or_irrelevant"],
            "rg -m 5 -n -F -- 'Foo::bar' . | sed -n '1,20p'"
        );
        let rendered = pretty_retrieval_route(&route);
        assert!(
            rendered.contains("runs the task-aware find-code fallback once in the same command")
        );
        assert!(rendered.contains("maximum 40-line window"));
        assert!(rendered.contains("warnings or prior-call counts alone never trigger it"));
        assert!(!rendered.contains("run exactly once: pixel find-code"));
        assert_eq!(
            next_retrieval_command(&route, 1, RetrievalOutcome::NoUsableResult),
            route["first_on_empty_or_irrelevant"].as_str()
        );
        assert_eq!(
            next_retrieval_command(&route, 1, RetrievalOutcome::Usable),
            None
        );
        assert_eq!(
            next_retrieval_command(&route, 2, RetrievalOutcome::NoUsableResult),
            route["after_two_nonconverging_calls"].as_str()
        );
        assert_eq!(
            next_retrieval_command(&route, 1, RetrievalOutcome::WarningOnly),
            None
        );
    }

    #[test]
    fn behavior_route_has_a_distinct_second_query_then_bounded_fallback() {
        let route = retrieval_route("How does task preparation refresh stale source evidence?");
        assert_eq!(route["automatic_empty_fallback"], false);
        assert_eq!(
            route["first_command"],
            "pixel find-code 'How does task preparation refresh stale source evidence?'"
        );
        assert_eq!(
            route["first_operation"],
            json!({
                "subcommand": "find-code",
                "args": ["How does task preparation refresh stale source evidence?"]
            })
        );
        assert_eq!(
            route["first_on_empty_or_irrelevant"],
            "pixel find-code 'How does task preparation refresh stale source evidence? implementation and callers'"
        );
        // The native fallback carries the bounded term derived from the
        // task — not the whole sentence and not an empty or placeholder
        // term — shell-quoted into one literal `rg` query.
        assert_eq!(
            route["after_two_nonconverging_calls"],
            "rg -m 5 -n -F -- 'preparation' . | sed -n '1,20p'"
        );
        assert_eq!(
            route["progression"],
            "A warning or prior-call count is informational, not a no-hit. Advance only when this invocation returns no usable match, an unresolved result, or an irrelevant result."
        );
    }

    #[test]
    fn identifier_shell_quoting_is_safe_and_invalid_backticks_do_not_select_exact_search() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        let route = retrieval_route("Find `two words` in code");
        assert!(
            route["first_command"]
                .as_str()
                .unwrap()
                .starts_with("pixel find-code ")
        );
    }

    #[test]
    fn route_command_should_leave_known_flags_bare_and_quote_query_values() {
        let route = retrieval_route("Trace callers of `Foo::bar` in Livio's code");
        assert_eq!(
            route["first_command"],
            "pixel search-content -F 'Foo::bar' --fallback-query 'Trace callers of `Foo::bar` in Livio'\\''s code' --no-daemon"
        );
    }

    #[test]
    fn retrieval_request_should_route_only_prompts_that_ask_about_code() {
        // Prompts from real sessions on 2026-10-04 that received a route.
        for prompt in [
            "go to branch main and pull",
            "commit and push",
            "release 0.6.2",
            "This is a good question, and I'm not sure about the answer because I would say yes, it should rewrite.",
            "So basically we should also do it with codex, of course, because codex does not necessarily give the same result.",
            "<pasted_content id=\"4f44\">\nLe model il s'en fout de pixel. `grep` src/main.rs\n</pasted_content id=\"4f44\">\n\nWe have only one remaining task: coherence.",
            "",
        ] {
            assert_eq!(retrieval_request(prompt), None, "{prompt}");
        }
        for (prompt, typed) in [
            ("Trace callers of `Foo::bar`", "Trace callers of `Foo::bar`"),
            (
                "where is retrieval_route called",
                "where is retrieval_route called",
            ),
            (
                "How does task preparation refresh stale source evidence?",
                "How does task preparation refresh stale source evidence?",
            ),
            ("make TaskHookEvent cheaper", "make TaskHookEvent cheaper"),
            ("look at pixel_task::digest", "look at pixel_task::digest"),
            ("open crates/pixel/src/guard", "open crates/pixel/src/guard"),
            ("check guard.rs", "check guard.rs"),
            ("fix the failing test", "fix the failing test"),
            (
                "explain the guard's precedence rules",
                "explain the guard's precedence rules",
            ),
            (
                "fix the hook so a prompt keeps the task",
                "fix the hook so a prompt keeps the task",
            ),
            (
                "What does the watchdog guarantee?",
                "What does the watchdog guarantee?",
            ),
            ("Ok. Who calls the daemon?", "Ok. Who calls the daemon?"),
            // Each signal on its own: a backticked plain word, a code word,
            // a code question after a sentence break.
            ("ask about `config`", "ask about `config`"),
            (
                "the parser panics on empty input",
                "the parser panics on empty input",
            ),
            (
                "Thanks. Where is the config loaded?",
                "Thanks. Where is the config loaded?",
            ),
            (
                "<pasted_content id=\"1\">\nchat\n</pasted_content id=\"1\">\nWhy does the parser panic?",
                "Why does the parser panic?",
            ),
            // A code question after any clause break, not only ". ".
            (
                "Hey, where is the config loaded?",
                "Hey, where is the config loaded?",
            ),
            (
                "Context:\nwhy does the daemon stall",
                "Context:\nwhy does the daemon stall",
            ),
            ("Done! where is the config", "Done! where is the config"),
            // A block glued to the words around it does not fuse them.
            (
                "where is<pasted_content>x</pasted_content>the config",
                "where is the config",
            ),
        ] {
            assert_eq!(
                retrieval_request(prompt).as_deref(),
                Some(typed),
                "{prompt}"
            );
        }
    }

    #[test]
    fn typed_text_should_drop_every_pasted_block_and_an_unclosed_one() {
        assert_eq!(
            typed_text(
                "a<pasted_content id=\"1\">x</pasted_content id=\"1\">b<pasted_content>y</pasted_content>c"
            ),
            "a b c"
        );
        // A longer tag name neither opens nor closes a block.
        assert_eq!(
            typed_text("<pasted_content>q </pasted_contentious>check guard.rs</pasted_content>x"),
            " x"
        );
        assert_eq!(
            typed_text("see <pasted_contents> here"),
            "see <pasted_contents> here"
        );
        // A tag cut off by the end of the prompt still opens a block.
        assert_eq!(typed_text("a <pasted_content"), "a ");
        // A block that ends the prompt leaves no trailing separator.
        assert_eq!(typed_text("x<pasted_content>y</pasted_content>"), "x");
        assert_eq!(
            typed_text("keep<pasted_content id=\"2\">never closed"),
            "keep"
        );
        assert_eq!(typed_text("x<pasted_content>y</pasted_content"), "x");
        assert_eq!(typed_text("plain"), "plain");
    }

    #[test]
    fn names_code_should_accept_code_shapes_and_reject_prose() {
        for token in [
            "a::b",
            "snake_case",
            "(snake_case)",
            "camelCase",
            "PascalCase",
            "src/main",
            "a.rs",
            "Cargo.toml",
            "x.tsx,",
            "guard.rs.",
            "`guard.rs`",
            "README.MD",
        ] {
            assert!(names_code(token), "{token}");
        }
        for token in [
            "Hello",
            "word",
            "_private",
            "trailing_",
            "a//b",
            "https://",
            "e.g.",
            ".rs",
            "v0.6",
            "ALLCAPS",
            "x.unknown",
            "/",
        ] {
            assert!(!names_code(token), "{token}");
        }
    }

    #[test]
    fn brief_should_carry_relevance_and_the_rendered_route() {
        let brief = from_scope_task("where is retrieval_route called", &json!({"targets": []}));
        assert_eq!(brief["asks_about_code"], true);
        assert_eq!(
            brief["retrieval_route_text"],
            pretty_retrieval_route(&retrieval_route("where is retrieval_route called"))
        );
        let ops = from_scope_task("go to branch main and pull", &json!({"targets": []}));
        assert_eq!(ops["asks_about_code"], false);
        assert_eq!(
            ops["retrieval_route"],
            retrieval_route("go to branch main and pull")
        );
        // A pasted block never reaches the brief's route: it is not typed text.
        let pasted = from_scope_task(
            "<pasted_content id=\"1\">\nthread about grep\n</pasted_content id=\"1\">\nWhy does the parser panic?",
            &json!({"targets": []}),
        );
        assert_eq!(pasted["asks_about_code"], true);
        assert_eq!(
            pasted["retrieval_route"],
            retrieval_route("Why does the parser panic?")
        );
    }

    /// `pad` padded so the whole text is `len` characters over four lines,
    /// the last two forming the final paragraph.
    fn pasted_text(len: usize) -> String {
        let rest = "\n\nwhy is the daemon slow\nafter startup";
        format!("{}{rest}", "p".repeat(len - rest.len()))
    }

    #[test]
    fn brief_task_should_keep_the_whole_text_up_to_the_character_and_line_bounds() {
        let tail = "why is the daemon slow\nafter startup";
        // Exactly 600 characters over four lines is not past the bound.
        let at = pasted_text(PASTE_TAIL_CHARS);
        assert_eq!(at.chars().count(), 600);
        assert_eq!(at.lines().count(), 4);
        assert_eq!(brief_task(&at), at);
        // One more character and the paste in front is dropped.
        let over = pasted_text(PASTE_TAIL_CHARS + 1);
        assert_eq!(brief_task(&over), tail);
        assert_eq!(brief_task(&pasted_text(1000)), tail);
        // Three lines are one paragraph and a question: still whole.
        let three = format!("{}\n\nwhy is it slow", "p".repeat(PASTE_TAIL_CHARS));
        assert_eq!(three.lines().count(), PASTE_TAIL_LINES - 1);
        assert_eq!(brief_task(&three), three);
        // Four lines is the lower bound that counts.
        let four = format!("{}\n\nwhy is\nit slow", "p".repeat(PASTE_TAIL_CHARS));
        assert_eq!(four.lines().count(), PASTE_TAIL_LINES);
        assert_eq!(brief_task(&four), "why is\nit slow");
    }

    #[test]
    fn brief_task_should_trim_and_keep_a_long_text_without_a_blank_line_whole() {
        let block = "log line\n".repeat(80);
        assert_eq!(brief_task(&block), block.trim());
        assert_eq!(brief_task("  short question  "), "short question");
        assert_eq!(brief_task(""), "");
    }

    #[test]
    fn last_paragraph_should_start_after_the_last_blank_line_of_any_kind() {
        assert_eq!(last_paragraph("a\n\nb"), "b");
        assert_eq!(last_paragraph("a\n   \nb\nc"), "b\nc");
        assert_eq!(last_paragraph("a\r\n\r\nb"), "b");
        assert_eq!(last_paragraph("a\n\nb\n\nc"), "c");
        assert_eq!(last_paragraph("a\nb"), "a\nb");
    }

    #[test]
    fn brief_signal_should_return_a_code_shaped_prompt_exactly_as_code_signal_does() {
        for prompt in [
            "Trace callers of `Foo::bar`",
            "where is retrieval_route called",
            "fix the failing test",
            "How does task preparation refresh stale source evidence?",
            "make TaskHookEvent cheaper",
        ] {
            assert!(code_signal(prompt).is_some(), "{prompt}");
            assert_eq!(brief_signal(prompt), code_signal(prompt), "{prompt}");
        }
    }

    #[test]
    fn brief_signal_should_take_plain_language_as_prose() {
        for prompt in [
            "the metrics line is missing when i run the pie harness",
            "so it would be nice if search content could print how many files matched",
            "comment on modifie un fichier point env sans perdre aucune cle",
            "relance la ci qui a plante",
            // Nothing in the shape says these are off topic: the evidence does.
            "what's the weather going to be like tomorrow",
            "quel temps fera-t-il demain",
        ] {
            assert_eq!(
                brief_signal(prompt),
                Some((prompt.to_string(), Signal::Prose)),
                "{prompt}"
            );
        }
    }

    #[test]
    fn brief_signal_should_refuse_operations_acknowledgements_and_one_word_replies() {
        for prompt in [
            "commit and push",
            "go to branch main and pull",
            "release 0.6.2",
            "pousse sur main",
            "merge main and push it",
            "deploy v12 to production",
            "thanks, that works",
            "thanks, that looks good",
            "merci parfait",
            "daemon",
            "thanks daemon",
            "",
            "   ",
        ] {
            assert_eq!(brief_signal(prompt), None, "{prompt:?}");
        }
    }

    #[test]
    fn brief_signal_should_need_two_content_words_and_not_one() {
        assert_eq!(MIN_PROSE_KEYWORDS, 2);
        assert_eq!(
            brief_signal("daemon startup").map(|found| found.1),
            Some(Signal::Prose)
        );
        assert_eq!(brief_signal("daemon"), None);
        assert_eq!(brief_signal("the daemon"), None);
        // An operation word beside a content word is a content word's prompt.
        assert_eq!(
            brief_signal("push daemon startup").map(|found| found.1),
            Some(Signal::Prose)
        );
    }

    #[test]
    fn brief_signal_should_ignore_a_pasted_block_and_judge_the_last_paragraph_of_a_paste() {
        let tagged = "<pasted_content id=\"1\">\nthe daemon startup race watch\n</pasted_content id=\"1\">\nthanks";
        assert_eq!(brief_signal(tagged), None);
        // An untagged paste of a thread, then "thoughts?": one content word.
        let untagged = format!(
            "{}\n\nthoughts?",
            "slack message about the daemon startup\n".repeat(30)
        );
        assert!(untagged.chars().count() > PASTE_TAIL_CHARS);
        assert_eq!(brief_signal(&untagged), None);
        // The same paste with a real question after it.
        let asked = format!(
            "{}\n\nwhy is the daemon slow after startup",
            "chat line\n".repeat(80)
        );
        assert_eq!(
            brief_signal(&asked),
            Some((
                "why is the daemon slow after startup".to_string(),
                Signal::Prose
            ))
        );
    }

    #[test]
    fn is_ops_request_should_need_every_keyword_to_be_an_operation_or_a_version() {
        let words = |text: &[&str]| text.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert!(is_ops_request(&words(&["commit", "push"])));
        assert!(is_ops_request(&words(&["release", "2026"])));
        assert!(is_ops_request(&words(&["deploy", "v12"])));
        assert!(is_ops_request(&words(&["pousse", "sur", "main"])));
        assert!(!is_ops_request(&words(&["commit", "parser"])));
        assert!(!is_ops_request(&words(&["parser", "push"])));
        assert!(!is_ops_request(&words(&["parser", "daemon"])));
    }

    #[test]
    fn is_version_shaped_should_take_digits_with_an_optional_leading_v() {
        for word in ["2026", "v12", "7", "v7"] {
            assert!(is_version_shaped(word), "{word}");
        }
        for word in ["", "v", "vx", "12a", "release", "v1x"] {
            assert!(!is_version_shaped(word), "{word}");
        }
    }

    #[test]
    fn brief_keywords_should_drop_conversation_and_keep_content_words() {
        assert_eq!(
            brief_keywords("thanks, the daemon startup works"),
            ["daemon", "startup"]
        );
        assert!(brief_keywords("merci, parfait").is_empty());
        assert!(brief_keywords("").is_empty());
    }

    #[test]
    fn signal_should_name_itself_for_the_decision_log() {
        assert_eq!(Signal::Strong.as_str(), "strong");
        assert_eq!(Signal::Weak.as_str(), "weak");
        assert_eq!(Signal::Prose.as_str(), "prose");
    }
}
