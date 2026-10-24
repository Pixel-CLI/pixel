// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `doc-sections` (`specs/brief.md`, "Recipes"): the documentation sections
//! whose heading names the question's words.
//!
//! An explanatory question ("how does X keep Y", "why does Z behave like
//! that") is often answered by a section of the Markdown or reStructuredText
//! docs. Source navigation is left to the code packs: a lookup, a tests
//! question or a bugfix never gets a doc section, nor does a prompt that names
//! a constant, a caller or a `snake_case` identifier. Sections are ranked by
//! the question's words their heading and opening lines carry, each word
//! weighed by how few sections hold it (the project's own name heads most of
//! them and says little); the top two are quoted, truncation marked. Read-only
//! and deterministic: ties break on path, then line.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::region_pack::Block;
use crate::routes::QuestionKind;

use super::{Ctx, Outcome, Step, stem};

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
/// Lines after a heading whose words describe the section.
const OPENING_LINES: usize = 12;
const OPENING_NON_EMPTY: usize = 4;

static SKIP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(^|/)(CHANGELOG|HISTORY|LICENSE|NOTICE|CODE_OF_CONDUCT|node_modules|vendor|website|bench)")
        .expect("the skipped-docs pattern is a literal")
});
static WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z]{3,}").expect("the word pattern is a literal"));
static SOURCE_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:constants?|callers?|callees?)\b|\b[a-z][a-z0-9]*(?:_[a-z0-9]+)+\b")
        .expect("the source-question pattern is a literal")
});
static MARKDOWN_HEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^#{1,4} \S").expect("the heading pattern is a literal"));

/// The stems of the content words of `text`.
fn words(text: &str) -> BTreeSet<String> {
    WORD.find_iter(text)
        .map(|found| found.as_str())
        .filter(|word| {
            let lower = word.to_lowercase();
            !STOP.contains(&lower.as_str()) && !GENERIC.contains(&lower.as_str())
        })
        .map(stem)
        .collect()
}

/// A reStructuredText underline: three or more of one punctuation character.
fn is_rst_rule(line: &str) -> bool {
    let line = line.trim_end();
    let mut chars = line.chars();
    chars.next().is_some_and(|first| {
        "=-~^*+#\"'`:.".contains(first) && line.chars().count() >= 3 && chars.all(|ch| ch == first)
    })
}

/// `(start, end, heading)` (1-based, inclusive) of the heading sections of a
/// Markdown file, or of a reStructuredText one, whose titles are underlined
/// (`Title` then `=====` at least as long).
fn sections(lines: &[&str], rst: bool) -> Vec<(usize, usize, String)> {
    let heads: Vec<(usize, String)> = if rst {
        lines
            .windows(2)
            .enumerate()
            .filter(|(_, pair)| {
                !pair[0].trim().is_empty()
                    && !is_rst_rule(pair[0])
                    && is_rst_rule(pair[1])
                    && pair[1].trim_end().chars().count() >= pair[0].trim_end().chars().count()
            })
            .map(|(index, pair)| (index + 1, pair[0].trim().to_string()))
            .collect()
    } else {
        lines
            .iter()
            .enumerate()
            .filter(|(_, line)| MARKDOWN_HEAD.is_match(line))
            .map(|(index, line)| {
                (
                    index + 1,
                    line.trim_start_matches(['#', ' ']).trim().to_string(),
                )
            })
            .collect()
    };
    heads
        .iter()
        .enumerate()
        .map(|(k, (start, title))| {
            let end = heads.get(k + 1).map_or(lines.len(), |next| next.0 - 1);
            (*start, end, title.clone())
        })
        .collect()
}

/// A source-navigation intent vetoes docs; kinds that can still ask about
/// code are checked on the prompt's own words.
fn source_question(ctx: &Ctx) -> bool {
    match ctx.kind {
        QuestionKind::Overview | QuestionKind::Config => false,
        _ => SOURCE_WORDS.is_match(ctx.typed),
    }
}

pub(super) fn run(ctx: &Ctx, out: &mut Outcome) {
    if !ctx.wants(
        Step::DocSections,
        &[
            QuestionKind::Review,
            QuestionKind::History,
            QuestionKind::Lookup,
            QuestionKind::Tests,
            QuestionKind::Bugfix,
        ],
    ) || source_question(ctx)
    {
        return;
    }
    let want = words(ctx.typed);
    if want.len() < MIN_WORDS {
        return;
    }
    let _timer = ctx.timer(Step::DocSections);
    let Some(files) = ctx.files() else {
        return;
    };
    let docs: Vec<&String> = files
        .iter()
        .filter(|path| {
            [".md", ".mdx", ".rst"]
                .iter()
                .any(|ext| path.ends_with(ext))
        })
        .filter(|path| !SKIP.is_match(path) && path.matches('/').count() <= 2)
        .take(MAX_FILES)
        .collect();
    let mut texts: BTreeMap<&str, String> = BTreeMap::new();
    let mut found: Vec<(&str, usize, usize, String, BTreeSet<String>)> = Vec::new();
    for path in docs {
        if ctx.timed_out() {
            return;
        }
        let Some(text) = ctx.read_up_to(path, MAX_BYTES) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        for (start, end, title) in sections(&lines, path.ends_with(".rst")) {
            // The section's own lines: Pixel's script read the lines after the
            // heading whatever came next, so a parent heading scored on its
            // children's words and took the slot of the child it contained.
            let opening: Vec<&str> = lines
                .iter()
                .take(end)
                .skip(start)
                .take(OPENING_LINES)
                .filter(|line| !line.trim().is_empty())
                .take(OPENING_NON_EMPTY)
                .copied()
                .collect();
            let head = format!("{title} {}", opening.join(" "));
            found.push((path, start, end, title, words(&head)));
        }
        texts.insert(path, text);
    }
    // A word that heads most sections says little: weigh by how few carry it.
    let mut frequency: BTreeMap<&str, usize> = BTreeMap::new();
    for (.., ws) in &found {
        for word in ws {
            *frequency.entry(word).or_default() += 1;
        }
    }
    let total = found.len() as f64;
    let weight = |word: &str| -> f64 {
        frequency
            .get(word)
            .map_or(0.0, |count| ((total + 1.0) / *count as f64).ln())
    };
    let mut ranked: Vec<(i64, &str, usize, usize, &str)> = found
        .iter()
        .filter_map(|(path, start, end, title, ws)| {
            let hits: Vec<&String> = want.intersection(ws).collect();
            (hits.len() >= MIN_WORDS).then(|| {
                let score: f64 = hits.iter().map(|word| weight(word)).sum();
                (
                    -((score * 1000.0).round() as i64),
                    *path,
                    *start,
                    *end,
                    title.as_str(),
                )
            })
        })
        .collect();
    ranked.sort_unstable();
    for (_, path, start, end, title) in ranked.into_iter().take(SHOWN) {
        let Some(text) = texts.get(path) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        let last = lines.len().min(end).min(start + BODY_LINES + WINDOW);
        let mut body: Vec<String> = (start..=last)
            .map(|number| {
                let line = lines[number - 1];
                if line.chars().count() <= LINE_CHARS {
                    format!("{number}| {line}")
                } else {
                    let cut: String = line.chars().take(LINE_CHARS).collect();
                    format!("{number}| {cut} … [truncated]")
                }
            })
            .collect();
        if last < end {
            body.push(format!(
                "excerpt truncated: section continues after line {last} (ends at {end})"
            ));
        }
        out.append.push(Block::new(
            format!("{path}:{start}-{last} — {title} (documentation section)"),
            body,
        ));
    }
}

#[cfg(test)]
mod tests;
