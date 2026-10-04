// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! pixel-install — M5/M6 rollout: idempotent `pixel install`, `pixel doctor`,
//! and the clean-cut deprecation of the usable-git/gitpixel/sniper MCP
//! entries. pixel is a CLI + lifecycle integration tool, not an MCP server —
//! install scrubs deprecated entries (and the Warp MCP entry older releases
//! wrote), unconditionally configures Claude, Codex and Pi, and wires
//! lifecycle hooks. OpenCode, Antigravity and zcode are integrated only
//! when their configuration directory or file already exists, not by
//! detecting agent CLIs. Agent-config files are rewritten with managed
//! markers only where applicable.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

pub mod antigravity;
pub mod banner;
pub mod codex_config;
pub mod config;
pub mod copilot_config;
pub mod doctor;
pub mod install;
pub mod intro;
pub mod opencode_config;
mod pi_project;
mod pixel_first;
mod repo_git;
mod routing;
pub mod uninstall;
mod warp;

/// Shared error type for the install/doctor/migrate surface.
#[derive(Debug, Error)]
pub enum InstallError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("config: {0}")]
    Config(#[from] config::ConfigError),
    #[error("cannot resolve home directory")]
    NoHome,
    #[error("cannot resolve current executable: {0}")]
    CurrentExe(io::Error),
    #[error(
        "the shell profile holds a `# >>> pixel-managed >>>` block with no \
         `# <<< pixel-managed <<<` end marker; close or delete that block, then re-run"
    )]
    UnterminatedManagedBlock,
    #[error("invalid settings.json at {path}: {reason}")]
    InvalidSettings { path: PathBuf, reason: String },
    #[error("unknown doctor check `{0}` — `pixel doctor --list` names every check")]
    UnknownDoctorCheck(String),
    #[error("doctor check `{0}` is both selected by --only and excluded by --skip")]
    ConflictingDoctorSelection(String),
}

pub type Result<T> = std::result::Result<T, InstallError>;
