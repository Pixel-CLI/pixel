// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Deterministic recognition of questions about the Pixel tool itself.
//!
//! A prompt that asks about Pixel, Pi or a Pixel operation by name is not a
//! request for code locations: its answer is the CLI's own interface, and a
//! model that answers it from memory is exactly what bypasses structured
//! retrieval ("what does `pixel find-code` do?" answered from the weights
//! instead of `pixel find-code --help`). The classifier here recognises those
//! prompts so the prompt hook points the agent at the named operation first;
//! every other prompt keeps the normal retrieval path untouched, and native
//! reads are never gated on the verdict.
//!
//! The classifier is deliberately narrow, in the same spirit as
//! `overview_intent`: whole prompts that name the tool or one of its
//! operations, plus the CLI-syntax mentions ("use pixel", "run pixel"). A
//! repository task such as "fix the pixel daemon crash" is not a question
//! about the tool and is left alone.

/// Whole-prompt shapes that ask about the tool, after [`normalize`] and
/// [`strip_affixes`]: "what is pixel", "what does pixel do", "how does the pi
/// extension work", and the like.
const TOOL_CORE: &[&str] = &[
    "what is pixel",
    "what is pixel cli",
    "what is the pixel cli",
    "what does pixel do",
    "what can pixel do",
    "how does pixel work",
    "how do i use pixel",
    "how do i run pixel",
    "how to use pixel",
    "how to run pixel",
    "how do i install pixel",
    "how to install pixel",
    "how is pixel installed",
    "explain pixel",
    "describe pixel",
    "tell me about pixel",
    "pixel help",
    "pixel documentation",
    "pixel manual",
    "what are pixel commands",
    "what pixel commands are there",
    "list pixel commands",
    "what is pi",
    "what does pi do",
    "how does pi work",
    "how do i use pi",
    "how to use pi",
    "explain pi",
    "describe pi",
    "tell me about pi",
    "what is the pi agent",
    "what is the pi coding agent",
    "what is the pi extension",
    "what does the pi extension do",
    "how does the pi extension work",
    "explain the pi extension",
    "describe the pi extension",
];

/// Substring shapes that only read as Pixel questions: a command question or
/// a CLI-syntax mention of the tool or the Pi extension.
const TOOL_ASKS: &[&str] = &[
    "pixel command for",
    "pixel command to",
    "what pixel command",
    "which pixel command",
    "is there a pixel command",
    "does pixel have",
    "pixel cli",
    "the pixel cli",
    "pi extension",
    "pi coding agent",
    "use pixel",
    "run pixel",
];

/// Lead-ins removed (repeatedly) before whole-prompt matching.
const LEAD_INS: &[&str] = &[
    "please",
    "hey",
    "hi",
    "so",
    "can you",
    "could you",
    "would you",
    "do you know",
    "help me understand",
];

/// Trailing softeners removed (repeatedly) before whole-prompt matching.
const TRAIL_INS: &[&str] = &[
    "please",
    "thanks",
    "thank you",
    "briefly",
    "quickly",
    "in short",
    "in simple terms",
];

/// The CLI's own operation nouns, in their canonical `kebab-case` spelling.
/// Common English homonyms are deliberately absent so a coding prompt is not
/// mistaken for a question about the tool: "impact" ("the impact of this
/// change"), "recall" ("recall the session"), "resolve" and "status" stay
/// out.
const OPERATIONS: &[&str] = &[
    "build-index",
    "call-path",
    "classify",
    "dig-history",
    "docs-drift",
    "evaluate-path",
    "find-code",
    "find-symbol",
    "index-stats",
    "list-areas",
    "mutants",
    "pack-context",
    "plan-rollback",
    "repo-state",
    "review-changes",
    "review-gate",
    "run-recipe",
    "scope-task",
    "search-content",
    "search-meaning",
    "sync-branch",
    "task-event",
    "task-state",
    "what-changed",
    "who-calls",
    "who-wrote",
];

/// Verbs and question frames that make an operation mention an ask about the
/// tool rather than a passing use of the words ("who calls this function").
const OPERATION_ASKS: &[&str] = &[
    "what is ",
    "what are ",
    "what does ",
    "what can ",
    "how do i use ",
    "how do i run ",
    "how to use ",
    "how to run ",
    "how does ",
    "does ",
    "is there ",
    "explain ",
    "describe ",
    "usage",
    "manual",
    "documentation",
];

/// Lowercase, punctuation and whitespace folded, contractions of `what's`
/// expanded: `find-code` and `find code` become the same words.
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

/// The spaced spelling `normalize` renders for a `kebab-case` operation.
fn spaced(operation: &str) -> String {
    operation.replace('-', " ")
}

/// The operation a normalized prompt asks about, or `None` when none of
/// [`OPERATIONS`] appears in it.
fn named_operation(core: &str) -> Option<&'static str> {
    OPERATIONS
        .iter()
        .find(|operation| core.contains(spaced(operation).as_str()))
        .copied()
}

/// Whether a normalized prompt pairs one of [`OPERATIONS`] with a question
/// frame: "explain review-gate", "how do i use find code".
fn operation_ask(core: &str) -> bool {
    OPERATION_ASKS.iter().any(|ask| core.contains(ask))
}

/// `true` when `prompt` is a deterministically recognizable question about
/// the Pixel tool itself (or the Pi agent, or one of the CLI's operations).
pub(crate) fn is_pixel_question(prompt: &str) -> bool {
    let text = normalize(prompt);
    let core = strip_affixes(&text, LEAD_INS, true);
    let core = strip_affixes(core, TRAIL_INS, false);
    let names_tool =
        TOOL_CORE.contains(&core) || TOOL_ASKS.iter().any(|phrase| core.contains(phrase));
    names_tool || (operation_ask(core) && named_operation(core).is_some())
}

/// The prompt-hook pointer for a Pixel question: names the operation the
/// prompt asks about (or the command list), and `None` for every other
/// prompt. The generic Pixel-first guidance already rides every indexed
/// prompt; this makes the relevant operation the first-class answer for the
/// prompt that names it.
pub(crate) fn pixel_question_note(prompt: &str) -> Option<String> {
    if !is_pixel_question(prompt) {
        return None;
    }
    let command = named_operation(&normalize(prompt)).map_or_else(
        || "pixel --help".to_string(),
        |operation| format!("pixel {operation}"),
    );
    Some(format!(
        "[PIXEL:TASK_CONTEXT] This prompt asks about the Pixel tool itself, not for a code location. \
         Answer it from the CLI's own interface, not from memory: run `{command}` \
         (add `--help` for the operation's surface) and read its output before explaining. \
         This is a pointer, not a read/edit boundary."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_questions_are_recognised() {
        for prompt in [
            "what is pixel",
            "what does Pixel do?",
            "what can pixel do",
            "how does pixel work",
            "How do I use Pixel?",
            "how to use pixel",
            "how do i run pixel",
            "how do i install pixel",
            "how is pixel installed",
            "explain pixel",
            "describe pixel",
            "tell me about pixel",
            "pixel help",
            "pixel documentation",
            "what are pixel commands",
            "list pixel commands",
            "what pixel commands are there",
            "what is the pixel cli",
            "what is pi",
            "explain pi",
            "what is the pi agent",
            "what is the pi coding agent",
            "what is the pi extension",
            "what does the pi extension do",
            "how does the pi extension work",
            "explain the pi extension",
            "please explain pixel briefly",
            "does pixel have a command to find callers",
            "is there a pixel command for scope-task",
            "which pixel command shows what changed",
            "use pixel find-code",
            "how do I use pixel impact",
            "what does pixel classify do",
            "what is search-content",
            "how do i use find code",
            "explain review-gate",
            "how does build-index work",
            "what does who-calls do",
            "how do i run mutants",
            "what is the docs drift check",
        ] {
            assert!(is_pixel_question(prompt), "{prompt}");
        }
    }

    #[test]
    fn unrelated_prompts_are_not_pixel_questions() {
        for prompt in [
            "",
            "fix the login bug",
            "implement the isolated worker",
            "what does this repo do",
            "summarize the meeting notes",
            "who calls this function",
            "help me find code that uses the session cookie",
            "how do i find code that migrates the database",
            "calculate pi to ten digits",
            "is pi rational",
            "why is the build failing",
            "add a pagination flag",
            "refactor the auth module",
            "what is the impact of changing the timeout",
            "the search content index is broken",
            "describe the pixel daemon crash to me",
            "fix the pixel daemon crash",
            "how does the pixel daemon start",
            "what does the pixel graph store",
        ] {
            assert!(!is_pixel_question(prompt), "{prompt}");
        }
    }

    #[test]
    fn affixes_are_stripped_for_whole_prompt_shapes() {
        assert!(is_pixel_question("please explain pixel please"));
        assert!(is_pixel_question("hey, what is pixel?"));
        assert!(!is_pixel_question("please pixel"));
    }

    #[test]
    fn pixel_question_note_names_the_operation_and_skips_other_prompts() {
        let note = pixel_question_note("how do i use pixel find-code").unwrap();
        assert!(note.starts_with("[PIXEL:TASK_CONTEXT]"), "{note}");
        assert!(note.contains("`pixel find-code`"), "{note}");
        assert!(note.contains("not from memory"), "{note}");
        assert!(note.ends_with("not a read/edit boundary."), "{note}");

        let generic = pixel_question_note("what is pixel").unwrap();
        assert!(generic.contains("`pixel --help`"), "{generic}");
        assert!(!generic.contains("`pixel find-code`"), "{generic}");

        assert_eq!(pixel_question_note("fix the login bug"), None);
        assert_eq!(pixel_question_note(""), None);
    }

    #[test]
    fn named_operation_finds_either_spelling() {
        let text = || normalize("what does pixel find-code do");
        assert_eq!(named_operation(&text()), Some("find-code"));
        assert_eq!(
            named_operation(&normalize("how does build-index work")),
            Some("build-index")
        );
        assert_eq!(named_operation(&normalize("what is pixel")), None);
    }
}
