//! Thin shell-out helpers over the `git` CLI.
//!
//! Every function degrades gracefully outside a git repository (returns
//! `None` / empty). Rename detection is disabled (`--no-renames`) so a rename
//! always surfaces as a delete + add, which the delta layer handles natively.
//!
//! All calls now delegate to `pixel_git::GitRunner`, which enforces a
//! wall-clock timeout + stdout byte cap (the defect class PLAN.md calls out
//! from usable-git's ingest path) and validates refs via
//! `pixel_git::validate_ref` consistently — closing the three ref-injection
//! gaps the original ad-hoc wrapper had (unvalidated `oid` in `show_blob`,
//! `blob_size`, `diff_name_status`).

use std::path::Path;

use pixel_git::GitRunner;

/// HEAD commit OID, truncated to 40 hex chars (the shard header width).
/// `None` when not a git repo or the repo has no commits yet.
pub fn rev_parse_head(root: &Path) -> Option<String> {
    GitRunner::new(root).rev_parse_head()
}

/// Tracked files (repo-relative, NUL-safe). Empty outside a git repo.
pub fn ls_files(root: &Path) -> Vec<String> {
    GitRunner::new(root).ls_files()
}

/// Blob content of `path` as it exists in commit `oid` (`git show oid:path`).
/// Returns the raw bytes git stores for that path at that commit — for a
/// symlink this is the target text (a few bytes), never a traversal. `None`
/// on any git failure, missing path at that commit, or an invalid `oid`
/// (the `oid` is now validated via `pixel_git::validate_ref` — the original
/// wrapper passed it unvalidated to `git show`, a ref-injection gap).
pub fn show_blob(root: &Path, oid: &str, rel: &str) -> Option<Vec<u8>> {
    GitRunner::new(root).show_blob(oid, rel)
}

/// Size of a committed blob without materializing it. `oid` is validated
/// via `pixel_git::validate_ref` (the original wrapper did not validate).
pub fn blob_size(root: &Path, oid: &str, rel: &str) -> Option<u64> {
    GitRunner::new(root).blob_size(oid, rel)
}

/// `git diff --name-status --no-renames -z <from> <to>` as (status, path).
/// Statuses are single chars: A, M, D, T (typechange), etc. Both `from` and
/// `to` are validated via `pixel_git::validate_ref` (the original wrapper
/// passed both unvalidated — a ref-injection gap).
pub fn diff_name_status(root: &Path, from: &str, to: &str) -> Vec<(char, String)> {
    GitRunner::new(root).diff_name_status(from, to)
}

/// `git status --porcelain -z --untracked-files=all --no-renames` as
/// (XY, path). Untracked files appear with XY `"??"`.
pub fn status_porcelain(root: &Path) -> Vec<(String, String)> {
    GitRunner::new(root).status_porcelain()
}
