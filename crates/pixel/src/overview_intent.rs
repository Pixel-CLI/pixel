// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Deterministic detection of "what does this repo do" prompts.
//!
//! An overview question names no code. Fed to the keyword retrievers it
//! matches the word "repo" itself: `repo.rs`, `repo_git.rs` and string
//! literals in `guard.rs` come back as "targets", and an agent that trusts
//! them reads the wrong files. The answer to such a question is the project's
//! own description, so the classifier here routes it there instead.
//!
//! The classifier is deliberately narrow: the whole prompt, minus polite
//! lead-ins and trailing softeners, must be one of a fixed set of shapes. A
//! prompt that also asks for a change ("what does this repo do, then fix the
//! bug in guard.rs") is not an overview question and keeps normal retrieval.

use std::path::Path;

/// Nouns that stand for the whole project.
const PROJECT_NOUNS: &[&str] = &["repo", "repository", "project", "codebase", "code base"];

/// Determiners in front of a project noun.
const DETERMINERS: &[&str] = &["this", "the", "our", "my"];

/// Lead-ins removed (repeatedly) before matching.
const LEAD_INS: &[&str] = &[
    "please",
    "hey",
    "hi",
    "ok",
    "okay",
    "so",
    "just",
    "can you",
    "could you",
    "would you",
    "tell me",
    "show me",
    "let me know",
    "i want to know",
    "i would like to know",
    "i want to understand",
    "help me understand",
];

/// Trailing softeners removed (repeatedly) before matching.
const TRAIL_INS: &[&str] = &[
    "please",
    "thanks",
    "thank you",
    "to me",
    "briefly",
    "quickly",
    "in short",
    "in a nutshell",
    "at a high level",
    "in a few words",
];

/// Verbs that ask for a description of the project when followed by it.
const DESCRIBE_VERBS: &[&str] = &[
    "explain",
    "describe",
    "summarize",
    "summarise",
    "introduce",
    "walk me through",
    "about",
    "an overview of",
    "overview of",
    "give me an overview of",
];

/// Files that describe a project, in the order to read them.
pub(crate) const OVERVIEW_FILES: &[&str] = &["README.md", "ARCHITECTURE.md", "Cargo.toml"];

/// Lowercase, contractions of `what's` expanded, everything that is not a
/// letter or digit a single space.
fn normalize(prompt: &str) -> String {
    let lowered = prompt.to_lowercase().replace("what's", "what is");
    lowered
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Remove `affixes` from one end of `text`, repeatedly, until none applies.
fn strip_affixes<'a>(mut text: &'a str, affixes: &[&str], leading: bool) -> &'a str {
    'again: loop {
        for affix in affixes {
            let stripped = if leading {
                text.strip_prefix(affix)
                    .and_then(|rest| rest.strip_prefix(' '))
            } else {
                text.strip_suffix(affix)
                    .and_then(|rest| rest.strip_suffix(' '))
            };
            if let Some(rest) = stripped {
                text = rest;
                continue 'again;
            }
        }
        return text;
    }
}

/// Every core phrase that is an overview question for `subject` (a
/// determiner and a project noun, e.g. `this repo`).
fn core_phrases(subject: &str) -> Vec<String> {
    let (det, noun) = subject.split_once(' ').unwrap_or(("", subject));
    let mut phrases = vec![
        format!("what does {subject} do"),
        format!("what {subject} does"),
        format!("what is {subject} about"),
        format!("what is {subject} for"),
        format!("what {det} {noun} is about"),
        format!("what {det} {noun} is for"),
        format!("what is {subject}"),
        format!("how does {subject} work"),
        format!("{subject} overview"),
    ];
    phrases.extend(
        DESCRIBE_VERBS
            .iter()
            .map(|verb| format!("{verb} {subject}")),
    );
    phrases
}

/// `true` when the whole prompt asks what the project is or does.
pub(crate) fn is_overview_prompt(prompt: &str) -> bool {
    let normalized = normalize(prompt);
    let core = strip_affixes(&normalized, LEAD_INS, true);
    let core = strip_affixes(core, TRAIL_INS, false);
    if core == "give me an overview" || core == "overview" {
        return true;
    }
    PROJECT_NOUNS.iter().any(|noun| {
        DETERMINERS.iter().any(|det| {
            core_phrases(&format!("{det} {noun}"))
                .iter()
                .any(|phrase| phrase == core)
        })
    })
}

/// The project-description files that exist under `root`, in reading order.
fn existing_overview_files(root: &Path) -> Vec<&'static str> {
    OVERVIEW_FILES
        .iter()
        .copied()
        .filter(|name| root.join(name).is_file())
        .collect()
}

/// What `find-code` and `scope-task` print for an overview query instead of
/// keyword matches.
pub(crate) fn overview_answer(root: &Path) -> String {
    let files = existing_overview_files(root);
    if files.is_empty() {
        return "no concept match; this reads as an overview question and the repo has no README.md, ARCHITECTURE.md or Cargo.toml; list the top-level directories".to_owned();
    }
    format!("no concept match; read {}", files.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overview_questions_are_recognised() {
        for prompt in [
            "what does this repo do",
            "What does this repo do?",
            "tell me what this repo does",
            "Tell me what this repo does.",
            "what's this repo about?",
            "what is this project for",
            "can you tell me about the codebase",
            "please explain this repository",
            "describe the project briefly",
            "summarize this codebase to me",
            "give me an overview of this repo",
            "hey, what does the repo do please",
            "walk me through this project",
            "how does this codebase work",
            "what is this repo",
            "give me an overview",
            "overview",
            "this repo overview",
            "what does this code base do",
        ] {
            assert!(is_overview_prompt(prompt), "{prompt}");
        }
    }

    #[test]
    fn task_prompts_are_not_overview_questions() {
        for prompt in [
            "",
            "fix the bug in guard.rs",
            "what does the repo_git module do",
            "where is the repo cache cleared",
            "what does this function do",
            "what does this repo do, then fix the bug in guard.rs",
            "explain this repo layout in guard.rs",
            "what does your project do",
            "tell me",
            "please",
            "how does the daemon work",
            "add a repo flag",
        ] {
            assert!(!is_overview_prompt(prompt), "{prompt}");
        }
    }

    #[test]
    fn normalization_folds_case_punctuation_and_contractions() {
        assert_eq!(normalize("  What's THIS,  repo?! "), "what is this repo");
        assert_eq!(normalize("a-b_c"), "a b c");
    }

    #[test]
    fn affixes_are_stripped_at_word_boundaries_only() {
        assert_eq!(strip_affixes("so so hi x", LEAD_INS, true), "x");
        // `so` is not a prefix of the word `something`.
        assert_eq!(strip_affixes("something", LEAD_INS, true), "something");
        assert_eq!(strip_affixes("x please thanks", TRAIL_INS, false), "x");
        // `please` is not a suffix of `displease`.
        assert_eq!(strip_affixes("displease", TRAIL_INS, false), "displease");
        // An affix alone with nothing after it is left as it is.
        assert_eq!(strip_affixes("please", LEAD_INS, true), "please");
    }

    /// A scratch directory holding empty files of the given names.
    fn fixture(files: &[&str]) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("pixel-overview-intent-{}-{n}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        for name in files {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        dir
    }

    #[test]
    fn the_answer_names_only_the_files_that_exist_in_reading_order() {
        let dir = fixture(&["Cargo.toml", "README.md"]);
        assert_eq!(
            overview_answer(&dir),
            "no concept match; read README.md, Cargo.toml"
        );
        let all = fixture(OVERVIEW_FILES);
        assert_eq!(
            overview_answer(&all),
            "no concept match; read README.md, ARCHITECTURE.md, Cargo.toml"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&all).ok();
    }

    #[test]
    fn a_directory_named_like_a_description_file_is_not_offered() {
        let dir = fixture(&[]);
        std::fs::create_dir(dir.join("README.md")).unwrap();
        assert!(!overview_answer(&dir).contains("read README.md"));
        assert!(
            overview_answer(&dir).contains("no README.md"),
            "{}",
            overview_answer(&dir)
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
