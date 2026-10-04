// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Repository identity — one canonical key per repository.
//!
//! The repository lock and the operation journal must agree on what "this
//! repository" means. `publish`, `push` and `rewrite` used to derive the
//! journal key from `root.canonicalize()` and the lock key from a raw
//! `<root>/.git`: the two agree by luck when the root is already spelled
//! canonically, and diverge for `/repo` vs `/./repo`, a symlink,
//! `/tmp` vs `/private/tmp`, or a linked worktree — each spelling then
//! takes its own lock, so two mutations of one repository run at once.
//!
//! [`repo_identity`] is that single key.

use std::path::{Path, PathBuf};

use pixel_git::GitRunner;

/// The identity of the repository `root` belongs to: the canonical path of
/// its real git common directory.
///
/// Worktree-aware: in a linked worktree `<root>/.git` is a file pointing at
/// `<main>/.git/worktrees/<name>`, and the refs and objects a mutation has
/// to serialize on live in `<main>/.git`. The lock and the journal both key
/// on this value, so every spelling of one repository contends for one
/// lock and shares one journal.
pub fn repo_identity(root: &Path) -> String {
    let common_dir = git_common_dir(root).unwrap_or_else(|| root.join(".git"));
    common_dir
        .canonicalize()
        .unwrap_or(common_dir)
        .display()
        .to_string()
}

/// `git rev-parse --path-format=absolute --git-common-dir`, trimmed.
///
/// The flag order matters: `--path-format` only applies to the options that
/// follow it, so the reversed spelling silently yields the relative `.git`.
///
/// `None` when git cannot answer — not a repository, no `git` on `PATH` —
/// so the caller falls back to `<root>/.git`.
fn git_common_dir(root: &Path) -> Option<PathBuf> {
    let out = GitRunner::new(root)
        .run(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .ok()?;
    let text = String::from_utf8_lossy(&out).trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(PathBuf::from(text))
}
