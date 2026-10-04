// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Measured task trajectories and offline policy replay, without counterfactual outcome claims.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{Decision, Gate, Route, Task};

pub const TELEMETRY_VERSION: u32 = 1;
pub const REPLAY_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("unsupported trajectory or replay schema {0}")]
    Version(u32),
    #[error("invalid trajectory: {0}")]
    Invalid(String),
    #[error("cannot encode replay input: {0}")]
    Json(#[from] serde_json::Error),
}

/// One adapter observation; no raw prompts, tool arguments, credentials, or outputs are required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub task_id: String,
    pub attempt_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub host_call_id: Option<String>,
    pub occurred_ms: u64,
    #[serde(flatten)]
    pub observation: Observation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Observation {
    /// A logical model request, recorded even when the host blocks execution.
    ToolRequested {
        request_id: String,
        tool: String,
        retry_of: Option<String>,
    },
    ToolFinished {
        request_id: String,
        outcome: ToolOutcome,
        duration_ms: Option<u64>,
    },
    ModelResponse {
        response_id: String,
        usage: Option<TokenUsage>,
        duration_ms: Option<u64>,
    },
    InternalCall {
        call_id: String,
        actor: InternalActor,
        duration_ms: Option<u64>,
    },
    /// Final adapter coverage includes every known child span, even children with no events.
    Coverage {
        complete: bool,
        child_spans: Vec<String>,
        missing: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    Succeeded,
    Failed,
    Blocked,
    Aborted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InternalActor {
    Coordinator,
    Classifier,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Complete,
    Partial,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrajectorySummary {
    pub schema_version: u32,
    pub coverage: Coverage,
    pub missing: Vec<String>,
    /// None means the count is a lower bound, never zero work.
    pub model_tool_requests: Option<u64>,
    pub observed_model_tool_requests: u64,
    pub blocked_requests: u64,
    pub retried_requests: u64,
    pub model_responses: u64,
    pub tool_duration_ms: Option<u64>,
    pub model_duration_ms: Option<u64>,
    pub tokens: Option<TokenUsage>,
    pub coordinator_calls: u64,
    pub coordinator_duration_ms: Option<u64>,
    pub classifier_calls: u64,
    pub classifier_duration_ms: Option<u64>,
}

/// Summarize measured events, rejecting conflicting duplicate identities.
pub fn summarize_trajectory(events: &[TelemetryEvent]) -> Result<TrajectorySummary, ReplayError> {
    let mut unique = BTreeMap::new();
    let mut requests: BTreeMap<_, &Observation> = BTreeMap::new();
    let mut results = BTreeMap::new();
    let mut responses = BTreeMap::new();
    let mut internal = BTreeMap::new();
    let mut spans = BTreeSet::new();
    let mut covered = BTreeSet::new();
    let mut missing = BTreeSet::new();
    let task = events.first().map(|e| e.task_id.as_str());
    for event in events {
        if event.schema_version != TELEMETRY_VERSION {
            return Err(ReplayError::Version(event.schema_version));
        }
        if [
            &event.event_id,
            &event.task_id,
            &event.attempt_id,
            &event.span_id,
        ]
        .iter()
        .any(|s| s.is_empty())
        {
            return Err(ReplayError::Invalid("empty identity".into()));
        }
        if Some(event.task_id.as_str()) != task {
            return Err(ReplayError::Invalid("mixed tasks".into()));
        }
        insert_consistent(&mut unique, event.event_id.clone(), event)?;
        let span = (event.attempt_id.as_str(), event.span_id.as_str());
        spans.insert(span);
        if let Some(parent) = &event.parent_span_id {
            if parent.is_empty() || parent == &event.span_id {
                return Err(ReplayError::Invalid("invalid parent span".into()));
            }
            spans.insert((event.attempt_id.as_str(), parent.as_str()));
        }
        let logical_id = match &event.observation {
            Observation::ToolRequested { request_id, .. }
            | Observation::ToolFinished { request_id, .. } => Some(request_id),
            Observation::ModelResponse { response_id, .. } => Some(response_id),
            Observation::InternalCall { call_id, .. } => Some(call_id),
            Observation::Coverage { .. } => None,
        };
        if logical_id.is_some_and(String::is_empty) {
            return Err(ReplayError::Invalid("empty logical identity".into()));
        }
        match &event.observation {
            Observation::ToolRequested {
                request_id,
                tool,
                retry_of,
            } => {
                let key = (span, request_id.as_str());
                if let Some(Observation::ToolRequested {
                    tool: prior_tool,
                    retry_of: prior_retry,
                    ..
                }) = requests.get(&key)
                {
                    if prior_retry != retry_of
                        || (prior_tool != tool && prior_tool != "unknown" && tool != "unknown")
                    {
                        return Err(ReplayError::Invalid(
                            "conflicting duplicate observation".into(),
                        ));
                    }
                    if tool != "unknown" {
                        requests.insert(key, &event.observation);
                    }
                } else {
                    requests.insert(key, &event.observation);
                }
            }
            Observation::ToolFinished { request_id, .. } => {
                insert_consistent(
                    &mut results,
                    (span, request_id.as_str()),
                    &event.observation,
                )?;
            }
            Observation::ModelResponse { response_id, .. } => {
                insert_consistent(
                    &mut responses,
                    (span, response_id.as_str()),
                    &event.observation,
                )?;
            }
            Observation::InternalCall { call_id, .. } => {
                insert_consistent(&mut internal, (span, call_id.as_str()), &event.observation)?;
            }
            Observation::Coverage {
                complete,
                child_spans,
                missing: gaps,
            } => {
                if *complete && gaps.is_empty() {
                    covered.insert(span);
                } else {
                    missing.insert(format!("incomplete span {}", event.span_id));
                }
                missing.extend(gaps.iter().cloned());
                for child in child_spans {
                    spans.insert((event.attempt_id.as_str(), child.as_str()));
                }
            }
        }
    }
    for (_, span) in spans.difference(&covered) {
        missing.insert(format!("uncovered span {span}"));
    }
    for (span, request) in results.keys() {
        if !requests.contains_key(&(*span, *request)) {
            missing.insert(format!("result without request {request}"));
        }
    }
    let coverage = if events.is_empty() {
        Coverage::Unavailable
    } else if missing.is_empty() {
        Coverage::Complete
    } else {
        Coverage::Partial
    };
    let request_count = requests.len() as u64;
    let tool_durations = requests.keys().map(|key| match results.get(key) {
        Some(Observation::ToolFinished { duration_ms, .. }) => *duration_ms,
        _ => None,
    });
    let mut token_usage = None;
    for response in responses.values() {
        let Observation::ModelResponse { usage, .. } = response else {
            unreachable!()
        };
        let Some(usage) = usage else {
            token_usage = None;
            break;
        };
        match &mut token_usage {
            None => token_usage = Some(usage.clone()),
            Some(total) => {
                total.input = total.input.saturating_add(usage.input);
                total.output = total.output.saturating_add(usage.output);
                total.cache_read = add_optional(total.cache_read, usage.cache_read);
                total.cache_write = add_optional(total.cache_write, usage.cache_write);
            }
        }
    }
    let duration_for = |actor| {
        measured_sum(internal.values().filter_map(|event| match event {
            Observation::InternalCall {
                actor: actual,
                duration_ms,
                ..
            } if *actual == actor => Some(*duration_ms),
            _ => None,
        }))
    };
    let count_for = |actor| {
        internal
            .values()
            .filter(|event| {
                matches!(event,
        Observation::InternalCall { actor: actual, .. } if *actual == actor)
            })
            .count() as u64
    };
    Ok(TrajectorySummary {
        schema_version: TELEMETRY_VERSION,
        coverage,
        missing: missing.into_iter().collect(),
        model_tool_requests: (coverage == Coverage::Complete).then_some(request_count),
        observed_model_tool_requests: request_count,
        blocked_requests: results
            .values()
            .filter(|event| {
                matches!(
                    event,
                    Observation::ToolFinished {
                        outcome: ToolOutcome::Blocked,
                        ..
                    }
                )
            })
            .count() as u64,
        retried_requests: requests
            .values()
            .filter(|event| {
                matches!(
                    event,
                    Observation::ToolRequested {
                        retry_of: Some(_),
                        ..
                    }
                )
            })
            .count() as u64,
        model_responses: responses.len() as u64,
        tool_duration_ms: measured_sum(tool_durations),
        model_duration_ms: measured_sum(responses.values().map(|event| match event {
            Observation::ModelResponse { duration_ms, .. } => *duration_ms,
            _ => None,
        })),
        tokens: token_usage,
        coordinator_calls: count_for(InternalActor::Coordinator),
        coordinator_duration_ms: duration_for(InternalActor::Coordinator),
        classifier_calls: count_for(InternalActor::Classifier),
        classifier_duration_ms: duration_for(InternalActor::Classifier),
    })
}

fn insert_consistent<K: Ord, V: PartialEq>(
    map: &mut BTreeMap<K, V>,
    key: K,
    value: V,
) -> Result<(), ReplayError> {
    if map.get(&key).is_some_and(|old| *old != value) {
        return Err(ReplayError::Invalid(
            "conflicting duplicate observation".into(),
        ));
    }
    map.insert(key, value);
    Ok(())
}

fn measured_sum(values: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    let mut count = 0;
    let mut sum = 0_u64;
    for value in values {
        count += 1;
        sum = sum.saturating_add(value?);
    }
    (count > 0).then_some(sum)
}

fn add_optional(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    Some(a?.saturating_add(b?))
}

/// Recorded ranking is replay input, never a request to run a classifier again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedAdvice {
    pub input_fingerprint: String,
    pub model_id: String,
    pub policy_version: String,
    pub ranked_routes: Vec<Route>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayFrame {
    pub schema_version: u32,
    pub task: Task,
    pub gate: Gate,
    pub current_source_id: Option<String>,
    pub recorded_decision: Decision,
    pub classifier: Option<RecordedAdvice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayDivergence {
    pub frame: usize,
    pub recorded: Decision,
    pub replayed: Decision,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayReport {
    pub schema_version: u32,
    pub frames_evaluated: usize,
    pub first_divergence: Option<ReplayDivergence>,
    pub recorded_recommendations: Vec<Option<Route>>,
    /// Always false: different decisions do not establish different eventual outcomes.
    pub counterfactual_outcome_established: bool,
}

/// Hash the complete frozen decision input; timestamps are whatever the recording captured.
pub fn replay_fingerprint(frame: &ReplayFrame) -> Result<String, ReplayError> {
    let bytes = serde_json::to_vec(&(&frame.task, &frame.gate, &frame.current_source_id))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

/// Re-evaluate frozen inputs using the current pure policy, without filesystem or model calls.
pub fn replay_policy(frames: &[ReplayFrame]) -> Result<ReplayReport, ReplayError> {
    let mut report = ReplayReport {
        schema_version: REPLAY_VERSION,
        frames_evaluated: 0,
        first_divergence: None,
        recorded_recommendations: Vec::with_capacity(frames.len()),
        counterfactual_outcome_established: false,
    };
    for (index, frame) in frames.iter().enumerate() {
        if frame.schema_version != REPLAY_VERSION {
            return Err(ReplayError::Version(frame.schema_version));
        }
        let decision =
            crate::policy::decide(&frame.task, frame.gate, frame.current_source_id.as_deref());
        let recommendation = if let Some(advice) = &frame.classifier {
            if advice.input_fingerprint != replay_fingerprint(frame)? {
                return Err(ReplayError::Invalid(format!(
                    "classifier input mismatch at frame {index}"
                )));
            }
            advice
                .ranked_routes
                .iter()
                .find(|route| decision.eligible_routes.contains(route))
                .cloned()
        } else {
            None
        };
        report.recorded_recommendations.push(recommendation);
        if report.first_divergence.is_none() && decision != frame.recorded_decision {
            report.first_divergence = Some(ReplayDivergence {
                frame: index,
                recorded: frame.recorded_decision.clone(),
                replayed: decision,
            });
        }
        report.frames_evaluated += 1;
    }
    Ok(report)
}

/// Conditions held equal for an empirical comparison, independent of the policy variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComparisonIdentity {
    pub contract_hash: String,
    pub source_hash: String,
    pub host: String,
    pub host_version: String,
    pub model_config_hash: String,
    pub tool_environment_hash: String,
    pub verifier_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub attempt_id: String,
    pub identity: ComparisonIdentity,
    pub verified_success: bool,
    pub trajectory: TrajectorySummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegretReport {
    pub extra_interactions: Option<u64>,
    pub best_observed_interactions: Option<u64>,
    pub matched_successful_attempts: Vec<String>,
    pub reason: String,
}

/// Compare successful, fully observed attempts; this is an empirical gap, not a shortest-path proof.
pub fn empirical_regret(attempt: &AttemptRecord, candidates: &[AttemptRecord]) -> RegretReport {
    let unknown = |reason: &str| RegretReport {
        extra_interactions: None,
        best_observed_interactions: None,
        matched_successful_attempts: Vec::new(),
        reason: reason.into(),
    };
    if attempt.attempt_id.is_empty()
        || [
            &attempt.identity.contract_hash,
            &attempt.identity.source_hash,
            &attempt.identity.host,
            &attempt.identity.host_version,
            &attempt.identity.model_config_hash,
            &attempt.identity.tool_environment_hash,
            &attempt.identity.verifier_hash,
        ]
        .iter()
        .any(|value| value.is_empty())
    {
        return unknown("comparison identity is incomplete");
    }
    if !attempt.verified_success {
        return unknown("subject did not pass the held-out verifier");
    }
    if attempt.trajectory.coverage != Coverage::Complete {
        return unknown("subject telemetry is incomplete");
    }
    let Some(count) = attempt.trajectory.model_tool_requests else {
        return unknown("subject interaction count is unknown");
    };
    let mut unique = BTreeMap::new();
    for candidate in candidates {
        if unique
            .get(&candidate.attempt_id)
            .is_some_and(|previous| *previous != candidate)
        {
            return unknown("conflicting duplicate attempt identity");
        }
        unique.insert(&candidate.attempt_id, candidate);
    }
    let comparable: Vec<_> = unique
        .values()
        .filter(|candidate| {
            candidate.attempt_id != attempt.attempt_id
                && candidate.verified_success
                && candidate.identity == attempt.identity
                && candidate.trajectory.coverage == Coverage::Complete
                && candidate.trajectory.model_tool_requests.is_some()
        })
        .collect();
    let Some(best) = comparable
        .iter()
        .filter_map(|candidate| candidate.trajectory.model_tool_requests)
        .min()
    else {
        return unknown("no matching successful attempt with complete telemetry");
    };
    RegretReport {
        extra_interactions: Some(count.saturating_sub(best)),
        best_observed_interactions: Some(best),
        matched_successful_attempts: comparable
            .iter()
            .map(|candidate| candidate.attempt_id.clone())
            .collect(),
        reason: "observed successful comparator; no counterfactual or optimality claim".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str, observation: Observation) -> TelemetryEvent {
        TelemetryEvent {
            schema_version: 1,
            event_id: id.into(),
            task_id: "task".into(),
            attempt_id: "attempt".into(),
            span_id: "root".into(),
            parent_span_id: None,
            host_call_id: None,
            occurred_ms: 10,
            observation,
        }
    }

    fn coverage() -> TelemetryEvent {
        event(
            "coverage",
            Observation::Coverage {
                complete: true,
                child_spans: vec![],
                missing: vec![],
            },
        )
    }

    fn request(id: &str) -> TelemetryEvent {
        event(
            id,
            Observation::ToolRequested {
                request_id: id.into(),
                tool: "read".into(),
                retry_of: None,
            },
        )
    }

    #[test]
    fn request_labels_should_coalesce_unknown_without_hiding_conflicts() {
        let unknown = event(
            "model",
            Observation::ToolRequested {
                request_id: "call".into(),
                tool: "unknown".into(),
                retry_of: None,
            },
        );
        let known = event(
            "native",
            Observation::ToolRequested {
                request_id: "call".into(),
                tool: "edit".into(),
                retry_of: None,
            },
        );
        for pair in [
            [unknown.clone(), known.clone()],
            [known.clone(), unknown.clone()],
        ] {
            assert_eq!(
                summarize_trajectory(&[pair[0].clone(), pair[1].clone(), coverage()])
                    .unwrap()
                    .model_tool_requests,
                Some(1)
            );
        }
        let conflict = event(
            "other",
            Observation::ToolRequested {
                request_id: "call".into(),
                tool: "write".into(),
                retry_of: None,
            },
        );
        assert!(matches!(
            summarize_trajectory(&[unknown, known.clone(), conflict.clone()]),
            Err(ReplayError::Invalid(_))
        ));
        assert!(matches!(
            summarize_trajectory(&[known.clone(), conflict]),
            Err(ReplayError::Invalid(_))
        ));
        let retry = event(
            "retry",
            Observation::ToolRequested {
                request_id: "call".into(),
                tool: "unknown".into(),
                retry_of: Some("prior".into()),
            },
        );
        assert!(matches!(
            summarize_trajectory(&[known, retry]),
            Err(ReplayError::Invalid(_))
        ));
    }

    #[test]
    fn summary_should_count_blocked_retried_requests_once_and_keep_internal_cost_separate() {
        let first = request("one");
        let mut repeated = first.clone();
        repeated.event_id = "another-observer".into();
        let events = vec![
            first,
            repeated,
            event(
                "retry",
                Observation::ToolRequested {
                    request_id: "two".into(),
                    tool: "read".into(),
                    retry_of: Some("one".into()),
                },
            ),
            event(
                "blocked",
                Observation::ToolFinished {
                    request_id: "one".into(),
                    outcome: ToolOutcome::Blocked,
                    duration_ms: Some(0),
                },
            ),
            event(
                "done",
                Observation::ToolFinished {
                    request_id: "two".into(),
                    outcome: ToolOutcome::Succeeded,
                    duration_ms: Some(23),
                },
            ),
            event(
                "internal",
                Observation::InternalCall {
                    call_id: "i".into(),
                    actor: InternalActor::Coordinator,
                    duration_ms: Some(7),
                },
            ),
            event(
                "classifier",
                Observation::InternalCall {
                    call_id: "c".into(),
                    actor: InternalActor::Classifier,
                    duration_ms: Some(11),
                },
            ),
            coverage(),
        ];
        let result = summarize_trajectory(&events).unwrap();
        assert_eq!(result.model_tool_requests, Some(2));
        assert_eq!(result.blocked_requests, 1);
        assert_eq!(result.retried_requests, 1);
        assert_eq!(result.tool_duration_ms, Some(23));
        assert_eq!(
            (result.coordinator_calls, result.coordinator_duration_ms),
            (1, Some(7))
        );
        assert_eq!(
            (result.classifier_calls, result.classifier_duration_ms),
            (1, Some(11))
        );
        assert_eq!(result.model_duration_ms, None);
        assert_eq!(result.tokens, None);
    }

    #[test]
    fn summary_should_mark_missing_children_and_missing_usage_unknown() {
        let events = vec![
            request("one"),
            event(
                "coverage",
                Observation::Coverage {
                    complete: true,
                    child_spans: vec!["child".into()],
                    missing: vec![],
                },
            ),
        ];
        let result = summarize_trajectory(&events).unwrap();
        assert_eq!(result.coverage, Coverage::Partial);
        assert_eq!(result.model_tool_requests, None);
        assert_eq!(result.observed_model_tool_requests, 1);
        assert_eq!(result.missing, vec!["uncovered span child"]);
        assert_eq!(result.tool_duration_ms, None);
        assert_eq!(
            summarize_trajectory(&[]).unwrap().coverage,
            Coverage::Unavailable
        );
        let responses = vec![
            event(
                "model1",
                Observation::ModelResponse {
                    response_id: "m1".into(),
                    usage: Some(TokenUsage {
                        input: 3,
                        output: 7,
                        cache_read: None,
                        cache_write: Some(0),
                    }),
                    duration_ms: Some(25),
                },
            ),
            event(
                "model2",
                Observation::ModelResponse {
                    response_id: "m2".into(),
                    usage: Some(TokenUsage {
                        input: 2,
                        output: 5,
                        cache_read: Some(1),
                        cache_write: Some(2),
                    }),
                    duration_ms: Some(10),
                },
            ),
            coverage(),
        ];
        let summary = summarize_trajectory(&responses).unwrap();
        assert_eq!(
            summary.tokens,
            Some(TokenUsage {
                input: 5,
                output: 12,
                cache_read: None,
                cache_write: Some(2)
            })
        );
        assert_eq!(summary.model_responses, 2);
        assert_eq!(summary.model_duration_ms, Some(35));
        let mut missing = responses;
        missing.push(event(
            "model3",
            Observation::ModelResponse {
                response_id: "m3".into(),
                usage: None,
                duration_ms: None,
            },
        ));
        assert_eq!(summarize_trajectory(&missing).unwrap().tokens, None);
        assert_eq!(
            summarize_trajectory(&missing).unwrap().model_duration_ms,
            None
        );
    }

    #[test]
    fn summary_should_reject_conflicting_duplicates_versions_and_mixed_tasks() {
        let a = request("one");
        let mut b = a.clone();
        b.occurred_ms = 11;
        assert!(matches!(
            summarize_trajectory(&[a.clone(), b]),
            Err(ReplayError::Invalid(_))
        ));
        let mut b = a.clone();
        b.schema_version = 2;
        assert!(matches!(
            summarize_trajectory(&[b]),
            Err(ReplayError::Version(2))
        ));
        let mut b = request("two");
        b.task_id = "different".into();
        assert!(matches!(
            summarize_trajectory(&[a, b]),
            Err(ReplayError::Invalid(_))
        ));
    }

    fn attempt(id: &str, count: u64) -> AttemptRecord {
        let mut trajectory = summarize_trajectory(&[coverage()]).unwrap();
        trajectory.model_tool_requests = Some(count);
        AttemptRecord {
            attempt_id: id.into(),
            verified_success: true,
            trajectory,
            identity: ComparisonIdentity {
                contract_hash: "contract".into(),
                source_hash: "source".into(),
                host: "pi".into(),
                host_version: "0.87.1".into(),
                model_config_hash: "model".into(),
                tool_environment_hash: "env".into(),
                verifier_hash: "verifier".into(),
            },
        }
    }

    #[test]
    fn regret_should_compare_only_distinct_matched_successes_with_complete_coverage() {
        let subject = attempt("subject", 8);
        let mut failed = attempt("failed", 1);
        failed.verified_success = false;
        let mut other = attempt("other", 2);
        other.identity.source_hash = "other".into();
        let mut partial = attempt("partial", 3);
        partial.trajectory.coverage = Coverage::Partial;
        let report = empirical_regret(
            &subject,
            &[
                failed,
                other,
                partial,
                attempt("good", 5),
                attempt("slower", 9),
                subject.clone(),
            ],
        );
        assert_eq!(report.extra_interactions, Some(3));
        assert_eq!(report.best_observed_interactions, Some(5));
        assert_eq!(report.matched_successful_attempts, vec!["good", "slower"]);
        assert_eq!(empirical_regret(&subject, &[]).extra_interactions, None);
        assert_eq!(
            empirical_regret(&subject, std::slice::from_ref(&subject)).extra_interactions,
            None
        );
        assert_eq!(
            empirical_regret(&subject, &[attempt("worse", 20)]).extra_interactions,
            Some(0)
        );
        assert_eq!(
            empirical_regret(&subject, &[attempt("same", 2), attempt("same", 3)])
                .extra_interactions,
            None
        );
        let mut incomplete = subject.clone();
        incomplete.identity.model_config_hash.clear();
        assert_eq!(
            empirical_regret(&incomplete, &[attempt("good", 2)]).reason,
            "comparison identity is incomplete"
        );
    }

    fn frame() -> ReplayFrame {
        let task = Task {
            version: crate::model::SCHEMA_VERSION,
            task_id: "task".into(),
            provider: "pi".into(),
            session_id: None,
            attempt_id: "attempt".into(),
            revision: 1,
            phase: crate::model::Phase::Contracted,
            created_ms: 0,
            updated_ms: 0,
            contract: crate::model::TaskContract {
                objective: "fix parser".into(),
                ..Default::default()
            },
            source: None,
            observations: vec![],
            receipts: vec![],
            review: None,
            claims: vec![],
            budget: Default::default(),
            running: None,
            legacy_status: None,
        };
        let recorded_decision = crate::policy::decide(&task, Gate::Edit, None);
        ReplayFrame {
            schema_version: REPLAY_VERSION,
            task,
            gate: Gate::Edit,
            current_source_id: None,
            recorded_decision,
            classifier: None,
        }
    }

    #[test]
    fn replay_should_use_frozen_inputs_and_recorded_rankings_without_counterfactual_claims() {
        let mut first = frame();
        first.classifier = Some(RecordedAdvice {
            input_fingerprint: replay_fingerprint(&first).unwrap(),
            model_id: "offline-recorded".into(),
            policy_version: "v1".into(),
            ranked_routes: vec![Route::Finish, Route::Configure],
        });
        let same = replay_policy(std::slice::from_ref(&first)).unwrap();
        assert_eq!(same.recorded_recommendations, vec![Some(Route::Configure)]);
        assert_eq!(same.frames_evaluated, 1);
        assert!(same.first_divergence.is_none());
        assert!(!same.counterfactual_outcome_established);
        let mut changed = first.clone();
        changed.recorded_decision.allowed = true;
        let report = replay_policy(&[first, changed.clone(), changed]).unwrap();
        let divergence = report.first_divergence.unwrap();
        assert_eq!(divergence.frame, 1);
        assert!(divergence.recorded.allowed);
        assert!(!divergence.replayed.allowed);
        assert_eq!(report.frames_evaluated, 3);
        assert!(!report.counterfactual_outcome_established);
    }

    #[test]
    fn replay_should_reject_unknown_versions_and_changed_classifier_inputs() {
        let mut frame = frame();
        frame.schema_version = 2;
        assert!(matches!(
            replay_policy(std::slice::from_ref(&frame)),
            Err(ReplayError::Version(2))
        ));
        frame.schema_version = 1;
        let fingerprint = replay_fingerprint(&frame).unwrap();
        frame.task.revision += 1;
        assert_ne!(replay_fingerprint(&frame).unwrap(), fingerprint);
        frame.classifier = Some(RecordedAdvice {
            input_fingerprint: fingerprint,
            model_id: "recorded".into(),
            policy_version: "v1".into(),
            ranked_routes: vec![Route::Configure],
        });
        assert!(
            matches!(replay_policy(&[frame]), Err(ReplayError::Invalid(message)) if message == "classifier input mismatch at frame 0")
        );
    }

    #[test]
    fn summary_should_preserve_attempt_identity_and_require_parent_and_result_coverage() {
        let first = request("one");
        let mut second = first.clone();
        second.event_id = "other-event".into();
        second.attempt_id = "other-attempt".into();
        let mut second_coverage = coverage();
        second_coverage.event_id = "other-coverage".into();
        second_coverage.attempt_id = "other-attempt".into();
        let result = summarize_trajectory(&[first, second, coverage(), second_coverage]).unwrap();
        assert_eq!(result.model_tool_requests, Some(2));
        let orphan = summarize_trajectory(&[
            event(
                "result",
                Observation::ToolFinished {
                    request_id: "orphan".into(),
                    outcome: ToolOutcome::Failed,
                    duration_ms: Some(1),
                },
            ),
            coverage(),
        ])
        .unwrap();
        assert_eq!(orphan.model_tool_requests, None);
        assert_eq!(orphan.missing, vec!["result without request orphan"]);
        let incomplete = summarize_trajectory(&[event(
            "partial",
            Observation::Coverage {
                complete: false,
                child_spans: vec![],
                missing: vec!["host skipped paths".into()],
            },
        )])
        .unwrap();
        assert_eq!(incomplete.coverage, Coverage::Partial);
        assert_eq!(
            incomplete.missing,
            vec![
                "host skipped paths",
                "incomplete span root",
                "uncovered span root"
            ]
        );
        let invalid = event(
            "",
            Observation::Coverage {
                complete: true,
                child_spans: vec![],
                missing: vec![],
            },
        );
        assert!(
            matches!(summarize_trajectory(&[invalid]), Err(ReplayError::Invalid(message)) if message == "empty identity")
        );
        let mut child = request("child-request");
        child.parent_span_id = Some("parent".into());
        assert_eq!(
            summarize_trajectory(&[child, coverage()]).unwrap().missing,
            vec!["uncovered span parent"]
        );
        let empty = event(
            "event",
            Observation::ToolRequested {
                request_id: String::new(),
                tool: "edit".into(),
                retry_of: None,
            },
        );
        assert!(
            matches!(summarize_trajectory(&[empty]), Err(ReplayError::Invalid(message)) if message == "empty logical identity")
        );
    }

    #[test]
    fn summary_rejects_empty_and_self_parent_spans_independently() {
        for parent in ["", "root"] {
            let mut value = request("one");
            value.parent_span_id = Some(parent.into());
            assert!(
                matches!(summarize_trajectory(&[value]), Err(ReplayError::Invalid(message)) if message == "invalid parent span")
            );
        }
        let inconsistent = event(
            "coverage",
            Observation::Coverage {
                complete: true,
                child_spans: vec![],
                missing: vec!["missing request".into()],
            },
        );
        let summary = summarize_trajectory(&[inconsistent]).unwrap();
        assert_eq!(summary.coverage, Coverage::Partial);
        assert_eq!(
            summary.missing,
            vec![
                "incomplete span root",
                "missing request",
                "uncovered span root"
            ]
        );
    }

    #[test]
    fn regret_rejects_each_incomplete_subject_and_deduplicates_equal_comparators() {
        let subject = attempt("subject", 8);
        let comparator = attempt("other", 4);
        let mut invalid = subject.clone();
        invalid.attempt_id.clear();
        assert_eq!(
            empirical_regret(&invalid, std::slice::from_ref(&comparator)).reason,
            "comparison identity is incomplete"
        );
        let mut invalid = subject.clone();
        invalid.verified_success = false;
        assert_eq!(
            empirical_regret(&invalid, std::slice::from_ref(&comparator)).reason,
            "subject did not pass the held-out verifier"
        );
        let mut invalid = subject.clone();
        invalid.trajectory.coverage = Coverage::Partial;
        assert_eq!(
            empirical_regret(&invalid, std::slice::from_ref(&comparator)).reason,
            "subject telemetry is incomplete"
        );
        let mut invalid = subject.clone();
        invalid.trajectory.model_tool_requests = None;
        assert_eq!(
            empirical_regret(&invalid, std::slice::from_ref(&comparator)).reason,
            "subject interaction count is unknown"
        );
        let report = empirical_regret(&subject, &[comparator.clone(), comparator]);
        assert_eq!(report.matched_successful_attempts, vec!["other"]);
        assert_eq!(report.extra_interactions, Some(4));
        let mut unknown = attempt("unknown", 1);
        unknown.trajectory.model_tool_requests = None;
        let report = empirical_regret(&subject, &[unknown, attempt("known", 5)]);
        assert_eq!(report.matched_successful_attempts, vec!["known"]);
        assert_eq!(report.extra_interactions, Some(3));
    }
}
