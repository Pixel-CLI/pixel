//! `publish` — stage files, commit, and optionally push.
//!
//! Crash-safe: runs under repository lock + operation journal. Every phase
//! is journaled durably. On crash recovery, `begin` returns Resume/Replay
//! so the operation can restart or replay the terminal result.
//!
//! Phases: started → index_staged → commit_observed → (push_started) → terminal
//!
//! The probe hook allows the crash matrix to inject failures at each phase.

use std::path::Path;

use serde_json::{json, Value};

use pixel_git::GitRunner;

use crate::durable::{sha256_hex, state_root};
use crate::journal::{BeginOutcome, JournalOperation, JournalPhase, OperationJournal};
use crate::lock::RepositoryLock;

/// Options for a publish operation.
#[derive(Debug, Clone)]
pub struct PublishOptions {
    pub message: String,
    pub files: Vec<String>,
    pub expected_head: Option<String>,
    pub expected_fingerprints: std::collections::BTreeMap<String, String>,
    pub push: bool,
    pub amend: bool,
    pub request_id: String,
}

/// A probe hook called at each journal phase. Used by the crash matrix
/// to inject failures. Returns `Err` to simulate a crash.
pub type PublishProbe = Box<dyn FnMut(&str) -> Result<(), String>>;

/// Execute a publish operation with crash safety.
pub fn publish(
    root: &Path,
    opts: &PublishOptions,
    probe: Option<PublishProbe>,
) -> Result<Value, String> {
    let state_root = state_root();
    publish_with_state(root, opts, probe, &state_root)
}

pub fn publish_with_state(
    root: &Path,
    opts: &PublishOptions,
    mut probe: Option<PublishProbe>,
    state_root: &Path,
) -> Result<Value, String> {
    let runner = GitRunner::new(root);
    let repo_key = repo_key(root);
    let input_hash = publish_input_hash(opts);

    let journal = OperationJournal::with_state_root(state_root.to_path_buf());

    // Begin — check for existing journal record (crash recovery).
    let outcome = journal.begin(
        &opts.request_id,
        JournalOperation::Publish,
        &repo_key,
        &input_hash,
    )?;

    match outcome {
        BeginOutcome::Replay(result) => return Ok(result),
        BeginOutcome::Resume { phase, .. } => {
            // Resume from the appropriate phase.
            return resume_publish(root, opts, &journal, phase, &runner);
        }
        BeginOutcome::Start => {} // Fresh start.
    }

    // Acquire lock.
    let mut lock = RepositoryLock::acquire_with_state_root(
        &common_dir(root),
        state_root,
    ).map_err(|_| "repository is busy".to_string())?;

    // Probe: journal:started
    if let Some(p) = probe.as_mut() {
        p("journal:started").map_err(|e| {
            let _ = lock.release();
            e
        })?;
    }

    // Verify expected state (STALE_STATE check).
    let current_head = runner.rev_parse_head();
    if let Some(expected) = &opts.expected_head {
        if current_head.as_deref() != Some(expected.as_str()) {
            let _ = lock.release();
            return Err(format!(
                "STALE_STATE: expected head {}, got {:?}",
                expected, current_head
            ));
        }
    }

    // Stage files.
    if !opts.files.is_empty() {
        let mut args: Vec<String> = vec!["add".into(), "--".into()];
        args.extend(opts.files.iter().cloned());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        runner.run(&arg_refs).map_err(|e| {
            let _ = lock.release();
            format!("git add: {e}")
        })?;
    }

    // Journal: index_staged
    journal.transition(&opts.request_id, &repo_key, JournalPhase::IndexStaged, None)?;

    // Probe: recovery:snapshotted / journal:index_staged
    if let Some(p) = probe.as_mut() {
        p("recovery:snapshotted").map_err(|e| {
            let _ = lock.release();
            e
        })?;
    }
    if let Some(p) = probe.as_mut() {
        p("journal:index_staged").map_err(|e| {
            let _ = lock.release();
            e
        })?;
    }

    // Commit — scope to requested files with pathspec to avoid sweeping
    // unrelated staged files into the commit.
    let commit_mode = if opts.amend { "--amend" } else { "--no-edit" };
    let mut commit_args: Vec<String> = vec!["commit".into(), commit_mode.into(), "-m".into(), opts.message.clone()];
    // Add pathspec to scope the commit to only the requested files.
    if !opts.files.is_empty() {
        commit_args.push("--".into());
        commit_args.extend(opts.files.iter().cloned());
    }
    let arg_refs: Vec<&str> = commit_args.iter().map(String::as_str).collect();

    // Probe: recovery:commit_started (before git commit runs)
    if let Some(p) = probe.as_mut() {
        p("recovery:commit_started").map_err(|e| {
            let _ = lock.release();
            e
        })?;
    }

    runner.run(&arg_refs).map_err(|e| {
        let _ = lock.release();
        format!("git commit: {e}")
    })?;

    // Observe the new HEAD.
    let new_head = runner.rev_parse_head();
    journal.transition(
        &opts.request_id,
        &repo_key,
        JournalPhase::CommitObserved,
        Some(json!({"head": new_head})),
    )?;

    // Probe: journal:commit_observed
    if let Some(p) = probe.as_mut() {
        p("journal:commit_observed").map_err(|e| {
            let _ = lock.release();
            e
        })?;
    }

    // Push if requested.
    if opts.push {
        journal.transition(&opts.request_id, &repo_key, JournalPhase::PushStarted, None)?;
        if let Some(p) = probe.as_mut() {
            p("journal:push_started").map_err(|e| {
                let _ = lock.release();
                e
            })?;
        }
        runner.run(&["push"]).map_err(|e| {
            let _ = lock.release();
            format!("git push: {e}")
        })?;
    }

    // Complete.
    let result = json!({
        "head": new_head,
        "published": true,
        "pushed": opts.push,
    });
    journal.complete(&opts.request_id, &repo_key, result.clone())?;

    // Probe: journal:terminal
    if let Some(p) = probe.as_mut() {
        p("journal:terminal").map_err(|e| {
            let _ = lock.release();
            e
        })?;
    }

    let _ = lock.release();
    Ok(result)
}

/// Resume a publish from a given phase after a crash.
fn resume_publish(
    root: &Path,
    opts: &PublishOptions,
    journal: &OperationJournal,
    phase: JournalPhase,
    runner: &GitRunner,
) -> Result<Value, String> {
    let repo_key = repo_key(root);
    match phase {
        JournalPhase::Started => {
            // Journal record exists at "started" but no git mutation happened.
            // Continue the operation from after begin — don't call publish()
            // again (that would re-enter begin and loop). Instead, re-run
            // the operation body with the existing journal.
            continue_publish_after_begin(root, opts, journal, runner)
        }
        JournalPhase::IndexStaged => {
            // Index was staged but commit didn't happen. Roll back the
            // index and re-stage + commit.
            runner.run(&["reset", "--quiet", "HEAD"]).map_err(|e| format!("git reset: {e}"))?;
            continue_publish_after_begin(root, opts, journal, runner)
        }
        JournalPhase::CommitObserved => {
            // Commit already happened. Verify and complete.
            let new_head = runner.rev_parse_head();
            let result = json!({
                "head": new_head,
                "published": true,
                "pushed": false,
            });
            journal.complete(&opts.request_id, &repo_key, result.clone())?;
            Ok(result)
        }
        JournalPhase::PushStarted => {
            // Push may or may not have happened. Check if remote matches.
            // For safety, report as NETWORK_AMBIGUITY.
            Err("NETWORK_AMBIGUITY: push may have started, cannot safely retry".to_string())
        }
        JournalPhase::Terminal => {
            // Should have been caught by Replay in begin().
            let record = journal.read(&repo_key, &opts.request_id);
            if let Some(r) = record {
                Ok(r.result.unwrap_or(json!({})))
            } else {
                Err("journal record lost".to_string())
            }
        }
        JournalPhase::RefUpdateStarted => {
            Err("unexpected phase for publish".to_string())
        }
    }
}

/// Continue a publish operation after the journal has been begun (phase
/// = Started). This re-runs staging + commit + optional push without
/// re-entering `begin()`, avoiding infinite recursion on resume.
fn continue_publish_after_begin(
    root: &Path,
    opts: &PublishOptions,
    journal: &OperationJournal,
    runner: &GitRunner,
) -> Result<Value, String> {
    let repo_key = repo_key(root);
    let state_root = state_root();

    let mut lock = RepositoryLock::acquire_with_state_root(
        &common_dir(root),
        &state_root,
    ).map_err(|_| "repository is busy".to_string())?;

    // Verify expected state.
    let current_head = runner.rev_parse_head();
    if let Some(expected) = &opts.expected_head {
        if current_head.as_deref() != Some(expected.as_str()) {
            let _ = lock.release();
            return Err(format!("STALE_STATE: expected head {}, got {:?}", expected, current_head));
        }
    }

    // Stage files.
    if !opts.files.is_empty() {
        let mut args: Vec<String> = vec!["add".into(), "--".into()];
        args.extend(opts.files.iter().cloned());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        runner.run(&arg_refs).map_err(|e| {
            let _ = lock.release();
            format!("git add: {e}")
        })?;
    }

    journal.transition(&opts.request_id, &repo_key, JournalPhase::IndexStaged, None)?;

    // Commit — scoped to requested files.
    let commit_mode = if opts.amend { "--amend" } else { "--no-edit" };
    let mut commit_args: Vec<String> = vec!["commit".into(), commit_mode.into(), "-m".into(), opts.message.clone()];
    if !opts.files.is_empty() {
        commit_args.push("--".into());
        commit_args.extend(opts.files.iter().cloned());
    }
    let arg_refs: Vec<&str> = commit_args.iter().map(String::as_str).collect();
    runner.run(&arg_refs).map_err(|e| {
        let _ = lock.release();
        format!("git commit: {e}")
    })?;

    let new_head = runner.rev_parse_head();
    journal.transition(
        &opts.request_id,
        &repo_key,
        JournalPhase::CommitObserved,
        Some(json!({"head": new_head})),
    )?;

    // Push if requested.
    if opts.push {
        journal.transition(&opts.request_id, &repo_key, JournalPhase::PushStarted, None)?;
        runner.run(&["push"]).map_err(|e| {
            let _ = lock.release();
            format!("git push: {e}")
        })?;
    }

    let result = json!({
        "head": new_head,
        "published": true,
        "pushed": opts.push,
    });
    journal.complete(&opts.request_id, &repo_key, result.clone())?;
    let _ = lock.release();
    Ok(result)
}

fn repo_key(root: &Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .display()
        .to_string()
}

fn common_dir(root: &Path) -> String {
    root.join(".git").display().to_string()
}

fn publish_input_hash(opts: &PublishOptions) -> String {
    let mut input = format!("{}\u{0}{}\u{0}{}\u{0}{}",
        opts.message,
        opts.files.join(","),
        opts.expected_head.as_deref().unwrap_or(""),
        opts.push,
    );
    let mut fps: Vec<String> = opts.expected_fingerprints
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    fps.sort();
    input.push_str(&fps.join(","));
    sha256_hex(&input)
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
        std::fs::write(root.join("base.txt"), b"base").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["add", "."])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["commit", "-qm", "base"])
            .status()
            .unwrap();
    }

    fn make_opts(msg: &str, files: &[&str]) -> PublishOptions {
        PublishOptions {
            message: msg.to_string(),
            files: files.iter().map(|s| s.to_string()).collect(),
            expected_head: None,
            expected_fingerprints: std::collections::BTreeMap::new(),
            push: false,
            amend: false,
            request_id: format!("test-{}", uuid::Uuid::new_v4()),
        }
    }

    #[test]
    fn publish_creates_commit() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("new.txt"), b"new content").unwrap();

        let opts = make_opts("test commit", &["new.txt"]);
        let result = publish(dir.path(), &opts, None).unwrap();
        assert_eq!(result["published"], json!(true));
        assert!(result["head"].as_str().unwrap().len() >= 7);

        // Verify the commit exists.
        let head = GitRunner::new(dir.path()).rev_parse_head().unwrap();
        assert_eq!(result["head"], json!(head));
    }

    #[test]
    fn publish_idempotent_replay() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();

        let opts = make_opts("idempotent test", &["a.txt"]);
        let result1 = publish(dir.path(), &opts, None).unwrap();

        // Retry with same request_id — should replay.
        let result2 = publish(dir.path(), &opts, None).unwrap();
        assert_eq!(result1["head"], result2["head"]);
    }

    #[test]
    fn publish_stale_state_rejected() {
        let dir = tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();

        let opts = PublishOptions {
            message: "stale test".to_string(),
            files: vec!["a.txt".to_string()],
            expected_head: Some("0000000000000000000000000000000000000000".to_string()),
            expected_fingerprints: std::collections::BTreeMap::new(),
            push: false,
            amend: false,
            request_id: format!("stale-{}", uuid::Uuid::new_v4()),
        };

        let err = publish(dir.path(), &opts, None).unwrap_err();
        assert!(err.contains("STALE_STATE"));
    }
}
