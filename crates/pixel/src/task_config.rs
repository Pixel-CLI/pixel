// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Repository check definitions and task contracts, preserving mandatory requirements.

use std::path::Path;

use pixel_task::model::TaskContract;
use serde_json::{Value, json};

pub(crate) fn settings(root: &Path) -> Result<Value, String> {
    let path = crate::config_file::preferred_path(&root.join(".pixel"));
    let config = crate::config_file::load(&path)?;
    Ok(config.get("task").cloned().unwrap_or_else(|| json!({})))
}

/// Whether the task gates may deny a tool call or a stop. Off by default:
/// tasks still bind, observe and record — the verdicts are advisory unless
/// the repository asks for enforcement (`task.enforcement: enforce`).
pub(crate) fn enabled(root: &Path) -> Result<bool, String> {
    let config = settings(root)?;
    match config.get("enforcement").and_then(Value::as_str) {
        Some("enforce") => Ok(true),
        None | Some("advisory") | Some("off") => Ok(false),
        Some(_) => Err("task.enforcement must be enforce, advisory or off".into()),
    }
}

pub(crate) fn initial(root: &Path, objective: &str) -> Result<TaskContract, String> {
    let config = settings(root)?;
    let mut contract = json!({
        "version": 1,
        "objective": objective,
        "checks": [], "criteria": [], "inputs": [], "outputs": [],
        "require_preparation": true, "require_review": true
    });
    for name in [
        "checks",
        "criteria",
        "inputs",
        "outputs",
        "conservative_checks",
        "toolchain",
    ] {
        if let Some(value) = config.get(name) {
            contract[name] = value.clone();
        }
    }
    let criteria = contract["criteria"]
        .as_array_mut()
        .ok_or("task.criteria must be an array")?;
    if !criteria
        .iter()
        .any(|criterion| criterion["id"] == "task-acceptance")
    {
        criteria.push(json!({"id":"task-acceptance", "description":objective, "checks":[]}));
    }
    parse(contract)
}

pub(crate) fn from_file(root: &Path, path: &Path) -> Result<TaskContract, String> {
    let proposed = crate::config_file::load(path)?;
    if proposed.as_object().is_none_or(serde_json::Map::is_empty) {
        return Err("contract file is empty or missing".into());
    }
    parse(merge_required(&settings(root)?, proposed)?)
}

pub(crate) fn from_definition(root: &Path, definition: &str) -> Result<TaskContract, String> {
    let proposed = serde_json::from_str(definition).map_err(|_| "invalid task contract JSON")?;
    parse(merge_required(&settings(root)?, proposed)?)
}

pub(crate) fn reconcile(root: &Path, contract: &TaskContract) -> Result<TaskContract, String> {
    let value = serde_json::to_value(contract).map_err(|error| error.to_string())?;
    parse(merge_required(&settings(root)?, value)?)
}

fn parse(value: Value) -> Result<TaskContract, String> {
    let contract: TaskContract = serde_json::from_value(value).map_err(
        |_| "invalid task contract; inspect task contract help and the documented schema",
    )?;
    contract.validate().map_err(|error| error.to_string())?;
    Ok(contract)
}

fn merge_required(config: &Value, mut contract: Value) -> Result<Value, String> {
    let object = contract
        .as_object_mut()
        .ok_or("contract must be an object")?;
    if let Some(required) = config.get("toolchain") {
        let required = required
            .as_object()
            .ok_or("task.toolchain must be an object")?;
        let target = object
            .entry("toolchain")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or("contract toolchain must be an object")?;
        for (executable, identity) in required {
            if target
                .get(executable)
                .is_some_and(|value| value != identity)
            {
                return Err(format!(
                    "contract cannot replace repository toolchain requirement {executable}"
                ));
            }
            target.insert(executable.clone(), identity.clone());
        }
    }
    for key in [
        "checks",
        "criteria",
        "inputs",
        "outputs",
        "conservative_checks",
    ] {
        object.entry(key).or_insert_with(|| json!([]));
        let target = object
            .get_mut(key)
            .and_then(Value::as_array_mut)
            .ok_or_else(|| format!("contract {key} must be an array"))?;
        let Some(required) = config.get(key) else {
            continue;
        };
        let required = required
            .as_array()
            .ok_or_else(|| format!("task.{key} must be an array"))?;
        for entry in required {
            if key == "checks" || key == "criteria" {
                let id = entry
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("repository {key} require IDs"))?;
                if let Some(existing) = target.iter().find(|item| item["id"] == id) {
                    if !equivalent_requirement(key, existing, entry)? {
                        return Err(format!(
                            "contract cannot replace repository requirement {id}"
                        ));
                    }
                } else {
                    target.push(entry.clone());
                }
            } else if !target.contains(entry) {
                target.push(entry.clone());
            }
        }
    }
    Ok(contract)
}

fn equivalent_requirement(key: &str, existing: &Value, required: &Value) -> Result<bool, String> {
    if key == "checks" {
        let decode = |value: &Value| {
            serde_json::from_value::<pixel_task::model::Check>(value.clone())
                .map_err(|_| "invalid repository check".to_string())
        };
        Ok(decode(existing)? == decode(required)?)
    } else {
        let decode = |value: &Value| {
            serde_json::from_value::<pixel_task::model::Criterion>(value.clone())
                .map_err(|_| "invalid repository criterion".to_string())
        };
        let existing = decode(existing)?;
        let required = decode(required)?;
        Ok(existing.id == required.id
            && existing.description == required.description
            && required
                .checks
                .iter()
                .all(|check| existing.checks.contains(check)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "pixel-task-config-{}-{}",
                crate::task_commands::request(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(root.join(".pixel")).unwrap();
            Self(root)
        }

        fn write(&self, task: &Value) {
            std::fs::write(
                self.0.join(".pixel/config.yaml"),
                serde_json::to_vec(&json!({"task":task})).unwrap(),
            )
            .unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn initial_should_add_acceptance_beside_other_criteria_and_preserve_existing_acceptance() {
        let root = Scratch::new();
        let repository = json!({
            "id":"repository", "description":"repository requirement", "checks":[]
        });
        root.write(&json!({"criteria":[repository.clone()]}));
        let contract = initial(&root.0, "fix observed behavior").unwrap();
        assert_eq!(contract.objective, "fix observed behavior");
        assert_eq!(
            serde_json::to_value(&contract.criteria).unwrap(),
            json!([
                repository,
                {"id":"task-acceptance", "description":"fix observed behavior", "checks":[]}
            ])
        );

        let acceptance = json!({
            "id":"task-acceptance", "description":"configured acceptance", "checks":[]
        });
        root.write(&json!({"criteria":[acceptance.clone()]}));
        let contract = initial(&root.0, "another objective").unwrap();
        assert_eq!(contract.objective, "another objective");
        assert_eq!(
            serde_json::to_value(&contract.criteria).unwrap(),
            json!([acceptance])
        );
    }

    #[test]
    fn enabled_should_require_explicit_enforcement_and_reject_invalid_settings() {
        let root = Scratch::new();
        assert!(!enabled(&root.0).unwrap());
        root.write(&json!({"enforcement":"advisory"}));
        assert!(!enabled(&root.0).unwrap());
        root.write(&json!({"enforcement":"off"}));
        assert!(!enabled(&root.0).unwrap());
        root.write(&json!({"enforcement":"enforce"}));
        assert!(enabled(&root.0).unwrap());
        root.write(&json!({"enforcement":"invalid"}));
        assert_eq!(
            enabled(&root.0).unwrap_err(),
            "task.enforcement must be enforce, advisory or off"
        );
    }

    #[test]
    fn merging_should_preserve_repository_checks_and_unrelated_task_criteria() {
        let mandatory = json!({"id":"test", "argv":["cargo","test"], "required":true});
        let config =
            json!({"checks":[mandatory.clone()], "inputs":["fixtures"], "outputs":["target"]});
        let proposed = json!({"objective":"fix", "checks":[], "criteria":[{"id":"bug","checks":["test"]}], "inputs":["data"], "outputs":[]});
        let merged = merge_required(&config, proposed).unwrap();
        assert_eq!(merged["checks"], json!([mandatory]));
        assert_eq!(merged["criteria"], json!([{"id":"bug","checks":["test"]}]));
        assert_eq!(merged["inputs"], json!(["data", "fixtures"]));
        assert_eq!(merged["outputs"], json!(["target"]));
        assert_eq!(merge_required(&config, merged.clone()).unwrap(), merged);
    }

    #[test]
    fn merging_should_reject_replacing_a_repository_check_with_a_weaker_command() {
        let config = json!({"checks":[{"id":"test","argv":["cargo","test"]}]});
        let proposed = json!({"checks":[{"id":"test","argv":["true"]}]});
        assert_eq!(
            merge_required(&config, proposed).unwrap_err(),
            "contract cannot replace repository requirement test"
        );
    }

    #[test]
    fn merging_should_reject_malformed_requirements_instead_of_dropping_them() {
        assert!(merge_required(&json!({"checks":{}}), json!({})).is_err());
        assert!(
            merge_required(
                &json!({"criteria":[{"description":"missing id"}]}),
                json!({})
            )
            .is_err()
        );
        assert!(merge_required(&json!({}), json!({"inputs":true})).is_err());
        assert!(merge_required(&json!({"toolchain":[]}), json!({})).is_err());
        assert!(merge_required(&json!({"toolchain":{}}), json!({"toolchain":[]})).is_err());
    }

    #[test]
    fn merging_should_keep_pinned_tools_and_reject_conflicting_identities() {
        let config = json!({"toolchain":{"compiler":"a".repeat(64)}});
        let proposed = json!({"toolchain":{"driver":"b".repeat(64)}});
        let merged = merge_required(&config, proposed).unwrap();
        assert_eq!(
            merged["toolchain"],
            json!({"compiler":"a".repeat(64),"driver":"b".repeat(64)})
        );
        assert!(merge_required(&config, json!({"toolchain":{"compiler":"c".repeat(64)}})).is_err());
        assert_eq!(merge_required(&config, merged.clone()).unwrap(), merged);
    }

    #[test]
    fn criterion_requirements_should_preserve_identity_description_and_every_mapping() {
        let required =
            json!({"id":"acceptance","description":"expected behavior","checks":["test"]});
        let stronger = json!({"id":"acceptance","description":"expected behavior","checks":["test","additional"]});
        assert!(equivalent_requirement("criteria", &stronger, &required).unwrap());
        for weaker in [
            json!({"id":"different","description":"expected behavior","checks":["test"]}),
            json!({"id":"acceptance","description":"different","checks":["test"]}),
            json!({"id":"acceptance","description":"expected behavior","checks":[]}),
        ] {
            assert!(!equivalent_requirement("criteria", &weaker, &required).unwrap());
        }
        let config = json!({"criteria":[required.clone()]});
        assert_eq!(
            merge_required(&config, json!({})).unwrap()["criteria"],
            json!([required])
        );
        assert!(merge_required(&config, json!({"criteria":[{"id":"acceptance","description":"different","checks":["test"]}]})).is_err());
    }
}
