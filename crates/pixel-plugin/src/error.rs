// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! One error type for the plugin host; every variant says what to do next.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "invalid plugin name `{0}`: use lowercase letters, digits, `-` and `_`, starting with a letter or digit"
    )]
    InvalidName(String),
    #[error("{}: {message}", path.display())]
    Manifest { path: PathBuf, message: String },
    #[error("plugin `{0}` is not installed")]
    NotFound(String),
    #[error(
        "repo plugin `{name}` ({dir}) is not trusted, or changed since it was: review it, then run `pixel plugin trust {name}`",
        dir = dir.display()
    )]
    Untrusted { name: String, dir: PathBuf },
    #[error(
        "plugin `{0}` declares network access and is not enabled: run `pixel plugin enable {0}`"
    )]
    NotEnabled(String),
    #[error("plugin `{0}` does not declare the `command` capability, so `pixel {0}` cannot run it")]
    NoCommandCapability(String),
    #[error("plugin `{name}`: `run` = \"{run}\" {why}")]
    BadRun {
        name: String,
        run: String,
        why: String,
    },
    #[error("plugin `{0}` is already installed: run `pixel plugin remove {0}` first")]
    AlreadyInstalled(String),
    #[error("{0}")]
    Source(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },
    #[error("{0}")]
    State(String),
}

impl Error {
    /// An I/O failure with what the host was doing and to which path.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}
