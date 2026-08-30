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
use crate::runner::{BLOB_MAX_OUTPUT_BYTES, ENUMERATION_MAX_OUTPUT_BYTES, GitRunner};

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
        let Some(out) = self
            .with_max_output_bytes(Some(ENUMERATION_MAX_OUTPUT_BYTES))
            .run_opt(&["ls-files", "-z"])
        else {
            return Vec::new();
        };
        out.split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
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
    pub fn diff_name_status_or_err(&self, from: &str, to: &str) -> Result<Vec<(char, String)>, GitError> {
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
    /// `pixel rescue --apply` conclude "nothing is dirty" (and overwrite an
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
    pub fn log_follow(&self, path: &str, depth: usize) -> Result<Vec<(String, i64, String)>, GitError> {
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
            rows.push((oid.to_string(), ct.parse().unwrap_or(0), subject.to_string()));
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
        self.run(&["rev-parse", "--verify", "-q", &spec]).map(|_| ())
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
    /// Returns the exit status: 0 = clean merge, positive = conflict count
    /// (markers left in `current`), negative = real failure. Port of
    /// `pixel-cli::rescue_cmd::apply`'s 3-way merge branch, including the
    /// cosmetic `-L` diff3 labels.
    pub fn merge_file_with_labels(
        &self,
        current: &Path,
        base: &Path,
        other: &Path,
        label_ours: &str,
        label_base: &str,
        label_theirs: &str,
    ) -> Result<std::process::ExitStatus, GitError> {
        std::process::Command::new("git")
            .arg("-C")
            .arg(self.root())
            .arg("merge-file")
            .arg("-L")
            .arg(label_ours)
            .arg("-L")
            .arg(label_base)
            .arg("-L")
            .arg(label_theirs)
            .arg(current)
            .arg(base)
            .arg(other)
            .status()
            .map_err(GitError::from)
    }
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
        let content = runner.show_blob_string(&head, "c.txt").expect("blob string");
        assert_eq!(content, "content here");
        // Flag injection rejected.
        assert!(runner.show_blob_string("--output=/tmp/evil", "c.txt").is_err());
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
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "b-dirty");
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
        let root = tmpdir("plumbing-lsfiles-big");
        init_repo(&root);
        let dir = long_component("tracked");
        std::fs::create_dir_all(root.join(&dir)).unwrap();
        const N: usize = 5300; // ~5300 * ~250 bytes ≈ 1.3 MiB of `ls-files -z` output
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
        let root = tmpdir("plumbing-status-big");
        init_repo(&root);
        std::fs::write(root.join("tracked.txt"), b"hello").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-q", "-m", "seed"]);

        let dir = long_component("untracked");
        std::fs::create_dir_all(root.join(&dir)).unwrap();
        const N: usize = 5300; // pushes `status --porcelain -z` past the old 1 MiB cap
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
        let root = tmpdir("plumbing-diffns-big");
        init_repo(&root);
        std::fs::write(root.join("seed.txt"), b"seed").unwrap();
        git(&root, &["add", "seed.txt"]);
        git(&root, &["commit", "-q", "-m", "seed"]);
        let from = GitRunner::new(&root).rev_parse_head().unwrap();

        let dir = long_component("added");
        std::fs::create_dir_all(root.join(&dir)).unwrap();
        const N: usize = 5300;
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
