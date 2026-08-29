//! Typed convenience methods consolidating every git subcommand used across
//! the three original wrappers (`pixel-index::gitsync`, `pixel-cli::rescue_cmd`,
//! `pixel-graph::changes`). Same flags, same semantics as the originals —
//! this module only unifies *where* the calls live, plus applies
//! `ref_guard::validate_ref` consistently everywhere a ref/commit-ish string
//! is interpolated (see the ref-injection gap audit in the crate-level docs
//! / final report).

use crate::error::GitError;
use crate::ref_guard::{end_of_options, validate_ref};
use crate::runner::GitRunner;

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

    /// Tracked files (repo-relative, NUL-safe). Empty outside a git repo.
    pub fn ls_files(&self) -> Vec<String> {
        let Some(out) = self.run_opt(&["ls-files", "-z"]) else {
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
    pub fn show_blob(&self, oid: &str, rel: &str) -> Option<Vec<u8>> {
        validate_ref(oid).ok()?;
        let spec = format!("{oid}:{rel}");
        self.run_opt(&["show", end_of_options(), &spec])
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
    pub fn diff_name_status(&self, from: &str, to: &str) -> Vec<(char, String)> {
        if validate_ref(from).is_err() || validate_ref(to).is_err() {
            return Vec::new();
        }
        let Some(out) = self.run_opt(&["diff", "--name-status", "--no-renames", "-z", from, to])
        else {
            return Vec::new();
        };
        let mut fields = out.split(|&b| b == 0).filter(|s| !s.is_empty());
        let mut result = Vec::new();
        while let Some(status) = fields.next() {
            let Some(path) = fields.next() else { break };
            let c = status.first().copied().unwrap_or(b'M') as char;
            result.push((c, String::from_utf8_lossy(path).into_owned()));
        }
        result
    }

    /// `git status --porcelain -z --untracked-files=all --no-renames` as
    /// (XY, path). Untracked files appear with XY `"??"`.
    pub fn status_porcelain(&self) -> Vec<(String, String)> {
        let Some(out) = self.run_opt(&[
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ]) else {
            return Vec::new();
        };
        out.split(|&b| b == 0)
            .filter(|s| s.len() > 3)
            .map(|entry| {
                let xy = String::from_utf8_lossy(&entry[0..2]).into_owned();
                let path = String::from_utf8_lossy(&entry[3..]).into_owned();
                (xy, path)
            })
            .collect()
    }

    /// `git diff --unified=0 [--end-of-options <base_ref>] -- .`, validating
    /// `base_ref` via `validate_ref` first when given (port of
    /// `pixel-graph::changes::detect`'s diff invocation).
    pub fn diff_unified0(&self, base_ref: Option<&str>) -> Result<Vec<u8>, GitError> {
        let mut args: Vec<&str> = vec!["diff", "--unified=0"];
        if let Some(r) = base_ref {
            validate_ref(r)?;
            args.push(end_of_options());
            args.push(r);
        }
        args.push("--");
        args.push(".");
        self.run(&args)
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
}
