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
//! This crate only builds and tests standalone; migrating the three
//! existing call sites onto it is a separate, later step.
//!
//! ```no_run
//! use pixel_git::GitRunner;
//!
//! let runner = GitRunner::new("/path/to/repo");
//! if let Some(head) = runner.rev_parse_head() {
//!     println!("HEAD = {head}");
//! }
//! ```

mod discover;
mod error;
mod plumbing;
mod redact;
mod ref_guard;
mod runner;

pub use discover::discover_root;
pub use error::GitError;
pub use redact::redact;
pub use ref_guard::{end_of_options, validate_ref};
pub use runner::{DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT, GitOptions, GitRunner};
