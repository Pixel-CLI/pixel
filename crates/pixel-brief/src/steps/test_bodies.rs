// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `test-bodies` (`specs/brief.md`, "Recipes"): for a question about tests,
//! quote the test cases whose name or body carries the prompt's subject.
//!
//! Candidates are the test files the brief names, then those whose path names
//! the subject, then those whose text carries the subject words; Rust keeps
//! its tests inline, so a named or subject-named source holding a `#[test]`
//! counts too. A case is a Python `def test_*`, a JS `it(...)`/`test(...)`, or
//! a Rust `fn` under `#[test]`, cut at its closing line and at 40 lines. A
//! case scores by the subject words its name (3x) and body carry, each weighted
//! by how few cases carry it; cases under half the best score are left out, and
//! a prompt that names one option (`--exec`) keeps only the cases that prove
//! that exact option. Read-only, deterministic, at most 9 000 bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::region_pack::Block;
use crate::routes::QuestionKind;

use super::{Ctx, Outcome, Step, subwords};

/// Function words only: how much a content word says is measured on the
/// repository (the idf below), never listed here.
const STOP: &[&str] = &[
    "what", "where", "which", "that", "this", "with", "from", "does", "how", "the", "and", "are",
    "they", "them", "for", "into", "about", "when", "should", "their", "there", "these", "those",
    "have", "your", "can", "could", "would", "will", "may", "might", "must", "whether", "test",
    "tests", "testing", "check", "checks", "please", "need", "want", "you", "our", "its", "did",
    "do", "was", "were", "be", "been", "being", "cover", "covers", "exercise", "include", "spec",
    "specs",
];
const CODE_EXTENSIONS: &[&str] = &[
    ".py", ".rs", ".js", ".ts", ".tsx", ".jsx", ".mjs", ".go", ".rb",
];
const OUT_CAP: usize = 9_000;
const UNIT_CAP: usize = 40;
const MAX_FILES: usize = 16;
const MAX_BYTES: u64 = 2_000_000;
const NAME_CHARS: usize = 80;
const LINE_CHARS: usize = 160;
/// Rows the "which test files carry the words" search may read.
const CARRY_ROWS: usize = 2_000;

static ASKS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:where|which|what)\b.{0,80}\b(?:tests?|specs?)\b|\b(?:tests?|specs?)\s+(?:for|of|around)\b|\b(?:tests?|specs?)\b.{0,40}\b(?:cover|exercise|include)\b",
    )
    .expect("the tests-question pattern is a literal")
});
static TEST_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(^|/)(tests?|__tests__|spec|testdata)/|(^|/)test_[^/]*$|_tests?\.[a-z]+$|\.(test|spec)\.[a-z]+$|(^|/)tests?\.rs$",
    )
    .expect("the test-path pattern is a literal")
});
static WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z][A-Za-z0-9_.]*").expect("the word pattern is a literal"));
static OPTION_FLAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)--([a-z][a-z0-9]*(?:-[a-z0-9]+)*)").expect("the flag pattern is a literal")
});
static OPTION_NAMED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b([a-z][a-z0-9]*(?:-[a-z0-9]+)*)\s+option\b")
        .expect("the option pattern is a literal")
});
static PY_CASE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:async\s+)?def\s+(test\w*)\s*\(")
        .expect("the python case pattern is a literal")
});
static JS_CASE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*(?:it|test|specify)(?:\.\w+)?\s*\(\s*(?:'([^']+)'|"([^"]+)"|`([^`]+)`)"#)
        .expect("the js case pattern is a literal")
});
static RS_ATTR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*#\[(?:\w+::)?test\b").expect("the rust attribute pattern is a literal")
});
static RS_FN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)\s*\(")
        .expect("the rust fn pattern is a literal")
});

/// `w` lowercased with one derivational or plural ending cut.
fn stem(word: &str) -> String {
    let word = word.to_lowercase();
    for suffix in [
        "ations", "ation", "ings", "ing", "ers", "ies", "es", "s", "ed",
    ] {
        if word.chars().count() >= 4 + suffix.len()
            && let Some(cut) = word.strip_suffix(suffix)
        {
            return cut.to_string();
        }
    }
    word
}

/// The subject words of the prompt, stemmed, in the order it gave them.
fn terms(prompt: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for found in WORD.find_iter(prompt) {
        let raw = found.as_str();
        // Split at `.`, `_` and a lowercase-to-uppercase change.
        let mut parts: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut previous: Option<char> = None;
        for ch in raw.chars() {
            if ch == '.' || ch == '_' {
                parts.push(std::mem::take(&mut current));
            } else {
                if ch.is_ascii_uppercase() && previous.is_some_and(|p| p.is_ascii_lowercase()) {
                    parts.push(std::mem::take(&mut current));
                }
                current.push(ch);
            }
            previous = Some(ch);
        }
        parts.push(current);
        for part in parts {
            if part.chars().count() >= 3 && !STOP.contains(&part.to_lowercase().as_str()) {
                out.push(stem(&part));
            }
        }
        let joined: String = raw.chars().filter(|ch| *ch != '.' && *ch != '_').collect();
        if joined != raw && joined.chars().count() >= 5 {
            out.push(stem(&joined));
        }
    }
    let mut seen = BTreeSet::new();
    out.retain(|word| seen.insert(word.clone()));
    out
}

/// An option the prompt names, with `-` as `_`: `--exec-batch` or `the exec
/// option`.
fn requested_option(prompt: &str) -> Option<String> {
    let caps = OPTION_FLAG
        .captures(prompt)
        .or_else(|| OPTION_NAMED.captures(prompt))?;
    Some(caps[1].to_lowercase().replace('-', "_"))
}

/// Whether the case proves the exact option: its title is the option, or its
/// body holds the exact flag literal.
fn exact_option_case(name: &str, option: Option<&str>, body: &str) -> bool {
    let Some(option) = option else {
        return false;
    };
    let squashed = squash_title(name);
    let squashed = squashed
        .strip_prefix("test_")
        .or_else(|| squashed.strip_prefix("spec_"))
        .unwrap_or(&squashed);
    if squashed == option {
        return true;
    }
    let flag = format!("--{}", option.replace('_', "-"));
    let word_char = |ch: char| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-';
    body.match_indices(&flag).any(|(start, found)| {
        body[..start]
            .chars()
            .next_back()
            .is_none_or(|ch| !word_char(ch))
            && body[start + found.len()..]
                .chars()
                .next()
                .is_none_or(|ch| !word_char(ch))
    })
}

/// `name` lowercased with runs of non-alphanumerics as one `_`, ends trimmed.
fn squash_title(name: &str) -> String {
    let mut out = String::new();
    for ch in name.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

fn tokens(text: &str) -> BTreeSet<String> {
    subwords(text).into_iter().collect()
}

/// Whether the tokens carry `word` or a simple inflection of it, not any
/// longer token that shares its prefix.
fn carries(tokens: &BTreeSet<String>, word: &str) -> bool {
    let mut variants = vec![
        word.to_string(),
        format!("{word}s"),
        format!("{word}es"),
        format!("{word}ed"),
        format!("{word}d"),
        format!("{word}ing"),
        format!("{word}e"),
    ];
    if let Some(root) = word.strip_suffix('e') {
        variants.push(format!("{root}ed"));
        variants.push(format!("{root}ing"));
    }
    variants.iter().any(|variant| tokens.contains(variant))
}

/// A test case: its name and its 1-based first and last lines.
type Unit = (String, usize, usize);

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The test cases of a file: Python `def test_*`, JS `it/test(...)`, Rust `fn`
/// under `#[test]`.
fn units(lines: &[&str], path: &str) -> Vec<Unit> {
    let mut starts: Vec<(String, usize)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if path.ends_with(".py") {
            if let Some(caps) = PY_CASE.captures(line) {
                starts.push((caps[1].to_string(), index));
            }
        } else if path.ends_with(".rs") {
            if RS_ATTR.is_match(line) {
                let window = lines
                    .iter()
                    .take((index + 4).min(lines.len()))
                    .skip(index + 1);
                if let Some(caps) = window.into_iter().find_map(|next| RS_FN.captures(next)) {
                    starts.push((caps[1].to_string(), index));
                }
            }
        } else if let Some(caps) = JS_CASE.captures(line) {
            let title = (1..=3)
                .find_map(|group| caps.get(group))
                .map_or("", |m| m.as_str());
            starts.push((title.to_string(), index));
        }
    }
    let python = path.ends_with(".py");
    let mut out = Vec::new();
    for (k, (name, i)) in starts.iter().enumerate() {
        let indent = indent_of(lines[*i]);
        let mut end = starts.get(k + 1).map_or(lines.len() - 1, |next| next.1 - 1);
        for (j, text) in lines.iter().enumerate().take(end + 1).skip(i + 1) {
            if text.trim().is_empty() {
                continue;
            }
            let ind = indent_of(text);
            if ind < indent || (python && ind == indent) {
                end = j - 1;
                break;
            }
            if ind == indent && text.trim_start().starts_with(['}', ')']) {
                end = j;
                break;
            }
        }
        while end > *i && lines[end].trim().is_empty() {
            end -= 1;
        }
        out.push((name.clone(), i + 1, (end + 1).min(i + UNIT_CAP)));
    }
    out
}

pub(super) fn run(ctx: &Ctx, out: &mut Outcome) {
    if !ctx.wants(
        Step::TestBodies,
        &[
            QuestionKind::Lookup,
            QuestionKind::Flow,
            QuestionKind::Config,
            QuestionKind::Bugfix,
            QuestionKind::Rationale,
            QuestionKind::Overview,
            QuestionKind::History,
            QuestionKind::Review,
        ],
    ) || !ASKS.is_match(ctx.typed)
    {
        return;
    }
    let words = terms(ctx.typed);
    if words.is_empty() {
        return;
    }
    let _timer = ctx.timer(Step::TestBodies);
    let Some(tracked) = ctx.files() else {
        return;
    };
    let blocks = cases(ctx, tracked, &words);
    out.rescuing |= !blocks.is_empty();
    out.append.extend(blocks);
}

fn cases(ctx: &Ctx, tracked: &[String], words: &[String]) -> Vec<Block> {
    let is_code = |path: &str| CODE_EXTENSIONS.iter().any(|ext| path.ends_with(ext));
    let tests: BTreeSet<&str> = tracked
        .iter()
        .map(String::as_str)
        .filter(|path| is_code(path) && TEST_PATH.is_match(path))
        .collect();
    let rust: BTreeSet<&str> = tracked
        .iter()
        .map(String::as_str)
        .filter(|path| path.ends_with(".rs"))
        .collect();
    let flat = |path: &str| -> String {
        path.to_lowercase()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect()
    };
    let flats: BTreeMap<&str, String> =
        tests.union(&rust).map(|path| (*path, flat(path))).collect();
    // A word that names more than a quarter of the test files (the word for
    // "test" itself) names no subject.
    let pathy: Vec<&String> = words
        .iter()
        .filter(|word| {
            tests
                .iter()
                .filter(|path| flats[*path].contains(word.as_str()))
                .count()
                * 4
                <= tests.len().max(1)
        })
        .collect();
    let names_subject = |path: &str| pathy.iter().any(|word| flats[path].contains(word.as_str()));

    // The test files the brief names, then those whose path names the subject,
    // then those whose text carries the subject words.
    let mut pool: Vec<String> = ctx
        .near
        .iter()
        .filter(|path| tests.contains(path.as_str()) || rust.contains(path.as_str()))
        .cloned()
        .collect();
    pool.extend(
        tests
            .union(&rust)
            .filter(|path| names_subject(path))
            .map(ToString::to_string),
    );
    pool.extend(carrying(ctx, &tests, &pathy));

    let mut files: Vec<String> = Vec::new();
    let mut parsed: BTreeMap<String, (String, Vec<Unit>)> = BTreeMap::new();
    for path in pool {
        if files.contains(&path) {
            continue;
        }
        if ctx.timed_out() {
            break;
        }
        let Some(text) = ctx.read_up_to(&path, MAX_BYTES) else {
            continue;
        };
        if tests.contains(path.as_str()) || text.contains("#[test]") {
            let lines: Vec<&str> = text.lines().collect();
            let found = units(&lines, &path);
            parsed.insert(path.clone(), (text, found));
            files.push(path);
        }
        if files.len() == MAX_FILES {
            break;
        }
    }

    let option = requested_option(ctx.typed);
    let lines_of = |path: &str| -> Vec<&str> { parsed[path].0.lines().collect() };
    let case_text = |lines: &[&str], a: usize, b: usize| lines[a - 1..b].join(" ");
    let mut df: BTreeMap<&str, usize> = words.iter().map(|word| (word.as_str(), 0)).collect();
    let mut total = 0;
    for path in &files {
        let lines = lines_of(path);
        for (_, a, b) in &parsed[path].1 {
            total += 1;
            let toks = tokens(&case_text(&lines, *a, *b));
            for word in words {
                if carries(&toks, word) {
                    *df.entry(word.as_str()).or_default() += 1;
                }
            }
        }
    }
    let total = total.max(1) as f64;
    let idf = |word: &str| -> f64 {
        match df[word] {
            0 => 0.0,
            count => ((total + 1.0) / count as f64).ln(),
        }
    };

    // (score, rank, path, first, last, name)
    let mut cands: Vec<(f64, usize, &str, usize, usize, &str)> = Vec::new();
    for (rank, path) in files.iter().enumerate() {
        let lines = lines_of(path);
        for (name, a, b) in &parsed[path].1 {
            let text = case_text(&lines, *a, *b);
            let exact = exact_option_case(name, option.as_deref(), &text);
            let (body, title) = (tokens(&text), tokens(name));
            let support = words
                .iter()
                .filter(|word| carries(&title, word) || carries(&body, word))
                .count();
            if support < words.len().min(2) && !exact {
                continue;
            }
            let mut score: f64 = words
                .iter()
                .map(|word| {
                    idf(word)
                        * (3.0 * f64::from(u8::from(carries(&title, word)))
                            + f64::from(u8::from(carries(&body, word))))
                })
                .sum();
            if exact {
                score = score.max(1.0);
            }
            if score <= 0.0 {
                continue;
            }
            cands.push((score, rank, path, *a, *b, name));
        }
    }
    cands.sort_by(|x, y| {
        y.0.total_cmp(&x.0)
            .then(x.1.cmp(&y.1))
            .then(x.2.cmp(y.2))
            .then(x.3.cmp(&y.3))
            .then(x.4.cmp(&y.4))
            .then(x.5.cmp(y.5))
    });
    if option.is_some() {
        let titled: Vec<_> = cands
            .iter()
            .filter(|c| exact_option_case(c.5, option.as_deref(), ""))
            .copied()
            .collect();
        cands = if titled.is_empty() {
            cands
                .into_iter()
                .filter(|c| {
                    let lines = lines_of(c.2);
                    exact_option_case(c.5, option.as_deref(), &case_text(&lines, c.3, c.4))
                })
                .collect()
        } else {
            titled
        };
    }
    if let Some(best) = cands.first().map(|c| c.0) {
        // The subject's tests, not every test that says "prompt".
        cands.retain(|c| c.0 >= 0.5 * best);
    }
    let mut blocks = Vec::new();
    let mut size = 0;
    for (_, _, path, a, b, name) in cands {
        let lines = lines_of(path);
        let body: Vec<String> = (a..=b)
            .map(|k| {
                let line: String = lines[k - 1].chars().take(LINE_CHARS).collect();
                format!("{k}| {line}")
            })
            .collect();
        let head: String = name.chars().take(NAME_CHARS).collect();
        let block = Block::new(format!("{path}:{a}-{b} — test {head}"), body);
        let cost = block.text().len() + 1;
        if size + cost > OUT_CAP {
            continue;
        }
        size += cost;
        blocks.push(block);
    }
    blocks
}

/// Test files whose text carries the most subject words, most first.
fn carrying(ctx: &Ctx, tests: &BTreeSet<&str>, words: &[&String]) -> Vec<String> {
    if tests.is_empty() || words.is_empty() {
        return Vec::new();
    }
    let alternation: Vec<String> = words.iter().map(|word| regex::escape(word)).collect();
    let pattern = format!("(?i){}", alternation.join("|"));
    let Ok(rows) = ctx.evidence.grep(&pattern, CARRY_ROWS, ctx.deadline) else {
        return Vec::new();
    };
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for row in &rows.hits {
        if tests.contains(row.path.as_str()) {
            *counts.entry(row.path.as_str()).or_default() += 1;
        }
    }
    let mut ranked: Vec<(usize, &str)> = counts.into_iter().map(|(path, n)| (n, path)).collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    ranked
        .into_iter()
        .map(|(_, path)| path.to_string())
        .collect()
}

#[cfg(test)]
mod tests;
