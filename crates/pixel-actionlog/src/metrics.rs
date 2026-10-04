// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Workflow estimator v2. These are policy assumptions, not measured averages:
//! 4 KiB per distinct returned evidence file and 1 KiB per native command or
//! returned relationship inspected. Known evidence bytes replace file assumptions.
//! v2 adds the measured whole-file read: a command that stands in for reading
//! one file (`list-signatures`) records that file's size as its known bytes and
//! no native command, and the live line compares it with the answer on stdout.
//! No source, graph, or native comparison operation is performed here. Tokens are
//! always UTF-8 bytes / 4, including the live reporting line when emitted.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ActionEvent;

/// Version history: `workflow-v1` estimated every file at 4 KiB; `workflow-v2`
/// records the measured size of the one file `list-signatures` stands in for.
pub const ESTIMATOR_VERSION: &str = "workflow-v2";
pub const TIME_ESTIMATOR_VERSION: &str = "sequential-v1";
pub const DEFAULT_ROUND_TRIP_MS: u64 = 2000;

/// Why a meaningful native-workflow comparison is absent from a record.
/// Recorded when the metrics are built rather than inferred at render time,
/// so a live line and its replay state the same reason — except
/// [`Self::ZeroStep`], which is inferred from the recorded evidence at render
/// time (deterministically, so a replay states the same cause). `None` with
/// no evidence means the record predates this field, never "a baseline
/// exists".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonGap {
    /// `native_commands` has no policy entry for this command.
    NoPolicy,
    /// The operation failed before returning evidence.
    OperationFailed,
    /// The rendered output cap hid the evidence from the caller.
    OutputTruncated,
    /// Evidence traversal exceeded its depth limit.
    EvidenceDepthCapped,
    /// The evidence accumulator was not initialized, or its lock failed.
    Uninitialized,
    /// No command or file-read steps: the sequential baseline is undefined.
    /// Never serialized in `comparison_gap`: the renderer infers it from
    /// zero-step evidence instead.
    ZeroStep,
}

impl ComparisonGap {
    /// The clause the live line renders after `unavailable: `. Exhaustive so a
    /// new gap cannot render as a silent absence.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NoPolicy => "no native-workflow baseline is defined for this command",
            Self::OperationFailed => "the operation failed before returning evidence",
            Self::OutputTruncated => "the rendered output cap hid the evidence",
            Self::EvidenceDepthCapped => "evidence traversal exceeded its depth limit",
            Self::Uninitialized => "evidence collection was not initialized",
            Self::ZeroStep => "no command or file-read steps; the sequential baseline is undefined",
        }
    }
}

static INVOCATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn invocation_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{}-{nanos:x}-{:x}",
        std::process::id(),
        INVOCATION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

/// Counts refer only to evidence returned by this operation. Callers deduplicate
/// file paths before passing them. Never use repository size or unseen results.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowEvidence {
    pub distinct_files: u64,
    pub relationships: u64,
    pub native_commands: u64,
    pub known_file_bytes: Option<u64>,
    pub partial: bool,
}

impl WorkflowEvidence {
    pub fn estimated_bytes(&self) -> u64 {
        self.known_file_bytes
            .unwrap_or_else(|| self.distinct_files.saturating_mul(4096))
            .saturating_add(
                self.native_commands
                    .saturating_add(self.relationships)
                    .saturating_mul(1024),
            )
    }

    /// The measured size of the files read in full, when that is the whole baseline.
    ///
    /// `Some` only when the known bytes are the entire native workflow: no
    /// native command and no relationship adds an assumed kilobyte on top, so
    /// the baseline is exactly "read these files", a measurement and not a
    /// policy estimate.
    pub fn measured_file_read(&self) -> Option<u64> {
        if self.native_commands == 0 && self.relationships == 0 {
            self.known_file_bytes
        } else {
            None
        }
    }
}

/// Rough sequential workflow scenario, not a latency measurement. Each file
/// read and native command is one assumed round trip; relationships affect only
/// token volume. The common first round trip cancels. Native command execution
/// time is assumed zero; elapsed Pixel execution is the measured subtraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowTimeEstimate {
    pub estimator_version: String,
    pub round_trip_ms: u64,
    pub native_command_ms: u64,
    pub sequential_steps: u64,
    pub saved_ms: f64,
}

impl WorkflowTimeEstimate {
    fn from_evidence(
        evidence: &WorkflowEvidence,
        duration_us: u64,
        round_trip_ms: u64,
    ) -> Option<Self> {
        let sequential_steps = evidence
            .native_commands
            .saturating_add(evidence.distinct_files);
        if sequential_steps == 0 {
            return None;
        }
        Some(Self {
            estimator_version: TIME_ESTIMATOR_VERSION.to_owned(),
            round_trip_ms,
            native_command_ms: 0,
            sequential_steps,
            saved_ms: sequential_steps.saturating_sub(1) as f64 * round_trip_ms as f64
                - duration_us as f64 / 1000.0,
        })
    }
}

/// Measured facts and versioned estimates remain separate in the serialized log.
/// `None` evidence means a meaningful native comparison is unavailable; explicit
/// zero evidence is a meaningful zero token baseline. Sequential time remains
/// unavailable when there are no command/file-read steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationMetrics {
    pub duration_us: u64,
    /// Describes which rendered streams `output_bytes` measures. Absence means
    /// unspecified (including records written before this field existed).
    /// `cli-rendered-streams` means CLI-owned rendered stdout and diagnostics;
    /// it excludes writes owned by lower-level libraries or subprocesses.
    /// Reporting is accounted separately in `reporting_bytes` in every scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_scope: Option<String>,
    pub output_bytes: u64,
    /// Bytes rendered on stdout alone: the answer, without diagnostics or
    /// the reporting block. Absent on records written before this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_bytes: Option<u64>,
    pub reporting_bytes: u64,
    pub estimator_version: String,
    pub native_workflow_bytes: Option<u64>,
    pub evidence: Option<WorkflowEvidence>,
    /// Why no native-workflow comparison exists. `None` when one does, on a
    /// record written before this field, or on zero-step evidence, whose
    /// reason the renderer infers; any other absence is rendered as
    /// unrecorded, never as an explained gap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison_gap: Option<ComparisonGap>,
    /// Missing in older records; never backfill an unrecorded time assumption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_estimate: Option<WorkflowTimeEstimate>,
}

impl OperationMetrics {
    pub fn new(duration: Duration, output_bytes: u64, evidence: Option<WorkflowEvidence>) -> Self {
        let duration_us = duration.as_micros().min(u64::MAX as u128) as u64;
        Self {
            duration_us,
            output_scope: None,
            output_bytes,
            answer_bytes: None,
            reporting_bytes: 0,
            estimator_version: ESTIMATOR_VERSION.to_owned(),
            native_workflow_bytes: evidence.as_ref().map(WorkflowEvidence::estimated_bytes),
            time_estimate: evidence.as_ref().and_then(|evidence| {
                WorkflowTimeEstimate::from_evidence(evidence, duration_us, DEFAULT_ROUND_TRIP_MS)
            }),
            evidence,
            comparison_gap: None,
        }
    }

    /// Override only this invocation's sequential round-trip policy assumption.
    /// Zero is valid. Unavailable/zero-step evidence remains unavailable.
    pub fn with_round_trip_ms(mut self, round_trip_ms: u64) -> Self {
        self.time_estimate = self.evidence.as_ref().and_then(|evidence| {
            WorkflowTimeEstimate::from_evidence(evidence, self.duration_us, round_trip_ms)
        });
        self
    }

    /// Record why this operation has no native-workflow comparison. The gap
    /// is consulted only when the evidence (and so the baseline) is absent;
    /// evidence wins at render time.
    pub fn with_comparison_gap(mut self, gap: ComparisonGap) -> Self {
        self.comparison_gap = Some(gap);
        self
    }

    pub fn saved_time_ms(&self) -> Option<f64> {
        self.time_estimate
            .as_ref()
            .map(|estimate| estimate.saved_ms)
    }

    pub fn output_tokens(&self) -> f64 {
        (self.output_bytes as f64 + self.reporting_bytes as f64) / 4.0
    }

    pub fn saved_tokens(&self) -> Option<f64> {
        Some(self.native_workflow_bytes? as f64 / 4.0 - self.output_tokens())
    }

    pub fn partial(&self) -> bool {
        self.evidence.as_ref().is_some_and(|e| e.partial)
    }
}

impl ActionEvent {
    pub fn with_metrics(mut self, metrics: OperationMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Call once before logging/emitting a live line. Repeated calls are
    /// idempotent. Disabled/stream-only reporting should not call this method.
    /// The returned string excludes its newline, which is still accounted for.
    pub fn finalize_metrics_line(&mut self) -> Option<String> {
        // Width can only change near digit boundaries. Taking the maximum
        // prevents two-value oscillation at a negative-savings boundary.
        for _ in 0..8 {
            let line = format_metrics_line(self)?;
            let bytes = line.len() as u64 + 2;
            let metrics = self.metrics.as_mut()?;
            if metrics.reporting_bytes == bytes {
                return Some(line);
            }
            metrics.reporting_bytes = metrics.reporting_bytes.max(bytes);
        }
        format_metrics_line(self)
    }
}

/// Reproduce the invocation's authoritative line from its finalized record.
/// Never consult a global latest record: native hosts correlate invocation IDs.
pub fn format_metrics_line(event: &ActionEvent) -> Option<String> {
    let metrics = event.metrics.as_ref()?;
    // Prevent command/control characters from entering the line.
    let command = event
        .command
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>();
    let duration_ms = metrics.duration_us as f64 / 1000.0;
    // Evaluate the user-facing savings claim from the command payload only.
    // Reporting bytes remain fully accounted for, but including them here
    // makes whether a savings row exists depend on its own emitted size.
    let payload_tok = metrics.output_bytes as f64 / 4.0;
    let id = event.invocation_id.as_deref().unwrap_or("legacy");
    let short_id: String = id.split('-').nth(1).map_or_else(
        || id.to_owned(),
        |s| {
            s.chars()
                .rev()
                .take(6)
                .collect::<String>()
                .chars()
                .rev()
                .collect()
        },
    );

    // Line 1: identity header.
    let header = format!("🟩 pixel {command} ❀ {duration_ms:.1}ms ❀ #{short_id}");

    // Commands without a comparison policy only need identity and duration.
    // Keep the gap in the record, and let actual evidence take precedence.
    if metrics.evidence.is_none() && metrics.comparison_gap == Some(ComparisonGap::NoPolicy) {
        return Some(header);
    }

    let partial_tag = if metrics.partial() { ", partial" } else { "" };
    let unavailable = format!("unavailable: {}", comparison_gap_reason(metrics));

    // Keep the estimate readable in terminal UIs that render block characters
    // as low-contrast progress tracks. A negative estimate is overhead, not
    // negative savings, so never express it as a misleading percentage.
    let time_section = if let Some(saved_ms) = metrics.saved_time_ms() {
        let native_time_ms = saved_ms + duration_ms;
        if native_time_ms > 0.0 {
            if saved_ms > 0.0 {
                let pct = (saved_ms / native_time_ms * 100.0).round() as i64;
                format!(
                    "{pct}% faster, {duration_ms:.1}ms against ~{native_time_ms:.0}ms estimated{partial_tag}"
                )
            } else if saved_ms < 0.0 {
                format!(
                    "+{:.1}ms overhead, {duration_ms:.1}ms against ~{native_time_ms:.0}ms estimated{partial_tag}",
                    -saved_ms
                )
            } else {
                format!(
                    "{duration_ms:.1}ms against ~{native_time_ms:.0}ms estimated · no estimated time saving{partial_tag}"
                )
            }
        } else {
            // A one-step baseline (or a zero round-trip assumption) saves no
            // round trip: say so instead of omitting the row.
            format!("no estimated time saving (baseline has no saved round trip){partial_tag}")
        }
    } else {
        unavailable.clone()
    };

    let measured_read = metrics
        .evidence
        .as_ref()
        .and_then(WorkflowEvidence::measured_file_read)
        .zip(metrics.answer_bytes);
    let token_section = if metrics.answer_bytes == Some(0) {
        // Nothing reached stdout, so there is no answer to have saved context
        // on: a baseline minus an empty payload is not a saving.
        format!("no estimated context saving (empty output){partial_tag}")
    } else if let Some((file_bytes, answer_bytes)) = measured_read {
        format!(
            "{}{partial_tag}",
            measured_read_section(file_bytes, answer_bytes)
        )
    } else if let Some(native_bytes) = metrics.native_workflow_bytes {
        let native_tok = native_bytes as f64 / 4.0;
        if native_tok > 0.0 {
            let saved_tok = native_tok - payload_tok;
            if saved_tok > 0.0 {
                let pct = (saved_tok / native_tok * 100.0).round() as i64;
                format!(
                    "estimated LLM context saved: ~{saved_tok:.0} tok ({pct}%) against ~{native_tok:.0} tok estimated{partial_tag}"
                )
            } else {
                // The comparison exists, it is just not a saving: state the
                // rendered volume against the baseline instead of dropping it.
                format!(
                    "no estimated context saving (rendered output meets or exceeds the ~{native_tok:.0} tok baseline){partial_tag}"
                )
            }
        } else {
            format!("no estimated context saving (baseline is zero bytes){partial_tag}")
        }
    } else {
        unavailable
    };

    let rows = [
        format!("  ├─ ⏱ {time_section}"),
        format!("  ├─ § {token_section}"),
    ];
    let stem = "  │";
    let separator = "  └────────────────────────────────────────────────────────";

    let mut line = format!("{header}\n{stem}\n{}\n{stem}\n{separator}", rows.join("\n"));
    // Padding resolves the rare digit-boundary fixed-point oscillation without
    // lying about emitted overhead. Bound it when replaying malformed records.
    let padding = metrics
        .reporting_bytes
        .saturating_sub(line.len() as u64 + 2)
        .min(64);
    line.extend(std::iter::repeat_n(' ', padding as usize));
    Some(line)
}

/// The token row of a measured whole-file read: both sides, then the saving.
///
/// Counts are UTF-8 bytes divided by four, rounded down, on both sides: the
/// rule of `scripts/bench-read-savings.sh`, so the line and the published
/// table can be re-derived with `wc -c` and agree to the token.
fn measured_read_section(file_bytes: u64, answer_bytes: u64) -> String {
    let full_tok = read_tokens(file_bytes);
    let answer_tok = read_tokens(answer_bytes);
    let both = format!("full read {full_tok} tok, pixel answer {answer_tok} tok");
    match saved_percent(full_tok, answer_tok) {
        Some(pct) => format!("{both} (-{pct}%)"),
        None => format!("{both} (no saving)"),
    }
}

/// Tokens a measured read counts for `bytes` of UTF-8: divided by four,
/// rounded down, the rule `scripts/bench-read-savings.sh` publishes.
pub fn read_tokens(bytes: u64) -> u64 {
    bytes / 4
}

/// The saving of an answer over the whole read, in whole percent rounded to
/// nearest; `None` when the answer is not smaller, so no caller can print a
/// zero or negative saving.
pub fn saved_percent(full_tok: u64, answer_tok: u64) -> Option<i64> {
    (answer_tok < full_tok)
        .then(|| (100.0 * (1.0 - answer_tok as f64 / full_tok as f64)).round() as i64)
}

/// The clause after `unavailable: ` for a record with no comparison. The gap
/// recorded at construction wins; a baseline that observed no command or
/// file-read steps is the zero-step shape; a record older than the gap field
/// says the reason was not recorded instead of presenting absence as
/// unexplained.
fn comparison_gap_reason(metrics: &OperationMetrics) -> &'static str {
    if let Some(gap) = metrics.comparison_gap {
        return gap.reason();
    }
    if metrics.evidence.as_ref().is_some_and(|evidence| {
        evidence
            .native_commands
            .saturating_add(evidence.distinct_files)
            == 0
    }) {
        return ComparisonGap::ZeroStep.reason();
    }
    "reason not recorded (record predates gap tracking)"
}

/// Summarize only finalized operation records. Duplicate invocation IDs count
/// once (first record wins); legacy records without metrics remain separate.
/// Each estimator version and coverage class has its own totals: partial and
/// unavailable baselines are never blended into complete workflow comparisons.
pub fn summarize_metrics(events: &[ActionEvent]) -> serde_json::Value {
    #[derive(Default)]
    struct Totals {
        operations: u64,
        duration_us: u64,
        output_bytes: u64,
        reporting_bytes: u64,
        native_workflow_bytes: u64,
        output_scopes: std::collections::BTreeSet<String>,
    }
    #[derive(Default)]
    struct TimeTotals {
        operations: u64,
        duration_us: u64,
        sequential_steps: u64,
        saved_ms: f64,
    }

    let mut seen = std::collections::HashSet::new();
    let mut duplicate_records = 0;
    let mut legacy_records = 0;
    let mut unavailable_records = 0;
    let mut time_unavailable_records = 0;
    // Token estimates keep their existing aggregation contract. Time estimates
    // have a separate grouping because round-trip assumptions may vary between
    // invocations even when the token estimator and coverage are identical.
    let mut time_groups: std::collections::BTreeMap<(&str, &str, u64, u64, &str), TimeTotals> =
        std::collections::BTreeMap::new();
    let mut versions: std::collections::BTreeMap<&str, std::collections::BTreeMap<&str, Totals>> =
        std::collections::BTreeMap::new();
    for event in events {
        if let Some(id) = event.invocation_id.as_deref()
            && !seen.insert(id)
        {
            duplicate_records += 1;
            continue;
        }
        let Some(metrics) = event.metrics.as_ref() else {
            legacy_records += 1;
            continue;
        };
        let coverage = if metrics.native_workflow_bytes.is_none() {
            unavailable_records += 1;
            "unavailable"
        } else if metrics.partial() {
            "partial"
        } else {
            "complete"
        };
        if let Some(time) = metrics.time_estimate.as_ref() {
            let totals = time_groups
                .entry((
                    metrics.estimator_version.as_str(),
                    time.estimator_version.as_str(),
                    time.round_trip_ms,
                    time.native_command_ms,
                    coverage,
                ))
                .or_default();
            totals.operations += 1;
            totals.duration_us = totals.duration_us.saturating_add(metrics.duration_us);
            totals.sequential_steps = totals
                .sequential_steps
                .saturating_add(time.sequential_steps);
            totals.saved_ms += time.saved_ms;
        } else {
            time_unavailable_records += 1;
        }
        let totals = versions
            .entry(&metrics.estimator_version)
            .or_default()
            .entry(coverage)
            .or_default();
        totals.operations += 1;
        totals.output_scopes.insert(
            metrics
                .output_scope
                .as_deref()
                .unwrap_or("unspecified")
                .to_owned(),
        );
        totals.duration_us = totals.duration_us.saturating_add(metrics.duration_us);
        totals.output_bytes = totals.output_bytes.saturating_add(metrics.output_bytes);
        totals.reporting_bytes = totals
            .reporting_bytes
            .saturating_add(metrics.reporting_bytes);
        totals.native_workflow_bytes = totals
            .native_workflow_bytes
            .saturating_add(metrics.native_workflow_bytes.unwrap_or(0));
    }
    let versions: serde_json::Map<String, serde_json::Value> = versions
        .into_iter()
        .map(|(version, groups)| {
            let groups: serde_json::Map<String, serde_json::Value> = groups
                .into_iter()
                .map(|(coverage, totals)| {
                    let output_tokens =
                        (totals.output_bytes as f64 + totals.reporting_bytes as f64) / 4.0;
                    let native_bytes =
                        (coverage != "unavailable").then_some(totals.native_workflow_bytes);
                    let native_tokens = native_bytes.map(|bytes| bytes as f64 / 4.0);
                    (
                        coverage.to_owned(),
                        serde_json::json!({
                            "operations": totals.operations,
                            "measured": {
                                "duration_us": totals.duration_us,
                                "output_bytes": totals.output_bytes,
                                "reporting_bytes": totals.reporting_bytes,
                                "output_scopes": totals.output_scopes,
                            },
                            "estimated": {
                                "native_workflow_bytes": native_bytes,
                                "output_tokens": output_tokens,
                                "native_workflow_tokens": native_tokens,
                                "saved_tokens": native_tokens.map(|tokens| tokens - output_tokens),
                            },
                        }),
                    )
                })
                .collect();
            (version.to_owned(), serde_json::Value::Object(groups))
        })
        .collect();
    let time_estimates: Vec<serde_json::Value> = time_groups.into_iter().map(|((token_version, time_version, round_trip_ms, native_command_ms, coverage), totals)| {
        serde_json::json!({
            "token_estimator_version": token_version,
            "time_estimator_version": time_version,
            "round_trip_ms": round_trip_ms,
            "native_command_ms": native_command_ms,
            "coverage": coverage,
            "operations": totals.operations,
            "measured": { "duration_us": totals.duration_us },
            "estimated": { "sequential_steps": totals.sequential_steps, "saved_ms": totals.saved_ms },
        })
    }).collect();
    serde_json::json!({
        "record_count": events.len(),
        "unique_records": events.len() - duplicate_records,
        "duplicate_records": duplicate_records,
        "legacy_records": legacy_records,
        "unavailable_records": unavailable_records,
        "time_unavailable_records": time_unavailable_records,
        "token_approximation": "UTF-8 bytes / 4 (including reporting overhead)",
        "versions": versions,
        "time_estimates": time_estimates,
        "time_approximation": "sequential one-round-trip-per-command-or-file-read scenario; common first round trip cancels; native command runtime assumed zero; batching or parallel calls may save less",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequential_time_cancels_first_round_trip_and_excludes_relationships() {
        let metrics = OperationMetrics::new(
            Duration::from_micros(12400),
            100,
            Some(WorkflowEvidence {
                native_commands: 1,
                distinct_files: 3,
                relationships: 900,
                partial: true,
                ..Default::default()
            }),
        );
        let time = metrics.time_estimate.as_ref().unwrap();
        assert_eq!(time.estimator_version, "sequential-v1");
        assert_eq!(time.round_trip_ms, 2000);
        assert_eq!(time.native_command_ms, 0);
        assert_eq!(time.sequential_steps, 4);
        assert_eq!(metrics.saved_time_ms(), Some(5987.6));
        assert_eq!(
            metrics.clone().with_round_trip_ms(500).saved_time_ms(),
            Some(1487.6)
        );
        assert_eq!(metrics.with_round_trip_ms(0).saved_time_ms(), Some(-12.4));
    }

    #[test]
    fn one_step_time_is_negative_or_zero_and_missing_zero_step_evidence_is_unavailable() {
        let one = WorkflowEvidence {
            native_commands: 1,
            ..Default::default()
        };
        assert_eq!(
            OperationMetrics::new(Duration::from_micros(12400), 0, Some(one.clone()))
                .saved_time_ms(),
            Some(-12.4)
        );
        assert_eq!(
            OperationMetrics::new(Duration::ZERO, 0, Some(one)).saved_time_ms(),
            Some(0.0)
        );
        assert!(
            OperationMetrics::new(Duration::ZERO, 0, Some(WorkflowEvidence::default()))
                .time_estimate
                .is_none()
        );
        assert!(
            OperationMetrics::new(Duration::ZERO, 0, None)
                .with_round_trip_ms(0)
                .time_estimate
                .is_none()
        );
    }

    #[test]
    fn older_metrics_never_invent_time_estimates_and_new_records_round_trip() {
        let old: OperationMetrics = serde_json::from_str(r#"{"duration_us":21,"output_bytes":120,"reporting_bytes":0,"estimator_version":"workflow-v1","native_workflow_bytes":5120,"evidence":{"distinct_files":1,"relationships":0,"native_commands":1,"known_file_bytes":null,"partial":false}}"#).unwrap();
        assert!(old.time_estimate.is_none());
        assert_eq!(old.saved_time_ms(), None);
        let new = OperationMetrics::new(Duration::from_millis(20), 120, old.evidence)
            .with_round_trip_ms(2500);
        let decoded: OperationMetrics =
            serde_json::from_value(serde_json::to_value(&new).unwrap()).unwrap();
        assert_eq!(decoded.time_estimate.unwrap().round_trip_ms, 2500);
        let mut event = ActionEvent::new("impact", "run").with_metrics(new);
        let line = event.finalize_metrics_line().unwrap();
        assert!(line.starts_with("🟩 pixel impact"));
        assert!(line.contains("saved"));
        assert!(line.contains("against ~2500ms estimated"));
        assert_eq!(
            event.metrics.unwrap().reporting_bytes,
            line.len() as u64 + 2
        );
    }

    #[test]
    fn time_summary_separates_versions_assumptions_coverage_and_missing_history() {
        let make = |round_trip_ms, partial| {
            ActionEvent::new("impact", "run").with_metrics(
                OperationMetrics::new(
                    Duration::from_millis(20),
                    40,
                    Some(WorkflowEvidence {
                        native_commands: 1,
                        distinct_files: 2,
                        partial,
                        ..Default::default()
                    }),
                )
                .with_round_trip_ms(round_trip_ms),
            )
        };
        let a = make(2000, false);
        let b = make(500, false);
        let partial = make(2000, true);
        let mut future_time = make(2000, false);
        future_time
            .metrics
            .as_mut()
            .unwrap()
            .time_estimate
            .as_mut()
            .unwrap()
            .estimator_version = "sequential-v2".to_owned();
        let mut future_tokens = make(2000, false);
        future_tokens.metrics.as_mut().unwrap().estimator_version = "workflow-v3".to_owned();
        let mut old_metrics = make(2000, false);
        old_metrics.metrics.as_mut().unwrap().time_estimate = None;
        let unavailable = ActionEvent::new("index", ".").with_metrics(OperationMetrics::new(
            Duration::from_millis(4),
            0,
            None,
        ));
        let legacy = ActionEvent::new("search", "a").with_savings(20, 8000);
        let summary = summarize_metrics(&[
            a.clone(),
            a,
            b,
            partial,
            future_time,
            future_tokens,
            old_metrics,
            unavailable,
            legacy,
        ]);
        assert_eq!(summary["duplicate_records"], 1);
        assert_eq!(summary["legacy_records"], 1);
        assert_eq!(summary["time_unavailable_records"], 2);
        let groups = summary["time_estimates"].as_array().unwrap();
        assert_eq!(groups.len(), 5);
        assert!(groups.iter().all(|group| group["operations"] == 1
            && group["measured"]["duration_us"] == 20000
            && group["estimated"]["sequential_steps"] == 3));
        let group = groups
            .iter()
            .find(|group| group["round_trip_ms"] == 500)
            .unwrap();
        assert_eq!(group["estimated"]["saved_ms"], 980.0);
        assert_eq!(
            groups
                .iter()
                .filter(|group| group["coverage"] == "partial")
                .count(),
            1
        );
        assert!(
            groups
                .iter()
                .any(|group| group["time_estimator_version"] == "sequential-v2")
        );
        assert!(
            groups
                .iter()
                .any(|group| group["token_estimator_version"] == "workflow-v3")
        );
    }

    #[test]
    fn time_summary_retains_negative_and_zero_estimates() {
        let make = |duration| {
            ActionEvent::new("search", "needle").with_metrics(OperationMetrics::new(
                duration,
                0,
                Some(WorkflowEvidence {
                    native_commands: 1,
                    ..Default::default()
                }),
            ))
        };
        let summary = summarize_metrics(&[make(Duration::from_millis(12)), make(Duration::ZERO)]);
        let group = &summary["time_estimates"][0];
        assert_eq!(group["operations"], 2);
        assert_eq!(group["measured"]["duration_us"], 12000);
        assert_eq!(group["estimated"]["saved_ms"], -12.0);
        assert_eq!(group["estimated"]["sequential_steps"], 2);
    }

    #[test]
    fn output_scope_defaults_unspecified_and_round_trips_explicit_cli_scope() {
        let old_json = r#"{"duration_us":21,"output_bytes":120,"reporting_bytes":0,"estimator_version":"workflow-v1","native_workflow_bytes":null,"evidence":null}"#;
        let mut metrics: OperationMetrics = serde_json::from_str(old_json).unwrap();
        assert_eq!(metrics.output_scope, None);
        assert!(
            serde_json::to_value(&metrics)
                .unwrap()
                .get("output_scope")
                .is_none()
        );
        metrics.output_scope = Some("cli-rendered-streams".to_owned());
        let reloaded: OperationMetrics =
            serde_json::from_value(serde_json::to_value(&metrics).unwrap()).unwrap();
        assert_eq!(
            reloaded.output_scope.as_deref(),
            Some("cli-rendered-streams")
        );
        let event = ActionEvent::new("index", ".").with_metrics(reloaded);
        let summary = summarize_metrics(&[event]);
        assert_eq!(
            summary["versions"]["workflow-v1"]["unavailable"]["measured"]["output_scopes"],
            serde_json::json!(["cli-rendered-streams"])
        );
    }

    #[test]
    fn negative_zero_partial_and_unavailable_are_distinct() {
        let evidence = WorkflowEvidence {
            distinct_files: 2,
            relationships: 3,
            native_commands: 1,
            partial: true,
            ..Default::default()
        };
        let metrics = OperationMetrics::new(Duration::from_micros(12400), 12896, Some(evidence));
        assert_eq!(metrics.native_workflow_bytes, Some(12288));
        assert_eq!(metrics.saved_tokens(), Some(-152.0));
        assert!(metrics.partial());
        let zero = OperationMetrics::new(Duration::ZERO, 0, Some(WorkflowEvidence::default()));
        assert_eq!(zero.saved_tokens(), Some(0.0));
        assert_eq!(
            OperationMetrics::new(Duration::ZERO, 0, None).saved_tokens(),
            None
        );
    }

    #[test]
    fn known_evidence_bytes_replace_file_assumptions() {
        let evidence = WorkflowEvidence {
            distinct_files: 5,
            known_file_bytes: Some(31),
            native_commands: 1,
            ..Default::default()
        };
        assert_eq!(evidence.estimated_bytes(), 1055);
    }

    #[test]
    fn reporting_is_accounted_and_finalizing_is_idempotent() {
        let mut event =
            ActionEvent::new("impact", "target=run").with_metrics(OperationMetrics::new(
                Duration::from_micros(12400),
                3280,
                Some(WorkflowEvidence {
                    distinct_files: 4,
                    partial: true,
                    ..Default::default()
                }),
            ));
        let line = event.finalize_metrics_line().unwrap();
        assert!(line.contains("12.4ms"));
        assert!(line.contains("partial"));
        assert_eq!(
            event.metrics.as_ref().unwrap().reporting_bytes,
            line.len() as u64 + 2
        );
        assert_eq!(
            event.finalize_metrics_line().as_deref(),
            Some(line.as_str())
        );
        assert_eq!(format_metrics_line(&event).unwrap(), line);
    }

    #[test]
    fn reporting_matches_actual_bytes_across_digit_and_sign_boundaries() {
        for output_bytes in 0..12000 {
            let mut event = ActionEvent::new("impact", "run").with_metrics(OperationMetrics::new(
                Duration::from_micros(12400),
                output_bytes,
                Some(WorkflowEvidence {
                    distinct_files: 1,
                    ..Default::default()
                }),
            ));
            let line = event.finalize_metrics_line().unwrap();
            let metrics = event.metrics.as_ref().unwrap();
            assert_eq!(metrics.reporting_bytes, line.len() as u64 + 2);
            assert_eq!(
                metrics.output_tokens(),
                (output_bytes + line.len() as u64 + 2) as f64 / 4.0
            );
            assert_eq!(
                metrics.saved_tokens(),
                Some(1024.0 - metrics.output_tokens())
            );
        }
    }

    #[test]
    fn no_reporting_has_zero_overhead_and_keeps_error_outcome() {
        let event = ActionEvent::new("search", "[")
            .with_result(
                &Err("invalid pattern".to_owned()),
                Duration::from_micros(900),
            )
            .with_metrics(OperationMetrics::new(Duration::from_micros(900), 41, None));
        assert_eq!(event.outcome, crate::Outcome::Error);
        assert_eq!(event.error.as_deref(), Some("invalid pattern"));
        let metrics = event.metrics.as_ref().unwrap();
        assert_eq!(metrics.reporting_bytes, 0);
        assert_eq!(metrics.output_tokens(), 10.25);
        assert_eq!(metrics.duration_us, 900);
    }

    #[test]
    fn legacy_json_has_no_invented_metrics_or_identity() {
        let event: ActionEvent = serde_json::from_str(r#"{"ts_ms":1,"pid":2,"command":"search","args":"abc","cwd":"/tmp","outcome":"ok","duration_ms":5,"snippet_cap_chars":5,"pool_chars":20}"#).unwrap();
        assert!(event.metrics.is_none());
        assert!(event.invocation_id.is_none());
        assert_eq!(event.savings_ratio(), Some(0.75));
    }

    #[test]
    fn concurrent_ids_are_unique_and_records_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("actions.jsonl");
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let path = &path;
                scope.spawn(move || {
                    let mut log = crate::ActionLog::spawn_at(path.clone());
                    for _ in 0..10 {
                        let event = ActionEvent::new("search", "needle").with_metrics(
                            OperationMetrics::new(Duration::from_micros(99), 43, None),
                        );
                        log.log(event);
                    }
                    // The records are read back below: `finish` returns before
                    // the writer drains, so under load some would be missing.
                    log.finish_flush();
                });
            }
        });
        let events = crate::tail(&path, 100).unwrap();
        assert_eq!(events.len(), 80);
        let ids = events
            .iter()
            .map(|e| e.invocation_id.as_ref().unwrap())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), 80);
        assert!(
            events
                .iter()
                .all(|e| e.metrics.as_ref().unwrap().duration_us == 99)
        );
    }

    #[test]
    fn summary_deduplicates_and_separates_legacy_partial_unavailable_and_versions() {
        let complete = ActionEvent::new("impact", "run").with_metrics(OperationMetrics::new(
            Duration::from_micros(12),
            5000,
            Some(WorkflowEvidence {
                distinct_files: 1,
                ..Default::default()
            }),
        ));
        let partial = ActionEvent::new("targets", "task").with_metrics(OperationMetrics::new(
            Duration::from_micros(18),
            40,
            Some(WorkflowEvidence {
                native_commands: 1,
                partial: true,
                ..Default::default()
            }),
        ));
        let unavailable = ActionEvent::new("index", ".").with_metrics(OperationMetrics::new(
            Duration::from_micros(30),
            120,
            None,
        ));
        let mut legacy = ActionEvent::new("search", "a").with_savings(20, 8000);
        legacy.invocation_id = None;
        let mut future = ActionEvent::new("impact", "run").with_metrics(OperationMetrics::new(
            Duration::from_micros(3),
            0,
            Some(WorkflowEvidence::default()),
        ));
        future.metrics.as_mut().unwrap().estimator_version = "workflow-v3".to_owned();
        let summary = summarize_metrics(&[
            complete.clone(),
            complete,
            partial,
            unavailable,
            legacy.clone(),
            legacy,
            future,
        ]);
        assert_eq!(summary["record_count"], 7);
        assert_eq!(summary["unique_records"], 6);
        assert_eq!(summary["duplicate_records"], 1);
        assert_eq!(summary["legacy_records"], 2);
        assert_eq!(summary["unavailable_records"], 1);
        let v1 = &summary["versions"][ESTIMATOR_VERSION];
        assert_eq!(v1["complete"]["operations"], 1);
        assert_eq!(v1["complete"]["measured"]["duration_us"], 12);
        assert_eq!(v1["complete"]["estimated"]["saved_tokens"], -226.0);
        assert_eq!(v1["partial"]["estimated"]["saved_tokens"], 246.0);
        assert!(v1["unavailable"]["estimated"]["saved_tokens"].is_null());
        assert_eq!(v1["unavailable"]["measured"]["output_bytes"], 120);
        assert_eq!(
            summary["versions"]["workflow-v3"]["complete"]["estimated"]["saved_tokens"],
            0.0
        );
    }

    #[test]
    fn summary_counts_live_reporting_bytes_once_and_empty_has_no_baseline() {
        let mut event = ActionEvent::new("search", "needle").with_metrics(OperationMetrics::new(
            Duration::from_micros(21),
            300,
            Some(WorkflowEvidence {
                distinct_files: 1,
                ..Default::default()
            }),
        ));
        let line = event.finalize_metrics_line().unwrap();
        let summary = summarize_metrics(&[event.clone(), event]);
        let group = &summary["versions"][ESTIMATOR_VERSION]["complete"];
        assert_eq!(group["measured"]["reporting_bytes"], line.len() + 2);
        assert_eq!(
            group["estimated"]["output_tokens"],
            (300 + line.len() + 2) as f64 / 4.0
        );
        let empty = summarize_metrics(&[]);
        assert_eq!(empty["record_count"], 0);
        assert_eq!(empty["versions"], serde_json::json!({}));
    }

    #[test]
    fn every_comparison_gap_reason_is_distinct_and_states_its_cause() {
        let gaps = [
            ComparisonGap::NoPolicy,
            ComparisonGap::OperationFailed,
            ComparisonGap::OutputTruncated,
            ComparisonGap::EvidenceDepthCapped,
            ComparisonGap::Uninitialized,
            ComparisonGap::ZeroStep,
        ];
        assert_eq!(
            gaps.map(ComparisonGap::reason),
            [
                "no native-workflow baseline is defined for this command",
                "the operation failed before returning evidence",
                "the rendered output cap hid the evidence",
                "evidence traversal exceeded its depth limit",
                "evidence collection was not initialized",
                "no command or file-read steps; the sequential baseline is undefined",
            ]
        );
        let unique: std::collections::HashSet<&str> = gaps.iter().map(|gap| gap.reason()).collect();
        assert_eq!(unique.len(), gaps.len());
    }

    #[test]
    fn metrics_should_render_only_identity_when_no_policy_applies() {
        let metrics = OperationMetrics::new(Duration::from_millis(3), 100, None)
            .with_comparison_gap(ComparisonGap::NoPolicy);
        let mut event = ActionEvent::new("install", ".").with_metrics(metrics);
        event.invocation_id = Some("test-abcdef".to_owned());
        let line = event.finalize_metrics_line().unwrap();
        assert_eq!(line, "🟩 pixel install ❀ 3.0ms ❀ #abcdef");
        assert_eq!(
            event.metrics.as_ref().unwrap().reporting_bytes,
            line.len() as u64 + 2
        );
        assert_eq!(event.finalize_metrics_line().unwrap(), line);
        assert_eq!(
            serde_json::to_value(&event).unwrap()["metrics"]["comparison_gap"],
            "no_policy"
        );
    }

    #[test]
    fn metrics_should_keep_comparisons_when_evidence_overrides_no_policy() {
        let metrics = OperationMetrics::new(
            Duration::from_millis(3),
            100,
            Some(WorkflowEvidence {
                native_commands: 2,
                ..Default::default()
            }),
        )
        .with_comparison_gap(ComparisonGap::NoPolicy);
        let event = ActionEvent::new("search", ".").with_metrics(metrics);
        let line = format_metrics_line(&event).unwrap();
        assert!(
            line.contains("├─ ⏱ 100% faster, 3.0ms against ~2000ms estimated"),
            "{line}"
        );
        assert!(
            line.contains(
                "├─ § estimated LLM context saved: ~487 tok (95%) against ~512 tok estimated"
            ),
            "{line}"
        );
    }

    #[test]
    fn zero_step_evidence_is_unavailable_never_a_fabricated_saving() {
        let metrics = OperationMetrics::new(
            Duration::from_millis(3),
            100,
            Some(WorkflowEvidence::default()),
        );
        let event = ActionEvent::new("zero-step", "x").with_metrics(metrics);
        let line = format_metrics_line(&event).unwrap();
        assert!(
            line.contains(
                "  ├─ ⏱ unavailable: no command or file-read steps; the sequential baseline is undefined"
            ),
            "{line}"
        );
        assert!(
            line.contains("  ├─ § no estimated context saving (baseline is zero bytes)"),
            "{line}"
        );
        assert!(!line.contains("faster"), "{line}");
        assert!(!line.contains("context saved"), "{line}");
    }

    #[test]
    fn one_step_and_negative_token_baselines_state_why_they_are_not_savings() {
        let metrics = OperationMetrics::new(
            Duration::from_millis(12),
            4096,
            Some(WorkflowEvidence {
                native_commands: 1,
                ..Default::default()
            }),
        );
        let event = ActionEvent::new("what-changed", ".").with_metrics(metrics);
        let line = format_metrics_line(&event).unwrap();
        assert!(
            line.contains("  ├─ ⏱ no estimated time saving (baseline has no saved round trip)"),
            "{line}"
        );
        assert!(
            line.contains(
                "  ├─ § no estimated context saving (rendered output meets or exceeds the ~256 tok baseline)"
            ),
            "{line}"
        );
        assert!(!line.contains("context saved:"), "{line}");
    }

    #[test]
    fn partial_coverage_is_retained_on_non_positive_rows() {
        let metrics = OperationMetrics::new(
            Duration::from_millis(12),
            4096,
            Some(WorkflowEvidence {
                native_commands: 1,
                partial: true,
                ..Default::default()
            }),
        );
        let event = ActionEvent::new("what-changed", ".").with_metrics(metrics);
        let line = format_metrics_line(&event).unwrap();
        assert!(
            line.contains("no estimated time saving (baseline has no saved round trip), partial"),
            "{line}"
        );
        assert!(
            line.contains(
                "no estimated context saving (rendered output meets or exceeds the ~256 tok baseline), partial"
            ),
            "{line}"
        );
    }

    #[test]
    fn records_without_a_gap_field_say_the_reason_was_not_recorded() {
        let old: OperationMetrics = serde_json::from_str(
            r#"{"duration_us":3000,"output_bytes":100,"reporting_bytes":0,"estimator_version":"workflow-v1","native_workflow_bytes":null,"evidence":null}"#,
        )
        .unwrap();
        assert!(old.comparison_gap.is_none());
        let event = ActionEvent::new("legacy", "x").with_metrics(old);
        let line = format_metrics_line(&event).unwrap();
        assert!(
            line.contains("unavailable: reason not recorded (record predates gap tracking)"),
            "{line}"
        );
    }

    #[test]
    fn saving_overhead_and_zero_rows_pin_their_exact_estimates() {
        let line_for = |evidence: WorkflowEvidence, duration_ms: u64, output_bytes: u64| {
            let event = ActionEvent::new("find-code", ".").with_metrics(OperationMetrics::new(
                Duration::from_millis(duration_ms),
                output_bytes,
                Some(evidence),
            ));
            format_metrics_line(&event).unwrap()
        };
        let saving = line_for(
            WorkflowEvidence {
                distinct_files: 1,
                native_commands: 1,
                ..Default::default()
            },
            100,
            0,
        );
        assert!(
            saving.contains("  ├─ ⏱ 95% faster, 100.0ms against ~2000ms estimated"),
            "{saving}"
        );
        assert!(
            saving.contains(
                "  ├─ § estimated LLM context saved: ~1280 tok (100%) against ~1280 tok estimated"
            ),
            "{saving}"
        );
        let overhead = line_for(
            WorkflowEvidence {
                native_commands: 2,
                ..Default::default()
            },
            5000,
            0,
        );
        assert!(
            overhead.contains("  ├─ ⏱ +3000.0ms overhead, 5000.0ms against ~2000ms estimated"),
            "{overhead}"
        );
        let zero = line_for(
            WorkflowEvidence {
                native_commands: 2,
                ..Default::default()
            },
            2000,
            2048,
        );
        assert!(
            zero.contains("  ├─ ⏱ 2000.0ms against ~2000ms estimated · no estimated time saving"),
            "{zero}"
        );
        assert!(
            zero.contains(
                "  ├─ § no estimated context saving (rendered output meets or exceeds the ~512 tok baseline)"
            ),
            "{zero}"
        );
    }

    #[test]
    fn comparison_gap_round_trips_and_absent_field_stays_unrecorded() {
        let metrics = OperationMetrics::new(Duration::ZERO, 0, None)
            .with_comparison_gap(ComparisonGap::OperationFailed);
        let value = serde_json::to_value(&metrics).unwrap();
        assert_eq!(value["comparison_gap"], "operation_failed");
        let decoded: OperationMetrics = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.comparison_gap, Some(ComparisonGap::OperationFailed));
        let absent: OperationMetrics = serde_json::from_str(
            r#"{"duration_us":1,"output_bytes":0,"reporting_bytes":0,"estimator_version":"workflow-v1","native_workflow_bytes":null,"evidence":null}"#,
        )
        .unwrap();
        assert_eq!(absent.comparison_gap, None);
    }

    /// Only a baseline made of the file bytes alone is a measurement: an
    /// assumed kilobyte per command or relationship would turn the "full
    /// read" figure back into the policy estimate it replaces.
    #[test]
    fn a_measured_read_is_the_known_bytes_only_when_nothing_is_assumed_on_top() {
        let read = |native_commands, relationships, known_file_bytes| WorkflowEvidence {
            distinct_files: 1,
            native_commands,
            relationships,
            known_file_bytes,
            partial: false,
        };
        assert_eq!(read(0, 0, Some(41_462)).measured_file_read(), Some(41_462));
        assert_eq!(read(1, 0, Some(41_462)).measured_file_read(), None);
        assert_eq!(read(0, 1, Some(41_462)).measured_file_read(), None);
        assert_eq!(read(0, 0, None).measured_file_read(), None);
    }

    /// The row a first-time user reads under `pixel list-signatures`: the two
    /// counts the bench script publishes, floored the same way, then the
    /// saving. psf/requests `models.py` (41 462 bytes, a 2 567-byte answer)
    /// is the case the website quotes.
    #[test]
    fn a_measured_read_states_both_counts_floored_like_the_bench() {
        assert_eq!(
            measured_read_section(41_462, 2_567),
            "full read 10365 tok, pixel answer 641 tok (-94%)"
        );
        // Floored on both sides: 7 and 5 bytes are one token each, no saving.
        assert_eq!(
            measured_read_section(7, 5),
            "full read 1 tok, pixel answer 1 tok (no saving)"
        );
        assert_eq!(
            measured_read_section(400, 800),
            "full read 100 tok, pixel answer 200 tok (no saving)"
        );
        assert_eq!(
            measured_read_section(400, 300),
            "full read 100 tok, pixel answer 75 tok (-25%)"
        );
        assert_eq!(
            measured_read_section(0, 0),
            "full read 0 tok, pixel answer 0 tok (no saving)"
        );
    }

    /// `pixel audit` prints these two numbers per file and must agree with
    /// the row above to the token: a tie is no saving, never `-0%`.
    #[test]
    fn read_tokens_and_saved_percent_are_the_rows_parts() {
        assert_eq!(read_tokens(41_462), 10_365);
        assert_eq!(read_tokens(3), 0);
        assert_eq!(saved_percent(10_365, 641), Some(94));
        assert_eq!(saved_percent(100, 75), Some(25));
        assert_eq!(saved_percent(100, 100), None);
        assert_eq!(saved_percent(100, 200), None);
        assert_eq!(saved_percent(0, 0), None);
    }

    /// The live line of a measured read carries both counts from the stdout
    /// answer, never the estimate that also counts diagnostics: a stale-install
    /// note on stderr must not move the "pixel answer" figure.
    #[test]
    fn the_live_line_of_a_measured_read_compares_the_file_with_the_stdout_answer() {
        let evidence = WorkflowEvidence {
            distinct_files: 1,
            known_file_bytes: Some(41_462),
            ..Default::default()
        };
        let mut metrics =
            OperationMetrics::new(Duration::from_millis(167), 2_727, Some(evidence.clone()));
        metrics.answer_bytes = Some(2_567);
        let event = ActionEvent::new("list-signatures", "models.py").with_metrics(metrics);
        let line = format_metrics_line(&event).unwrap();
        assert!(
            line.contains("  ├─ § full read 10365 tok, pixel answer 641 tok (-94%)\n"),
            "{line}"
        );
        assert!(!line.contains("estimated LLM context saved"), "{line}");

        let mut partial = evidence.clone();
        partial.partial = true;
        let mut metrics = OperationMetrics::new(Duration::from_millis(1), 2_727, Some(partial));
        metrics.answer_bytes = Some(2_567);
        let event = ActionEvent::new("list-signatures", "models.py").with_metrics(metrics);
        assert!(
            format_metrics_line(&event)
                .unwrap()
                .contains("pixel answer 641 tok (-94%), partial\n")
        );

        // A record without the answer size (written before the field) keeps
        // the estimate, which now names its base.
        let old = ActionEvent::new("list-signatures", "models.py").with_metrics(
            OperationMetrics::new(Duration::from_millis(1), 2_727, Some(evidence)),
        );
        let line = format_metrics_line(&old).unwrap();
        assert!(
            line.contains(
                "  ├─ § estimated LLM context saved: ~9684 tok (93%) against ~10366 tok estimated\n"
            ),
            "{line}"
        );
    }

    /// An empty stdout has no answer to have saved context on. The baseline of
    /// a search that matched nothing is still a few kilobytes, so the estimate
    /// used to claim "~225 tok (88%)" for a command that printed nothing.
    #[test]
    fn an_empty_answer_claims_no_context_saving() {
        let evidence = WorkflowEvidence {
            distinct_files: 1,
            native_commands: 1,
            ..Default::default()
        };
        let build = |answer: Option<u64>, partial: bool| {
            let mut evidence = evidence.clone();
            evidence.partial = partial;
            let mut metrics = OperationMetrics::new(Duration::from_millis(3), 40, Some(evidence));
            metrics.answer_bytes = answer;
            format_metrics_line(&ActionEvent::new("search-content", "x").with_metrics(metrics))
                .unwrap()
        };
        let empty = build(Some(0), false);
        assert!(
            empty.contains("  ├─ § no estimated context saving (empty output)\n"),
            "{empty}"
        );
        assert!(!empty.contains("estimated LLM context saved"), "{empty}");
        assert!(build(Some(0), true).contains("(empty output), partial\n"));
        // One byte of answer, or an unrecorded answer size, keeps the estimate.
        for answer in [Some(1), None] {
            let line = build(answer, false);
            assert!(line.contains("estimated LLM context saved"), "{line}");
        }
    }
}
