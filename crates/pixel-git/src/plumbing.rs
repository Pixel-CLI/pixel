// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Typed convenience methods consolidating every git subcommand used across
//! the three original wrappers (`pixel-index::gitsync`, `pixel-cli::rescue_cmd`,
//! `pixel-graph::changes`). Same flags, same semantics as the originals —
//! this module only unifies *where* the calls live, plus applies
//! `ref_guard::validate_ref` consistently everywhere a ref/commit-ish string
//! is interpolated (see the ref-injection gap audit in the crate-level docs
//! / final report).

use std::path::Path;

use crate::error::GitError;
use crate::ref_guard::{end_of_options, validate_ref};
use crate::runner::{BLOB_MAX_OUTPUT_BYTES, ENUMERATION_MAX_OUTPUT_BYTES, GitOutput, GitRunner};

impl GitRunner {
    /// HEAD commit OID, truncated to 40 hex chars. `None` when not a git
    /// repo, the repo has no commits yet, or the command failed/timed out.
    pub fn rev_parse_head(&self) -> Option<String> {
        let out = self.run_opt(&["rev-parse", "HEAD"])?;
        let s = String::from_utf8_lossy(&out).trim().to_string();
        if s.is_empty() {
            return None;
        }
        Some(s.chars().take(40).collect())
    }

    /// Current branch name (`git symbolic-ref --short HEAD`). `None` when
    /// not a git repo, in detached HEAD state, or on any git failure.
    pub fn current_branch(&self) -> Option<String> {
        let out = self.run_opt(&["symbolic-ref", "--short", "HEAD"])?;
        let s = String::from_utf8_lossy(&out).trim().to_string();
        if s.is_empty() {
            return None;
        }
        Some(s)
    }

    /// Tracked files (repo-relative, NUL-safe). Empty outside a git repo.
    ///
    /// Uses `ENUMERATION_MAX_OUTPUT_BYTES` rather than the (much smaller)
    /// construction-time default: a repo with tens of thousands of tracked
    /// files can legitimately exceed 1 MiB of `ls-files -z` output, and
    /// treating that overflow as "no files" previously emptied the index
    /// outright above roughly 25k files.
    pub fn ls_files(&self) -> Vec<String> {
        self.ls_files_or_err().unwrap_or_default()
    }

    /// Same as `ls_files`, but propagates the `GitError` instead of
    /// degrading to an empty list: for a caller that must tell "no tracked
    /// files" from "not a repository" (the task handoff refuses to run on
    /// either, with different messages).
    pub fn ls_files_or_err(&self) -> Result<Vec<String>, GitError> {
        let out = self
            .with_max_output_bytes(Some(ENUMERATION_MAX_OUTPUT_BYTES))
            .run(&["ls-files", "-z"])?;
        Ok(out
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect())
    }

    /// Working-tree root as git sees it (`git rev-parse --show-toplevel`):
    /// the enclosing repository from any subdirectory, the worktree itself
    /// from a linked worktree. `None` outside a repository, inside a bare
    /// one, or on any git failure.
    pub fn show_toplevel(&self) -> Option<std::path::PathBuf> {
        let out = self.run_opt(&["rev-parse", "--show-toplevel"])?;
        let top = String::from_utf8_lossy(&out).trim().to_owned();
        (!top.is_empty()).then(|| std::path::PathBuf::from(top))
    }

    /// Number of commits reachable from any ref (`git rev-list --count
    /// --all`). `Some(0)` in a repository without commits; `None` outside a
    /// repository or on any git failure. The `status` and `doctor`
    /// freshness checks compare it with the facts store's commit count.
    pub fn rev_list_count_all(&self) -> Option<u64> {
        let out = self.run_opt(&["rev-list", "--count", "--all"])?;
        String::from_utf8_lossy(&out).trim().parse().ok()
    }

    /// Raw `git status --porcelain=v2 -z --untracked-files=all --ignored=no
    /// -- <path>` output for one path, for the porcelain-v2 fingerprint
    /// parser in `pixel-ops`. Empty when the path is clean; an `Err` when
    /// git fails, so a caller can tell "clean" from "unknown".
    pub fn status_porcelain_v2_path(&self, path: &str) -> Result<String, GitError> {
        let out = self
            .with_max_output_bytes(Some(ENUMERATION_MAX_OUTPUT_BYTES))
            .run(&[
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
                "--ignored=no",
                "--",
                path,
            ])?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// Blob content of `path` as it exists in commit `oid`
    /// (`git show --end-of-options oid:path`). `None` on any git failure,
    /// missing path at that commit, or an invalid `oid`.
    ///
    /// Uses `BLOB_MAX_OUTPUT_BYTES` (kept equal to
    /// `pixel_index::index::MAX_FILE_BYTES`) rather than the small
    /// construction-time default, so a file within the size the index
    /// considers indexable is never silently dropped here.
    pub fn show_blob(&self, oid: &str, rel: &str) -> Option<Vec<u8>> {
        validate_ref(oid).ok()?;
        let spec = format!("{oid}:{rel}");
        self.with_max_output_bytes(Some(BLOB_MAX_OUTPUT_BYTES))
            .run_opt(&["show", end_of_options(), &spec])
    }

    /// The staged version of `rel` (`git show :<rel>`) — the "old" side of a
    /// plain `git diff`, which compares the working tree against the index
    /// rather than against a commit. `None` when the path is not in the
    /// index (untracked, or added in the working tree only) or git failed.
    ///
    /// [`GitRunner::show_blob`] cannot serve this: it validates its `oid`
    /// with `validate_ref`, which refuses the empty left-hand side the
    /// `:<path>` spec needs. The spec still goes after `--end-of-options`,
    /// so a path opening on a dash cannot be read as a flag.
    pub fn show_index_blob(&self, rel: &str) -> Option<Vec<u8>> {
        let spec = format!(":{rel}");
        self.with_max_output_bytes(Some(BLOB_MAX_OUTPUT_BYTES))
            .run_opt(&["show", end_of_options(), &spec])
    }

    /// All files in commit `oid`'s tree (`git ls-tree -r --name-only -z`).
    /// Returns the commit's file universe, not the working-tree index —
    /// staged additions/deletions don't affect this list. Used to build
    /// cache-keyed base shards that are a pure function of (commit, extractor).
    pub fn ls_tree(&self, oid: &str) -> Vec<String> {
        if validate_ref(oid).is_err() {
            return Vec::new();
        }
        let Some(out) = self
            .with_max_output_bytes(Some(ENUMERATION_MAX_OUTPUT_BYTES))
            .run_opt(&["ls-tree", "-r", "--name-only", "-z", oid])
        else {
            return Vec::new();
        };
        out.split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    }

    /// Paths of the blobs in commit `oid`'s tree (`git ls-tree -r -z`),
    /// gitlinks (submodules) excluded: their objects live in another
    /// repository and cannot be read here. Unlike [`Self::ls_tree`] a git
    /// failure is an error, never an empty list: a base shard built from a
    /// failed listing is empty and looks complete.
    pub fn ls_tree_blobs(&self, oid: &str) -> Result<Vec<String>, GitError> {
        validate_ref(oid)?;
        let out = self
            .with_max_output_bytes(Some(ENUMERATION_MAX_OUTPUT_BYTES))
            .run(&["ls-tree", "-r", "-z", oid])?;
        Ok(parse_ls_tree_blobs(&out))
    }

    /// Whether `oid` names a commit this repository holds
    /// (`git cat-file -e <oid>^{commit}`). `false` for an invalid ref, an
    /// object that is not a commit, or a commit that was never fetched or
    /// has been pruned.
    pub fn commit_exists(&self, oid: &str) -> bool {
        if validate_ref(oid).is_err() {
            return false;
        }
        let spec = format!("{oid}^{{commit}}");
        self.run(&["cat-file", "-e", &spec]).is_ok()
    }

    /// Size of a committed blob without materializing it.
    pub fn blob_size(&self, oid: &str, rel: &str) -> Option<u64> {
        validate_ref(oid).ok()?;
        let spec = format!("{oid}:{rel}");
        let out = self.run_opt(&["cat-file", "-s", &spec])?;
        String::from_utf8(out).ok()?.trim().parse().ok()
    }

    /// `git diff --name-status --no-renames -z <from> <to>` as
    /// (status, path). Statuses are single chars: A, M, D, T, etc. Empty on
    /// any git failure or if either ref is invalid.
    ///
    /// Uses `ENUMERATION_MAX_OUTPUT_BYTES`: a diff spanning tens of
    /// thousands of paths can exceed 1 MiB of `--name-status` output, and
    /// that must not silently read back as "nothing changed".
    pub fn diff_name_status(&self, from: &str, to: &str) -> Vec<(char, String)> {
        self.diff_name_status_or_err(from, to).unwrap_or_default()
    }

    /// Same output as `diff_name_status`, but propagates a `GitError`
    /// instead of silently degrading to an empty result on any failure —
    /// including output-cap overflow or an invalid ref. Required by any
    /// safety-critical caller that decides whether a set of "changed paths"
    /// intersects the working tree's dirty files before proceeding with a
    /// fast-forward/rebase (e.g. `pixel-ops::update`, `pixel-ops::reconcile`):
    /// an undetermined changed-path set must abort that decision, never be
    /// silently read as "nothing changed" (which would let a mutation
    /// proceed as if no dirty file were ever at risk).
    pub fn diff_name_status_or_err(
        &self,
        from: &str,
        to: &str,
    ) -> Result<Vec<(char, String)>, GitError> {
        validate_ref(from)?;
        validate_ref(to)?;
        let out = self
            .with_max_output_bytes(Some(ENUMERATION_MAX_OUTPUT_BYTES))
            .run(&["diff", "--name-status", "--no-renames", "-z", from, to])?;
        let mut fields = out.split(|&b| b == 0).filter(|s| !s.is_empty());
        let mut result = Vec::new();
        while let Some(status) = fields.next() {
            let Some(path) = fields.next() else { break };
            let c = status.first().copied().unwrap_or(b'M') as char;
            result.push((c, String::from_utf8_lossy(path).into_owned()));
        }
        Ok(result)
    }

    /// `git status --porcelain -z --untracked-files=all --no-renames` as
    /// (XY, path). Untracked files appear with XY `"??"`. Empty on any git
    /// failure (including cap overflow) — safe for callers where "status
    /// unknown" degrading to "nothing changed" only costs staleness (e.g.
    /// the search index's dirty overlay). A caller for whom that
    /// degradation would be unsafe (e.g. deciding whether it is safe to
    /// overwrite a file) MUST use `status_porcelain_or_err` instead so an
    /// undetermined status aborts rather than reading as clean.
    pub fn status_porcelain(&self) -> Vec<(String, String)> {
        self.status_porcelain_or_err().unwrap_or_default()
    }

    /// Same as `status_porcelain`, but propagates a `GitError` instead of
    /// silently degrading to an empty result on any failure — including
    /// output-cap overflow. Required by any safety-critical caller that
    /// decides whether it is safe to overwrite working-tree content:
    /// `status_porcelain`'s "empty on failure" behavior previously let
    /// `pixel plan-rollback --apply` conclude "nothing is dirty" (and overwrite an
    /// actually-dirty file with no strategy flag given) whenever a large
    /// untracked tree pushed `status --porcelain` output past the output
    /// cap. Uses `ENUMERATION_MAX_OUTPUT_BYTES` so a legitimately large
    /// untracked tree does not trip this either.
    pub fn status_porcelain_or_err(&self) -> Result<Vec<(String, String)>, GitError> {
        let out = self
            .with_max_output_bytes(Some(ENUMERATION_MAX_OUTPUT_BYTES))
            .run(&[
                "status",
                "--porcelain",
                "-z",
                "--untracked-files=all",
                "--no-renames",
            ])?;
        Ok(out
            .split(|&b| b == 0)
            .filter(|s| s.len() > 3)
            .map(|entry| {
                let xy = String::from_utf8_lossy(&entry[0..2]).into_owned();
                let path = String::from_utf8_lossy(&entry[3..]).into_owned();
                (xy, path)
            })
            .collect())
    }

    /// `git diff --unified=0 [--end-of-options <base_ref>] -- .`, validating
    /// `base_ref` via `validate_ref` first when given (port of
    /// `pixel-graph::changes::detect`'s diff invocation).
    ///
    /// Uses `ENUMERATION_MAX_OUTPUT_BYTES`: a diff over many changed files
    /// can exceed 1 MiB, and unlike most enumeration calls this one already
    /// propagates a hard error on overflow rather than degrading to
    /// "empty" — raising the cap keeps that error from firing on
    /// legitimately large (not just pathological) diffs.
    pub fn diff_unified0(&self, base_ref: Option<&str>) -> Result<Vec<u8>, GitError> {
        let mut args: Vec<&str> = vec!["diff", "--unified=0"];
        if let Some(r) = base_ref {
            validate_ref(r)?;
            args.push(end_of_options());
            args.push(r);
        }
        args.push("--");
        args.push(".");
        self.with_max_output_bytes(Some(ENUMERATION_MAX_OUTPUT_BYTES))
            .run(&args)
    }

    /// `git log --follow -n <depth> --format=%H%x1f%ct%x1f%s -- <path>`,
    /// parsed into (oid, commit_unix_timestamp, subject) tuples. Port of
    /// `pixel-cli::rescue_cmd::plan`'s history walk.
    pub fn log_follow(
        &self,
        path: &str,
        depth: usize,
    ) -> Result<Vec<(String, i64, String)>, GitError> {
        let depth_str = depth.to_string();
        let out = self.run(&[
            "log",
            "--follow",
            "-n",
            &depth_str,
            "--format=%H%x1f%ct%x1f%s",
            "--",
            path,
        ])?;
        let text = String::from_utf8(out)?;
        let mut rows = Vec::new();
        for line in text.lines() {
            let mut parts = line.split('\u{1f}');
            let (Some(oid), Some(ct), Some(subject)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            rows.push((
                oid.to_string(),
                ct.parse().unwrap_or(0),
                subject.to_string(),
            ));
        }
        Ok(rows)
    }

    /// `git rev-parse <commit>:<path>` — the blob oid of `path` as it exists
    /// in `commit`. `None` on any git failure, missing path, or an invalid
    /// `commit`. Port of `pixel-cli::rescue_cmd::blob_oid`.
    pub fn rev_parse_at(&self, commit: &str, path: &str) -> Option<String> {
        validate_ref(commit).ok()?;
        let spec = format!("{commit}:{path}");
        let out = self.run_opt(&["rev-parse", &spec])?;
        let s = String::from_utf8_lossy(&out).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    }

    /// `git stash push -m <message>`.
    pub fn stash_push(&self, message: &str) -> Result<(), GitError> {
        self.run(&["stash", "push", "-m", message]).map(|_| ())
    }

    /// `git stash push -m <message> -- <paths>`. Stashes only the named
    /// paths (port of `pixel-cli::rescue_cmd::apply`'s stash-first branch).
    /// Paths are placed after `--` so they're never parsed as options.
    pub fn stash_push_paths(&self, message: &str, paths: &[String]) -> Result<(), GitError> {
        let mut args: Vec<String> = vec![
            "stash".into(),
            "push".into(),
            "-m".into(),
            message.into(),
            "--".into(),
        ];
        args.extend(paths.iter().cloned());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&arg_refs).map(|_| ())
    }

    /// `git rev-parse --verify -q <oid>^{commit}` — confirms `oid` resolves
    /// to a commit in this repo. Port of `pixel-cli::rescue_cmd::apply`'s
    /// commit-existence check. `oid` is validated via `validate_ref` first.
    pub fn rev_verify_commit(&self, oid: &str) -> Result<(), GitError> {
        validate_ref(oid)?;
        let spec = format!("{oid}^{{commit}}");
        self.run(&["rev-parse", "--verify", "-q", &spec])
            .map(|_| ())
    }

    /// `git show <oid>:<path>` returning the blob content as a String.
    /// `oid` is validated via `validate_ref`. Port of
    /// `pixel-cli::rescue_cmd::apply`'s content-restore path. Errors carry
    /// a redacted stderr.
    ///
    /// Uses `BLOB_MAX_OUTPUT_BYTES` (kept equal to
    /// `pixel_index::index::MAX_FILE_BYTES`) rather than the small
    /// construction-time default. Previously capped at 1 MiB, this made
    /// `rescue --apply` hard-fail on files over 1 MiB with a misleading
    /// "does not exist at <oid>" error even though the file existed and was
    /// well within the size the index itself considers restorable.
    pub fn show_blob_string(&self, oid: &str, path: &str) -> Result<String, GitError> {
        validate_ref(oid)?;
        let spec = format!("{oid}:{path}");
        let out = self
            .with_max_output_bytes(Some(BLOB_MAX_OUTPUT_BYTES))
            .run(&["show", end_of_options(), &spec])?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// `git show <oid>^:<path>` — blob content of `path` at the commit's
    /// FIRST PARENT. The pre-deletion read: for a commit that deleted
    /// `path`, this returns the last content that existed before the
    /// deletion. Only the bare `oid` is validated via `validate_ref`; the
    /// `^` suffix is appended internally (same pattern as
    /// `rev_verify_commit`'s `^{{commit}}` suffix) so callers never pass a
    /// suffixed refspec through validation.
    pub fn show_blob_string_at_parent(&self, oid: &str, path: &str) -> Result<String, GitError> {
        validate_ref(oid)?;
        let spec = format!("{oid}^:{path}");
        let out = self
            .with_max_output_bytes(Some(BLOB_MAX_OUTPUT_BYTES))
            .run(&["show", end_of_options(), &spec])?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// `git merge-file -L <label1> -L <label2> -L <label3> <current> <base> <other>`.
    /// Returns git's exit code as data: 0 = clean merge, positive = conflict
    /// count (markers left in `current`), `None` = killed by a signal. Port
    /// of `pixel-cli::rescue_cmd::apply`'s 3-way merge branch, including the
    /// cosmetic `-L` diff3 labels. Goes through the runner's bounded
    /// `merge-file` primitive, so the timeout and the output cap apply and
    /// stderr is captured and redacted instead of inheriting the caller's.
    pub fn merge_file_with_labels(
        &self,
        current: &Path,
        base: &Path,
        other: &Path,
        label_ours: &str,
        label_base: &str,
        label_theirs: &str,
    ) -> Result<GitOutput, GitError> {
        self.run_merge_file(
            current,
            base,
            other,
            Some([label_ours, label_base, label_theirs]),
        )
    }
}

/// The blob paths of `git ls-tree -r -z` output: each NUL-terminated entry
/// is `<mode> SP <type> SP <object> TAB <path>`; only `blob` entries count.
fn parse_ls_tree_blobs(out: &[u8]) -> Vec<String> {
    out.split(|&b| b == 0)
        .filter_map(|entry| {
            let tab = entry.iter().position(|&b| b == b'\t')?;
            let kind = entry[..tab].split(|&b| b == b' ').nth(1)?;
            (kind == b"blob").then(|| String::from_utf8_lossy(&entry[tab + 1..]).into_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pixel-git-{tag}-{}-{}",
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

    /// The index rebuilds a restored base whose commit this clone lacks, so
    /// `commit_exists` must say no to an absent commit, to a malformed ref and
    /// to an object that is not a commit, and yes to a commit it holds.
    #[test]
    fn commit_exists_accepts_only_commits_the_repository_holds() {
        let root = tmpdir("plumbing-commit-exists");
        init_repo(&root);
        std::fs::write(root.join("a.txt"), b"hello\n").unwrap();
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-q", "-m", "first"]);
        let runner = GitRunner::new(&root);
        let head = runner.rev_parse_head().unwrap();
        let tree = String::from_utf8(runner.run(&["rev-parse", "HEAD^{tree}"]).unwrap()).unwrap();

        assert!(runner.commit_exists(&head));
        assert!(!runner.commit_exists(&"0".repeat(40)), "never fetched");
        assert!(!runner.commit_exists(tree.trim()), "a tree, not a commit");
        assert!(!runner.commit_exists("--output=/tmp/x"), "not a ref");
    }

    #[test]
    fn rev_parse_head_and_ls_files_and_status_and_diff() {
        let root = tmpdir("plumbing-basic");
        init_repo(&root);
        std::fs::write(root.join("a.txt"), b"hello\n").unwrap();
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-q", "-m", "first"]);
        let runner = GitRunner::new(&root);

        let head1 = runner.rev_parse_head().expect("head after first commit");
        assert_eq!(head1.len(), 40);

        assert_eq!(runner.ls_files(), vec!["a.txt".to_string()]);

        std::fs::write(root.join("b.txt"), b"second\n").unwrap();
        git(&root, &["add", "b.txt"]);
        git(&root, &["commit", "-q", "-m", "second"]);
        let head2 = runner.rev_parse_head().unwrap();
        assert_ne!(head1, head2);

        let diff = runner.diff_name_status(&head1, &head2);
        assert_eq!(diff, vec![('A', "b.txt".to_string())]);

        std::fs::write(root.join("c.txt"), b"untracked\n").unwrap();
        let status = runner.status_porcelain();
        assert!(status.iter().any(|(xy, p)| xy == "??" && p == "c.txt"));
    }

    /// A submodule is a gitlink in the tree, not a blob: listing it would
    /// make every base build count an unreadable file.
    #[test]
    fn ls_tree_blobs_lists_blobs_and_skips_gitlinks() {
        let root = tmpdir("plumbing-ls-tree-blobs");
        init_repo(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.txt"), b"a\n").unwrap();
        std::fs::write(root.join("with space.txt"), b"b\n").unwrap();
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000,1111111111111111111111111111111111111111,vendor/sub",
            ],
        );
        git(&root, &["commit", "-q", "-m", "tree with a gitlink"]);
        let runner = GitRunner::new(&root);
        let head = runner.rev_parse_head().unwrap();
        assert_eq!(
            runner.ls_tree_blobs(&head).unwrap(),
            ["src/a.txt", "with space.txt"]
        );
        assert!(
            runner.ls_tree(&head).contains(&"vendor/sub".to_string()),
            "ls_tree keeps listing every entry"
        );
        assert!(
            runner
                .ls_tree_blobs("0000000000000000000000000000000000000000")
                .is_err()
        );
        assert!(runner.ls_tree_blobs("-bad").is_err());
        assert_eq!(
            parse_ls_tree_blobs(b"100644 blob abc\tx\x00garbage\x00040000 tree def\tdir\x00"),
            ["x"],
            "entries without a tab or of another type are not blobs"
        );
    }

    #[test]
    fn rev_list_count_all_counts_every_ref_and_is_none_outside_a_repo() {
        let root = tmpdir("plumbing-revlist");
        init_repo(&root);
        let runner = GitRunner::new(&root);
        assert_eq!(runner.rev_list_count_all(), Some(0), "empty repo counts 0");

        std::fs::write(root.join("a.txt"), b"a\n").unwrap();
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-q", "-m", "first"]);
        assert_eq!(runner.rev_list_count_all(), Some(1));

        // A commit on another branch is still reachable from a ref, so
        // `--all` counts it: two, not one.
        git(&root, &["checkout", "-q", "-b", "side"]);
        std::fs::write(root.join("b.txt"), b"b\n").unwrap();
        git(&root, &["add", "b.txt"]);
        git(&root, &["commit", "-q", "-m", "side"]);
        assert_eq!(runner.rev_list_count_all(), Some(2));

        let outside = tmpdir("plumbing-revlist-outside");
        assert_eq!(GitRunner::new(&outside).rev_list_count_all(), None);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn show_toplevel_resolves_a_subdirectory_to_the_repo_root() {
        let root = tmpdir("plumbing-toplevel");
        init_repo(&root);
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        let top = GitRunner::new(&nested)
            .show_toplevel()
            .expect("inside a repo");
        assert_eq!(top, root.canonicalize().unwrap());

        let outside = tmpdir("plumbing-toplevel-outside");
        assert_eq!(GitRunner::new(&outside).show_toplevel(), None);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ls_files_or_err_distinguishes_no_files_from_no_repo() {
        let root = tmpdir("plumbing-lsfiles-err");
        init_repo(&root);
        let runner = GitRunner::new(&root);
        assert_eq!(runner.ls_files_or_err().unwrap(), Vec::<String>::new());

        std::fs::write(root.join("a.txt"), b"a\n").unwrap();
        std::fs::write(root.join("b.txt"), b"b\n").unwrap();
        git(&root, &["add", "a.txt", "b.txt"]);
        let mut files = runner.ls_files_or_err().unwrap();
        files.sort();
        assert_eq!(files, vec!["a.txt".to_string(), "b.txt".to_string()]);
        assert_eq!(runner.ls_files(), files, "ls_files is the lenient view");

        let outside = tmpdir("plumbing-lsfiles-outside");
        let err = GitRunner::new(&outside).ls_files_or_err().unwrap_err();
        assert!(
            matches!(err, GitError::NonZeroExit { .. }),
            "outside a repo git exits non-zero: {err}"
        );
        assert!(GitRunner::new(&outside).ls_files().is_empty());
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn status_porcelain_v2_path_reports_one_path_and_errors_outside_a_repo() {
        let root = tmpdir("plumbing-status-v2");
        init_repo(&root);
        std::fs::write(root.join("a.txt"), b"a\n").unwrap();
        std::fs::write(root.join("b.txt"), b"b\n").unwrap();
        git(&root, &["add", "a.txt", "b.txt"]);
        git(&root, &["commit", "-q", "-m", "first"]);
        let runner = GitRunner::new(&root);

        assert_eq!(runner.status_porcelain_v2_path("a.txt").unwrap(), "");

        std::fs::write(root.join("a.txt"), b"changed\n").unwrap();
        std::fs::write(root.join("b.txt"), b"changed\n").unwrap();
        let out = runner.status_porcelain_v2_path("a.txt").unwrap();
        assert!(
            out.starts_with("1 .M "),
            "porcelain v2 ordinary record: {out:?}"
        );
        assert!(out.contains("a.txt"), "{out:?}");
        assert!(!out.contains("b.txt"), "only the asked path: {out:?}");

        std::fs::write(root.join("new.txt"), b"n\n").unwrap();
        let out = runner.status_porcelain_v2_path("new.txt").unwrap();
        assert_eq!(out, "? new.txt\0", "untracked records are requested");

        let outside = tmpdir("plumbing-status-v2-outside");
        assert!(
            GitRunner::new(&outside)
                .status_porcelain_v2_path("a.txt")
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn show_blob_string_at_parent_returns_pre_deletion_content() {
        let root = tmpdir("plumbing-parent");
        init_repo(&root);
        std::fs::write(root.join("gone.txt"), b"pre-deletion body\n").unwrap();
        git(&root, &["add", "gone.txt"]);
        git(&root, &["commit", "-q", "-m", "add gone.txt"]);
        git(&root, &["rm", "-q", "gone.txt"]);
        git(&root, &["commit", "-q", "-m", "delete gone.txt"]);
        let runner = GitRunner::new(&root);
        let del_oid = runner.rev_parse_head().unwrap();

        // The file does not exist at the deleting commit itself...
        assert!(runner.show_blob_string(&del_oid, "gone.txt").is_err());
        // ...but the parent read returns the pre-deletion content.
        let content = runner
            .show_blob_string_at_parent(&del_oid, "gone.txt")
            .expect("parent read must succeed for a deletion commit");
        assert_eq!(content, "pre-deletion body\n");
    }

    #[test]
    fn show_blob_and_blob_size_for_committed_file() {
        let root = tmpdir("plumbing-blob");
        init_repo(&root);
        std::fs::write(root.join("f.txt"), b"0123456789").unwrap();
        git(&root, &["add", "f.txt"]);
        git(&root, &["commit", "-q", "-m", "add f"]);
        let runner = GitRunner::new(&root);
        let head = runner.rev_parse_head().unwrap();

        let blob = runner.show_blob(&head, "f.txt").expect("blob content");
        assert_eq!(blob, b"0123456789");

        let size = runner.blob_size(&head, "f.txt").expect("blob size");
        assert_eq!(size, 10);
    }

    /// The index version, which is the base side of a plain `git diff`. It
    /// has to be the staged bytes and not HEAD's: change detection reads a
    /// deleted symbol out of it, and HEAD would answer for a state the
    /// diff never compared against.
    #[test]
    fn show_index_blob_reads_the_staged_bytes() {
        let root = tmpdir("plumbing-index-blob");
        init_repo(&root);
        std::fs::write(root.join("f.txt"), b"committed\n").unwrap();
        git(&root, &["add", "f.txt"]);
        git(&root, &["commit", "-q", "-m", "add f"]);
        std::fs::write(root.join("f.txt"), b"staged\n").unwrap();
        git(&root, &["add", "f.txt"]);
        // A third version that is only in the working tree, so each of the
        // three states is distinguishable.
        std::fs::write(root.join("f.txt"), b"working\n").unwrap();
        let runner = GitRunner::new(&root);

        assert_eq!(runner.show_index_blob("f.txt").unwrap(), b"staged\n");
        let head = runner.rev_parse_head().unwrap();
        assert_eq!(runner.show_blob(&head, "f.txt").unwrap(), b"committed\n");

        // Never in the index: untracked, and a path that does not exist.
        std::fs::write(root.join("untracked.txt"), b"u\n").unwrap();
        assert!(runner.show_index_blob("untracked.txt").is_none());
        assert!(runner.show_index_blob("nope.txt").is_none());
    }

    #[test]
    fn show_index_blob_rejects_a_path_that_reads_as_a_flag() {
        let root = tmpdir("plumbing-index-inject");
        init_repo(&root);
        let runner = GitRunner::new(&root);
        // `--end-of-options` keeps this a path, so git fails to find it
        // rather than acting on it.
        assert!(runner.show_index_blob("--upload-pack=/bin/sh").is_none());
    }

    #[test]
    fn show_blob_rejects_flag_injection_oid() {
        let root = tmpdir("plumbing-inject");
        init_repo(&root);
        let runner = GitRunner::new(&root);
        assert!(runner.show_blob("--upload-pack=/bin/sh", "f.txt").is_none());
    }

    #[test]
    fn log_follow_and_rev_parse_at() {
        let root = tmpdir("plumbing-log");
        init_repo(&root);
        std::fs::write(root.join("g.txt"), b"v1").unwrap();
        git(&root, &["add", "g.txt"]);
        git(&root, &["commit", "-q", "-m", "v1 commit"]);
        std::fs::write(root.join("g.txt"), b"v2").unwrap();
        git(&root, &["add", "g.txt"]);
        git(&root, &["commit", "-q", "-m", "v2 commit"]);
        let runner = GitRunner::new(&root);

        let rows = runner.log_follow("g.txt", 10).expect("log rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].2, "v2 commit");
        assert_eq!(rows[1].2, "v1 commit");

        let head = runner.rev_parse_head().unwrap();
        let blob_at_head = runner.rev_parse_at(&head, "g.txt").expect("blob oid");
        assert!(!blob_at_head.is_empty());
    }

    #[test]
    fn diff_unified0_validates_base_ref() {
        let root = tmpdir("plumbing-diffu0");
        init_repo(&root);
        std::fs::write(root.join("h.txt"), b"one").unwrap();
        git(&root, &["add", "h.txt"]);
        git(&root, &["commit", "-q", "-m", "h1"]);
        let runner = GitRunner::new(&root);

        assert!(matches!(
            runner.diff_unified0(Some("--evil")),
            Err(GitError::InvalidRef(_))
        ));

        let head = runner.rev_parse_head().unwrap();
        std::fs::write(root.join("h.txt"), b"two").unwrap();
        let out = runner.diff_unified0(Some(&head)).expect("diff output");
        assert!(String::from_utf8_lossy(&out).contains("h.txt"));
    }

    #[test]
    fn stash_push_stashes_dirty_file() {
        let root = tmpdir("plumbing-stash");
        init_repo(&root);
        std::fs::write(root.join("s.txt"), b"tracked").unwrap();
        git(&root, &["add", "s.txt"]);
        git(&root, &["commit", "-q", "-m", "s1"]);
        std::fs::write(root.join("s.txt"), b"dirty edit").unwrap();
        let runner = GitRunner::new(&root);
        runner.stash_push("test stash").expect("stash push");
        let content = std::fs::read_to_string(root.join("s.txt")).unwrap();
        assert_eq!(content, "tracked");
    }

    #[test]
    fn rev_verify_commit_rejects_flag_injection() {
        let root = tmpdir("plumbing-revverify");
        init_repo(&root);
        std::fs::write(root.join("v.txt"), b"v").unwrap();
        git(&root, &["add", "v.txt"]);
        git(&root, &["commit", "-q", "-m", "v1"]);
        let runner = GitRunner::new(&root);
        let head = runner.rev_parse_head().unwrap();
        assert!(runner.rev_verify_commit(&head).is_ok());
        // Flag injection rejected by validate_ref before reaching git.
        assert!(runner.rev_verify_commit("--upload-pack=/bin/sh").is_err());
    }

    #[test]
    fn show_blob_string_returns_content_and_rejects_injection() {
        let root = tmpdir("plumbing-showstr");
        init_repo(&root);
        std::fs::write(root.join("c.txt"), b"content here").unwrap();
        git(&root, &["add", "c.txt"]);
        git(&root, &["commit", "-q", "-m", "c1"]);
        let runner = GitRunner::new(&root);
        let head = runner.rev_parse_head().unwrap();
        let content = runner
            .show_blob_string(&head, "c.txt")
            .expect("blob string");
        assert_eq!(content, "content here");
        // Flag injection rejected.
        assert!(
            runner
                .show_blob_string("--output=/tmp/evil", "c.txt")
                .is_err()
        );
    }

    #[test]
    fn stash_push_paths_stashes_only_named_files() {
        let root = tmpdir("plumbing-stashpaths");
        init_repo(&root);
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::write(root.join("b.txt"), b"b").unwrap();
        git(&root, &["add", "a.txt", "b.txt"]);
        git(&root, &["commit", "-q", "-m", "ab"]);
        std::fs::write(root.join("a.txt"), b"a-dirty").unwrap();
        std::fs::write(root.join("b.txt"), b"b-dirty").unwrap();
        let runner = GitRunner::new(&root);
        runner
            .stash_push_paths("partial", &["a.txt".to_string()])
            .expect("stash push paths");
        // a.txt stashed (clean), b.txt still dirty
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "a");
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "b-dirty"
        );
    }

    #[test]
    fn merge_file_with_labels_reports_the_conflict_count_and_keeps_the_labels() {
        let root = tmpdir("plumbing-mergelabels");
        std::fs::write(root.join("current.txt"), "one-mine\ntwo\nthree\n").unwrap();
        std::fs::write(root.join("base.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(root.join("other.txt"), "one-theirs\ntwo\nthree\n").unwrap();

        let runner = GitRunner::new(&root);
        let out = runner
            .merge_file_with_labels(
                &root.join("current.txt"),
                &root.join("base.txt"),
                &root.join("other.txt"),
                "in-progress",
                "HEAD",
                "rescue:abc1234",
            )
            .expect("merge-file runs");
        assert_eq!(out.code, Some(1), "one conflicting region: {out:?}");
        let merged = std::fs::read_to_string(root.join("current.txt")).unwrap();
        assert!(merged.contains("<<<<<<< in-progress"), "{merged}");
        assert!(merged.contains(">>>>>>> rescue:abc1234"), "{merged}");
    }

    // -----------------------------------------------------------------
    // Regression coverage for the output-cap bug: every enumeration call
    // used to share the 1 MiB `DEFAULT_MAX_OUTPUT_BYTES` cap, so any repo
    // whose `ls-files`/`status --porcelain`/`diff --name-status` output
    // crossed that threshold silently read back as *empty* rather than
    // erroring — the exact defect class this module now guards against via
    // `ENUMERATION_MAX_OUTPUT_BYTES` / `BLOB_MAX_OUTPUT_BYTES`.
    // -----------------------------------------------------------------

    /// A directory-name prefix long enough to make each enumerated path
    /// (well under the ~255-byte per-component filesystem limit) push total
    /// `-z`-delimited output past the *old* 1 MiB default cap with only a
    /// few thousand files, so these tests stay fast.
    fn long_component(tag: &str) -> String {
        format!("{tag}-{}", "x".repeat(240))
    }

    #[test]
    fn ls_files_survives_enumeration_output_past_the_old_1mib_cap() {
        const N: usize = 5300; // ~5300 * ~250 bytes ≈ 1.3 MiB of `ls-files -z` output
        let root = tmpdir("plumbing-lsfiles-big");
        init_repo(&root);
        let dir = long_component("tracked");
        std::fs::create_dir_all(root.join(&dir)).unwrap();
        for i in 0..N {
            std::fs::write(root.join(&dir).join(format!("f{i:05}.txt")), b"x").unwrap();
        }
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "big tracked tree"]);

        let runner = GitRunner::new(&root);
        let files = runner.ls_files();
        assert_eq!(
            files.len(),
            N,
            "ls_files must return every tracked file even when output exceeds the old 1 MiB cap \
             (previously silently returned an empty Vec above that threshold)"
        );
    }

    #[test]
    fn status_porcelain_survives_enumeration_output_past_the_old_1mib_cap() {
        const N: usize = 5300; // pushes `status --porcelain -z` past the old 1 MiB cap
        let root = tmpdir("plumbing-status-big");
        init_repo(&root);
        std::fs::write(root.join("tracked.txt"), b"hello").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-q", "-m", "seed"]);

        let dir = long_component("untracked");
        std::fs::create_dir_all(root.join(&dir)).unwrap();
        for i in 0..N {
            std::fs::write(root.join(&dir).join(format!("g{i:05}.txt")), b"y").unwrap();
        }

        let runner = GitRunner::new(&root);
        let status = runner.status_porcelain();
        let untracked = status.iter().filter(|(xy, _)| xy == "??").count();
        assert_eq!(
            untracked, N,
            "status_porcelain must report every untracked file even when output exceeds the old \
             1 MiB cap (previously silently returned an empty Vec, which made a dirty working \
             tree with a large untracked tree look completely clean)"
        );

        let strict = runner
            .status_porcelain_or_err()
            .expect("status_porcelain_or_err must also survive the same large untracked tree");
        assert_eq!(strict.iter().filter(|(xy, _)| xy == "??").count(), N);
    }

    #[test]
    fn status_porcelain_or_err_propagates_failure_instead_of_reading_as_clean() {
        // Outside a git repo, `git status` fails outright. The strict
        // variant MUST surface that as an error (never as "nothing is
        // dirty"), which is the exact contract `pixel::rescue_cmd::apply`'s
        // dirty-file guard now depends on.
        let root = tmpdir("plumbing-status-not-a-repo");
        let runner = GitRunner::new(&root);
        assert!(
            runner.status_porcelain_or_err().is_err(),
            "status_porcelain_or_err must error, not silently report an empty (\"clean\") status"
        );
        // The lenient variant is still allowed to degrade to empty for
        // non-safety-critical callers (e.g. the search index's dirty
        // overlay), which only costs staleness, not data loss.
        assert_eq!(runner.status_porcelain(), Vec::new());
    }

    #[test]
    fn diff_name_status_survives_enumeration_output_past_the_old_1mib_cap() {
        const N: usize = 5300;
        let root = tmpdir("plumbing-diffns-big");
        init_repo(&root);
        std::fs::write(root.join("seed.txt"), b"seed").unwrap();
        git(&root, &["add", "seed.txt"]);
        git(&root, &["commit", "-q", "-m", "seed"]);
        let from = GitRunner::new(&root).rev_parse_head().unwrap();

        let dir = long_component("added");
        std::fs::create_dir_all(root.join(&dir)).unwrap();
        for i in 0..N {
            std::fs::write(root.join(&dir).join(format!("h{i:05}.txt")), b"z").unwrap();
        }
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "big add"]);
        let to = GitRunner::new(&root).rev_parse_head().unwrap();

        let runner = GitRunner::new(&root);
        let diff = runner.diff_name_status(&from, &to);
        let added = diff.iter().filter(|(status, _)| *status == 'A').count();
        assert_eq!(
            added, N,
            "diff_name_status must report every added path even when output exceeds the old \
             1 MiB cap (previously silently returned an empty Vec, which would empty the delta \
             layer above a large enough change set)"
        );
    }

    #[test]
    fn show_blob_and_show_blob_string_survive_files_between_the_old_1mib_and_new_4mib_cap() {
        let root = tmpdir("plumbing-blob-big");
        init_repo(&root);
        // ~2 MiB file: over the old 1 MiB default cap, comfortably under
        // the new 4 MiB blob cap (kept equal to `pixel_index::index::MAX_FILE_BYTES`).
        let needle = "UNIQUE_NEEDLE_TOKEN_2MIB";
        let mut content = vec![b'a'; 2 * 1024 * 1024];
        content.extend_from_slice(needle.as_bytes());
        std::fs::write(root.join("big.txt"), &content).unwrap();
        git(&root, &["add", "big.txt"]);
        git(&root, &["commit", "-q", "-m", "add 2mib file"]);

        let runner = GitRunner::new(&root);
        let head = runner.rev_parse_head().unwrap();

        let blob = runner
            .show_blob(&head, "big.txt")
            .expect("show_blob must not drop a ~2 MiB file (previously capped at 1 MiB)");
        assert_eq!(blob.len(), content.len());
        assert!(String::from_utf8_lossy(&blob).contains(needle));

        let blob_string = runner
            .show_blob_string(&head, "big.txt")
            .expect("show_blob_string must not drop a ~2 MiB file (previously capped at 1 MiB)");
        assert!(blob_string.contains(needle));
    }

    #[test]
    fn show_blob_string_reports_output_too_large_for_files_over_the_blob_cap() {
        let root = tmpdir("plumbing-blob-toolarge");
        init_repo(&root);
        // ~5 MiB file: over the 4 MiB blob cap. Must surface as a real
        // `GitError::OutputTooLarge`, not a misleading "does not exist".
        let content = vec![b'a'; 5 * 1024 * 1024];
        std::fs::write(root.join("huge.txt"), &content).unwrap();
        git(&root, &["add", "huge.txt"]);
        git(&root, &["commit", "-q", "-m", "add 5mib file"]);

        let runner = GitRunner::new(&root);
        let head = runner.rev_parse_head().unwrap();
        let err = runner
            .show_blob_string(&head, "huge.txt")
            .expect_err("a file over the blob cap must error, not silently succeed or truncate");
        assert!(
            matches!(err, GitError::OutputTooLarge { .. }),
            "expected OutputTooLarge, got {err:?}"
        );
    }
}
