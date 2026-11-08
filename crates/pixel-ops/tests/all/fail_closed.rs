// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! HO-05 / HO-06: a durable record that cannot be read is not "no record".
//!
//! A truncated recovery record used to read back as `None`, which
//! `publish`'s resume takes as "the crash happened before anything durable
//! was written" and re-runs `git add`/`git commit` over a mutation the
//! record cannot prove never applied. The journal had the same shape: a
//! record the initial read could not open fell through to creation, and
//! `write_new_durably` reporting a lost race (`Ok(false)`) was ignored, so
//! both racers reported `Start` and ran the mutation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use pixel_ops::durable::sha256_hex;
use pixel_ops::journal::{BeginOutcome, JournalOperation, JournalPhase, OperationJournal};
use pixel_ops::publish::{PublishOptions, PublishProbe, publish_with_state};
use pixel_ops::repo_identity;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git -C {} {args:?} failed: {}",
        root.display(),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn init_repo(root: &Path) {
    let out = Command::new("git")
        .args(["init", "-q"])
        .arg(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "git init {root:?}");
    git(root, &["config", "user.email", "fail-closed@example.com"]);
    git(root, &["config", "user.name", "Fail Closed"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("base.txt"), "base\n").unwrap();
    git(root, &["add", "--", "base.txt"]);
    git(root, &["commit", "-qm", "base"]);
}

fn publish_options(request_id: &str) -> PublishOptions {
    PublishOptions {
        message: "fail closed".to_string(),
        files: vec!["new.txt".to_string()],
        expected_head: None,
        expected_fingerprints: BTreeMap::new(),
        push: false,
        amend: false,
        request_id: request_id.to_string(),
    }
}

/// Where `PublishRecoveryStore` looks for a request's record.
fn recovery_record_path(state_root: &Path, repo_key: &str, request_id: &str) -> PathBuf {
    state_root
        .join("publish-recovery")
        .join(sha256_hex(repo_key))
        .join(format!("{}.json", sha256_hex(request_id)))
}

/// Where `OperationJournal` looks for a request's record.
fn journal_record_path(state_root: &Path, repo_key: &str, request_id: &str) -> PathBuf {
    state_root
        .join("journals")
        .join(sha256_hex(repo_key))
        .join(format!("{}.json", sha256_hex(request_id)))
}

/// A recovery record truncated mid-write (the snapshot embeds the whole
/// index in hex: a full disk truncates it) is not "no crash". The retry
/// must fail with `GIT_FAILED` and leave HEAD where it was — the old
/// behavior committed a second time.
#[test]
fn a_truncated_recovery_record_fails_the_retry_and_leaves_head_unchanged() {
    let repo_dir = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();
    let root = repo_dir.path();
    init_repo(root);
    std::fs::write(root.join("new.txt"), "new\n").unwrap();

    let opts = publish_options("truncated-record");
    // Crash at `journal:started`: the journal record exists at phase
    // `started` and no recovery record has been written yet.
    let probe: PublishProbe = Box::new(|phase: &str| {
        if phase == "journal:started" {
            Err("CRASH@journal:started".to_string())
        } else {
            Ok(())
        }
    });
    let err = publish_with_state(root, &opts, Some(probe), state_dir.path())
        .expect_err("the injected crash must fail the first publish");
    assert!(err.contains("CRASH@"), "{err}");

    let record = recovery_record_path(state_dir.path(), &repo_identity(root), &opts.request_id);
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(
        &record,
        br#"{"schema_version": 1, "request_id": "truncated-record", "repo_key": "/repo/.git", "phase": "commit_started", "pre_head": "ab"#,
    )
    .unwrap();

    let head_before = git(root, &["rev-parse", "HEAD"]);

    let err = publish_with_state(root, &opts, None, state_dir.path())
        .expect_err("an unreadable recovery record must fail the retry");

    assert!(
        err.contains("GIT_FAILED"),
        "the retry must fail closed with GIT_FAILED, got: {err}",
    );
    assert!(
        err.contains("recovery record"),
        "the error must name the recovery record, got: {err}",
    );
    assert_eq!(
        git(root, &["rev-parse", "HEAD"]),
        head_before,
        "HEAD must not move: the record cannot prove the mutation never applied",
    );
    assert_eq!(
        git(root, &["rev-list", "--count", "HEAD"]).trim(),
        "1",
        "no second commit may be created",
    );
}

/// A journal record that exists but cannot be read is an error, never a
/// fresh start: `Start` there re-runs the mutation behind the record.
#[test]
fn begin_never_starts_over_a_journal_record_it_cannot_read() {
    let state_dir = TempDir::new().unwrap();
    let repo_key = "/repo/.git";
    let request_id = "unreadable-record";
    let record = journal_record_path(state_dir.path(), repo_key, request_id);
    // A directory where the record belongs: `read` fails with EISDIR,
    // which is not `NotFound`.
    std::fs::create_dir_all(&record).unwrap();

    let journal = OperationJournal::with_state_root(state_dir.path().to_path_buf());
    let err = journal
        .begin(request_id, JournalOperation::Publish, repo_key, "hash")
        .expect_err("an unreadable record must not read as a fresh start");
    assert!(err.contains("GIT_FAILED"), "{err}");
}

/// A second caller with the same request id resumes (or replays) the
/// record the first one wrote — it never starts the operation over.
#[test]
fn begin_on_an_existing_record_resumes_or_replays_it() {
    let state_dir = TempDir::new().unwrap();
    let journal = OperationJournal::with_state_root(state_dir.path().to_path_buf());
    let repo_key = "/repo/.git";

    assert!(matches!(
        journal
            .begin("req-1", JournalOperation::Publish, repo_key, "hash-1")
            .unwrap(),
        BeginOutcome::Start,
    ));
    journal
        .transition("req-1", repo_key, JournalPhase::IndexStaged, None)
        .unwrap();
    match journal
        .begin("req-1", JournalOperation::Publish, repo_key, "hash-1")
        .unwrap()
    {
        BeginOutcome::Resume { phase, result } => {
            assert_eq!(phase, JournalPhase::IndexStaged);
            assert!(result.is_none());
        }
        other => panic!("expected Resume, got {other:?}"),
    }

    let result = serde_json::json!({"head": "abc"});
    journal.complete("req-1", repo_key, result.clone()).unwrap();
    match journal
        .begin("req-1", JournalOperation::Publish, repo_key, "hash-1")
        .unwrap()
    {
        BeginOutcome::Replay(replayed) => assert_eq!(replayed, result),
        other => panic!("expected Replay, got {other:?}"),
    }
}

#[test]
fn duplicate_publish_should_leave_a_live_owners_index_and_journal_untouched() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    init_repo(root.path());
    std::fs::write(root.path().join("new.txt"), "new\n").unwrap();
    let opts = publish_options("live-duplicate");
    let retry_root = root.path().to_path_buf();
    let retry_state = state.path().to_path_buf();
    let retry_opts = opts.clone();
    let probe = Box::new(move |phase: &str| {
        if phase == "journal:index_staged" || phase == "journal:commit_observed" {
            let index = std::fs::read(retry_root.join(".git/index")).unwrap();
            let record = std::fs::read(journal_record_path(
                &retry_state,
                &repo_identity(&retry_root),
                &retry_opts.request_id,
            ))
            .unwrap();
            let duplicate = publish_with_state(&retry_root, &retry_opts, None, &retry_state);
            assert_eq!(
                std::fs::read(retry_root.join(".git/index")).unwrap(),
                index,
                "a duplicate request must not roll back the live owner's staged index"
            );
            assert_eq!(
                std::fs::read(journal_record_path(
                    &retry_state,
                    &repo_identity(&retry_root),
                    &retry_opts.request_id,
                ))
                .unwrap(),
                record,
                "a duplicate must not advance the live journal"
            );
            assert_eq!(duplicate.unwrap_err(), "repository is busy");
        }
        Ok(())
    });
    let result = publish_with_state(root.path(), &opts, Some(probe), state.path()).unwrap();
    assert_eq!(
        result["head"],
        git(root.path(), &["rev-parse", "HEAD"]).trim()
    );
    assert_eq!(git(root.path(), &["show", "HEAD:new.txt"]), "new\n");
    assert_eq!(
        publish_with_state(root.path(), &opts, None, state.path()).unwrap(),
        result
    );
}

#[test]
fn resumed_push_should_contend_on_the_supplied_state_root() {
    use pixel_ops::RepositoryLock;
    use pixel_ops::push::{PushOptions, push_with_state};
    let root = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    init_repo(root.path());
    git(remote.path(), &["init", "--bare", "-q"]);
    git(
        root.path(),
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
    );
    let opts = PushOptions {
        remote: "origin".into(),
        refspec: "HEAD:refs/heads/result".into(),
        request_id: "push-retry-owner".into(),
        force_with_lease: false,
    };
    let error = push_with_state(
        root.path(),
        &opts,
        Some(Box::new(|phase| {
            if phase == "journal:started" {
                Err("interrupted".into())
            } else {
                Ok(())
            }
        })),
        state.path(),
    )
    .unwrap_err();
    assert_eq!(error, "interrupted");
    let lock =
        RepositoryLock::acquire_with_state_root(&repo_identity(root.path()), state.path()).unwrap();
    let retry = push_with_state(root.path(), &opts, None, state.path());
    assert_eq!(
        git(remote.path(), &["for-each-ref", "--format=%(refname)"]),
        "",
        "a busy retry must not push under a different lock namespace"
    );
    assert_eq!(retry.unwrap_err(), "repository is busy");
    drop(lock);
    assert_eq!(
        push_with_state(root.path(), &opts, None, state.path()).unwrap()["pushed"],
        true
    );
}
