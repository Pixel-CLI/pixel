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
/// prompt-submit hook recognizes the same envelopes as recall does.
pub const ORCHESTRATOR_PREFIXES: &[&str] = &[
    "<system-reminder>",
    "<task-notification>",
    "<command-name>",
    "<command-message>",
    "<local-command",
    "<bash-input>",
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
    }
}
