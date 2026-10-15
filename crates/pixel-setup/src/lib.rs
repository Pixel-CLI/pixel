// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! pixel-setup — idempotent `pixel setup`, `pixel setup --remove` and
//! `pixel doctor`. pixel is a CLI + lifecycle integration tool: install keeps
//! Claude and Codex task lifecycle hooks, the `pixel` skill, the prompt-brief
//! hooks of Devin, Gemini CLI, Antigravity and OpenCode, and gives Pi the
//! explicit impact command as a local package. It deploys no prompt and
//! wires no retrieval guard.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

pub mod antigravity;
pub mod banner;
pub mod bundled_probes;
pub mod codex_config;
pub mod config;
pub mod doctor;
pub mod install;
pub mod opencode_config;
mod pi_global;
pub use pi_global::{CLASSIFY_PACKAGE_DIR, ClassifyPiPackage};
mod routing;
pub mod setup;
pub mod skill;
pub mod uninstall;

/// Shared error type for the install/doctor surface.
#[derive(Debug, Error)]
pub enum InstallError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
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
    #[error("setup: {0}")]
    Setup(String),
    #[error("unknown doctor check `{0}` — `pixel doctor --list` names every check")]
    UnknownDoctorCheck(String),
    #[error("doctor check `{0}` is both selected by --only and excluded by --skip")]
    ConflictingDoctorSelection(String),
}

pub type Result<T> = std::result::Result<T, InstallError>;
