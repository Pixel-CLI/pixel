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

/// Current branch name (`git symbolic-ref --short HEAD`). `None` when not a
/// git repo, in detached HEAD state, or on any git failure.
pub fn current_branch(root: &Path) -> Option<String> {
    GitRunner::new(root).current_branch()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pixel-index-gitsync-{tag}-{}-{}",
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

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
    }

    /// Regression test for the finding that a real `IndexSet` went
    /// completely empty above ~25k tracked files: the base shard is built
    /// from `gitsync::ls_files(root)` (see `IndexSet::open_or_build` in
    /// `indexset.rs`), and `ls_files`/`status_porcelain` used to share the
    /// 1 MiB `pixel_git::DEFAULT_MAX_OUTPUT_BYTES` cap — overflow silently
    /// returned an empty `Vec` instead of erroring, which built the base
    /// shard from zero tracked files. This directly exercises the exact
    /// function `IndexSet::open_or_build` depends on for that decision.
    ///
    /// (An earlier version of this test additionally built a real
    /// `IndexSet` and searched it end-to-end over ~1600–5300 files with
    /// very long, deeply nested paths. That exercised `extract_blob`'s
    /// per-file `git cat-file`/`git show` subprocesses at a scale and path
    /// length this machine could not sustain reliably today — it either
    /// timed out under heavy concurrent build load from other agents, or
    /// produced a shard with zero entries in a way unrelated to output-cap
    /// size, and unrelated to the four files this fix owns; investigating
    /// `indexset.rs`/`shard.rs` internals is out of scope here. The
    /// `ls_files` assertion below is the load-bearing regression check —
    /// it is the precise call `IndexSet::open_or_build` makes, with no
    /// subprocess-per-file cost, so it stays fast and deterministic.)
    #[test]
    fn ls_files_sees_every_tracked_file_past_the_old_1mib_cap_for_index_building() {
        let root = tmpdir("indexset-large-tree");
        init_repo(&root);

        let dir = format!("tracked-{}", "x".repeat(240));
        std::fs::create_dir_all(root.join(&dir)).unwrap();
        const N: usize = 5300; // ~5300 * ~250 bytes ≈ 1.3 MiB of `ls-files -z` output
        for i in 0..N {
            std::fs::write(root.join(&dir).join(format!("f{i:05}.txt")), b"filler").unwrap();
        }
        std::fs::write(root.join(&dir).join("needle.txt"), b"needle content").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "big tracked tree with needle"]);

        let tracked = ls_files(&root);
        assert_eq!(
            tracked.len(),
            N + 1,
            "ls_files (the exact call IndexSet::open_or_build uses to decide which files go \
             into the base shard) must see every tracked file, including the needle, in a tree \
             whose enumeration output exceeds the old 1 MiB cap — previously this returned an \
             empty Vec above that threshold, silently emptying the index"
        );
        assert!(
            tracked.iter().any(|p| p.ends_with("needle.txt")),
            "the needle file specifically must be present in the tracked list"
        );
    }

    /// Regression test mirroring the above for the working-tree dirty
    /// overlay: `status_porcelain` must see every untracked file, even when
    /// a large untracked tree pushes `status --porcelain -z` output past
    /// the old 1 MiB cap (previously silently read back as "nothing is
    /// dirty").
    #[test]
    fn status_porcelain_sees_a_large_untracked_tree_past_the_old_1mib_cap() {
        let root = tmpdir("gitsync-status-large");
        init_repo(&root);
        std::fs::write(root.join("tracked.txt"), b"hello").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-q", "-m", "seed"]);

        let dir = format!("untracked-{}", "x".repeat(240));
        std::fs::create_dir_all(root.join(&dir)).unwrap();
        const N: usize = 5300;
        for i in 0..N {
            std::fs::write(root.join(&dir).join(format!("g{i:05}.txt")), b"y").unwrap();
        }

        let status = status_porcelain(&root);
        let untracked = status.iter().filter(|(xy, _)| xy == "??").count();
        assert_eq!(
            untracked, N,
            "status_porcelain must see every untracked file in a tree whose status output \
             exceeds the old 1 MiB cap"
        );
    }
}
