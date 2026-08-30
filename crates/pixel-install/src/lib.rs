//! pixel-install — M5/M6 rollout: idempotent `pixel install`, `pixel doctor`,
//! and the clean-cut deprecation of the usable-git/gitpixel/sniper MCP
//! entries. pixel is a CLI + hooks tool, NOT an MCP server — install scrubs
//! the deprecated MCP entries, wires the guard/SessionStart hooks, and
//! rewrites agent-config with managed markers.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

pub mod doctor;
pub mod install;
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
    #[error("cannot resolve home directory")]
    NoHome,
    #[error("cannot resolve current executable: {0}")]
    CurrentExe(io::Error),
    #[error("invalid settings.json at {path}: {reason}")]
    InvalidSettings { path: PathBuf, reason: String },
}

pub type Result<T> = std::result::Result<T, InstallError>;
