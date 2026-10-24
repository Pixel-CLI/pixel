// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The hosts PIXEL serves and the events its one entry point, `pixel hook
//! <event> --host <host>`, answers (`specs/hosts.md`, "Event map").

use std::str::FromStr;
use std::time::Duration;

/// An agent host with hooks or an extension API PIXEL registers into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Host {
    Claude,
    Codex,
    Devin,
    Gemini,
    Antigravity,
    /// pi's extension package calls the hook command itself.
    Pi,
    /// The OpenCode plugin calls the hook command itself.
    OpenCode,
}

impl Host {
    pub const ALL: [Host; 7] = [
        Host::Claude,
        Host::Codex,
        Host::Devin,
        Host::Gemini,
        Host::Antigravity,
        Host::Pi,
        Host::OpenCode,
    ];

    /// The `--host` spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Host::Claude => "claude",
            Host::Codex => "codex",
            Host::Devin => "devin",
            Host::Gemini => "gemini",
            Host::Antigravity => "antigravity",
            Host::Pi => "pi",
            Host::OpenCode => "opencode",
        }
    }

    /// The deadline the host gives one hook command; the brief's window and
    /// the printing stay inside it (`specs/hosts.md`, "pi package").
    pub const fn deadline(self) -> Duration {
        match self {
            Host::Pi => Duration::from_secs(6),
            _ => Duration::from_secs(8),
        }
    }

    /// Whether the host measures a hook's context in bytes (Codex estimates
    /// tokens from them) and not in characters.
    pub const fn counts_bytes(self) -> bool {
        matches!(self, Host::Codex)
    }

    /// Whether the host draws a hook's top-level `systemMessage` to the user.
    /// Devin and Antigravity draw neither it nor hook output, so their user
    /// sees the 🟩 line only when the model repeats it.
    pub const fn shows_system_message(self) -> bool {
        matches!(self, Host::Claude | Host::Codex | Host::Gemini)
    }

    /// Whether the 🟩 line reaches the user as a request to the model to
    /// repeat it (`shows_system_message` is false and the host has a hook
    /// envelope at all).
    pub const fn relays_through_model(self) -> bool {
        matches!(self, Host::Devin | Host::Antigravity)
    }

    /// Whether the host speaks to PIXEL in the bare decision shape
    /// (`{"context": …}`) its extension reads, and not a hook envelope.
    pub const fn bare_decision(self) -> bool {
        matches!(self, Host::Pi | Host::OpenCode)
    }

    /// The name the host gives `event` when it registers a hook for it, or
    /// `None` where the host fires none (extension hosts, Antigravity's
    /// session start).
    pub const fn registration(self, event: Event) -> Option<&'static str> {
        match (self, event) {
            (Host::Pi | Host::OpenCode, _) | (Host::Antigravity, Event::SessionStart) => None,
            (Host::Gemini, Event::PromptSubmit) => Some("BeforeAgent"),
            (Host::Gemini, Event::PreEdit) => Some("BeforeTool"),
            (Host::Gemini, Event::PostEdit | Event::PostTool) => Some("AfterTool"),
            (Host::Gemini, Event::Stop) => Some("AfterAgent"),
            (Host::Antigravity, Event::PromptSubmit) => Some("PreInvocation"),
            (Host::Antigravity, Event::Stop) => Some("PostInvocation"),
            (_, Event::SessionStart) => Some("SessionStart"),
            (_, Event::PromptSubmit) => Some("UserPromptSubmit"),
            (_, Event::PreEdit) => Some("PreToolUse"),
            (_, Event::PostEdit | Event::PostTool) => Some("PostToolUse"),
            (_, Event::Stop) => Some("Stop"),
        }
    }
}

impl FromStr for Host {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        Host::ALL
            .into_iter()
            .find(|host| host.as_str() == text)
            .ok_or_else(|| format!("unknown host `{text}`"))
    }
}

/// What a host event means to PIXEL (`specs/hooks.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Event {
    SessionStart,
    PromptSubmit,
    PreEdit,
    PostEdit,
    PostTool,
    Stop,
}

impl Event {
    pub const ALL: [Event; 6] = [
        Event::SessionStart,
        Event::PromptSubmit,
        Event::PreEdit,
        Event::PostEdit,
        Event::PostTool,
        Event::Stop,
    ];

    /// The `pixel hook <event>` spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Event::SessionStart => "session-start",
            Event::PromptSubmit => "prompt-submit",
            Event::PreEdit => "pre-edit",
            Event::PostEdit => "post-edit",
            Event::PostTool => "post-tool",
            Event::Stop => "stop",
        }
    }
}

impl FromStr for Event {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        Event::ALL
            .into_iter()
            .find(|event| event.as_str() == text)
            .ok_or_else(|| format!("unknown event `{text}`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_host_and_event_round_trips_through_its_spelling() {
        for host in Host::ALL {
            assert_eq!(host.as_str().parse::<Host>(), Ok(host));
        }
        for event in Event::ALL {
            assert_eq!(event.as_str().parse::<Event>(), Ok(event));
        }
        assert!("cursor".parse::<Host>().is_err());
        assert!("prompt".parse::<Event>().is_err());
    }

    #[test]
    fn the_registration_names_are_the_ones_specs_hosts_lists() {
        let names = |host: Host| Event::ALL.map(|event| host.registration(event));
        assert_eq!(
            names(Host::Claude),
            [
                Some("SessionStart"),
                Some("UserPromptSubmit"),
                Some("PreToolUse"),
                Some("PostToolUse"),
                Some("PostToolUse"),
                Some("Stop"),
            ]
        );
        assert_eq!(names(Host::Codex), names(Host::Claude));
        assert_eq!(names(Host::Devin), names(Host::Claude));
        assert_eq!(
            names(Host::Gemini),
            [
                Some("SessionStart"),
                Some("BeforeAgent"),
                Some("BeforeTool"),
                Some("AfterTool"),
                Some("AfterTool"),
                Some("AfterAgent"),
            ]
        );
        assert_eq!(
            names(Host::Antigravity),
            [
                None,
                Some("PreInvocation"),
                Some("PreToolUse"),
                Some("PostToolUse"),
                Some("PostToolUse"),
                Some("PostInvocation"),
            ]
        );
        assert_eq!(names(Host::Pi), [None; 6]);
        assert_eq!(names(Host::OpenCode), [None; 6]);
    }

    #[test]
    fn pi_has_the_shorter_deadline_and_only_codex_counts_bytes() {
        assert_eq!(Host::Pi.deadline(), Duration::from_secs(6));
        assert_eq!(Host::Claude.deadline(), Duration::from_secs(8));
        let bytes: Vec<_> = Host::ALL
            .into_iter()
            .filter(|host| host.counts_bytes())
            .collect();
        assert_eq!(bytes, [Host::Codex]);
    }

    #[test]
    fn the_user_line_has_one_route_per_host() {
        for host in Host::ALL {
            let routes = [
                host.shows_system_message(),
                host.relays_through_model(),
                host.bare_decision(),
            ];
            assert_eq!(routes.iter().filter(|route| **route).count(), 1, "{host:?}");
        }
    }
}
