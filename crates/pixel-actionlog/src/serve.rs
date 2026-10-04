// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! How an invocation's requests reached an answer: through a running daemon,
//! through one it had to start, or in its own process — and where the time
//! went on the way. A slow line in `actions.jsonl` is only diagnosable when
//! it says whether the seconds went to reaching a busy daemon, waiting for a
//! new one to come up, or opening the index in process.

use serde::{Deserialize, Serialize};

/// Who answered one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServeRoute {
    /// A daemon that was already running.
    Daemon,
    /// A daemon this invocation started because none answered.
    DaemonStarted,
    /// The CLI's own process.
    InProcess,
    /// A route written by a newer pixel; kept so the rest of the line parses.
    #[serde(other)]
    Unknown,
}

/// Why a request was answered in process rather than by a daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InProcessReason {
    /// `--no-daemon`.
    NoDaemon,
    /// No daemon answered and `PIXEL_DAEMON_AUTO_START=0` forbade starting one.
    AutoStartDisabled,
    /// A newer daemon serves a newer CLI and is left alone.
    NewerDaemon,
    /// Spawning a daemon failed.
    StartFailed,
    /// A daemon was spawned but did not answer within the start window.
    StartTimedOut,
    /// No daemon answered, and this command never starts one.
    DaemonAbsent,
    /// The daemon answered with an error; the request was redone in process.
    DaemonError,
    /// The command has no daemon route at all.
    NotRouted,
    /// A reason written by a newer pixel; kept so the rest of the line parses.
    #[serde(other)]
    Unknown,
}

/// One request's route and phase timings, in milliseconds. A phase the
/// request did not go through is absent, never zero: `open_ms: 0` means an
/// in-process open that took under a millisecond.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServeStep {
    pub route: ServeRoute,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<InProcessReason>,
    /// Asking the running daemon whether it can serve. The daemon answers
    /// between two requests, so a queue behind a long request shows here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_ms: Option<u64>,
    /// Spawning a daemon and waiting for its socket to answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<u64>,
    /// The request's round trip to the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_ms: Option<u64>,
    /// In process: opening the index, graph or store, catch-up included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_ms: Option<u64>,
    /// In process: answering the request once opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle_ms: Option<u64>,
}

impl ServeStep {
    /// A step answered by `route`, with no phase timed yet.
    pub fn new(route: ServeRoute) -> Self {
        ServeStep {
            route,
            reason: None,
            probe_ms: None,
            start_ms: None,
            request_ms: None,
            open_ms: None,
            handle_ms: None,
        }
    }

    /// A step answered in process, for `reason`.
    pub fn in_process(reason: InProcessReason) -> Self {
        ServeStep {
            reason: Some(reason),
            ..ServeStep::new(ServeRoute::InProcess)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log is read by `pixel-retro` and by hand: the wire names are the
    /// contract, and an untimed phase must not appear as a zero.
    #[test]
    fn a_step_serializes_its_route_reason_and_only_the_timed_phases() {
        let mut step = ServeStep::in_process(InProcessReason::StartTimedOut);
        step.start_ms = Some(5_012);
        step.open_ms = Some(0);
        let json = serde_json::to_value(&step).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "route": "in_process",
                "reason": "start_timed_out",
                "start_ms": 5012,
                "open_ms": 0,
            })
        );
        let daemon = serde_json::to_value(ServeStep::new(ServeRoute::DaemonStarted)).unwrap();
        assert_eq!(daemon, serde_json::json!({ "route": "daemon_started" }));
    }

    /// An older pixel reading a newer log must keep the line: `tail` drops a
    /// line that fails to parse, so an unknown name would erase the event.
    #[test]
    fn names_from_a_newer_pixel_parse_as_unknown() {
        let step: ServeStep =
            serde_json::from_str(r#"{"route":"daemon_pool","reason":"warm_spare","probe_ms":3}"#)
                .unwrap();
        assert_eq!(step.route, ServeRoute::Unknown);
        assert_eq!(step.reason, Some(InProcessReason::Unknown));
        assert_eq!(step.probe_ms, Some(3));
    }
}
