// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The PIXEL code graph (`specs/graph.md`): one store, one pipeline, one parse
//! per file.
//!
//! Each language is one grammar and one rule module (`ruby`, `rust`). The
//! indexer returns rows and writes nothing: the daemon hands them to the
//! [`store`]. A run is `tree::scan`, then `link::plan` against the current
//! generation, `extract::extract` of the planned files within an optional
//! deadline, and `link::link`, which decides every site of the generation
//! again so that an incremental run stores exactly what a full run stores.

pub mod analysis;
pub mod codec;
pub mod edit;
pub mod extract;
pub mod git_index;
pub mod guard;
pub mod lang;
pub mod link;
pub mod names;
pub mod refresh;
pub mod rename;
pub mod ruby;
pub mod rust;
pub mod shape;
pub mod store;
pub mod text;
pub mod tree;

pub use extract::{Extracted, extract};
pub use lang::extractor;
pub use link::{Plan, link, plan};
pub use refresh::{RefreshError, Refreshed, refresh};
pub use tree::{
    Known, Repository, Tree, TreeError, TreeFile, changes, diff, discover, root, scan,
    scan_discovered, scan_discovered_in, scan_with,
};

#[cfg(test)]
mod grammar;
#[cfg(test)]
mod truth;

#[cfg(test)]
pub(crate) mod testutil {
    //! Git fixtures for this crate's tests: real `git`, run directly (test
    //! code is exempt from the single-runner boundary).

    use std::path::Path;
    use std::process::Command;

    /// `git -C dir` that never takes `.git/index.lock`, as the scan runs it.
    pub(crate) fn git_in(dir: &Path) -> Command {
        let mut command = Command::new("git");
        command.arg("--no-optional-locks").arg("-C").arg(dir);
        command
    }

    /// Runs git in `dir` with an identity and no signing, as a test sets a
    /// repository up.
    pub(crate) fn run_git(dir: &Path, args: &[&str]) {
        let status = git_in(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    /// Writes `files` (path, content) under `dir` and commits everything.
    pub(crate) fn commit(dir: &Path, files: &[(&str, &str)], message: &str) {
        for (path, content) in files {
            let full = dir.join(path);
            std::fs::create_dir_all(full.parent().expect("a parent")).expect("directories");
            std::fs::write(full, content).expect("written");
        }
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["commit", "-qm", message]);
    }

    /// A fresh repository holding `files`, committed.
    pub(crate) fn repo_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temporary directory");
        run_git(dir.path(), &["init", "-q"]);
        commit(dir.path(), files, "one");
        dir
    }
}
