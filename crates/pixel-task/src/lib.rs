// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Durable task contracts, workflow decisions and snapshot-bound verification.

pub mod evaluation;
pub mod model;
pub mod policy;
pub mod replay;
pub mod runner;
pub mod snapshot;
pub mod store;

use std::time::{SystemTime, UNIX_EPOCH};

pub use model::*;
pub use store::Store;

/// Errors never become successful or absent task evidence.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid task input: {0}")]
    Invalid(String),
    #[error("task state unavailable: {0}")]
    Unavailable(String),
    #[error("task journal corrupt: {0}")]
    Corrupt(String),
    #[error("task not found: {0}")]
    NotFound(String),
    #[error("task busy: {0}")]
    Busy(String),
    #[error("task revision conflict: expected {expected}, actual {actual}")]
    Conflict { expected: u64, actual: u64 },
    #[error("request id reused with different input: {0}")]
    Idempotency(String),
    #[error("workflow gate blocked: {0}")]
    Blocked(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Wall time for durable event provenance, in milliseconds since Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

/// Full SHA-256 of a serialized, deterministic value.
pub fn digest<T: serde::Serialize + ?Sized>(value: &T) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(value)?)))
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;

    pub fn git(root: &Path, args: &[&str]) -> Vec<u8> {
        let mut configured = vec![
            "-c",
            "user.name=Task tests",
            "-c",
            "user.email=task@example.com",
            "-c",
            "commit.gpgsign=false",
        ];
        configured.extend(args);
        pixel_git::GitRunner::new(root)
            .run_isolated(&configured)
            .unwrap()
    }

    pub fn repo() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "-q"]);
        std::fs::write(directory.path().join("source.txt"), "value").unwrap();
        std::fs::write(
            directory.path().join(".gitignore"),
            ".pixel/\ntarget/\nignored/\n",
        )
        .unwrap();
        git(directory.path(), &["add", "."]);
        git(directory.path(), &["commit", "-qm", "fixture"]);
        directory
    }

    pub fn contract() -> crate::TaskContract {
        crate::TaskContract {
            objective: "capture task source".into(),
            ..crate::TaskContract::default()
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn digest_is_exact_sha256_of_serialized_value() {
        use sha2::{Digest, Sha256};
        assert_eq!(
            super::digest(&"value").unwrap(),
            hex::encode(Sha256::digest(br#""value""#))
        );
        assert_ne!(
            super::digest(&"value").unwrap(),
            super::digest(&"other").unwrap()
        );
    }
}
