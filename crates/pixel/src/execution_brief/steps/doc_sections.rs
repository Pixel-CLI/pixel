// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `doc-sections`: quote the Markdown and reStructuredText sections whose
//! heading names an explanatory question's words (issues #906, #1200, #1234).
//!
//! Ported from `probes/doc-sections/probe`. Source navigation is left to the
//! code packs: a brief of kind `lookup`, `tests`, `impact` or no kind never
//! gets documentation, nor does one of another kind than `overview` and
//! `config` whose prompt names a constant, callers, callees or a snake_case
//! identifier. Sections are ranked by the question words their heading and
//! first paragraph carry, each weighed by how few sections carry it; the top
//! two are quoted (at most 120 lines each), skipping the rows the brief
//! already quotes, and an excerpt that stops short says where the section
//! goes on. Read-only and deterministic (score, path, line).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use pixel_plugin::ProbeMode;
use regex::Regex;

use super::text::{splitlines, stem_ing_es_s};
use super::{Ctx, Step};

pub(super) const STEP: Step = Step {
    name: "doc-sections",
    when_kinds: &[],
    skip_kinds: &["review", "history", "lookup", "tests", "impact"],
    when_prompt: None,
    when_gate_closed: false,
    budget_ms: 1200,
    mode: ProbeMode::Append,
    run,
};

const STOP: &[&str] = &[
    "what", "where", "which", "that", "this", "with", "from", "does", "the", "and", "how", "is",
    "a", "of", "it", "defined", "define", "number", "returns", "return", "set", "get", "use",
    "used", "make", "could", "there", "keep", "keeps", "change", "changes", "behaves", "behave",
    "when", "should", "would", "about", "have", "for", "its", "are", "not", "you", "can", "any",
    "all", "but", "was", "has", "our", "your", "into", "than",
];
const GENERIC: &[&str] = &[
    "file", "files", "code", "line", "thing", "error", "work", "works", "time", "name", "value",
];
const MAX_FILES: usize = 60;
const MAX_BYTES: u64 = 400_000;
const MIN_WORDS: usize = 2;
const SHOWN: usize = 2;
const WINDOW: usize = 40;
const BODY_LINES: usize = 80;
const LINE_CHARS: usize = 160;

static SKIP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(^|/)(CHANGELOG|HISTORY|LICENSE|NOTICE|CODE_OF_CONDUCT|node_modules|vendor|website|bench)",
    )
    .expect("the skipped-path pattern is a literal")
});
static SOURCE_INTENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:constants?|callers?|callees?)\b|\b[a-z][a-z0-9]*(?:_[a-z0-9]+)+\b")
        .expect("the source-intent pattern is a literal")
});
static LETTERS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z]{3,}").expect("the word pattern is a literal"));
static MARKDOWN_HEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^#{1,4} \S").expect("the heading pattern is a literal"));
static QUOTED_HEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^### ([\w@+./-]+):(\d+)(?:-(\d+))?(?:[ —:]|$)")
        .expect("the quoted-header pattern is a literal")
});
static ROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+)\|").expect("the row pattern is a literal"));

/// One heading section of a documentation file.
struct Section {
    file: usize,
    start: usize,
    end: usize,
    title: String,
    words: BTreeSet<String>,
}

fn run(ctx: &Ctx) -> String {
    if source_question(ctx.kind, ctx.prompt) {
        return String::new();
    }
    let want = words(ctx.prompt);
    if want.len() < MIN_WORDS {
        return String::new();
    }
    let Some(tracked) = ctx.files() else {
        return String::new();
    };
    let mut paths: Vec<&str> = tracked
        .iter()
        .map(String::as_str)
        .filter(|path| [".md", ".mdx", ".rst"].iter().any(|ext| path.ends_with(ext)))
        .filter(|path| !SKIP.is_match(path) && path.matches('/').count() <= 2)
        .collect();
    paths.sort_unstable();
    paths.truncate(MAX_FILES);

    let mut texts: Vec<Vec<String>> = Vec::new();
    let mut docs: Vec<Section> = Vec::new();
    let mut names: Vec<&str> = Vec::new();
    for path in paths {
        if ctx.timed_out() {
            return String::new();
        }
        let Some(text) = ctx.read(path, MAX_BYTES) else {
            continue;
        };
        let lines: Vec<String> = splitlines(&text).into_iter().map(ToString::to_string).collect();
        for (start, end, title) in sections(&lines, path) {
            let body = lines
                .iter()
                .skip(start)
                .take(12)
                .filter(|line| !line.trim().is_empty())
                .take(4);
            let head = std::iter::once(&title).chain(body).cloned().collect::<Vec<_>>().join(" ");
            docs.push(Section {
                file: texts.len(),
                start,
                end,
                title,
                words: words(&head),
            });
        }
        texts.push(lines);
        names.push(path);
    }
    // A word that heads most sections (the project's own name) says little:
    // weigh by how few sections carry it.
    let mut df: BTreeMap<&str, usize> = BTreeMap::new();
    for doc in &docs {
        for word in &doc.words {
            *df.entry(word).or_default() += 1;
        }
    }
    let weight: BTreeMap<&str, f64> = want
        .iter()
        .filter_map(|word| {
            let count = *df.get(word.as_str())?;
            Some((word.as_str(), ((docs.len() + 1) as f64 / count as f64).ln()))
        })
        .collect();
    let mut found: Vec<(f64, &str, &Section)> = Vec::new();
    for doc in &docs {
        let hits: Vec<&String> = want.intersection(&doc.words).collect();
        if hits.len() >= MIN_WORDS {
            let score: f64 = hits.iter().map(|word| weight[word.as_str()]).sum();
            found.push((-rounded(score), names[doc.file], doc));
        }
    }
    found.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(b.1))
            .then_with(|| a.2.start.cmp(&b.2.start))
            .then_with(|| a.2.end.cmp(&b.2.end))
            .then_with(|| a.2.title.cmp(&b.2.title))
    });

    let mut out = String::new();
    let mut held = quoted_lines(ctx.brief);
    for (_, path, doc) in found.into_iter().take(SHOWN) {
        let lines = &texts[doc.file];
        let last = lines.len().min(doc.end).min(doc.start + BODY_LINES + WINDOW);
        let held_here = held.entry(path.to_owned()).or_default();
        let ranges = novel_ranges(doc.start, last, held_here);
        for &(low, high) in &ranges {
            out.push_str(&format!(
                "### {path}:{low}-{high} — {} (documentation section)\n",
                doc.title
            ));
            for number in low..=high {
                let line = &lines[number - 1];
                if line.chars().count() <= LINE_CHARS {
                    out.push_str(&format!("{number}| {line}\n"));
                } else {
                    let cut: String = line.chars().take(LINE_CHARS).collect();
                    out.push_str(&format!("{number}| {cut} … [truncated]\n"));
                }
            }
            held_here.extend(low..=high);
        }
        if !ranges.is_empty() && last < doc.end {
            out.push_str(&format!(
                "excerpt truncated: section continues after line {last} (ends at {})\n",
                doc.end
            ));
        }
    }
    out
}

/// Python's `round(score, 3)`: the nearest value with three decimals.
fn rounded(score: f64) -> f64 {
    format!("{score:.3}").parse().unwrap_or(score)
}

/// A source-navigation intent vetoes documentation; the weak kinds can still
/// ask about code.
fn source_question(kind: Option<&str>, prompt: &str) -> bool {
    match kind {
        None | Some("lookup" | "tests" | "impact") => true,
        Some("overview" | "config") => false,
        Some(_) => SOURCE_INTENT.is_match(prompt),
    }
}

/// The stemmed words of three letters or more that are not function words.
fn words(text: &str) -> BTreeSet<String> {
    LETTERS
        .find_iter(text)
        .map(|found| found.as_str())
        .filter(|word| {
            let lower = word.to_lowercase();
            !STOP.contains(&lower.as_str()) && !GENERIC.contains(&lower.as_str())
        })
        .map(stem_ing_es_s)
        .collect()
}

/// `^([=\-~^*+#"'`:.])\1{2,}\s*$`: one punctuation character three times or
/// more, then nothing but blanks (the underline of a reStructuredText title).
fn is_rst_rule(line: &str) -> bool {
    let mut chars = line.chars();
    let Some(first) = chars.next().filter(|ch| "=-~^*+#\"'`:.".contains(*ch)) else {
        return false;
    };
    let rest = chars.as_str();
    let run = rest.chars().take_while(|ch| *ch == first).count();
    run >= 2 && rest[run * first.len_utf8()..].trim().is_empty()
}

/// `(start, end, heading)` (1-based, inclusive) of the heading sections of a
/// Markdown file, or of a reStructuredText one, whose titles are underlined
/// (`Title`, then `=====` at least as long).
fn sections(lines: &[String], path: &str) -> Vec<(usize, usize, String)> {
    let heads: Vec<(usize, String)> = if path.ends_with(".rst") {
        lines
            .windows(2)
            .enumerate()
            .filter(|(_, pair)| {
                !pair[0].trim().is_empty()
                    && !is_rst_rule(&pair[0])
                    && is_rst_rule(&pair[1])
                    && pair[1].trim_end().chars().count() >= pair[0].trim_end().chars().count()
            })
            .map(|(index, pair)| (index + 1, pair[0].trim().to_owned()))
            .collect()
    } else {
        lines
            .iter()
            .enumerate()
            .filter(|(_, line)| MARKDOWN_HEAD.is_match(line))
            .map(|(index, line)| {
                (
                    index + 1,
                    line.trim_start_matches(['#', ' ']).trim().to_owned(),
                )
            })
            .collect()
    };
    heads
        .iter()
        .enumerate()
        .map(|(at, (start, title))| {
            let end = heads.get(at + 1).map_or(lines.len(), |next| next.0 - 1);
            (*start, end, title.clone())
        })
        .collect()
}

/// The numbered source rows the brief already quotes, by file.
fn quoted_lines(brief: &str) -> BTreeMap<String, BTreeSet<usize>> {
    let mut held: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    let mut current: Option<regex::Captures> = None;
    for line in splitlines(brief) {
        if line.starts_with("### ") {
            current = QUOTED_HEAD.captures(line);
        } else if let Some(head) = &current
            && let Some(row) = ROW.captures(line)
        {
            let number = row[1].parse().unwrap_or(usize::MAX);
            let low: usize = head[2].parse().unwrap_or(usize::MAX);
            let high: usize = head.get(3).map_or(low, |end| end.as_str().parse().unwrap_or(usize::MAX));
            if (low..=high).contains(&number) {
                held.entry(head[1].to_owned()).or_default().insert(number);
            }
        }
    }
    held
}

/// The consecutive ranges of `first..=last` not already held; an excerpt
/// stops before a held row.
fn novel_ranges(first: usize, last: usize, held: &BTreeSet<usize>) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start: Option<usize> = None;
    for number in first..=last + 1 {
        if number <= last && !held.contains(&number) {
            start.get_or_insert(number);
        } else if let Some(from) = start.take() {
            ranges.push((from, number - 1));
        }
    }
    ranges
}

#[cfg(test)]
mod tests;
