// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Query-layer behavior: what `pixel list-errors` reads back from the store.

use std::fs;
use std::path::{Path, PathBuf};

use pixel_session::query;
use pixel_session::store::{Store, now_ms};
use pixel_session::types::{ErrorInput, EventInput, EventKind, RunInput, Surface};
use serde_json::json;

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> TempRoot {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pixel-session-qtest-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        TempRoot(dir)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn error(surface: Surface, message: &str, ts: Option<i64>) -> ErrorInput {
    ErrorInput {
        surface,
        message: message.into(),
        kind: None,
        stack_raw: None,
        frames: None,
        values: None,
        http: None,
        extra: None,
        run_id: None,
        ts,
    }
}

/// Two runs (lockfile+deps changed between them), one browser error tied to
/// run B, dep-optimize + hmr events around it, and a vitest failure.
fn seeded_store(state: &TempRoot) -> (Store, i64) {
    let store = Store::open_at(Path::new("/tmp/query-fixture"), &state.0).unwrap();
    let now = now_ms();
    store
        .record_run(&RunInput {
            run_id: "01A".into(),
            pid: Some(100),
            port: Some(3000),
            git_head: Some("abc123".into()),
            lockfile_hash: Some("lockaaa".into()),
            vite_dep_hash: Some("depaaa".into()),
            fingerprint: None,
            changed_since_last_run: None,
            ts: Some(now - 60_000),
        })
        .unwrap();
    store
        .record_run(&RunInput {
            run_id: "01B".into(),
            pid: Some(200),
            port: Some(3000),
            git_head: Some("abc123".into()),
            lockfile_hash: Some("lockbbb".into()),
            vite_dep_hash: Some("depbbb".into()),
            fingerprint: None,
            changed_since_last_run: Some(vec!["vite-deps".into()]),
            ts: Some(now - 10_000),
        })
        .unwrap();
    let mut router = error(
        Surface::BrowserRejection,
        "undefined is not an object (evaluating 'api.sessions.x')",
        Some(now - 5_000),
    );
    router.kind = Some("TypeError".into());
    router.run_id = Some("01B".into());
    store.record_error(&router).unwrap();
    store
        .record_event(&EventInput {
            kind: EventKind::DepOptimized,
            data: Some(json!({"trigger": "lockfile change"})),
            run_id: Some("01B".into()),
            ts: Some(now - 8_000),
        })
        .unwrap();
    store
        .record_event(&EventInput {
            kind: EventKind::HmrUpdate,
            data: Some(json!({"files": ["src/routes/chat.tsx"], "rev": 7})),
            run_id: Some("01B".into()),
            ts: Some(now - 2_000),
        })
        .unwrap();
    store
        .record_event(&EventInput {
            kind: EventKind::HmrUpdate,
            data: Some(json!({"files": ["src/other.tsx"], "rev": 8})),
            run_id: Some("01B".into()),
            ts: Some(now - 1_000),
        })
        .unwrap();
    store
        .record_error(&error(
            Surface::Vitest,
            "2 failed | 108 passed (312s)",
            Some(now - 500),
        ))
        .unwrap();
    (store, now)
}

#[test]
fn last_and_since_share_cursor_semantics() {
    let state = TempRoot::new();
    let (store, _) = seeded_store(&state);
    let last = query::last(&store, 10, None).unwrap();
    assert_eq!(last.errors.len(), 2);
    assert_eq!(last.cursor, 2);
    // Newest first in `last`.
    assert_eq!(last.errors[0].surface, Surface::Vitest);
    // `since` past cursor 1 returns only the vitest record, ascending.
    let since = query::since(&store, 1).unwrap();
    assert_eq!(since.errors.len(), 1);
    assert_eq!(since.errors[0].id, 2);
    // Fully caught up → empty, cursor unchanged.
    let caught_up = query::since(&store, 2).unwrap();
    assert!(caught_up.errors.is_empty());
    assert_eq!(caught_up.cursor, 2);
}

#[test]
fn show_correlates_events_and_run() {
    let state = TempRoot::new();
    let (store, _) = seeded_store(&state);
    let shown = query::show(&store, 1).unwrap().unwrap();
    assert_eq!(shown.error.id, 1);
    // dep-optimized (−8s) and hmr (−2s, −1s) all fall in ±30s of the error.
    assert_eq!(shown.correlated_events.len(), 3);
    let run = shown.run.unwrap();
    assert_eq!(run.run_id, "01B");
    assert_eq!(
        run.changed_since_last_run.as_deref(),
        Some(&["vite-deps".to_owned()][..])
    );
    assert!(query::show(&store, 999).unwrap().is_none());
}

#[test]
fn hmr_filters_updates_by_file() {
    let state = TempRoot::new();
    let (store, _) = seeded_store(&state);
    let all = query::hmr(&store, None).unwrap();
    assert_eq!(
        all.last_update.as_ref().unwrap().data.as_ref().unwrap()["rev"],
        8
    );
    let filtered = query::hmr(&store, Some("src/routes/chat.tssx")).unwrap();
    assert!(filtered.last_update.is_none());
    let filtered = query::hmr(&store, Some("src/routes/chat.tsx")).unwrap();
    assert_eq!(
        filtered
            .last_update
            .as_ref()
            .unwrap()
            .data
            .as_ref()
            .unwrap()["rev"],
        7
    );
}

#[test]
fn env_diff_names_changed_fields() {
    let state = TempRoot::new();
    let (store, _) = seeded_store(&state);
    let plain = query::env(&store, false).unwrap();
    assert_eq!(plain.run.as_ref().unwrap().run_id, "01B");
    assert!(plain.previous.is_none());
    assert!(plain.changed.is_none());
    let diffed = query::env(&store, true).unwrap();
    assert_eq!(diffed.previous.as_ref().unwrap().run_id, "01A");
    let changed = diffed.changed.unwrap();
    assert!(changed.iter().any(|c| c.starts_with("lockfile_hash:")));
    assert!(changed.iter().any(|c| c.starts_with("vite_dep_hash:")));
    assert!(!changed.iter().any(|c| c.starts_with("git_head:")));
}

#[test]
fn test_status_tracks_latest_signal() {
    let state = TempRoot::new();
    let (store, now) = seeded_store(&state);
    let failing = query::test_status(&store).unwrap();
    assert_eq!(failing.passing, Some(false));
    assert_eq!(
        failing.latest_failure.as_ref().unwrap().message,
        "2 failed | 108 passed (312s)"
    );
    store
        .record_event(&EventInput {
            kind: EventKind::TestPass,
            data: Some(json!({"passed": 110})),
            run_id: None,
            ts: Some(now + 1_000),
        })
        .unwrap();
    assert_eq!(query::test_status(&store).unwrap().passing, Some(true));
}

#[test]
fn test_status_picks_the_newest_failure_across_test_surfaces() {
    for (first, second) in [
        (Surface::Vitest, Surface::Minitest),
        (Surface::Minitest, Surface::Vitest),
        (Surface::Rspec, Surface::Minitest),
    ] {
        let state = TempRoot::new();
        let store = Store::open_at(Path::new("/tmp/query-fixture"), &state.0).unwrap();
        let now = now_ms();
        store
            .record_error(&error(first, "older run", Some(now - 2_000)))
            .unwrap();
        let newest = store
            .record_error(&error(second, "newer run", Some(now - 1_000)))
            .unwrap();
        // A lint record is never a test signal, however new.
        store
            .record_error(&error(Surface::Rubocop, "Style/X: lint", Some(now)))
            .unwrap();
        let status = query::test_status(&store).unwrap();
        assert_eq!(status.passing, Some(false));
        let failure = status.latest_failure.unwrap();
        assert_eq!(failure.id, newest.id, "{first:?} then {second:?}");
        assert_eq!(failure.surface, second);
    }
}
