// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::{Arc, Barrier};

use pixel_task::{Action, CheckOutcome, Error, Gate, Phase, Store, TaskContract, snapshot};
use serde_json::json;

use super::support::{contract, git, observations, repo};

#[test]
fn completion_requires_actual_current_receipts_and_review() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let task = store
        .begin(
            contract("test \"$(cat source.txt)\" = correct"),
            "pi",
            Some("session"),
            "begin",
        )
        .unwrap();
    assert!(!store.decision(&task.task_id, Gate::Edit).unwrap().allowed);
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    assert!(store.decision(&task.task_id, Gate::Edit).unwrap().allowed);
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "claim",
            Action::Claim {
                text: "all checks pass".into(),
            },
        )
        .unwrap();
    assert!(!store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
    let task = store.verify(&task.task_id, &[], "verify").unwrap();
    assert_eq!(task.phase, Phase::Reviewing);
    assert_eq!(task.receipts.len(), 1);
    assert_eq!(
        task.receipts[0].outcome,
        CheckOutcome::Passed,
        "{:?}",
        task.receipts[0].diagnostic
    );
    assert_eq!(task.receipts[0].exit_code, Some(0));
    assert_ne!(
        task.receipts[0].execution_root,
        directory.path().display().to_string()
    );
    assert!(!store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "review",
            Action::Review {
                passed: true,
                findings: Vec::new(),
            },
        )
        .unwrap();
    assert!(store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
    let task = store
        .update(&task.task_id, task.revision, "finish", Action::Finish)
        .unwrap();
    assert_eq!(task.phase, Phase::Complete);
    fs::write(directory.path().join("source.txt"), "wrong\n").unwrap();
    assert!(!store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
}

#[test]
fn failed_and_timed_out_checks_never_satisfy_acceptance() {
    for (script, expected) in [
        ("exit 7", CheckOutcome::Failed),
        ("sleep 5", CheckOutcome::TimedOut),
    ] {
        let directory = repo();
        let store = Store::open(directory.path()).unwrap();
        let mut configured = contract(script);
        configured.checks[0].timeout_ms = 50;
        let task = store.begin(configured, "pi", None, "begin").unwrap();
        let task = store
            .update(
                &task.task_id,
                task.revision,
                "prepare",
                Action::Prepare {
                    observations: observations(),
                },
            )
            .unwrap();
        let task = store.verify(&task.task_id, &[], "verify").unwrap();
        assert_eq!(task.receipts[0].outcome, expected);
        assert!(!store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
    }
}

#[test]
fn check_source_mutation_stays_private_and_invalidates_receipt() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let task = store
        .begin(contract("printf wrong > source.txt"), "pi", None, "begin")
        .unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let task = store.verify(&task.task_id, &[], "verify").unwrap();
    assert_eq!(task.receipts[0].outcome, CheckOutcome::SourceChanged);
    assert_eq!(
        fs::read_to_string(directory.path().join("source.txt")).unwrap(),
        "correct\n"
    );
    assert!(!store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
}

#[test]
fn undeclared_check_outputs_invalidate_receipts_without_changing_captured_files() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let task = store
        .begin(
            contract("printf generated > unexpected.txt"),
            "pi",
            None,
            "begin",
        )
        .unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let task = store.verify(&task.task_id, &[], "verify").unwrap();
    assert_eq!(task.receipts.len(), 1);
    assert_eq!(task.receipts[0].outcome, CheckOutcome::SourceChanged);
    assert!(!store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
    assert!(!directory.path().join("unexpected.txt").exists());
    assert_eq!(
        fs::read_to_string(directory.path().join("source.txt")).unwrap(),
        "correct\n"
    );
}

#[test]
fn explicit_outputs_are_isolated_but_never_hide_tracked_source() {
    let directory = repo();
    let mut configured = contract("mkdir -p output; printf generated > output/result");
    configured.outputs.push("output".into());
    let store = Store::open(directory.path()).unwrap();
    let task = store
        .begin(configured.clone(), "pi", None, "begin")
        .unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let task = store.verify(&task.task_id, &[], "verify").unwrap();
    assert_eq!(task.receipts[0].outcome, CheckOutcome::Passed);
    assert!(!directory.path().join("output").exists());
    configured.outputs = vec!["source.txt".into()];
    assert!(snapshot::capture(directory.path(), &configured, true).is_err());
}

#[test]
fn source_identity_matches_copies_and_includes_git_staging() {
    let directory = repo();
    let configured = contract("true");
    let before = snapshot::capture(directory.path(), &configured, true).unwrap();
    let copied = snapshot::materialize(&before).unwrap();
    let copy = snapshot::capture(copied.path(), &configured, true).unwrap();
    assert_eq!(before.content_id, copy.content_id);
    assert_ne!(before.root, copy.root);
    fs::write(directory.path().join("source.txt"), "modified\n").unwrap();
    let dirty = snapshot::capture(directory.path(), &configured, false).unwrap();
    assert_ne!(before.content_id, dirty.content_id);
    git(directory.path(), &["add", "source.txt"]);
    let staged = snapshot::capture(directory.path(), &configured, true).unwrap();
    assert_ne!(staged.content_id, dirty.content_id);
    assert_ne!(staged.index_id, dirty.index_id);
}

#[test]
fn source_identity_covers_untracked_deleted_and_executable_inputs() {
    let directory = repo();
    let configured = contract("true");
    let baseline = snapshot::capture(directory.path(), &configured, true).unwrap();
    fs::write(directory.path().join("new.txt"), "new").unwrap();
    let new = snapshot::capture(directory.path(), &configured, false).unwrap();
    assert_ne!(baseline.content_id, new.content_id);
    fs::set_permissions(
        directory.path().join("new.txt"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let executable = snapshot::capture(directory.path(), &configured, false).unwrap();
    assert_ne!(new.content_id, executable.content_id);
    fs::remove_file(directory.path().join("source.txt")).unwrap();
    let deleted = snapshot::capture(directory.path(), &configured, true).unwrap();
    assert_ne!(deleted.content_id, executable.content_id);
}

#[test]
fn external_symlinks_and_unavailable_declared_inputs_fail_closed() {
    let directory = repo();
    let mut configured = contract("true");
    configured.inputs.push("missing".into());
    assert!(snapshot::capture(directory.path(), &configured, true).is_err());
    configured.inputs.clear();
    symlink("/etc/passwd", directory.path().join("escape")).unwrap();
    assert!(snapshot::capture(directory.path(), &configured, true).is_err());
}

#[test]
fn idempotent_requests_replay_without_duplicate_effects() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let task = store.begin(contract("true"), "pi", None, "begin").unwrap();
    assert_eq!(
        store.begin(contract("true"), "pi", None, "begin").unwrap(),
        task
    );
    assert!(matches!(
        store.begin(contract("false"), "pi", None, "begin"),
        Err(Error::Idempotency(_))
    ));
    let prepared = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    assert_eq!(
        store
            .update(
                &task.task_id,
                task.revision,
                "prepare",
                Action::Prepare {
                    observations: observations()
                }
            )
            .unwrap(),
        prepared
    );
    assert!(matches!(
        store.update(&task.task_id, task.revision, "prepare", Action::Edited),
        Err(Error::Idempotency(_))
    ));
    let verified = store.verify(&task.task_id, &[], "verify").unwrap();
    assert_eq!(
        store.verify(&task.task_id, &[], "verify").unwrap(),
        verified
    );
    assert_eq!(
        store
            .events(&task.task_id)
            .unwrap()
            .iter()
            .filter(|event| event.kind == "verification_started")
            .count(),
        1
    );
}

#[test]
fn concurrent_updates_never_lose_an_acknowledged_event() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let task = store.begin(contract("true"), "pi", None, "begin").unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = (0..2)
        .map(|i| {
            let barrier = barrier.clone();
            let store = store.clone();
            let task = task.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.update(
                    &task.task_id,
                    task.revision,
                    &format!("claim-{i}"),
                    Action::Claim {
                        text: format!("claim-{i}"),
                    },
                )
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(store.status(&task.task_id).unwrap().claims.len(), 1);
    assert_eq!(store.events(&task.task_id).unwrap().len(), 2);
}

#[test]
fn corrupt_authority_never_falls_back_to_successful_cached_view() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let task = store.begin(contract("true"), "pi", None, "begin").unwrap();
    let journal = directory
        .path()
        .join(".pixel/tasks")
        .join(&task.task_id)
        .join("journal.jsonl");
    fs::write(journal, "not json\n").unwrap();
    assert!(matches!(
        store.status(&task.task_id),
        Err(Error::Corrupt(_))
    ));
    assert!(store.decision(&task.task_id, Gate::Edit).is_err());
}

#[test]
fn torn_tail_is_not_an_event_and_can_be_recovered_by_next_transaction() {
    use std::io::Write;
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let task = store.begin(contract("true"), "pi", None, "begin").unwrap();
    let journal = directory
        .path()
        .join(".pixel/tasks")
        .join(&task.task_id)
        .join("journal.jsonl");
    fs::OpenOptions::new()
        .append(true)
        .open(journal)
        .unwrap()
        .write_all(b"{\"partial\":")
        .unwrap();
    assert_eq!(store.status(&task.task_id).unwrap(), task);
    let updated = store
        .update(
            &task.task_id,
            task.revision,
            "claim",
            Action::Claim {
                text: "claim".into(),
            },
        )
        .unwrap();
    assert_eq!(updated.revision, 2);
    assert_eq!(store.events(&task.task_id).unwrap().len(), 2);
}

#[test]
fn legacy_migration_preserves_claims_and_status_but_grants_no_authority() {
    let directory = repo();
    let path = directory.path().join(".pixel/tasks/task-old");
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("task.json"), serde_json::to_vec(&json!({"version":1,"task_id":"task-old","provider":"claude","session_id":"session","status":"accepted","created_unix":100,"updated_unix":100,"spec":{"objective":"legacy"},"snapshot":{"head_oid":"abc"},"model_claims":[{"text":"done"}]})).unwrap()).unwrap();
    let store = Store::open(directory.path()).unwrap();
    let task = store.status("task-old").unwrap();
    assert_eq!(task.legacy_status.as_deref(), Some("accepted"));
    assert_eq!(task.claims, ["done"]);
    assert!(task.receipts.is_empty());
    assert!(!store.decision("task-old", Gate::Finish).unwrap().allowed);
    let updated = store
        .update(
            "task-old",
            0,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    assert_eq!(updated.revision, 1);
    assert_eq!(store.status("task-old").unwrap(), updated);
}

#[test]
fn criterion_strengthening_is_allowed_but_waivers_are_not() {
    let mut old = contract("true");
    old.criteria[0].checks.clear();
    let mut new = old.clone();
    new.criteria[0].checks.push("value".into());
    assert!(new.preserves(&old));
    assert!(!old.preserves(&new));
    let mut weakened = new.clone();
    weakened.checks[0].required = false;
    assert!(!weakened.preserves(&old));
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let task = store.begin(old, "pi", None, "begin").unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "strengthen",
            Action::SetContract {
                contract: new.clone(),
                human_authorized: false,
            },
        )
        .unwrap();
    assert_eq!(task.contract, new);
    assert!(matches!(
        store.update(
            &task.task_id,
            task.revision,
            "weaken",
            Action::SetContract {
                contract: weakened.clone(),
                human_authorized: false
            }
        ),
        Err(Error::Blocked(_))
    ));
    assert_eq!(store.status(&task.task_id).unwrap(), task);
    let authorized = store
        .update(
            &task.task_id,
            task.revision,
            "authorized-weaken",
            Action::SetContract {
                contract: weakened.clone(),
                human_authorized: true,
            },
        )
        .unwrap();
    assert_eq!(authorized.contract, weakened);
    assert_eq!(authorized.revision, task.revision + 1);
}

#[test]
fn bad_contract_paths_commands_ids_and_conservative_mappings_are_rejected() {
    for bad in ["../outside", "/absolute", ""] {
        let mut configured = contract("true");
        configured.checks[0].cwd = bad.into();
        assert!(configured.validate().is_err());
    }
    let mut configured = contract("true");
    configured.conservative_checks.push("missing".into());
    assert!(configured.validate().is_err());
    configured.conservative_checks = vec!["value".into()];
    assert!(configured.validate().is_ok());
    configured.outputs = vec!["input".into()];
    configured.inputs = vec!["input/file".into()];
    assert!(configured.validate().is_err());
    configured = contract("true");
    configured.checks[0].argv.clear();
    assert!(configured.validate().is_err());
}

#[test]
fn continuation_budget_survives_reopening_and_session_lookup_is_exact() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let mut task = store
        .begin(contract("true"), "pi", Some("session-a"), "begin")
        .unwrap();
    for i in 0..2 {
        task = store
            .update(
                &task.task_id,
                task.revision,
                &format!("correction-{i}"),
                Action::Correction {
                    state_key: "unchanged".into(),
                },
            )
            .unwrap();
    }
    let reopened = Store::open(directory.path()).unwrap();
    assert!(matches!(
        reopened.update(
            &task.task_id,
            task.revision,
            "third-same",
            Action::Correction {
                state_key: "unchanged".into()
            }
        ),
        Err(Error::Blocked(_))
    ));
    task = reopened
        .update(
            &task.task_id,
            task.revision,
            "third",
            Action::Correction {
                state_key: "changed".into(),
            },
        )
        .unwrap();
    assert!(matches!(
        reopened.update(
            &task.task_id,
            task.revision,
            "fourth",
            Action::Correction {
                state_key: "another".into()
            }
        ),
        Err(Error::Blocked(_))
    ));
    assert_eq!(
        reopened
            .find_session("pi", "session-a")
            .unwrap()
            .unwrap()
            .task_id,
        task.task_id
    );
    assert!(
        reopened
            .find_session("claude", "session-a")
            .unwrap()
            .is_none()
    );
    assert!(reopened.find_session("pi", "session-b").unwrap().is_none());
}

#[test]
fn missing_checks_block_edits_while_unmapped_criteria_only_block_finish() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let configured = TaskContract {
        objective: "task".into(),
        ..TaskContract::default()
    };
    let task = store.begin(configured, "pi", None, "empty").unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    assert!(!store.decision(&task.task_id, Gate::Edit).unwrap().allowed);
    let mut configured = contract("true");
    configured.criteria[0].checks.clear();
    let task = store.begin(configured, "pi", None, "unmapped").unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    assert!(store.decision(&task.task_id, Gate::Edit).unwrap().allowed);
    assert!(!store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
}

#[test]
fn cancelled_tasks_stay_bound_and_cannot_be_reopened_by_contract_update() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let task = store
        .begin(contract("true"), "pi", Some("session"), "begin")
        .unwrap();
    let cancelled = store
        .update(&task.task_id, task.revision, "cancel", Action::Cancel)
        .unwrap();
    assert_eq!(
        store.find_session("pi", "session").unwrap().unwrap().phase,
        Phase::Cancelled
    );
    assert!(matches!(
        store.update(
            &task.task_id,
            cancelled.revision,
            "reopen",
            Action::SetContract {
                contract: cancelled.contract,
                human_authorized: false
            }
        ),
        Err(Error::Blocked(_))
    ));
    assert!(!store.decision(&task.task_id, Gate::Edit).unwrap().allowed);
}

#[test]
fn unavailable_latest_run_invalidates_an_earlier_success_for_same_source() {
    let directory = repo();
    let executable_dir = tempfile::tempdir().unwrap();
    let executable = executable_dir.path().join("check");
    fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let mut configured = contract("true");
    configured.require_review = false;
    configured.checks[0].argv = vec![executable.display().to_string()];
    let store = Store::open(directory.path()).unwrap();
    let task = store.begin(configured, "pi", None, "begin").unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let task = store.verify(&task.task_id, &[], "first").unwrap();
    assert_eq!(task.receipts.last().unwrap().outcome, CheckOutcome::Passed);
    fs::remove_file(&executable).unwrap();
    let failed = store.verify(&task.task_id, &[], "second").unwrap();
    assert_eq!(failed.receipts.len(), 2);
    assert_eq!(failed.receipts[1].outcome, CheckOutcome::Unavailable);
    assert_eq!(failed.receipts[1].exit_code, None);
    assert!(
        !pixel_task::policy::decide(
            &failed,
            Gate::Finish,
            failed
                .source
                .as_ref()
                .map(|source| source.content_id.as_str())
        )
        .allowed
    );
}

#[test]
fn source_write_and_restore_during_check_is_not_certified() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let task = store
        .begin(
            contract("printf wrong > source.txt; printf 'correct\\n' > source.txt"),
            "pi",
            None,
            "begin",
        )
        .unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let task = store.verify(&task.task_id, &[], "verify").unwrap();
    assert_eq!(task.receipts[0].outcome, CheckOutcome::SourceChanged);
    assert_eq!(
        fs::read_to_string(directory.path().join("source.txt")).unwrap(),
        "correct\n"
    );
}

#[test]
fn journal_reconstructs_state_without_cached_view_but_rejects_corrupt_manifest() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let task = store.begin(contract("true"), "pi", None, "begin").unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let root = directory.path().join(".pixel/tasks");
    fs::remove_file(root.join(&task.task_id).join("task.json")).unwrap();
    assert_eq!(store.status(&task.task_id).unwrap(), task);
    let source = task.source.unwrap();
    fs::write(
        root.join("source-manifests")
            .join(format!("{}.json", source.content_id)),
        "[]",
    )
    .unwrap();
    assert!(matches!(
        store.status(&task.task_id),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn initial_unconfigured_contract_can_add_outputs_but_obligations_freeze_them() {
    let prior = TaskContract {
        objective: "task".into(),
        ..TaskContract::default()
    };
    let mut configured = prior.clone();
    configured.checks = contract("true").checks;
    configured.outputs = vec!["dist".into()];
    assert!(configured.preserves(&prior));
    let mut broadened = configured.clone();
    broadened.outputs.push("generated".into());
    assert!(!broadened.preserves(&configured));
}

#[test]
fn preparation_rejects_observations_from_an_older_source_and_unknown_graph() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let task = store.begin(contract("true"), "pi", None, "begin").unwrap();
    let source = snapshot::capture(directory.path(), &task.contract, true).unwrap();
    let mut evidence = observations();
    for observation in &mut evidence {
        observation.source_id.clone_from(&source.content_id);
    }
    fs::write(directory.path().join("source.txt"), "changed").unwrap();
    assert!(matches!(
        store.update(
            &task.task_id,
            task.revision,
            "stale",
            Action::Prepare {
                observations: evidence
            }
        ),
        Err(Error::Blocked(_))
    ));
    let mut evidence = observations();
    evidence[1].complete = false;
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "partial",
            Action::Prepare {
                observations: evidence,
            },
        )
        .unwrap();
    assert!(!store.decision(&task.task_id, Gate::Edit).unwrap().allowed);
}

#[test]
fn interrupted_spawn_is_unknown_and_live_process_groups_are_not_replayed() {
    let directory = tempfile::tempdir().unwrap();
    let lease = directory.path().join("run.json");
    fs::write(&lease, r#"{"state":"starting","process_group":null}"#).unwrap();
    assert!(matches!(
        pixel_task::runner::check_recovery(&lease),
        Err(Error::Blocked(_))
    ));
    // SAFETY: getpgrp only reads this test process's process-group identity.
    let group = unsafe { libc::getpgrp() };
    fs::write(
        &lease,
        serde_json::to_vec(&json!({"state":"running","process_group":group})).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        pixel_task::runner::check_recovery(&lease),
        Err(Error::Busy(_))
    ));
    fs::write(&lease, r#"{"state":"finished","process_group":null}"#).unwrap();
    assert!(pixel_task::runner::check_recovery(&lease).is_ok());
}

#[test]
fn event_clock_is_a_real_wall_clock_observation() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let value = pixel_task::now_ms();
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(value >= before && value <= after);
    assert!(value > 1_577_836_800_000);
}

#[test]
fn inherited_environment_changes_invalidate_finish_without_persisting_secrets() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let mut configured = contract("test \"$FEATURE\" = pass");
    configured.require_review = false;
    let task = store.begin(configured, "pi", Some("env"), "begin").unwrap();
    store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    for (feature, attempt) in [("pass", "first"), ("pass", "second"), ("fail", "third")] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "core::environment_freshness_child", "--ignored"])
            .env("PIXEL_TASK_ENV_TEST_ROOT", directory.path())
            .env("PIXEL_TASK_ATTEMPT_ID", attempt)
            .env(
                "PIXEL_ENV_SECRET",
                "private-sentinel-must-never-be-persisted",
            )
            .env("FEATURE", feature)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    let journal = fs::read_to_string(
        directory
            .path()
            .join(".pixel/tasks")
            .join(&task.task_id)
            .join("journal.jsonl"),
    )
    .unwrap();
    assert!(!journal.contains("private-sentinel-must-never-be-persisted"));
}

#[test]
#[ignore = "subprocess fixture for isolated process environments"]
fn environment_freshness_child() {
    let root = std::env::var_os("PIXEL_TASK_ENV_TEST_ROOT").unwrap();
    let store = Store::open(std::path::Path::new(&root)).unwrap();
    let mut task = store.find_session("pi", "env").unwrap().unwrap();
    if task.receipts.is_empty() {
        task = store.verify(&task.task_id, &[], "verify").unwrap();
        assert_eq!(task.receipts[0].outcome, CheckOutcome::Passed);
    }
    let expected = std::env::var("FEATURE").unwrap() == "pass";
    assert_eq!(
        store.decision(&task.task_id, Gate::Finish).unwrap().allowed,
        expected
    );
    if !expected {
        assert!(matches!(
            store.update(&task.task_id, task.revision, "finish", Action::Finish),
            Err(Error::Blocked(_))
        ));
    }
}

#[test]
fn declared_toolchain_bytes_are_enforced_at_run_and_finish() {
    use sha2::{Digest, Sha256};
    let directory = repo();
    let tools = tempfile::tempdir().unwrap();
    let binary = tools.path().join("child-tool");
    fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let mut configured = contract("true");
    configured.require_review = false;
    configured.toolchain.insert(
        binary.display().to_string(),
        hex::encode(Sha256::digest(fs::read(&binary).unwrap())),
    );
    let store = Store::open(directory.path()).unwrap();
    let task = store.begin(configured, "pi", None, "begin").unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let task = store.verify(&task.task_id, &[], "verify").unwrap();
    assert_eq!(task.receipts[0].outcome, CheckOutcome::Passed);
    assert!(store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
    fs::write(&binary, "#!/bin/sh\nexit 7\n").unwrap();
    assert!(!store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
    let failed = store
        .verify(&task.task_id, &[], "changed-toolchain")
        .unwrap();
    assert_eq!(
        failed.receipts.last().unwrap().outcome,
        CheckOutcome::Unavailable
    );
}

#[test]
fn verification_preserves_real_git_diffs_and_never_cleans_away_failures() {
    let directory = repo();
    fs::write(
        directory.path().join("source.txt"),
        "wrong trailing space \n",
    )
    .unwrap();
    for staged in [false, true] {
        if staged {
            git(directory.path(), &["add", "source.txt"]);
        }
        let store = Store::open(directory.path()).unwrap();
        let mut configured = contract(if staged {
            "git diff --cached --check"
        } else {
            "git diff --check"
        });
        configured.require_review = false;
        let task = store
            .begin(
                configured,
                "pi",
                None,
                if staged { "staged" } else { "dirty" },
            )
            .unwrap();
        let task = store
            .update(
                &task.task_id,
                task.revision,
                "prepare",
                Action::Prepare {
                    observations: observations(),
                },
            )
            .unwrap();
        let task = store.verify(&task.task_id, &[], "verify").unwrap();
        assert_eq!(
            task.receipts[0].outcome,
            CheckOutcome::Failed,
            "{:?}",
            task.receipts[0].diagnostic
        );
        assert!(!store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
    }
}

#[test]
fn private_git_preserves_remote_refs_and_ref_changes_invalidate_evidence() {
    let directory = repo();
    git(
        directory.path(),
        &["update-ref", "refs/remotes/origin/main", "HEAD"],
    );
    git(
        directory.path(),
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    fs::write(directory.path().join("source.txt"), "changed\n").unwrap();
    git(directory.path(), &["add", "source.txt"]);
    git(
        directory.path(),
        &["-c", "commit.gpgsign=false", "commit", "-qm", "change"],
    );
    let store = Store::open(directory.path()).unwrap();
    let mut configured = contract(
        "test \"$(git diff --name-only origin/main)\" = source.txt && test \"$(git symbolic-ref refs/remotes/origin/HEAD)\" = refs/remotes/origin/main",
    );
    configured.require_review = false;
    let task = store.begin(configured, "pi", None, "begin").unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let task = store.verify(&task.task_id, &[], "verify").unwrap();
    assert_eq!(
        task.receipts[0].outcome,
        CheckOutcome::Passed,
        "{:?}",
        task.receipts[0].diagnostic
    );
    assert!(store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
    git(
        directory.path(),
        &["update-ref", "refs/remotes/origin/main", "HEAD"],
    );
    assert!(!store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
}

#[test]
fn inherited_git_routing_cannot_redirect_private_capture_into_live_checkout() {
    let directory = repo();
    let foreign = repo();
    fs::write(directory.path().join("source.txt"), "dirty\n").unwrap();
    let before = snapshot::capture(directory.path(), &contract("true"), true).unwrap();
    let foreign_before = snapshot::capture(foreign.path(), &contract("true"), true).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "core::git_routing_child", "--ignored"])
        .env("PIXEL_TASK_GIT_TEST_ROOT", directory.path())
        .env("GIT_DIR", foreign.path().join(".git"))
        .env("GIT_WORK_TREE", foreign.path())
        .env("GIT_INDEX_FILE", foreign.path().join(".git/index"))
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.worktree")
        .env("GIT_CONFIG_VALUE_0", foreign.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        snapshot::capture(directory.path(), &contract("true"), true)
            .unwrap()
            .content_id,
        before.content_id
    );
    assert_eq!(
        snapshot::capture(foreign.path(), &contract("true"), true)
            .unwrap()
            .content_id,
        foreign_before.content_id
    );
}

#[test]
#[ignore = "subprocess fixture for isolated Git routing"]
fn git_routing_child() {
    let root = std::env::var_os("PIXEL_TASK_GIT_TEST_ROOT").unwrap();
    let store = Store::open(std::path::Path::new(&root)).unwrap();
    let task = store
        .begin(
            contract("test \"$(cat source.txt)\" = dirty && test -n \"$(git diff --name-only)\""),
            "pi",
            None,
            "begin",
        )
        .unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let task = store.verify(&task.task_id, &[], "verify").unwrap();
    assert_eq!(
        task.receipts[0].outcome,
        CheckOutcome::Passed,
        "{:?}",
        task.receipts[0].diagnostic
    );
}

#[test]
fn every_independent_receipt_and_review_obligation_blocks_completion() {
    use pixel_task::{Route, Task};
    type GateMutation = (&'static str, Box<dyn Fn(&mut Task)>);
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let task = store.begin(contract("true"), "pi", None, "begin").unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let task = store.verify(&task.task_id, &[], "verify").unwrap();
    let ready = store
        .update(
            &task.task_id,
            task.revision,
            "review",
            Action::Review {
                passed: true,
                findings: vec![],
            },
        )
        .unwrap();
    let decide = |task: &Task, gate| {
        pixel_task::policy::decide(
            task,
            gate,
            ready
                .source
                .as_ref()
                .map(|source| source.content_id.as_str()),
        )
    };
    assert_eq!(
        decide(&ready, Gate::Finish).eligible_routes,
        vec![Route::Investigate, Route::Recover, Route::Finish]
    );
    assert_eq!(
        decide(&ready, Gate::Edit).eligible_routes,
        vec![Route::Investigate, Route::Recover, Route::Edit]
    );
    let mutations: Vec<GateMutation> = vec![
        (
            "receipt source",
            Box::new(|task| task.receipts[0].source_id = "other".into()),
        ),
        (
            "receipt contract",
            Box::new(|task| task.receipts[0].contract_id = "other".into()),
        ),
        (
            "receipt check",
            Box::new(|task| task.receipts[0].check_id = "other".into()),
        ),
        (
            "receipt nonzero",
            Box::new(|task| task.receipts[0].exit_code = Some(1)),
        ),
        (
            "receipt missing exit",
            Box::new(|task| task.receipts[0].exit_code = None),
        ),
        (
            "review failure",
            Box::new(|task| task.review.as_mut().unwrap().passed = false),
        ),
        (
            "review finding",
            Box::new(|task| {
                task.review
                    .as_mut()
                    .unwrap()
                    .findings
                    .push("unresolved".into())
            }),
        ),
        (
            "review source",
            Box::new(|task| task.review.as_mut().unwrap().source_id = "other".into()),
        ),
        (
            "review contract",
            Box::new(|task| task.review.as_mut().unwrap().contract_id = "other".into()),
        ),
        (
            "missing scope",
            Box::new(|task| task.observations.remove(0).source_id.clear()),
        ),
        (
            "legacy status",
            Box::new(|task| task.legacy_status = Some("accepted".into())),
        ),
    ];
    for (label, mutate) in mutations {
        let mut task = ready.clone();
        mutate(&mut task);
        assert!(!decide(&task, Gate::Finish).allowed, "{label}");
    }
    let mut task = ready.clone();
    let mut later_failure = task.receipts[0].clone();
    later_failure.outcome = CheckOutcome::Failed;
    task.receipts.push(later_failure);
    assert!(!decide(&task, Gate::Finish).allowed);
    let mut task = ready.clone();
    task.phase = Phase::Complete;
    assert!(!decide(&task, Gate::Edit).allowed);
    task.phase = Phase::Cancelled;
    assert_eq!(
        decide(&task, Gate::Finish).eligible_routes,
        vec![Route::Investigate, Route::Recover]
    );
    assert!(!decide(&task, Gate::Finish).allowed);
}

#[test]
fn required_checks_and_task_criteria_are_independent_obligations() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let mut configured = contract("true");
    configured.require_review = false;
    let mut extra = configured.checks[0].clone();
    extra.id = "mandatory-unmapped".into();
    extra.argv[2] = "exit 7".into();
    configured.checks.push(extra);
    let task = store
        .begin(configured.clone(), "pi", None, "required")
        .unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let task = store
        .verify(&task.task_id, &["value".into()], "selected")
        .unwrap();
    assert_eq!(task.receipts.len(), 1);
    assert_eq!(task.receipts[0].check_id, "value");
    assert!(!store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
    assert!(matches!(
        store.verify(&task.task_id, &["unknown".into()], "unknown"),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        store.verify(
            &task.task_id,
            &["value".into(), "unknown".into()],
            "mixed-selection"
        ),
        Err(Error::Invalid(_))
    ));
    assert_eq!(store.status(&task.task_id).unwrap(), task);
    configured.checks[1].required = false;
    let task = store
        .begin(configured.clone(), "pi", None, "optional")
        .unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let task = store.verify(&task.task_id, &[], "all").unwrap();
    assert_eq!(task.receipts[1].outcome, CheckOutcome::Failed);
    assert!(store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
    configured.criteria[0]
        .checks
        .push("mandatory-unmapped".into());
    let task = store
        .begin(configured.clone(), "pi", None, "mapped-optional")
        .unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let task = store.verify(&task.task_id, &[], "all").unwrap();
    assert!(!store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
    configured.criteria.clear();
    let task = store.begin(configured, "pi", None, "no-criteria").unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let task = store.verify(&task.task_id, &[], "all").unwrap();
    assert!(!store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
}

#[test]
fn output_receipts_measure_exact_bytes_and_digest_for_each_stream() {
    use sha2::{Digest, Sha256};
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let task = store
        .begin(
            contract("printf stdout-value; printf stderr-value >&2"),
            "pi",
            None,
            "begin",
        )
        .unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let task = store.verify(&task.task_id, &[], "verify").unwrap();
    let receipt = &task.receipts[0];
    assert_eq!(receipt.outcome, CheckOutcome::Passed);
    assert_eq!(receipt.stdout_bytes, 12);
    assert_eq!(receipt.stderr_bytes, 12);
    assert_eq!(
        receipt.stdout_sha256,
        hex::encode(Sha256::digest(b"stdout-value"))
    );
    assert_eq!(
        receipt.stderr_sha256,
        hex::encode(Sha256::digest(b"stderr-value"))
    );
    assert!(receipt.finished_ms >= receipt.started_ms);
    assert!(receipt.duration_ms < 2000);
    assert!(!std::path::Path::new(&receipt.execution_root).exists());
}

#[test]
fn valid_json_journal_tampering_is_rejected_by_authority_checksum() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let task = store.begin(contract("true"), "pi", None, "begin").unwrap();
    let journal = directory
        .path()
        .join(".pixel/tasks")
        .join(&task.task_id)
        .join("journal.jsonl");
    let original = fs::read(&journal).unwrap();
    for field in ["task_id", "version", "revision", "previous", "phase"] {
        let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
        if field == "previous" {
            value["previous"] = json!("unexpected-parent");
        } else {
            value["task"][field] = match field {
                "task_id" => json!("task-other"),
                "version" => json!(99),
                "revision" => json!(99),
                _ => json!("complete"),
            };
        }
        if field != "phase" {
            // A valid checksum must not excuse a wrong identity, schema or
            // sequence. The phase-only case separately challenges the hash.
            let task: pixel_task::Task = serde_json::from_value(value["task"].clone()).unwrap();
            let event: pixel_task::TrajectoryEvent =
                serde_json::from_value(value["event"].clone()).unwrap();
            value["checksum"] = json!(
                pixel_task::digest(&(
                    value["request_id"].as_str().unwrap(),
                    value["input_id"].as_str().unwrap(),
                    value["previous"].as_str().unwrap(),
                    event,
                    task,
                ))
                .unwrap()
            );
        }
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        fs::write(&journal, bytes).unwrap();
        assert!(
            matches!(store.status(&task.task_id), Err(Error::Corrupt(_))),
            "{field}"
        );
    }
}

#[test]
fn recovering_interrupted_verification_never_resurrects_cancelled_task() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let task = store
        .begin(contract("true"), "pi", Some("cancelled"), "begin")
        .unwrap();
    let cancelled = store
        .update(&task.task_id, task.revision, "cancel", Action::Cancel)
        .unwrap();
    let mut exited_owner = std::process::Command::new("true").spawn().unwrap();
    let owner_pid = exited_owner.id();
    assert!(exited_owner.wait().unwrap().success());
    let journal = directory
        .path()
        .join(".pixel/tasks")
        .join(&task.task_id)
        .join("journal.jsonl");
    let text = fs::read_to_string(&journal).unwrap();
    let mut records: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let record = records.last_mut().unwrap();
    let mut interrupted: pixel_task::Task = serde_json::from_value(record["task"].clone()).unwrap();
    interrupted.running = Some(pixel_task::VerificationRun {
        run_id: "interrupted-before-spawn".into(),
        request_id: "verification".into(),
        source_id: String::new(),
        owner_pid,
        started_ms: pixel_task::now_ms(),
    });
    record["task"] = serde_json::to_value(&interrupted).unwrap();
    let event: pixel_task::TrajectoryEvent =
        serde_json::from_value(record["event"].clone()).unwrap();
    record["checksum"] = json!(
        pixel_task::digest(&(
            record["request_id"].as_str().unwrap(),
            record["input_id"].as_str().unwrap(),
            record["previous"].as_str().unwrap(),
            event,
            interrupted,
        ))
        .unwrap()
    );
    let mut bytes = Vec::new();
    for record in records {
        bytes.extend(serde_json::to_vec(&record).unwrap());
        bytes.push(b'\n');
    }
    fs::write(journal, bytes).unwrap();
    let recovered = store
        .update(
            &task.task_id,
            cancelled.revision,
            "recover",
            Action::Recover,
        )
        .unwrap();
    assert!(recovered.running.is_none());
    assert_eq!(recovered.phase, Phase::Cancelled);
    assert_eq!(
        store
            .find_session("pi", "cancelled")
            .unwrap()
            .unwrap()
            .phase,
        Phase::Cancelled
    );
    assert!(!store.decision(&task.task_id, Gate::Edit).unwrap().allowed);
    assert!(!store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
}

#[test]
fn output_limit_accepts_exact_cap_and_rejects_each_overflowing_stream() {
    const CAP: u64 = 16_777_216;
    for (bytes, stderr, expected) in [
        (CAP, false, CheckOutcome::Passed),
        (CAP + 1, false, CheckOutcome::Unavailable),
        (CAP + 1, true, CheckOutcome::Unavailable),
    ] {
        let directory = repo();
        fs::File::create(directory.path().join("payload"))
            .unwrap()
            .set_len(bytes)
            .unwrap();
        let store = Store::open(directory.path()).unwrap();
        let task = store
            .begin(
                contract(if stderr {
                    "cat payload >&2; sleep 5"
                } else if bytes > CAP {
                    "cat payload; sleep 5"
                } else {
                    "cat payload; sleep 0.05"
                }),
                "pi",
                None,
                "begin",
            )
            .unwrap();
        let task = store
            .update(
                &task.task_id,
                task.revision,
                "prepare",
                Action::Prepare {
                    observations: observations(),
                },
            )
            .unwrap();
        let task = store.verify(&task.task_id, &[], "verify").unwrap();
        let receipt = &task.receipts[0];
        assert_eq!(receipt.outcome, expected, "bytes={bytes}, stderr={stderr}");
        assert_eq!(receipt.stdout_bytes, if stderr { 0 } else { bytes });
        assert_eq!(receipt.stderr_bytes, if stderr { bytes } else { 0 });
        if bytes > CAP {
            assert!(receipt.duration_ms < 1_500);
            assert!(!store.decision(&task.task_id, Gate::Finish).unwrap().allowed);
        }
    }
}

#[test]
fn timeout_terminates_descendants_as_well_as_the_shell() {
    use std::time::{Duration, Instant};
    let directory = repo();
    let observer = tempfile::tempdir().unwrap();
    let pid_file = observer.path().join("descendant.pid");
    let mut configured = contract("sleep 10 & printf '%s' \"$!\" > \"$1\"; wait");
    configured.checks[0]
        .argv
        .extend(["pixel-check".into(), pid_file.display().to_string()]);
    configured.checks[0].timeout_ms = 300;
    let store = Store::open(directory.path()).unwrap();
    let task = store.begin(configured, "pi", None, "begin").unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let started = Instant::now();
    let task = store.verify(&task.task_id, &[], "verify").unwrap();
    assert_eq!(task.receipts[0].outcome, CheckOutcome::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(3));
    let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let terminated = loop {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&output.stdout);
        // A terminated orphan may briefly remain a zombie until init reaps it.
        if state.trim().is_empty() || state.trim().starts_with('Z') {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if !terminated {
        // SAFETY: this is the still-live descendant PID reported by our own
        // check, and cleanup prevents a failed assertion leaving it running.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    assert!(terminated, "timed-out check left descendant {pid} running");
}

#[test]
fn policy_routes_identify_configuration_preparation_and_each_verification_gap() {
    use pixel_task::Route;
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let mut configured = contract("true");
    configured.require_review = false;
    let task = store.begin(configured, "pi", None, "begin").unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let ready = store.verify(&task.task_id, &[], "verify").unwrap();
    let source = ready.source.as_ref().unwrap().content_id.as_str();
    let mut missing = ready.clone();
    missing.contract.checks.clear();
    missing.contract.criteria.clear();
    assert_eq!(
        pixel_task::policy::decide(&missing, Gate::Edit, Some(source)).eligible_routes,
        vec![Route::Investigate, Route::Recover, Route::Configure]
    );
    assert_eq!(
        pixel_task::policy::decide(&ready, Gate::Edit, Some("stale")).eligible_routes,
        vec![Route::Investigate, Route::Recover, Route::Prepare]
    );
    for gap in ["required-only", "criterion-only", "unmapped"] {
        let mut task = ready.clone();
        if gap == "unmapped" {
            task.contract.criteria[0].checks.clear();
        } else {
            let mut additional = task.contract.checks[0].clone();
            additional.id = "additional".into();
            additional.required = gap == "required-only";
            task.contract.checks.push(additional);
            if gap == "criterion-only" {
                task.contract.criteria[0].checks.push("additional".into());
            }
        }
        task.receipts[0].contract_id = task.contract.id().unwrap();
        let decision = pixel_task::policy::decide(&task, Gate::Finish, Some(source));
        assert!(!decision.allowed, "{gap}");
        assert_eq!(
            decision.eligible_routes,
            vec![Route::Investigate, Route::Recover, Route::Verify],
            "{gap}"
        );
    }
}

#[test]
fn finish_rehashes_source_instead_of_trusting_a_damaged_digest_cache() {
    let directory = repo();
    let store = Store::open(directory.path()).unwrap();
    let mut configured = contract("true");
    configured.require_review = false;
    let task = store.begin(configured, "pi", None, "begin").unwrap();
    let task = store
        .update(
            &task.task_id,
            task.revision,
            "prepare",
            Action::Prepare {
                observations: observations(),
            },
        )
        .unwrap();
    let task = store.verify(&task.task_id, &[], "verify").unwrap();
    let cache_path = directory.path().join(".pixel/tasks/source-cache.json");
    let mut cache: serde_json::Value =
        serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
    cache["source.txt"]["file"]["sha256"] = json!("damaged-cached-digest");
    fs::write(&cache_path, serde_json::to_vec(&cache).unwrap()).unwrap();
    let (input, source_id) = store.decision_input(&task.task_id, Gate::Finish).unwrap();
    assert_eq!(source_id, task.source.as_ref().unwrap().content_id);
    assert_eq!(input.task_id, task.task_id);
    assert_eq!(input.receipts[0].outcome, CheckOutcome::Passed);
    assert!(pixel_task::policy::decide(&input, Gate::Finish, Some(&source_id)).allowed);
}
