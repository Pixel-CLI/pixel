// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The word handling of the script: stems, the prompt's words, the tokens of
//! a key.

use std::collections::{BTreeSet, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use super::super::text::{head, is_lower, is_upper, subwords};

const STOP_TEXT: &str = "about above after again against also another because been before being below between both could does doing down during each
from further have having here into just more most much once only other over same should some such than that their them then there these they this those
through under until very want what when where which while will with would your locally local want where want
set run use app own the and for how can any not are you has get was its out";

static STOP: LazyLock<HashSet<&'static str>> = LazyLock::new(|| STOP_TEXT.split_whitespace().collect());

const WEAK_WORDS: [&str; 12] = [
    "dev", "server", "app", "config", "setting", "file", "default", "value", "url", "locall",
    "local", "where",
];

/// The weak words and their six-letter prefixes.
pub(super) static WEAK: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
    WEAK_WORDS
        .iter()
        .flat_map(|word| [(*word).to_owned(), head(word, 6).to_owned()])
        .collect()
});

pub(super) static BOUND_WORDS: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
    stems(["max", "maximum", "min", "minimum", "bound", "bounds", "limit", "limited", "cap", "caps"])
});
pub(super) static BOUND_VALUE_WORDS: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
    stems(["defined", "definition", "level", "size", "value", "number", "amount"])
});
pub(super) static MAX_WORDS: LazyLock<BTreeSet<String>> =
    LazyLock::new(|| stems(["max", "maximum", "cap", "caps"]));
pub(super) static MIN_WORDS: LazyLock<BTreeSet<String>> =
    LazyLock::new(|| stems(["min", "minimum"]));
pub(super) static PERSISTENCE_WORDS: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
    stems(["persist", "persistence", "session", "remember", "restore", "startup", "initial", "load"])
});

static WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("[a-z]{2,}").expect("the word pattern is a literal"));
static KEY_SPLIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[_\-\s.]+").expect("the key separator is a literal"));

/// Each word cut to six characters with a trailing `s` dropped (not `ss`), so
/// `byte` matches `bytes` and `file` matches `files`.
pub(super) fn stems<S: AsRef<str>>(words: impl IntoIterator<Item = S>) -> BTreeSet<String> {
    words
        .into_iter()
        .map(|word| {
            let cut = head(word.as_ref(), 6);
            if cut.chars().count() >= 4 && cut.ends_with('s') && !cut.ends_with("ss") {
                cut[..cut.len() - 1].to_owned()
            } else {
                cut.to_owned()
            }
        })
        .collect()
}

/// The lowercase words of the prompt, once each, in order, without the stop
/// words and the two-letter ones (but `ws`).
pub(super) fn words_of(prompt: &str) -> Vec<String> {
    let lower = prompt.to_lowercase();
    let mut seen = HashSet::new();
    WORD.find_iter(&lower)
        .map(|found| found.as_str())
        .filter(|word| seen.insert(*word))
        .filter(|word| (!STOP.contains(word) && word.len() >= 3) || *word == "ws")
        .map(ToString::to_string)
        .collect()
}

/// The tokens of a key: `API_BASE_URL` and `apiBaseUrl` are `api`, `base`, `url`.
pub(super) fn key_tokens(key: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in KEY_SPLIT.split(key) {
        if is_upper(part) || is_lower(part) {
            out.push(part.to_lowercase());
        } else {
            out.extend(subwords(part));
        }
    }
    out.retain(|token| !token.is_empty());
    out
}

/// The lowercase `[a-z]+` runs of a text.
pub(super) fn letters(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    lower
        .split(|ch: char| !ch.is_ascii_lowercase())
        .filter(|run| !run.is_empty())
        .map(ToString::to_string)
        .collect()
}
