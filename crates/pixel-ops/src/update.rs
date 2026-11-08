// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `update` — fast-forward merge with expectedHead + targetOid.
//!
//! Returns NON_FAST_FORWARD + merge base on divergence.
//! Refuses if incoming changes intersect dirty paths.

use std::path::Path;

use serde_json::{Value, json};

use pixel_git::GitRunner;

use crate::durable::{sha256_hex, state_root};
use crate::journal::{BeginOutcome, JournalOperation, JournalPhase, OperationJournal};
use crate::lock::RepositoryLock;
use crate::repo::repo_identity;

#[derive(Debug, Clone)]
pub struct UpdateOptions {
    pub expected_head: String,
    pub target_oid: String,
    pub request_id: String,
}

pub fn update(root: &Path, opts: &UpdateOptions) -> Result<Value, String> {
    update_with_state(root, opts, &state_root())
}

/// `update` with an explicit state root for the journal and the lock.
pub fn update_with_state(
    root: &Path,
    opts: &UpdateOptions,
    state_root: &Path,
) -> Result<Value, String> {
    update_with_probe(root, opts, state_root, || Ok(()))
}

fn update_with_probe(
    root: &Path,
    opts: &UpdateOptions,
    state_root: &Path,
    mut applied: impl FnMut() -> Result<(), String>,
) -> Result<Value, String> {
    let runner = GitRunner::new(root);
    let repo_key = repo_identity(root);
    let input_hash = sha256_hex(&format!("{}\u{0}{}", opts.expected_head, opts.target_oid));

    let journal = OperationJournal::with_state_root(state_root.to_path_buf());

    let mut lock = RepositoryLock::acquire_with_state_root(&repo_key, state_root)
        .map_err(|_| "repository is busy".to_string())?;

    let outcome = journal.begin(
        &opts.request_id,
        JournalOperation::Update,
        &repo_key,
        &input_hash,
    )?;
    if let BeginOutcome::Replay(result) = &outcome {
        return Ok(result.clone());
    }

    // Validate refs.
    pixel_git::validate_ref(&opts.target_oid).map_err(|e| e.to_string())?;

    // Check current HEAD matches expected.
    let current_head = runner.rev_parse_head().ok_or("no HEAD")?;
    let worktree = root.canonicalize().map_err(|e| e.to_string())?;
    let result = json!({"updated": true, "from": opts.expected_head,
        "to": opts.target_oid, "fast_forwarded": true});
    let target = if let BeginOutcome::Resume {
        phase: JournalPhase::RefUpdateStarted,
        result: Some(meta),
    } = &outcome
    {
        if meta.get("worktree") != Some(&json!(worktree))
            || meta.get("branch") != Some(&json!(runner.current_branch()))
        {
            return Err(
                "STALE_STATE: fast-forward intent belongs to another branch/worktree".into(),
            );
        }
        let target = meta
            .get("target")
            .and_then(Value::as_str)
            .ok_or("STALE_STATE: fast-forward intent target missing")?;
        if current_head == target {
            journal.complete(&opts.request_id, &repo_key, result.clone())?;
            return Ok(result);
        }
        target.to_owned()
    } else {
        let target = runner
            .run(&[
                "rev-parse",
                "--verify",
                &format!("{}^{{commit}}", opts.target_oid),
            ])
            .map_err(|e| format!("git resolve fast-forward target: {e}"))?;
        String::from_utf8_lossy(&target).trim().to_owned()
    };
    if current_head != opts.expected_head {
        lock.release();
        return Err(format!(
            "STALE_STATE: expected {}, got {}",
            opts.expected_head, current_head
        ));
    }

    // Check if target is a descendant of HEAD (fast-forward possible).
    let merge_base = runner
        .run_opt(&["merge-base", &opts.expected_head, &target])
        .unwrap_or_default();
    let merge_base = String::from_utf8_lossy(&merge_base).trim().to_string();

    if merge_base == opts.expected_head {
        // Fast-forward is possible.
        // Check for dirty paths that would be overwritten. Uses
        // `status_porcelain_or_err`, NOT `status_porcelain`: an
        // undetermined status (git error, timeout, or output-cap overflow)
        // must abort the fast-forward, never be silently read as "nothing
        // is dirty" — that would let `git merge --ff-only` overwrite a
        // genuinely dirty file with no way for the caller to have known.
        let dirty = runner.status_porcelain_or_err().map_err(|e| {
            lock.release();
            format!(
                "could not determine working-tree status, refusing to fast-forward \
                 (would otherwise risk overwriting dirty files as if the tree were clean): {e}"
            )
        })?;
        if !dirty.is_empty() {
            // Check if the ff would touch any dirty files. Same fail-closed
            // reasoning: an undetermined changed-path set must not be read
            // as "nothing changed".
            let changes = runner
                .diff_name_status_or_err(&opts.expected_head, &target)
                .map_err(|e| {
                    lock.release();
                    format!(
                        "could not determine which paths the fast-forward would change, \
                         refusing to proceed while dirty files are present: {e}"
                    )
                })?;
            let changed_paths: std::collections::HashSet<String> =
                changes.iter().map(|(_, p)| p.clone()).collect();
            let dirty_intersect: Vec<String> = dirty
                .iter()
                .filter(|(_, p)| changed_paths.contains(p))
                .map(|(_, p)| p.clone())
                .collect();
            if !dirty_intersect.is_empty() {
                lock.release();
                return Err(format!(
                    "UNSUPPORTED_STATE: dirty files would be overwritten: {}",
                    dirty_intersect.join(", ")
                ));
            }
        }

        // Persist intent before Git moves the ref. A retry may acknowledge
        // this exact target, but cannot adopt a different HEAD or worktree.
        journal.transition(
            &opts.request_id,
            &repo_key,
            JournalPhase::RefUpdateStarted,
            Some(
                json!({"worktree": worktree, "branch": runner.current_branch(), "target": target}),
            ),
        )?;
        // Perform the fast-forward.
        runner.run(&["merge", "--ff-only", &target]).map_err(|e| {
            lock.release();
            format!("git merge --ff-only: {e}")
        })?;

        applied()?;

        journal.complete(&opts.request_id, &repo_key, result.clone())?;
        lock.release();
        Ok(result)
    } else {
        // Diverged — NON_FAST_FORWARD.
        lock.release();
        Err(format!(
            "NON_FAST_FORWARD: merge-base is {}, expected {} to be an ancestor of {}",
            merge_base, opts.expected_head, opts.target_oid
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_repo(root: &Path) {
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(root)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", "user.email", "t@t"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", "user.name", "t"])
            .status()
            .unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["add", "."])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["commit", "-qm", "init"])
            .status()
            .unwrap();
    }

    #[test]
    fn update_should_refuse_a_fast_forward_over_a_dirty_changed_file() {
        let dir = tempdir().unwrap();
        let state = tempdir().unwrap();
        init_repo(dir.path());
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(dir.path())
                    .args([
                        "-c",
                        "core.hooksPath=/dev/null",
                        "-c",
                        "commit.gpgsign=false"
                    ])
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        let head = GitRunner::new(dir.path()).rev_parse_head().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"upstream").unwrap();
        git(&["commit", "-qam", "change a"]);
        let target = GitRunner::new(dir.path()).rev_parse_head().unwrap();
        git(&["reset", "-q", "--hard", &head]);
        std::fs::write(dir.path().join("a.txt"), b"local edit").unwrap();

        let opts = UpdateOptions {
            expected_head: head,
            target_oid: target,
            request_id: "upd-dirty".to_string(),
        };
        let err = update_with_state(dir.path(), &opts, state.path()).unwrap_err();
        assert!(err.starts_with("UNSUPPORTED_STATE: dirty files"), "{err}");
        // The journal lives under the supplied state root, not the default one.
        let journal = OperationJournal::with_state_root(state.path().to_path_buf());
        assert!(
            journal
                .read(&repo_identity(dir.path()), "upd-dirty")
                .is_some(),
            "no journal record under {}",
            state.path().display()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "local edit"
        );
    }

    #[test]
    fn update_fast_forwards() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        let head = GitRunner::new(dir.path()).rev_parse_head().unwrap();
        // Make a new commit.
        std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", "."])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit", "-qm", "new"])
            .status()
            .unwrap();
        let target = GitRunner::new(dir.path()).rev_parse_head().unwrap();
        // Reset back to old head.
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["reset", "--hard", &head])
            .status()
            .unwrap();

        let opts = UpdateOptions {
            expected_head: head,
            target_oid: target,
            request_id: format!("upd-{}", uuid::Uuid::new_v4()),
        };
        let result = update(dir.path(), &opts).unwrap();
        assert_eq!(result["fast_forwarded"], json!(true));
    }

    #[test]
    fn update_rejects_non_ff() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        let head = GitRunner::new(dir.path()).rev_parse_head().unwrap();
        // Diverge: make a commit on the current branch.
        std::fs::write(dir.path().join("c.txt"), b"c").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", "."])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit", "-qm", "c"])
            .status()
            .unwrap();
        let diverged = GitRunner::new(dir.path()).rev_parse_head().unwrap();
        // Reset to head and make a different commit.
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["reset", "--hard", &head])
            .status()
            .unwrap();
        std::fs::write(dir.path().join("d.txt"), b"d").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", "."])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit", "-qm", "d"])
            .status()
            .unwrap();

        let opts = UpdateOptions {
            expected_head: GitRunner::new(dir.path()).rev_parse_head().unwrap(),
            target_oid: diverged,
            request_id: format!("upd-{}", uuid::Uuid::new_v4()),
        };
        let err = update(dir.path(), &opts).unwrap_err();
        assert!(err.contains("NON_FAST_FORWARD"));
    }
    #[test]
    fn update_should_replay_applied_target_but_refuse_unrelated_head_drift() {
        for drift in ["none", "abbreviated", "moving-ref", "commit", "branch"] {
            let root = tempdir().unwrap();
            let state = tempdir().unwrap();
            init_repo(root.path());
            let runner = GitRunner::new(root.path());
            let expected_head = runner.rev_parse_head().unwrap();
            std::fs::write(root.path().join("b.txt"), "target").unwrap();
            runner.run(&["add", "b.txt"]).unwrap();
            runner.run(&["commit", "-qm", "target"]).unwrap();
            let target_oid = runner.rev_parse_head().unwrap();
            runner.run(&["reset", "--hard", &expected_head]).unwrap();
            runner.run(&["branch", "target", &target_oid]).unwrap();
            let opts = UpdateOptions {
                expected_head,
                target_oid: match drift {
                    "abbreviated" => target_oid[..12].to_owned(),
                    "moving-ref" => "target".into(),
                    _ => target_oid.clone(),
                },
                request_id: "applied".into(),
            };
            assert_eq!(
                update_with_probe(root.path(), &opts, state.path(), || Err(
                    "interrupted".into()
                ))
                .unwrap_err(),
                "interrupted"
            );
            if drift == "moving-ref" {
                runner
                    .run(&["update-ref", "refs/heads/target", &opts.expected_head])
                    .unwrap();
            }
            if drift == "commit" {
                runner
                    .run(&["commit", "--allow-empty", "-qm", "later"])
                    .unwrap();
            }
            if drift == "branch" {
                runner.run(&["checkout", "-qb", "other"]).unwrap();
            }
            let before = runner.rev_parse_head();
            let result = update_with_probe(root.path(), &opts, state.path(), || {
                panic!("must not merge twice")
            });
            assert_eq!(runner.rev_parse_head(), before);
            if drift == "commit" || drift == "branch" {
                assert!(result.unwrap_err().starts_with("STALE_STATE:"));
            } else {
                let result = result.unwrap();
                assert_eq!(
                    runner.rev_parse_head().as_deref(),
                    Some(target_oid.as_str())
                );
                assert_eq!(result["from"], opts.expected_head);
                assert_eq!(result["to"], opts.target_oid);
                assert_eq!(result["fast_forwarded"], true);
                assert_eq!(
                    update_with_state(root.path(), &opts, state.path()).unwrap(),
                    result
                );
            }
        }
    }
}
