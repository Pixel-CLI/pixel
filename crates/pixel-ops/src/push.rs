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

/// Split a refspec into its (source, destination) halves.
///
/// `main` → (`main`, `main`); `src:dst` → (`src`, `dst`). A leading `+`
/// (git's own force marker) is stripped from the source side — pixel expresses
/// force through `force_with_lease`, never through refspec syntax.
fn split_refspec(refspec: &str) -> (String, String) {
    match refspec.split_once(':') {
        Some((src, dst)) => (
            src.trim_start_matches('+').to_string(),
            dst.trim_start_matches('+').to_string(),
        ),
        None => {
            let one = refspec.trim_start_matches('+').to_string();
            (one.clone(), one)
        }
    }
}

/// Validate both halves of a refspec independently.
///
/// `validate_ref` rejects `:` because a colon is not legal *inside* a ref name.
/// A refspec is two ref names joined by one, so validating the whole string
/// rejected every `src:dst` push. Split first, then validate each side.
fn validate_refspec(refspec: &str) -> Result<(), String> {
    let (src, dst) = split_refspec(refspec);
    pixel_git::validate_ref(&src).map_err(|e| e.to_string())?;
    pixel_git::validate_ref(&dst).map_err(|e| e.to_string())?;
    Ok(())
}

/// The OID the *pushed ref* points at — not HEAD.
///
/// Reporting HEAD was wrong in two ways: the `source_oid` in the result
/// described a different commit than the one that moved whenever the refspec
/// named anything but the checked-out branch, and the crash-resume check
/// compared the remote ref against HEAD, so it could never confirm a completed
/// push of a non-HEAD branch. Falls back to HEAD only when the source side
/// cannot be resolved locally (e.g. a delete refspec).
fn resolve_source_oid(runner: &GitRunner, refspec: &str) -> Option<String> {
    let (src, _) = split_refspec(refspec);
    if src.is_empty() {
        return runner.rev_parse_head();
    }
    runner
        .run_opt(&["rev-parse", "--verify", "--quiet", &src])
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| runner.rev_parse_head())
}

/// The OID the remote currently has for `dst`, via `ls-remote`.
///
/// Returns `None` when the remote has no such ref (the push will create it).
fn resolve_remote_oid(runner: &GitRunner, remote: &str, dst: &str) -> Option<String> {
    let out = runner.run_opt(&["ls-remote", remote, dst])?;
    let text = String::from_utf8_lossy(&out);
    text.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let oid = fields.next()?;
        let name = fields.next()?;
        // Skip the `^{}` peeled entries annotated tags emit; we want the tag
        // object's own OID, which is what the remote ref actually holds.
        if name.ends_with("^{}") {
            return None;
        }
        let matches = name == dst
            || name == format!("refs/heads/{dst}")
            || name == format!("refs/tags/{dst}")
            || name == format!("refs/{dst}");
        if matches { Some(oid.to_string()) } else { None }
    })
}

/// Build the `git push` argument list.
///
/// When a lease is requested, resolve the remote's current OID and pass the
/// EXPLICIT `--force-with-lease=<dst>:<oid>` form. Bare `--force-with-lease`
/// leases against the remote-tracking ref, which does not exist for tags and is
/// destroyed by history-rewriting tools — git then refuses with "stale info",
/// so a legitimately rewritten history could never be pushed. Pinning the OID
/// we just observed keeps the actual safety property (refuse if someone else
/// moved the ref) while allowing a non-fast-forward we intend.
fn build_push_args(runner: &GitRunner, opts: &PushOptions) -> Vec<String> {
    let mut args: Vec<String> = vec!["push".into()];
    if opts.force_with_lease {
        let (_, dst) = split_refspec(&opts.refspec);
        match resolve_remote_oid(runner, &opts.remote, &dst) {
            // Remote has the ref: lease against exactly what we saw.
            Some(remote_oid) => args.push(format!("--force-with-lease={dst}:{remote_oid}")),
            // Remote has no such ref: the push creates it, so it cannot
            // clobber anything and needs no lease.
            None => {}
        }
    }
    args.push("--end-of-options".into());
    args.push(opts.remote.clone());
    args.push(opts.refspec.clone());
    args
}

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
    validate_refspec(&opts.refspec).map_err(|e| { let _ = lock.release(); e })?;

    // OID of the ref being pushed (not HEAD).
    let source_oid = resolve_source_oid(&runner, &opts.refspec).ok_or("no HEAD")?;

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
    let args = build_push_args(&runner, opts);
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
                    // Ask the REMOTE what it holds, rather than trusting a
                    // local remote-tracking ref. The tracking ref may be stale,
                    // absent (tags never have one), or removed by a
                    // history-rewriting tool — in all of which cases the old
                    // check silently failed to confirm a push that had in fact
                    // completed, and reported NETWORK_AMBIGUITY instead.
                    let (_, dst) = split_refspec(&opts.refspec);
                    if let Some(remote_ref) = resolve_remote_oid(runner, &opts.remote, &dst) {
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
    validate_refspec(&opts.refspec).map_err(|e| { let _ = lock.release(); e })?;

    let source_oid = resolve_source_oid(runner, &opts.refspec).ok_or("no HEAD")?;

    journal.transition(
        &opts.request_id,
        &repo_key,
        JournalPhase::PushStarted,
        Some(json!({"source_oid": source_oid})),
    )?;

    let args = build_push_args(runner, opts);
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
        // `-b main`: never rely on the machine's init.defaultBranch — the
        // push tests below use the literal refspec "main".
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg("-b")
            .arg("main")
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

    fn git(root: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn split_refspec_handles_both_forms() {
        assert_eq!(split_refspec("main"), ("main".into(), "main".into()));
        assert_eq!(split_refspec("src:dst"), ("src".into(), "dst".into()));
        assert_eq!(split_refspec("+src:dst"), ("src".into(), "dst".into()));
    }

    #[test]
    fn validate_refspec_accepts_src_colon_dst() {
        // `validate_ref` alone rejects a colon, which used to make every
        // `src:dst` push fail with "invalid git ref".
        assert!(validate_refspec("refs/heads/main:refs/heads/main").is_ok());
        assert!(validate_refspec("main").is_ok());
        assert!(validate_refspec("--upload-pack=evil").is_err());
    }

    /// A rewritten history must be pushable even though the remote-tracking
    /// ref is gone — this is exactly the state `git filter-repo` leaves behind,
    /// and bare `--force-with-lease` fails it with "stale info".
    #[test]
    fn force_with_lease_pushes_rewritten_history_without_tracking_ref() {
        let dir = tempdir().unwrap();
        let remote = tempdir().unwrap();
        init_repo_with_remote(dir.path(), remote.path());

        let branch = git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]);
        let opts_plain = PushOptions {
            remote: "origin".to_string(),
            refspec: branch.clone(),
            request_id: format!("base-{}", uuid::Uuid::new_v4()),
            force_with_lease: false,
        };
        push(dir.path(), &opts_plain, None).unwrap();

        // Rewrite history, then destroy the remote-tracking refs the way
        // git-filter-repo does.
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit", "-q", "--amend", "-m", "rewritten"])
            .status()
            .unwrap();
        for r in git(dir.path(), &["for-each-ref", "--format=%(refname)", "refs/remotes"])
            .lines()
        {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["update-ref", "-d", r])
                .status()
                .unwrap();
        }
        assert_eq!(
            git(dir.path(), &["for-each-ref", "refs/remotes"]),
            "",
            "precondition: no remote-tracking refs"
        );

        let opts = PushOptions {
            remote: "origin".to_string(),
            refspec: branch.clone(),
            request_id: format!("rw-{}", uuid::Uuid::new_v4()),
            force_with_lease: true,
        };
        let result = push(dir.path(), &opts, None).unwrap();
        assert_eq!(result["pushed"], json!(true));

        let local = git(dir.path(), &["rev-parse", &branch]);
        let pushed = git(remote.path(), &["rev-parse", &branch]);
        assert_eq!(local, pushed, "remote must hold the rewritten commit");
        assert_eq!(
            result["source_oid"], json!(local),
            "source_oid must be the pushed ref's OID"
        );
    }

    /// `source_oid` used to report HEAD, so pushing any branch other than the
    /// checked-out one described the wrong commit.
    #[test]
    fn source_oid_tracks_the_pushed_ref_not_head() {
        let dir = tempdir().unwrap();
        let remote = tempdir().unwrap();
        init_repo_with_remote(dir.path(), remote.path());

        let head_branch = git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]);
        // A side branch, then move HEAD forward so the two differ.
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["branch", "side"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit", "-q", "--allow-empty", "-m", "moves HEAD only"])
            .status()
            .unwrap();

        let side_oid = git(dir.path(), &["rev-parse", "side"]);
        let head_oid = git(dir.path(), &["rev-parse", &head_branch]);
        assert_ne!(side_oid, head_oid, "precondition: HEAD moved past side");

        let opts = PushOptions {
            remote: "origin".to_string(),
            refspec: "side".to_string(),
            request_id: format!("side-{}", uuid::Uuid::new_v4()),
            force_with_lease: false,
        };
        let result = push(dir.path(), &opts, None).unwrap();
        assert_eq!(result["source_oid"], json!(side_oid));
        assert_eq!(git(remote.path(), &["rev-parse", "side"]), side_oid);
    }
}
