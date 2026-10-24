// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The history steps (`specs/brief.md`, "Recipes"): a question about a commit
//! is answered from git itself when the brief holds no commits.
//!
//! The commit is found the way the probes found it, first match wins:
//! the commit a version the prompt quotes (`3.10`, `v5.3.0`) names (its release
//! tag first, then the subjects that share a word stem with the rest of the
//! prompt); a quoted title through `git log --grep`; the subjects that share
//! several of the prompt's words, weighted by how rare each is in the log
//! (a lone shared word never admits a commit); the prompt's rarest words
//! through `--grep --all-match` and, for `history-fallback` alone, a `-S`
//! pickaxe on the rarest one.
//!
//! * **`history-fallback`** prints the matching commits, then the top commit's
//!   message, stat and patch (a merge against its first parent) and the first
//!   tag that contains it.
//! * **`cand-k-commit-patch`** prints the commit's message (fenced output cut),
//!   the complete `--stat` file list, then the hunks file by file, source first,
//!   manifests next, lockfiles summarised, inside a byte budget; and it takes
//!   every block that is not about commits out of the rest of the pack. It
//!   stays silent where a single rare word is all there is: that names no
//!   commit.
//!
//! Both read git through `Evidence::git_read` (`log`, `show`, `rev-list`,
//! `tag`), are deterministic and stay silent when the brief already holds
//! commits or nothing matches. Headers carry the abbreviated sha: the script
//! printed the full one for a subject match.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;

use crate::region_pack::Block;
use crate::routes::QuestionKind;

use super::{Ctx, Drop, Outcome, Step};

const PATCH_LINES: usize = 90;
const MESSAGE_LINES: usize = 10;
const SUBJECT_LIMIT: &str = "20000";
const SUBJECT_BYTES: usize = 4 * 1024 * 1024;
const SUBJECT_TIME: Duration = Duration::from_millis(800);
const SUBJECT_MATCHES: usize = 6;
/// Bytes of the log of every message the rare-word search reads.
const MESSAGES_BYTES: usize = 8 * 1024 * 1024;
const LINE_BYTES: usize = 64 * 1024;
const PATCH_BYTES: usize = 3300 + 1500;
const SHOW_BYTES: usize = 2 * 1024 * 1024;
const LINE_CHARS: usize = 160;
const PATCH_LINE_CHARS: usize = 150;
const COMMIT_MESSAGE_LINES: usize = 34;
const FENCED_KEEP: usize = 6;
const LOCK_LINES: usize = 6;
const MAX_WORDS_COUNTED: usize = 24;

/// Words any history question uses: they name no commit of their own.
const ASKING: &[&str] = &[
    "which", "what", "when", "where", "commit", "commits", "change", "changed", "changes",
    "release", "released", "made", "make", "does", "version", "versions",
];
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

static ASKS_HISTORY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(commits?|changelog|git history|introduced|blame|released?|tagged?)\b")
        .expect("the history-question pattern is a literal")
});
static VERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"v?(\d+\.\d+(?:\.\d+)*)").expect("the version pattern is a literal")
});
static QUOTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"["'`]([^"'`]{8,120})["'`]"#).expect("the quoted-title pattern is a literal")
});
static WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[a-z0-9]+").expect("the word pattern is a literal"));
static STEM_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[a-z]{4,}").expect("the stem pattern is a literal"));
static RARE_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[a-z]{5,}").expect("the rare-word pattern is a literal"));
static NUMBERS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d(\.\d+)?\s*(±|x\b|times|%|ms\b)").expect("the numbers pattern is a literal")
});
static LOCKS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(^|/)(Cargo\.lock|package-lock\.json|yarn\.lock|pnpm-lock\.yaml|bun\.lockb?|go\.sum|poetry\.lock|uv\.lock|Gemfile\.lock|composer\.lock|Pipfile\.lock)$|\.min\.(js|css)$|\.snap$",
    )
    .expect("the lockfile pattern is a literal")
});
static MANIFEST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(^|/)(Cargo\.toml|package\.json|pyproject\.toml|go\.mod|build\.gradle|pom\.xml|Gemfile)$",
    )
    .expect("the manifest pattern is a literal")
});
static DIFF_HEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^diff --git a/(.*) b/(.*)$").expect("the diff header pattern is a literal")
});

pub(super) fn run(ctx: &Ctx, out: &mut Outcome) {
    if ctx.kind != QuestionKind::History || ctx.held.history || !ASKS_HISTORY.is_match(ctx.typed) {
        return;
    }
    if ctx.steps.enabled(Step::CommitPatch) {
        let _timer = ctx.timer(Step::CommitPatch);
        if let Some(sha) = find(ctx, true).map(|found| found.sha)
            && let Some(block) = commit_patch(ctx, &sha)
        {
            out.drops.push(Drop::AllButHistory);
            out.rescuing = true;
            out.lead.push(block);
        }
    }
    if ctx.steps.enabled(Step::HistoryFallback) {
        let _timer = ctx.timer(Step::HistoryFallback);
        fallback(ctx, out);
    }
}

/// The commit a question names, the lines that list the candidates, and the
/// words they matched.
struct Found {
    sha: String,
    shown: Vec<String>,
    words: Vec<String>,
}

/// A read-only git command's text, `None` when it fails.
fn git(ctx: &Ctx, args: &[&str], max_bytes: usize) -> Option<String> {
    ctx.evidence.git_read(args, max_bytes, ctx.deadline).ok()
}

fn is_stop(word: &str) -> bool {
    ASKING.contains(&word) || SUBJECT_STOP.contains(&word)
}

/// The words of a subject or a prompt that can name a commit.
fn subject_words(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    WORD.find_iter(&lower)
        .map(|found| found.as_str())
        .filter(|word| !is_stop(word) && word.len() >= 2)
        .map(ToString::to_string)
        .collect()
}

/// First four letters of every word of four or more that is no asking word.
fn stems(text: &str) -> BTreeSet<String> {
    let lower = text.to_lowercase();
    STEM_WORD
        .find_iter(&lower)
        .map(|found| found.as_str())
        .filter(|word| !ASKING.contains(word))
        .map(|word| word[..4].to_string())
        .collect()
}

/// The versions a prompt quotes, once each, in order: `3.10` or `v5.3.0`, not
/// part of a longer number or a word.
fn versions(prompt: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for caps in VERSION.captures_iter(prompt) {
        let (Some(whole), Some(number)) = (caps.get(0), caps.get(1)) else {
            continue;
        };
        let before = prompt[..whole.start()].chars().next_back();
        let after = prompt[whole.end()..].chars().next();
        if before.is_some_and(|ch| ch.is_alphanumeric() || ch == '_' || ch == '.')
            || after.is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
        {
            continue;
        }
        if !found.iter().any(|held| held == number.as_str()) {
            found.push(number.as_str().to_string());
        }
    }
    found
}

/// The commits that name a version the prompt quotes, best first, as
/// `(sha, "%h %as %s")`: the commit its release tag points at, then the
/// subjects that share the most word stems with the rest of the prompt. Only
/// the tagged commit and subjects sharing a stem are kept: a bare number names
/// no commit.
fn by_version(ctx: &Ctx) -> Vec<(String, String)> {
    let asked = stems(&VERSION.replace_all(ctx.typed, " "));
    for version in versions(ctx.typed) {
        let escaped = regex::escape(&version);
        let pattern = format!("(^|[^0-9.]){escaped}([^0-9.]|\\.([^0-9]|$)|$)");
        let grep = format!("--grep={pattern}");
        let log = git(
            ctx,
            &[
                "log",
                "-i",
                "-E",
                "-n",
                "20",
                "--format=%H\t%h %as %s",
                &grep,
            ],
            LINE_BYTES,
        )
        .unwrap_or_default();
        let mut found: Vec<(String, String)> = log
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(sha, line)| (sha.to_string(), line.to_string()))
            .collect();
        let tagged = [format!("v{version}"), version.clone()]
            .iter()
            .find_map(|tag| {
                let name = format!("refs/tags/{tag}");
                git(ctx, &["rev-list", "-n", "1", &name, "--"], LINE_BYTES)
                    .map(|text| text.trim().to_string())
                    .filter(|sha| !sha.is_empty())
            })
            .unwrap_or_default();
        if !tagged.is_empty() && !found.iter().any(|(sha, _)| *sha == tagged) {
            let line = git(
                ctx,
                &["show", "-s", "--format=%h %as %s", &tagged],
                LINE_BYTES,
            )
            .unwrap_or_default();
            found.insert(0, (tagged.clone(), line.trim().to_string()));
        }
        let shared = |line: &str| {
            let subject = line.splitn(3, ' ').last().unwrap_or("");
            asked.intersection(&stems(subject)).count()
        };
        let mut ranked: Vec<(usize, (String, String))> = found.into_iter().enumerate().collect();
        ranked.sort_by_key(|(index, (sha, line))| {
            (*sha != tagged, std::cmp::Reverse(shared(line)), *index)
        });
        let kept: Vec<(String, String)> = ranked
            .into_iter()
            .map(|(_, row)| row)
            .filter(|(sha, line)| *sha == tagged || shared(line) > 0)
            .collect();
        if !kept.is_empty() {
            return kept;
        }
    }
    Vec::new()
}

/// The subjects that share at least three of the prompt's words (or two that
/// are adjacent in both), best first, as `(abbreviated sha, "%h %as %s")`.
/// Weighted by how rare each word is in the log; a ranking of candidates, not
/// proof of first introduction.
fn by_subject(ctx: &Ctx) -> Vec<(String, String)> {
    let deadline = ctx.deadline.min(Instant::now() + SUBJECT_TIME);
    let limit = SUBJECT_LIMIT;
    let Ok(text) = ctx.evidence.git_read(
        &["log", "-n", limit, "--format=%H\t%h %as %s"],
        SUBJECT_BYTES,
        deadline,
    ) else {
        return Vec::new();
    };
    let records: Vec<(&str, &str)> = text
        .lines()
        .filter_map(|row| row.split_once('\t'))
        .collect();
    let subject_of = |line: &str| line.splitn(3, ' ').last().unwrap_or("").to_string();
    let corpus: Vec<BTreeSet<String>> = records
        .iter()
        .map(|(_, line)| subject_words(&subject_of(line)).into_iter().collect())
        .collect();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for row in &corpus {
        for word in row {
            *counts.entry(word.as_str()).or_insert(0) += 1;
        }
    }
    let asked: BTreeSet<String> = subject_words(ctx.typed).into_iter().collect();
    let lower = ctx.typed.to_lowercase();
    let raw: Vec<&str> = WORD.find_iter(&lower).map(|found| found.as_str()).collect();
    let pairs: BTreeSet<(&str, &str)> = raw
        .windows(2)
        .filter(|pair| asked.contains(pair[0]) && asked.contains(pair[1]))
        .map(|pair| (pair[0], pair[1]))
        .collect();
    let total = records.len() as f64;
    let mut ranked: Vec<(f64, usize, std::cmp::Reverse<usize>, &str, &str)> = Vec::new();
    for (index, ((_, line), tokens)) in records.iter().zip(&corpus).enumerate() {
        let shared: Vec<&String> = asked.intersection(tokens).collect();
        if shared.len() < 2 {
            continue;
        }
        let subject = subject_of(line).to_lowercase();
        let seq: Vec<&str> = WORD
            .find_iter(&subject)
            .map(|found| found.as_str())
            .collect();
        let adjacent = seq
            .windows(2)
            .map(|pair| (pair[0], pair[1]))
            .collect::<BTreeSet<_>>()
            .intersection(&pairs)
            .count();
        if shared.len() < 3 && adjacent == 0 {
            continue;
        }
        let score: f64 = shared
            .iter()
            .map(|word| (total / (1.0 + counts[word.as_str()] as f64)).ln_1p())
            .sum::<f64>()
            + 4.0 * adjacent as f64;
        let short = line.split(' ').next().unwrap_or("");
        ranked.push((score, shared.len(), std::cmp::Reverse(index), short, line));
    }
    ranked.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(b.1.cmp(&a.1))
            .then(b.2.cmp(&a.2))
            .then(b.4.cmp(a.4))
    });
    ranked
        .into_iter()
        .take(SUBJECT_MATCHES)
        .map(|(.., short, line)| (short.to_string(), line.to_string()))
        .collect()
}

/// The rarest of the prompt's longer words that some commit message holds,
/// rarest first.
fn rare_words(ctx: &Ctx) -> Vec<String> {
    let lower = ctx.typed.to_lowercase();
    let mut seen = BTreeSet::new();
    let words: Vec<String> = RARE_WORD
        .find_iter(&lower)
        .map(|found| found.as_str().to_string())
        .filter(|word| seen.insert(word.clone()))
        .collect();
    let Some(log) = git(
        ctx,
        &["log", "-i", "-n", SUBJECT_LIMIT, "--format=%B%x00"],
        MESSAGES_BYTES,
    ) else {
        return Vec::new();
    };
    let messages: Vec<String> = log
        .to_lowercase()
        .split('\0')
        .map(ToString::to_string)
        .collect();
    let mut counted: Vec<(usize, String)> = words
        .into_iter()
        .take(MAX_WORDS_COUNTED)
        .map(|word| {
            (
                messages
                    .iter()
                    .filter(|message| message.contains(&word))
                    .count(),
                word,
            )
        })
        .filter(|(count, _)| *count > 0)
        .collect();
    counted.sort();
    counted.into_iter().map(|(_, word)| word).collect()
}

/// The commit and the lines that list it. `strict` is the commit-patch
/// search: no single-word match and no pickaxe.
fn find(ctx: &Ctx, strict: bool) -> Option<Found> {
    let named = by_version(ctx);
    if let Some((_, line)) = named.first() {
        let sha = line.split_whitespace().next()?.to_string();
        return Some(Found {
            sha,
            shown: named.iter().map(|(_, line)| line.clone()).collect(),
            words: versions(ctx.typed),
        });
    }
    for caps in QUOTED.captures_iter(ctx.typed) {
        let phrase = caps[1].trim();
        let grep = format!("--grep={phrase}");
        let out = git(
            ctx,
            &[
                "log",
                "-i",
                "--fixed-strings",
                "-n",
                "6",
                "--format=%h %as %s",
                &grep,
            ],
            LINE_BYTES,
        )
        .unwrap_or_default();
        let shown: Vec<String> = out.trim().lines().map(ToString::to_string).collect();
        if let Some(sha) = shown
            .first()
            .and_then(|line| line.split_whitespace().next())
        {
            return Some(Found {
                sha: sha.to_string(),
                words: subject_words(phrase),
                shown,
            });
        }
    }
    let named = by_subject(ctx);
    if let Some((sha, _)) = named.first() {
        return Some(Found {
            sha: sha.clone(),
            shown: named.iter().map(|(_, line)| line.clone()).collect(),
            words: subject_words(ctx.typed),
        });
    }
    let words = rare_words(ctx);
    if words.is_empty() {
        return None;
    }
    let ks: &[usize] = if strict { &[3, 2] } else { &[3, 2, 1] };
    // A search on fewer words than `k` is the search already made.
    let mut tried = BTreeSet::new();
    for &k in ks {
        if !tried.insert(k.min(words.len())) {
            continue;
        }
        let greps: Vec<String> = words
            .iter()
            .take(k)
            .map(|word| format!("--grep={word}"))
            .collect();
        let mut args = vec!["log", "-i", "--all-match", "-n", "6", "--format=%h %as %s"];
        args.extend(greps.iter().map(String::as_str));
        let out = git(ctx, &args, LINE_BYTES).unwrap_or_default();
        let shown: Vec<String> = out.trim().lines().map(ToString::to_string).collect();
        if let Some(sha) = shown
            .first()
            .and_then(|line| line.split_whitespace().next())
        {
            return Some(Found {
                sha: sha.to_string(),
                shown,
                words,
            });
        }
    }
    if strict {
        return None;
    }
    let pickaxe = format!("-S{}", words[0]);
    let out = git(
        ctx,
        &[
            "log",
            &pickaxe,
            "-n",
            "5",
            "--format=%h %as %s",
            "--",
            "*.rs",
            "*.py",
            "*.ts",
            "*.js",
            "*.go",
        ],
        LINE_BYTES,
    )
    .unwrap_or_default();
    let shown: Vec<String> = out.trim().lines().map(ToString::to_string).collect();
    let sha = shown.first()?.split_whitespace().next()?.to_string();
    Some(Found { sha, shown, words })
}

fn truncate(line: &str, chars: usize) -> String {
    line.chars().take(chars).collect()
}

/// `history-fallback`: the candidates, then the top commit.
fn fallback(ctx: &Ctx, out: &mut Outcome) {
    let Some(found) = find(ctx, false) else {
        return;
    };
    let named = found
        .words
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    out.rescuing = true;
    out.append.push(Block::new(
        format!("history — commits matching {named}"),
        found.shown.clone(),
    ));
    let head = format!("commit {} — message, files and patch", found.sha);
    if out.lead.iter().any(|block| block.head() == head) {
        return;
    }
    let sha = found.sha.as_str();
    let Some(message) = git(
        ctx,
        &["show", "-s", "--format=%h %ad %an%n%B", "--date=short", sha],
        LINE_BYTES,
    ) else {
        return;
    };
    let mut body: Vec<String> = message
        .trim()
        .lines()
        .take(MESSAGE_LINES)
        .map(ToString::to_string)
        .collect();
    let patch = git(
        ctx,
        &[
            "show",
            "--format=",
            "--stat=100",
            "--patch",
            "-U2",
            "--diff-merges=first-parent",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            sha,
        ],
        SHOW_BYTES,
    )
    .unwrap_or_default();
    body.extend(patch.lines().map(ToString::to_string));
    let mut body: Vec<String> = body
        .iter()
        .take(PATCH_LINES)
        .map(|line| truncate(line, LINE_CHARS))
        .collect();
    let tags = git(
        ctx,
        &["tag", "--contains", sha, "--sort=creatordate"],
        LINE_BYTES,
    )
    .unwrap_or_default();
    if let Some(tag) = tags.lines().next().filter(|tag| !tag.is_empty()) {
        body.push(format!("first tag containing it: {tag}"));
    }
    out.append.push(Block::new(head, body));
}

/// The leading paragraphs of a commit message, fenced output cut to the few
/// lines that carry numbers, blank runs collapsed.
fn message(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.trim().lines().collect();
    let mut out: Vec<String> = lines.iter().take(1).map(ToString::to_string).collect();
    let (mut fenced, mut kept) = (false, 0);
    for line in lines.iter().skip(1) {
        if line.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            if kept < FENCED_KEEP
                && NUMBERS.is_match(line)
                && !line.contains("Summary")
                && !line.contains("Range")
            {
                kept += 1;
                out.push(format!("  {}", line.trim()));
            }
            continue;
        }
        out.push((*line).to_string());
    }
    let mut clean: Vec<String> = Vec::new();
    for line in out {
        if !line.trim().is_empty() || clean.last().is_some_and(|last| !last.trim().is_empty()) {
            clean.push(line);
        }
    }
    clean.truncate(COMMIT_MESSAGE_LINES + 1);
    clean
}

/// A unified diff cut into `(path, lines)` in the order git printed it, header
/// lines left out.
fn sections(diff: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for line in diff.lines() {
        if let Some(caps) = DIFF_HEAD.captures(line) {
            out.push((caps[2].to_string(), Vec::new()));
        } else if let Some((_, body)) = out.last_mut()
            && ![
                "index ",
                "--- ",
                "+++ ",
                "new file",
                "deleted file",
                "similarity",
            ]
            .iter()
            .any(|skip| line.starts_with(skip))
        {
            body.push(line.to_string());
        }
    }
    out
}

fn cost(lines: &[String]) -> usize {
    lines.iter().map(|line| line.chars().count() + 1).sum()
}

/// How many leading lines of `chunk` fit in `room` bytes, at least two. The
/// script assumed sixty bytes a line; the lines here are counted.
fn lines_within(chunk: &[String], room: usize) -> usize {
    let mut spent = 0;
    for (kept, line) in chunk.iter().enumerate() {
        spent += line.chars().count() + 1;
        if spent > room && kept >= 2 {
            return kept;
        }
    }
    chunk.len()
}

/// `cand-k-commit-patch`: the block for `sha`.
fn commit_patch(ctx: &Ctx, sha: &str) -> Option<Block> {
    let stat = git(
        ctx,
        &[
            "show",
            "--format=",
            "--stat=100",
            "--diff-merges=first-parent",
            "--no-ext-diff",
            "--no-color",
            sha,
        ],
        SHOW_BYTES,
    )?;
    let diff = git(
        ctx,
        &[
            "show",
            "--format=",
            "-U1",
            "--diff-merges=first-parent",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            sha,
        ],
        SHOW_BYTES,
    )
    .unwrap_or_default();
    let head = format!("commit {sha} — message, files and patch");
    let text = git(
        ctx,
        &["show", "-s", "--format=%h %ad %an%n%B", "--date=short", sha],
        LINE_BYTES,
    )?;
    let mut body: Vec<String> = message(&text)
        .iter()
        .map(|line| truncate(line, PATCH_LINE_CHARS))
        .collect();
    body.push("files in the commit:".to_string());
    body.extend(
        stat.trim_end()
            .lines()
            .map(|line| truncate(line, PATCH_LINE_CHARS)),
    );
    let mut used = head.chars().count() + 1 + cost(&body);
    let mut left_out = 0;
    let mut parts = sections(&diff);
    parts.sort_by_key(|(path, _)| {
        (
            if LOCKS.is_match(path) {
                2
            } else {
                usize::from(MANIFEST.is_match(path))
            },
            path.clone(),
        )
    });
    for (path, lines) in parts {
        if used >= PATCH_BYTES {
            left_out += 1;
            continue;
        }
        if LOCKS.is_match(&path) {
            let edits: Vec<&String> = lines
                .iter()
                .filter(|line| line.starts_with('+') || line.starts_with('-'))
                .collect();
            body.push(format!(
                "@@ {path} (lockfile, {} edited lines; first ones)",
                edits.len()
            ));
            body.extend(
                edits
                    .iter()
                    .take(LOCK_LINES)
                    .map(|line| truncate(line, PATCH_LINE_CHARS)),
            );
            continue;
        }
        let mut chunk = vec![format!("@@ {path}")];
        chunk.extend(lines.iter().map(|line| truncate(line, PATCH_LINE_CHARS)));
        if used + cost(&chunk) > PATCH_BYTES && used > 0 {
            chunk.truncate(lines_within(&chunk, PATCH_BYTES - used));
            chunk.push("... (hunks cut)".to_string());
        }
        used += cost(&chunk);
        body.extend(chunk);
    }
    if left_out > 0 {
        body.push(format!(
            "... hunks of {left_out} more files cut (named in the list above)"
        ));
    }
    Some(Block::new(head, body))
}

#[cfg(test)]
mod tests;
