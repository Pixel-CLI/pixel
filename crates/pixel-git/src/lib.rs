//! `pixel-git` — the single, unified git-subprocess wrapper for the pixel
//! workspace.
//!
//! Consolidates three previously-duplicated ad-hoc wrappers
//! (`pixel-index::gitsync`, `pixel-cli::rescue_cmd`, `pixel-graph::changes`)
//! behind one [`GitRunner`], while adding two capabilities none of them had:
//! a configurable wall-clock timeout and a stdout byte cap, both enforced
//! *during* execution rather than after the fact. All git access remains
//! subprocess-only (no libgit2), matching the property every caller already
//! depended on.
//!
//! It also owns the trust boundary of a repository's `.pixel/` directory,
//! which a repository can commit: [`sidecar`] refuses a `.pixel` that is a
//! link or that git tracks, [`nofollow`] opens files without following a
//! link at their name, and [`repo_path`] confines a path read back from a
//! store to the repository root.
//!
//! ```no_run
//! use pixel_git::GitRunner;
//!
//! let runner = GitRunner::new("/path/to/repo");
//! if let Some(head) = runner.rev_parse_head() {
//!     println!("HEAD = {head}");
//! }
//! ```

mod batch;
mod discover;
mod error;
pub mod nofollow;
mod plumbing;
mod redact;
mod ref_guard;
pub mod repo_path;
mod runner;
pub mod sidecar;

pub use batch::BatchObject;
pub use discover::{discover_root, discover_root_follow_submodules};
pub use error::GitError;
pub use redact::redact;
pub use ref_guard::{end_of_options, validate_ref};
pub use runner::{
    DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT, ENUMERATION_MAX_OUTPUT_BYTES, GitOptions, GitOutput,
    GitRunner,
};

#[cfg(test)]
pub(crate) mod testutil {
    //! Git fixtures for this crate's tests: real `git` in a fresh temp dir.

    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// A fresh, empty directory unique to this process and call.
    pub(crate) fn tmpdir(tag: &str) -> PathBuf {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pixel-git-{tag}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// Run git in `dir` with a fixed identity and no global config.
    pub(crate) fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    pub(crate) fn init_repo(dir: &Path) {
        git(dir, &["init", "-q"]);
    }
}
