// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `test-bodies` (issue #1196): for a question about tests, quote the test
//! cases whose name or body carries the prompt's subject.
//!
//! Ported from `probes/test-bodies/probe`. Candidates are the test files the
//! brief names, then those whose path names the subject, then those whose text
//! carries the subject words; Rust keeps its tests inline, so a named or
//! subject-named source holding a `#[test]` counts too. A case is a Python
//! `def test_*`, a JS `it(...)`/`test(...)`, or a Rust `fn` under `#[test]`,
//! cut at its closing line and at 40 lines. A case scores by the subject
//! words its name (3x) and body carry (a token that is the word or a simple
//! inflection of it), each weighted by how few cases carry it; cases under
//! half the best score, and ranges the brief already quotes, are left out. A
//! prompt that names one option (`--exec`) keeps only the cases that prove
//! that exact option. Read-only, deterministic, at most 9 000 characters.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use pixel_plugin::ProbeMode;
use regex::Regex;

use super::text::subwords;
use super::{Ctx, Step};

pub(super) const STEP: Step = Step {
    name: "test-bodies",
    when_kinds: &["tests"],
    skip_kinds: &["history", "review"],
    when_prompt: Some(
        r"(?i)\b(?:where|which|what)\b.{0,80}\b(?:tests?|specs?)\b|\b(?:tests?|specs?)\s+(?:for|of|around)\b|\b(?:tests?|specs?)\b.{0,40}\b(?:cover|exercise|include)\b",
    ),
    when_gate_closed: true,
    budget_ms: 1500,
    mode: ProbeMode::Append,
    run,
};

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
const CODE: &[&str] = &[
    ".py", ".rs", ".js", ".ts", ".tsx", ".jsx", ".mjs", ".go", ".rb",
];
/// Characters of output at most.
const OUT_CAP: usize = 9_000;
/// Lines of one case at most.
const UNIT_CAP: usize = 40;
/// Files whose cases compete.
const MAX_FILES: usize = 16;
/// Bytes of a file that is read; a longer one is not a test file to quote.
const MAX_BYTES: u64 = 2_000_000;
const NAME_CHARS: usize = 80;
const LINE_CHARS: usize = 160;

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
static MENTIONED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\w./-]+\.\w+").expect("the file-mention pattern is a literal")
});
static QUOTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^### (\S+?):(\d+)-(\d+)").expect("the quoted-range pattern is a literal")
});

/// `word` lowercased with one derivational or plural ending cut.
fn stem(word: &str) -> String {
    let word = word.to_lowercase();
    for suffix in [
        "ations", "ation", "ings", "ing", "ers", "ies", "es", "s", "ed",
    ] {
        if word.chars().count() >= 4 + suffix.len()
            && let Some(cut) = word.strip_suffix(suffix)
        {
            return cut.to_owned();
        }
    }
    word
}

/// The subject words of the prompt, stemmed, in the order it gave them: the
/// parts of each identifier-like word (split at `.`, `_` and a lowercase to
/// uppercase change) that are not function words, and the identifier joined.
fn terms(prompt: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for found in WORD.find_iter(prompt) {
        let raw = found.as_str();
        let mut parts = vec![String::new()];
        let mut previous: Option<char> = None;
        for ch in raw.chars() {
            if ch == '.' || ch == '_' {
                parts.push(String::new());
            } else {
                if ch.is_ascii_uppercase() && previous.is_some_and(|p| p.is_ascii_lowercase()) {
                    parts.push(String::new());
                }
                if let Some(part) = parts.last_mut() {
                    part.push(ch);
                }
            }
            previous = Some(ch);
        }
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
    out.trim_matches('_').to_owned()
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
        !body[..start].chars().next_back().is_some_and(word_char)
            && !body[start + found.len()..]
                .chars()
                .next()
                .is_some_and(word_char)
    })
}

fn tokens(text: &str) -> BTreeSet<String> {
    subwords(text).into_iter().collect()
}

/// Whether the tokens carry `word` or a simple inflection of it, not any
/// longer token that shares its prefix.
fn carries(tokens: &BTreeSet<String>, word: &str) -> bool {
    let mut variants = vec![
        word.to_owned(),
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

/// Leading whitespace characters of a line.
fn indent_of(line: &str) -> usize {
    line.chars().take_while(|ch| ch.is_whitespace()).count()
}

/// The test cases of a file: Python `def test_*`, JS `it/test(...)`, Rust `fn`
/// under `#[test]`, each cut at its closing line.
fn units(lines: &[String], path: &str) -> Vec<Unit> {
    let mut starts: Vec<(String, usize)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if path.ends_with(".py") {
            if let Some(caps) = PY_CASE.captures(line) {
                starts.push((caps[1].to_owned(), index));
            }
        } else if path.ends_with(".rs") {
            if RS_ATTR.is_match(line) {
                let named = lines
                    .iter()
                    .skip(index + 1)
                    .take(3)
                    .find_map(|next| RS_FN.captures(next));
                if let Some(caps) = named {
                    starts.push((caps[1].to_owned(), index));
                }
            }
        } else if let Some(caps) = JS_CASE.captures(line) {
            let title = (1..=3)
                .find_map(|group| caps.get(group))
                .map_or("", |m| m.as_str());
            starts.push((title.to_owned(), index));
        }
    }
    let python = path.ends_with(".py");
    let mut out = Vec::new();
    for (k, (name, first)) in starts.iter().enumerate() {
        let indent = indent_of(&lines[*first]);
        let mut end = starts
            .get(k + 1)
            .map_or(lines.len() - 1, |next| next.1 - 1);
        for (j, text) in lines.iter().enumerate().take(end + 1).skip(first + 1) {
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
        while end > *first && lines[end].trim().is_empty() {
            end -= 1;
        }
        out.push((name.clone(), first + 1, (end + 1).min(first + UNIT_CAP)));
    }
    out
}

/// The line ranges (`path:a-b`) the brief already quotes, per path.
fn quoted_ranges(brief: &str) -> BTreeMap<&str, Vec<(usize, usize)>> {
    let mut got: BTreeMap<&str, Vec<(usize, usize)>> = BTreeMap::new();
    for caps in QUOTED.captures_iter(brief) {
        let (Some(path), Ok(first), Ok(last)) = (
            caps.get(1),
            caps[2].parse::<usize>(),
            caps[3].parse::<usize>(),
        ) else {
            continue;
        };
        got.entry(path.as_str()).or_default().push((first, last));
    }
    got
}

/// A file of cases: its lines and its cases.
struct Parsed {
    lines: Vec<String>,
    cases: Vec<Unit>,
}

impl Parsed {
    /// Lines `first..=last` (1-based) as one string.
    fn text(&self, first: usize, last: usize) -> String {
        self.lines
            .get(first - 1..last)
            .map_or_else(String::new, |lines| lines.join(" "))
    }
}

/// A case that may be quoted, with its score and its file's rank.
struct Candidate<'a> {
    score: f64,
    rank: usize,
    path: &'a str,
    first: usize,
    last: usize,
    name: &'a str,
}

fn run(ctx: &Ctx) -> String {
    let words = terms(ctx.prompt);
    if words.is_empty() {
        return String::new();
    }
    let Some(tracked) = ctx.files() else {
        return String::new();
    };
    let blocks = quotes(ctx, tracked, &words);
    if blocks.is_empty() {
        String::new()
    } else {
        blocks.join("\n") + "\n"
    }
}

/// Whether the path is a test file of a language whose cases are read.
fn is_test_path(path: &str) -> bool {
    CODE.iter().any(|ext| path.ends_with(ext)) && TEST_PATH.is_match(path)
}

/// The files whose cases compete, best pool first: tests the brief names, then
/// those whose path names the subject, then those whose text carries it.
fn pool(ctx: &Ctx, tracked: &[String], words: &[String]) -> Vec<String> {
    let tests: BTreeSet<&str> = tracked
        .iter()
        .map(String::as_str)
        .filter(|path| is_test_path(path))
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
    let flats: BTreeMap<&str, String> = tests.union(&rust).map(|path| (*path, flat(path))).collect();
    // A word that names more than a quarter of the test files (the word for
    // "test" itself) names no subject.
    let pathy: Vec<&str> = words
        .iter()
        .map(String::as_str)
        .filter(|word| tests.iter().filter(|path| flats[*path].contains(*word)).count() * 4 <= tests.len().max(1))
        .collect();
    let names_subject = |path: &str| pathy.iter().any(|word| flats[path].contains(*word));

    let mut mentioned: Vec<&str> = Vec::new();
    for found in MENTIONED.find_iter(ctx.brief) {
        if !mentioned.contains(&found.as_str()) {
            mentioned.push(found.as_str());
        }
    }
    // Rust keeps its tests inline (`#[cfg(test)]`), so its named or
    // subject-named sources count when they hold a `#[test]`.
    let mut pool: Vec<String> = mentioned
        .into_iter()
        .filter(|path| tests.contains(path) || rust.contains(path))
        .map(ToString::to_string)
        .collect();
    pool.extend(
        tests
            .union(&rust)
            .filter(|path| names_subject(path))
            .map(ToString::to_string),
    );
    pool.extend(carrying(ctx, &tests, &pathy));
    pool
}

/// Test files whose text carries the most subject-word lines, most first;
/// none when time runs out, so a late answer is never a partial ranking.
fn carrying(ctx: &Ctx, tests: &BTreeSet<&str>, words: &[&str]) -> Vec<String> {
    if tests.is_empty() || words.is_empty() {
        return Vec::new();
    }
    let mut counts: Vec<(usize, &str)> = Vec::new();
    for path in tests {
        if ctx.timed_out() {
            return Vec::new();
        }
        let Some(text) = ctx.read(path, MAX_BYTES) else {
            continue;
        };
        let lowered = text.to_lowercase();
        let lines = lowered
            .lines()
            .filter(|line| words.iter().any(|word| line.contains(word)))
            .count();
        if lines > 0 {
            counts.push((lines, *path));
        }
    }
    counts.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    counts.into_iter().map(|(_, path)| path.to_owned()).collect()
}

/// The blocks of the cases that carry the subject, best first.
fn quotes(ctx: &Ctx, tracked: &[String], words: &[String]) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    let mut parsed: BTreeMap<String, Parsed> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for path in pool(ctx, tracked, words) {
        if ctx.timed_out() {
            break;
        }
        if !seen.insert(path.clone()) {
            continue;
        }
        let is_test = is_test_path(&path);
        let Some(text) = ctx.read(&path, MAX_BYTES) else {
            continue;
        };
        if is_test || text.contains("#[test]") {
            let lines: Vec<String> = text.lines().map(ToString::to_string).collect();
            let cases = units(&lines, &path);
            parsed.insert(path.clone(), Parsed { lines, cases });
            files.push(path);
        }
        if files.len() == MAX_FILES {
            break;
        }
    }

    let have = quoted_ranges(ctx.brief);
    let option = requested_option(ctx.prompt);
    let mut df: BTreeMap<&str, usize> = words.iter().map(|word| (word.as_str(), 0)).collect();
    let mut total = 0_usize;
    for file in parsed.values() {
        for (_, first, last) in &file.cases {
            total += 1;
            let case = tokens(&file.text(*first, *last));
            for word in words {
                if carries(&case, word)
                    && let Some(count) = df.get_mut(word.as_str())
                {
                    *count += 1;
                }
            }
        }
    }
    // Weight by rarity among the cases: a word every case carries weighs
    // almost nothing, one no case carries nothing.
    let all = u32::try_from(total.max(1)).map_or(f64::from(u32::MAX), f64::from);
    let idf = |word: &str| match df.get(word).copied().unwrap_or(0) {
        0 => 0.0,
        count => ((all + 1.0) / f64::from(u32::try_from(count).unwrap_or(u32::MAX))).ln(),
    };

    let mut candidates: Vec<Candidate> = Vec::new();
    for (rank, path) in files.iter().enumerate() {
        let file = &parsed[path];
        for (name, first, last) in &file.cases {
            if have
                .get(path.as_str())
                .is_some_and(|ranges| ranges.iter().any(|(a, b)| a <= first && last <= b))
            {
                continue;
            }
            let text = file.text(*first, *last);
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
            if score > 0.0 {
                candidates.push(Candidate {
                    score,
                    rank,
                    path,
                    first: *first,
                    last: *last,
                    name,
                });
            }
        }
    }
    candidates.sort_by(|x, y| {
        y.score
            .total_cmp(&x.score)
            .then(x.rank.cmp(&y.rank))
            .then(x.path.cmp(y.path))
            .then(x.first.cmp(&y.first))
            .then(x.last.cmp(&y.last))
            .then(x.name.cmp(y.name))
    });
    if option.is_some() {
        let titled = |c: &Candidate| exact_option_case(c.name, option.as_deref(), "");
        if candidates.iter().any(titled) {
            candidates.retain(titled);
        } else {
            candidates.retain(|c| {
                let text = parsed[c.path].text(c.first, c.last);
                exact_option_case(c.name, option.as_deref(), &text)
            });
        }
    }
    // The subject's tests, not every test that says "prompt".
    if let Some(best) = candidates.first().map(|c| c.score) {
        candidates.retain(|c| c.score >= 0.5 * best);
    }

    let mut blocks = Vec::new();
    let mut size = 0;
    for c in candidates {
        let file = &parsed[c.path];
        let name: String = c.name.chars().take(NAME_CHARS).collect();
        let mut block = vec![format!("### {}:{}-{} — test {name}", c.path, c.first, c.last)];
        for k in c.first..=c.last {
            let line: String = file
                .lines
                .get(k - 1)
                .map_or_else(String::new, |line| line.chars().take(LINE_CHARS).collect());
            block.push(format!("{k}| {line}"));
        }
        let text = block.join("\n");
        let cost = text.chars().count() + 1;
        if size + cost > OUT_CAP {
            continue;
        }
        size += cost;
        blocks.push(text);
    }
    blocks
}

#[cfg(test)]
mod tests;
