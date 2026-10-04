// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Execute frozen checks in private source workspaces and capture factual receipts.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{Check, CheckOutcome, SourceSnapshot, TaskContract, VerificationReceipt};
use crate::{Error, Result, digest, now_ms, snapshot};

const MAX_OUTPUT_BYTES: u64 = 16_777_216;

#[derive(Debug, Serialize, Deserialize)]
struct ProcessLease {
    state: String,
    process_group: Option<u32>,
}

fn lease(path: Option<&Path>, state: &str, process_group: Option<u32>) -> Result<()> {
    if let Some(path) = path {
        pixel_ops::durable::write_durably(
            path,
            &serde_json::to_vec(&ProcessLease {
                state: state.into(),
                process_group,
            })?,
        )?;
    }
    Ok(())
}

/// An interrupted launch is unknown; a recorded live group cannot be rerun.
pub fn check_recovery(path: &Path) -> Result<()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let lease: ProcessLease = serde_json::from_slice(&bytes)?;
    if lease.state == "finished" {
        return Ok(());
    }
    if lease.state != "running" {
        return Err(Error::Blocked(
            "interrupted process launch has unknown ownership; no automatic rerun".into(),
        ));
    }
    let pid = lease
        .process_group
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or_else(|| Error::Corrupt("invalid verification process group".into()))?;
    // SAFETY: signal zero only probes the recorded process group.
    if unsafe { libc::kill(-pid, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    {
        return Err(Error::Busy(
            "interrupted verification process group is still alive".into(),
        ));
    }
    Ok(())
}

/// Bind declared child toolchains to observed executable bytes as well.
pub fn contract_check_identity(check: &Check, contract: &TaskContract) -> Result<String> {
    execution(check, &contract.toolchain).map(|execution| execution.identity)
}

struct Execution {
    binary: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    identity: String,
}

fn execution(check: &Check, toolchain: &BTreeMap<String, String>) -> Result<Execution> {
    let program = check
        .argv
        .first()
        .ok_or_else(|| Error::Invalid("empty check command".into()))?;
    let binary = executable(program)?;
    let mut environment: BTreeMap<_, _> = std::env::vars_os().collect();
    environment.retain(|name, _| {
        !name.as_bytes().starts_with(b"GIT_") && !name.as_bytes().starts_with(b"PIXEL_TASK_")
    });
    // These values are either forbidden, private-workspace overrides, or shell
    // bookkeeping. Remove them from both the identity and the child process.
    for name in [
        "ANTHROPIC_API_KEY",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "CARGO_TARGET_DIR",
        "PWD",
        "OLDPWD",
        "SHLVL",
        "_",
    ] {
        environment.remove(std::ffi::OsStr::new(name));
    }
    environment.insert("PIXEL_VERIFICATION".into(), "1".into());
    environment.insert("GIT_CONFIG_GLOBAL".into(), "/dev/null".into());
    environment.insert("GIT_CONFIG_SYSTEM".into(), "/dev/null".into());
    let environment_bytes: Vec<_> = environment
        .iter()
        .map(|(name, value)| (name.as_bytes(), value.as_bytes()))
        .collect();
    let mut observed_toolchain = Vec::with_capacity(toolchain.len());
    for (program, expected) in toolchain {
        let path = executable(program)?;
        let observed = hex::encode(Sha256::digest(fs::read(&path)?));
        if observed != *expected {
            return Err(Error::Blocked(format!(
                "toolchain executable differs from contract: {program}"
            )));
        }
        observed_toolchain.push((program, path.display().to_string(), observed));
    }
    let identity = digest(&(
        check,
        binary.display().to_string(),
        hex::encode(Sha256::digest(fs::read(&binary)?)),
        digest(&environment_bytes)?,
        // The actual private output directory changes per run; its semantic
        // location is fixed and never points at the live checkout.
        ("CARGO_TARGET_DIR", "<captured-workspace>/target"),
        observed_toolchain,
    ))?;
    Ok(Execution {
        binary,
        environment,
        identity,
    })
}

fn executable(program: &str) -> Result<PathBuf> {
    executable_in_paths(program, std::env::var_os("PATH").as_deref())
}

fn executable_in_paths(program: &str, paths: Option<&std::ffi::OsStr>) -> Result<PathBuf> {
    let candidate = Path::new(program);
    if candidate.is_absolute() {
        return candidate.canonicalize().map_err(Into::into);
    }
    if candidate.components().count() > 1 {
        return Err(Error::Invalid("check executable must be an absolute path or a PATH program; use a script interpreter for repo scripts".into()));
    }
    let paths = paths.ok_or_else(|| Error::Unavailable("PATH is unavailable".into()))?;
    for directory in std::env::split_paths(paths) {
        let path = directory.join(program);
        if let Ok(meta) = fs::metadata(&path)
            && meta.is_file()
            && meta.permissions().mode() & 0o111 != 0
        {
            return path.canonicalize().map_err(Into::into);
        }
    }
    Err(Error::Unavailable(format!(
        "check executable unavailable: {program}"
    )))
}

fn before_deadline(now: Instant, deadline: Instant) -> bool {
    now < deadline
}

fn output_overflow(stdout: u64, stderr: u64) -> bool {
    stdout > MAX_OUTPUT_BYTES || stderr > MAX_OUTPUT_BYTES
}

fn output_file(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

fn output_summary(path: &Path) -> Result<(String, u64)> {
    use std::io::Read;
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buffer = vec![0; 8192];
    loop {
        match file.read(&mut buffer)? {
            0 => break,
            count => {
                hash.update(&buffer[..count]);
                bytes += count as u64;
            }
        }
    }
    Ok((hex::encode(hash.finalize()), bytes))
}

fn terminate_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: the child was started in its own process group; a negative
        // PID addresses that group, never this process's group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        terminate_group(self.0.id());
        let _ = self.0.wait();
    }
}

/// Runs only contract checks; callers cannot submit a successful receipt.
pub(crate) fn verify_with_lease(
    snapshot: &SourceSnapshot,
    contract: &TaskContract,
    checks: &[Check],
    run_id: &str,
    lease_path: Option<&Path>,
) -> Result<Vec<VerificationReceipt>> {
    let workspace = snapshot::materialize(snapshot)?;
    let logs = tempfile::Builder::new()
        .prefix("pixel-check-output-")
        .tempdir()?;
    let mut receipts = Vec::with_capacity(checks.len());
    for check in checks {
        let source_marker = snapshot::mutation_marker(workspace.path(), snapshot)?;
        let started_ms = now_ms();
        let started = Instant::now();
        let stdout_path = logs.path().join(format!("{}.stdout", check.id));
        let stderr_path = logs.path().join(format!("{}.stderr", check.id));
        let stdout = output_file(&stdout_path)?;
        let stderr = output_file(&stderr_path)?;
        let execution = execution(check, &contract.toolchain)?;
        let cwd = workspace.path().join(&check.cwd).canonicalize()?;
        if !cwd.starts_with(workspace.path().canonicalize()?) {
            return Err(Error::Invalid(
                "check cwd escapes captured workspace".into(),
            ));
        }
        let mut command = Command::new(&execution.binary);
        command
            .args(&check.argv[1..])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .env_clear()
            .envs(&execution.environment)
            .env("CARGO_TARGET_DIR", workspace.path().join("target"))
            .process_group(0);
        lease(lease_path, "starting", None)?;
        let mut guard = match command.spawn() {
            Ok(child) => ChildGuard(child),
            Err(error) => {
                lease(lease_path, "finished", None)?;
                return Err(error.into());
            }
        };
        let child = &mut guard.0;
        let pid = child.id();
        if let Err(error) = lease(lease_path, "running", Some(pid)) {
            terminate_group(pid);
            let _ = child.wait();
            return Err(error);
        }
        let deadline = Instant::now() + Duration::from_millis(check.timeout_ms);
        let (outcome, exit_code) = loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    terminate_group(pid);
                    break (
                        if status.success() {
                            CheckOutcome::Passed
                        } else {
                            CheckOutcome::Failed
                        },
                        status.code(),
                    );
                }
                Ok(None)
                    if output_overflow(
                        fs::metadata(&stdout_path)?.len(),
                        fs::metadata(&stderr_path)?.len(),
                    ) =>
                {
                    terminate_group(pid);
                    child.wait()?;
                    break (CheckOutcome::Unavailable, None);
                }
                Ok(None) if before_deadline(Instant::now(), deadline) => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Ok(None) => {
                    terminate_group(pid);
                    child.wait()?;
                    break (CheckOutcome::TimedOut, None);
                }
                Err(error) => {
                    terminate_group(pid);
                    let _ = child.wait();
                    return Err(error.into());
                }
            }
        };
        lease(lease_path, "finished", Some(pid))?;
        let intact = snapshot::unchanged(workspace.path(), snapshot, contract)?
            && snapshot::mutation_marker(workspace.path(), snapshot)? == source_marker;
        let (stdout_sha256, stdout_bytes) = output_summary(&stdout_path)?;
        let (stderr_sha256, stderr_bytes) = output_summary(&stderr_path)?;
        let identity_current = contract_check_identity(check, contract)
            .is_ok_and(|identity| identity == execution.identity);
        let outcome = if output_overflow(stdout_bytes, stderr_bytes) || !identity_current {
            CheckOutcome::Unavailable
        } else if intact {
            outcome
        } else {
            CheckOutcome::SourceChanged
        };
        receipts.push(VerificationReceipt {
            run_id: run_id.to_string(),
            check_id: check.id.clone(),
            source_id: snapshot.content_id.clone(),
            contract_id: contract.id()?,
            check_digest: execution.identity,
            outcome,
            exit_code,
            started_ms,
            finished_ms: now_ms(),
            duration_ms: started.elapsed().as_millis() as u64,
            stdout_sha256,
            stderr_sha256,
            stdout_bytes,
            stderr_bytes,
            execution_root: workspace.path().display().to_string(),
            diagnostic: if !intact {
                Some("check modified its captured source".into())
            } else if !identity_current {
                Some("check execution environment changed during verification".into())
            } else {
                None
            },
        });
        if !intact {
            break;
        }
    }
    Ok(receipts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{contract, repo};

    fn check(script: &str) -> Check {
        Check {
            id: "check".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            cwd: ".".into(),
            timeout_ms: 1000,
            required: true,
        }
    }

    #[test]
    fn deadline_and_output_limits_distinguish_both_exact_boundaries() {
        let at = Instant::now();
        assert!(before_deadline(at - Duration::from_nanos(1), at));
        assert!(!before_deadline(at, at));
        assert!(!before_deadline(at + Duration::from_nanos(1), at));
        assert!(!output_overflow(MAX_OUTPUT_BYTES, MAX_OUTPUT_BYTES));
        assert!(!output_overflow(0, 0));
        assert!(output_overflow(MAX_OUTPUT_BYTES + 1, 0));
        assert!(output_overflow(0, MAX_OUTPUT_BYTES + 1));
    }

    #[test]
    fn executable_resolution_skips_nonfiles_nonexecutables_and_missing_candidates() {
        let root = tempfile::tempdir().unwrap();
        let paths: Vec<_> = ["missing", "directory", "nonexec", "valid"]
            .iter()
            .map(|name| root.path().join(name))
            .collect();
        for path in &paths[1..] {
            fs::create_dir(path).unwrap();
        }
        fs::create_dir(paths[1].join("tool")).unwrap();
        fs::write(paths[2].join("tool"), "not executable").unwrap();
        fs::set_permissions(paths[2].join("tool"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(paths[3].join("tool"), "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(paths[3].join("tool"), fs::Permissions::from_mode(0o755)).unwrap();
        let joined = std::env::join_paths(&paths).unwrap();
        assert_eq!(
            executable_in_paths("tool", Some(&joined)).unwrap(),
            paths[3].join("tool").canonicalize().unwrap()
        );
        assert!(matches!(
            executable_in_paths("absent", Some(&joined)),
            Err(Error::Unavailable(_))
        ));
        assert!(matches!(
            executable_in_paths("tool", None),
            Err(Error::Unavailable(_))
        ));
        for program in ["./tool", "sub/tool", "sub/dir/tool"] {
            assert!(matches!(
                executable_in_paths(program, Some(&joined)),
                Err(Error::Invalid(_))
            ));
        }
        let absolute = paths[3].join("tool");
        assert_eq!(
            executable_in_paths(absolute.to_str().unwrap(), None).unwrap(),
            absolute.canonicalize().unwrap()
        );
    }

    #[test]
    fn process_leases_are_durable_and_read_errors_never_mean_missing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lease.json");
        assert!(check_recovery(&path).is_ok());
        lease(Some(&path), "starting", None).unwrap();
        let observed: ProcessLease = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(observed.state, "starting");
        assert_eq!(observed.process_group, None);
        assert!(matches!(check_recovery(&path), Err(Error::Blocked(_))));
        lease(Some(&path), "running", Some(u32::MAX)).unwrap();
        assert!(matches!(check_recovery(&path), Err(Error::Corrupt(_))));
        lease(Some(&path), "finished", Some(12)).unwrap();
        let observed: ProcessLease = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(observed.state, "finished");
        assert_eq!(observed.process_group, Some(12));
        assert!(check_recovery(&path).is_ok());
        let mut exited = Command::new("true").process_group(0).spawn().unwrap();
        let gone_group = exited.id();
        assert!(exited.wait().unwrap().success());
        lease(Some(&path), "running", Some(gone_group)).unwrap();
        assert!(check_recovery(&path).is_ok());
        assert!(matches!(check_recovery(root.path()), Err(Error::Io(_))));
        fs::write(&path, b"not JSON").unwrap();
        assert!(matches!(check_recovery(&path), Err(Error::Json(_))));
    }

    #[test]
    fn child_guard_drop_terminates_an_unfinished_owned_process() {
        let child = Command::new("sleep")
            .arg("10")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let started = Instant::now();
        drop(ChildGuard(child));
        // SAFETY: signal zero only observes the PID of the child just reaped.
        let alive = unsafe { libc::kill(i32::try_from(pid).unwrap(), 0) } == 0;
        if alive {
            // SAFETY: cleanup targets this test's still-live owned child only.
            unsafe {
                libc::kill(i32::try_from(pid).unwrap(), libc::SIGKILL);
            }
        }
        assert!(!alive, "guard drop left its child running");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn recovery_probes_the_group_when_its_original_leader_has_exited() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lease.json");
        let mut leader = Command::new("/bin/sh")
            .args(["-c", "sleep 10 & exit 0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let group = leader.id();
        assert!(leader.wait().unwrap().success());
        lease(Some(&path), "running", Some(group)).unwrap();
        let result = check_recovery(&path);
        // SAFETY: this is the distinct process group created above; cleanup
        // does not use the implementation whose group probe is under test.
        unsafe {
            libc::kill(-i32::try_from(group).unwrap(), libc::SIGKILL);
        }
        assert!(matches!(result, Err(Error::Busy(_))));
    }

    #[test]
    fn output_files_are_private_and_existing_files_are_never_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("output");
        drop(output_file(&path).unwrap());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            output_summary(&path).unwrap(),
            (hex::encode(Sha256::digest(b"")), 0)
        );
        fs::write(&path, "kept").unwrap();
        assert!(output_file(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"kept");
        assert_eq!(
            output_summary(&path).unwrap(),
            (hex::encode(Sha256::digest(b"kept")), 4)
        );
    }

    #[test]
    fn private_verification_records_finished_lease_and_stops_after_source_mutation() {
        let root = repo();
        let configured = contract();
        let captured = snapshot::capture(root.path(), &configured, true).unwrap();
        let lease_path = root.path().join(".pixel/run.json");
        let first = check("printf changed > source.txt");
        let mut second = check("true");
        second.id = "never-run".into();
        let receipts = verify_with_lease(
            &captured,
            &configured,
            &[first, second],
            "run",
            Some(&lease_path),
        )
        .unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].outcome, CheckOutcome::SourceChanged);
        assert_eq!(
            receipts[0].diagnostic.as_deref(),
            Some("check modified its captured source")
        );
        let finished: ProcessLease =
            serde_json::from_slice(&fs::read(lease_path).unwrap()).unwrap();
        assert_eq!(finished.state, "finished");
        assert!(finished.process_group.unwrap() > 0);
        assert_eq!(
            fs::read_to_string(root.path().join("source.txt")).unwrap(),
            "value"
        );
    }

    #[test]
    fn toolchain_changes_during_execution_are_unavailable_with_explicit_diagnostic() {
        let root = repo();
        let tools = tempfile::tempdir().unwrap();
        let binary = tools.path().join("tool");
        fs::write(&binary, "original").unwrap();
        let mut configured = contract();
        configured.toolchain.insert(
            binary.display().to_string(),
            hex::encode(Sha256::digest(b"original")),
        );
        let captured = snapshot::capture(root.path(), &configured, true).unwrap();
        let mut command = check("printf changed > \"$1\"");
        command
            .argv
            .extend(["check".into(), binary.display().to_string()]);
        let receipts = verify_with_lease(&captured, &configured, &[command], "run", None).unwrap();
        assert_eq!(receipts[0].outcome, CheckOutcome::Unavailable);
        assert_eq!(
            receipts[0].diagnostic.as_deref(),
            Some("check execution environment changed during verification")
        );
    }
}
