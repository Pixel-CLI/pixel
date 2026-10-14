// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The closed lists a plugin run is named with (`design/plugins.md`, API
//! 1): what a plugin can do, why its answer was dropped, and the shape of
//! its name. The plugin host reads manifests and runs plugins with them;
//! the local log writes a run with them (C-17, C-31), so a log line can
//! hold nothing else of a plugin.

/// What a plugin can do (API 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    Command,
    ChainStep,
    SessionStart,
    Classify,
}

impl Capability {
    pub const ALL: [Capability; 4] = [
        Capability::Command,
        Capability::ChainStep,
        Capability::SessionStart,
        Capability::Classify,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Command => "command",
            Capability::ChainStep => "chain-step",
            Capability::SessionStart => "session-start",
            Capability::Classify => "classify",
        }
    }

    pub fn parse(text: &str) -> Option<Capability> {
        Capability::ALL.into_iter().find(|c| c.as_str() == text)
    }
}

/// Why a plugin's answer was dropped: a closed list
/// (`design/plugins.md`, "The protocol").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Code {
    /// No answer by the deadline; the process group was killed.
    Timeout,
    /// It did not start, or exited other than 0.
    Exit,
    /// Not the protocol's JSON, or more than the host's output cap.
    Malformed,
    /// A pick names a path outside the repository.
    OutsideRepo,
    /// A pick names a file or a symbol the current generation does not
    /// hold.
    UnknownSymbol,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Code::Timeout => "timeout",
            Code::Exit => "exit",
            Code::Malformed => "malformed",
            Code::OutsideRepo => "outside-repo",
            Code::UnknownSymbol => "unknown-symbol",
        }
    }
}

/// The longest plugin name.
pub const NAME_MAX: usize = 64;

/// Whether `name` has a plugin name's shape: 1 to [`NAME_MAX`] of `a-z`,
/// `0-9` and `-`, not starting with `-`. The host also refuses a core
/// command's name.
pub fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
}
