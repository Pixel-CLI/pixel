// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

use super::*;

fn task_entry(id_src: &str, created: u64, path: &str) -> Value {
    serde_json::json!({
        "id": targets_task_id(id_src),
        "task": id_src,
        "created_unix": created,
        "targets": [{"path": path, "tier": "P0"}],
    })
}

#[test]
fn merge_two_tasks_coexist() {
    let now = 1_000_000;
    let v = merge_targets_manifest(None, task_entry("task A", now, "src/a.rs"), now);
    let text = v.to_string();
    let v2 = merge_targets_manifest(Some(&text), task_entry("task B", now, "src/b.rs"), now);
    let tasks = v2["tasks"].as_array().unwrap();
    assert_eq!(v2["version"], 2);
    assert_eq!(tasks.len(), 2, "concurrent tasks must both survive");
    let names: Vec<&str> = tasks.iter().map(|t| t["task"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["task A", "task B"]);
}

#[test]
fn merge_replaces_same_task_id() {
    let now = 1_000_000;
    let v = merge_targets_manifest(None, task_entry("task A", now - 100, "src/old.rs"), now);
    let text = v.to_string();
    let v2 = merge_targets_manifest(Some(&text), task_entry("task A", now, "src/new.rs"), now);
    let tasks = v2["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1, "same task id must replace, not append");
    assert_eq!(tasks[0]["targets"][0]["path"], "src/new.rs");
}

#[test]
fn merge_drops_expired_tasks() {
    let now = 1_000_000_000;
    let old = merge_targets_manifest(
        None,
        task_entry("stale task", now - TARGETS_TTL_SECS - 1, "src/stale.rs"),
        now - TARGETS_TTL_SECS - 1,
    );
    let text = old.to_string();
    let v2 = merge_targets_manifest(
        Some(&text),
        task_entry("fresh task", now, "src/fresh.rs"),
        now,
    );
    let tasks = v2["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1, "expired task must be dropped on merge");
    assert_eq!(tasks[0]["task"], "fresh task");
}

#[test]
fn merge_wraps_legacy_singleton() {
    let now = 1_000_000;
    let legacy = serde_json::json!({
        "version": 1,
        "task": "legacy task",
        "created_unix": now - 50,
        "head_oid": "abc",
        "limit": 20,
        "files": [{"path": "src/legacy.rs", "tier": "P0"}],
    })
    .to_string();
    let v2 = merge_targets_manifest(
        Some(&legacy),
        task_entry("new task", now, "src/new.rs"),
        now,
    );
    let tasks = v2["tasks"].as_array().unwrap();
    assert_eq!(
        tasks.len(),
        2,
        "legacy singleton must be preserved as a v2 task"
    );
    assert_eq!(tasks[0]["task"], "legacy task");
    assert_eq!(tasks[0]["targets"][0]["path"], "src/legacy.rs");
    assert_eq!(tasks[1]["task"], "new task");
}

#[test]
fn merge_survives_corrupt_existing() {
    let now = 1_000_000;
    let v2 = merge_targets_manifest(
        Some("{not json"),
        task_entry("task A", now, "src/a.rs"),
        now,
    );
    assert_eq!(v2["tasks"].as_array().unwrap().len(), 1);
}

#[test]
fn task_id_stable_and_short() {
    assert_eq!(targets_task_id("x"), targets_task_id("x"));
    assert_ne!(targets_task_id("x"), targets_task_id("y"));
    assert_eq!(targets_task_id("anything").len(), 12);
}

#[test]
fn merge_caps_at_max_manifest_tasks() {
    let mut now = 1_000_000u64;
    let mut text =
        merge_targets_manifest(None, task_entry("task 0", now, "src/a0.rs"), now).to_string();

    // Add MAX_MANIFEST_TASKS more tasks (total = MAX+1, should cap).
    for i in 1..=MAX_MANIFEST_TASKS {
        now += 10;
        text = merge_targets_manifest(
            Some(&text),
            task_entry(&format!("task {i}"), now, &format!("src/a{i}.rs")),
            now,
        )
        .to_string();
    }
    let v: Value = serde_json::from_str(&text).unwrap();
    let tasks = v["tasks"].as_array().unwrap();
    assert_eq!(
        tasks.len(),
        MAX_MANIFEST_TASKS,
        "manifest must be capped at MAX_MANIFEST_TASKS"
    );
    // Oldest task ("task 0") must be evicted; newest ("task {MAX}") kept.
    let names: Vec<&str> = tasks.iter().map(|t| t["task"].as_str().unwrap()).collect();
    assert!(!names.contains(&"task 0"), "oldest task must be evicted");
    assert!(
        names.contains(&format!("task {MAX_MANIFEST_TASKS}").as_str()),
        "newest task must survive"
    );
}

#[test]
fn an_overfull_manifest_on_disk_is_trimmed_back_to_the_cap() {
    let now = 1_000_000u64;
    let tasks: Vec<Value> = (0..MAX_MANIFEST_TASKS + 2)
        .map(|i| task_entry(&format!("old {i}"), now + i as u64, "src/a.rs"))
        .collect();
    let text = serde_json::json!({"version": 2, "tasks": tasks}).to_string();
    let v = merge_targets_manifest(
        Some(&text),
        task_entry("new", now + 100, "src/n.rs"),
        now + 100,
    );
    let names: Vec<&str> = v["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["task"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), MAX_MANIFEST_TASKS);
    assert_eq!(
        names.first(),
        Some(&"old 3"),
        "the three oldest are evicted"
    );
    assert_eq!(names.last(), Some(&"new"));
}
