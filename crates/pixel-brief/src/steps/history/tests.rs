// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use std::time::{Duration, Instant};

use super::*;
use crate::steps::testing::{Repo, texts};
use crate::steps::{Held, Steps, blocks, blocks_held};

const MESSAGE: &str = "abc1234 2026-03-01 Ada Lovelace\nadd retry backoff to the client\n\nRetries now wait 2^n * 100 ms.\n\n```bench\nlatency 1.5x faster\nSummary line\n```\n";
const STAT: &str = " src/client.rs | 12 ++++++++----\n Cargo.lock | 4 ++--\n 2 files changed, 8 insertions(+), 8 deletions(-)\n";
const DIFF: &str = "diff --git a/Cargo.lock b/Cargo.lock\nindex 1..2 100644\n--- a/Cargo.lock\n+++ b/Cargo.lock\n@@ -1,2 +1,2 @@\n-version = \"1\"\n+version = \"2\"\ndiff --git a/src/client.rs b/src/client.rs\nindex 3..4 100644\n--- a/src/client.rs\n+++ b/src/client.rs\n@@ -1,2 +1,3 @@\n fn send() {}\n+fn backoff() {}\n";

/// A repository whose git answers the commit `abc1234`.
fn repo(extra: &[(&str, &str)]) -> Repo {
    let mut answers: Vec<(&str, &str)> = extra.to_vec();
    answers.extend([
        ("--patch", "src/client.rs | 12 +++++\n+fn backoff() {}\n"),
        ("--stat=100 --diff-merges", STAT),
        ("-U1", DIFF),
        ("show -s", MESSAGE),
        ("tag --contains", "v1.2.0\nv1.3.0\n"),
    ]);
    Repo::new(&[]).with_git(&answers)
}

fn ask(repo: &Repo, typed: &str, kind: QuestionKind) -> Outcome {
    let deadline = Instant::now() + Duration::from_secs(5);
    blocks(typed, kind, Steps::default(), &[], repo, deadline)
}

const QUOTED_TITLE: &str =
    "which commit \"add retry backoff to the client\" and which files did it change";
const TITLE_LINE: &str = "abc1234 2026-03-01 add retry backoff to the client\n";

#[test]
fn a_quoted_title_gets_the_commit_message_files_and_patch_and_takes_the_rest_of_the_pack() {
    let repo = repo(&[("--fixed-strings", TITLE_LINE)]);
    let out = ask(&repo, QUOTED_TITLE, QuestionKind::History);
    assert!(out.rescuing);
    assert_eq!(
        texts(&out.lead),
        ["### commit abc1234 — message, files and patch\n\
             abc1234 2026-03-01 Ada Lovelace\n\
             add retry backoff to the client\n\
             \n\
             Retries now wait 2^n * 100 ms.\n\
             \n\
             \x20 latency 1.5x faster\n\
             files in the commit:\n\
             \x20src/client.rs | 12 ++++++++----\n\
             \x20Cargo.lock | 4 ++--\n\
             \x202 files changed, 8 insertions(+), 8 deletions(-)\n\
             @@ src/client.rs\n\
             @@ -1,2 +1,3 @@\n\
             \x20fn send() {}\n\
             +fn backoff() {}\n\
             @@ Cargo.lock (lockfile, 2 edited lines; first ones)\n\
             -version = \"1\"\n\
             +version = \"2\""]
    );
    // The candidates list rides after; the commit block is not repeated.
    assert_eq!(
        texts(&out.append),
        [format!(
            "### history — commits matching retry, backoff, client\n{}",
            TITLE_LINE.trim_end()
        )]
    );
    assert!(
        out.drops
            .drops_head("src/client.rs:1-3 — send definition (function)")
    );
    assert!(!out.drops.drops_head("history — commits matching retry"));
    assert!(
        !out.drops
            .drops_head("commit abc1234 — message, files and patch")
    );
}

#[test]
fn the_fallback_alone_prints_the_candidates_the_patch_and_the_first_tag() {
    let repo = repo(&[("--fixed-strings", TITLE_LINE)]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let steps = Steps::default()
        .without(&["cand-k-commit-patch".to_string()])
        .unwrap();
    let out = blocks(
        QUOTED_TITLE,
        QuestionKind::History,
        steps,
        &[],
        &repo,
        deadline,
    );
    assert!(out.lead.is_empty());
    assert!(out.drops.is_empty());
    let shown = texts(&out.append);
    assert_eq!(shown.len(), 2);
    assert_eq!(
        shown[1],
        "### commit abc1234 — message, files and patch\n\
         abc1234 2026-03-01 Ada Lovelace\n\
         add retry backoff to the client\n\
         \n\
         Retries now wait 2^n * 100 ms.\n\
         \n\
         ```bench\n\
         latency 1.5x faster\n\
         Summary line\n\
         ```\n\
         src/client.rs | 12 +++++\n\
         +fn backoff() {}\n\
         first tag containing it: v1.2.0"
    );
}

#[test]
fn a_version_the_prompt_quotes_names_the_commit_its_release_tag_points_at() {
    let tagged = "1111111111111111111111111111111111111111";
    let repo = repo(&[
        (
            "--grep=(^|",
            "1111111111111111111111111111111111111111\t1111111 2026-02-01 client: bump to 2.3.0\n",
        ),
        ("rev-list", "1111111111111111111111111111111111111111\n"),
    ]);
    assert_eq!(tagged.len(), 40);
    let out = ask(
        &repo,
        "what changed in the client at release v2.3.0",
        QuestionKind::History,
    );
    assert_eq!(
        out.lead[0].head(),
        "commit 1111111 — message, files and patch"
    );
    assert_eq!(out.append[0].head(), "history — commits matching 2.3.0");
    assert_eq!(
        out.append[0].text(),
        "### history — commits matching 2.3.0\n1111111 2026-02-01 client: bump to 2.3.0"
    );
}

#[test]
fn a_bare_number_that_no_tag_or_subject_shares_a_word_with_names_no_commit() {
    let repo = Repo::new(&[]).with_git(&[(
        "--grep=(^|",
        "2222222222222222222222222222222222222222\t2222222 2026-02-01 unrelated housekeeping 3.10\n",
    )]);
    let out = ask(
        &repo,
        "what changed in the 3.10 release",
        QuestionKind::History,
    );
    assert!(out.lead.is_empty() && out.append.is_empty() && !out.rescuing);
}

#[test]
fn subjects_that_share_several_rare_words_name_the_commit() {
    let rows = "aaaaaaaa\tabc1111 2026-01-01 add retry backoff to client requests\n\
                bbbbbbbb\tdef2222 2026-01-02 fix typo in readme\n\
                cccccccc\tghi3333 2026-01-03 tune retry window\n";
    let repo = repo(&[("-n 20000 --format=%H", rows)]);
    let out = ask(
        &repo,
        "which commit added the retry backoff for client requests",
        QuestionKind::History,
    );
    assert_eq!(
        out.lead[0].head(),
        "commit abc1111 — message, files and patch"
    );
    assert_eq!(
        out.append[0].text(),
        "### history — commits matching retry, backoff, client\n\
         abc1111 2026-01-01 add retry backoff to client requests"
    );
}

#[test]
fn the_rarest_word_is_searched_and_the_pickaxe_is_the_fallbacks_alone() {
    let repo = Repo::new(&[]).with_git(&[
        (
            "-n 20000 --format=%B",
            "zanzibar support\0other change\0third\0",
        ),
        ("--all-match", ""),
        ("-Szanzibar", "fed4321 2026-02-02 add zanzibar\n"),
    ]);
    let out = ask(&repo, "when was zanzibar introduced", QuestionKind::History);
    assert!(out.lead.is_empty(), "the patch step stays silent");
    assert_eq!(
        texts(&out.append),
        ["### history — commits matching zanzibar\nfed4321 2026-02-02 add zanzibar"]
    );
    let found = repo.with_git(&[
        ("-n 20000 --format=%B", "zanzibar support\0other\0"),
        ("--all-match", "fed4321 2026-02-02 zanzibar support\n"),
    ]);
    let out = ask(
        &found,
        "when was zanzibar introduced",
        QuestionKind::History,
    );
    assert_eq!(
        texts(&out.append).len(),
        1,
        "no show answers: no commit block"
    );
}

#[test]
fn only_a_history_question_with_a_history_word_and_no_commits_in_the_brief_runs() {
    let repo = repo(&[("--fixed-strings", TITLE_LINE)]);
    let silent =
        |out: Outcome| out.lead.is_empty() && out.append.is_empty() && out.drops.is_empty();
    assert!(silent(ask(&repo, QUOTED_TITLE, QuestionKind::Lookup)));
    assert!(silent(ask(
        &repo,
        "what is \"add retry backoff to the client\" about",
        QuestionKind::History
    )));
    let deadline = Instant::now() + Duration::from_secs(5);
    assert!(silent(blocks_held(
        QUOTED_TITLE,
        QuestionKind::History,
        Steps::default(),
        &[],
        Held { history: true },
        &repo,
        deadline,
    )));
    let off = Steps::default()
        .without(&[
            "history-fallback".to_string(),
            "cand-k-commit-patch".to_string(),
        ])
        .unwrap();
    assert!(silent(blocks(
        QUOTED_TITLE,
        QuestionKind::History,
        off,
        &[],
        &repo,
        deadline
    )));
}

#[test]
fn the_hunks_stop_at_the_budget_and_name_the_files_left_out() {
    let line = "x".repeat(200);
    let mut diff = String::new();
    for file in ["src/a.rs", "src/b.rs"] {
        diff.push_str(&format!("diff --git a/{file} b/{file}\n@@ -1 +1 @@\n"));
        for _ in 0..200 {
            diff.push_str(&format!("+{line}\n"));
        }
    }
    let repo = Repo::new(&[]).with_git(&[
        ("--fixed-strings", TITLE_LINE),
        ("--stat=100 --diff-merges", STAT),
        ("-U1", &diff),
        ("show -s", MESSAGE),
    ]);
    let out = ask(&repo, QUOTED_TITLE, QuestionKind::History);
    let text = out.lead[0].text();
    assert!(text.contains("@@ src/a.rs\n"), "{text}");
    assert!(text.contains("... (hunks cut)"), "{text}");
    assert!(
        text.contains("@@ src/b.rs\n"),
        "a file past the budget still names itself"
    );
    assert!(text.chars().count() < 6_500, "{}", text.chars().count());
}

#[test]
fn versions_stems_and_subject_words_follow_the_probe() {
    assert_eq!(
        versions("since 3.10 and v5.3.0, not x1.2 or 1.2.3x or 2.0. again 3.10"),
        ["3.10", "5.3.0", "2.0"]
    );
    assert_eq!(
        stems("which retries Changed the clients"),
        BTreeSet::from(["retr".to_string(), "clie".to_string()])
    );
    assert_eq!(
        subject_words("Which commit added the retry-backoff for I/O?"),
        ["retry", "backoff"]
    );
}

#[test]
fn a_message_keeps_the_numbers_of_fenced_output_and_collapses_blank_runs() {
    assert_eq!(
        message("h 2026 A\ntitle\n\n\n\nbody\n```\nlatency 2x faster\nplain text\nRange 3x\n```\n"),
        ["h 2026 A", "title", "", "body", "  latency 2x faster"]
    );
}

#[test]
fn a_diff_is_cut_into_files_without_its_header_lines() {
    assert_eq!(
        sections(DIFF),
        [
            (
                "Cargo.lock".to_string(),
                vec![
                    "@@ -1,2 +1,2 @@".to_string(),
                    "-version = \"1\"".to_string(),
                    "+version = \"2\"".to_string()
                ]
            ),
            (
                "src/client.rs".to_string(),
                vec![
                    "@@ -1,2 +1,3 @@".to_string(),
                    " fn send() {}".to_string(),
                    "+fn backoff() {}".to_string()
                ]
            ),
        ]
    );
}

#[test]
fn a_commit_whose_file_list_fills_the_budget_names_its_files_and_cuts_every_hunk() {
    let stat: String = (0..70)
        .map(|n| format!(" src/file_{n:03}.rs | {}\n", "+".repeat(60)))
        .collect();
    let repo = Repo::new(&[]).with_git(&[
        ("--fixed-strings", TITLE_LINE),
        ("--stat=100 --diff-merges", &stat),
        ("-U1", DIFF),
        ("show -s", MESSAGE),
    ]);
    let out = ask(&repo, QUOTED_TITLE, QuestionKind::History);
    let text = out.lead[0].text();
    assert!(text.contains(" src/file_069.rs |"), "{text}");
    assert!(!text.contains("@@ src/client.rs"), "{text}");
    assert!(
        text.ends_with("... hunks of 2 more files cut (named in the list above)"),
        "{text}"
    );
}
