//! Error type for every `pixel-git` operation.
//!
//! Mirrors the manual `Display`/`Error` pattern used elsewhere in this
//! workspace (see `pixel_index::indexset::IndexSetError`) rather than pulling
//! in a derive-macro crate.

use std::fmt;

#[derive(Debug)]
pub enum GitError {
    /// The `git` binary could not be found/executed (e.g. missing from PATH).
    NotFound,
    /// The command did not finish within the configured timeout and was
    /// killed.
    Timeout { args: Vec<String> },
    /// Stdout exceeded the configured byte cap; the process was killed
    /// before it could produce more.
    OutputTooLarge { args: Vec<String>, cap: usize },
    /// `git` exited with a non-zero status. `stderr` has already been
    /// credential-redacted (see `crate::redact`) before being stored here.
    NonZeroExit {
        args: Vec<String>,
        code: Option<i32>,
        stderr: String,
    },
    /// Any other I/O failure spawning or communicating with the child
    /// process.
    Io(std::io::Error),
    /// A ref/commit-ish argument failed `ref_guard::validate_ref` (e.g. it
    /// started with `-`, which could be interpreted as a flag by git).
    InvalidRef(String),
    /// Captured output was not valid UTF-8 where strict decoding was
    /// required.
    Utf8(std::string::FromUtf8Error),
}

impl From<std::io::Error> for GitError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::NotFound {
            GitError::NotFound
        } else {
            GitError::Io(e)
        }
    }
}

impl From<std::string::FromUtf8Error> for GitError {
    fn from(e: std::string::FromUtf8Error) -> Self {
        GitError::Utf8(e)
    }
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::NotFound => write!(f, "git binary not found"),
            GitError::Timeout { args } => write!(f, "git {args:?} timed out"),
            GitError::OutputTooLarge { args, cap } => {
                write!(f, "git {args:?} exceeded output cap of {cap} bytes")
            }
            GitError::NonZeroExit { args, code, stderr } => {
                write!(f, "git {args:?} exited with {code:?}: {stderr}")
            }
            GitError::Io(e) => write!(f, "io error running git: {e}"),
            GitError::InvalidRef(r) => write!(f, "invalid git ref: {r:?}"),
            GitError::Utf8(e) => write!(f, "invalid utf8 in git output: {e}"),
        }
    }
}

impl std::error::Error for GitError {}
