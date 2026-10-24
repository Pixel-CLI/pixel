// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The one JSON object a hook prints back to its host (`specs/hosts.md`,
//! "What each event may print").

use serde_json::{Value, json};

use crate::hosts::{Event, Host};

/// The envelope that hands `text` to the model on `host` at `event`.
///
/// Claude Code, Codex, Devin and Gemini share the Claude contract
/// (`hookSpecificOutput.additionalContext`, the event named as the host names
/// it); Antigravity injects a step into the trajectory at a prompt; pi and
/// OpenCode read a bare decision from their extension.
pub fn context_envelope(host: Host, event: Event, text: &str) -> Value {
    if host.bare_decision() {
        return json!({"context": text});
    }
    if host == Host::Antigravity && event == Event::PromptSubmit {
        return json!({"injectSteps": [{"ephemeralMessage": text}]});
    }
    json!({"hookSpecificOutput": {
        "hookEventName": host.registration(event).unwrap_or_default(),
        "additionalContext": text,
    }})
}

/// The envelope that stops `host` from ending its turn, telling the model
/// `reason`: Claude Code, Codex and Devin block the stop, Gemini denies the
/// end of the agent turn, pi and OpenCode read `continue` from their
/// extension, which sends the reason as a message that starts another turn.
/// Antigravity's invocation end cannot continue a turn: `None`.
pub fn block_envelope(host: Host, reason: &str) -> Option<Value> {
    match host {
        Host::Claude | Host::Codex | Host::Devin => {
            Some(json!({"decision": "block", "reason": reason}))
        }
        Host::Gemini => Some(json!({"decision": "deny", "reason": reason})),
        Host::Pi | Host::OpenCode => Some(json!({"context": reason, "continue": true})),
        Host::Antigravity => None,
    }
}

/// The model-visible text of an envelope [`context_envelope`] built.
pub fn context_of(host: Host, envelope: &Value) -> Option<&str> {
    if host.bare_decision() {
        envelope.get("context")?.as_str()
    } else if host == Host::Antigravity && envelope.get("injectSteps").is_some() {
        envelope
            .pointer("/injectSteps/0/ephemeralMessage")?
            .as_str()
    } else {
        envelope
            .pointer("/hookSpecificOutput/additionalContext")?
            .as_str()
    }
}

/// Puts the metrics `line` where the user of `host` can read it: a
/// `systemMessage` the host draws; a request to repeat it, appended to the
/// context, where the host draws nothing; a `notice` for an extension to
/// show.
pub fn attach_line(host: Host, envelope: &mut Value, line: &str) {
    if host.shows_system_message() {
        envelope["systemMessage"] = json!(line);
    } else if host.bare_decision() {
        envelope["notice"] = json!(line);
    } else if host.relays_through_model() {
        let pointer = if envelope.get("injectSteps").is_some() {
            "/injectSteps/0/ephemeralMessage"
        } else {
            "/hookSpecificOutput/additionalContext"
        };
        if let Some(context) = envelope.pointer_mut(pointer)
            && let Some(text) = context.as_str()
        {
            *context = json!(format!(
                "{text}\n\nEnd your answer with this line, verbatim: {line}"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRIEF: &str = "[PIXEL:BRIEF]\nbody";

    #[test]
    fn claude_codex_and_devin_get_the_claude_contract() {
        for host in [Host::Claude, Host::Codex, Host::Devin] {
            assert_eq!(
                context_envelope(host, Event::PromptSubmit, BRIEF),
                json!({"hookSpecificOutput": {
                    "hookEventName": "UserPromptSubmit", "additionalContext": BRIEF}}),
                "{host:?}"
            );
        }
    }

    #[test]
    fn gemini_names_the_event_as_it_does() {
        assert_eq!(
            context_envelope(Host::Gemini, Event::PromptSubmit, BRIEF),
            json!({"hookSpecificOutput": {
                "hookEventName": "BeforeAgent", "additionalContext": BRIEF}})
        );
        assert_eq!(
            context_envelope(Host::Gemini, Event::PostEdit, "x")["hookSpecificOutput"]["hookEventName"],
            "AfterTool"
        );
    }

    #[test]
    fn antigravity_injects_a_step_at_a_prompt() {
        assert_eq!(
            context_envelope(Host::Antigravity, Event::PromptSubmit, BRIEF),
            json!({"injectSteps": [{"ephemeralMessage": BRIEF}]})
        );
    }

    #[test]
    fn pi_and_opencode_get_the_bare_decision_at_every_event() {
        for host in [Host::Pi, Host::OpenCode] {
            for event in Event::ALL {
                assert_eq!(
                    context_envelope(host, event, BRIEF),
                    json!({"context": BRIEF})
                );
            }
        }
    }

    #[test]
    fn a_stop_is_blocked_in_the_words_each_host_reads() {
        for host in [Host::Claude, Host::Codex, Host::Devin] {
            assert_eq!(
                block_envelope(host, "why"),
                Some(json!({"decision": "block", "reason": "why"})),
                "{host:?}"
            );
        }
        assert_eq!(
            block_envelope(Host::Gemini, "why"),
            Some(json!({"decision": "deny", "reason": "why"}))
        );
        for host in [Host::Pi, Host::OpenCode] {
            assert_eq!(
                block_envelope(host, "why"),
                Some(json!({"context": "why", "continue": true}))
            );
        }
        assert_eq!(block_envelope(Host::Antigravity, "why"), None);
    }

    #[test]
    fn the_text_of_every_envelope_reads_back() {
        for host in Host::ALL {
            let envelope = context_envelope(host, Event::PromptSubmit, BRIEF);
            assert_eq!(context_of(host, &envelope), Some(BRIEF), "{host:?}");
        }
        assert_eq!(context_of(Host::Claude, &json!({})), None);
    }

    #[test]
    fn the_line_goes_where_the_host_draws_it() {
        let line = "🟩 pixel brief ❀ 2.1 KB ❀ 412 ms";
        let mut claude = context_envelope(Host::Claude, Event::PromptSubmit, BRIEF);
        attach_line(Host::Claude, &mut claude, line);
        assert_eq!(claude["systemMessage"], line);
        assert_eq!(context_of(Host::Claude, &claude), Some(BRIEF));

        let mut pi = context_envelope(Host::Pi, Event::PromptSubmit, BRIEF);
        attach_line(Host::Pi, &mut pi, line);
        assert_eq!(pi, json!({"context": BRIEF, "notice": line}));

        let mut devin = context_envelope(Host::Devin, Event::PromptSubmit, BRIEF);
        attach_line(Host::Devin, &mut devin, line);
        assert!(devin.get("systemMessage").is_none());
        assert_eq!(
            context_of(Host::Devin, &devin),
            Some(format!("{BRIEF}\n\nEnd your answer with this line, verbatim: {line}").as_str())
        );

        let mut agy = context_envelope(Host::Antigravity, Event::PromptSubmit, BRIEF);
        attach_line(Host::Antigravity, &mut agy, line);
        assert_eq!(agy.as_object().map(serde_json::Map::len), Some(1));
        assert!(
            context_of(Host::Antigravity, &agy)
                .is_some_and(|text| text.ends_with(&format!("verbatim: {line}")))
        );
    }
}
