// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Which prompts are continuations rather than tasks: acknowledgements,
//! greetings and the envelopes a harness submits on the user's behalf. The
//! per-prompt evidence brief skips them.

/// Trivial continuations that are almost certainly not new tasks, and
/// harness envelopes (see [`is_harness_envelope`]), which are not the user's.
pub(crate) fn is_trivial_continuation(prompt: &str) -> bool {
    if is_harness_envelope(prompt) {
        return true;
    }
    let trimmed = prompt
        .trim()
        .trim_end_matches(['.', '!', '?'])
        .to_lowercase();
    if trimmed.is_empty() {
        return true;
    }
    let words = trimmed.split_whitespace().count();
    if words <= 3 {
        // Single-word or common short affirmative/acknowledgment phrases
        if matches!(
            trimmed.as_str(),
            "yes"
                | "hi"
                | "hello"
                | "hey"
                | "good morning"
                | "good afternoon"
                | "good evening"
                | "y"
                | "no"
                | "n"
                | "ok"
                | "okay"
                | "continue"
                | "go"
                | "proceed"
                | "thanks"
                | "thank you"
                | "done"
                | "next"
                | "sure"
                | "correct"
                | "right"
                | "exactly"
                | "yep"
                | "yeah"
                | "nope"
                | "fine"
                | "good"
                | "great"
                | "perfect"
                | "looks good"
                | "lgtm"
                | "go ahead"
                | "sounds good"
                | "do it"
                | "ship it"
                | "go for it"
                | "proceed with that"
                | "all good"
        ) {
            return true;
        }
    }
    false
}

const SYSTEM_REMINDER_OPEN: &str = "<system-reminder>";
const SYSTEM_REMINDER_CLOSE: &str = "</system-reminder>";

/// A prompt the harness submitted on the user's behalf: a background-task
/// `<task-notification>`, a slash-command or local-command wrapper, or
/// nothing but `<system-reminder>` blocks. It must neither rewrite the task
/// packet nor open a boundary. Only the opening counts (after any leading
/// reminders), so a human prompt that quotes an envelope mid-text is still a
/// prompt; the prefixes are recall's, so both classifiers agree.
fn is_harness_envelope(prompt: &str) -> bool {
    let mut rest = prompt.trim_start();
    while let Some(after) = rest.strip_prefix(SYSTEM_REMINDER_OPEN) {
        let Some(end) = after.find(SYSTEM_REMINDER_CLOSE) else {
            return true;
        };
        rest = after[end + SYSTEM_REMINDER_CLOSE.len()..].trim_start();
    }
    rest.is_empty()
        || pixel_recall::intent::ORCHESTRATOR_PREFIXES
            .iter()
            .any(|prefix| rest.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trivial_continuations_detected() {
        assert!(is_trivial_continuation("yes"));
        assert!(is_trivial_continuation("OK"));
        assert!(is_trivial_continuation("  continue  "));
        assert!(is_trivial_continuation("thanks"));
        assert!(is_trivial_continuation("thank you"));
        assert!(is_trivial_continuation("looks good"));
        assert!(is_trivial_continuation("lgtm"));
        assert!(is_trivial_continuation("go ahead"));
        assert!(is_trivial_continuation("sounds good"));
        assert!(is_trivial_continuation("ship it"));
        assert!(is_trivial_continuation(""));
    }

    #[test]
    fn non_trivial_prompts_not_flagged() {
        assert!(!is_trivial_continuation("now let's set up docker"));
        assert!(!is_trivial_continuation("fix the login bug"));
        assert!(!is_trivial_continuation(
            "can you also add tests for the auth module"
        ));
    }

    const TASK_NOTIFICATION: &str = "<task-notification>\n<task-id>b1f0c2</task-id>\n<status>completed</status>\n<summary>Agent \"fix auth\" completed</summary>\n</task-notification>";

    #[test]
    fn harness_envelopes_are_continuations() {
        for prompt in [
            TASK_NOTIFICATION,
            "  <task-notification><task-id>x</task-id></task-notification>",
            "<system-reminder>ctx</system-reminder>",
            "<system-reminder>a</system-reminder>\n<system-reminder>b</system-reminder>\n",
            "<system-reminder>unterminated",
            "<system-reminder>ctx</system-reminder><task-notification>done</task-notification>",
            "<command-name>/clear</command-name>",
            "<command-message>review</command-message>",
            "<local-command-caveat>Caveat: generated locally</local-command-caveat>",
            "<local-command-stdout>ok</local-command-stdout>",
        ] {
            assert!(is_harness_envelope(prompt), "{prompt}");
            assert!(is_trivial_continuation(prompt), "{prompt}");
        }
    }

    #[test]
    fn prompts_that_quote_an_envelope_are_still_prompts() {
        for prompt in [
            "fix the hook: a <task-notification> prompt overwrites the task",
            "why does task-notification reach the packet",
            "<system-reminder>ctx</system-reminder>\nfix the login bug",
            "<system-reminder>ctx</system-reminder>fix auth",
            "implement <command-name> parsing",
        ] {
            assert!(!is_harness_envelope(prompt), "{prompt}");
            assert!(!is_trivial_continuation(prompt), "{prompt}");
        }
    }

    #[test]
    fn greetings_and_punctuated_continuations_skip_lookup() {
        for prompt in ["Hello!", "hi", "Good morning.", "OK!", "yes."] {
            assert!(is_trivial_continuation(prompt), "{prompt}");
        }
        assert!(!is_trivial_continuation("fix auth"));
    }
}
