// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `cand-k-commit-patch`: a question that quotes a commit title and asks what
//! the commit did and which files it touched.
//!
//! Ported from `probes/cand-k-commit-patch/probe`. The commit is found like
//! `history-fallback` does (`commit_lookup`); what is printed differs: the
//! message's leading paragraphs (fenced output is cut), the complete `--stat`
//! file list (so every file of the commit is named, with its counts), then the
//! hunks file by file with source files first and manifests next (a merge
//! against its first parent), lockfiles and generated files only summarised,
//! inside a byte budget. The block carries the header `history-fallback` uses,
//! so in `lead` mode it replaces that block (the merge skips a later block
//! whose header is already held), and every other block of the brief is
//! dropped. Read-only git, deterministic, nothing printed when no commit
//! matches or the brief already holds a history block.

use std::sync::LazyLock;

use pixel_plugin::ProbeMode;
use regex::Regex;

use super::commit_lookup::{by_subject, by_version, git, greps, id_of, quoted, rare_words};
use super::text::escape;
use super::{Ctx, Step};

pub(super) const STEP: Step = Step {
    name: "cand-k-commit-patch",
    when_kinds: &["history"],
    skip_kinds: &["review"],
    when_prompt: Some(
        r"(?i)\b(commits?|changelog|git history|introduced|blame|released?|tagged?)\b",
    ),
    when_gate_closed: true,
    budget_ms: 2000,
    mode: ProbeMode::Lead,
    run,
};

/// Lines of the message kept, after its first line.
const MESSAGE_LINES: usize = 34;
/// Lines of fenced output kept in a message (the ones that carry a number).
const FENCED_KEEP: usize = 6;
/// Characters of hunks, message and stat the block aims at, and the slack after
/// which a file is cut or not shown at all: 3 300 + 1 500.
const PATCH_BUDGET: usize = 4_800;
/// Edited lines of a lockfile shown.
const LOCK_LINES: usize = 6;
/// Characters of one line shown.
const LINE_CHARS: usize = 150;
/// Characters one more line of a cut file is worth when sizing what is kept.
const CUT_LINE_COST: usize = 60;
/// Candidates a `--all-match` search starts with, then one fewer.
const MATCH_WORDS: [usize; 2] = [3, 2];

/// A figure with a unit: the line of a fenced benchmark worth keeping.
static NUMBERS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d(?:\.\d+)?\s*(?:±|x\b|times|%|ms\b)").expect("the figure pattern is a literal")
});
static LOCKS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|/)(?:Cargo\.lock|package-lock\.json|yarn\.lock|pnpm-lock\.yaml|bun\.lockb?|go\.sum|poetry\.lock|uv\.lock|Gemfile\.lock|composer\.lock|Pipfile\.lock)$|\.min\.(?:js|css)$|\.snap$",
    )
    .expect("the lockfile pattern is a literal")
});
static MANIFEST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|/)(?:Cargo\.toml|package\.json|pyproject\.toml|go\.mod|build\.gradle|pom\.xml|Gemfile)$")
        .expect("the manifest pattern is a literal")
});
static DIFF_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^diff --git a/(.*) b/(.*)$").expect("the diff header pattern is a literal")
});
static HEADERS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^### (.+)$").expect("the header pattern is a literal"));

fn run(ctx: &Ctx) -> String {
    let brief = ctx.brief;
    if brief
        .split('\n')
        .any(|line| line.starts_with("### history") || line.starts_with("### commit "))
    {
        return String::new();
    }
    let Some(sha) = find_commit(ctx, ctx.prompt) else {
        return String::new();
    };
    let stat = git(
        ctx,
        &[
            "show",
            "--format=",
            "--stat=100",
            "--diff-merges=first-parent",
            "--no-ext-diff",
            &sha,
        ],
    );
    let diff = git(
        ctx,
        &[
            "show",
            "--format=",
            "-U1",
            "--diff-merges=first-parent",
            "--no-ext-diff",
            &sha,
        ],
    );
    let mut files = sections(&diff);
    files.sort_by(|a, b| (rank(&a.0), &a.0).cmp(&(rank(&b.0), &b.0)));
    let mut out = vec![format!("### commit {sha} — message, files and patch")];
    out.extend(message(ctx, &sha).iter().map(|line| cut(line)));
    out.push("files in the commit:".to_owned());
    out.extend(stat.trim_end().lines().map(cut));
    let mut used: usize = out.iter().map(|line| cost(line)).sum();
    let mut left_out = 0;
    for (path, body) in files {
        if used >= PATCH_BUDGET {
            left_out += 1; // the stat above already names it
            continue;
        }
        if LOCKS.is_match(&path) {
            let edits: Vec<&String> = body
                .iter()
                .filter(|line| line.is_empty() || line.starts_with(['+', '-']))
                .collect();
            out.push(format!(
                "@@ {path} (lockfile, {} edited lines; first ones)",
                edits.len()
            ));
            out.extend(edits.into_iter().take(LOCK_LINES).map(|line| cut(line)));
            continue;
        }
        let mut chunk = vec![format!("@@ {path}")];
        chunk.extend(body.iter().map(|line| cut(line)));
        let size: usize = chunk.iter().map(|line| cost(line)).sum();
        if used + size > PATCH_BUDGET && used > 0 {
            chunk.truncate(((PATCH_BUDGET - used) / CUT_LINE_COST).max(2));
            chunk.push("... (hunks cut)".to_owned());
        }
        used += chunk.iter().map(|line| cost(line)).sum::<usize>();
        out.extend(chunk);
    }
    if left_out > 0 {
        out.push(format!(
            "... hunks of {left_out} more files cut (named in the list above)"
        ));
    }
    let discarded: Vec<String> = HEADERS
        .captures_iter(brief)
        .filter_map(|found| found.get(1))
        .map(|header| header.as_str())
        .filter(|header| !header.starts_with("history") && !header.starts_with("commit "))
        .map(escape)
        .collect();
    let mut text = String::new();
    if !discarded.is_empty() {
        text.push_str(&format!("drop: ^(?:{})$\n", discarded.join("|")));
    }
    text.push_str(&out.join("\n"));
    text.push('\n');
    text
}

/// The commit the prompt is about: the one its quoted version names, else its
/// quoted title, else the commit whose subject shares several words with it,
/// else the one that holds the rarest words (a single rare word names no
/// commit: stay silent rather than show a neighbour).
fn find_commit(ctx: &Ctx, prompt: &str) -> Option<String> {
    if let Some((_, line)) = by_version(ctx, prompt).first() {
        return id_of(line).map(ToOwned::to_owned);
    }
    for phrase in quoted(prompt) {
        let out = git(
            ctx,
            &[
                "log",
                "-i",
                "--fixed-strings",
                "-n",
                "6",
                "--format=%h",
                &format!("--grep={phrase}"),
            ],
        );
        if let Some(sha) = out.split_whitespace().next() {
            return Some(sha.to_owned());
        }
    }
    if let Some((sha, _)) = by_subject(ctx, prompt).first() {
        return Some(sha.clone());
    }
    let words = rare_words(ctx, prompt);
    let mut last = 0;
    for count in MATCH_WORDS {
        let count = count.min(words.len());
        if count == last || count == 0 {
            continue;
        }
        last = count;
        let greps = greps(&words, count);
        let mut args = vec!["log", "-i", "--all-match", "-n", "6", "--format=%h"];
        args.extend(greps.iter().map(String::as_str));
        if let Some(sha) = git(ctx, &args).split_whitespace().next() {
            return Some(sha.to_owned());
        }
    }
    None
}

/// `sha`'s first line, then its message without the fenced output (but the
/// lines of it that carry a figure), blank runs collapsed.
fn message(ctx: &Ctx, sha: &str) -> Vec<String> {
    let shown = git(
        ctx,
        &["show", "-s", "--format=%h %ad %an%n%B", "--date=short", sha],
    );
    let lines: Vec<&str> = shown.trim().lines().collect();
    let mut out: Vec<String> = lines.iter().take(1).map(ToString::to_string).collect();
    let (mut fenced, mut kept) = (false, 0);
    for line in lines.iter().skip(1) {
        if line.starts_with("```") {
            fenced = !fenced;
        } else if !fenced {
            out.push((*line).to_owned());
        } else if kept < FENCED_KEEP
            && NUMBERS.is_match(line)
            && !line.contains("Summary")
            && !line.contains("Range")
        {
            kept += 1;
            out.push(format!("  {}", line.trim()));
        }
    }
    let mut clean: Vec<String> = Vec::new();
    for line in out {
        let after_text = clean.last().is_some_and(|last| !last.trim().is_empty());
        if !line.trim().is_empty() || after_text {
            clean.push(line);
        }
    }
    clean.truncate(MESSAGE_LINES + 1);
    clean
}

/// `(path, lines)` of each file of a unified diff, in the order git printed
/// them; the lines are the hunks, without the `index`, `---`, `+++` and mode
/// lines.
fn sections(diff: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for line in diff.lines() {
        if let Some(found) = DIFF_HEADER.captures(line) {
            let path = found.get(2).map_or("", |path| path.as_str());
            out.push((path.to_owned(), Vec::new()));
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
            .any(|prefix| line.starts_with(prefix))
        {
            body.push(line.to_owned());
        }
    }
    out
}

/// Where a file goes among the hunks: sources, then manifests, then lockfiles.
fn rank(path: &str) -> u8 {
    if LOCKS.is_match(path) {
        2
    } else {
        u8::from(MANIFEST.is_match(path))
    }
}

fn cut(line: &str) -> String {
    line.chars().take(LINE_CHARS).collect()
}

/// What a line costs the budget: its characters and the newline.
fn cost(line: &str) -> usize {
    line.chars().count() + 1
}

#[cfg(test)]
mod tests;
