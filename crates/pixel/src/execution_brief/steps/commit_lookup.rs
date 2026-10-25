// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! What the two history steps share: finding the commit a question is about.
//!
//! Ported from the helpers `probes/history-fallback/probe` and
//! `probes/cand-k-commit-patch/probe` carry twice. A commit is looked for in
//! this order, the first that names one winning: a version the prompt quotes
//! (the commit its release tag points at, then commit subjects that share the
//! prompt's other words), a quoted title (`git log --grep`), the commit
//! subjects that share several words with the prompt, and the prompt's rarest
//! words in the commit messages. Read-only git, deterministic.

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use pixel_git::{ENUMERATION_MAX_OUTPUT_BYTES, GitOptions, GitRunner};
use regex::Regex;

use super::Ctx;
use super::text::is_word;

/// Words any history question uses: they name no commit of their own.
const ASKING: &[&str] = &[
    "which", "what", "when", "where", "commit", "commits", "change", "changed", "changes",
    "release", "released", "made", "make", "does", "version", "versions",
];
/// Words of a subject that say nothing about it, besides [`ASKING`].
const SUBJECT_STOP: &[&str] = &[
    "was",
    "were",
    "is",
    "are",
    "the",
    "to",
    "of",
    "a",
    "an",
    "and",
    "or",
    "it",
    "did",
    "that",
    "this",
    "how",
    "from",
    "with",
    "for",
    "in",
    "added",
    "add",
    "introduced",
    "introduce",
    "happen",
    "happens",
    "first",
];
/// Most commits whose subjects are ranked, and whose messages are counted.
const LOG_LIMIT: &str = "20000";
/// Most bytes of subjects read; a longer history is not ranked.
const SUBJECT_BYTES: usize = 4 * 1024 * 1024;
/// Wall time the subject ranking gets of the step's.
const SUBJECT_TIME: Duration = Duration::from_millis(800);
/// Prompt words (of this many letters at least) looked for in commit messages.
const WORD_LETTERS: usize = 5;
/// Prompt words whose messages are counted at most.
const WORDS_COUNTED: usize = 24;
/// Subject words two commits must share to be a candidate.
const SUBJECT_SHARED_MIN: usize = 2;
/// Shared subject words that admit a candidate without an adjacent pair.
const SUBJECT_SHARED_SURE: usize = 3;
/// Score of one adjacent pair of prompt words found in a subject.
const ADJACENT_SCORE: f64 = 4.0;
/// Candidates the subject ranking returns.
const SUBJECT_CANDIDATES: usize = 6;

/// A version at `from`: an optional `v`, then digits with at least one `.digits`.
static VERSION_AT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^v?(\d+(?:\.\d+)+)").expect("the version pattern is a literal"));
static QUOTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"["'`]([^"'`]{8,120})["'`]"#).expect("the quoted-title pattern is a literal")
});

/// Stdout of read-only `git <args>`, or `None` when git fails, prints more than
/// `cap` bytes, or the step's time (or `limit`, when shorter) is up.
fn git_within(ctx: &Ctx, args: &[&str], limit: Option<Duration>, cap: usize) -> Option<String> {
    let left = ctx.deadline.saturating_duration_since(Instant::now());
    let timeout = limit.map_or(left, |limit| limit.min(left));
    if timeout.is_zero() {
        return None;
    }
    let git = GitRunner::with_options(
        ctx.root,
        GitOptions {
            timeout: Some(timeout),
            max_output_bytes: Some(cap),
        },
    );
    let out = git.run_isolated(args).ok()?;
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// Stdout of read-only `git <args>`; `""` when git fails or time is up, as a
/// commit the script could not find.
pub(super) fn git(ctx: &Ctx, args: &[&str]) -> String {
    git_within(ctx, args, None, ENUMERATION_MAX_OUTPUT_BYTES).unwrap_or_default()
}

/// The first word of a `%h %as %s` line: the commit id.
pub(super) fn id_of(line: &str) -> Option<&str> {
    line.split_whitespace().next()
}

/// The subject of a `%h %as %s` line: what follows the date.
fn subject_of(line: &str) -> &str {
    line.splitn(3, ' ').last().unwrap_or_default()
}

/// Maximal runs of the characters `keep` accepts.
fn runs(text: &str, keep: fn(char) -> bool) -> impl Iterator<Item = &str> {
    text.split(move |ch: char| !keep(ch))
        .filter(|run| !run.is_empty())
}

/// `(start, end, number)` of each version the text quotes (`3.10`, `v5.3.0`):
/// not glued to a word or a dot before, not followed by a word character.
/// A match that is followed by `.digit` cannot happen: the digits are taken
/// greedily.
fn version_spans(text: &str) -> Vec<(usize, usize, &str)> {
    let mut spans = Vec::new();
    let mut from = 0;
    for (at, _) in text.char_indices() {
        let glued = text[..at]
            .chars()
            .next_back()
            .is_some_and(|before| is_word(before) || before == '.');
        if at < from || glued {
            continue;
        }
        let Some(found) = VERSION_AT.captures(&text[at..]) else {
            continue;
        };
        let (Some(whole), Some(number)) = (found.get(0), found.get(1)) else {
            continue;
        };
        let end = at + whole.end();
        if text[end..].chars().next().is_some_and(is_word) {
            continue;
        }
        spans.push((at, end, &text[at + number.start()..at + number.end()]));
        from = end;
    }
    spans
}

/// The versions the prompt quotes, each once, in order.
pub(super) fn versions(prompt: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for (_, _, number) in version_spans(prompt) {
        if !seen.iter().any(|held| held == number) {
            seen.push(number.to_owned());
        }
    }
    seen
}

/// `text` with every version it quotes replaced by a space.
fn without_versions(text: &str) -> String {
    let mut out = String::new();
    let mut from = 0;
    for (start, end, _) in version_spans(text) {
        out.push_str(&text[from..start]);
        out.push(' ');
        from = end;
    }
    out.push_str(&text[from..]);
    out
}

/// The first four letters of each word of four letters or more, but the ones
/// every history question uses.
fn stems(text: &str) -> HashSet<String> {
    runs(&text.to_lowercase(), |ch| ch.is_ascii_lowercase())
        .filter(|word| word.len() >= 4 && !ASKING.contains(word))
        .map(|word| word[..4].to_owned())
        .collect()
}

/// The words of a subject or a prompt that can name a commit: lowercase
/// letters and digits, two or more, but the ones that say nothing.
pub(super) fn subject_words(text: &str) -> Vec<String> {
    runs(&text.to_lowercase(), |ch| {
        ch.is_ascii_lowercase() || ch.is_ascii_digit()
    })
    .filter(|word| word.len() >= 2 && !ASKING.contains(word) && !SUBJECT_STOP.contains(word))
    .map(ToOwned::to_owned)
    .collect()
}

/// `(commit, "%h %as %s")` of the commits naming a version the prompt quotes,
/// best first: the commit its exact release tag points at, then the subjects
/// sharing most of the prompt's other word stems, then the newest. Only the
/// tagged commit and subjects sharing a stem are kept: a bare number names no
/// commit. Empty when no quoted version names one.
pub(super) fn by_version(ctx: &Ctx, prompt: &str) -> Vec<(String, String)> {
    let asked = stems(&without_versions(prompt));
    let shared = |line: &str| asked.intersection(&stems(subject_of(line))).count();
    for version in versions(prompt) {
        if ctx.timed_out() {
            break;
        }
        let pattern = format!(
            "(^|[^0-9.]){}([^0-9.]|\\.([^0-9]|$)|$)",
            version.replace('.', "\\.")
        );
        let log = git(
            ctx,
            &[
                "log",
                "-i",
                "-E",
                "-n",
                "20",
                "--format=%H\t%h %as %s",
                &format!("--grep={pattern}"),
            ],
        );
        let mut found: Vec<(String, String)> = log
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(sha, line)| (sha.to_owned(), line.to_owned()))
            .collect();
        let tagged = [format!("v{version}"), version.clone()]
            .iter()
            .map(|tag| {
                let tag = format!("refs/tags/{tag}");
                git(ctx, &["rev-list", "-n", "1", &tag, "--"])
                    .trim()
                    .to_owned()
            })
            .find(|sha| !sha.is_empty())
            .unwrap_or_default();
        if !tagged.is_empty() && !found.iter().any(|(sha, _)| *sha == tagged) {
            let line = git(ctx, &["show", "-s", "--format=%h %as %s", &tagged]);
            found.insert(0, (tagged.clone(), line.trim().to_owned()));
        }
        let mut ranked: Vec<(usize, (String, String))> = found.into_iter().enumerate().collect();
        ranked.sort_by_key(|(index, (sha, line))| (*sha != tagged, Reverse(shared(line)), *index));
        let kept: Vec<(String, String)> = ranked
            .into_iter()
            .map(|(_, commit)| commit)
            .filter(|(sha, line)| *sha == tagged || shared(line) > 0)
            .collect();
        if !kept.is_empty() {
            return kept;
        }
    }
    Vec::new()
}

/// The titles the prompt quotes between quotes or backticks (8 to 120 characters).
pub(super) fn quoted(prompt: &str) -> Vec<String> {
    QUOTED
        .captures_iter(prompt)
        .filter_map(|found| found.get(1))
        .map(|title| title.as_str().trim().to_owned())
        .collect()
}

/// `(commit, "%h %as %s")` of the commits whose subject shares several words
/// with the prompt, best first: the shared words weigh by how few subjects hold
/// them, an adjacent pair of prompt words adds more. This ranks candidates, not
/// proof of first introduction; a lone shared word never admits one.
pub(super) fn by_subject(ctx: &Ctx, prompt: &str) -> Vec<(String, String)> {
    let Some(log) = git_within(
        ctx,
        &["log", "-n", LOG_LIMIT, "--format=%H\t%h %as %s"],
        Some(SUBJECT_TIME),
        SUBJECT_BYTES,
    ) else {
        return Vec::new();
    };
    let records: Vec<(&str, &str)> = log.lines().filter_map(|row| row.split_once('\t')).collect();
    let corpus: Vec<BTreeSet<String>> = records
        .iter()
        .map(|(_, line)| subject_words(subject_of(line)).into_iter().collect())
        .collect();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for word in corpus.iter().flatten() {
        *counts.entry(word.as_str()).or_default() += 1;
    }
    let asked: BTreeSet<String> = subject_words(prompt).into_iter().collect();
    let lower = prompt.to_lowercase();
    let raw: Vec<&str> =
        runs(&lower, |ch| ch.is_ascii_lowercase() || ch.is_ascii_digit()).collect();
    let pairs: HashSet<(&str, &str)> = raw
        .windows(2)
        .filter(|pair| asked.contains(pair[0]) && asked.contains(pair[1]))
        .map(|pair| (pair[0], pair[1]))
        .collect();
    let mut ranked: Vec<(f64, usize, usize, &(&str, &str))> = Vec::new();
    for (index, (record, tokens)) in records.iter().zip(&corpus).enumerate() {
        if ctx.timed_out() {
            return Vec::new();
        }
        let shared: Vec<&String> = asked.intersection(tokens).collect();
        if shared.len() < SUBJECT_SHARED_MIN {
            continue;
        }
        let subject = subject_of(record.1).to_lowercase();
        let seq: Vec<&str> = runs(&subject, |ch| {
            ch.is_ascii_lowercase() || ch.is_ascii_digit()
        })
        .collect();
        let adjacent = seq
            .windows(2)
            .map(|pair| (pair[0], pair[1]))
            .collect::<HashSet<_>>()
            .intersection(&pairs)
            .count();
        if shared.len() < SUBJECT_SHARED_SURE && adjacent == 0 {
            continue;
        }
        let weight: f64 = shared
            .iter()
            .map(|word| {
                let held = counts.get(word.as_str()).copied().unwrap_or_default();
                (records.len() as f64 / (1 + held) as f64).ln_1p()
            })
            .sum();
        ranked.push((
            weight + ADJACENT_SCORE * adjacent as f64,
            shared.len(),
            index,
            record,
        ));
    }
    // Best score first, then most shared words, then the newest.
    ranked.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.1.cmp(&a.1))
            .then(a.2.cmp(&b.2))
    });
    ranked
        .into_iter()
        .take(SUBJECT_CANDIDATES)
        .map(|(_, _, _, (sha, line))| ((*sha).to_owned(), (*line).to_owned()))
        .collect()
}

/// The prompt's words of five letters or more that some commit message holds,
/// rarest first (ties alphabetical): the words a commit is looked for by.
pub(super) fn rare_words(ctx: &Ctx, prompt: &str) -> Vec<String> {
    let lower = prompt.to_lowercase();
    let mut words: Vec<&str> = Vec::new();
    for word in runs(&lower, |ch| ch.is_ascii_lowercase()).filter(|word| word.len() >= WORD_LETTERS)
    {
        if !words.contains(&word) {
            words.push(word);
        }
    }
    if words.is_empty() {
        return Vec::new();
    }
    let log = git(ctx, &["log", "-i", "-n", LOG_LIMIT, "--format=%B%x00"]).to_lowercase();
    let messages: Vec<&str> = log.split('\0').collect();
    let mut counted: Vec<(usize, &str)> = Vec::new();
    for word in words.into_iter().take(WORDS_COUNTED) {
        if ctx.timed_out() {
            return Vec::new();
        }
        let held = messages
            .iter()
            .filter(|message| message.contains(word))
            .count();
        if held > 0 {
            counted.push((held, word));
        }
    }
    counted.sort_unstable();
    counted
        .into_iter()
        .map(|(_, word)| word.to_owned())
        .collect()
}

/// `--grep=<word>` arguments for the first `count` of `words`.
pub(super) fn greps(words: &[String], count: usize) -> Vec<String> {
    words
        .iter()
        .take(count)
        .map(|word| format!("--grep={word}"))
        .collect()
}

#[cfg(test)]
mod tests;
