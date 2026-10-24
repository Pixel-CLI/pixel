// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! `name-files` (`specs/brief.md`, "Recipes"): three locators the brief lacks
//! for a question about the parts of the code.
//!
//! * Adjacent prompt words joined into a code name ("smart case" becomes
//!   `SmartCase`, `smart_case`), found with one pattern search over the text
//!   index and shown with its definition.
//! * A source file whose stem is a word of the prompt ("config" names
//!   `flags/config.rs`), shown whole when short — but only beside the files
//!   the brief already points at.
//! * The modules a question about "parts", "functions", "files" or what
//!   "depends" on them names: the source of a file whose name shares two words
//!   with the prompt, and the lines that import it.
//!
//! The step runs only on a prompt that says "part", "function", "module",
//! "file" or "depend": the scope it was validated on. Read-only and
//! deterministic.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::chain::SEARCH_ROWS;
use crate::region_pack::{Block, number_lines, range_block};
use crate::routes::QuestionKind;

use super::{Ctx, Outcome, Step};

const STOP: &[&str] = &[
    "what", "where", "which", "that", "this", "with", "from", "does", "the", "and", "how", "is",
    "a", "of", "it", "defined", "define", "number", "returns", "return", "set", "get", "use",
    "used", "make", "could", "there",
];
const GENERIC: &[&str] = &[
    "for", "to", "in", "on", "my", "its", "keep", "file", "line", "code", "thing", "error",
    "change", "nothing", "make", "read", "ignored", "get", "files", "lines", "work", "works",
    "happens", "nothing", "time",
];
const SOURCE_EXTENSIONS: &[&str] = &["rs", "ts", "tsx", "js", "py", "go", "rb"];
const FILE_LINES: usize = 180;
const DEFINITION_LINES: u64 = 60;
const MAX_FILE_LINES: usize = 220;
const MODULE_FILE_LINES: usize = 24;
const MAX_MODULES: usize = 2;
const MAX_MODULE_REFS: usize = 8;
const MODULE_SCAN_ROWS: usize = 400;
const MODULE_OUT_CHARS: usize = 3_000;
const MODULE_FILE_MAX_BYTES: u64 = 250_000;
const LINE_CHARS: usize = 160;
/// Candidates examined for the module slots: one that cannot be read does not
/// take a slot.
const MODULE_ATTEMPTS: usize = 6;

static MODULE_INTENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:parts?|functions?|modules?|files?|depend(?:s|ed|ing)?)\b")
        .expect("the module-intent pattern is a literal")
});
static MODULE_REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:mod|use|import|from|require|include)\b|::")
        .expect("the module-reference pattern is a literal")
});
static TEST_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|/)(?:tests?|__tests__|spec)(?:/|$)|(?:^|/)(?:test|spec)[^/]*\.")
        .expect("the test-path pattern is a literal")
});

fn content(word: &str) -> bool {
    !STOP.contains(&word) && !GENERIC.contains(&word)
}

fn is_source(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, ext)| SOURCE_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
}

/// The file name of `path` without its extension.
fn stem_of(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// The subwords of an identifier or phrase, lowercased, with ordinary English
/// plurals normalised the same way everywhere.
fn identifier_parts(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for mut word in super::subwords(text) {
        let long = word.chars().count();
        if long > 4 && word.ends_with("ies") {
            word = format!("{}y", &word[..word.len() - 3]);
        } else if long > 5
            && ["ches", "shes", "sses", "xes", "zes"]
                .iter()
                .any(|suffix| word.ends_with(suffix))
        {
            word.truncate(word.len() - 2);
        } else if long > 3 && word.ends_with('s') && !word.ends_with("ss") {
            word.truncate(word.len() - 1);
        }
        if word.chars().count() > 2 {
            out.push(word);
        }
    }
    out
}

/// A name the adjacent words of the prompt may spell, in three spellings.
struct Compound {
    camel: String,
    snake: String,
    phrase: String,
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or(String::new(), |first| {
        first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect()
    })
}

fn compounds(prompt: &str) -> Vec<Compound> {
    let lower = prompt.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|ch: char| !ch.is_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect();
    tokens
        .windows(2)
        .filter(|pair| pair.iter().all(|word| word.len() >= 3 && content(word)))
        .map(|pair| Compound {
            camel: format!("{}{}", capitalize(pair[0]), capitalize(pair[1])),
            snake: format!("{}_{}", pair[0], pair[1]),
            phrase: format!("{} {}", pair[0], pair[1]),
        })
        .collect()
}

/// The leading directories two paths share.
fn common_dirs(a: &str, b: &str) -> usize {
    let dirs = |path: &str| -> Vec<String> {
        let mut parts: Vec<String> = path.split('/').map(ToString::to_string).collect();
        parts.pop();
        parts
    };
    dirs(a)
        .iter()
        .zip(dirs(b).iter())
        .take_while(|(x, y)| x == y)
        .count()
}

fn clip(line: &str) -> String {
    line.chars().take(LINE_CHARS).collect()
}

/// Lines `lo..lo+count` of `lines`, numbered, each cut to [`LINE_CHARS`].
fn numbered(lines: &[&str], lo: usize, count: usize) -> Vec<String> {
    let hi = lines.len().min(lo + count - 1);
    (lo..=hi)
        .map(|number| format!("{number}| {}", clip(lines[number - 1])))
        .collect()
}

pub(super) fn run(ctx: &Ctx, out: &mut Outcome) {
    if !ctx.wants(
        Step::NameFiles,
        &[
            QuestionKind::Overview,
            QuestionKind::Review,
            QuestionKind::Flow,
            QuestionKind::History,
        ],
    ) || !MODULE_INTENT.is_match(ctx.typed)
    {
        return;
    }
    let _timer = ctx.timer(Step::NameFiles);
    let Some(files) = ctx.files() else {
        return;
    };
    let mut found = definitions(ctx);
    found.extend(modules(ctx, files));
    found.extend(named_file(ctx, files));
    out.rescuing |= !found.is_empty();
    out.lead.extend(found);
}

/// The definition of each code name the prompt's adjacent words spell.
fn definitions(ctx: &Ctx) -> Vec<Block> {
    let pairs = compounds(ctx.typed);
    if pairs.is_empty() {
        return Vec::new();
    }
    let mut spellings: Vec<(usize, &str)> = Vec::new();
    for (index, pair) in pairs.iter().enumerate() {
        spellings.push((index, &pair.camel));
        spellings.push((index, &pair.snake));
    }
    let mut ordered: Vec<&str> = spellings.iter().map(|(_, word)| *word).collect();
    ordered.sort_by_key(|word| std::cmp::Reverse(word.len()));
    ordered.dedup();
    let alternation: Vec<String> = ordered.iter().map(|word| regex::escape(word)).collect();
    let pattern = format!(
        r"(struct|class|fn|def|const|enum|interface|type)\s+({})\b",
        alternation.join("|")
    );
    let Ok(rows) = ctx.evidence.grep(&pattern, SEARCH_ROWS, ctx.deadline) else {
        return Vec::new();
    };
    let mut hits: BTreeMap<usize, Vec<(String, u64)>> = BTreeMap::new();
    for row in rows.hits.iter().filter(|row| is_source(&row.path)) {
        let text = row.text.as_deref().unwrap_or_default();
        let Some(word) = ordered.iter().find(|word| has_word(text, word)) else {
            continue;
        };
        for (index, spelling) in &spellings {
            if spelling == word {
                hits.entry(*index)
                    .or_default()
                    .push((row.path.clone(), row.line));
            }
        }
    }
    let mut blocks = Vec::new();
    for (index, sites) in hits {
        let pair = &pairs[index];
        for (path, line) in sites.into_iter().take(2) {
            let Some(text) = ctx.read(&path) else {
                continue;
            };
            let lines: Vec<String> = text.lines().map(ToString::to_string).collect();
            let first = line.saturating_sub(3).max(1);
            let last = (first + DEFINITION_LINES - 1).min(lines.len() as u64);
            if let Some(block) = range_block(
                format!(
                    "{} definition (named by the prompt: '{}')",
                    pair.camel, pair.phrase
                ),
                &path,
                &lines,
                first,
                last,
            ) {
                blocks.push(block);
            }
        }
    }
    blocks
}

/// Whether `word` occurs in `text` as a whole word.
fn has_word(text: &str, word: &str) -> bool {
    let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
    text.match_indices(word).any(|(start, found)| {
        text[..start]
            .chars()
            .next_back()
            .is_none_or(|ch| !is_word(ch))
            && text[start + found.len()..]
                .chars()
                .next()
                .is_none_or(|ch| !is_word(ch))
    })
}

/// The source of a file whose name shares two words with a question about
/// modules, and the lines that import it.
fn modules(ctx: &Ctx, files: &[String]) -> Vec<Block> {
    let prompt_words: BTreeSet<String> = identifier_parts(ctx.typed)
        .into_iter()
        .filter(|word| !STOP.contains(&word.as_str()))
        .collect();
    if prompt_words.len() < 2 {
        return Vec::new();
    }
    let mut candidates: Vec<(std::cmp::Reverse<usize>, usize, &str, &str)> = files
        .iter()
        .filter(|path| is_source(path) && !TEST_PATH.is_match(path))
        .filter_map(|path| {
            let stem = stem_of(path);
            let parts: BTreeSet<String> = identifier_parts(stem).into_iter().collect();
            let shared = parts.intersection(&prompt_words).count();
            (parts.len() >= 2 && shared >= 2).then_some((
                std::cmp::Reverse(shared),
                parts.len() - shared,
                path.as_str(),
                stem,
            ))
        })
        .collect();
    candidates.sort();
    let mut chosen: Vec<(&str, &str, Vec<String>)> = Vec::new();
    for (_, _, path, stem) in candidates.into_iter().take(MODULE_ATTEMPTS) {
        if chosen.len() == MAX_MODULES {
            break;
        }
        if let Some(text) = ctx.read_up_to(path, MODULE_FILE_MAX_BYTES) {
            chosen.push((path, stem, text.lines().map(ToString::to_string).collect()));
        }
    }
    if chosen.is_empty() {
        return Vec::new();
    }
    let names: Vec<&str> = chosen.iter().map(|(_, stem, _)| *stem).collect();
    let refs = module_references(ctx, files, &names);

    let mut out: Vec<Block> = Vec::new();
    let mut used = 0;
    let mut add = |block: Block| {
        let cost = block.text().len() + 1;
        if used + cost <= MODULE_OUT_CHARS {
            used += cost;
            out.push(block);
        }
    };
    for (path, _, lines) in &chosen {
        let last = lines.len().min(MODULE_FILE_LINES);
        let shown: Vec<String> = lines[..last].iter().map(|line| clip(line)).collect();
        add(Block::new(
            format!("{path}:1-{last} — module named by the prompt"),
            number_lines(1, &shown),
        ));
    }
    for (path, number, name) in refs {
        let Some(text) = ctx.read_up_to(&path, MODULE_FILE_MAX_BYTES) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        if number == 0 || number > lines.len() {
            continue;
        }
        add(Block::new(
            format!("{path}:{number}-{number} — source reference to {name}"),
            vec![format!("{number}| {}", clip(lines[number - 1]))],
        ));
    }
    out
}

/// The first line per source file that imports one of `names`, in path order,
/// tests left out.
fn module_references(ctx: &Ctx, files: &[String], names: &[&str]) -> Vec<(String, usize, String)> {
    let alternation: Vec<String> = names.iter().map(|name| regex::escape(name)).collect();
    let pattern = alternation.join("|");
    let Ok(rows) = ctx.evidence.grep(&pattern, MODULE_SCAN_ROWS, ctx.deadline) else {
        return Vec::new();
    };
    let tracked: BTreeSet<&str> = files.iter().map(String::as_str).collect();
    let mut refs: Vec<(String, usize, String)> = Vec::new();
    for row in &rows.hits {
        let Some(text) = row.text.as_deref() else {
            continue;
        };
        let Some(name) = names.iter().find(|name| text.contains(**name)) else {
            continue;
        };
        if is_source(&row.path)
            && tracked.contains(row.path.as_str())
            && !TEST_PATH.is_match(&row.path)
            && MODULE_REFERENCE.is_match(text)
            && refs.iter().all(|(path, ..)| *path != row.path)
        {
            refs.push((row.path.clone(), row.line as usize, (*name).to_string()));
        }
    }
    refs.sort();
    refs.truncate(MAX_MODULE_REFS);
    refs
}

/// A short source file named by a word of the prompt, beside the files the
/// brief already shows.
fn named_file(ctx: &Ctx, files: &[String]) -> Option<Block> {
    let lower = ctx.typed.to_lowercase();
    let stems: BTreeSet<&str> = lower
        .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .filter(|token| token.len() >= 4 && token.chars().all(|ch| ch.is_ascii_lowercase()))
        .filter(|token| content(token))
        .collect();
    let mut candidates: Vec<(std::cmp::Reverse<usize>, &str)> = files
        .iter()
        .filter(|path| is_source(path))
        .filter(|path| stems.contains(stem_of(path).to_lowercase().as_str()))
        .filter(|path| !path.to_lowercase().contains("test"))
        .filter_map(|path| {
            let shared = ctx
                .near
                .iter()
                .map(|other| common_dirs(path, other))
                .max()?;
            (shared > 0).then_some((std::cmp::Reverse(shared), path.as_str()))
        })
        .collect();
    candidates.sort();
    let (_, name) = candidates.first()?;
    let text = ctx.read(name)?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > MAX_FILE_LINES {
        return None;
    }
    let last = lines.len().min(FILE_LINES);
    Some(Block::new(
        format!(
            "{name}:1-{last} — file named '{}' (a word of the prompt)",
            stem_of(name)
        ),
        numbered(&lines, 1, FILE_LINES),
    ))
}

#[cfg(test)]
mod tests;
