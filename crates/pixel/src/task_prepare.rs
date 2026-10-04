// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Source-bound preparation and review using Pixel's existing deterministic operations.

use std::collections::BTreeSet;
use std::path::Path;

use crate::Request;
use pixel_task::{Action, Observation, ObservationKind, Store, Task};
use serde_json::{Value, json};

use crate::task_commands::{error, observe};

const SCOPE_LIMIT: usize = 20;
const IMPACT_LIMIT: usize = 16;

pub(crate) fn prepare(
    root: &Path,
    store: &Store,
    task: &Task,
    request_id: &str,
) -> Result<Task, String> {
    let source = pixel_task::snapshot::capture(root, &task.contract, false).map_err(error)?;
    let scope = crate::execute(
        root,
        Request::Targets {
            task: task.contract.objective.clone(),
            limit: Some(SCOPE_LIMIT),
            max_tier: None,
            precision: false,
        },
        false,
    );
    let changes = crate::execute(
        root,
        Request::Changes {
            base: None,
            offset: Some(0),
            include_tests: true,
        },
        false,
    );
    let observations = collect(
        &source.content_id,
        &task.contract.conservative_checks,
        scope,
        changes,
        |symbol| {
            crate::execute(
                root,
                Request::Impact {
                    uid_or_name: symbol.to_string(),
                    direction: "both".into(),
                    depth: Some(2),
                },
                false,
            )
        },
    );
    store
        .update(
            &task.task_id,
            task.revision,
            request_id,
            Action::Prepare { observations },
        )
        .map_err(error)
}

fn collect(
    source_id: &str,
    conservative_checks: &[String],
    scope: Result<Value, String>,
    changes: Result<Value, String>,
    impact: impl Fn(&str) -> Result<Value, String>,
) -> Vec<Observation> {
    let fallback = !conservative_checks.is_empty();
    let scope_complete = scope.as_ref().is_ok_and(complete);
    let changes_complete = changes.as_ref().is_ok_and(complete);
    let mut symbols = BTreeSet::new();
    if let Ok(scope) = &scope {
        for symbol in scope["targets"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|target| target["symbols"].as_array().into_iter().flatten())
        {
            if let Some(uid) = symbol["uid"].as_str() {
                symbols.insert(uid.to_string());
            }
        }
    }
    let truncated = symbols.len() > IMPACT_LIMIT;
    let impacts: Vec<_> = symbols.into_iter().take(IMPACT_LIMIT).map(|symbol| {
        let result = impact(&symbol);
        json!({"symbol":symbol,"complete":result.as_ref().is_ok_and(complete),"result":evidence(result)})
    }).collect();
    let impact_complete = scope_complete
        && changes_complete
        && !truncated
        && !impacts.is_empty()
        && impacts.iter().all(|result| result["complete"] == true);
    let observation = |kind, complete, data| Observation {
        kind,
        source_id: source_id.into(),
        complete,
        data,
    };
    vec![
        observation(
            ObservationKind::Scope,
            scope_complete || fallback,
            json!({"collection":evidence(scope),"graph_complete":scope_complete,"conservative_checks":conservative_checks}),
        ),
        observation(
            ObservationKind::Impact,
            impact_complete || fallback,
            json!({"collection":impacts,"graph_complete":impact_complete,"capped":truncated,"conservative_checks":conservative_checks,
                "note":"Static impact remains a lower bound. Configured conservative checks cover unresolved consumers."}),
        ),
        observation(
            ObservationKind::TestSuggestions,
            changes_complete || fallback,
            json!({"collection":evidence(changes),"graph_complete":changes_complete,"conservative_checks":conservative_checks}),
        ),
    ]
}

fn evidence(result: Result<Value, String>) -> Value {
    match result {
        Ok(value) => value,
        Err(_) => {
            json!({"status":"unavailable","note":"retrieval unavailable; configure conservative checks or repair the index"})
        }
    }
}

fn complete(value: &Value) -> bool {
    value.is_object()
        && value.get("error").is_none()
        && !matches!(
            value["status"].as_str(),
            Some("unresolved" | "unknown" | "unavailable" | "capped")
        )
        && value["truncated"] != true
        && value["epistemics"]["lower_bound"] != true
        && value["envelope"]["lower_bound"] != true
        && value["envelope"]["graph"]
            .as_str()
            .is_none_or(|state| state == "fresh")
        && value.get("next_offset").is_none_or(Value::is_null)
        && value["warnings"].as_array().is_none_or(Vec::is_empty)
}

pub(crate) fn review(
    root: &Path,
    store: &Store,
    task: &Task,
    mut findings: Vec<String>,
    request_id: &str,
) -> Result<Task, String> {
    let source = pixel_task::snapshot::capture(root, &task.contract, true).map_err(error)?;
    let review = pixel_ops::review::review(root, None, Some(131_072))?;
    let git = pixel_git::GitRunner::new(root);
    if git.run(&["diff", "--check"]).is_err() || git.run(&["diff", "--cached", "--check"]).is_err()
    {
        findings.push("git diff --check failed".into());
    }
    append_review_findings(&review, git.status_porcelain().len(), &mut findings);
    let after = pixel_task::snapshot::capture(root, &task.contract, true).map_err(error)?;
    if after.content_id != source.content_id {
        return Err("source changed while reviewing; retry review".into());
    }
    observe(
        store,
        task,
        &format!(
            "review-data-{}",
            pixel_task::digest(request_id).map_err(error)?
        ),
        "review_evidence",
        json!({"source_id":source.content_id,"diff_digest":pixel_task::digest(&review).map_err(error)?,"findings":findings,
            "basis":"deterministic change inventory and diff validation; does not substitute for configured acceptance checks"}),
    )?;
    let current = store.status(&task.task_id).map_err(error)?;
    store
        .update(
            &task.task_id,
            current.revision,
            request_id,
            Action::Review {
                passed: findings.is_empty(),
                findings,
            },
        )
        .map_err(error)
}

fn append_review_findings(review: &Value, changed_files: usize, findings: &mut Vec<String>) {
    if review["truncated"] == true || review["count"].as_u64() != Some(changed_files as u64) {
        findings.push("review is capped; narrow the change before completion".into());
    }
    if review["items"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item["kind"] == "conflicted"))
    {
        findings.push("unresolved merge conflicts".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_should_reject_hidden_changes_and_conflicts_independently() {
        let clean = json!({"count":1,"truncated":false,"items":[{"kind":"modified"}]});
        let mut findings = vec!["acceptance review still pending".into()];
        append_review_findings(&clean, 1, &mut findings);
        assert_eq!(findings, ["acceptance review still pending"]);
        for (review, expected) in [
            (
                json!({"count":1,"truncated":true,"items":[]}),
                "review is capped; narrow the change before completion",
            ),
            (
                json!({"count":0,"truncated":false,"items":[]}),
                "review is capped; narrow the change before completion",
            ),
            (
                json!({"truncated":false,"items":[]}),
                "review is capped; narrow the change before completion",
            ),
            (
                json!({"count":1,"truncated":false,"items":[{"kind":"conflicted"}]}),
                "unresolved merge conflicts",
            ),
        ] {
            let mut findings = Vec::new();
            append_review_findings(&review, 1, &mut findings);
            assert_eq!(findings, [expected]);
        }
    }

    #[test]
    fn unavailable_retrieval_should_require_an_explicit_conservative_suite() {
        let observations = collect(
            "source",
            &[],
            Err("index".into()),
            Err("index".into()),
            |_| panic!("no symbols"),
        );
        assert_eq!(
            observations
                .iter()
                .map(|item| item.complete)
                .collect::<Vec<_>>(),
            vec![false, false, false]
        );
        let observations = collect(
            "source",
            &["all-tests".into()],
            Err("index".into()),
            Err("index".into()),
            |_| panic!("no symbols"),
        );
        assert!(
            observations
                .iter()
                .all(|item| item.complete && item.source_id == "source")
        );
        assert_eq!(observations[1].data["graph_complete"], false);
        assert_eq!(
            observations[1].data["conservative_checks"],
            json!(["all-tests"])
        );
    }

    #[test]
    fn bounded_impact_should_preserve_uncertainty_and_query_each_symbol_once() {
        let scope = json!({"targets":[{"symbols":[{"uid":"a"},{"uid":"a"},{"uid":"b"}]}]});
        let calls = std::cell::RefCell::new(Vec::new());
        let observations = collect("source", &[], Ok(scope), Ok(json!({})), |symbol| {
            calls.borrow_mut().push(symbol.to_string());
            Ok(json!({"epistemics":{"lower_bound":symbol == "b"}}))
        });
        assert_eq!(*calls.borrow(), vec!["a", "b"]);
        assert!(observations[0].complete);
        assert!(!observations[1].complete);
        for value in [
            json!({"truncated":true}),
            json!({"next_offset":20}),
            json!({"envelope":{"graph":"stale"}}),
            json!({"status":"unknown"}),
            json!({"status":"unavailable"}),
            json!({"status":"unresolved"}),
            json!({"status":"capped"}),
            json!({"error":"failed"}),
            json!({"epistemics":{"lower_bound":true}}),
            json!({"envelope":{"lower_bound":true}}),
            json!({"warnings":["coverage missing"]}),
            Value::Null,
            json!([]),
        ] {
            assert!(!complete(&value));
        }
        assert!(complete(
            &json!({"status":"complete","truncated":false,"next_offset":null,
            "epistemics":{"lower_bound":false},"envelope":{"lower_bound":false,"graph":"fresh"},"warnings":[]})
        ));
    }

    #[test]
    fn impact_should_require_every_collection_and_honor_the_exact_symbol_cap() {
        for count in [0, IMPACT_LIMIT, IMPACT_LIMIT + 1] {
            let scope = json!({"targets":[{"symbols":(0..count).map(|n|json!({"uid":format!("symbol-{n}")})).collect::<Vec<_>>()}]});
            let observations = collect("source", &[], Ok(scope.clone()), Ok(json!({})), |_| {
                Ok(json!({}))
            });
            assert_eq!(observations[1].complete, count == IMPACT_LIMIT);
            assert_eq!(observations[1].data["capped"], count > IMPACT_LIMIT);
            assert_eq!(
                observations[1].data["collection"].as_array().unwrap().len(),
                count.min(IMPACT_LIMIT)
            );
            if count == IMPACT_LIMIT {
                for (scope, changes, impact) in [
                    (Err("scope".into()), Ok(json!({})), Ok(json!({}))),
                    (Ok(scope.clone()), Err("changes".into()), Ok(json!({}))),
                    (Ok(scope.clone()), Ok(json!({})), Err("impact".into())),
                ] {
                    let unresolved = collect("source", &[], scope, changes, |_| impact.clone());
                    assert!(!unresolved[1].complete);
                }
            }
        }
        assert_eq!(
            evidence(Ok(json!({"targets":["source.rs"]}))),
            json!({"targets":["source.rs"]})
        );
        assert_eq!(
            evidence(Err("private diagnostic".into()))["status"],
            "unavailable"
        );
    }
}
