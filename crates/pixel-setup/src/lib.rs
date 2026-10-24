// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! pixel-setup: what PIXEL knows about each agent host (`specs/hosts.md`) and
//! the idempotent `pixel setup` / `pixel setup --remove` that registers it
//! (`specs/setup-migration.md`).
//!
//! The host table is pure: the hook events a host fires and what they are
//! called there (`hosts`), the payload fields PIXEL reads (`payload`), and the
//! envelope a host accepts back (`envelope`), so every host × event contract
//! is a golden test. The installers write the hook entries of Claude Code,
//! Codex, Devin, Gemini CLI and Antigravity, OpenCode's plugin, pi's package
//! and the `pixel` skill, and keep a receipt of what they wrote. PIXEL deploys
//! no prompt and wires no guard.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

pub mod antigravity;
pub mod codex_config;
pub mod codex_review;
pub mod config;
pub mod doctor;
pub mod edit_input;
mod envelope;
mod hosts;
pub mod install;
mod json_edit;
pub mod opencode_config;
mod payload;
mod pi_global;
pub mod receipt;
mod relay;
mod routing;
pub mod settings;
pub mod setup;
pub mod skill;
pub mod uninstall;

pub use envelope::{attach_line, block_envelope, context_envelope, context_of};
pub use hosts::{Event, Host};
pub use payload::{Payload, last_user_request, normalize};
pub use pi_global::{CLASSIFY_PACKAGE_DIR, ClassifyPiPackage};
pub use relay::{footer, footers, relay_response};

/// Shared error type for the install surface.
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
