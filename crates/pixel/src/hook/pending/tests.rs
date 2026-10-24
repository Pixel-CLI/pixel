// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

use super::*;

/// A home with `~/.gemini/artifacts/c1` and the payload of a step in it.
fn fixture() -> (tempfile::TempDir, PathBuf, Value) {
    let home = tempfile::tempdir().unwrap();
    let artifacts = home.path().join(".gemini/artifacts/c1");
    fs::create_dir_all(&artifacts).unwrap();
    let payload = json!({
        "conversationId": "c1", "stepIdx": 4, "initialNumSteps": 3,
        "artifactDirectoryPath": artifacts.display().to_string(),
    });
    (home, artifacts, payload)
}

#[test]
fn parked_blocks_come_back_in_step_order_once() {
    let (home, artifacts, mut payload) = fixture();
    for (step, block) in [(6_u64, "six"), (4, "four"), (5, "five")] {
        payload["stepIdx"] = json!(step);
        store(&payload, home.path(), block);
    }
    assert!(artifacts.join(".pixel-pending-metrics/4.json").is_file());
    assert_eq!(take(&payload, home.path()), ["four", "five", "six"]);
    assert!(
        !artifacts.join(".pixel-pending-metrics").exists(),
        "the directory goes with the last block"
    );
    assert!(take(&payload, home.path()).is_empty());
}

#[test]
fn a_block_of_an_earlier_invocation_or_another_conversation_is_dropped() {
    let (home, _, mut payload) = fixture();
    payload["stepIdx"] = json!(2);
    store(&payload, home.path(), "before this invocation");
    payload["stepIdx"] = json!(5);
    payload["conversationId"] = json!("other");
    store(&payload, home.path(), "another conversation");
    payload["conversationId"] = json!("c1");
    payload["stepIdx"] = json!(7);
    store(&payload, home.path(), "this one");
    assert_eq!(take(&payload, home.path()), ["this one"]);
}

#[test]
fn nothing_is_parked_without_a_step_a_conversation_or_a_path_under_gemini() {
    let (home, artifacts, payload) = fixture();
    for key in ["stepIdx", "conversationId", "artifactDirectoryPath"] {
        let mut missing = payload.clone();
        missing.as_object_mut().unwrap().remove(key);
        store(&missing, home.path(), "x");
        assert!(!artifacts.join(".pixel-pending-metrics").exists(), "{key}");
    }
    let outside = tempfile::tempdir().unwrap();
    let mut elsewhere = payload.clone();
    elsewhere["artifactDirectoryPath"] = json!(outside.path().display().to_string());
    store(&elsewhere, home.path(), "x");
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    let mut relative = payload.clone();
    relative["artifactDirectoryPath"] = json!("artifacts/c1");
    store(&relative, home.path(), "x");
    assert!(!artifacts.join(".pixel-pending-metrics").exists());
}

#[test]
fn the_tilde_names_the_home_and_a_corrupt_file_is_dropped() {
    let (home, artifacts, mut payload) = fixture();
    payload["artifactDirectoryPath"] = json!("~/.gemini/artifacts/c1");
    store(&payload, home.path(), "four");
    let dir = artifacts.join(".pixel-pending-metrics");
    fs::write(dir.join("9.json"), "not json").unwrap();
    assert_eq!(take(&payload, home.path()), ["four"]);
    assert!(!dir.exists());
}
