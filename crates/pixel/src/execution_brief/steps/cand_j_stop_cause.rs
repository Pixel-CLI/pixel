// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `cand-j-stop-cause`: a "why does X stop/skip/ignore ..." question is
//! answered by the code that implements the effect and the tests that pin it
//! down.
//!
//! Ported from `probes/cand-j-stop-cause/probe`. The question's rare words
//! (IDF over the tracked source, plus a small table of verb synonyms: stop =
//! quit = halt ...) each form a group; every 30-line window of every file
//! scores the groups it holds close together. Windows inside multi-line string
//! literals (flag documentation) and windows with little code are left out;
//! the best non-overlapping windows are shown, tests and source alike, a test
//! file named after a group weighing a little more. Brief blocks whose best
//! window holds fewer than three groups are dropped, longer ones are cut to
//! their best window. A shown function that is short and named by a group
//! brings its first caller from the graph. Read-only, deterministic.
//!
//! Since #1344 the step no longer matches the script byte for byte: a source
//! excerpt is bounded to [`SOURCE_CAP`] lines that favour executable matches
//! and control flow, test units are kept whole and ranked by evidence per
//! character, and the densest code leads, then the tests, then the rest.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::LazyLock;

use pixel_plugin::ProbeMode;
use regex::Regex;

use super::graph::{Role, Sym};
use super::text::{
    BRACE_NOISE, COMMENT_START, DECL, LETTERS, SRC, TESTPATH, escape, head, lines_of, stem,
};
use super::{Ctx, Step};

pub(super) const STEP: Step = Step {
    name: "cand-j-stop-cause",
    when_kinds: &[],
    skip_kinds: &["overview", "review", "history"],
    when_prompt: Some(
        r"(?i)\bwhy\b.*\b(stops?|stopp\w+|skips?|skipp\w+|ignor\w+|hid\w+|hides?|suppress\w+|refus\w+|never|silently|missing|drops?|dropp\w+|fails?|not)\b",
    ),
    when_gate_closed: true,
    budget_ms: 2500,
    mode: ProbeMode::Lead,
    run,
};

/// Words that name nothing of the code.
const STOP: &[&str] = &[
    "what", "where", "which", "that", "this", "with", "from", "does", "how", "the", "and", "for",
    "are", "was", "were", "why", "when", "who", "whom", "whose", "into", "onto", "than", "then",
    "them", "they", "their", "there", "these", "those", "have", "has", "had", "been", "being",
    "will", "would", "should", "could", "can", "may", "might", "must", "about", "after", "before",
    "between", "through", "during", "over", "under", "again", "further", "once", "here", "some",
    "such", "only", "same", "also", "just", "very", "more", "most", "other", "each", "both", "any",
    "all", "not", "our", "your", "its", "his", "her", "him", "she", "you", "out", "off", "use",
    "used", "using", "make", "made", "get", "got", "let", "trace", "explain", "tell", "show",
    "describe", "find", "list", "name", "names", "code", "function", "functions", "method",
    "methods", "file", "files", "line", "lines", "value", "values", "data",
];
/// Words of the frame of a question, by prefix.
const FRAME: &[&str] = &[
    "test", "prov", "proo", "cove", "defi", "call", "beha", "work", "happ", "deci", "expl", "show",
    "mean",
];
/// Verbs that say the same thing in a code base.
const SYNONYMS: &[&str] = &[
    "stop quit halt abort terminate bail cancel",
    "skip suppress ignore hide filter refuse reject discard omit exclude",
    "fail error crash panic",
    "print display output emit",
];
/// Lines of a window.
const SIZE: usize = 30;
/// Groups a window needs, for the brief's own blocks.
const MIN_GROUPS: usize = 3;
/// Source windows shown at most.
const MAX_SRC: usize = 5;
/// Source windows per file.
const PER_FILE: usize = 1;
/// Bytes of source windows.
const BYTES: usize = 5300;
/// Share of code-like lines a window needs.
const MIN_DENSITY: f64 = 0.35;
/// Share of string-literal lines a window may hold.
const MAX_PROSE: f64 = 0.1;
/// A window in the brief's own blocks weighs this much more.
const BRIEF_BOOST: f64 = 1.3;
/// Groups a window holds at least.
const MIN_WINDOW: usize = 2;
/// The rarest groups a window must hold all of.
const REQUIRED: usize = 2;
/// Test files shown, units of a family, bytes of tests.
const MAX_TEST: usize = 3;
const FAMILY: usize = 2;
const TEST_BYTES: usize = 3000;
/// Lines of a source excerpt.
const SOURCE_CAP: usize = 18;
/// A test file named after a group: its score, and its units' extra weight.
const STEM_BOOST: f64 = 1.15;
const DEDICATED_BOOST: f64 = 1.5;
/// How much a match weighs on an executable line, a comment line, any other.
const CODE_FACTOR: f64 = 2.0;
const COMMENT_FACTOR: f64 = 0.05;
const OTHER_FACTOR: f64 = 0.5;
/// Files read, and the bytes of one.
const MAX_FILES: usize = 4000;
const MAX_FILE_BYTES: u64 = 300_000;

static BRIEF_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^### (\S+?):(\d+)-(\d+)(.*)$").expect("the header pattern is a literal")
});
static BRIEF_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"definition|named by the prompt").expect("the label pattern is a literal")
});
static WANTS_TESTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(tests?|specs?|prov\w*|proof|cover\w*)\b")
        .expect("the tests pattern is a literal")
});
static STRING_OPEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(r#*"|"""|'''|`)"#).expect("the string pattern is a literal")
});
static TEST_MARK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*#\[(tokio::)?test|^\s*@(Test|pytest)").expect("the mark pattern is a literal")
});
/// Quoted literals (kept as evidence) and trailing line comments (cut).
static EVIDENCE_NOISE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(\\.|[^"\\])*"|'(\\.|[^'\\])*'|//.*$|(?:^|\s)#.*$"#)
        .expect("the evidence pattern is a literal")
});
static CONTROL_FLOW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:return|throw|raise|break|continue)\b")
        .expect("the control-flow pattern is a literal")
});

fn run(ctx: &Ctx) -> String {
    stop_cause(ctx).unwrap_or_default()
}

/// A test-shaped unit of a file: first and last line (1-based) and its name.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Unit {
    first: usize,
    last: usize,
    name: String,
}

/// The tests of a file, from the test mark (or the declaration) to the end;
/// leading comments are not part of a unit.
fn units(path: &str, lines: &[String]) -> Vec<Unit> {
    let mut found = Vec::new();
    let in_test_file = TESTPATH.is_match(path);
    for (at, line) in lines.iter().enumerate() {
        let Some(decl) = DECL.captures(line) else {
            continue;
        };
        let Some(name) = (1..=3).find_map(|k| decl.get(k).map(|m| m.as_str())) else {
            continue;
        };
        let mut start = at;
        while start > 0 && COMMENT_START.is_match(&lines[start - 1]) {
            start -= 1;
        }
        let marked = lines[start..=at].iter().any(|row| TEST_MARK.is_match(row));
        if !(in_test_file
            || marked
            || name.starts_with("test_")
            || decl.get(2).is_some()
            || decl.get(3).is_some())
        {
            continue;
        }
        let mut end = None;
        let (mut depth, mut seen) = (0_i64, false);
        'scan: for (offset, row) in lines[at..lines.len().min(at + 400)].iter().enumerate() {
            for ch in BRACE_NOISE.replace_all(row, "").chars() {
                match ch {
                    '{' => (depth, seen) = (depth + 1, true),
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            if seen && depth <= 0 {
                end = Some(at + offset);
                break 'scan;
            }
        }
        let end = end.unwrap_or_else(|| {
            let indent = |row: &str| row.chars().count() - row.trim_start().chars().count();
            let base = indent(&lines[at]);
            let mut next = at + 1;
            while next < lines.len()
                && (lines[next].trim().is_empty() || indent(&lines[next]) > base)
            {
                next += 1;
            }
            let mut last = next - 1;
            while last > at && lines[last].trim().is_empty() {
                last -= 1;
            }
            last
        });
        found.push(Unit {
            first: (start..=at)
                .find(|k| TEST_MARK.is_match(&lines[*k]))
                .map_or(at + 1, |k| k + 1),
            last: end + 1,
            name: name.to_owned(),
        });
    }
    found
}

/// Flags the lines inside multi-line string literals (Rust raw strings,
/// triple quotes, template literals).
fn prose_lines(lines: &[String]) -> Vec<bool> {
    let mut flags = Vec::with_capacity(lines.len());
    let mut inside: Option<String> = None;
    for line in lines {
        let mut flag = inside.is_some();
        match &inside {
            None => {
                if let Some(open) = STRING_OPEN.find(line) {
                    let token = open.as_str();
                    let close = if token.starts_with('r') {
                        format!("\"{}", "#".repeat(token.chars().count() - 2))
                    } else {
                        token.to_owned()
                    };
                    if !line[open.end()..].contains(&close) {
                        inside = Some(close);
                        flag = true;
                    }
                }
            }
            Some(close) => {
                if line.contains(close.as_str()) {
                    inside = None;
                }
            }
        }
        flags.push(flag);
    }
    flags
}

fn code_like(line: &str) -> bool {
    let line = line.trim();
    !line.is_empty()
        && !["//", "#", "*", "/*", "\"", "'"].iter().any(|p| line.starts_with(p))
        && (line.ends_with([';', '{', '}', ')', ',']) || line.contains('('))
}

/// One group per rare question word: its weight and its members (the word's
/// stem and its synonyms').
type Group = (f64, BTreeSet<String>);

fn groups_of(prompt: &str, texts: &BTreeMap<String, Vec<String>>) -> Vec<Group> {
    let count = texts.len();
    let lowered: Vec<String> = texts.values().map(|lines| lines.join("\n").to_lowercase()).collect();
    let synonyms: Vec<BTreeSet<&str>> = SYNONYMS
        .iter()
        .map(|group| group.split_whitespace().collect())
        .collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for word in LETTERS.find_iter(prompt).map(|word| word.as_str()) {
        if !seen.insert(word) {
            continue;
        }
        let lower = word.to_lowercase();
        if STOP.contains(&lower.as_str()) || FRAME.iter().any(|frame| lower.starts_with(frame)) {
            continue;
        }
        let root = stem(word);
        let df = lowered.iter().filter(|body| body.contains(root.as_str())).count();
        #[allow(clippy::cast_precision_loss)]
        let (df, count) = (df as f64, count as f64);
        if df == 0.0 || df / count > 0.5 {
            continue;
        }
        let mut members = BTreeSet::from([root.clone()]);
        for group in &synonyms {
            if group.contains(root.as_str()) || group.iter().any(|word| stem(word) == root) {
                members.extend(group.iter().map(|word| stem(word)));
            }
        }
        let weight = (count / df).ln();
        out.push((weight * weight, members));
    }
    out
}

/// A window: score, groups held, first and last line (1-based).
type Window = (f64, usize, usize, usize);

/// Every window of [`SIZE`] lines that holds the rarest groups.
fn windows(lines: &[String], groups: &[Group]) -> Vec<Window> {
    let count = lines.len();
    let mut order: Vec<usize> = (0..groups.len()).collect();
    order.sort_by(|a, b| groups[*b].0.total_cmp(&groups[*a].0));
    let required: Vec<usize> = order.into_iter().take(REQUIRED).collect();
    let lower: Vec<String> = lines.iter().map(|line| line.to_lowercase()).collect();
    let mut prefix = vec![vec![0_usize; count + 1]; groups.len()];
    for (at, line) in lower.iter().enumerate() {
        for (g, (_, members)) in groups.iter().enumerate() {
            let held: usize = members.iter().map(|member| line.matches(member.as_str()).count()).sum();
            prefix[g][at + 1] = prefix[g][at] + held;
        }
    }
    let prose = prose_lines(lines);
    let mut prose_prefix = vec![0_usize; count + 1];
    let mut code_prefix = vec![0_usize; count + 1];
    for at in 0..count {
        prose_prefix[at + 1] = prose_prefix[at] + usize::from(prose[at]);
        code_prefix[at + 1] = code_prefix[at] + usize::from(code_like(&lines[at]));
    }
    let mut out = Vec::new();
    for a in 0..count.saturating_sub(SIZE).saturating_add(1).max(1) {
        let b = count.min(a + SIZE);
        let counts: Vec<usize> = prefix.iter().map(|row| row[b] - row[a]).collect();
        if counts.iter().all(|c| *c == 0) || !required.iter().all(|g| counts[*g] != 0) {
            continue;
        }
        let got = counts.iter().filter(|c| **c != 0).count();
        #[allow(clippy::cast_precision_loss)]
        let span = (b - a) as f64;
        #[allow(clippy::cast_precision_loss)]
        let (prose_share, code_share) = (
            (prose_prefix[b] - prose_prefix[a]) as f64 / span,
            (code_prefix[b] - code_prefix[a]) as f64 / span,
        );
        if prose_share > MAX_PROSE || code_share < MIN_DENSITY {
            continue;
        }
        let mut score = 0.0;
        for ((weight, _), c) in groups.iter().zip(&counts) {
            if *c != 0 {
                #[allow(clippy::cast_precision_loss)]
                let held = *c as f64;
                score += weight * (1.0 + held.ln());
            }
        }
        out.push((score, got, a + 1, b));
    }
    out
}

fn count_in(members: &BTreeSet<String>, text: &str) -> usize {
    members.iter().map(|member| text.matches(member.as_str()).count()).sum()
}

/// `weight * (1 + ln count)`, the score of a group seen `count` times.
fn weighed(weight: f64, count: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let count = count as f64;
    weight * (1.0 + count.ln())
}

/// Groups held and their summed score in `lines`, line by line: a match on an
/// executable line weighs most, one in a comment or a declared name least.
fn unit_score(lines: &[String], groups: &[Group]) -> (usize, f64) {
    let (mut got, mut total) = (0, 0.0);
    for (weight, members) in groups {
        let (mut held, mut score) = (0, 0.0);
        for original in lines {
            let text = evidence_text(original).to_lowercase();
            let matches = count_in(members, &text);
            if matches == 0 {
                continue;
            }
            held += matches;
            let name = DECL
                .captures(original)
                .and_then(|caps| (1..=3).find_map(|k| caps.get(k)))
                .map_or(0, |name| count_in(members, &name.as_str().to_lowercase()))
                .min(matches);
            let body = matches - name;
            if body > 0 {
                score += weighed(*weight, body) * line_factor(original, &text);
            }
            if name > 0 {
                score += weighed(*weight, name) * COMMENT_FACTOR;
            }
        }
        if held != 0 {
            got += 1;
            total += score;
        }
    }
    (got, total)
}

/// The weight of a match on `original` (`text` is its [`evidence_text`]).
fn line_factor(original: &str, text: &str) -> f64 {
    if is_comment_line(original) {
        COMMENT_FACTOR
    } else if code_like(text) {
        CODE_FACTOR
    } else {
        OTHER_FACTOR
    }
}

/// A line's group evidence, a control-flow statement weighing twice the
/// heaviest group.
fn line_evidence(original: &str, groups: &[Group], heaviest: f64) -> f64 {
    let text = evidence_text(original).to_lowercase();
    let factor = line_factor(original, &text);
    let words: f64 = groups
        .iter()
        .map(|(weight, members)| match count_in(members, &text) {
            0 => 0.0,
            n => weighed(*weight, n) * factor,
        })
        .sum();
    let flow = factor == CODE_FACTOR && CONTROL_FLOW.is_match(&text);
    words + if flow { heaviest * 2.0 } else { 0.0 }
}

fn heaviest(groups: &[Group]) -> f64 {
    groups.iter().map(|(weight, _)| *weight).reduce(f64::max).unwrap_or(1.0)
}

/// The line without its trailing comment; quoted literals stay evidence.
fn evidence_text(line: &str) -> String {
    if is_comment_line(line) {
        return line.to_owned();
    }
    let mut text = String::with_capacity(line.len());
    let mut copied = 0;
    for found in EVIDENCE_NOISE.find_iter(line) {
        text.push_str(&line[copied..found.start()]);
        let token = found.as_str();
        if token.starts_with("//") || token.trim_start().starts_with('#') {
            return text;
        }
        text.push_str(token);
        copied = found.end();
    }
    text.push_str(&line[copied..]);
    text
}

/// The [`SOURCE_CAP`]-line part of `[first, last]` with the most evidence:
/// comments keep context without outweighing executable lines.
fn focused_window(lines: &[String], first: usize, last: usize, groups: &[Group]) -> (usize, usize) {
    let size = last - first + 1;
    if size <= SOURCE_CAP {
        return (first, last);
    }
    let heaviest = heaviest(groups);
    let evidence: Vec<f64> =
        (first..=last).map(|n| line_evidence(line_of(lines, n), groups, heaviest)).collect();
    let mut best = (first, f64::NEG_INFINITY);
    for (offset, window) in evidence.windows(SOURCE_CAP).enumerate() {
        let score: f64 = window.iter().sum();
        if score > best.1 {
            best = (first + offset, score);
        }
    }
    (best.0, best.0 + SOURCE_CAP - 1)
}

/// Rendered characters of `[a, b]`.
fn cost(lines: &[String], a: usize, b: usize) -> usize {
    (a..=b).map(|n| line_of(lines, n).chars().count() + 8).sum()
}

/// Rendered characters of a unit, blank lines left out as the block does.
fn unit_cost(lines: &[String], unit: &Unit) -> usize {
    (unit.first..=unit.last)
        .map(|n| line_of(lines, n))
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.chars().count() + 8)
        .sum()
}

/// Evidence per rendered character of a span.
fn density(lines: &[String], a: usize, b: usize, groups: &[Group]) -> f64 {
    let heaviest = heaviest(groups);
    let score: f64 = (a..=b).map(|n| line_evidence(line_of(lines, n), groups, heaviest)).sum();
    #[allow(clippy::cast_precision_loss)]
    let size = cost(lines, a, b).max(1) as f64;
    score / size
}

/// The densest code span first, then the test units in their order, then the
/// rest of the code by density; a span shown twice is kept once.
fn evidence_order(
    mut code: Vec<(String, usize, usize)>,
    tests: Vec<(String, usize, usize)>,
    texts: &BTreeMap<String, Vec<String>>,
    groups: &[Group],
) -> Vec<(String, usize, usize)> {
    let dense = |(path, a, b): &(String, usize, usize)| density(&texts[path], *a, *b, groups);
    code.sort_by(|x, y| dense(y).total_cmp(&dense(x)).then_with(|| x.cmp(y)));
    let rest = if code.is_empty() { Vec::new() } else { code.split_off(1) };
    let mut seen = BTreeSet::new();
    code.into_iter()
        .chain(tests)
        .chain(rest)
        .filter(|span| seen.insert(span.clone()))
        .collect()
}

fn line_of(lines: &[String], n: usize) -> &str {
    n.checked_sub(1)
        .and_then(|at| lines.get(at))
        .map_or("", String::as_str)
}

/// The window of the same size centred on the weighted centroid of the group
/// words inside `[a, b]`.
fn recentre(lines: &[String], a: usize, b: usize, groups: &[Group]) -> (usize, usize) {
    let mut weights = Vec::new();
    for n in a..=b {
        let low = line_of(lines, n).to_lowercase();
        let mut weight = 0.0;
        for (w, members) in groups {
            if members.iter().any(|member| low.contains(member.as_str())) {
                weight += w;
            }
        }
        weights.push((n, weight));
    }
    let total: f64 = weights.iter().fold(0.0, |sum, (_, w)| sum + w);
    if total == 0.0 {
        return (a, b);
    }
    #[allow(clippy::cast_precision_loss)]
    let centre = weights.iter().fold(0.0, |sum, (n, w)| sum + *n as f64 * w) / total;
    let size = b - a + 1;
    #[allow(clippy::cast_precision_loss)]
    let from = (centre - size as f64 / 2.0).round_ties_even();
    // `max(1, min(len - size + 1, round(..)))` of the script, on signed values.
    #[allow(clippy::cast_possible_truncation)]
    let from = from as i64;
    let ceiling = i64::try_from(lines.len()).unwrap_or(i64::MAX) - i64::try_from(size).unwrap_or(0) + 1;
    let lo = usize::try_from(from.min(ceiling).max(1)).unwrap_or(1);
    (lo, lo + size - 1)
}

fn is_comment_line(line: &str) -> bool {
    let line = line.trim_start();
    ["//", "#", "*", "/*"].iter().any(|p| line.starts_with(p))
}

fn brace_balance(line: &str) -> i64 {
    let count = |ch: char| i64::try_from(line.matches(ch).count()).unwrap_or(0);
    count('{') - count('}')
}

/// `[a, b]` widened to whole syntax: a window that starts inside a comment run
/// starts at the run, and one that ends inside an open brace ends where it
/// closes.
fn snap(lines: &[String], a: usize, b: usize) -> (usize, usize) {
    const REACH: usize = 10;
    let comment = |n: usize| is_comment_line(line_of(lines, n));
    let mut n = a;
    while n > 1 && a - n < REACH && comment(n) && comment(n - 1) {
        n -= 1;
    }
    let a = if comment(a) { n } else { a };
    let mut depth: i64 = (a..=b).map(|k| brace_balance(line_of(lines, k))).sum();
    let mut k = b;
    while depth > 0 && k < lines.len().min(b + REACH) {
        k += 1;
        depth += brace_balance(line_of(lines, k));
    }
    (a, if depth <= 0 { k } else { b })
}

/// `[a, b]` cut to the lines that hold a group word, with two lines around
/// them, and at least eight lines when the window allows.
fn trim(lines: &[String], a: usize, b: usize, groups: &[Group]) -> (usize, usize) {
    const CONTEXT: usize = 2;
    const FLOOR: usize = 8;
    let hit: Vec<usize> = (a..=b)
        .filter(|n| {
            let low = line_of(lines, *n).to_lowercase();
            groups
                .iter()
                .any(|(_, members)| members.iter().any(|member| low.contains(member.as_str())))
        })
        .collect();
    let (Some(first), Some(last)) = (hit.first(), hit.last()) else {
        return (a, b);
    };
    let (mut lo, mut hi) = (a.max(first.saturating_sub(CONTEXT)), b.min(last + CONTEXT));
    while hi - lo + 1 < FLOOR && (lo > a || hi < b) {
        (lo, hi) = (a.max(lo.saturating_sub(1)), b.min(hi + 1));
    }
    (lo, hi)
}

fn stem_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let cut = match name.rfind('.') {
        Some(dot) if dot > 0 && dot + 1 < name.len() => dot,
        _ => name.len(),
    };
    name[..cut].to_lowercase()
}

fn stop_cause(ctx: &Ctx) -> Option<String> {
    let mut paths: Vec<&String> = ctx.files()?.iter().filter(|path| SRC.is_match(path)).collect();
    paths.sort();
    let mut texts: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in paths.into_iter().take(MAX_FILES) {
        if ctx.timed_out() {
            return None;
        }
        let lines = lines_of(ctx, path, MAX_FILE_BYTES);
        if !lines.is_empty() {
            texts.insert(path.clone(), lines);
        }
    }
    if texts.len() < 4 {
        return None;
    }
    let groups = groups_of(ctx.prompt, &texts);
    if groups.len() < MIN_GROUPS {
        return None;
    }
    let names: BTreeSet<&str> = groups
        .iter()
        .flat_map(|(_, members)| members.iter().map(String::as_str))
        .collect();
    let boost = |path: &str| {
        let stem = stem_of(path);
        if names.iter().any(|name| stem.contains(name)) { 1.15 } else { 1.0 }
    };
    let mut scored: Vec<(&String, Vec<Window>)> = Vec::new();
    for (path, lines) in &texts {
        if ctx.timed_out() {
            return None;
        }
        let boost = boost(path);
        let mut found: Vec<Window> = windows(lines, &groups)
            .into_iter()
            .filter(|window| window.1 >= MIN_WINDOW)
            .map(|(score, got, a, b)| (score * boost, got, a, b))
            .collect();
        if !found.is_empty() && !TESTPATH.is_match(path) {
            let mut marked: HashSet<usize> = HashSet::new();
            for unit in units(path, lines) {
                // A unit's leading comments count as test lines here.
                let mut first = unit.first;
                while first > 1 && COMMENT_START.is_match(line_of(lines, first - 1)) {
                    first -= 1;
                }
                marked.extend(first..=unit.last);
            }
            found.retain(|(_, _, a, b)| {
                let inside = (*a..=*b).filter(|n| marked.contains(n)).count();
                #[allow(clippy::cast_precision_loss)]
                let share = inside as f64 / (b - a + 1) as f64;
                share < 0.5
            });
        }
        scored.push((path, found));
    }

    let mut drops: Vec<String> = Vec::new();
    let mut shown: HashMap<&str, Vec<(usize, usize)>> = HashMap::new();
    let mut in_brief: HashMap<&str, HashSet<usize>> = HashMap::new();
    for caps in BRIEF_HEADER.captures_iter(ctx.brief) {
        let (Some(path), Some(a), Some(b), Some(label)) =
            (caps.get(1), caps.get(2), caps.get(3), caps.get(4))
        else {
            continue;
        };
        let (Ok(a), Ok(b)) = (a.as_str().parse::<usize>(), b.as_str().parse::<usize>()) else {
            continue;
        };
        if BRIEF_LABEL.is_match(label.as_str()) {
            shown.entry(path.as_str()).or_default().push((a, b));
        } else {
            drops.push(escape(&format!("{}:{a}-{b}", path.as_str())));
        }
        in_brief.entry(path.as_str()).or_default().extend(a..=b);
    }
    let covered = |path: &str, a: usize, b: usize| {
        let mut have: HashSet<usize> = HashSet::new();
        for (lo, hi) in shown.get(path).into_iter().flatten() {
            have.extend(*lo..=*hi);
        }
        let inside = (a..=b).filter(|n| have.contains(n)).count();
        #[allow(clippy::cast_precision_loss)]
        let share = inside as f64 / (b + 1).saturating_sub(a).max(1) as f64;
        share
    };
    let wants_tests = WANTS_TESTS.is_match(ctx.prompt);
    let boosted = |score: f64, path: &str, a: usize, b: usize| {
        let inside = in_brief
            .get(path)
            .map_or(0, |held| (a..=b).filter(|n| held.contains(n)).count());
        #[allow(clippy::cast_precision_loss)]
        let share = inside as f64 / (b - a + 1) as f64;
        score * if share > 0.5 { BRIEF_BOOST } else { 1.0 }
    };

    let mut rows: Vec<(f64, &str, usize, usize)> = Vec::new();
    for (path, windows) in &scored {
        if TESTPATH.is_match(path) {
            continue;
        }
        for (score, _, a, b) in windows {
            rows.push((boosted(*score, path, *a, *b), path.as_str(), *a, *b));
        }
    }
    rows.sort_by(|x, y| {
        y.0.total_cmp(&x.0)
            .then_with(|| x.1.cmp(y.1))
            .then_with(|| x.2.cmp(&y.2))
    });
    let mut chosen: Vec<(String, usize, usize)> = Vec::new();
    // The windows before they are bounded: where callers are looked for.
    let mut discovery: Vec<(String, usize, usize)> = Vec::new();
    let mut per_file: HashMap<&str, usize> = HashMap::new();
    let mut used = 0;
    for (_, path, a, b) in rows {
        if chosen.len() >= MAX_SRC {
            break;
        }
        if per_file.get(path).copied().unwrap_or(0) >= PER_FILE || covered(path, a, b) > 0.5 {
            continue;
        }
        if chosen
            .iter()
            .any(|(other, x, y)| other == path && !(b + 4 < *x || a > y + 4))
        {
            continue;
        }
        let lines = &texts[path];
        let (a, b) = recentre(lines, a, b, &groups);
        let (a, b) = trim(lines, a, b, &groups);
        let (a, b) = snap(lines, a, b);
        let (lo, hi) = focused_window(lines, a, b, &groups);
        let size = cost(lines, lo, hi);
        if used + size > BYTES {
            continue;
        }
        chosen.push((path.to_owned(), lo, hi));
        discovery.push((path.to_owned(), a, b));
        *per_file.entry(path).or_insert(0) += 1;
        used += size;
    }

    chosen.extend(callers_of_named(ctx, &texts, &names, &discovery, &covered));
    let found = if wants_tests { tests(&texts, &groups, &names) } else { Vec::new() };
    let chosen = evidence_order(chosen, found, &texts, &groups);
    if chosen.is_empty() {
        return None;
    }
    let mut text = String::new();
    if !drops.is_empty() {
        let _ = writeln!(text, "drop: ^(?:{})", drops.join("|"));
    }
    for (path, a, b) in &chosen {
        let kind = if TESTPATH.is_match(path) { "test" } else { "code" };
        let _ = writeln!(text, "### {path}:{a}-{b} — {kind}");
        let lines = &texts[path];
        for n in *a..=*b {
            let line = line_of(lines, n);
            if !line.trim().is_empty() {
                let _ = writeln!(text, "{n}| {}", head(line, 200));
            }
        }
    }
    Some(text)
}

/// For every short function shown whose name carries a group word: the
/// first caller of the graph, around its call, unless the brief shows it.
fn callers_of_named(
    ctx: &Ctx,
    texts: &BTreeMap<String, Vec<String>>,
    names: &BTreeSet<&str>,
    chosen: &[(String, usize, usize)],
    covered: &dyn Fn(&str, usize, usize) -> f64,
) -> Vec<(String, usize, usize)> {
    let long: Vec<&str> = names.iter().copied().filter(|name| name.chars().count() >= 4).collect();
    let mut extra: Vec<(String, usize, usize)> = Vec::new();
    for (path, a, b) in chosen {
        if TESTPATH.is_match(path) {
            continue;
        }
        let lines = &texts[path];
        for k in *a..=*b {
            let Some(name) = DECL.captures(line_of(lines, k)).and_then(|caps| caps.get(1)) else {
                continue;
            };
            let lower = name.as_str().to_lowercase();
            if !(long.is_empty() || long.iter().any(|word| lower.contains(word))) {
                continue;
            }
            let Some(sym) = ctx
                .symbols_named(name.as_str())
                .into_iter()
                .find(|sym| sym.path == *path && usize::try_from(sym.start_line) == Ok(k))
            else {
                continue;
            };
            if sym.end_line - sym.start_line > 12 {
                continue;
            }
            let mut callers = ctx.call_edges(&sym.uid, Role::Callers, 0);
            callers.sort_by(|x, y| {
                x.symbol
                    .path
                    .cmp(&y.symbol.path)
                    .then(x.symbol.start_line.cmp(&y.symbol.start_line))
            });
            if let Some(edge) = callers.first() {
                let caller: &Sym = &edge.symbol;
                let site = edge.site_line.filter(|site| *site != 0).unwrap_or(caller.start_line);
                let lo = caller.start_line.max(site - 14);
                let hi = caller.end_line.min(site + 12);
                let (Ok(lo), Ok(hi)) = (usize::try_from(lo), usize::try_from(hi)) else {
                    continue;
                };
                if hi >= lo && covered(&caller.path, lo, hi) > 0.5 {
                    continue;
                }
                extra.push((caller.path.clone(), lo, hi));
            }
        }
    }
    let mut fresh = Vec::new();
    for item in extra {
        if !chosen.contains(&item) && !fresh.contains(&item) {
            fresh.push(item);
        }
    }
    fresh
}

/// A test and its score.
struct Found<'a> {
    total: f64,
    path: &'a str,
    unit: Unit,
}

/// The first two `_`-separated words of a name with three or more, else the
/// name; digits count (`mode1` and `mode2` are two families), so a numeric
/// suffix after them (`stop_after_binary_2`) is a sibling.
fn family_of(name: &str) -> String {
    if name.matches('_').count() >= 2 {
        name.split('_').take(2).collect::<Vec<_>>().join("_")
    } else {
        name.to_owned()
    }
}

/// The whole tests that carry the question's groups, by evidence per
/// character: at most one per file (two in a file named after a group, of two
/// families), each with one sibling of its family, within [`TEST_BYTES`]; a
/// unit larger than that is skipped.
fn tests(
    texts: &BTreeMap<String, Vec<String>>,
    groups: &[Group],
    names: &BTreeSet<&str>,
) -> Vec<(String, usize, usize)> {
    let named = |path: &str| {
        let stem = stem_of(path);
        names.iter().any(|name| stem.contains(name))
    };
    let mut all: Vec<Found> = Vec::new();
    for (path, lines) in texts {
        let boost = if named(path) { STEM_BOOST * DEDICATED_BOOST } else { 1.0 };
        for unit in units(path, lines) {
            let size = unit_cost(lines, &unit);
            if size > TEST_BYTES {
                continue;
            }
            let body = &lines[unit.first - 1..unit.last.min(lines.len())];
            let (got, total) = unit_score(body, groups);
            if got >= MIN_GROUPS - 1 && total > 0.0 {
                #[allow(clippy::cast_precision_loss)]
                let total = total * boost / size.max(1) as f64;
                all.push(Found { total, path, unit });
            }
        }
    }
    all.sort_by(|x, y| {
        y.total
            .total_cmp(&x.total)
            .then_with(|| x.path.cmp(y.path))
            .then_with(|| x.unit.first.cmp(&y.unit.first))
    });
    let mut files: BTreeMap<&str, usize> = BTreeMap::new();
    let mut families: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    let mut out: Vec<(String, usize, usize)> = Vec::new();
    let mut spent = 0;
    for Found { path, unit, .. } in all {
        let lines = &texts[path];
        let family = family_of(&unit.name);
        let held = files.get(path).copied().unwrap_or(0);
        let room = if named(path) { 2 } else { 1 };
        if held >= room
            || families.get(path).is_some_and(|known| known.contains(&family))
            || spent + unit_cost(lines, &unit) > TEST_BYTES
            || (held == 0 && files.len() >= MAX_TEST)
        {
            continue;
        }
        let shown = |out: &[(String, usize, usize)], first: usize| {
            out.iter().any(|(known, at, _)| known == path && *at == first)
        };
        let mut group = vec![unit.clone()];
        if family != unit.name {
            group.extend(
                units(path, lines)
                    .into_iter()
                    .filter(|other| {
                        family_of(&other.name) == family
                            && other.first != unit.first
                            && !shown(&out, other.first)
                    })
                    .take(FAMILY - 1),
            );
        }
        files.insert(path, held + 1);
        families.entry(path).or_default().insert(family);
        for member in group {
            let size = unit_cost(lines, &member);
            if shown(&out, member.first) || spent + size > TEST_BYTES {
                continue;
            }
            spent += size;
            out.push((path.to_owned(), member.first, member.last));
        }
    }
    out
}

#[cfg(test)]
mod tests;
