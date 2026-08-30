//! pixel-install — M5/M6 rollout: idempotent `pixel install`, `pixel doctor`,
//! the gain ledger, and the clean-cut deprecation of usable-git/gitpixel/
//! sniper MCP entries. Registers ONE MCP server `pixel` and rewrites the
//! Claude config, hooks, and agent-config with managed markers.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

pub mod capabilities;
pub mod doctor;
pub mod install;
pub mod gain;
pub mod config;

/// Shared error type for the install/doctor/migrate surface.
#[derive(Debug, Error)]
pub enum InstallError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("config: {0}")]
    Config(#[from] config::ConfigError),
    #[error("gain: {0}")]
    Gain(#[from] gain::GainError),
    #[error("cannot resolve home directory")]
    NoHome,
    #[error("cannot resolve current executable: {0}")]
    CurrentExe(io::Error),
    #[error("invalid settings.json at {path}: {reason}")]
    InvalidSettings { path: PathBuf, reason: String },
}

pub type Result<T> = std::result::Result<T, InstallError>;
