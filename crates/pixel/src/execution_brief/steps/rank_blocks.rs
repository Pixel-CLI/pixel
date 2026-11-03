// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `rank-blocks`: the answer is often in the brief already, buried among blocks
//! that share no word with the question.
//!
//! Ported from `probes/rank-blocks/probe`. Scores each block by the question's
//! words its header names, drops the blocks that share none, moves the rest to
//! the front by score (`drop:` / `first:` directives, which act on the brief's
//! own blocks), and replaces the best constants by the constant with 12 lines
//! of context and where it is used. Says nothing when no block shares a word
//! with the question: it never empties a brief.
//!
//! `rg` is not started: the uses are searched in the tracked files, in the
//! order `rg --sort path` walks them, with its defaults for hidden files
//! (skipped), binary files (skipped) and the `tests` directories the script
//! excluded. A tracked file that `.gitignore` also lists is searched here and
//! not by `rg`, and a file over 4 MiB is not searched here. Lines are those
//! between `\n`, where Python's `splitlines` also broke at form feeds and
//! lone carriage returns, so the numbers of a file that holds one are the
//! brief's, not the script's.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::LazyLock;

use pixel_git::repo_path::is_normal_relative;
use pixel_plugin::ProbeMode;
use regex::Regex;

use super::text::{escape, is_word, stem_ing_es_s};
use super::{Ctx, Step};

pub(super) const STEP: Step = Step {
    name: "rank-blocks",
    when_kinds: &[],
    skip_kinds: &["overview", "review", "flow", "history"],
    when_prompt: None,
    when_gate_closed: false,
    budget_ms: 1200,
    mode: ProbeMode::Lead,
    run,
};

/// Words that name nothing of the question.
const STOP: &[&str] = &[
    "what", "where", "which", "that", "this", "with", "from", "does", "the", "and", "how", "is",
    "a", "of", "it", "defined", "define", "number", "returns", "return", "set", "get", "use",
    "used", "make", "could", "there",
];
/// Best-ranked blocks that are given their context when they are constants.
const CONTEXT_BLOCKS: usize = 3;
/// Lines shown on each side of a constant.
const CONTEXT_LINES: usize = 12;
/// Uses of a name shown at most.
const USES: usize = 4;
/// Matching lines of one file read as uses at most (`rg -m`).
const PER_FILE: usize = 3;
/// Names of one block whose uses are looked for.
const NAMES: usize = 2;
/// Characters of a source line shown.
const SOURCE_CHARS: usize = 160;
/// Characters of a use shown.
const USE_CHARS: usize = 180;
/// Bytes of a file that are read; a longer one is neither quoted nor searched.
const FILE_LIMIT: u64 = 4 * 1024 * 1024;

/// The header of a constant block: `path:lo-hi — NAMES (constant)`.
static CONSTANT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:pixel/)?(\S+?):(\d+)-(\d+) — (.+?) \(constant\)")
        .expect("the constant header pattern is a literal")
});

fn run(ctx: &Ctx) -> String {
    let want = words(ctx.prompt);
    let scored: Vec<(usize, &str)> = ctx
        .brief
        .lines()
        .filter_map(|line| line.strip_prefix("### "))
        .map(|header| {
            let (label, rest) = header.split_once(" — ").unwrap_or((header, ""));
            let score = want.intersection(&words(&format!("{rest} {label}"))).count();
            (score, header)
        })
        .collect();
    let mut keep: Vec<(usize, usize)> = scored
        .iter()
        .enumerate()
        .filter(|(_, (score, _))| *score > 0)
        .map(|(index, (score, _))| (*score, index))
        .collect();
    if keep.is_empty() {
        return String::new();
    }
    keep.sort_by_key(|(score, index)| (Reverse(*score), *index));
    let mut out = String::new();
    for (_, header) in scored.iter().filter(|(score, _)| *score == 0) {
        let _ = writeln!(out, "drop: ^{}$", escape(header));
    }
    for (rank, (_, index)) in keep.iter().enumerate() {
        let header = scored[*index].1;
        let context = if rank < CONTEXT_BLOCKS {
            with_context(ctx, header)
        } else {
            None
        };
        if let Some(lines) = context {
            let _ = writeln!(out, "drop: ^{}$", escape(header));
            out.push_str(&lines.join("\n"));
            out.push('\n');
        } else {
            let _ = writeln!(out, "first: ^{}$", escape(header));
        }
    }
    out
}

/// The parts of the identifiers and words of `text`, the way
/// `re.findall(r"[A-Za-z][a-z]+|[A-Z]+(?![a-z])", text)` cuts them: a capital
/// followed by lowercase letters, or a run of capitals that stops before the
/// capital that starts the next word.
fn parts(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let lowercase_after = |from: usize| {
            from + bytes[from..]
                .iter()
                .take_while(|byte| byte.is_ascii_lowercase())
                .count()
        };
        let starts_word = bytes[at].is_ascii_alphabetic();
        let follows = bytes.get(at + 1).is_some_and(u8::is_ascii_lowercase);
        if starts_word && follows {
            let end = lowercase_after(at + 1);
            found.push(&text[at..end]);
            at = end;
        } else if bytes[at].is_ascii_uppercase() {
            let capitals = at
                + bytes[at..]
                    .iter()
                    .take_while(|byte| byte.is_ascii_uppercase())
                    .count();
            let before_lowercase = bytes.get(capitals).is_some_and(u8::is_ascii_lowercase);
            // `HTTPServer`: the capitals but the last, which starts `Server`.
            let end = if before_lowercase { capitals - 1 } else { capitals };
            found.push(&text[at..end]);
            at = end;
        } else {
            at += 1;
        }
    }
    found
}

/// The stems of the words of `text` that are three letters or more and not
/// function words.
fn words(text: &str) -> HashSet<String> {
    parts(text)
        .into_iter()
        .filter(|part| part.len() > 2 && !STOP.contains(&part.to_lowercase().as_str()))
        .map(stem_ing_es_s)
        .collect()
}

/// The block of a constant header with its source lines and where its names are
/// used; `None` when the header is not a constant or its file cannot be read.
fn with_context(ctx: &Ctx, header: &str) -> Option<Vec<String>> {
    let found = CONSTANT.captures(header)?;
    let path = found.get(1)?.as_str();
    let low: usize = found.get(2)?.as_str().parse().ok()?;
    let high: usize = found.get(3)?.as_str().parse().ok()?;
    let names = found.get(4)?.as_str();
    if !is_normal_relative(path) {
        return None;
    }
    let text = ctx.read(path, FILE_LIMIT)?;
    let source: Vec<&str> = text.lines().collect();
    let first = low.saturating_sub(CONTEXT_LINES).max(1);
    let last = source.len().min(high.saturating_add(CONTEXT_LINES));
    let mut out = vec![format!(
        "### {path}:{first}-{last} — {names} (constant, with context)"
    )];
    for number in first..=last {
        let line: String = source[number - 1].chars().take(SOURCE_CHARS).collect();
        out.push(format!("{number}| {line}"));
    }
    let names: Vec<&str> = names
        .split(',')
        .map(str::trim)
        .take(NAMES)
        .filter(|name| !name.is_empty())
        .collect();
    for (name, uses) in names.iter().zip(uses_of(ctx, &names)) {
        if !uses.is_empty() {
            out.push(format!("uses of {name}:"));
            out.extend(uses);
        }
    }
    Some(out)
}

/// Whether `line` holds `name` as a whole word.
fn has_word(line: &str, name: &str) -> bool {
    line.match_indices(name).any(|(at, _)| {
        let before = line[..at].chars().next_back();
        let after = line[at + name.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

/// Whether the walk of `rg` (hidden entries are skipped) and the script's
/// `--glob '!**/tests/**'` leave `path` out.
fn skipped(path: &str) -> bool {
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        if part.starts_with('.') || (part == "tests" && parts.peek().is_some()) {
            return true;
        }
    }
    false
}

/// For each of `names`: up to four `./path:line:text` lines that use it as a
/// word, but not its declaration (`const NAME`), from the files in the order
/// `rg --sort path` walks them, reading at most three matching lines of a file.
fn uses_of(ctx: &Ctx, names: &[&str]) -> Vec<Vec<String>> {
    let mut found: Vec<Vec<String>> = vec![Vec::new(); names.len()];
    let declarations: Vec<Regex> = names
        .iter()
        .map(|name| {
            Regex::new(&format!(r"const {}\b", regex::escape(name)))
                .unwrap_or_else(|_| Regex::new("$^").expect("an empty pattern is valid"))
        })
        .collect();
    let Some(files) = ctx.files() else {
        return found;
    };
    let mut files: Vec<&String> = files.iter().filter(|path| !skipped(path)).collect();
    files.sort_by(|a, b| Path::new(a).cmp(Path::new(b)));
    for path in files {
        if ctx.timed_out() || found.iter().all(|uses| uses.len() >= USES) {
            break;
        }
        let wanted: Vec<usize> = (0..names.len())
            .filter(|index| found[*index].len() < USES)
            .collect();
        let Some(text) = ctx.read(path, FILE_LIMIT) else {
            continue;
        };
        if text.contains('\0') {
            continue;
        }
        for index in wanted {
            let mut matched = 0;
            for (number, line) in text.lines().enumerate() {
                if matched >= PER_FILE {
                    break;
                }
                if !has_word(line, names[index]) {
                    continue;
                }
                matched += 1;
                let hit = format!("./{path}:{}:{line}", number + 1);
                if !declarations[index].is_match(&hit) && found[index].len() < USES {
                    found[index].push(hit.chars().take(USE_CHARS).collect());
                }
            }
        }
    }
    found
}

#[cfg(test)]
mod tests;
