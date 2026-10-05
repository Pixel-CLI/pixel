// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Execute frozen checks in private source workspaces and capture factual receipts.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::model::{Check, CheckOutcome, SourceSnapshot, TaskContract, VerificationReceipt};
use crate::sandbox::{self, RunOutcome};
use crate::{Result, now_ms, snapshot};

/// Bind declared child toolchains to observed executable bytes as well.
pub fn contract_check_identity(check: &Check, contract: &TaskContract) -> Result<String> {
    sandbox::execution(&check.argv, &contract.toolchain).map(|execution| execution.identity)
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
    let mut receipts = Vec::with_capacity(checks.len());
    for check in checks {
        let source_marker = snapshot::mutation_marker(workspace.path(), snapshot)?;
        let started_ms = now_ms();
        let started = Instant::now();
        let execution = sandbox::execution(&check.argv, &contract.toolchain)?;
        let run = execution.run_confined(
            workspace.path(),
            &workspace.path().join(&check.cwd),
            Duration::from_millis(check.timeout_ms),
            lease_path,
        )?;
        let intact = snapshot::unchanged(workspace.path(), snapshot, contract)?
            && snapshot::mutation_marker(workspace.path(), snapshot)? == source_marker;
        let identity_current = contract_check_identity(check, contract)
            .is_ok_and(|identity| identity == execution.identity);
        let (outcome, exit_code) = match run.outcome {
            RunOutcome::Completed => (
                if run.exit_code == Some(0) {
                    CheckOutcome::Passed
                } else {
                    CheckOutcome::Failed
                },
                run.exit_code,
            ),
            RunOutcome::TimedOut => (CheckOutcome::TimedOut, None),
            RunOutcome::Overflow => (CheckOutcome::Unavailable, None),
        };
        let outcome =
            if sandbox::output_overflow(run.stdout_bytes, run.stderr_bytes) || !identity_current {
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
            stdout_sha256: run.stdout_sha256,
            stderr_sha256: run.stderr_sha256,
            stdout_bytes: run.stdout_bytes,
            stderr_bytes: run.stderr_bytes,
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
    use sha2::Digest;
    use std::fs;

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
        let finished: serde_json::Value =
            serde_json::from_slice(&fs::read(lease_path).unwrap()).unwrap();
        assert_eq!(finished["state"], "finished");
        assert!(finished["process_group"].as_u64().unwrap() > 0);
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
            hex::encode(sha2::Sha256::digest(b"original")),
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

    #[test]
    fn verify_records_failed_timed_out_and_overflowing_runs() {
        let root = repo();
        let configured = contract();
        let captured = snapshot::capture(root.path(), &configured, true).unwrap();
        let failing = check("exit 3");
        let mut timed = check("sleep 10");
        timed.timeout_ms = 100;
        let overflowing = check("head -c 20000000 /dev/zero; sleep 10");
        let receipts = verify_with_lease(
            &captured,
            &configured,
            &[failing, timed, overflowing],
            "run",
            None,
        )
        .unwrap();
        assert_eq!(receipts.len(), 3);
        assert_eq!(receipts[0].outcome, CheckOutcome::Failed);
        assert_eq!(receipts[0].exit_code, Some(3));
        assert_eq!(receipts[1].outcome, CheckOutcome::TimedOut);
        assert_eq!(receipts[1].exit_code, None);
        assert_eq!(receipts[2].outcome, CheckOutcome::Unavailable);
        assert_eq!(receipts[2].exit_code, None);
        assert!(receipts[2].stdout_bytes > sandbox::MAX_OUTPUT_BYTES);
    }
}
