// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Human-vs-orchestrator classification for user turns.
//!
//! Harness machinery injects text as the "user": system reminders, task
//! notifications, command wrappers, agent handoffs. Those turns stay
//! lexically searchable but are excluded from semantic embedding, and
//! derived session titles must never quote them.

use crate::model::IntentSource;

/// Openings of a turn the harness wrote, not the human. Public so the
/// prompt-submit hook recognizes the same envelopes as recall does. The tags
/// left open (`<local-command`, and Codex's two) also match one that carries an
/// attribute.
pub const ORCHESTRATOR_PREFIXES: &[&str] = &[
    "<system-reminder>",
    "<task-notification>",
    "<command-name>",
    "<command-message>",
    "<local-command",
    "<bash-input>",
    "<realtime_delegation",
    "<send_user_message_question_reply",
    "[SYSTEM NOTIFICATION",
    "Caveat: The messages below",
    "[Request interrupted",
];

const ORCHESTRATOR_MARKERS: &[&str] = &["CMUX agent handoff", "cmux-agent-send --queue"];

pub fn classify_user_text(text: &str) -> IntentSource {
    let trimmed = text.trim_start();
    for p in ORCHESTRATOR_PREFIXES {
        if trimmed.starts_with(p) {
            return IntentSource::Orchestrator;
        }
    }
    for m in ORCHESTRATOR_MARKERS {
        if trimmed.contains(m) {
            return IntentSource::Orchestrator;
        }
    }
    IntentSource::Human
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies() {
        assert_eq!(
            classify_user_text("<system-reminder>ctx</system-reminder>"),
            IntentSource::Orchestrator
        );
        assert_eq!(
            classify_user_text("  <task-notification>done"),
            IntentSource::Orchestrator
        );
        assert_eq!(classify_user_text("fix the login bug"), IntentSource::Human);
        assert_eq!(
            classify_user_text("Caveat: The messages below were generated"),
            IntentSource::Orchestrator
        );
        assert_eq!(
            classify_user_text(
                "<realtime_delegation><input>where is x</input></realtime_delegation>"
            ),
            IntentSource::Orchestrator
        );
        assert_eq!(
            classify_user_text(
                "<send_user_message_question_reply>[]</send_user_message_question_reply>"
            ),
            IntentSource::Orchestrator
        );
    }

    #[test]
    fn classifies_codex_delegation_and_question_reply_envelopes_as_orchestrator() {
        let delegation = [
            "<realtime_delegation>",
            "  <input>where is fetchUser</input>",
            "</realtime_delegation>",
        ]
        .join("\n");
        let reply = [
            "<send_user_message_question_reply>",
            "[{\"question\":\"which file\",\"answer\":\"src/api.rs\"}]",
            "</send_user_message_question_reply>",
        ]
        .join("\n");
        for text in [
            delegation.as_str(),
            reply.as_str(),
            // Leading blanks, and a tag that carries an attribute.
            "\n  <realtime_delegation>x</realtime_delegation>",
            "<realtime_delegation id=\"rt-1\">x</realtime_delegation>",
            "<send_user_message_question_reply id=\"q-1\">[]</send_user_message_question_reply>",
        ] {
            assert_eq!(
                classify_user_text(text),
                IntentSource::Orchestrator,
                "{text}"
            );
        }
    }

    #[test]
    fn classifies_a_human_who_names_a_codex_envelope_mid_sentence_as_human() {
        for text in [
            "why does <realtime_delegation> reach the brief",
            "the send_user_message_question_reply tag is missing from the list",
            "fix the hook:\n<send_user_message_question_reply> starts a brief",
        ] {
            assert_eq!(classify_user_text(text), IntentSource::Human, "{text}");
        }
    }
}
