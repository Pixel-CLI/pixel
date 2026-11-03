// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The one absolute deadline of a request.
//!
//! A hook or a command fixes the instant its answer is worth nothing after,
//! once, at its entry. Every stage that works for it (the daemon queue, a
//! reader, a probe) stops at that same wall-clock instant instead of starting
//! a relative timeout of its own when it begins. The instant travels beside
//! the op, not inside it: a top-level `deadline_unix_ms` member of the request
//! line, so no op variant carries it and every op can.
//!
//! A daemon that predates the member ignores it and runs the request to the
//! end, which is why `pixel_daemon::api::PROTOCOL_VERSION` moved with it: the
//! CLI only sends it to a daemon that answered a ping on its own version.

use serde::Deserialize;
use serde_json::Value;

use crate::Op;

/// The request-line member that carries the deadline, in milliseconds since
/// the Unix epoch.
pub const REQUEST_DEADLINE_FIELD: &str = "deadline_unix_ms";

/// `op` as one request line (no newline), carrying `deadline_unix_ms` when
/// the caller has one.
///
/// # Errors
///
/// The op does not serialize.
pub fn request_line(op: &Op, deadline_unix_ms: Option<u64>) -> serde_json::Result<String> {
    let Some(deadline) = deadline_unix_ms else {
        return serde_json::to_string(op);
    };
    let mut value = serde_json::to_value(op)?;
    if let Value::Object(members) = &mut value {
        members.insert(REQUEST_DEADLINE_FIELD.to_owned(), deadline.into());
    }
    serde_json::to_string(&value)
}

/// The deadline a request line carries; `None` when it names none, or not a
/// number. The common line has no such member, so it is not parsed twice.
#[must_use]
pub fn request_deadline_unix_ms(line: &str) -> Option<u64> {
    #[derive(Deserialize)]
    struct Frame {
        deadline_unix_ms: Option<u64>,
    }
    if !line.contains(REQUEST_DEADLINE_FIELD) {
        return None;
    }
    serde_json::from_str::<Frame>(line).ok()?.deadline_unix_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search() -> Op {
        Op::Search {
            stale_ok: false,
            pattern: "x".into(),
            json: false,
            limit: None,
            offset: None,
            paths: None,
            scope: None,
            globs: Vec::new(),
            types: Vec::new(),
        }
    }

    /// The deadline rides beside the op: the same line still parses as that
    /// op, and the member is the one the constant names.
    #[test]
    fn a_line_carries_its_deadline_beside_the_op() {
        let line = request_line(&search(), Some(1_800_000_000_123)).unwrap();
        assert_eq!(request_deadline_unix_ms(&line), Some(1_800_000_000_123));
        assert_eq!(serde_json::from_str::<Op>(&line).unwrap(), search());
        let members: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(members[REQUEST_DEADLINE_FIELD], 1_800_000_000_123_u64);
    }

    /// No deadline, no member: the line is the op's own bytes, so a request
    /// without one is unchanged on the wire.
    #[test]
    fn a_line_without_a_deadline_is_the_ops_own() {
        let line = request_line(&search(), None).unwrap();
        assert_eq!(line, serde_json::to_string(&search()).unwrap());
        assert_eq!(request_deadline_unix_ms(&line), None);
    }

    /// A member that is not a non-negative integer names no deadline; an op
    /// field that merely contains the member's name is not read as one.
    #[test]
    fn only_a_numeric_top_level_member_is_a_deadline() {
        assert_eq!(
            request_deadline_unix_ms(r#"{"op":"ping","deadline_unix_ms":"soon"}"#),
            None
        );
        assert_eq!(
            request_deadline_unix_ms(r#"{"op":"ping","deadline_unix_ms":-5}"#),
            None
        );
        let mut op = search();
        if let Op::Search { pattern, .. } = &mut op {
            *pattern = r#""deadline_unix_ms":7"#.into();
        }
        let line = request_line(&op, None).unwrap();
        assert_eq!(request_deadline_unix_ms(&line), None);
    }
}
