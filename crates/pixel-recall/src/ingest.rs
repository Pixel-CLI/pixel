// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Ingest orchestration: discover → classify → parse → store, one
//! transaction per session, resumable by construction.

use crate::sources::{Change, IngestError, SourceAdapter, SourceUnit};
use crate::store::{IngestState, RecallStore};

#[derive(Debug, Default, Clone)]
pub struct IngestReport {
    pub agent: String,
    pub units_seen: usize,
    pub units_new: usize,
    pub units_appended: usize,
    pub units_rewritten: usize,
    pub units_unchanged: usize,
    pub sessions_written: usize,
    pub turns_written: usize,
    /// Complete records the adapters could not parse (a truncated write).
    /// Counted separately from `parse_errors`, which counts whole units.
    pub skipped_records: usize,
    pub parse_errors: usize,
    pub elapsed_ms: u128,
}

/// Discover every unit of `adapter` and ingest the ones whose size or
/// mtime moved since the recorded state.
pub fn ingest_source(
    store: &mut RecallStore,
    adapter: &dyn SourceAdapter,
) -> Result<IngestReport, IngestError> {
    let units = adapter.discover()?;
    ingest_units(store, adapter, &units)
}

/// Discover every unit of `adapter` but ingest only those modified within
/// `window_ms` of `now_ms`: the daemon's periodic sweep. It does not rely
/// on the file watcher: on macOS, FSEvents holds back modify events while
/// a writer keeps the file open, which is exactly how an agent streams its
/// transcript, so the session being written is the one the watcher misses.
/// Every unit is still stat'ed by `discover`, so the sweep costs one
/// directory walk and no reparse for unchanged files.
pub fn ingest_recent(
    store: &mut RecallStore,
    adapter: &dyn SourceAdapter,
    now_ms: i64,
    window_ms: i64,
) -> Result<IngestReport, IngestError> {
    let units = recent_units(adapter.discover()?, now_ms, window_ms);
    ingest_units(store, adapter, &units)
}

/// Keep the units modified at or after `now_ms - window_ms`. A unit exactly
/// at the boundary is recent; a negative window keeps nothing older than
/// the future.
pub fn recent_units(units: Vec<SourceUnit>, now_ms: i64, window_ms: i64) -> Vec<SourceUnit> {
    let cutoff = now_ms.saturating_sub(window_ms);
    units.into_iter().filter(|u| u.mtime_ms >= cutoff).collect()
}

/// Ingest the given units of `adapter`, one transaction per session,
/// skipping the ones whose recorded state matches their size and mtime.
pub fn ingest_units(
    store: &mut RecallStore,
    adapter: &dyn SourceAdapter,
    units: &[SourceUnit],
) -> Result<IngestReport, IngestError> {
    let started = std::time::Instant::now();
    let agent = adapter.agent();
    let mut report = IngestReport {
        agent: agent.to_string(),
        units_seen: units.len(),
        ..Default::default()
    };

    for unit in units {
        let state = store.ingest_state(agent, &unit.unit_key)?;
        let mut change = adapter.classify(unit, state.as_ref());
        if let (Change::Appended { .. }, Some(st)) = (&change, state.as_ref())
            && !adapter.append_valid(unit, st)
        {
            // The file grew but its prefix changed — rewritten in place.
            change = Change::Rewritten;
        }
        match change {
            Change::Unchanged => {
                report.units_unchanged += 1;
                continue;
            }
            Change::New => report.units_new += 1,
            Change::Appended { .. } => report.units_appended += 1,
            Change::Rewritten => report.units_rewritten += 1,
        }

        match adapter.parse(unit, change, state.as_ref()) {
            Ok(parsed) => {
                let st = IngestState {
                    file_size: unit.size as i64,
                    mtime_ms: unit.mtime_ms,
                    bytes_ingested: parsed.consumed_bytes as i64,
                    cursor: parsed
                        .cursor
                        .or_else(|| adapter.make_cursor(unit, parsed.consumed_bytes)),
                };
                // The unit's resume state is committed only WITH THE LAST
                // session (or a final touch): an interrupted multi-session
                // unit must re-parse from the old cursor, never record
                // completion it didn't reach. Session writes are
                // idempotent (replace/append by source id), so re-parsing
                // is safe; skipping is not.
                let stale_st = state.clone().unwrap_or(IngestState {
                    file_size: -1,
                    mtime_ms: -1,
                    bytes_ingested: match &change {
                        Change::Appended { from } => *from as i64,
                        _ => 0,
                    },
                    cursor: None,
                });
                let last = parsed.sessions.len().saturating_sub(1);
                for (i, ps) in parsed.sessions.iter().enumerate() {
                    let st_for_this = if i == last { &st } else { &stale_st };
                    match ps.op {
                        crate::sources::SessionOp::Append => {
                            store.append_session(
                                &ps.session,
                                &ps.turns,
                                &unit.unit_key,
                                st_for_this,
                            )?;
                        }
                        crate::sources::SessionOp::Replace => {
                            store.replace_session(
                                &ps.session,
                                &ps.turns,
                                &unit.unit_key,
                                st_for_this,
                            )?;
                        }
                    }
                    report.sessions_written += 1;
                    report.turns_written += ps.turns.len();
                }
                report.skipped_records += parsed.skipped_records;
                if parsed.sessions.is_empty() {
                    store.touch_state(agent, &unit.unit_key, &st)?;
                }
            }
            Err(_) => {
                report.parse_errors += 1;
            }
        }
    }

    store.link_subagents(agent)?;
    report.elapsed_ms = started.elapsed().as_millis();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::model::{IntentSource, Role, TsSource, UnifiedSession, UnifiedTurn};
    use crate::sources::{ParseOutput, ParsedSession, SessionOp, SourceUnit};

    const NOW: i64 = 1_760_000_000_000;

    fn unit(key: &str, size: u64, mtime_ms: i64) -> SourceUnit {
        SourceUnit {
            unit_key: key.to_string(),
            path: PathBuf::from(key),
            size,
            mtime_ms,
        }
    }

    /// An adapter over in-memory units: one session per unit, one turn per
    /// parse, and a log of the `Change` each parse received.
    struct Fake {
        units: Vec<SourceUnit>,
        /// Units whose parse fails (a corrupt file).
        broken: Vec<String>,
        /// Units whose parse reports an unreadable record (a truncated line).
        skipped: Vec<String>,
        /// Units whose recorded prefix no longer matches (rewritten in place).
        prefix_changed: Vec<String>,
        parsed: RefCell<BTreeMap<String, Vec<Change>>>,
    }

    impl Fake {
        fn new(units: Vec<SourceUnit>) -> Self {
            Self {
                units,
                broken: Vec::new(),
                skipped: Vec::new(),
                prefix_changed: Vec::new(),
                parsed: RefCell::new(BTreeMap::new()),
            }
        }

        fn changes(&self, key: &str) -> Vec<Change> {
            self.parsed.borrow().get(key).cloned().unwrap_or_default()
        }
    }

    impl SourceAdapter for Fake {
        fn agent(&self) -> &'static str {
            "fake"
        }

        fn discover(&self) -> Result<Vec<SourceUnit>, IngestError> {
            Ok(self.units.clone())
        }

        fn parse(
            &self,
            unit: &SourceUnit,
            change: Change,
            _state: Option<&crate::store::IngestState>,
        ) -> Result<ParseOutput, IngestError> {
            self.parsed
                .borrow_mut()
                .entry(unit.unit_key.clone())
                .or_default()
                .push(change);
            if self.broken.contains(&unit.unit_key) {
                return Err(IngestError::Other("corrupt".into()));
            }
            let op = match change {
                Change::Appended { .. } => SessionOp::Append,
                _ => SessionOp::Replace,
            };
            let session = UnifiedSession {
                agent: "fake",
                source_session_id: format!("s-{}", unit.unit_key),
                source_path: unit.unit_key.clone(),
                cwd: None,
                git_branch: None,
                title: None,
                ts_source: TsSource::Iso,
                is_subagent: false,
                parent_source_session_id: None,
            };
            let turn = UnifiedTurn {
                role: Role::User,
                intent_source: Some(IntentSource::Human),
                ts: Some(NOW),
                text: format!("turn at {}", unit.size),
                truncated: false,
                source_byte_start: None,
                source_byte_len: None,
            };
            Ok(ParseOutput {
                sessions: vec![ParsedSession {
                    op,
                    session,
                    turns: vec![turn],
                }],
                skipped_records: usize::from(self.skipped.contains(&unit.unit_key)),
                consumed_bytes: unit.size,
                cursor: None,
            })
        }

        fn append_valid(&self, unit: &SourceUnit, _state: &crate::store::IngestState) -> bool {
            !self.prefix_changed.contains(&unit.unit_key)
        }
    }

    fn store() -> (tempfile::TempDir, RecallStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = RecallStore::open(&dir.path().join("recall.db")).unwrap();
        (dir, store)
    }

    fn turn_texts(store: &RecallStore, source_session_id: &str) -> Vec<String> {
        let rows = store
            .sessions_by_prefix(Some("fake"), source_session_id)
            .unwrap();
        assert_eq!(rows.len(), 1, "one session for {source_session_id}");
        store
            .turns_for_session(rows[0].id, None)
            .unwrap()
            .into_iter()
            .map(|t| t.text)
            .collect()
    }

    #[test]
    fn ingest_source_writes_every_unit_once_and_records_its_state() {
        let (_dir, mut store) = store();
        let fake = Fake::new(vec![unit("/a", 10, NOW), unit("/b", 20, NOW)]);

        let first = ingest_source(&mut store, &fake).unwrap();
        assert_eq!(first.agent, "fake");
        assert_eq!(
            (first.units_seen, first.units_new, first.units_unchanged),
            (2, 2, 0)
        );
        assert_eq!((first.sessions_written, first.turns_written), (2, 2));
        assert_eq!(turn_texts(&store, "s-/a"), vec!["turn at 10"]);
        let st = store.ingest_state("fake", "/b").unwrap().unwrap();
        assert_eq!(
            (st.file_size, st.mtime_ms, st.bytes_ingested),
            (20, NOW, 20)
        );

        // Nothing moved: the second pass parses nothing and writes nothing.
        let second = ingest_source(&mut store, &fake).unwrap();
        assert_eq!((second.units_seen, second.units_unchanged), (2, 2));
        assert_eq!((second.sessions_written, second.turns_written), (0, 0));
        assert_eq!(fake.changes("/a"), vec![Change::New]);
    }

    #[test]
    fn a_grown_unit_is_appended_from_the_recorded_offset() {
        let (_dir, mut store) = store();
        let mut fake = Fake::new(vec![unit("/a", 10, NOW)]);
        ingest_source(&mut store, &fake).unwrap();

        fake.units = vec![unit("/a", 25, NOW + 1)];
        let report = ingest_source(&mut store, &fake).unwrap();
        assert_eq!((report.units_appended, report.sessions_written), (1, 1));
        assert_eq!(
            fake.changes("/a"),
            vec![Change::New, Change::Appended { from: 10 }]
        );
        // Append keeps the earlier turn and adds the new one.
        assert_eq!(turn_texts(&store, "s-/a"), vec!["turn at 10", "turn at 25"]);
        let st = store.ingest_state("fake", "/a").unwrap().unwrap();
        assert_eq!((st.file_size, st.bytes_ingested), (25, 25));
    }

    #[test]
    fn a_grown_unit_whose_prefix_changed_is_reparsed_from_scratch() {
        let (_dir, mut store) = store();
        let mut fake = Fake::new(vec![unit("/a", 10, NOW)]);
        ingest_source(&mut store, &fake).unwrap();

        fake.units = vec![unit("/a", 25, NOW + 1)];
        fake.prefix_changed = vec!["/a".to_string()];
        let report = ingest_source(&mut store, &fake).unwrap();
        assert_eq!((report.units_rewritten, report.units_appended), (1, 0));
        assert_eq!(fake.changes("/a"), vec![Change::New, Change::Rewritten]);
        // Replace drops the stale turn instead of stacking on it.
        assert_eq!(turn_texts(&store, "s-/a"), vec!["turn at 25"]);
    }

    #[test]
    fn a_shrunk_unit_is_rewritten() {
        let (_dir, mut store) = store();
        let mut fake = Fake::new(vec![unit("/a", 10, NOW)]);
        ingest_source(&mut store, &fake).unwrap();

        fake.units = vec![unit("/a", 4, NOW + 1)];
        let report = ingest_source(&mut store, &fake).unwrap();
        assert_eq!(report.units_rewritten, 1);
        assert_eq!(turn_texts(&store, "s-/a"), vec!["turn at 4"]);
    }

    /// A truncated record is counted, never swallowed: the unit still writes
    /// its readable session, and the count reaches the report.
    #[test]
    fn a_skipped_record_is_counted_and_the_session_still_lands() {
        let (_dir, mut store) = store();
        let mut fake = Fake::new(vec![unit("/half", 10, NOW)]);
        fake.skipped = vec!["/half".to_string()];
        let report = ingest_source(&mut store, &fake).unwrap();
        assert_eq!(report.skipped_records, 1);
        assert_eq!((report.sessions_written, report.turns_written), (1, 1));
        assert_eq!(turn_texts(&store, "s-/half"), vec!["turn at 10"]);
    }

    #[test]
    fn a_parse_error_is_counted_and_leaves_no_state_behind() {
        let (_dir, mut store) = store();
        let mut fake = Fake::new(vec![unit("/bad", 10, NOW), unit("/ok", 10, NOW)]);
        fake.broken = vec!["/bad".to_string()];
        let report = ingest_source(&mut store, &fake).unwrap();
        assert_eq!((report.parse_errors, report.sessions_written), (1, 1));
        assert!(
            store.ingest_state("fake", "/bad").unwrap().is_none(),
            "a failed parse must be retried on the next pass"
        );
        assert!(store.ingest_state("fake", "/ok").unwrap().is_some());
    }

    #[test]
    fn recent_units_keeps_the_boundary_and_drops_older() {
        let window = 6 * 60 * 60 * 1000;
        let cutoff = NOW - window;
        let kept = recent_units(
            vec![
                unit("/older", 1, cutoff - 1),
                unit("/at", 1, cutoff),
                unit("/newer", 1, cutoff + 1),
                unit("/future", 1, NOW + 1),
            ],
            NOW,
            window,
        );
        let keys: Vec<&str> = kept.iter().map(|u| u.unit_key.as_str()).collect();
        assert_eq!(keys, vec!["/at", "/newer", "/future"]);
        // A cutoff that would underflow saturates instead of panicking.
        assert_eq!(
            recent_units(vec![unit("/x", 1, i64::MIN)], i64::MIN, 10).len(),
            1
        );
    }

    #[test]
    fn ingest_recent_stats_everything_but_parses_only_the_window() {
        let (_dir, mut store) = store();
        let window = 6 * 60 * 60 * 1000;
        let fake = Fake::new(vec![
            unit("/old", 10, NOW - window - 1),
            unit("/fresh", 10, NOW),
        ]);
        let report = ingest_recent(&mut store, &fake, NOW, window).unwrap();
        assert_eq!((report.units_seen, report.units_new), (1, 1));
        assert_eq!(fake.changes("/old"), Vec::<Change>::new());
        assert_eq!(fake.changes("/fresh"), vec![Change::New]);
        assert!(store.ingest_state("fake", "/old").unwrap().is_none());
        assert_eq!(turn_texts(&store, "s-/fresh"), vec!["turn at 10"]);
    }
}
