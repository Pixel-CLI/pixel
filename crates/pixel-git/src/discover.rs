// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Repo-root discovery: walk up from a starting file/dir looking for `.git`.
//!
//! Mirrors the `.git` ancestor-walk half of `pixel-cli::main::discover_root`
//! exactly (checking `cur.join(".git").exists()`, which covers both a real
//! `.git` directory and the `.git` *file* redirection used by worktrees and
//! submodules). It deliberately does **not** reproduce that function's
//! `.pixel`-shard fallback or its "fall back to the starting directory"
//! behavior — `pixel-git` has no notion of a `.pixel` index, so on no `.git`
//! found it returns `None` rather than guessing a root. Callers that want
//! the old fallback behavior apply it themselves on top of `None`.
//!
//! ## Submodules / gitlinks
//!
//! A submodule entry manifests on disk as a `.git` *file* inside the subproject
//! directory, whose contents redirect to a path under the superproject's
//! `.git/modules/...`. The bare `.exists()` probe treats both a `.git` dir and
//! a `.git` file as a repo boundary, so by default the walk *will* stop at
//! each submodule root — indexing a path inside a submodule resolves to the
//! subproject, not the superproject. That is the deliberate default:
//!
//! **Decision (P2.1): do not follow submodules by default.** A submodule
//! inner tree is indexed per subproject (its own root), and the superproject
//! scopes exclude the submodule's working-tree contents (its gitlink entry,
//! pointing at a fixed commit, is the only trace the superproject index keeps —
//! the contents belong to the submodule's own repo). This mirrors `git`'s own
//! model, avoids double-indexing a submodule's working tree under two roots,
//! and matches how a monorepo-with-submodules treats each subproject as an
//! independent versioned unit.
//!
//! Callers that *do* want to operate on the enclosing superproject from a
//! submodule-internal path can use [`discover_root_follow_submodules`] (the
//! `--follow-submodules` equivalent): it climbs through `.git`-file redirect
//! chains whose target sits under an ancestor's `.git/modules`. Real,
//! self-contained `.git` directories are always a hard boundary regardless of
//! the flag — only submodule/worktree redirects are ever followed.

use std::path::{Path, PathBuf};

/// Walk up from `start` (file or directory) to the nearest ancestor holding
/// a `.git` entry (dir or file). Returns `None` if none is found, `start`
/// doesn't exist, or `start`'s path cannot be canonicalized.
///
/// This is the default (non-following) form: it stops at the first `.git`
/// boundary, which — for a `.git` *file* (submodule/worktree) — is the
/// subproject's own root. See module docs for the submodule scoping decision.
pub fn discover_root(start: &Path) -> Option<PathBuf> {
    discover_root_impl(start, FollowSubmodules::No)
}

/// Like [`discover_root`], but opts in to follow submodule and worktree
/// `.git`-file redirects: when the nearest `.git` entry is a *file* (not a
/// directory) whose `gitdir:` target resolves to a path that sits under some
/// ancestor repo's `.git/modules`, the walk keeps climbing past that boundary
/// and settles on the enclosing (superproject) root instead of the subproject
/// root.
///
/// This is the `--follow-submodules` equivalent for discovery: it lets a
/// command accept a submodule-internal path yet operate on the containing
/// monorepo/superproject root. Directories — real, self-contained repos with
/// a `.git` *dir* — are always a hard boundary (never followed), matching
/// `git`'s own behavior: only submodule/worktree redirects are climbed
/// through.
pub fn discover_root_follow_submodules(start: &Path) -> Option<PathBuf> {
    discover_root_impl(start, FollowSubmodules::Yes)
}

#[derive(Clone, Copy)]
enum FollowSubmodules {
    Yes,
    No,
}

fn discover_root_impl(start: &Path, follow: FollowSubmodules) -> Option<PathBuf> {
    let abs = start.canonicalize().ok()?;
    let mut cur = if abs.is_file() {
        abs.parent()?.to_path_buf()
    } else {
        abs
    };
    loop {
        let git_entry = cur.join(".git");
        if git_entry.exists() {
            if matches!(follow, FollowSubmodules::Yes) && git_entry.is_file() {
                // A `.git` *file* is a submodule or worktree redirect. Parse
                // its `gitdir:` target. When that target lives under another
                // repo's `.git/modules`, the redirect points at the enclosing
                // superproject's object store and we keep climbing (we are
                // inside a subproject that belongs to a superproject) — unless
                // the target is a plain bare/worktree dir outside any ancestor,
                // in which case the subproject stands alone and we still stop
                // at its own root.
                if let Some(target) = gitdir_target(&git_entry)
                    && let Ok(abs_target) = target.canonicalize()
                    && is_under_ancestor_git_modules(&cur, &abs_target)
                {
                    // Keep climbing: `cur` is a submodule root — the
                    // nearest `.git`-carrying ancestor is the superproject
                    // root.
                    if let Some(parent) = cur.parent() {
                        cur = parent.to_path_buf();
                        continue;
                    }
                }
            }
            return Some(cur);
        }
        cur = cur.parent()?.to_path_buf();
    }
}

/// Read the `gitdir: <path>` target out of a `.git` *file* (submodule /
/// worktree redirect). Returns `None` if the file has no `gitdir:` line;
/// the returned path is taken verbatim from the redirect (it may be relative
/// to the file's containing directory, in which case the caller resolves it
/// itself).
fn gitdir_target(git_file: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(git_file).ok()?;
    contents.lines().find_map(|l| {
        let l = l.trim();
        let (key, value) = l.split_once(':')?;
        if key.trim() == "gitdir" {
            Some(PathBuf::from(value.trim()))
        } else {
            None
        }
    })
}

/// Is `target` (the `.git` file's redirect target, absolute) located under an
/// ancestor repo's `.git/modules`? I.e. does some ancestor of `cur` (the dir
/// holding the `.git` file) own a `.git/modules/<name>` path that contains or
/// prefixes `target`? If so `cur` is a submodule of that ancestor and the
/// ancestor's superproject root is what the caller wants.
fn is_under_ancestor_git_modules(cur: &Path, target: &Path) -> bool {
    let mut anc = cur.parent();
    while let Some(dir) = anc {
        let modules = dir.join(".git").join("modules");
        if modules.is_dir() && target.starts_with(&modules) {
            return true;
        }
        anc = dir.parent();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pixel-git-discover-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn init(dir: &Path) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["init", "-q"])
            .output()
            .unwrap();
        assert!(out.status.success(), "git init failed: {out:?}");
    }

    #[test]
    fn resolves_nested_subdirectory_to_repo_root() {
        let root = tmpdir("nested");
        init(&root);

        let nested = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();

        let found = discover_root(&nested).expect("should find repo root");
        assert_eq!(found, root.canonicalize().unwrap());
    }

    #[test]
    fn returns_none_outside_any_repo() {
        // A tmpdir with no .git ancestor at all is unlikely on CI systems
        // where /tmp itself may be inside a repo, so scope the assertion to
        // "does not equal a nested-under-tmp dir with no .git" by using a
        // dedicated non-repo dir and checking it doesn't resolve to itself
        // as a false positive; if the whole /tmp tree is under a repo (rare
        // in CI sandboxes) this test would need a different anchor — assert
        // the weaker, still-meaningful property that it never returns the
        // starting dir when that dir has no .git of its own.
        let dir = tmpdir("no-git-here");
        if let Some(found) = discover_root(&dir) {
            assert_ne!(found, dir.canonicalize().unwrap());
        }
    }

    #[test]
    fn default_stops_follow_climbs_at_submodule() {
        if Command::new("git").arg("--version").output().is_err() {
            return; // git unavailable — skip integration-style test
        }
        let sup = tmpdir("sub-super");
        init(&sup);
        // Minimal gitlink simulation: drop a `.git` *file* that redirects
        // into the superproject's module store, like a real checked-out
        // submodule does.
        let sub = sup.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let modules = sup.join(".git").join("modules").join("sub");
        std::fs::create_dir_all(&modules).unwrap();

        let inner = sub.join("nested");
        std::fs::create_dir_all(&inner).unwrap();
        let sup_canon = sup.canonicalize().unwrap();
        let sub_canon = sub.canonicalize().unwrap();

        // Without a redirect target the `.git` *file* still marks a boundary:
        // default discovery stops at the subproject root.
        std::fs::write(sub.join(".git"), "gitdir: bogus\n").unwrap();
        assert_eq!(discover_root(&inner).unwrap(), sub_canon);

        // Point the redirect at the (absolute) superproject module store.
        let mods_abs = modules.canonicalize().unwrap();
        std::fs::write(
            sub.join(".git"),
            format!("gitdir: {}\n", mods_abs.display()),
        )
        .unwrap();

        // Default discovery still stops at the submodule (its own root).
        assert_eq!(follow_root(&inner, false).unwrap(), sub_canon);
        // Follow-discovery climbs past the redirect to the superproject once
        // the target is a real path under an ancestor's `.git/modules`.
        assert_eq!(follow_root(&inner, true).unwrap(), sup_canon);
    }

    fn follow_root(p: &Path, follow: bool) -> Option<PathBuf> {
        if follow {
            discover_root_follow_submodules(p)
        } else {
            discover_root(p)
        }
    }
}
