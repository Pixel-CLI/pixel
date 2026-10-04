// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Bounded local ranking over deterministic legal routes; every fallback preserves gates.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use fs2::FileExt;
use pixel_task::replay::{RecordedAdvice, ReplayFrame, replay_fingerprint};
use pixel_task::{Gate, Route, Store, Task};
use serde_json::{Value, json};

use crate::task_commands::{error, observe};

pub(crate) fn route(
    root: &Path,
    store: &Store,
    task: &Task,
    _request_id: &str,
) -> Result<Value, String> {
    let (mut task, source_id) = store
        .decision_input(&task.task_id, Gate::Finish)
        .map_err(error)?;
    // Journal appends do not change the policy input and must not trigger another prediction.
    task.revision = 0;
    task.updated_ms = 0;
    let decision = pixel_task::policy::decide(&task, Gate::Finish, Some(&source_id));
    let mut frame = ReplayFrame {
        schema_version: 1,
        task: task.clone(),
        gate: Gate::Finish,
        current_source_id: Some(source_id),
        recorded_decision: decision.clone(),
        classifier: None,
    };
    let fingerprint = replay_fingerprint(&frame).map_err(error)?;
    let enabled = crate::config_cmd::classify_enabled().unwrap_or(false)
        && std::env::var("PIXEL_TASK_POLICY").map_or(true, |value| value == "gates_classifier");
    let engine = crate::config_cmd::classify_engine();
    let base = crate::classify_setup::local_base();
    let key = pixel_task::digest(&(
        pixel_task::POLICY_VERSION,
        &fingerprint,
        enabled,
        &engine,
        &base,
    ))
    .map_err(error)?;
    let cache_id = format!("route-{key}");
    let locks = root.join(".pixel/tasks/route-locks");
    pixel_ops::durable::ensure_dir(&locks).map_err(error)?;
    let lock = pixel_git::nofollow::open_lock(&locks.join(format!("{}-{key}", task.task_id)))
        .map_err(error)?;
    lock.lock_exclusive().map_err(error)?;
    let events = store.events(&task.task_id).map_err(error)?;
    let classifier_id = format!("classify-{key}");
    if let Some(previous) = events.iter().find(|event| event.id == classifier_id) {
        let duration_ms = previous.data["data"]["duration_ms"]
            .as_u64()
            .ok_or("recorded classifier duration is unavailable")?;
        // Resume a failed export even when inference or the route is cached.
        crate::task_bridge::record_internal(
            store,
            &task,
            &classifier_id,
            pixel_task::replay::InternalActor::Classifier,
            duration_ms,
        )?;
    }
    if let Some(cached) = events.into_iter().find(|event| event.id == cache_id) {
        return Ok(cached.data["data"].clone());
    }
    let labels: Vec<_> = decision
        .eligible_routes
        .iter()
        .map(|route| {
            serde_json::to_value(route)
                .unwrap_or(Value::Null)
                .as_str()
                .unwrap_or("")
                .to_string()
        })
        .collect();
    let start = Instant::now();
    let mut invoked = false;
    let attempt_id = format!("route-attempt-{key}");
    let attempted = store
        .events(&task.task_id)
        .map_err(error)?
        .iter()
        .any(|event| event.id == attempt_id);
    // Policy always includes Investigate and Recover, so there are at least two routes.
    let prediction = if enabled
        && !attempted
        && crate::classify_setup::local_permitted(engine.as_deref())
        && crate::classify_setup::server_reachable_within(&base, Duration::from_millis(15))
    {
        // Reserve once before inference. A process crash cannot spend the same
        // decision budget twice; the next delivery uses deterministic fallback.
        observe(
            store,
            &task,
            &attempt_id,
            "route_attempt",
            json!({"input":key}),
        )?;
        invoked = true;
        let spec = crate::classify::Spec::checked(
            task.contract.objective.clone(),
            format!(
                "Rank the next action using only these legal routes. Missing evidence: {}",
                decision.reasons.join("; ")
            ),
            labels.clone(),
            BTreeMap::new(),
        )?;
        let mut engine = crate::decide_ollaya::Ollaya::open(crate::decide_ollaya::OllayaConfig {
            base,
            // The budget covers inference alone: the reservation above fsyncs
            // the journal, its directory and task.json, and on a loaded disk
            // those writes consumed the whole budget before the request went
            // out (shard 3 of #704, shard 0 of #703). `start` still spans the
            // attempt end-to-end for the recorded classifier duration.
            timeout: Duration::from_millis(300),
            ..Default::default()
        });
        engine.decide(&spec).ok().and_then(|scores| {
            rank(&labels, &scores).map(|ranking| (ranking, scores, engine.model_id().to_string()))
        })
    } else {
        None
    };
    let mut ranked = decision.eligible_routes.clone();
    if let Some((ranking, _, model)) = &prediction {
        ranked = ranking.clone();
        frame.classifier = Some(RecordedAdvice {
            input_fingerprint: fingerprint,
            model_id: model.clone(),
            policy_version: pixel_task::POLICY_VERSION.into(),
            ranked_routes: ranked.clone(),
        });
    }
    let result = json!({"schema_version":1,"task_id":task.task_id,"policy_version":pixel_task::POLICY_VERSION,
        "decision":decision,"ranked_routes":ranked,"recommended":ranked.first(),"classifier":prediction.as_ref().map(|(_,scores,model)| json!({"scores":scores,"model":model})),"frame":frame});
    if invoked {
        crate::task_bridge::record_internal(
            store,
            &task,
            &classifier_id,
            pixel_task::replay::InternalActor::Classifier,
            start.elapsed().as_millis() as u64,
        )?;
    }
    // A committed route must never hide an unrecorded classifier operation.
    observe(store, &task, &cache_id, "route", result.clone())?;
    Ok(result)
}

fn rank(labels: &[String], scores: &BTreeMap<String, f64>) -> Option<Vec<Route>> {
    if labels.len() != scores.len()
        || labels.iter().any(|label| {
            scores
                .get(label)
                .is_none_or(|score| !score.is_finite() || !(0.0..=1.0).contains(score))
        })
    {
        return None;
    }
    let mut ranked: Vec<_> = labels.iter().collect();
    ranked.sort_by(|a, b| scores[*b].total_cmp(&scores[*a]));
    if ranked.len() < 2 || scores[ranked[0]] - scores[ranked[1]] < 0.05 {
        return None;
    }
    ranked
        .into_iter()
        .map(|label| serde_json::from_value(json!(label)).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranking_should_reject_unknown_missing_invalid_and_ambiguous_predictions() {
        let labels = vec!["prepare".into(), "verify".into()];
        let scores = BTreeMap::from([("prepare".into(), 0.2), ("verify".into(), 0.8)]);
        assert_eq!(
            rank(&labels, &scores),
            Some(vec![Route::Verify, Route::Prepare])
        );
        for scores in [
            BTreeMap::new(),
            BTreeMap::from([("finish".into(), 1.0)]),
            BTreeMap::from([("prepare".into(), f64::NAN), ("verify".into(), 0.8)]),
            BTreeMap::from([("prepare".into(), 0.5), ("verify".into(), 0.5)]),
        ] {
            assert_eq!(rank(&labels, &scores), None);
        }
    }

    #[test]
    fn ranking_should_enforce_probability_and_margin_boundaries() {
        let labels = vec!["prepare".into(), "verify".into()];
        for (first, second, accepted) in [
            (0.05, 0.0, true),
            (0.049, 0.0, false),
            (1.0, 0.0, true),
            (-0.01, 0.9, false),
            (1.01, 0.0, false),
            (f64::INFINITY, 0.0, false),
        ] {
            let scores = BTreeMap::from([("prepare".into(), first), ("verify".into(), second)]);
            assert_eq!(
                rank(&labels, &scores),
                accepted.then_some(vec![Route::Prepare, Route::Verify]),
                "{first}, {second}"
            );
        }
        assert_eq!(rank(&[], &BTreeMap::new()), None);
        assert_eq!(
            rank(
                &["prepare".into()],
                &BTreeMap::from([("prepare".into(), 1.0)])
            ),
            None
        );
        assert_eq!(
            rank(
                &labels,
                &BTreeMap::from([("prepare".into(), 1.0), ("finish".into(), 0.0)])
            ),
            None
        );
        assert_eq!(
            rank(
                &["invalid".into(), "verify".into()],
                &BTreeMap::from([("invalid".into(), 1.0), ("verify".into(), 0.0)])
            ),
            None
        );
    }
}
