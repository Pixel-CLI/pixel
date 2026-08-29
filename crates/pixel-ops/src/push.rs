//! `push` — leased push with crash-safe journaling.
//!
//! Phases: started → push_started → terminal
//! Crash at push_started → NETWORK_AMBIGUITY (push may have succeeded).
//! Crash at remote:returned → success (push already happened).

use std::path::Path;

use serde_json::{json, Value};

use pixel_git::GitRunner;

use crate::durable::{sha256_hex, state_root};
use crate::journal::{BeginOutcome, JournalOperation, JournalPhase, OperationJournal};
use crate::lock::RepositoryLock;

#[derive(Debug, Clone)]
pub struct PushOptions {
    pub remote: String,
    pub refspec: String,
    pub request_id: String,
    pub force_with_lease: bool,
}

pub type PushProbe = Box<dyn FnMut(&str) -> Result<(), String>>;

pub fn push(root: &Path, opts: &PushOptions, probe: Option<PushProbe>) -> Result<Value, String> {
    let state_root = state_root();
    push_with_state(root, opts, probe, &state_root)
}

pub fn push_with_state(
    root: &Path,
    opts: &PushOptions,
    mut probe: Option<PushProbe>,
    state_root: &Path,
) -> Result<Value, String> {
    let runner = GitRunner::new(root);
    let repo_key = repo_key(root);
    let input_hash = push_input_hash(opts);

    let journal = OperationJournal::with_state_root(state_root.to_path_buf());

    let outcome = journal.begin(
        &opts.request_id,
        JournalOperation::Push,
        &repo_key,
        &input_hash,
    )?;

    match outcome {
        BeginOutcome::Replay(result) => return Ok(result),
        BeginOutcome::Resume { phase, .. } => {
            return resume_push(root, opts, &journal, phase, &runner);
        }
        BeginOutcome::Start => {}
    }

    let mut lock = RepositoryLock::acquire_with_state_root(
        &common_dir(root),
        state_root,
    ).map_err(|_| "repository is busy".to_string())?;

    // Probe: journal:started
    if let Some(p) = probe.as_mut() {
        p("journal:started").map_err(|e| { let _ = lock.release(); e })?;
    }

    // Validate remote ref.
    pixel_git::validate_ref(&opts.remote).map_err(|e| { let _ = lock.release(); e.to_string() })?;
    pixel_git::validate_ref(&opts.refspec).map_err(|e| { let _ = lock.release(); e.to_string() })?;

    // Get source OID before push (for lease verification).
    let source_oid = runner.rev_parse_head().ok_or("no HEAD")?;

    // Journal: push_started
    journal.transition(
        &opts.request_id,
        &repo_key,
        JournalPhase::PushStarted,
        Some(json!({"source_oid": source_oid})),
    )?;

    // Probe: journal:push_started
    if let Some(p) = probe.as_mut() {
        p("journal:push_started").map_err(|e| { let _ = lock.release(); e })?;
    }

    // Build push args.
    let mut args: Vec<String> = vec!["push".into()];
    if opts.force_with_lease {
        args.push("--force-with-lease".into());
    }
    args.push("--end-of-options".into());
    args.push(opts.remote.clone());
    args.push(opts.refspec.clone());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    runner.run(&arg_refs).map_err(|e| {
        let _ = lock.release();
        format!("git push: {e}")
    })?;

    // Probe: remote:returned
    if let Some(p) = probe.as_mut() {
        p("remote:returned").map_err(|e| { let _ = lock.release(); e })?;
    }

    let result = json!({
        "pushed": true,
        "source_oid": source_oid,
        "remote": opts.remote,
        "refspec": opts.refspec,
    });
    journal.complete(&opts.request_id, &repo_key, result.clone())?;

    // Probe: journal:terminal
    if let Some(p) = probe.as_mut() {
        p("journal:terminal").map_err(|e| { let _ = lock.release(); e })?;
    }

    let _ = lock.release();
    Ok(result)
}

fn resume_push(
    root: &Path,
    opts: &PushOptions,
    journal: &OperationJournal,
    phase: JournalPhase,
    runner: &GitRunner,
) -> Result<Value, String> {
    let repo_key = repo_key(root);
    match phase {
        JournalPhase::Started => {
            // Journal at "started" — no git mutation happened. Continue
            // the operation without re-entering begin().
            continue_push_after_begin(root, opts, journal, runner)
        }
        JournalPhase::PushStarted => {
            // Push may have started — check if remote already has the commit.
            // If remote matches source_oid, push succeeded.
            let record = journal.read(&repo_key, &opts.request_id);
            if let Some(r) = record {
                if let Some(source_oid) = r.result
                    .as_ref()
                    .and_then(|v| v.get("source_oid"))
                    .and_then(|v| v.as_str())
                {
                    // Check if remote-tracking ref matches source_oid.
                    let remote_ref_name = format!("{}/{}", opts.remote, opts.refspec);
                    let check = runner.run_opt(&[
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        &remote_ref_name,
                    ]);
                    if let Some(out) = check {
                        let remote_ref = String::from_utf8_lossy(&out).trim().to_string();
                        if remote_ref == source_oid {
                            // Push already succeeded.
                            let result = json!({
                                "pushed": true,
                                "source_oid": source_oid,
                                "remote": opts.remote,
                                "refspec": opts.refspec,
                            });
                            journal.complete(&opts.request_id, &repo_key, result.clone())?;
                            return Ok(result);
                        }
                    }
                }
            }
            Err("NETWORK_AMBIGUITY: push may have started, cannot safely retry".to_string())
        }
        JournalPhase::Terminal => {
            let record = journal.read(&repo_key, &opts.request_id);
            Ok(record.and_then(|r| r.result).unwrap_or(json!({})))
        }
        _ => Err("unexpected phase for push".to_string()),
    }
}

/// Continue a push operation after the journal has been begun (phase =
/// Started). Re-runs the push without re-entering begin().
fn continue_push_after_begin(
    root: &Path,
    opts: &PushOptions,
    journal: &OperationJournal,
    runner: &GitRunner,
) -> Result<Value, String> {
    let repo_key = repo_key(root);
    let state_root = state_root();

    let mut lock = RepositoryLock::acquire_with_state_root(
        &common_dir(root),
        &state_root,
    ).map_err(|_| "repository is busy".to_string())?;

    pixel_git::validate_ref(&opts.remote).map_err(|e| { let _ = lock.release(); e.to_string() })?;
    pixel_git::validate_ref(&opts.refspec).map_err(|e| { let _ = lock.release(); e.to_string() })?;

    let source_oid = runner.rev_parse_head().ok_or("no HEAD")?;

    journal.transition(
        &opts.request_id,
        &repo_key,
        JournalPhase::PushStarted,
        Some(json!({"source_oid": source_oid})),
    )?;

    let mut args: Vec<String> = vec!["push".into()];
    if opts.force_with_lease {
        args.push("--force-with-lease".into());
    }
    args.push("--end-of-options".into());
    args.push(opts.remote.clone());
    args.push(opts.refspec.clone());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    runner.run(&arg_refs).map_err(|e| {
        let _ = lock.release();
        format!("git push: {e}")
    })?;

    let result = json!({
        "pushed": true,
        "source_oid": source_oid,
        "remote": opts.remote,
        "refspec": opts.refspec,
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

fn push_input_hash(opts: &PushOptions) -> String {
    sha256_hex(&format!("{}\u{0}{}\u{0}{}", opts.remote, opts.refspec, opts.force_with_lease))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_repo_with_remote(root: &Path, remote: &Path) {
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
        std::fs::write(root.join("a.txt"), b"base").unwrap();
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
        // Create bare remote.
        std::process::Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg("-q")
            .arg(remote)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["remote", "add", "origin", remote.to_str().unwrap()])
            .status()
            .unwrap();
    }

    #[test]
    fn push_succeeds() {
        let dir = tempdir().unwrap();
        let remote = tempdir().unwrap();
        init_repo_with_remote(dir.path(), remote.path());

        // Make a new commit.
        std::fs::write(dir.path().join("b.txt"), b"new").unwrap();
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

        let opts = PushOptions {
            remote: "origin".to_string(),
            refspec: "main".to_string(),
            request_id: format!("push-{}", uuid::Uuid::new_v4()),
            force_with_lease: false,
        };
        let result = push(dir.path(), &opts, None).unwrap();
        assert_eq!(result["pushed"], json!(true));
    }

    #[test]
    fn push_idempotent_replay() {
        let dir = tempdir().unwrap();
        let remote = tempdir().unwrap();
        init_repo_with_remote(dir.path(), remote.path());

        std::fs::write(dir.path().join("b.txt"), b"new").unwrap();
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

        let opts = PushOptions {
            remote: "origin".to_string(),
            refspec: "main".to_string(),
            request_id: format!("replay-{}", uuid::Uuid::new_v4()),
            force_with_lease: false,
        };
        let r1 = push(dir.path(), &opts, None).unwrap();
        let r2 = push(dir.path(), &opts, None).unwrap();
        assert_eq!(r1["source_oid"], r2["source_oid"]);
    }
}
