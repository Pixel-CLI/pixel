// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Human-readable progress shared by daemon-routed and standalone index builds.

use pixel_facts::ingest::{IngestOptions, ingest_until_fresh_with_progress};
use std::path::Path;
use std::time::{Duration, Instant};

/// Completed ingestions get their final report; only unfinished work emits progress.
fn progress_due(fresh: bool, elapsed: Duration) -> bool {
    !fresh && elapsed >= Duration::from_secs(1)
}

pub(crate) fn build(root: &Path) -> Result<(), String> {
    let started = Instant::now();
    let mut store = pixel_facts::FactsStore::open(root).map_err(|e| e.to_string())?;
    let mut last = Instant::now();
    eprintln!("facts: starting history ingestion");
    let report =
        ingest_until_fresh_with_progress(&mut store, &IngestOptions::default(), |report| {
            if progress_due(report.fresh, last.elapsed()) {
                eprintln!(
                    "facts: phase={} metadata_commits={} blobs={}/{} diffs={}/{} elapsed_s={:.1}",
                    report.phase,
                    report.commits_indexed,
                    report.blob_commits_done,
                    report.blob_commits_total,
                    report.diff_commits_done,
                    report.diff_commits_total,
                    started.elapsed().as_secs_f64()
                );
                last = Instant::now();
            }
        })
        .map_err(|e| e.to_string())?;
    let times = &report.timings;
    eprintln!(
        "facts: phase={} commits={} diff_coverage={:.0}% fresh={} metadata_ms={:.1} blobs_ms={:.1} diffs_ms={:.1} retention_ms={:.1} total_ms={:.1}",
        report.phase,
        report.commits_indexed,
        report.diff_indexed_pct * 100.0,
        report.fresh,
        times.metadata.as_secs_f64() * 1000.0,
        times.blobs.as_secs_f64() * 1000.0,
        times.diffs.as_secs_f64() * 1000.0,
        times.retention.as_secs_f64() * 1000.0,
        started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

#[cfg(test)]
mod audited_contract_tests {
    use super::*;

    #[test]
    fn history_progress_should_start_at_one_second_and_stop_when_fresh() {
        for (fresh, millis, expected) in [
            (false, 0, false),
            (false, 999, false),
            (false, 1000, true),
            (false, 1001, true),
            (true, 1001, false),
        ] {
            assert_eq!(progress_due(fresh, Duration::from_millis(millis)), expected);
        }
    }
}
