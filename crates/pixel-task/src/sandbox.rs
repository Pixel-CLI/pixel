// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Confined child-process execution with durable leases and stable identities.
//!
//! Shared by task-contract verification and standalone verification runs:
//! resolves a PATH program, sanitizes the child environment, binds toolchain
//! digests into a stable identity, and runs one command inside a confinement
//! root under a deadline, output caps, and a durable process lease that
//! survives interruption.

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

use crate::{Error, Result, digest};

/// Cap on captured output per stream; a run writing past it is stopped.
pub const MAX_OUTPUT_BYTES: u64 = 16_777_216; // 16 MiB

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

/// A resolved binary, its sanitized environment, and a digest identity.
///
/// Only constructible through [`execution`], which guarantees a non-empty
/// argv, so confined runs always have a program to execute.
///
/// The fields are read-only: what runs and the identity that names it are
/// fixed together by [`execution`], so a caller cannot change one alone.
#[derive(Debug)]
pub struct Execution {
    argv: Vec<String>,
    binary: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    identity: String,
}

/// How a confined run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The process exited on its own; see [`RunOutput::exit_code`].
    Completed,
    /// The deadline passed; the process group was killed.
    TimedOut,
    /// An output stream passed [`MAX_OUTPUT_BYTES`]; the group was killed.
    Overflow,
}

/// Factual result of one confined run; captured output is hashed, not kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    pub outcome: RunOutcome,
    pub exit_code: Option<i32>,
    pub stdout_sha256: String,
    pub stdout_bytes: u64,
    pub stderr_sha256: String,
    pub stderr_bytes: u64,
}

impl Execution {
    /// The canonical path of the resolved `argv[0]`.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// The sanitized environment the child starts with.
    pub fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.environment
    }

    /// The digest binding the subject, binary, environment and toolchain.
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Run this command once inside `root`, waiting at most `timeout`.
    ///
    /// `cwd` must resolve to a directory inside `root`. The child runs in its
    /// own process group with the cleared, sanitized environment; the captured
    /// streams are hashed into the output, never retained. With `lease_path`,
    /// starting/running/finished transitions are written durable to
    /// interruption: a later [`check_recovery`] can tell a finished run from a
    /// live one that must not be replayed.
    ///
    /// # Errors
    ///
    /// `cwd` escaping `root`, a failed spawn, a lease write failure, or output
    /// capture I/O all abort before or during the run and leave no live child.
    pub fn run_confined(
        &self,
        root: &Path,
        cwd: &Path,
        timeout: Duration,
        lease_path: Option<&Path>,
    ) -> Result<RunOutput> {
        let logs = tempfile::Builder::new()
            .prefix("pixel-check-output-")
            .tempdir()?;
        let stdout_path = logs.path().join("stdout");
        let stderr_path = logs.path().join("stderr");
        let stdout = output_file(&stdout_path)?;
        let stderr = output_file(&stderr_path)?;
        let root = root.canonicalize()?;
        let cwd = cwd.canonicalize()?;
        if !cwd.starts_with(&root) {
            return Err(Error::Invalid(
                "check cwd escapes captured workspace".into(),
            ));
        }
        let mut command = Command::new(&self.binary);
        command
            .args(&self.argv[1..])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .env_clear()
            .envs(&self.environment)
            .env("CARGO_TARGET_DIR", root.join("target"))
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
        let deadline = Instant::now() + timeout;
        let (outcome, exit_code) = loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    terminate_group(pid);
                    break (RunOutcome::Completed, status.code());
                }
                Ok(None)
                    if output_overflow(
                        fs::metadata(&stdout_path)?.len(),
                        fs::metadata(&stderr_path)?.len(),
                    ) =>
                {
                    terminate_group(pid);
                    child.wait()?;
                    break (RunOutcome::Overflow, None);
                }
                Ok(None) if before_deadline(Instant::now(), deadline) => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Ok(None) => {
                    terminate_group(pid);
                    child.wait()?;
                    break (RunOutcome::TimedOut, None);
                }
                Err(error) => {
                    terminate_group(pid);
                    let _ = child.wait();
                    return Err(error.into());
                }
            }
        };
        lease(lease_path, "finished", Some(pid))?;
        let (stdout_sha256, stdout_bytes) = output_summary(&stdout_path)?;
        let (stderr_sha256, stderr_bytes) = output_summary(&stderr_path)?;
        Ok(RunOutput {
            outcome,
            exit_code,
            stdout_sha256,
            stdout_bytes,
            stderr_sha256,
            stderr_bytes,
        })
    }
}

/// True when either captured stream exceeded [`MAX_OUTPUT_BYTES`].
pub fn output_overflow(stdout: u64, stderr: u64) -> bool {
    stdout > MAX_OUTPUT_BYTES || stderr > MAX_OUTPUT_BYTES
}

fn before_deadline(now: Instant, deadline: Instant) -> bool {
    now < deadline
}

/// Resolve `argv[0]` against PATH and bind toolchain digests.
///
/// The identity digests `subject`, the resolved binary bytes, the sanitized
/// environment, and the toolchain, so the same subject under the same
/// environment binds one identity. `subject` is what the caller's records
/// name: task verification passes the whole [`crate::model::Check`], whose
/// digest is the `check_digest` persisted in every receipt, so its
/// serialization must not change; a caller with nothing else passes `argv`.
///
/// # Errors
///
/// An empty argv, an unresolvable program, an unreadable binary or toolchain
/// file, or a toolchain digest mismatch.
pub fn execution<S: Serialize + ?Sized>(
    argv: &[String],
    subject: &S,
    toolchain: &BTreeMap<String, String>,
) -> Result<Execution> {
    let program = argv
        .first()
        .ok_or_else(|| Error::Invalid("empty check command".into()))?;
    let binary = executable(program)?;
    let environment = sanitize(std::env::vars_os().collect());
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
    let identity = identity(
        subject,
        &binary,
        &hex::encode(Sha256::digest(fs::read(&binary)?)),
        &environment,
        &observed_toolchain,
    )?;
    Ok(Execution {
        argv: argv.to_vec(),
        binary,
        environment,
        identity,
    })
}

/// The digest persisted as a receipt's `check_digest`.
///
/// Its input shape is a stored contract: a receipt recorded by one release is
/// compared with the value the next release computes, and any change to what
/// is serialized here turns every recorded receipt `Unavailable`.
fn identity<S: Serialize + ?Sized>(
    subject: &S,
    binary: &Path,
    binary_sha256: &str,
    environment: &BTreeMap<OsString, OsString>,
    observed_toolchain: &[(&String, String, String)],
) -> Result<String> {
    let environment_bytes: Vec<_> = environment
        .iter()
        .map(|(name, value)| (name.as_bytes(), value.as_bytes()))
        .collect();
    digest(&(
        subject,
        binary.display().to_string(),
        binary_sha256,
        digest(&environment_bytes)?,
        // The actual private output directory changes per run; its semantic
        // location is fixed and never points at the live checkout.
        ("CARGO_TARGET_DIR", "<captured-workspace>/target"),
        observed_toolchain,
    ))
}

fn sanitize(mut environment: BTreeMap<OsString, OsString>) -> BTreeMap<OsString, OsString> {
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
    environment
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn run(argv: &[&str], root: &Path, cwd: &Path, timeout_ms: u64) -> RunOutput {
        let argv: Vec<String> = argv.iter().map(ToString::to_string).collect();
        let execution = execution(&argv, &argv, &BTreeMap::new()).unwrap();
        execution
            .run_confined(root, cwd, Duration::from_millis(timeout_ms), None)
            .unwrap()
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
    fn sanitize_removes_forbidden_bookkeeping_and_git_variables() {
        let mut given: BTreeMap<OsString, OsString> = BTreeMap::new();
        for (name, value) in [
            ("GIT_DIR", "x"),
            ("GIT_SOMETHING", "x"),
            ("PIXEL_TASK_MARKER", "x"),
            ("ANTHROPIC_API_KEY", "x"),
            ("CARGO_TARGET_DIR", "x"),
            ("PWD", "x"),
            ("OLDPWD", "x"),
            ("SHLVL", "x"),
            ("_", "x"),
            ("KEEP_ME", "v"),
        ] {
            given.insert(name.into(), value.into());
        }
        let sanitized = sanitize(given);
        assert_eq!(
            sanitized
                .get(OsStr::new("KEEP_ME"))
                .map(OsString::as_os_str),
            Some(OsStr::new("v"))
        );
        for removed in [
            "GIT_DIR",
            "GIT_SOMETHING",
            "PIXEL_TASK_MARKER",
            "ANTHROPIC_API_KEY",
            "CARGO_TARGET_DIR",
            "PWD",
            "OLDPWD",
            "SHLVL",
            "_",
        ] {
            assert!(
                !sanitized.contains_key(OsStr::new(removed)),
                "{removed} survived sanitization"
            );
        }
        assert_eq!(
            sanitized
                .get(OsStr::new("PIXEL_VERIFICATION"))
                .map(OsString::as_os_str),
            Some(OsStr::new("1"))
        );
        assert_eq!(
            sanitized
                .get(OsStr::new("GIT_CONFIG_GLOBAL"))
                .map(OsString::as_os_str),
            Some(OsStr::new("/dev/null"))
        );
        assert_eq!(
            sanitized
                .get(OsStr::new("GIT_CONFIG_SYSTEM"))
                .map(OsString::as_os_str),
            Some(OsStr::new("/dev/null"))
        );
    }

    #[test]
    fn execution_identity_binds_command_environment_and_toolchain() {
        let argv: Vec<String> = ["/bin/sh", "-c", "true"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let no_tools = BTreeMap::new();
        let first = execution(&argv, &argv, &no_tools).unwrap();
        let second = execution(&argv, &argv, &no_tools).unwrap();
        assert_eq!(first.identity, second.identity);

        let mut different = argv.clone();
        different[2] = "false".into();
        assert_ne!(
            first.identity,
            execution(&different, &different, &no_tools)
                .unwrap()
                .identity
        );

        let tools = tempfile::tempdir().unwrap();
        let tool = tools.path().join("tool");
        fs::write(&tool, "original").unwrap();
        let mut toolchain = BTreeMap::new();
        toolchain.insert(
            tool.display().to_string(),
            hex::encode(Sha256::digest(b"original")),
        );
        let bound = execution(&argv, &argv, &toolchain).unwrap();
        assert_ne!(first.identity, bound.identity);

        toolchain.insert(
            tool.display().to_string(),
            hex::encode(Sha256::digest(b"other")),
        );
        assert!(matches!(
            execution(&argv, &argv, &toolchain),
            Err(Error::Blocked(_))
        ));
    }

    #[test]
    fn identity_input_shape_matches_the_receipts_main_recorded() {
        // Expected value computed by main's runner (b3551ad) on these inputs:
        // `digest(&(check, binary, binary_sha256, digest(env), target, toolchain))`.
        // A change here turns every recorded receipt `Unavailable` on upgrade.
        let check = crate::model::Check {
            id: "build".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "true".into()],
            cwd: "crates".into(),
            timeout_ms: 1000,
            required: false,
        };
        let mut environment: BTreeMap<OsString, OsString> = BTreeMap::new();
        environment.insert("PATH".into(), "/usr/bin".into());
        environment.insert("PIXEL_VERIFICATION".into(), "1".into());
        let program = String::from("cc");
        let observed_toolchain = [(&program, "/usr/bin/cc".to_string(), "1".repeat(64))];
        assert_eq!(
            identity(
                &check,
                Path::new("/bin/sh"),
                &"0".repeat(64),
                &environment,
                &observed_toolchain,
            )
            .unwrap(),
            "92b5b1fec7592dc6db6d028d4072f01e88bd945ab9093d746f095b2a3583fc91"
        );
    }

    #[test]
    fn execution_accessors_expose_what_runs_and_its_identity() {
        let argv: Vec<String> = ["/bin/sh", "-c", "true"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let execution = execution(&argv, &argv, &BTreeMap::new()).unwrap();
        assert_eq!(
            execution.binary(),
            Path::new("/bin/sh").canonicalize().unwrap()
        );
        assert_eq!(
            execution
                .environment()
                .get(OsStr::new("PIXEL_VERIFICATION"))
                .map(OsString::as_os_str),
            Some(OsStr::new("1"))
        );
        assert_eq!(execution.identity(), execution.identity.as_str());
        assert_eq!(execution.identity().len(), 64);
    }

    #[test]
    fn run_confined_completes_and_hashes_captured_output() {
        let root = tempfile::tempdir().unwrap();
        let output = run(
            &["/bin/sh", "-c", "printf hello"],
            root.path(),
            root.path(),
            1000,
        );
        assert_eq!(output.outcome, RunOutcome::Completed);
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout_bytes, 5);
        assert_eq!(output.stdout_sha256, hex::encode(Sha256::digest(b"hello")));
        assert_eq!(output.stderr_bytes, 0);
        assert_eq!(output.stderr_sha256, hex::encode(Sha256::digest(b"")));
    }

    #[test]
    fn run_confined_reports_a_failing_exit_code() {
        let root = tempfile::tempdir().unwrap();
        let output = run(&["/bin/sh", "-c", "exit 3"], root.path(), root.path(), 1000);
        assert_eq!(output.outcome, RunOutcome::Completed);
        assert_eq!(output.exit_code, Some(3));
    }

    /// True once `pid` no longer names a running process. A killed child
    /// whose parent died is reparented and reaped asynchronously; until
    /// then it is a zombie, which `kill(pid, 0)` still reports as present.
    fn gone(pid: i32) -> bool {
        // SAFETY: signal zero only probes the pid; it delivers nothing.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, rest)| rest.starts_with('Z'))
        })
    }

    #[test]
    fn run_confined_times_out_and_kills_the_process_group() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("pid");
        // The leader outlives the deadline by seconds and leaves a descendant
        // in its group that outlives the test: only a group kill at the
        // deadline ends both before the bound below.
        let script = format!(
            "sleep 30 >/dev/null 2>&1 & echo $! > \"{}\"; sleep 3",
            marker.display()
        );
        let argv: Vec<String> = ["/bin/sh", "-c", &script]
            .iter()
            .map(ToString::to_string)
            .collect();
        let execution = execution(&argv, &argv, &BTreeMap::new()).unwrap();
        let started = Instant::now();
        let output = execution
            .run_confined(root.path(), root.path(), Duration::from_millis(100), None)
            .unwrap();
        let returned = started.elapsed();
        let descendant: i32 = fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
        let probe_deadline = Instant::now() + Duration::from_secs(2);
        while !gone(descendant) && Instant::now() < probe_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let survived = !gone(descendant);
        if survived {
            // SAFETY: cleanup targets this test's own recorded descendant only.
            unsafe {
                libc::kill(descendant, libc::SIGKILL);
            }
        }
        assert_eq!(output.outcome, RunOutcome::TimedOut);
        assert_eq!(output.exit_code, None);
        assert!(!survived, "a descendant of the timed-out group survived");
        assert!(
            returned < Duration::from_secs(2),
            "timed-out run returned after {returned:?}: the group was not killed at the deadline"
        );
    }

    #[test]
    fn run_confined_stops_a_run_whose_output_overflows_the_cap() {
        let root = tempfile::tempdir().unwrap();
        let output = run(
            &["/bin/sh", "-c", "head -c 20000000 /dev/zero; sleep 10"],
            root.path(),
            root.path(),
            5000,
        );
        assert_eq!(output.outcome, RunOutcome::Overflow);
        assert_eq!(output.exit_code, None);
        assert!(output.stdout_bytes > MAX_OUTPUT_BYTES);
    }

    #[test]
    fn run_confined_rejects_a_cwd_outside_the_confinement_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let argv: Vec<String> = ["/bin/sh", "-c", "true"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let execution = execution(&argv, &argv, &BTreeMap::new()).unwrap();
        assert!(matches!(
            execution.run_confined(
                root.path(),
                outside.path(),
                Duration::from_millis(1000),
                None
            ),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn run_confined_marks_the_lease_finished_when_the_spawn_fails() {
        let root = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        let binary = tools.path().join("tool");
        fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let argv: Vec<String> = [binary.to_str().unwrap(), "-c", "true"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let execution = execution(&argv, &argv, &BTreeMap::new()).unwrap();
        fs::remove_file(&binary).unwrap();
        let lease_path = root.path().join("lease.json");
        assert!(
            execution
                .run_confined(
                    root.path(),
                    root.path(),
                    Duration::from_millis(1000),
                    Some(&lease_path),
                )
                .is_err()
        );
        let recorded: ProcessLease =
            serde_json::from_slice(&fs::read(&lease_path).unwrap()).unwrap();
        assert_eq!(recorded.state, "finished");
    }

    #[test]
    fn run_confined_aborts_when_the_lease_cannot_be_written() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("no-such-dir");
        let argv: Vec<String> = ["/bin/sh", "-c", "sleep 10"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let execution = execution(&argv, &argv, &BTreeMap::new()).unwrap();
        assert!(matches!(
            execution.run_confined(
                root.path(),
                root.path(),
                Duration::from_millis(1000),
                Some(&missing.join("lease.json")),
            ),
            Err(Error::Io(_))
        ));
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
}
