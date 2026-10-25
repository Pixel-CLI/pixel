// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Source excerpts for a key, with the secrets masked, and the literal
//! `set("key", value)` calls a question names.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use super::super::text::{head, last_segment};
use super::rank::comment_above;
use super::registry::Def;
use super::repo::Repo;
use super::scan::{Row, SCAN_MAX_COLUMNS};
use super::words::{letters, stems, words_of};

/// Names that look like a secret; their values and lines are never shown.
pub(super) static SECRETISH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)key|secret|token|password|passwd|credential|private|signature|config")
        .expect("the secret pattern is a literal")
});
static SETTER_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"\b[\w.]+\.(?:set|setDefault|set_default)\(\s*(?:'([\w .-]+)'|"([\w .-]+)")"#,
        r#"\s*,\s*(?:'[^'\n]*'|"[^"\n]*"|true|false|null|-?\d+(?:\.\d+)?)\s*\)"#
    ))
    .expect("the setter pattern is a literal")
});

const JS_SUFFIXES: [&str; 8] = [".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs", ".mts", ".cts"];
const SETTER_BLOCKS: usize = 4;

/// The key `app.set('key', literal)` sets.
pub(super) fn setter_key(text: &str) -> Option<String> {
    let caps = SETTER_CALL.captures(text)?;
    let key = caps.get(1).or_else(|| caps.get(2))?;
    Some(key.as_str().trim().to_owned())
}

pub(super) fn is_js(path: &str) -> bool {
    JS_SUFFIXES.iter().any(|suffix| path.ends_with(suffix))
}

/// A secret-bearing value is never printed, whatever its length.
pub(super) fn mask(key: &str, value: &str) -> String {
    if !value.is_empty() && SECRETISH.is_match(key) {
        "<masked>".to_owned()
    } else {
        value.to_owned()
    }
}

/// `### path:lo-hi` and the numbered lines `before` above and `after` below line `n`.
pub(super) fn block(repo: &Repo, path: &str, n: usize, before: usize, after: usize) -> Vec<String> {
    let Some(lines) = repo.lines(path) else {
        return Vec::new();
    };
    let lo = n.saturating_sub(before).max(1);
    let hi = lines.len().min(n + after);
    let mut out = vec![format!("### {path}:{lo}-{hi}")];
    out.extend((lo..=hi).map(|at| format!("{at}| {}", head(&lines[at - 1], 150))));
    out
}

fn masked_line(line: &str) -> String {
    format!("{}| <masked>", line.split('|').next().unwrap_or(""))
}

/// [`block`] keeping the coordinates but never a secret-bearing source line.
pub(super) fn evidence_block(
    repo: &Repo,
    key: &str,
    path: &str,
    n: usize,
    before: usize,
    after: usize,
) -> Vec<String> {
    block(repo, path, n, before, after)
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 || !(SECRETISH.is_match(key) || SECRETISH.is_match(&line)) {
                line
            } else {
                masked_line(&line)
            }
        })
        .collect()
}

/// The literal setting calls whose key shares a word with the prompt, quoted
/// as source: no precedence or effect is inferred.
pub(super) fn setter_evidence(repo: &Repo, rows: &[Row], prompt: &str) -> String {
    let prompt_words = stems(words_of(prompt));
    if prompt_words.is_empty() {
        return String::new();
    }
    let tracked: HashSet<&str> = repo.tracked().iter().map(String::as_str).collect();
    let mut candidates: Vec<(usize, &str, usize, String)> = Vec::new();
    for Row { path, line, text } in rows {
        if !tracked.contains(path.as_str()) || !is_js(path) {
            continue;
        }
        let head = text.trim_start();
        if head.starts_with("//")
            || head.starts_with("/*")
            || head.starts_with('*')
            || text.len() > SCAN_MAX_COLUMNS
        {
            continue;
        }
        if repo.checked(path).is_none() {
            continue;
        }
        let Some(key) = setter_key(text) else {
            continue;
        };
        let overlap = stems(letters(&key)).intersection(&prompt_words).count();
        if overlap > 0 {
            candidates.push((overlap, path, *line, key));
        }
    }
    // `sorted(key=(-overlap, path, n))`, stable.
    candidates.sort_by(|a, b| (std::cmp::Reverse(a.0), a.1, a.2).cmp(&(std::cmp::Reverse(b.0), b.1, b.2)));
    let mut out: Vec<String> = Vec::new();
    for (_, path, line, key) in candidates.into_iter().take(SETTER_BLOCKS) {
        let source_block = block(repo, path, line, 3, 3);
        if let Some(head) = source_block.first() {
            out.push(format!("{head} — literal setting call candidate; inspect surrounding control flow"));
        }
        for text in source_block.iter().skip(1) {
            let source = text.split_once('|').map_or(text.as_str(), |(_, rest)| rest.trim_start());
            if SECRETISH.is_match(&key) || SECRETISH.is_match(source) {
                out.push(masked_line(text));
            } else {
                out.push(text.clone());
            }
        }
    }
    if out.is_empty() {
        String::new()
    } else {
        out.join("\n") + "\n"
    }
}

/// The head lines and excerpts of a key declared more than once: each
/// declaration stays a candidate, none becomes "the" default.
pub(super) fn declaration_candidates(repo: &Repo, key: &str, defs: &[&Def]) -> (Vec<String>, Vec<String>) {
    let mut heads = vec![format!(
        "- {key} | {} distinct declaration candidates; lexical binding and override precedence unknown",
        defs.len()
    )];
    let mut blocks = Vec::new();
    for def in defs.iter().take(4) {
        let value = mask(key, &def.value);
        let value = if value.is_empty() { "unknown" } else { value.as_str() };
        heads.push(format!("  - {key} @ {}:{} | candidate value {value}", def.path, def.line));
        // The context keeps the surrounding scope as evidence, not a guessed resolution.
        blocks.extend(evidence_block(repo, key, &def.path, def.line, 2, 2));
    }
    if defs.len() > 4 {
        heads.push(format!(
            "  - {} additional candidates omitted; no unique default established",
            defs.len() - 4
        ));
    }
    (heads, blocks)
}

/// Whether the base name of `path` is an environment file.
pub(super) fn is_env_path(path: &str) -> bool {
    last_segment(path).starts_with(".env")
}

pub(super) fn comment_marks(repo: &Repo, path: &str, line: usize) -> usize {
    let text = comment_above(repo, path, line);
    text.matches("//").count() + text.matches('#').count()
}
