// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The names the transport speaks in: a request is an [`Op`], a response an
//! envelope of JSON.

use pixel_proto::{Envelope, ErrorCode, Op, PixelError};
use serde_json::Value;

/// What a client sends.
pub type Request = Op;

/// What the daemon answers.
pub type Response = Envelope<Value>;

/// Why a daemon could not be run.
#[derive(Debug)]
pub enum ServeError {
    Io(std::io::Error),
    Msg(String),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeError::Io(err) => write!(f, "io error: {err}"),
            ServeError::Msg(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ServeError {}

impl From<std::io::Error> for ServeError {
    fn from(err: std::io::Error) -> Self {
        ServeError::Io(err)
    }
}

/// A failure envelope from a plain message: its leading `"<CODE>: "` names
/// the error code when it is one of [`ErrorCode`]'s wire names, a lookup that
/// came up empty is `NOT_FOUND`, a busy repository `BUSY_REPOSITORY`, and
/// anything else `INVALID_INPUT`.
pub fn failure_response(op: &str, message: impl Into<String>) -> Response {
    let message = message.into();
    let code = classify_error(&message);
    Envelope::failure(op, PixelError::new(code, message))
}

fn classify_error(message: &str) -> ErrorCode {
    if let Some(code) = code_named_in_prefix(message) {
        return code;
    }
    let lower = message.to_lowercase();
    if lower.starts_with("no symbol named") || lower.starts_with("no symbol with uid") {
        ErrorCode::NotFound
    } else if lower.starts_with("repository is busy") {
        ErrorCode::BusyRepository
    } else {
        ErrorCode::InvalidInput
    }
}

/// The code a message names in its leading `"<CODE>: …"` token.
fn code_named_in_prefix(message: &str) -> Option<ErrorCode> {
    let (head, _) = message.split_once(':')?;
    serde_json::from_str(&format!("\"{}\"", head.trim())).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_names_its_code_or_falls_to_invalid_input() {
        let cases: &[(&str, ErrorCode)] = &[
            ("STALE_STATE: expected head aaa", ErrorCode::StaleState),
            ("no symbol named \"nope\"", ErrorCode::NotFound),
            ("No symbol with uid x", ErrorCode::NotFound),
            ("repository is busy (locked)", ErrorCode::BusyRepository),
            ("bad: thing", ErrorCode::InvalidInput),
            ("", ErrorCode::InvalidInput),
        ];
        for (message, expected) in cases {
            let response = failure_response("symbol", *message);
            assert!(!response.ok);
            assert_eq!(
                response.error.as_ref().map(|e| e.code),
                Some(*expected),
                "{message}"
            );
        }
    }
}
