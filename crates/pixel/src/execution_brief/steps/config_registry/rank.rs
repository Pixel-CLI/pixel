// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Which keys a question asks about.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;

use super::registry::{Entry, Registry};
use super::repo::Repo;
use super::words::{
    BOUND_VALUE_WORDS, BOUND_WORDS, MAX_WORDS, MIN_WORDS, WEAK, key_tokens, letters, stems,
    words_of,
};

const DEV_SERVER_PORT: &str = "dev-server port";

static COMMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:#|//|\*|/\*)").expect("the comment pattern is a literal")
});
static MAX_QUERY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bmax(?:imum)?\b").expect("the maximum pattern is a literal")
});
static MIN_QUERY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bmin(?:imum)?\b").expect("the minimum pattern is a literal")
});

/// The comment lines (at most three) right above line `line` of `path`:
/// `range(n - 2, max(-1, n - 5), -1)` of the script.
pub(super) fn comment_above(repo: &Repo, path: &str, line: usize) -> String {
    let Some(lines) = repo.lines(path) else {
        return String::new();
    };
    let first = i64::try_from(line).unwrap_or(i64::MAX);
    let mut out: Vec<&str> = Vec::new();
    let mut at = first - 2;
    while at > (first - 5).max(-1) {
        let Some(text) = usize::try_from(at).ok().and_then(|index| lines.get(index)) else {
            break;
        };
        if !COMMENT.is_match(text) {
            break;
        }
        out.push(text);
        at -= 1;
    }
    out.join(" ")
}

/// The up-to-four keys that best match the prompt.
pub(super) fn rank(repo: &Repo, keys: &[(&String, &Entry)], prompt: &str) -> Vec<String> {
    let st = stems(words_of(prompt));
    let strong: BTreeSet<&String> = st.difference(&WEAK).collect();
    let mut scored: Vec<(usize, usize, &String)> = Vec::new();
    for (key, entry) in keys {
        let tokens = if key.as_str() == DEV_SERVER_PORT {
            vec!["dev".to_owned(), "server".to_owned(), "port".to_owned()]
        } else {
            key_tokens(key)
        };
        let key_stems = stems(tokens);
        let strong_key = strong.iter().filter(|word| key_stems.contains(**word)).count();
        let weak_key = st
            .iter()
            .filter(|word| key_stems.contains(*word) && !strong.contains(word))
            .count();
        let mut score = 3 * strong_key + weak_key;
        let mut hay = String::new();
        for def in entry.defs.iter().take(2) {
            hay.push(' ');
            hay.push_str(&comment_above(repo, &def.path, def.line));
            hay.push(' ');
            hay.push_str(&def.text);
        }
        let hay_stems = stems(letters(&hay).into_iter().filter(|run| run.len() >= 3));
        score += 2 * strong
            .iter()
            .filter(|word| hay_stems.contains(**word) && !key_stems.contains(**word))
            .count();
        if key.as_str() == DEV_SERVER_PORT && st.contains("port") {
            score += 3;
        }
        if entry
            .reads
            .iter()
            .any(|read| !read.path.ends_with(".rs") || read.text.contains("env"))
        {
            score += 2;
        }
        let named = strong_key > 0 || strong.iter().any(|word| hay_stems.contains(*word));
        if named {
            scored.push((score, entry.defs.len() + entry.reads.len(), key));
        }
    }
    scored.sort_by(|a, b| b.cmp(a));
    scored.into_iter().take(4).map(|(_, _, key)| key.clone()).collect()
}

/// [`rank`] over the keys that declare a bound and a prompt-specific subject,
/// for a question that asks only for a maximum or minimum.
pub(super) fn rank_bound(repo: &Repo, reg: &Registry, prompt: &str) -> Vec<String> {
    let prompt_words = stems(words_of(prompt));
    let subject: BTreeSet<&String> = prompt_words
        .iter()
        .filter(|word| {
            !WEAK.contains(*word) && !BOUND_WORDS.contains(*word) && !BOUND_VALUE_WORDS.contains(*word)
        })
        .collect();
    if subject.is_empty() {
        return Vec::new();
    }
    let requested_max = MAX_QUERY.is_match(prompt);
    let requested_min = MIN_QUERY.is_match(prompt);
    let mut eligible: Vec<(&String, &Entry)> = Vec::new();
    for (key, entry) in reg {
        let key_words = stems(key_tokens(key));
        let declarations: Vec<&str> = entry.defs.iter().take(2).map(|def| def.text.as_str()).collect();
        // The comment above each declaration counts, as in `rank`: "cap" in the
        // doc comment above a constant makes it a bound declaration.
        let comments: Vec<String> = entry
            .defs
            .iter()
            .take(2)
            .map(|def| comment_above(repo, &def.path, def.line))
            .collect();
        let text = format!("{} {}", declarations.join(" "), comments.join(" "));
        let declaration_words = stems(letters(&text));
        let all: BTreeSet<&String> = key_words.iter().chain(&declaration_words).collect();
        let has = |words: &BTreeSet<String>| all.iter().any(|word| words.contains(*word));
        let has_direction = !(requested_max || requested_min)
            || (requested_max && has(&MAX_WORDS))
            || (requested_min && has(&MIN_WORDS));
        if has(&BOUND_WORDS) && has_direction && subject.iter().any(|word| all.contains(*word)) {
            eligible.push((key, entry));
        }
    }
    rank(repo, &eligible, prompt)
}
