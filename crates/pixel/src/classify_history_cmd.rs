// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel classify-history` — manage the verified-history store.
//!
//! List, add, remove, correct, or clear stored examples. The store is a
//! bounded project-local file under `.pixel/classify-history.jsonl`. Only
//! human/independently-verified labels can be added; model predictions can
//! never enter the store.

use serde_json::{Value, json};

use crate::classify_history::{HistoryStore, NewEntry};
use crate::prompt_intent;

/// Run a `pixel classify-history` subcommand.
pub fn run_classify_history(cmd: crate::ClassifyHistoryCmd) -> Result<(), String> {
    match cmd {
        crate::ClassifyHistoryCmd::List { json } => list(json),
        crate::ClassifyHistoryCmd::Add {
            text,
            label,
            source,
        } => add(text, label, source),
        crate::ClassifyHistoryCmd::Remove { id } => remove(id),
        crate::ClassifyHistoryCmd::Correct { id, label, source } => correct(id, label, source),
        crate::ClassifyHistoryCmd::Clear => clear(),
    }
}

fn open_store() -> Result<HistoryStore, String> {
    let cwd =
        std::env::current_dir().map_err(|e| format!("cannot determine current directory: {e}"))?;
    let root = crate::discover_root(&cwd).map_err(|e| format!("cannot discover repo root: {e}"))?;
    let path = HistoryStore::repo_path(&root);
    HistoryStore::open(path)
}

fn list(json: bool) -> Result<(), String> {
    let store = open_store()?;
    let entries = store.all();
    if json {
        let items: Vec<Value> = entries
            .iter()
            .map(|e| {
                json!({
                    "id": e.id,
                    "text": e.text,
                    "label": e.label,
                    "task_family": e.task_family,
                    "source": e.source,
                    "superseded": e.superseded,
                    "created_unix": e.created_unix,
                    "corrections": e.corrections.iter().map(|c| {
                        json!({
                            "label": c.label,
                            "source": c.source,
                            "at_unix": c.at_unix,
                        })
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Array(items)).unwrap()
        );
    } else {
        if entries.is_empty() {
            println!("(empty)");
        } else {
            for e in entries {
                let status = if e.superseded { " [superseded]" } else { "" };
                println!(
                    "{} | {} | {}{} (source: {})",
                    e.id, e.text, e.label, status, e.source
                );
            }
        }
    }
    Ok(())
}

/// The task-intent vocabulary the classifier itself uses. A stored label
/// outside it — a typo like `bugfiz` — would never be predicted and would
/// skew the verified history, so it is refused at the boundary.
fn checked_label(label: &str) -> Result<(), String> {
    let labels = prompt_intent::labels();
    if labels.iter().any(|known| known == label) {
        Ok(())
    } else {
        Err(format!(
            "unknown label {label:?}: the task-intent vocabulary is {}",
            labels.join(", ")
        ))
    }
}

fn add(text: String, label: String, source: String) -> Result<(), String> {
    let mut store = open_store()?;
    checked_label(&label)?;
    let spec = prompt_intent::spec(&text).unwrap();
    let entry = NewEntry::for_spec(
        text,
        label,
        "task - intent".to_string(),
        source,
        &crate::classify::Spec::checked(spec.text, spec.context, spec.labels, spec.criteria)
            .map_err(|e| format!("invalid spec: {e}"))?,
    );
    let id = store.add(entry)?;
    store.save()?;
    println!("added {id:?}");
    Ok(())
}

fn remove(id: String) -> Result<(), String> {
    let mut store = open_store()?;
    store.remove(&id)?;
    store.save()?;
    println!("removed {id}");
    Ok(())
}

fn correct(id: String, label: String, source: String) -> Result<(), String> {
    let mut store = open_store()?;
    checked_label(&label)?;
    store.correct(&id, &label, &source)?;
    store.save()?;
    println!("corrected {id} -> {label}");
    Ok(())
}

fn clear() -> Result<(), String> {
    let mut store = open_store()?;
    store.clear();
    store.save()?;
    println!("cleared");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::checked_label;

    #[test]
    fn a_label_in_the_task_intent_vocabulary_is_accepted() {
        for label in [
            "bugfix",
            "feature",
            "refactor",
            "investigate",
            "question",
            "review",
            "ops",
        ] {
            assert!(checked_label(label).is_ok(), "{label} should be accepted");
        }
    }

    #[test]
    fn a_label_outside_the_vocabulary_is_refused_with_the_vocabulary() {
        let err = checked_label("bugfiz").unwrap_err();
        assert!(err.contains("unknown label"), "{err}");
        // The message names the real vocabulary so the typo is obvious.
        assert!(err.contains("bugfix"), "{err}");
    }
}
