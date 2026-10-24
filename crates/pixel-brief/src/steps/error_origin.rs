// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `error-origin` (`specs/brief.md`, "Recipes"): "where does this error come
//! from" is answered with the lines that can emit it, each labelled by what its
//! syntax establishes and no more.
//!
//! The message is the one the prompt quotes (found literally, the longest
//! leading run of at least three words, inside a string of a non-test source
//! line) or else the string literal that carries most of the prompt's words,
//! each weighed by how few source lines carry it; a literal found in a JSON
//! message catalogue is followed to the code that names its key. Beside it the
//! step quotes the nearest diagnostic emitter (`eprintln!`, `bail!`, `panic!`,
//! `throw new Error`) whose function carries two salient prompt words, the
//! function around the origin, the lexical callers of that function two levels
//! up (each marked as a candidate: a name does not resolve a receiver) and one
//! test that calls it by name.
//!
//! Not ported: the Python-AST lanes (a handler's caught type, its raises and
//! the import boundary; the exact role of a line), which need a parser the
//! step does not have: a Python line's role is read from its shape; the default
//! 404 handler lane, which reads one web framework's request/response callbacks
//! and its package manifest; the emitter lanes for `print_error!` and
//! `next(err)`, which are one tool's and one framework's. The gate is a quoted
//! message or an error word, not the script's `text` or `404 … page` clauses.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use pixel_model::is_test_path;
use regex::Regex;

use crate::chain::RichHit;
use crate::region_pack::Block;
use crate::routes::QuestionKind;

use super::{Ctx, Outcome, Step};

const SOURCE_EXT: &[&str] = &["rs", "py", "ts", "tsx", "js", "go"];
const JS_EXT: &[&str] = &["js", "ts", "tsx", "jsx"];
const MIN_WORDS: usize = 3;
const MIN_SHARED: usize = 2;
const SEED_WORDS: usize = 4;
const SEED_HITS: usize = 300;
const CALLER_LINES: usize = 14;
const LEVELS: usize = 2;
const MAX_LINES: usize = 220;
const MAX_BYTES: usize = 9000;
const LINE_CHARS: usize = 160;
const ROWS: usize = 2000;
const CALLER_ROWS: usize = 40;
const TEST_LINES: usize = 24;
const REACH: usize = 8;
const WINDOW: usize = 60;

const STOP: &[&str] = &[
    "what", "where", "which", "that", "this", "with", "from", "does", "how", "the", "and", "are",
    "they", "them", "for", "into", "about", "when", "should", "their", "there", "these", "those",
    "have", "your", "one", "than", "its", "was", "has", "can", "not", "but", "all", "any", "you",
    "of", "to", "in", "is", "it", "on", "at", "by", "an", "or", "as", "be", "do", "if", "so", "up",
    "me", "my", "we", "us", "am",
];
/// Words a question about an error uses; they say nothing about which one.
const GENERIC: &[&str] = &[
    "error",
    "exception",
    "alert",
    "message",
    "warning",
    "text",
    "code",
    "where",
    "which",
    "what",
    "when",
    "does",
    "do",
    "how",
    "the",
    "and",
    "or",
    "is",
    "are",
    "was",
    "were",
    "be",
    "not",
    "printed",
    "print",
    "produced",
    "defined",
    "raised",
    "raise",
    "emitted",
    "emits",
    "called",
    "comes",
    "come",
    "from",
    "that",
    "says",
    "reads",
    "contains",
    "default",
    "page",
];

static GATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"['\x22`‘“][^'\x22`’”]{12,}['\x22`’”]|(?i)\b(errors?|exceptions?|messages?|warnings?|alerts?|panics?)\b",
    )
    .expect("the error-question pattern is a literal")
});
static QUOTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"['\x22`‘“]([^'\x22`’”]{12,})['\x22`’”]").expect("the quoted pattern is a literal")
});
static FN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:fn|def|function)\s+([A-Za-z_]\w*)|^\s*(?:export\s+)?(?:const|let)\s+([A-Za-z_]\w*)\s*=\s*(?:async\s*)?\(",
    )
    .expect("the function pattern is a literal")
});
static COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(//|#|\*|/\*)").expect("the comment pattern is a literal"));
static MOD_TESTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(pub\s+)?mod tests\b").expect("the test-module pattern is a literal")
});
static CATALOGUE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*"([A-Za-z_][\w.-]*)"\s*:"#).expect("the catalogue-key pattern is a literal")
});
static EMITTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:eprintln!|bail!|panic!|throw\s+(?:new\s+)?(?:Error|Exception))")
        .expect("the emitter pattern is a literal")
});
static PY_RAISE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*raise\b").expect("the raise pattern is a literal"));
static PY_DEFINITION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*[A-Za-z_]\w*(\s*:[^=]+)?\s*=\s*[rbfuRBFU]*["']"#)
        .expect("the message-definition pattern is a literal")
});
static PY_STRING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*[rbfuRBFU]*("""|'''|"|')"#)
        .expect("the standalone-string pattern is a literal")
});

pub(super) fn run(ctx: &Ctx, out: &mut Outcome) {
    if !ctx.steps.enabled(Step::ErrorOrigin)
        || matches!(ctx.kind, QuestionKind::History | QuestionKind::Review)
        || !GATE.is_match(ctx.typed)
    {
        return;
    }
    let _timer = ctx.timer(Step::ErrorOrigin);
    let mut files = Files {
        ctx,
        cache: BTreeMap::new(),
    };
    let diagnostic = emission(ctx, &mut files);
    let mut origin = origins(ctx, &mut files);
    if let (Some(found), Some(shown)) = (&origin, &diagnostic)
        && found.path == shown.path
        && !found.how.starts_with("quoted occurrence")
    {
        origin = None;
    }
    let mut blocks: Vec<Block> = diagnostic.map(|found| found.blocks).unwrap_or_default();
    if let Some(found) = origin {
        origin_blocks(ctx, &mut files, &found, &mut blocks);
    }
    out.append.extend(cut(blocks));
}

/// The blocks kept within [`MAX_BYTES`], whole.
fn cut(blocks: Vec<Block>) -> Vec<Block> {
    let mut used = 0;
    let mut kept = Vec::new();
    for block in blocks {
        used += block.text().len() + 1;
        if used > MAX_BYTES && !kept.is_empty() {
            break;
        }
        kept.push(block);
    }
    kept
}

/// Lines of tracked files, read once.
struct Files<'a> {
    ctx: &'a Ctx<'a>,
    cache: BTreeMap<String, Vec<String>>,
}

impl Files<'_> {
    fn get(&mut self, path: &str) -> &[String] {
        let ctx = self.ctx;
        self.cache.entry(path.to_string()).or_insert_with(|| {
            ctx.read(path)
                .map(|text| text.lines().map(ToString::to_string).collect())
                .unwrap_or_default()
        })
    }
}

fn extension(path: &str) -> &str {
    path.rsplit_once('.').map_or("", |(_, ext)| ext)
}

fn is_source(path: &str) -> bool {
    SOURCE_EXT.contains(&extension(path))
}

fn is_js(path: &str) -> bool {
    JS_EXT.contains(&extension(path))
}

/// The matching lines of the tracked source files (and JSON when
/// `catalogues`), by path and line.
fn search(ctx: &Ctx, pattern: &str, catalogues: bool) -> Vec<(String, usize, String)> {
    let Ok(found) = ctx.evidence.grep(pattern, ROWS, ctx.deadline) else {
        return Vec::new();
    };
    let mut rows: Vec<(String, usize, String)> = found
        .hits
        .into_iter()
        .filter(|hit: &RichHit| {
            is_source(&hit.path) || (catalogues && extension(&hit.path) == "json")
        })
        .filter_map(|hit| {
            let text = hit.text?;
            Some((hit.path, usize::try_from(hit.line).ok()?, text))
        })
        .collect();
    rows.sort();
    rows
}

// ---------------------------------------------------------------------------
// Words
// ---------------------------------------------------------------------------

/// The words of code or prose, split at case changes, underscores and
/// punctuation, lowercased: an upper-case run, a capitalised word, a lower-case
/// word, a number.
fn tokens(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let ch = chars[at];
        let start = at;
        if ch.is_ascii_digit() {
            while at < chars.len() && chars[at].is_ascii_digit() {
                at += 1;
            }
        } else if ch.is_ascii_uppercase() {
            let mut end = at;
            while end < chars.len() && chars[end].is_ascii_uppercase() {
                end += 1;
            }
            let lower_next = end < chars.len() && chars[end].is_ascii_lowercase();
            if lower_next && end - at >= 2 {
                at = end - 1;
            } else if lower_next {
                at += 1;
                while at < chars.len() && chars[at].is_ascii_lowercase() {
                    at += 1;
                }
            } else {
                at = end;
            }
        } else if ch.is_ascii_lowercase() {
            while at < chars.len() && chars[at].is_ascii_lowercase() {
                at += 1;
            }
        } else {
            at += 1;
            continue;
        }
        out.push(chars[start..at].iter().collect::<String>().to_lowercase());
    }
    out
}

fn stem(word: &str) -> String {
    if word.len() > 4 && word.ends_with("ies") {
        return format!("{}y", &word[..word.len() - 3]);
    }
    for suffix in ["ing", "ed", "s"] {
        if word.len() >= suffix.len() + 3 && word.ends_with(suffix) {
            return word[..word.len() - suffix.len()].to_string();
        }
    }
    word.to_string()
}

fn prompt_words(prompt: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    tokens(prompt)
        .into_iter()
        .filter(|word| word.len() >= 2 && !STOP.contains(&word.as_str()))
        .map(|word| stem(&word))
        .filter(|word| seen.insert(word.clone()))
        .collect()
}

/// A token starting with the word carries it; a word of three letters or fewer
/// must be the token (`no` is not in `not`).
fn carried(word: &str, tokens: &BTreeSet<String>) -> bool {
    if word.len() <= 3 {
        tokens.contains(word)
    } else {
        tokens.iter().any(|token| token.starts_with(word))
    }
}

/// The text inside the string literals of a code line (`'…'`, `"…"`, `` `…` ``),
/// joined.
fn string_part(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let quote = chars[at];
        if !matches!(quote, '"' | '\'' | '`') {
            at += 1;
            continue;
        }
        let mut end = at + 1;
        let mut closed = false;
        while end < chars.len() {
            if chars[end] == '\\' {
                end += 2;
            } else if chars[end] == quote {
                closed = true;
                break;
            } else {
                end += 1;
            }
        }
        if closed {
            parts.push(chars[at + 1..end].iter().collect());
            at = end + 1;
        } else {
            at += 1;
        }
    }
    parts.join(" ")
}

// ---------------------------------------------------------------------------
// Strings that span lines
// ---------------------------------------------------------------------------

/// How many string delimiters a stretch of code holds: triple quotes in
/// Python, backticks in JavaScript, unescaped double quotes (outside char
/// literals) elsewhere; an odd count leaves a string open.
fn open_quotes(text: &str, path: &str) -> usize {
    if extension(path) == "py" {
        let chars: Vec<char> = text.chars().collect();
        let (mut count, mut at) = (0, 0);
        while at + 3 <= chars.len() {
            let triple = chars[at..at + 3].iter().all(|ch| *ch == '"')
                || chars[at..at + 3].iter().all(|ch| *ch == '\'');
            if triple {
                count += 1;
                at += 3;
            } else {
                at += 1;
            }
        }
        return count;
    }
    if is_js(path) {
        return text.matches('`').count();
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .iter()
        .enumerate()
        .filter(|(at, ch)| **ch == '"' && (*at == 0 || !matches!(chars[at - 1], '\\' | '\'')))
        .count()
}

/// Whether line `index` sits inside a string that began on a line above. A
/// message spans a few lines: parity over a bounded window keeps it cheap.
fn in_multiline_string(lines: &[String], index: usize, path: &str) -> bool {
    let start = index.saturating_sub(WINDOW);
    open_quotes(&lines[start..index].join("\n"), path) % 2 == 1
}

/// First and last line index of the multi-line string that holds line `index`
/// (itself when it holds none), or `None` when the string runs past [`REACH`]
/// lines on either side: a manual page or a doc block, not a message.
fn string_span(lines: &[String], index: usize, path: &str) -> Option<(usize, usize)> {
    let mut lo = index;
    while in_multiline_string(lines, lo, path) {
        if lo <= index.saturating_sub(REACH) {
            return None;
        }
        lo -= 1;
    }
    let mut hi = index;
    while hi + 1 < lines.len() && in_multiline_string(lines, hi + 1, path) {
        if hi >= index + REACH {
            return None;
        }
        hi += 1;
    }
    Some((lo, hi))
}

// ---------------------------------------------------------------------------
// Functions and tests
// ---------------------------------------------------------------------------

/// The name and line index of the function around line `index`; the
/// `const x = (` form counts in JavaScript and TypeScript only.
fn enclosing(lines: &[String], index: usize, js: bool) -> Option<(String, usize)> {
    if lines.is_empty() {
        return None;
    }
    for at in (0..=index.min(lines.len() - 1)).rev() {
        if let Some(caps) = FN.captures(&lines[at]) {
            if let Some(name) = caps.get(1) {
                return Some((name.as_str().to_string(), at));
            }
            if js && let Some(name) = caps.get(2) {
                return Some((name.as_str().to_string(), at));
            }
        }
    }
    None
}

/// Test code: a tests path, or after a `mod tests`.
fn in_test(path: &str, lines: &[String], index: usize) -> bool {
    is_test_path(path)
        || lines
            .iter()
            .take(index)
            .any(|line| MOD_TESTS.is_match(line))
}

/// The words of what emits a string that starts on line `first`: the call
/// opening on that line or the one above, and the name of the enclosing
/// function.
fn emitter_words(lines: &[String], first: usize, path: &str) -> BTreeSet<String> {
    let name = enclosing(lines, first, is_js(path)).map(|(name, _)| name);
    let above = first.checked_sub(1).map_or("", |at| lines[at].as_str());
    let head = lines[first].split('"').next().unwrap_or("");
    let head = head.split('\'').next().unwrap_or("");
    tokens(&format!("{} {above} {head}", name.unwrap_or_default()))
        .into_iter()
        .collect()
}

/// A numbered excerpt, 1-based and inclusive, clipped to the file.
fn show(path: &str, lines: &[String], lo: i64, hi: i64, label: &str) -> Block {
    let lo = usize::try_from(lo.max(1)).unwrap_or(1);
    let hi = usize::try_from(hi.max(0)).unwrap_or(0).min(lines.len());
    let body = (lo..=hi)
        .map(|at| {
            let text: String = lines[at - 1].chars().take(LINE_CHARS).collect();
            format!("{at}| {text}")
        })
        .collect();
    Block::new(format!("{path}:{lo}-{hi} — {label}"), body)
}

fn signed(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

// ---------------------------------------------------------------------------
// The message
// ---------------------------------------------------------------------------

/// Where a message is, and how the step came by it.
struct Origin {
    path: String,
    line: usize,
    how: String,
    catalogue: Option<(String, usize)>,
}

/// Whether the fragment sits inside a string literal of a code line: a quote
/// before it, and the line is no comment.
fn in_string(text: &str, fragment: &str) -> bool {
    text.find(fragment).is_some_and(|at| {
        at > 0 && text[..at].contains(['"', '\'', '`']) && !COMMENT.is_match(text)
    })
}

/// Hits of the longest leading run of `message` (at least [`MIN_WORDS`] words)
/// found literally in non-test source.
fn locate(ctx: &Ctx, message: &str) -> Vec<(String, usize, String)> {
    let trimmed = message.trim();
    let mut pieces: Vec<&str> = Vec::new();
    let mut start = 0;
    for (at, ch) in trimmed.char_indices() {
        let space_before = trimmed[..at].chars().next_back().map(char::is_whitespace);
        if at > 0 && space_before != Some(ch.is_whitespace()) {
            pieces.push(&trimmed[start..at]);
            start = at;
        }
    }
    if start < trimmed.len() {
        pieces.push(&trimmed[start..]);
    }
    let mut count = pieces.len();
    while count > MIN_WORDS + 1 {
        let fragment: String = pieces[..count].concat();
        let fragment = fragment.split('(').next().unwrap_or("");
        let fragment = fragment.split('{').next().unwrap_or("");
        let fragment = fragment.trim_matches([' ', ':']);
        if fragment.split_whitespace().count() < MIN_WORDS {
            break;
        }
        let hits: Vec<(String, usize, String)> = search(ctx, &regex::escape(fragment), false)
            .into_iter()
            .filter(|(path, _, text)| !is_test_path(path) && in_string(text, fragment))
            .collect();
        if !hits.is_empty() {
            return hits;
        }
        count = count.saturating_sub(2);
    }
    Vec::new()
}

/// The message the prompt asks about: a quoted one found literally first, else
/// the literal the prompt describes.
fn origins(ctx: &Ctx, files: &mut Files) -> Option<Origin> {
    for caps in QUOTED.captures_iter(ctx.typed).take(3) {
        if let Some((path, line, _)) = locate(ctx, &caps[1]).into_iter().next() {
            return Some(Origin {
                path,
                line,
                how: "quoted occurrence candidate; emission unverified".to_string(),
                catalogue: None,
            });
        }
    }
    let (path, line, catalogue) = described(ctx, files)?;
    Some(Origin {
        path,
        line,
        how: "described occurrence candidate; emission unverified".to_string(),
        catalogue,
    })
}

/// Where a described message is: the path, the line and, for a literal in a
/// catalogue, the catalogue's path and line.
type Described = (String, usize, Option<(String, usize)>);

/// The best literal so far: its ranking key, path, line and text.
type Best = ((i64, f64, String, usize), String, usize, String);

/// A message the prompt describes without quoting it: the string literal that
/// carries most of the prompt's words (two at least), each weighed by how few
/// source lines carry it. Candidate lines come from the rarest words; a literal
/// in a message catalogue (JSON) is followed to the code that names its key.
fn described(ctx: &Ctx, files: &mut Files) -> Option<Described> {
    let words = prompt_words(ctx.typed);
    let mut seeds: BTreeMap<String, Vec<(String, usize, String)>> = BTreeMap::new();
    let mut df: BTreeMap<String, usize> = BTreeMap::new();
    for word in &words {
        if ctx.timed_out() {
            return None;
        }
        let rows = search(ctx, &format!("(?i){}", regex::escape(word)), true);
        df.insert(word.clone(), rows.len());
        seeds.insert(word.clone(), rows);
    }
    let mut present: Vec<&String> = words.iter().filter(|word| df[*word] > 0).collect();
    present.sort_by(|a, b| df[*a].cmp(&df[*b]).then(a.cmp(b)));
    if present.len() < MIN_SHARED {
        return None;
    }
    let total = df.values().sum::<usize>() as f64 + 1.0;
    let idf: BTreeMap<&String, f64> = present
        .iter()
        .map(|word| (*word, (total / df[*word] as f64).ln()))
        .collect();
    let mut best: Option<Best> = None;
    let mut seen: BTreeSet<(String, usize)> = BTreeSet::new();
    for word in present.iter().take(SEED_WORDS) {
        for (path, line, text) in seeds[*word].iter().take(SEED_HITS) {
            if !seen.insert((path.clone(), *line)) || is_test_path(path) || COMMENT.is_match(text) {
                continue;
            }
            let mut message = string_part(text);
            let mut context = BTreeSet::new();
            if extension(path) != "json" {
                let lines = files.get(path);
                let index = line - 1;
                if index >= lines.len() || in_test(path, lines, index) {
                    continue;
                }
                let mut span = string_span(lines, index, path);
                if let Some((first, last)) = span
                    && first == last
                    && open_quotes(text, path) % 2 == 1
                {
                    span = if index + 1 < lines.len() {
                        string_span(lines, index + 1, path).map(|(_, hi)| (index, hi))
                    } else {
                        None
                    };
                }
                let (first, last) = span?;
                if last > first {
                    message = lines[first..=last].join(" ");
                }
                context = emitter_words(lines, first, path);
            }
            let in_message: BTreeSet<String> = tokens(&message).into_iter().collect();
            if !present.iter().any(|word| carried(word, &in_message)) {
                continue;
            }
            let inside: BTreeSet<String> = in_message.union(&context).cloned().collect();
            let shared: Vec<&&String> = present
                .iter()
                .filter(|word| carried(word, &inside))
                .collect();
            if shared.len() < MIN_SHARED {
                continue;
            }
            // More of the question's words first; then rarer words packed in a
            // shorter text (a message, not a manual).
            let weight: f64 = shared.iter().map(|word| idf[**word]).sum();
            let length = tokens(&message).len() as f64;
            let key = (
                -signed(shared.len()),
                -weight / (4.0 + length).log2(),
                path.clone(),
                *line,
            );
            let better = best.as_ref().is_none_or(|(held, ..)| {
                key.0
                    .cmp(&held.0)
                    .then(key.1.total_cmp(&held.1))
                    .then(key.2.cmp(&held.2))
                    .then(key.3.cmp(&held.3))
                    .is_lt()
            });
            if better {
                best = Some((key, path.clone(), *line, text.clone()));
            }
        }
    }
    let (_, path, line, text) = best?;
    if extension(&path) == "json" {
        let name = CATALOGUE_KEY.captures(&text)?[1].to_string();
        let pattern = format!(r"\b{}\b", regex::escape(&name));
        return search(ctx, &pattern, false)
            .into_iter()
            .find(|(found, ..)| !is_test_path(found))
            .map(|(found, at, _)| (found, at, Some((path.clone(), line))));
    }
    Some((path, line, None))
}

// ---------------------------------------------------------------------------
// The emitter, the origin and the callers
// ---------------------------------------------------------------------------

/// A diagnostic emitter whose function carries salient prompt words.
struct Emission {
    path: String,
    blocks: Vec<Block>,
}

/// The bounded diagnostic emitter whose nearby source carries two salient
/// prompt terms.
fn emission(ctx: &Ctx, files: &mut Files) -> Option<Emission> {
    let salient: BTreeSet<String> = prompt_words(ctx.typed)
        .into_iter()
        .filter(|word| !GENERIC.contains(&word.as_str()))
        .collect();
    if salient.len() < 2 {
        return None;
    }
    let mut best: Option<((i64, usize, String, usize), Emission)> = None;
    for (path, line, text) in search(ctx, EMITTER.as_str(), false) {
        if ctx.timed_out() {
            break;
        }
        if is_test_path(&path) {
            continue;
        }
        let lines = files.get(&path);
        if line == 0 || line > lines.len() {
            continue;
        }
        let js = is_js(&path);
        let function_start = enclosing(lines, line - 1, js).map_or(0, |(_, at)| at);
        let lo_context = function_start.max(line.saturating_sub(5));
        let mut hi_context = lines.len().min(line + 4);
        let mut function_end = lines.len();
        if let Some(following) = (line..hi_context).find(|at| {
            FN.captures(&lines[*at])
                .is_some_and(|caps| caps.get(1).is_some() || js)
        }) {
            hi_context = following;
            function_end = following;
        }
        let nearby: BTreeSet<String> = tokens(&lines[lo_context..hi_context].join(" "))
            .iter()
            .map(|token| stem(token))
            .collect();
        let shared = salient.intersection(&nearby).count();
        if shared < 2 {
            continue;
        }
        let priority = if text.contains("eprintln!") {
            0
        } else if text.contains("throw") {
            1
        } else if text.contains("bail!") {
            2
        } else {
            3
        };
        let rank = (-signed(shared), priority, path.clone(), line);
        if best.as_ref().is_some_and(|(held, _)| rank >= *held) {
            continue;
        }
        let block = show(
            &path,
            lines,
            signed(function_start + 1).max(signed(line) - 6),
            signed(function_end).min(signed(line) + 6),
            "error-emission candidate; matching terms and source condition, exact runtime cause unproven",
        );
        best = Some((
            rank,
            Emission {
                path,
                blocks: vec![block],
            },
        ));
    }
    best.map(|(_, emission)| emission)
}

/// What the syntax of a Python line establishes, read from its shape.
fn python_role(lines: &[String], index: usize, path: &str) -> &'static str {
    let line = &lines[index];
    if PY_RAISE.is_match(line) {
        "local raise syntax; message origin candidate"
    } else if PY_DEFINITION.is_match(line) {
        "message definition; emission unverified"
    } else if PY_STRING.is_match(line) || in_multiline_string(lines, index, path) {
        "documentation occurrence; no emission proved"
    } else {
        "occurrence candidate; emission unverified"
    }
}

/// The files that define a function called `name`.
fn definitions(ctx: &Ctx, name: &str) -> BTreeSet<String> {
    let pattern = format!(r"\b(fn|def|function)\s+{}\b", regex::escape(name));
    search(ctx, &pattern, false)
        .into_iter()
        .map(|(path, ..)| path)
        .collect()
}

fn origin_blocks(ctx: &Ctx, files: &mut Files, origin: &Origin, out: &mut Vec<Block>) {
    let lines = files.get(&origin.path).to_vec();
    let index = origin.line - 1;
    if index >= lines.len() {
        return;
    }
    let function = enclosing(&lines, index, is_js(&origin.path));
    let how = if extension(&origin.path) == "py" {
        python_role(&lines, index, &origin.path)
    } else {
        origin.how.as_str()
    };
    let scope = function.as_ref().map_or(String::new(), |(name, _)| {
        format!(" near {name} (scope unresolved)")
    });
    out.push(Block::new(
        format!(
            "error origin — {how} at {}:{}{scope}",
            origin.path, origin.line
        ),
        Vec::new(),
    ));
    if let Some((path, line)) = &origin.catalogue {
        let catalogue = files.get(path).to_vec();
        out.push(show(
            path,
            &catalogue,
            signed(*line),
            signed(*line),
            "message definition in catalogue; emission unverified",
        ));
    }
    let lo = function
        .as_ref()
        .map_or(signed(origin.line) - 6, |(_, start)| {
            signed(start + 1).max(signed(origin.line) - 12)
        });
    out.push(show(
        &origin.path,
        &lines,
        lo,
        signed(origin.line) + 6,
        "candidate source occurrence; inspect the evidence category above",
    ));
    let Some((name, _)) = function else {
        return;
    };
    let mut seen = BTreeSet::new();
    seen.insert((origin.path.clone(), name.clone(), name.clone()));
    let dirs: Vec<&str> = origin.path.split('/').collect();
    let home = dirs[..dirs.len() - 1]
        .iter()
        .take(2)
        .copied()
        .collect::<Vec<_>>()
        .join("/");
    callers(
        ctx,
        files,
        &name,
        &mut seen,
        LEVELS,
        out,
        &home,
        &origin.path,
    );
    let pattern = format!(r"\b{}\s*\(", regex::escape(&name));
    for (other, at, _) in search(ctx, &pattern, false) {
        let other_lines = files.get(&other).to_vec();
        if at == 0 || at > other_lines.len() || !in_test(&other, &other_lines, at - 1) {
            continue;
        }
        if let Some((test, start)) = enclosing(&other_lines, at - 1, false) {
            out.push(show(
                &other,
                &other_lines,
                signed(start + 1),
                signed(start + TEST_LINES),
                &format!("test {test}: name match for {name}; binding unresolved"),
            ));
            break;
        }
    }
}

fn lines_in(blocks: &[Block]) -> usize {
    blocks.iter().map(|block| 1 + block.body().len()).sum()
}

/// Bounded lexical call candidates; directory proximity does not resolve
/// receiver or binding identity.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn callers(
    ctx: &Ctx,
    files: &mut Files,
    name: &str,
    seen: &mut BTreeSet<(String, String, String)>,
    depth: usize,
    out: &mut Vec<Block>,
    home: &str,
    defined_in: &str,
) {
    if depth == 0 || lines_in(out) > MAX_LINES || ctx.timed_out() {
        return;
    }
    let near: Option<String> = (definitions(ctx, name).len() > 1).then(|| {
        defined_in
            .rsplit_once('/')
            .map_or(String::new(), |(dir, _)| format!("{dir}/"))
    });
    let call = format!(r"\b{}\s*\(", regex::escape(name));
    let definition = Regex::new(&format!(r"\b(fn|def|function)\s+{}\b", regex::escape(name))).ok();
    let mut frontier: Vec<(String, String)> = Vec::new();
    for (path, line, text) in search(ctx, &call, false).into_iter().take(CALLER_ROWS) {
        if definition.as_ref().is_some_and(|re| re.is_match(&text)) {
            continue;
        }
        let lines = files.get(&path).to_vec();
        if line == 0 || line > lines.len() {
            continue;
        }
        let index = line - 1;
        if in_test(&path, &lines, index) {
            continue;
        }
        if near
            .as_ref()
            .is_some_and(|dir| !path.starts_with(dir.as_str()))
        {
            continue;
        }
        if !home.is_empty() && depth < LEVELS && !path.starts_with(&format!("{home}/")) {
            continue;
        }
        let Some((caller, start)) = enclosing(&lines, index, is_js(&path)) else {
            continue;
        };
        if !seen.insert((path.clone(), caller.clone(), name.to_string())) {
            continue;
        }
        let lo = signed(start + 1).max(signed(index + 1) - signed(CALLER_LINES));
        if lo > signed(start) + 4 {
            out.push(show(
                &path,
                &lines,
                signed(start + 1),
                signed(start + 3),
                &format!("{caller}: lexical caller candidate (scope/binding unresolved)"),
            ));
        }
        let independent = if depth < LEVELS {
            "; independent context, no composed path"
        } else {
            ""
        };
        out.push(show(
            &path,
            &lines,
            lo,
            signed(index) + 4,
            &format!(
                "{caller} → {name}: lexical call candidate; receiver/binding unresolved; scope unverified{independent}"
            ),
        ));
        if depth > 1 && near.is_none() {
            frontier.push((caller, path));
        }
    }
    // Every direct match before indirect context can claim the shared slot.
    for (caller, path) in frontier {
        callers(ctx, files, &caller, seen, depth - 1, out, home, &path);
    }
}

#[cfg(test)]
mod tests;
