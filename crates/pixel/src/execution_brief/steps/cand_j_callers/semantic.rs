// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The source-level caller shapes `cand-j-callers` recovers when the graph
//! resolves no name of the question: the callers of an ID factory (through
//! barrels and aliases), the methods of one receiver that call another
//! (`req.get`), and the request-context `push` of a Python framework.
//!
//! Ported from `semantic_caller_blocks` and `tracked_source` of
//! `probes/cand-j-callers/probe`. The script reads Python's syntax tree for
//! one shape (a module `__getattr__` that returns a class for an alias);
//! this step reads the same shape from the lines of the function, which gives
//! the same answer for the code the shape is written for and abstains on
//! anything stranger.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;

use super::js::{
    self, Factory, assigned_functions, code_mask, export_name_candidates, export_name_edges,
    has_script_extension, imported_local_names, proven_getter_functions,
};
use super::super::text::{indent as indent_of, re};
use super::{Ctx, TESTPATH, head, lines_of};
use crate::execution_brief::steps::text::SRC;

/// Bytes of the semantic blocks.
const SEMANTIC_CAP_BYTES: usize = 6_500;
/// Source files read.
const MAX_SOURCE_FILES: usize = 1_200;
/// Bytes of source read.
const MAX_SOURCE_BYTES: usize = 12 * 1024 * 1024;
/// Wall time of the scan.
const MAX_SOURCE_SCAN: Duration = Duration::from_millis(1_000);
/// Bytes of one file.
const MAX_FILE_BYTES: u64 = 300_000;


static WORD: LazyLock<Regex> = LazyLock::new(|| re(r"[a-z]+"));
static CAMEL: LazyLock<Regex> = LazyLock::new(|| re(r"([a-z0-9])([A-Z])"));
static CONFIG_FILE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(^|/)(?:tsconfig|jsconfig)[^/]*\.json$"));
static INTENT: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\b(who|which|what|where|modules?|parts?|code|callers?|calls?|invoke|uses?)\b")
});
static ASKS: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b(which|who|what|show|locate)\b"));
static ABOUT_CALLERS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)\b(call|calls|calling|callers?|modules?|parts?|code|consumers?|entry points?|invoke|uses?)\b",
    )
});
static MEMBER: LazyLock<Regex> =
    LazyLock::new(|| re(r"\b([A-Za-z_$][\w$]*)\.([A-Za-z_$][\w$]*)\b"));
static MEMBER_PLAIN: LazyLock<Regex> =
    LazyLock::new(|| re(r"\b[A-Za-z_$][\w$]*\.[A-Za-z_$][\w$]*\b"));
static LIB: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\blib\b"));
static CONTEXT_ALIAS: LazyLock<Regex> =
    LazyLock::new(|| re(r"\b([A-Z][A-Za-z0-9]*Context)\s*\.\s*push\b"));
static ID_NAME: LazyLock<Regex> = LazyLock::new(|| {
    re(r"\b([A-Za-z_$][\w$]*(?:Id|ID|Uuid|UUID|Identifier))\s*(?:=|\()")
});
static GENERATOR: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)\b(?:nanoid|uuid4?|randomUUID)\b|(?:crypto|Math)\.random\s*\(")
});
static CLASS_LINE: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*class\s+([A-Za-z_]\w*)\b"));
static CLASS_AT_ROOT: LazyLock<Regex> = LazyLock::new(|| re(r"^class\s+[A-Za-z_]\w*"));
static DEF_PUSH: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*def\s+push\s*\("));
static DEF_ANY: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*def\s+([A-Za-z_]\w*)\s*\("));
static REQUEST_CONTEXT: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)request\s+context"));
static MERGED: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)merged with.{0,40}RequestContext|request context"));
static SELF_PUSH: LazyLock<Regex> = LazyLock::new(|| re(r"\bself\s*\.\s*push\s*\("));
static RETURNS_CONTEXT: LazyLock<Regex> = LazyLock::new(|| re(r"->\s*\w*Context"));
static RETURN_CONTEXT: LazyLock<Regex> = LazyLock::new(|| re(r"return\s+\w*Context\."));
static ALIAS_ASSIGN: LazyLock<Regex> =
    LazyLock::new(|| re(r"\b([A-Za-z_]\w*)\s*=\s*self\.request_context\s*\("));
static GETATTR: LazyLock<Regex> =
    LazyLock::new(|| re(r"^(?:async\s+)?def\s+__getattr__\s*\("));
static IF_NAME: LazyLock<Regex> = LazyLock::new(|| {
    re(r#"^(\s*)(?:el)?if\s+name\s*==\s*(?:'([^']*)'|"([^"]*)")\s*:"#)
});
static RETURN_NAME: LazyLock<Regex> =
    LazyLock::new(|| re(r"^\s*return\s+([A-Za-z_]\w*)\s*(?:#.*)?$"));

/// Words that say nothing about the topic.
const GENERIC: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "can", "change", "for", "from", "how", "i",
    "if", "in", "is", "it", "me", "of", "on", "or", "the", "this", "to", "what", "which", "who",
    "call", "calls", "calling", "caller", "callers", "function", "functions", "module", "modules",
    "code", "parts", "show", "locate", "where", "fresh", "new", "create", "creates", "created",
    "generate", "generated", "generator",
];
/// Words of an ID question that are not its topic.
const ID_FRAME: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "can", "change", "for", "from", "how", "i",
    "if", "in", "is", "it", "me", "of", "on", "or", "the", "this", "to", "what", "which", "who",
    "id", "ids", "identifier", "identifiers", "uuid", "generate", "generated", "generator",
    "create", "created", "random", "new", "module", "modules", "call", "calls",
];

/// The tracked source the fallbacks read, in the order they read it.
pub(super) struct Sources {
    /// Paths in reading order.
    pub(super) order: Vec<String>,
    /// Path -> its lines.
    pub(super) lines: HashMap<String, Vec<String>>,
}

impl Sources {
    fn each(&self) -> impl Iterator<Item = (&String, &Vec<String>)> {
        self.order.iter().filter_map(|path| self.lines.get(path).map(|lines| (path, lines)))
    }
}

/// Words of `text` made of lowercase letters, with `aB` split to `a B`.
fn lower_words(text: &str) -> BTreeSet<String> {
    let split = CAMEL.replace_all(text, "$1 $2").to_lowercase();
    WORD.find_iter(&split).map(|word| word.as_str().to_owned()).collect()
}

/// One bounded snapshot of the tracked source, the paths that share words
/// with the question first; `partial` when a cap or the clock cut it short.
pub(super) fn tracked_source(ctx: &Ctx, prompt: &str) -> (Sources, bool) {
    let limits = Limits { files: MAX_SOURCE_FILES, bytes: MAX_SOURCE_BYTES, scan: MAX_SOURCE_SCAN };
    tracked_source_within(ctx, prompt, &limits)
}

/// The caps of one snapshot; the tests lower them.
pub(super) struct Limits {
    pub(super) files: usize,
    pub(super) bytes: usize,
    pub(super) scan: Duration,
}

pub(super) fn tracked_source_within(ctx: &Ctx, prompt: &str, limits: &Limits) -> (Sources, bool) {
    let deadline = Instant::now() + limits.scan;
    let timed_out = || ctx.timed_out() || Instant::now() >= deadline;
    let mut partial = false;
    let Some(files) = ctx.files() else {
        return (Sources { order: Vec::new(), lines: HashMap::new() }, true);
    };
    let mut paths: Vec<&String> = files
        .iter()
        .filter(|path| SRC.is_match(path) || CONFIG_FILE.is_match(path))
        .collect();
    let mut terms = lower_words(prompt);
    for word in GENERIC {
        terms.remove(*word);
    }
    if !terms.is_empty() {
        paths.sort_by_key(|path| {
            let shared = lower_words(path).intersection(&terms).count();
            (std::cmp::Reverse(shared), (*path).clone())
        });
    }
    if paths.len() > limits.files {
        paths.truncate(limits.files);
        partial = true;
    }
    let (mut order, mut lines, mut total) = (Vec::new(), HashMap::new(), 0_usize);
    let wanted = paths.len();
    for path in paths {
        if timed_out() {
            partial = true;
            break;
        }
        let rows = lines_of(ctx, path, MAX_FILE_BYTES);
        let size: usize = rows.iter().map(|row| row.len() + 1).sum();
        if total + size > limits.bytes {
            partial = true;
            break;
        }
        order.push(path.clone());
        lines.insert(path.clone(), rows);
        total += size;
    }
    if wanted > order.len() {
        partial = true;
    }
    (Sources { order, lines }, partial)
}

/// Whether the question asks for callers in a shape the fallbacks know.
pub(super) fn wants_semantic_callers(prompt: &str) -> bool {
    let mut words = lower_words_plain(prompt);
    if words.contains("requestcontext") {
        words.extend(["request".to_owned(), "context".to_owned()]);
    }
    if words.contains("pushed") {
        words.insert("push".to_owned());
    }
    let caller_intent = ASKS.is_match(prompt) && ABOUT_CALLERS.is_match(prompt);
    caller_intent
        && (MEMBER_PLAIN.is_match(prompt) || id_question(&words) || has_all(&words, &["request", "context", "push"]))
}

fn has_all(words: &BTreeSet<String>, wanted: &[&str]) -> bool {
    wanted.iter().all(|word| words.contains(*word))
}

fn has_any(words: &BTreeSet<String>, wanted: &[&str]) -> bool {
    wanted.iter().any(|word| words.contains(*word))
}

/// `set(re.findall(r'[a-z]+', text.lower()))`.
fn lower_words_plain(text: &str) -> BTreeSet<String> {
    WORD.find_iter(&text.to_lowercase()).map(|word| word.as_str().to_owned()).collect()
}

fn id_question(words: &BTreeSet<String>) -> bool {
    has_any(words, &["id", "ids", "identifier", "identifiers", "uuid"])
        && has_any(
            words,
            &["generate", "generated", "generator", "create", "created", "creates", "random", "new"],
        )
}

/// What `emit` of the script keeps: the blocks, and which ranges it has shown.
struct Blocks {
    blocks: Vec<String>,
    seen: HashSet<(String, i64, i64)>,
    partial: bool,
}

impl Blocks {
    fn emit(&mut self, path: &str, lines: &[String], lo: i64, hi: i64, label: &str) {
        let len = i64::try_from(lines.len()).unwrap_or(i64::MAX);
        let (lo, hi) = (lo.max(1), hi.min(len));
        let key = (path.to_owned(), lo, hi);
        if lines.is_empty() || lo > hi || self.seen.contains(&key) {
            return;
        }
        self.seen.insert(key);
        let suffix = if self.partial {
            " (bounded source scan; additional candidates may be omitted)"
        } else {
            ""
        };
        let mut block = format!("### {path}:{lo}-{hi} — {label}{suffix}");
        for n in lo..=hi {
            let line = usize::try_from(n - 1).ok().and_then(|at| lines.get(at)).map_or("", String::as_str);
            block.push_str(&format!("\n{n}| {}", head(line, 200)));
        }
        self.blocks.push(block);
    }
}


/// `str.count('\n', 0, offset) + 1`.
fn line_at(source: &str, offset: usize) -> i64 {
    i64::try_from(source.as_bytes()[..offset].iter().filter(|byte| **byte == b'\n').count()).unwrap_or(0) + 1
}

/// The camel-case words of an identifier, lowercased: the script's
/// `[A-Z]+(?=[A-Z][a-z]|\b)|[A-Z]?[a-z]+|[0-9]+`, which the regex crate
/// cannot spell for want of a lookahead.
fn name_parts(name: &str) -> BTreeSet<String> {
    let chars: Vec<char> = name.chars().collect();
    let count = chars.len();
    let upper = |at: usize| chars.get(at).is_some_and(char::is_ascii_uppercase);
    let lower = |at: usize| chars.get(at).is_some_and(char::is_ascii_lowercase);
    let word = |at: usize| chars.get(at).is_some_and(|ch| ch.is_alphanumeric() || *ch == '_');
    let mut parts = BTreeSet::new();
    let mut at = 0;
    while at < count {
        if upper(at) {
            let mut end = at;
            while upper(end) {
                end += 1;
            }
            // A run of capitals ends at a word boundary, or before the capital
            // that starts a capitalised word.
            let cut = if !word(end) {
                Some(end)
            } else if lower(end) && end - at > 1 {
                Some(end - 1)
            } else {
                None
            };
            if let Some(cut) = cut {
                parts.insert(chars[at..cut].iter().collect::<String>().to_lowercase());
                at = cut;
                continue;
            }
            if lower(at + 1) {
                let mut stop = at + 1;
                while lower(stop) {
                    stop += 1;
                }
                parts.insert(chars[at..stop].iter().collect::<String>().to_lowercase());
                at = stop;
                continue;
            }
            at += 1;
        } else if lower(at) {
            let mut stop = at;
            while lower(stop) {
                stop += 1;
            }
            parts.insert(chars[at..stop].iter().collect::<String>());
            at = stop;
        } else if chars[at].is_ascii_digit() {
            let mut stop = at;
            while chars.get(stop).is_some_and(char::is_ascii_digit) {
                stop += 1;
            }
            parts.insert(chars[at..stop].iter().collect::<String>());
            at = stop;
        } else {
            at += 1;
        }
    }
    parts
}

/// Whether `candidates` occur in `text` as a whole name: no `[\w$]` on
/// either side (`(?<![\w$])(?:a|b)(?![\w$])`).
fn names_any(text: &str, candidates: &BTreeSet<String>) -> bool {
    let name_char = |ch: char| ch.is_alphanumeric() || ch == '_' || ch == '$';
    candidates.iter().any(|candidate| {
        text.match_indices(candidate.as_str()).any(|(at, found)| {
            !text[..at].chars().next_back().is_some_and(name_char)
                && !text[at + found.len()..].chars().next().is_some_and(name_char)
        })
    })
}

/// The Python lines of the body of a function or an `if`: from the line
/// after `at` while a line is blank or indented deeper than `base`.
fn block_end(lines: &[String], at: usize, base: usize) -> usize {
    let mut end = at + 1;
    while end < lines.len()
        && (lines[end].trim().is_empty() || indent_of(&lines[end]) > base)
    {
        end += 1;
    }
    end
}

/// The caller blocks the fallbacks recover.
pub(super) fn semantic_caller_blocks(ctx: &Ctx, sources: &Sources, partial: bool) -> String {
    let prompt = ctx.prompt;
    let intent = INTENT.is_match(prompt);
    let mut words = lower_words_plain(prompt);
    if words.contains("requestcontext") {
        words.extend(["request".to_owned(), "context".to_owned()]);
    }
    if words.contains("pushed") {
        words.insert("push".to_owned());
    }
    let requested_aliases: BTreeSet<String> =
        CONTEXT_ALIAS.captures_iter(prompt).map(|caps| caps[1].to_owned()).collect();
    let mut out = Blocks { blocks: Vec::new(), seen: HashSet::new(), partial };

    if intent && id_question(&words) {
        id_factories(ctx, sources, &words, &mut out);
    }
    if intent && let Some(member) = MEMBER.captures(prompt) {
        members(sources, &member[1], &member[2], prompt, &mut out);
    }
    if intent && has_all(&words, &["request", "context", "push"]) {
        request_context(sources, &requested_aliases, &mut out);
    }

    let (mut kept, mut size) = (Vec::new(), 0);
    for block in out.blocks {
        if size + block.len() + 1 > SEMANTIC_CAP_BYTES {
            break;
        }
        size += block.len() + 1;
        kept.push(block);
    }
    kept.join("\n")
}

/// An ID factory and the calls of it, scored by how close they are to the question.
struct Ranked {
    score: i64,
    name: String,
    path: String,
    lo: usize,
    hi: usize,
    /// Score, path and 0-based line of each call.
    refs: Vec<(i64, String, usize)>,
}

/// The declared, ID-named, randomly generated factories and the calls of them.
fn id_factories(ctx: &Ctx, sources: &Sources, words: &BTreeSet<String>, out: &mut Blocks) {
    let mut declarations: HashMap<String, usize> = HashMap::new();
    let mut factories: Vec<(String, String, usize, usize)> = Vec::new();
    let mut declared: HashMap<String, Regex> = HashMap::new();
    for (path, lines) in sources.each() {
        if TESTPATH.is_match(path) {
            continue;
        }
        if ctx.timed_out() {
            return;
        }
        for (i, line) in lines.iter().enumerate() {
            let Some(found) = ID_NAME.captures(line) else {
                continue;
            };
            let name = found[1].to_owned();
            let keyword = declared.entry(name.clone()).or_insert_with(|| {
                re(&format!(r"\b(?:const|let|var|function|fn|def)\s+{}\b", regex::escape(&name)))
            });
            if !keyword.is_match(line) {
                continue;
            }
            if !name_parts(&name).iter().any(|part| ["id", "identifier", "uuid"].contains(&part.as_str())) {
                continue;
            }
            *declarations.entry(name.clone()).or_default() += 1;
            let body = if !line.trim_end().ends_with('{') && line.contains("=>") {
                line.clone()
            } else {
                let (mut rows, mut depth, mut started) = (Vec::new(), 0_i64, false);
                for row in &lines[i..lines.len().min(i + 40)] {
                    rows.push(row.as_str());
                    depth += i64::try_from(row.matches('{').count()).unwrap_or(0)
                        - i64::try_from(row.matches('}').count()).unwrap_or(0);
                    started |= row.contains('{');
                    if started && depth <= 0 {
                        break;
                    }
                }
                rows.join("\n")
            };
            if !GENERATOR.is_match(&body) {
                continue;
            }
            factories.push((name, path.clone(), i + 1, lines.len().min(i + 9)));
        }
    }
    let mut topic: BTreeSet<String> = words.clone();
    for word in ID_FRAME {
        topic.remove(*word);
    }
    let joined: BTreeMap<String, String> =
        sources.each().map(|(path, lines)| (path.clone(), lines.join("\n"))).collect();
    let mut masks: HashMap<String, String> = HashMap::new();
    let mut cache = js::ExportCache::new();
    let name_edges = export_name_edges(&joined, &sources.order);

    // The first factory of a name, when that name is declared exactly once.
    let mut chosen: Vec<(String, String, usize, usize)> = Vec::new();
    for (name, path, lo, hi) in factories {
        if !chosen.iter().any(|(known, ..)| *known == name) && declarations.get(&name) == Some(&1) {
            chosen.push((name, path, lo, hi));
        }
    }
    let mut ranked: Vec<Ranked> = Vec::new();
    for (name, path, lo, hi) in chosen {
        let candidates = export_name_candidates(&name, &name_edges);
        let mut refs: Vec<(i64, String, usize)> = Vec::new();
        for (caller_path, lines) in sources.each() {
            if ctx.timed_out() {
                return;
            }
            if TESTPATH.is_match(caller_path) {
                continue;
            }
            let source = &joined[caller_path];
            if !names_any(source, &candidates) {
                continue;
            }
            let mask = masks.entry(caller_path.clone()).or_insert_with(|| code_mask(source)).clone();
            let masked_lines: Vec<String> = if has_script_extension(caller_path) {
                crate::execution_brief::steps::text::splitlines_owned(&mask)
            } else {
                lines.clone()
            };
            let locals: BTreeSet<String> = if *caller_path == path {
                BTreeSet::from([name.clone()])
            } else {
                let factory = Factory { path: &path, name: &name };
                imported_local_names(caller_path, source, &mask, &factory, &joined, &mut cache)
            };
            for local in locals {
                let escaped = regex::escape(&local);
                let call = re(&format!(r"\b{escaped}\s*\("));
                let strong = re(&format!(r"\bid\s*:\s*(?:[^,;]*?\|\|\s*)?{escaped}\s*\(|\.id\s*=\s*{escaped}\s*\("));
                for (i, line) in masked_lines.iter().enumerate() {
                    if *caller_path == path && (lo..=hi).contains(&(i + 1)) {
                        continue;
                    }
                    for _ in call.find_iter(line) {
                        let context = lower_words_plain(&format!("{caller_path} {line}"));
                        let shared = i64::try_from(topic.intersection(&context).count()).unwrap_or(0);
                        refs.push((if strong.is_match(line) { 4 } else { 0 } + shared, caller_path.clone(), i));
                    }
                }
            }
        }
        let best = refs.iter().map(|row| row.0).max().unwrap_or(0);
        ranked.push(Ranked { score: best, name, path, lo, hi, refs });
    }
    ranked.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)).then_with(|| a.lo.cmp(&b.lo)));
    for Ranked { score, name, path, lo, hi, mut refs } in ranked.into_iter().take(2) {
        if !topic.is_empty() && score == 0 {
            continue;
        }
        let (lo, hi) = (i64::try_from(lo).unwrap_or(0), i64::try_from(hi).unwrap_or(0));
        out.emit(&path, &sources.lines[&path], lo, hi, &format!("ID factory {name}"));
        refs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2)));
        for (_, caller_path, i) in refs.into_iter().take(6) {
            let lines = &sources.lines[&caller_path];
            let first = i64::try_from(i).unwrap_or(0).max(1);
            let last = i64::try_from(lines.len().min(i + 4)).unwrap_or(0);
            out.emit(&caller_path, lines, first, last, &format!("calls {name}"));
        }
    }
}

/// `receiver.method` of the question: its definition and the methods of the
/// same receiver that call `this.method`.
fn members(sources: &Sources, receiver: &str, method: &str, prompt: &str, out: &mut Blocks) {
    let only_lib = LIB.is_match(prompt);
    let uses = re(&format!(r"\bthis\s*\.\s*{}\s*\(", regex::escape(method)));
    for (path, lines) in sources.each() {
        if TESTPATH.is_match(path) || (only_lib && !path.starts_with("lib/")) {
            continue;
        }
        let source = lines.join("\n");
        let definitions = assigned_functions(&source, receiver);
        let Some(first) = definitions.iter().find(|row| row.name == method) else {
            continue;
        };
        let window = |start: usize, end: usize| {
            let begin = line_at(&source, start);
            (begin, line_at(&source, end).min(begin + 23))
        };
        let (lo, hi) = window(first.start, first.end);
        out.emit(path, lines, lo, hi, &format!("{receiver}.{method} definition on the same receiver"));
        for row in &definitions {
            if row.name != method && uses.is_match(&row.body) {
                let (lo, hi) = window(row.start, row.end);
                out.emit(path, lines, lo, hi, &format!("{receiver}.{} calls this.{method} on the same receiver", row.name));
            }
        }
        for (child, start, end) in proven_getter_functions(&source, receiver, method) {
            let (lo, hi) = window(start, end);
            out.emit(path, lines, lo, hi, &format!("{receiver}.{child} getter calls this.{method} on the proven receiver"));
        }
    }
}

/// The `push` of a request-context class, its callers in the class, the
/// factory that returns it and the alias that resolves to it.
fn request_context(sources: &Sources, requested: &BTreeSet<String>, out: &mut Blocks) {
    for (path, lines) in sources.each() {
        if !path.ends_with(".py") || TESTPATH.is_match(path) {
            continue;
        }
        let len = lines.len();
        let to_i = |value: usize| i64::try_from(value).unwrap_or(i64::MAX);
        let mut classes: Vec<(usize, String, usize)> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let indent = indent_of(line);
            if let Some(found) = CLASS_LINE.captures(line) {
                while classes.last().is_some_and(|row| row.0 >= indent) {
                    classes.pop();
                }
                classes.push((indent, found[1].to_owned(), i));
                continue;
            }
            if !DEF_PUSH.is_match(line) {
                continue;
            }
            let Some((_, owner, start)) = classes.iter().rev().find(|row| row.0 < indent).cloned() else {
                continue;
            };
            let class_end = (start + 1..len).find(|j| CLASS_AT_ROOT.is_match(&lines[*j])).unwrap_or(len);
            let class_doc = lines[start..class_end.min(start + 45)].join("\n");
            if !(REQUEST_CONTEXT.is_match(&class_doc) || owner.to_lowercase().contains("context")) {
                continue;
            }
            if MERGED.is_match(&class_doc) && owner != "_AppCtxGlobals" {
                out.emit(path, lines, to_i(start + 1), to_i(class_end.min(start + 34)), &format!("{owner} class documents request-context identity"));
            }
            let mut sig = i;
            while sig + 1 < len && !lines[sig].trim_end().ends_with(':') {
                sig += 1;
            }
            let end = block_end(lines, sig, indent);
            out.emit(path, lines, to_i(i + 1), to_i(end.min(i + 26)), &format!("{owner}.push request-context method"));
            for j in start + 1..class_end {
                let Some(caller) = DEF_ANY.captures(&lines[j]) else {
                    continue;
                };
                if &caller[1] == "push" {
                    continue;
                }
                let caller_end = {
                    let base = indent_of(&lines[j]);
                    let mut stop = j + 1;
                    while stop < class_end && (lines[stop].trim().is_empty() || indent_of(&lines[stop]) > base) {
                        stop += 1;
                    }
                    stop
                };
                if SELF_PUSH.is_match(&lines[j..caller_end].join("\n")) {
                    out.emit(path, lines, to_i(j + 1), to_i(caller_end.min(j + 12)), &format!("{owner}.{} calls self.push", &caller[1]));
                }
            }
        }
        request_context_factories(path, lines, out);
        if !requested.is_empty() {
            context_aliases(path, lines, &classes, requested, out);
        }
    }
}

/// The factory that returns the context type and the receivers it hands out.
fn request_context_factories(path: &str, lines: &[String], out: &mut Blocks) {
    let len = lines.len();
    let to_i = |value: usize| i64::try_from(value).unwrap_or(i64::MAX);
    for (i, line) in lines.iter().enumerate() {
        let Some(found) = DEF_ANY.captures(line) else {
            continue;
        };
        let indent = indent_of(line);
        let mut sig = i;
        while sig + 1 < len && !lines[sig].trim_end().ends_with(':') {
            sig += 1;
        }
        let end = block_end(lines, sig, indent);
        let body = &lines[i..end];
        let text = body.join("\n");
        if &found[1] == "request_context"
            && RETURNS_CONTEXT.is_match(line)
            && let Some(ret) = body.iter().position(|row| RETURN_CONTEXT.is_match(row))
        {
            out.emit(path, lines, to_i(i + 1), to_i(end.min(i + 12)), "request-context factory returns context type");
            out.emit(path, lines, to_i(i + ret + 1), to_i(end.min(i + ret + 2)), "request-context factory constructs returned context");
        }
        let aliases: BTreeSet<String> = ALIAS_ASSIGN.captures_iter(&text).map(|caps| caps[1].to_owned()).collect();
        for alias in aliases {
            let push = re(&format!(r"\b{}\.push\s*\(", regex::escape(&alias)));
            for (j, body_line) in body.iter().enumerate() {
                if push.is_match(body_line) {
                    out.emit(path, lines, to_i(i + j + 1), to_i(end.min(i + j + 9)), &format!("{} pushes request-context receiver {alias}", &found[1]));
                }
            }
        }
    }
}

/// A module `__getattr__` that answers a requested alias with a class the
/// documentation says is merged with it.
fn context_aliases(
    path: &str,
    lines: &[String],
    classes: &[(usize, String, usize)],
    requested: &BTreeSet<String>,
    out: &mut Blocks,
) {
    let len = lines.len();
    let to_i = |value: usize| i64::try_from(value).unwrap_or(i64::MAX);
    let mut merged: BTreeSet<&str> = BTreeSet::new();
    for (_, name, start) in classes {
        let class_end = (start + 1..len).find(|j| CLASS_AT_ROOT.is_match(&lines[*j])).unwrap_or(len);
        let doc = lines[*start..class_end.min(start + 45)].join("\n");
        let hit = requested.iter().any(|alias| {
            re(&format!(r"(?i)merged with.{{0,40}}\b{}\b", regex::escape(alias))).is_match(&doc)
        });
        if hit {
            merged.insert(name);
        }
    }
    for (at, line) in lines.iter().enumerate() {
        if !GETATTR.is_match(line) {
            continue;
        }
        let end = block_end(lines, at, 0);
        // The last line of the function holding code (a trailing comment is not part of it).
        let last = (at..end).rev().find(|j| !lines[*j].trim().is_empty() && !lines[*j].trim_start().starts_with('#')).unwrap_or(at);
        for j in at + 1..=last.min(len - 1) {
            let Some(test) = IF_NAME.captures(&lines[j]) else {
                continue;
            };
            let alias = test.get(2).or_else(|| test.get(3)).map_or("", |m| m.as_str());
            if !requested.contains(alias) {
                continue;
            }
            let base = test[1].chars().count();
            let target = (j + 1..=last)
                .take_while(|k| lines[*k].trim().is_empty() || indent_of(&lines[*k]) > base)
                .filter(|k| !lines[*k].trim().is_empty())
                .find_map(|k| {
                    let first = indent_of(&lines[k]);
                    let inner = (j + 1..=last).find(|m| !lines[*m].trim().is_empty()).map_or(first, |m| indent_of(&lines[m]));
                    if first != inner {
                        return None;
                    }
                    RETURN_NAME.captures(&lines[k]).map(|caps| caps[1].to_owned())
                });
            if let Some(target) = target
                && merged.contains(target.as_str())
            {
                out.emit(path, lines, to_i(at + 1), to_i((last + 1).min(at + 1 + 12)), &format!("{alias} alias resolves to merged {target}"));
            }
        }
    }
}
