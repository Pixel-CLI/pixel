// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Execute frozen checks in private source workspaces and capture factual receipts.

use std::path::Path;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::model::{
    Check, CheckKind, CheckOutcome, SourceSnapshot, TaskContract, VerificationReceipt,
};
use crate::sandbox::{self, RunOutcome};
use crate::structural::{self, StructuralContext, StructuralResult};
use crate::{Result, now_ms, snapshot};

/// Bind declared child toolchains to observed executable bytes as well.
///
/// The whole `check` (id, argv, cwd, timeout, required) is the identity's
/// subject, as receipts recorded before the sandbox extraction expect.
pub fn contract_check_identity(check: &Check, contract: &TaskContract) -> Result<String> {
    execution(check, contract).map(|execution| execution.identity().to_owned())
}

/// Identity for structural checks: the whole check, digested. No binary or
/// environment is involved — the kind selects a pure function over facts.
pub fn structural_check_identity(check: &Check) -> Result<String> {
    crate::digest(check)
}

fn execution(check: &Check, contract: &TaskContract) -> Result<sandbox::Execution> {
    sandbox::execution(&check.argv, check, &contract.toolchain)
}

/// The verdict of a structural check under `context`, or `None` when the
/// facts it needs are unavailable — which reports `Unavailable`, never a
/// silent pass.
fn structural_verdict(kind: CheckKind, context: Option<&StructuralContext>) -> Option<StructuralResult> {
    let context = context?;
    match kind {
        CheckKind::DiffInScope => {
            let manifest = context.manifest_paths.as_ref()?;
            Some(structural::diff_in_scope(
                &context.diff_paths,
                manifest,
            ))
        }
        CheckKind::GraphResolves => {
            let before = context.graph_before.as_ref()?;
            let after = context.graph_after.as_ref()?;
            Some(structural::graph_resolves(before, after))
        }
        CheckKind::TestsTouched => Some(structural::tests_touched(
            &context.diff_paths,
            &context.test_conventions,
        )),
        CheckKind::Argv => None,
    }
}

/// Runs only contract checks; callers cannot submit a successful receipt.
///
/// `context` carries the facts for the structural kinds (`diff-in-scope`,
/// `graph-resolves`, `tests-touched`); `None` leaves every structural check
/// `Unavailable`. The private workspace is materialized lazily: structural
/// checks never touch it.
pub(crate) fn verify_with_lease(
    snapshot: &SourceSnapshot,
    contract: &TaskContract,
    checks: &[Check],
    run_id: &str,
    lease_path: Option<&Path>,
    context: Option<&StructuralContext>,
) -> Result<Vec<VerificationReceipt>> {
    let mut receipts = Vec::with_capacity(checks.len());
    let mut workspace: Option<tempfile::TempDir> = None;
    for check in checks {
        let started_ms = now_ms();
        let started = Instant::now();
        if check.kind != CheckKind::Argv {
            let result = structural_verdict(check.kind, context);
            let outcome = match &result {
                Some(result) if result.passed && result.complete => CheckOutcome::Passed,
                Some(_) => CheckOutcome::Failed,
                None => CheckOutcome::Unavailable,
            };
            let diagnostic = match &result {
                Some(result) => {
                    let mut parts: Vec<String> = result.witnesses.clone();
                    if let Some(note) = &result.note {
                        parts.push(note.clone());
                    }
                    (!parts.is_empty()).then(|| parts.join("; "))
                }
                None => Some(format!(
                    "{} check needs structural facts the verify caller did not gather",
                    check.kind.as_str()
                )),
            };
            receipts.push(VerificationReceipt {
                run_id: run_id.to_string(),
                check_id: check.id.clone(),
                source_id: snapshot.content_id.clone(),
                contract_id: contract.id()?,
                check_digest: structural_check_identity(check)?,
                outcome,
                exit_code: None,
                started_ms,
                finished_ms: now_ms(),
                duration_ms: started.elapsed().as_millis() as u64,
                stdout_sha256: hex::encode(Sha256::digest(b"")),
                stderr_sha256: hex::encode(Sha256::digest(b"")),
                stdout_bytes: 0,
                stderr_bytes: 0,
                execution_root: String::new(),
                diagnostic,
                structural: result,
            });
            continue;
        }
        if workspace.is_none() {
            workspace = Some(snapshot::materialize(snapshot)?);
        }
        let workspace = workspace.as_ref().expect("workspace materialized");
        let source_marker = snapshot::mutation_marker(workspace.path(), snapshot)?;
        let execution = execution(check, contract)?;
        let run = execution.run_confined(
            workspace.path(),
            &workspace.path().join(&check.cwd),
            Duration::from_millis(check.timeout_ms),
            lease_path,
        )?;
        let intact = snapshot::unchanged(workspace.path(), snapshot, contract)?
            && snapshot::mutation_marker(workspace.path(), snapshot)? == source_marker;
        let identity_current = contract_check_identity(check, contract)
            .is_ok_and(|identity| identity == execution.identity());
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
            check_digest: execution.identity().to_owned(),
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
            structural: None,
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
    use crate::CallEdge;
    use sha2::Digest;
    use std::fs;

    fn check(script: &str) -> Check {
        Check {
            id: "check".into(),
            kind: CheckKind::Argv,
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
            None,
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
        let receipts =
            verify_with_lease(&captured, &configured, &[command], "run", None, None).unwrap();
        assert_eq!(receipts[0].outcome, CheckOutcome::Unavailable);
        assert_eq!(
            receipts[0].diagnostic.as_deref(),
            Some("check execution environment changed during verification")
        );
    }

    #[test]
    fn contract_identity_binds_the_whole_check_not_only_its_argv() {
        // Receipts recorded before the sandbox extraction digested the whole
        // check; an argv-only identity would mark every one `Unavailable`.
        let configured = contract();
        let base = check("true");
        let identity = contract_check_identity(&base, &configured).unwrap();
        let argv_only = sandbox::execution(&base.argv, &base.argv, &configured.toolchain).unwrap();
        assert_ne!(identity, argv_only.identity());
        let mut renamed = base.clone();
        renamed.id = "other".into();
        let mut moved = base.clone();
        moved.cwd = "sub".into();
        let mut slower = base.clone();
        slower.timeout_ms += 1;
        let mut optional = base;
        optional.required = false;
        for changed in [renamed, moved, slower, optional] {
            assert_ne!(
                contract_check_identity(&changed, &configured).unwrap(),
                identity
            );
        }
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

    fn structural_check(kind: crate::model::CheckKind) -> Check {
        Check {
            id: "structural".into(),
            kind,
            argv: Vec::new(),
            cwd: ".".into(),
            timeout_ms: 1000,
            required: true,
        }
    }

    #[test]
    fn structural_check_without_context_is_never_available() {
        let root = repo();
        let configured = contract();
        let captured = snapshot::capture(root.path(), &configured, true).unwrap();
        let receipts = verify_with_lease(
            &captured,
            &configured,
            &[
                structural_check(CheckKind::DiffInScope),
                structural_check(CheckKind::GraphResolves),
                structural_check(CheckKind::TestsTouched),
            ],
            "run",
            None,
            None,
        )
        .unwrap();
        assert_eq!(receipts.len(), 3);
        for receipt in &receipts {
            assert_eq!(receipt.outcome, CheckOutcome::Unavailable);
            assert!(receipt.structural.is_none());
            assert!(
                receipt
                    .diagnostic
                    .as_deref()
                    .is_some_and(|d| d.contains("did not gather")),
                "{:?}",
                receipt.diagnostic
            );
        }
    }

    #[test]
    fn diff_in_scope_check_reports_witnesses_and_complete_flag() {
        let root = repo();
        let configured = contract();
        let captured = snapshot::capture(root.path(), &configured, true).unwrap();
        let context = StructuralContext {
            diff_paths: vec!["src/a.rs".into(), "docs/b.md".into()],
            manifest_paths: Some(vec!["src/a.rs".into()]),
            ..StructuralContext::default()
        };
        let receipts = verify_with_lease(
            &captured,
            &configured,
            &[structural_check(CheckKind::DiffInScope)],
            "run",
            None,
            Some(&context),
        )
        .unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].outcome, CheckOutcome::Failed);
        let structural = receipts[0].structural.as_ref().expect("structural result");
        assert!(!structural.passed);
        assert!(structural.complete);
        assert_eq!(structural.witnesses, vec!["out of scope: docs/b.md".to_string()]);
    }

    #[test]
    fn diff_in_scope_check_passes_when_manifest_covers_the_diff() {
        let root = repo();
        let configured = contract();
        let captured = snapshot::capture(root.path(), &configured, true).unwrap();
        let context = StructuralContext {
            diff_paths: vec!["src/a.rs".into()],
            manifest_paths: Some(vec!["src/a.rs".into()]),
            ..StructuralContext::default()
        };
        let receipts = verify_with_lease(
            &captured,
            &configured,
            &[structural_check(CheckKind::DiffInScope)],
            "run",
            None,
            Some(&context),
        )
        .unwrap();
        assert_eq!(receipts[0].outcome, CheckOutcome::Passed);
        assert_eq!(receipts[0].exit_code, None);
        let structural = receipts[0].structural.as_ref().expect("structural result");
        assert!(structural.passed);
        assert!(structural.complete);
    }

    #[test]
    fn graph_resolves_check_names_dropped_edge_endpoints() {
        let root = repo();
        let configured = contract();
        let captured = snapshot::capture(root.path(), &configured, true).unwrap();
        let context = StructuralContext {
            graph_before: Some(vec![CallEdge {
                src: "src/a.rs::foo".into(),
                dst: "src/b.rs::bar".into(),
                site_line: 12,
            }]),
            graph_after: Some(Vec::new()),
            ..StructuralContext::default()
        };
        let receipts = verify_with_lease(
            &captured,
            &configured,
            &[structural_check(CheckKind::GraphResolves)],
            "run",
            None,
            Some(&context),
        )
        .unwrap();
        assert_eq!(receipts[0].outcome, CheckOutcome::Failed);
        let structural = receipts[0].structural.as_ref().expect("structural result");
        assert_eq!(
            structural.witnesses,
            vec!["dropped edge: src/a.rs::foo -> src/b.rs::bar (site line 12)".to_string()]
        );
    }

    #[test]
    fn tests_touched_check_finds_a_bare_source_change() {
        let root = repo();
        let configured = contract();
        let captured = snapshot::capture(root.path(), &configured, true).unwrap();
        let context = StructuralContext {
            diff_paths: vec!["src/a.rs".into()],
            ..StructuralContext::default()
        };
        let receipts = verify_with_lease(
            &captured,
            &configured,
            &[structural_check(CheckKind::TestsTouched)],
            "run",
            None,
            Some(&context),
        )
        .unwrap();
        assert_eq!(receipts[0].outcome, CheckOutcome::Failed);
        let structural = receipts[0].structural.as_ref().expect("structural result");
        assert_eq!(
            structural.witnesses,
            vec!["non-test change without any test change: src/a.rs".to_string()]
        );
    }

    #[test]
    fn structural_checks_never_touch_the_private_workspace() {
        // A structural-only run leaves the captured source exactly as it was:
        // no workspace materialization, no source mutation check.
        let root = repo();
        let configured = contract();
        let captured = snapshot::capture(root.path(), &configured, true).unwrap();
        let context = StructuralContext {
            diff_paths: vec!["src/a.rs".into()],
            manifest_paths: Some(vec!["src/a.rs".into()]),
            ..StructuralContext::default()
        };
        let receipts = verify_with_lease(
            &captured,
            &configured,
            &[structural_check(CheckKind::DiffInScope)],
            "run",
            None,
            Some(&context),
        )
        .unwrap();
        assert_eq!(receipts[0].outcome, CheckOutcome::Passed);
        assert_eq!(receipts[0].execution_root, "");
        assert!(snapshot::unchanged(root.path(), &captured, &configured).unwrap());
    }
}
