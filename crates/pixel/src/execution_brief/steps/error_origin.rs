// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `error-origin`: quote error evidence without promoting lexical
//! occurrences to runtime causes.
//!
//! Ported from `probes/error-origin/probe`. The question's quoted message (or
//! the string literal that carries most of its words) is located in the
//! source; Python syntax tells a message definition from documentation and a
//! local raise; a typed `except` clause that rethrows adds its branch and the
//! import it crosses; a diagnostic emitter whose nearby source carries two of
//! the question's terms is quoted; a default request handler that falls back
//! to an imported dependency is shown beside its declaration. Name-only
//! callers and test links stay candidates, never a causal chain.
//!
//! The script searched the working tree with `rg`; the step walks it the same
//! way (hidden and ignored files skipped, path order, binary files left out),
//! and reads Python syntax with tree-sitter instead of `ast`.

mod js;
mod python;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::LazyLock;

use pixel_plugin::ProbeMode;
use regex::Regex;

use super::text::{head, re, subwords};
use super::walk::Walk;
use super::{Ctx, Step};

pub(super) const STEP: Step = Step {
    name: "error-origin",
    when_kinds: &[],
    skip_kinds: &[],
    when_prompt: Some(
        r#"['"`‘“][^'"`’”]{12,}['"`’”]|(?i:\b(errors?|exceptions?|messages?|warnings?|alerts?|panics?|text)\b)|(?i:\b(?:404|not[\s-]+found)\b.{0,60}\b(?:page|handler|response|message|text|produced|generated|comes? from|created)\b|\b(?:page|handler|response|message|text)\b.{0,60}\b(?:404|not[\s-]+found)\b)"#,
    ),
    when_gate_closed: false,
    budget_ms: 2000,
    mode: ProbeMode::Append,
    run,
};

const SRC: &[&str] = &[".rs", ".py", ".ts", ".tsx", ".js", ".go"];
const SRC_AND_JSON: &[&str] = &[".rs", ".py", ".ts", ".tsx", ".js", ".go", ".json"];
pub(super) const JS_EXT: &[&str] = &[".js", ".ts", ".tsx", ".jsx"];
const MIN_QUOTE: usize = 12;
const MIN_WORDS: usize = 3;
const CALLER_LINES: i64 = 14;
const LEVELS: usize = 2;
const MAX_LINES: usize = 220;
const MAX_BYTES: usize = 9000;
const MAX_DIAGNOSTIC_FILES: usize = 1_500;
const MAX_DIAGNOSTIC_PATH_BYTES: usize = 180_000;
const MAX_DIAGNOSTIC_FILE_BYTES: u64 = 1_000_000;
const MAX_DIAGNOSTIC_LISTING_BYTES: usize = 512_000;
/// `--max-filesize 1M` of the script's diagnostic search.
const MAX_SEARCHED_FILE_BYTES: u64 = 1_048_576;
/// `--max-count 8` of the same search.
const MAX_HITS_PER_FILE: usize = 8;
pub(super) const MIN_SHARED: usize = 2;
const SEED_WORDS: usize = 4;
const SEED_HITS: usize = 300;

const STOP: &[&str] = &[
    "what", "where", "which", "that", "this", "with", "from", "does", "how", "the", "and", "are",
    "they", "them", "for", "into", "about", "when", "should", "their", "there", "these", "those",
    "have", "your", "one", "than", "its", "was", "has", "can", "not", "but", "all", "any", "you",
    "of", "to", "in", "is", "it", "on", "at", "by", "an", "or", "as", "be", "do", "if", "so", "up",
    "me", "my", "we", "us", "am",
];
const GENERIC: &[&str] = &[
    "error", "exception", "alert", "message", "warning", "text", "code", "where", "which", "what",
    "when", "does", "do", "how", "the", "and", "or", "is", "are", "was", "were", "be", "not",
    "printed", "print", "produced", "defined", "raised", "raise", "emitted", "emits", "called",
    "comes", "come", "from", "that", "says", "reads", "contains", "default", "page",
];


static FN: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:fn|def|function)\s+([A-Za-z_]\w*)|^\s*(?:export\s+)?(?:const|let)\s+([A-Za-z_]\w*)\s*=\s*(?:async\s*)?\(",
    )
});
static QUOTED: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!(r#"['"`‘“]([^'"`’”]{{{MIN_QUOTE},}})['"`’”]"#))
});
static COMMENT: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*(//|#|\*|/\*)"));
static TEST_PATH: LazyLock<Regex> = LazyLock::new(|| re(r"(^|/)tests?/|_test\.|test_"));
static TEST_MODULE: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*(pub\s+)?mod tests\b"));
static EXAMPLES: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)(^|/)examples?/"));
static CATALOGUE_KEY: LazyLock<Regex> =
    LazyLock::new(|| re(r#"^\s*"([A-Za-z_][\w.-]*)"\s*:"#));
static EMITTER: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?:print_error!|eprintln!|bail!|throw\s+(?:new\s+)?(?:Error|Exception)|next\(err\))")
});

/// A path of a test file.
pub(super) fn is_test_path(path: &str) -> bool {
    TEST_PATH.is_match(path)
}


/// A word without one plural or tense ending.
pub(super) fn stem(word: &str) -> String {
    let length = word.chars().count();
    if length > 4
        && let Some(cut) = word.strip_suffix("ies")
    {
        return format!("{cut}y");
    }
    for suffix in ["ing", "ed", "s"] {
        if length >= suffix.len() + 3
            && word.ends_with(suffix)
            && let Some(cut) = word.strip_suffix(suffix)
        {
            return cut.to_owned();
        }
    }
    word.to_owned()
}

/// The question's words, stemmed, once each, without the function words.
fn prompt_words(prompt: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in subwords(prompt) {
        if token.chars().count() < 2 || STOP.contains(&token.as_str()) {
            continue;
        }
        let word = stem(&token);
        if !out.contains(&word) {
            out.push(word);
        }
    }
    out
}

/// The text between quotes of the question that is long enough to be a message.
fn quoted(prompt: &str) -> Vec<String> {
    QUOTED
        .captures_iter(prompt)
        .filter_map(|found| found.get(1).map(|text| text.as_str().to_owned()))
        .collect()
}

/// The text inside the string literals of a code line, joined.
fn string_part(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let quote = chars[i];
        if matches!(quote, '"' | '\'' | '`') {
            let mut j = i + 1;
            let mut closed = false;
            while j < chars.len() {
                let ch = chars[j];
                if ch == '\\' {
                    if chars.get(j + 1).is_some_and(|next| *next != '\n') {
                        j += 2;
                        continue;
                    }
                    break;
                }
                if ch == quote {
                    closed = true;
                    break;
                }
                j += 1;
            }
            if closed {
                parts.push(chars[i + 1..j].iter().collect());
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    parts.join(" ")
}

/// How many string delimiters a stretch of code holds: triple quotes in Python,
/// backticks in JS, unescaped double quotes (outside char literals) in the
/// rest; an odd count leaves a string open.
fn open_quotes(text: &str, path: &str) -> usize {
    if path.ends_with(".py") {
        let (mut count, mut at) = (0, 0);
        while at < text.len() {
            if text[at..].starts_with("\"\"\"") || text[at..].starts_with("'''") {
                count += 1;
                at += 3;
            } else {
                at += text[at..].chars().next().map_or(1, char::len_utf8);
            }
        }
        return count;
    }
    if JS_EXT.iter().any(|ext| path.ends_with(ext)) {
        return text.matches('`').count();
    }
    let mut previous: Option<char> = None;
    let mut count = 0;
    for ch in text.chars() {
        if ch == '"' && !matches!(previous, Some('\\' | '\'')) {
            count += 1;
        }
        previous = Some(ch);
    }
    count
}

/// The line sits inside a string that spans lines: Python triple quotes or a
/// JS template literal opened above.
fn in_multiline_string(lines: &[String], index: usize, path: &str) -> bool {
    let start = index.saturating_sub(60);
    open_quotes(&lines[start..index.min(lines.len())].join("\n"), path) % 2 == 1
}

/// A word of three letters or fewer must be a token (`no` is not in `not`),
/// a longer one starts a token.
fn carried(word: &str, tokens: &HashSet<String>) -> bool {
    if word.chars().count() <= 3 {
        tokens.contains(word)
    } else {
        tokens.iter().any(|token| token.starts_with(word))
    }
}

/// `(name, line index)` of the function a line is in; the `const x = (` form
/// only counts in JS and TS.
fn enclosing(lines: &[String], index: usize, js: bool) -> (Option<String>, usize) {
    if lines.is_empty() {
        return (None, 0);
    }
    for j in (0..=index.min(lines.len() - 1)).rev() {
        if let Some(found) = FN.captures(&lines[j]) {
            let name = found.get(1).or_else(|| js.then(|| found.get(2)).flatten());
            if let Some(name) = name {
                return (Some(name.as_str().to_owned()), j);
            }
        }
    }
    (None, 0)
}

/// The words of what emits a string that starts on line `first`: the call
/// opening on that line or the one above, and the name of the enclosing
/// function.
fn emitter(lines: &[String], first: usize, path: &str) -> HashSet<String> {
    let js = JS_EXT.iter().any(|ext| path.ends_with(ext));
    let (name, _) = enclosing(lines, first, js);
    let above = if first > 0 { lines[first - 1].as_str() } else { "" };
    let before_quote = lines[first]
        .split('"')
        .next()
        .and_then(|part| part.split('\'').next())
        .unwrap_or("");
    subwords(&[name.as_deref().unwrap_or(""), above, before_quote].join(" "))
        .into_iter()
        .collect()
}

/// `(first, last)` line index of the multi-line string that holds line
/// `index` (itself when it holds none), or `None` when the string runs past
/// eight lines on either side: a manual page or a doc block, not a message.
fn string_span(lines: &[String], index: usize, path: &str) -> Option<(usize, usize)> {
    const REACH: usize = 8;
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

/// `### path:lo-hi — label` and the numbered lines.
pub(super) fn show(path: &str, lines: &[String], lo: i64, hi: i64, label: &str) -> Vec<String> {
    let lo = lo.max(1);
    let hi = hi.min(lines.len() as i64);
    let mut out = vec![format!("### {path}:{lo}-{hi} — {label}")];
    for k in lo..=hi {
        out.push(format!("{k}| {}", head(&lines[(k - 1) as usize], 160)));
    }
    out
}

/// Test code: a tests path, or after a `mod tests`; an attribute alone marks nothing.
fn in_test(path: &str, lines: &[String], index: usize) -> bool {
    is_test_path(path)
        || lines[..index.min(lines.len())]
            .iter()
            .any(|line| TEST_MODULE.is_match(line))
}

/// The fragment sits inside a string literal of a code line: a quote before
/// it, and the line is not a comment.
fn in_string(text: &str, fragment: &str) -> bool {
    text.find(fragment).is_some_and(|at| {
        at > 0
            && text[..at].contains(['"', '\'', '`'])
            && !COMMENT.is_match(text)
    })
}

/// Hits of the longest leading run of `message` (at least three words) found
/// literally in non-test source; the first one. `None` outside when the clock
/// ran out.
fn locate(walk: &mut Walk, message: &str) -> Option<Option<(String, usize)>> {
    let trimmed = message.trim();
    // `re.split(r"(\s+)", text)`: the words and the whitespace between them.
    let mut pieces: Vec<&str> = Vec::new();
    let mut start = 0;
    let mut in_space = false;
    for (at, ch) in trimmed.char_indices() {
        if ch.is_whitespace() != in_space {
            pieces.push(&trimmed[start..at]);
            start = at;
            in_space = !in_space;
        }
    }
    pieces.push(&trimmed[start..]);
    let mut n = pieces.len();
    while n > MIN_WORDS + 1 {
        let joined: String = pieces[..n].concat();
        let fragment = joined
            .split('(')
            .next()
            .and_then(|part| part.split('{').next())
            .unwrap_or("")
            .trim_matches([' ', ':']);
        if fragment.split_whitespace().count() < MIN_WORDS {
            break;
        }
        let hits = walk.grep(SRC, 1, &|path, line| {
            line.contains(fragment) && !is_test_path(path) && in_string(line, fragment)
        })?;
        if let Some(hit) = hits.first() {
            return Some(Some((hit.path.clone(), hit.line)));
        }
        n -= 2;
    }
    Some(None)
}

/// The best candidate of `described`: its rank, path, line and text.
type Ranked = ((i64, f64, String, usize), String, usize, String);

/// The best emitter of `error_emission`: its rank, path, line and block.
type RankedEmitter = ((i64, usize, String, usize), String, usize, Vec<String>);

/// A message found by the words of the question: where, and the catalogue
/// entry it came from.
struct Described {
    path: String,
    line: usize,
    catalogue: Option<(String, usize)>,
}

/// `lt` of Python's tuple order for the key of a described message.
fn better(a: &(i64, f64, String, usize), b: &(i64, f64, String, usize)) -> bool {
    a.0.cmp(&b.0)
        .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .then_with(|| a.2.cmp(&b.2))
        .then_with(|| a.3.cmp(&b.3))
        .is_lt()
}

/// A message the prompt describes without quoting it: the string literal that
/// carries most of the prompt's words (two at least), each weighed by how few
/// source lines carry it. A literal in a message catalogue (JSON) is followed
/// to the code that names its key.
fn described(walk: &mut Walk, prompt: &str) -> Option<Option<Described>> {
    let words = prompt_words(prompt);
    let mut frequency: HashMap<&str, usize> = HashMap::new();
    for word in &words {
        frequency.insert(word, walk.count_ci(SRC_AND_JSON, &word.to_lowercase())?);
    }
    let mut present: Vec<&String> = words.iter().filter(|word| frequency[word.as_str()] > 0).collect();
    present.sort_by(|a, b| {
        frequency[a.as_str()]
            .cmp(&frequency[b.as_str()])
            .then_with(|| a.cmp(b))
    });
    if present.len() < MIN_SHARED {
        return Some(None);
    }
    let total: usize = frequency.values().sum::<usize>() + 1;
    let idf: HashMap<&str, f64> = present
        .iter()
        .map(|word| (word.as_str(), (total as f64 / frequency[word.as_str()] as f64).ln()))
        .collect();
    let mut best: Option<Ranked> = None;
    let mut seen: HashSet<(String, usize)> = HashSet::new();
    for word in present.iter().take(SEED_WORDS) {
        let hits = walk.grep_ci(SRC_AND_JSON, SEED_HITS, &word.to_lowercase())?;
        for hit in hits {
            let path = hit.path.as_str();
            if seen.contains(&(hit.path.clone(), hit.line))
                || is_test_path(path)
                || COMMENT.is_match(&hit.text)
            {
                continue;
            }
            seen.insert((hit.path.clone(), hit.line));
            let mut message = string_part(&hit.text);
            let mut context: HashSet<String> = HashSet::new();
            if !path.ends_with(".json") {
                let lines = walk.lines(path);
                let index = hit.line - 1;
                if index >= lines.len() || in_test(path, &lines, index) {
                    continue;
                }
                let mut span = string_span(&lines, index, path);
                if let Some((first, last)) = span
                    && first == last
                    && open_quotes(&hit.text, path) % 2 == 1
                {
                    span = if index + 1 < lines.len() {
                        string_span(&lines, index + 1, path).map(|(_, last)| (index, last))
                    } else {
                        None
                    };
                }
                let Some((first, last)) = span else {
                    continue;
                };
                if last > first {
                    message = lines[first..=last].join(" ");
                }
                context = emitter(&lines, first, path);
            }
            let words_in = subwords(&message);
            let own: HashSet<String> = words_in.iter().cloned().collect();
            if !present.iter().any(|word| carried(word, &own)) {
                continue;
            }
            let inside: HashSet<String> = own.union(&context).cloned().collect();
            let shared: Vec<&&String> = present.iter().filter(|word| carried(word, &inside)).collect();
            if shared.len() < MIN_SHARED {
                continue;
            }
            let weight: f64 = shared.iter().fold(0.0, |sum, word| sum + idf[word.as_str()]);
            let key = (
                -(shared.len() as i64),
                -weight / ((4 + words_in.len()) as f64).log2(),
                hit.path.clone(),
                hit.line,
            );
            if best.as_ref().is_none_or(|(held, ..)| better(&key, held)) {
                best = Some((key, hit.path.clone(), hit.line, hit.text.clone()));
            }
        }
    }
    let Some((_, path, line, text)) = best else {
        return Some(None);
    };
    if !path.ends_with(".json") {
        return Some(Some(Described { path, line, catalogue: None }));
    }
    let Some(name) = CATALOGUE_KEY.captures(&text).and_then(|found| found.get(1)) else {
        return Some(None);
    };
    let name = name.as_str();
    let hits = walk.grep(SRC, 1, &|hit_path, row| {
        !is_test_path(hit_path) && names_word(row, name)
    })?;
    Some(hits.first().map(|hit| Described {
        path: hit.path.clone(),
        line: hit.line,
        catalogue: Some((path, line)),
    }))
}

/// What the question asks about: where a quoted message sits, else a described one.
struct Origin {
    path: String,
    line: usize,
    how: &'static str,
    catalogue: Option<(String, usize)>,
}

fn origins(walk: &mut Walk, prompt: &str) -> Option<Option<Origin>> {
    for message in quoted(prompt).iter().take(3) {
        if let Some((path, line)) = locate(walk, message)? {
            return Some(Some(Origin {
                path,
                line,
                how: "quoted occurrence candidate; emission unverified",
                catalogue: None,
            }));
        }
    }
    Some(described(walk, prompt)?.map(|found| Origin {
        path: found.path,
        line: found.line,
        how: "described occurrence candidate; emission unverified",
        catalogue: found.catalogue,
    }))
}

/// `rg -w -F`: the text holds `name` with no word character on either side.
fn names_word(line: &str, name: &str) -> bool {
    let word = |ch: char| ch.is_alphanumeric() || ch == '_';
    line.match_indices(name).any(|(at, found)| {
        !line[..at].chars().next_back().is_some_and(word)
            && !line[at + found.len()..].chars().next().is_some_and(word)
    })
}

/// The pattern of a line that defines a function called `name` (`fn`, `def`, `function`).
fn definition(name: &str) -> Option<Regex> {
    Regex::new(&format!(r"\b(fn|def|function)\s+{}\b", regex::escape(name))).ok()
}

/// Bounded lexical call candidates; directory proximity does not resolve
/// receiver or binding identity.
#[allow(clippy::too_many_arguments)]
fn callers(
    walk: &mut Walk,
    name: &str,
    seen: &mut HashSet<(String, String, String)>,
    depth: usize,
    out: &mut Vec<String>,
    home: &str,
    defined_in: &str,
) -> Option<()> {
    if depth == 0 || out.len() > MAX_LINES {
        return Some(());
    }
    let defined = definition(name)?;
    let definitions = walk.files_matching(SRC, 2, &|line| defined.is_match(line))?;
    let near: Option<String> = (definitions.len() > 1).then(|| {
        defined_in
            .rsplit_once('/')
            .map_or(String::new(), |(directory, _)| format!("{directory}/"))
    });
    let call = Regex::new(&format!(r"\b{}\s*\(", regex::escape(name))).ok()?;
    let mut frontier: Vec<(String, String)> = Vec::new();
    for hit in walk.grep(SRC, 40, &|_, line| call.is_match(line))? {
        if defined.is_match(&hit.text) {
            continue;
        }
        let lines = walk.lines(&hit.path);
        let index = hit.line - 1;
        if in_test(&hit.path, &lines, index) {
            continue;
        }
        if near.as_ref().is_some_and(|near| !hit.path.starts_with(near.as_str())) {
            continue;
        }
        if !home.is_empty() && depth < LEVELS && !hit.path.starts_with(&format!("{home}/")) {
            continue;
        }
        let js = JS_EXT.iter().any(|ext| hit.path.ends_with(ext));
        let (caller, start) = enclosing(&lines, index, js);
        let Some(caller) = caller else {
            continue;
        };
        if !seen.insert((hit.path.clone(), caller.clone(), name.to_owned())) {
            continue;
        }
        let lo = (start as i64 + 1).max(index as i64 + 1 - CALLER_LINES);
        if lo > start as i64 + 4 {
            out.extend(show(
                &hit.path,
                &lines,
                start as i64 + 1,
                start as i64 + 3,
                &format!("{caller}: lexical caller candidate (scope/binding unresolved)"),
            ));
        }
        let independent = if depth < LEVELS {
            "; independent context, no composed path"
        } else {
            ""
        };
        out.extend(show(
            &hit.path,
            &lines,
            lo,
            index as i64 + 4,
            &format!(
                "{caller} → {name}: lexical call candidate; receiver/binding unresolved; scope unverified{independent}"
            ),
        ));
        if depth > 1 && near.is_none() {
            frontier.push((caller, hit.path.clone()));
        }
    }
    // Visit every direct match before indirect context can claim the shared seen slot.
    for (caller, path) in frontier {
        callers(walk, &caller, seen, depth - 1, out, home, &path)?;
    }
    Some(())
}

/// A small deterministic set of tracked regular source files, excluding
/// symlink paths.
fn tracked_source_paths(ctx: &Ctx) -> Vec<String> {
    let mut listed: Vec<&String> = ctx
        .files()
        .unwrap_or_default()
        .iter()
        .filter(|path| SRC.iter().any(|ext| path.ends_with(ext)))
        .collect();
    if listed.iter().map(|path| path.len() + 1).sum::<usize>() > MAX_DIAGNOSTIC_LISTING_BYTES {
        return Vec::new();
    }
    listed.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let (mut paths, mut used) = (Vec::new(), 0);
    for path in listed {
        let relative = std::path::Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            continue;
        }
        let mut current = ctx.root().to_path_buf();
        let mut safe = true;
        for part in relative.components() {
            current.push(part);
            match current.symlink_metadata() {
                Ok(info) if info.is_symlink() => {
                    safe = false;
                    break;
                }
                Ok(_) => {}
                Err(_) => {
                    safe = false;
                    break;
                }
            }
        }
        let Some(info) = safe.then(|| current.symlink_metadata().ok()).flatten() else {
            continue;
        };
        if !info.is_file() || info.len() > MAX_DIAGNOSTIC_FILE_BYTES {
            continue;
        }
        if used + path.len() > MAX_DIAGNOSTIC_PATH_BYTES || paths.len() >= MAX_DIAGNOSTIC_FILES {
            break;
        }
        paths.push(path.clone());
        used += path.len();
    }
    paths
}

/// A bounded diagnostic emitter whose nearby tracked source carries two
/// salient prompt terms: path, line and the block to quote.
fn error_emission(
    ctx: &Ctx,
    walk: &mut Walk,
    prompt: &str,
    paths: &[String],
) -> Option<(String, usize, Vec<String>)> {
    let salient: HashSet<String> = prompt_words(prompt)
        .into_iter()
        .filter(|word| !GENERIC.contains(&word.as_str()))
        .collect();
    if salient.len() < 2 {
        return None;
    }
    let mut best: Option<RankedEmitter> = None;
    for path in paths {
        if ctx.timed_out() {
            return None;
        }
        if is_test_path(path) {
            continue;
        }
        let Some(text) = ctx.read(path, MAX_SEARCHED_FILE_BYTES) else {
            continue;
        };
        let rows: Vec<&str> = text.split('\n').collect();
        let hits = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| EMITTER.is_match(row.strip_suffix('\r').unwrap_or(row)))
            .map(|(i, _)| i + 1)
            .take(MAX_HITS_PER_FILE);
        for line in hits {
            let lines = walk.lines(path);
            if lines.is_empty() || line > lines.len() {
                continue;
            }
            let js = JS_EXT.iter().any(|ext| path.ends_with(ext));
            let (_, function_start) = enclosing(&lines, line - 1, js);
            let lo_context = function_start.max(line.saturating_sub(5));
            let mut hi_context = lines.len().min(line + 4);
            let mut function_end = lines.len();
            let stop = (line..hi_context).find(|&following| {
                FN.captures(&lines[following])
                    .is_some_and(|found| found.get(1).is_some() || js)
            });
            if let Some(following) = stop {
                hi_context = following;
                function_end = following;
            }
            let near: HashSet<String> = subwords(&lines[lo_context..hi_context.max(lo_context)].join(" "))
                .iter()
                .map(|token| stem(token))
                .collect();
            let shared = salient.intersection(&near).count();
            if shared < 2 {
                continue;
            }
            let statement = &lines[line - 1];
            let priority = if statement.contains("print_error!") {
                0
            } else if statement.contains("eprintln!") {
                1
            } else if statement.contains("throw") {
                2
            } else if statement.contains("bail!") {
                3
            } else {
                4
            };
            let rank = (-(shared as i64), priority, path.clone(), line);
            if best.as_ref().is_none_or(|(held, ..)| rank < *held) {
                let block = show(
                    path,
                    &lines,
                    (function_start as i64 + 1).max(line as i64 - 6),
                    (function_end as i64).min(line as i64 + 6),
                    "error-emission candidate; matching terms and source condition, exact runtime cause unproven",
                );
                best = Some((rank, path.clone(), line, block));
            }
        }
    }
    best.map(|(_, path, line, block)| (path, line, block))
}

/// The lines that follow a quoted `how` for a file the question points into.
fn answer(ctx: &Ctx) -> Option<String> {
    let prompt = ctx.prompt;
    let mut walk = Walk::new(ctx);
    let paths = tracked_source_paths(ctx);
    let boundary = js::boundary(ctx, &mut walk, prompt, &paths);
    let mut out: Vec<String> = boundary.clone();
    let meaningful: HashSet<String> = prompt_words(prompt)
        .into_iter()
        .filter(|word| !["error", "exception", "message", "text", "type", "warning"].contains(&word.as_str()))
        .collect();
    if let Some(handler) = python::handlers(&mut walk, &meaningful)? {
        let lines = walk.lines(&handler.path);
        out.extend(show(
            &handler.path,
            &lines,
            handler.first as i64,
            handler.last as i64,
            "conversion/rethrow candidate: matching caught type; runtime message origin unresolved",
        ));
        for (first, last) in &handler.imports {
            out.extend(show(
                &handler.path,
                &lines,
                *first as i64,
                *last as i64,
                "dependency boundary candidate: matching import name; binding unresolved; external emission not inspected",
            ));
        }
        for (first, last) in &handler.raises {
            out.extend(show(
                &handler.path,
                &lines,
                *first as i64,
                *last as i64,
                "local raise/rethrow syntax in this handler; message identity unverified",
            ));
        }
        out.push("Call context unresolved: method names alone do not establish receiver identity.".to_owned());
    }
    let diagnostic = error_emission(ctx, &mut walk, prompt, &paths);
    if let Some((path, _, block)) = &diagnostic
        && !(!boundary.is_empty() && EXAMPLES.is_match(path))
    {
        out.extend(block.iter().cloned());
    }
    let mut origin = origins(&mut walk, prompt)?;
    if !boundary.is_empty() && origin.as_ref().is_some_and(|found| EXAMPLES.is_match(&found.path)) {
        origin = None;
    }
    if let (Some((path, _, _)), Some(found)) = (&diagnostic, &origin)
        && *path == found.path
        && !found.how.starts_with("quoted occurrence")
    {
        origin = None;
    }
    if let Some(origin) = origin {
        quote_origin(&mut walk, &origin, &mut out)?;
    }
    let text = out.join("\n");
    if text.is_empty() {
        return Some(String::new());
    }
    let kept = if text.chars().count() <= MAX_BYTES {
        text
    } else {
        let cut = text.char_indices().nth(MAX_BYTES).map_or(text.len(), |(at, _)| at);
        let prefix = &text[..cut];
        prefix.rsplit_once('\n').map_or(prefix, |(before, _)| before).to_owned()
    };
    Some(format!("{kept}\n"))
}

/// The origin block and its call context.
fn quote_origin(walk: &mut Walk, origin: &Origin, out: &mut Vec<String>) -> Option<()> {
    let path = origin.path.as_str();
    let lines: Rc<Vec<String>> = walk.lines(path);
    let (line, index) = (origin.line, origin.line - 1);
    if index >= lines.len() {
        return Some(());
    }
    let js = JS_EXT.iter().any(|ext| path.ends_with(ext));
    let (mut name, mut start) = enclosing(&lines, index, js);
    let mut how = origin.how.to_owned();
    if path.ends_with(".py") {
        (how, name, start) = python::role(&lines, line);
    }
    let scope = match &name {
        Some(name) if path.ends_with(".py") => format!(" in {name}"),
        Some(name) => format!(" near {name} (scope unresolved)"),
        None => String::new(),
    };
    out.push(format!("### error origin — {how} at {path}:{line}{scope}"));
    if let Some((catalogue, catalogue_line)) = &origin.catalogue {
        let rows = walk.lines(catalogue);
        out.extend(show(
            catalogue,
            &rows,
            *catalogue_line as i64,
            *catalogue_line as i64,
            "message definition in catalogue; emission unverified",
        ));
    }
    let lo = if name.is_some() {
        (start as i64 + 1).max(line as i64 - 12)
    } else {
        line as i64 - 6
    };
    out.extend(show(
        path,
        &lines,
        lo,
        line as i64 + 6,
        "candidate source occurrence; inspect the evidence category above",
    ));
    let Some(name) = name else {
        return Some(());
    };
    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    seen.insert((path.to_owned(), name.clone(), name.clone()));
    let parts: Vec<&str> = path.split('/').collect();
    let home = parts[..parts.len() - 1].iter().take(2).copied().collect::<Vec<_>>().join("/");
    callers(walk, &name, &mut seen, LEVELS, out, &home, path)?;
    let call = Regex::new(&format!(r"\b{}\s*\(", regex::escape(&name))).ok()?;
    for hit in walk.grep(SRC, usize::MAX, &|_, row| call.is_match(row))? {
        let rows = walk.lines(&hit.path);
        if in_test(&hit.path, &rows, hit.line - 1) {
            let (test, test_start) = enclosing(&rows, hit.line - 1, false);
            out.extend(show(
                &hit.path,
                &rows,
                test_start as i64 + 1,
                test_start as i64 + 24,
                &format!(
                    "test {}: name match for {name}; binding unresolved",
                    test.as_deref().unwrap_or("None")
                ),
            ));
            break;
        }
    }
    Some(())
}

fn run(ctx: &Ctx) -> String {
    answer(ctx).unwrap_or_default()
}

#[cfg(test)]
mod tests;
