// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The one way these fixtures run git: isolated from the machine's own
//! configuration. A maintainer's global config that signs commits (through
//! a password manager, say) made every fixture commit fail or hang, and
//! the suite only passed under `GIT_CONFIG_GLOBAL=/dev/null`.

use std::path::Path;
use std::process::Command;

/// `git` with no global or system configuration and a fixed identity.
pub fn git_command() -> Command {
    let mut command = Command::new("git");
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
    command
}

/// Runs `git -C dir <args>` as a fixture sets a repository up.
pub fn git(dir: &Path, args: &[&str]) {
    let out = git_command()
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}
