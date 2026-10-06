// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Store integration tests: paths, permissions, dedup upsert, retention,
//! self-heal, WAL concurrency.

use std::fs;
use std::path::{Path, PathBuf};

use pixel_session::store::{
    Store, now_ms, project_key, resolve_project_root, store_directory, store_path,
};
use pixel_session::types::{
    ErrorInput, EventInput, EventKind, Frame, HttpContext, RunInput, Surface,
};

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> TempRoot {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pixel-session-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        TempRoot(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn error(surface: Surface, message: &str) -> ErrorInput {
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
        ts: None,
    }
}

#[test]
fn project_key_is_basename_plus_12_hex() {
    let key = project_key(Path::new("/Users/x/Documents/ship-fast"));
    assert!(key.starts_with("ship-fast-"));
    let hex = &key["ship-fast-".len()..];
    assert_eq!(hex.len(), 12);
    assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    // Same basename, different root → different key.
    assert_ne!(key, project_key(Path::new("/elsewhere/ship-fast")));
}

#[test]
fn store_path_is_versioned_and_namespaced() {
    let path = store_path(Path::new("/a/app"), Path::new("/state"));
    let expected: PathBuf = ["/state", "pixel", "sniper"]
        .iter()
        .collect::<PathBuf>()
        .join(project_key(Path::new("/a/app")))
        .join("errors-v1.sqlite");
    assert_eq!(path, expected);
}

#[cfg(unix)]
#[test]
fn open_sets_permissions_and_writes_project_json() {
    use std::os::unix::fs::PermissionsExt;
    let state = TempRoot::new();
    let project = Path::new("/tmp/fixture-project");
    let store = Store::open_at(project, state.path()).unwrap();
    let dir = store_directory(project, state.path());
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(store.path()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let project_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("project.json")).unwrap()).unwrap();
    assert_eq!(project_json["root"], "/tmp/fixture-project");
    assert_eq!(project_json["key"], project_key(project).as_str());
}

#[test]
fn dedup_upsert_increments_count_and_keeps_one_row() {
    let state = TempRoot::new();
    let store = Store::open_at(Path::new("/tmp/p"), state.path()).unwrap();
    let mut input = error(
        Surface::BrowserRejection,
        "undefined is not an object (evaluating 'api.sessions.x')",
    );
    input.kind = Some("TypeError".into());
    let first = store.record_error(&input).unwrap();
    assert!(!first.deduped);
    assert_eq!(first.count, 1);
    let second = store.record_error(&input).unwrap();
    assert_eq!(second.id, first.id);
    assert!(second.deduped);
    assert_eq!(second.count, 2);
    let fetched = store.get_error(first.id).unwrap().unwrap();
    assert_eq!(fetched.count, 2);
    assert!(fetched.last_ts >= fetched.first_ts);
    assert_eq!(store.last_errors(10, None).unwrap().len(), 1);
}

#[test]
fn distinct_errors_get_monotonic_ids_and_cursor() {
    let state = TempRoot::new();
    let store = Store::open_at(Path::new("/tmp/p"), state.path()).unwrap();
    let a = store.record_error(&error(Surface::Tsc, "a")).unwrap();
    let b = store.record_error(&error(Surface::Tsc, "b")).unwrap();
    assert!(b.id > a.id);
    assert_eq!(store.max_cursor().unwrap(), b.id);
    let since = store.errors_since(a.id, 100).unwrap();
    assert_eq!(since.len(), 1);
    assert_eq!(since[0].id, b.id);
}

#[test]
fn json_columns_round_trip() {
    let state = TempRoot::new();
    let store = Store::open_at(Path::new("/tmp/p"), state.path()).unwrap();
    let mut input = error(Surface::Http5xx, "internal error");
    input.frames = Some(vec![Frame {
        raw: "at handler".into(),
        file: Some("src/api.ts".into()),
        line: Some(10),
        ..Frame::default()
    }]);
    input.values = Some(serde_json::json!({"evaluatingChain": "a.b.c"}));
    input.http = Some(HttpContext {
        method: Some("POST".into()),
        url: Some("/api/x".into()),
        status: Some(500),
        body_excerpt: Some("{}".into()),
    });
    input.extra = Some(serde_json::json!({"alsoSeenAs": ["browser-console"]}));
    let recorded = store.record_error(&input).unwrap();
    let fetched = store.get_error(recorded.id).unwrap().unwrap();
    assert_eq!(
        fetched.frames.as_ref().unwrap()[0].file.as_deref(),
        Some("src/api.ts")
    );
    assert_eq!(fetched.values.unwrap()["evaluatingChain"], "a.b.c");
    assert_eq!(fetched.http.unwrap()["status"], 500);
    assert_eq!(fetched.extra.unwrap()["alsoSeenAs"][0], "browser-console");
}

#[test]
fn surface_filter_and_search() {
    let state = TempRoot::new();
    let store = Store::open_at(Path::new("/tmp/p"), state.path()).unwrap();
    store
        .record_error(&error(Surface::Vitest, "2 failed | 10 passed"))
        .unwrap();
    store.record_error(&error(Surface::Tsc, "TS boom")).unwrap();
    store
        .record_error(&error(Surface::Vitest, "1 failed | 11 passed"))
        .unwrap();
    let vitest = store.last_errors(10, Some(Surface::Vitest)).unwrap();
    assert_eq!(vitest.len(), 2);
    assert_eq!(vitest[0].message, "1 failed | 11 passed");
    // LIKE wildcards are escaped.
    store
        .record_error(&error(Surface::Reported, "100% broken"))
        .unwrap();
    assert_eq!(store.search_errors("100%", 10).unwrap().len(), 1);
    assert_eq!(store.search_errors("nomatch", 10).unwrap().len(), 0);
}

#[test]
fn events_and_runs_round_trip() {
    let state = TempRoot::new();
    let store = Store::open_at(Path::new("/tmp/p"), state.path()).unwrap();
    let base = now_ms();
    store
        .record_event(&EventInput {
            kind: EventKind::ServerStart,
            data: None,
            run_id: Some("01A".into()),
            ts: Some(base - 100_000),
        })
        .unwrap();
    store
        .record_event(&EventInput {
            kind: EventKind::HmrUpdate,
            data: Some(serde_json::json!({"files": ["src/a.tsx"], "rev": 3})),
            run_id: Some("01A".into()),
            ts: Some(base),
        })
        .unwrap();
    let windowed = store.events_between(base - 30_000, base + 30_000).unwrap();
    assert_eq!(windowed.len(), 1);
    assert_eq!(windowed[0].kind, EventKind::HmrUpdate);
    assert_eq!(windowed[0].data.as_ref().unwrap()["rev"], 3);
    assert_eq!(
        store
            .latest_event_by_kind(EventKind::ServerStart)
            .unwrap()
            .unwrap()
            .ts,
        base - 100_000
    );

    store
        .record_run(&RunInput {
            run_id: "01A".into(),
            pid: Some(1),
            port: None,
            git_head: None,
            lockfile_hash: Some("aaa".into()),
            vite_dep_hash: None,
            fingerprint: None,
            changed_since_last_run: None,
            ts: Some(1000),
        })
        .unwrap();
    store
        .record_run(&RunInput {
            run_id: "01B".into(),
            pid: Some(2),
            port: Some(3000),
            git_head: None,
            lockfile_hash: Some("bbb".into()),
            vite_dep_hash: None,
            fingerprint: None,
            changed_since_last_run: Some(vec!["lockfile".into()]),
            ts: Some(2000),
        })
        .unwrap();
    let runs = store.latest_runs(5).unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].run_id, "01B");
    assert_eq!(runs[0].port, Some(3000));
    assert_eq!(
        runs[0].changed_since_last_run.as_deref(),
        Some(&["lockfile".to_owned()][..])
    );
}

#[test]
fn retention_trims_on_open() {
    let state = TempRoot::new();
    let project = Path::new("/tmp/retention");
    {
        let store = Store::open_at(project, state.path()).unwrap();
        let now = now_ms();
        let mut ancient = error(Surface::Reported, "ancient");
        ancient.ts = Some(now - 8 * 86_400_000);
        store.record_error(&ancient).unwrap();
        store
            .record_error(&error(Surface::Reported, "fresh"))
            .unwrap();
        store
            .record_event(&EventInput {
                kind: EventKind::HmrUpdate,
                data: None,
                run_id: None,
                ts: Some(now - 4 * 86_400_000),
            })
            .unwrap();
        store
            .record_event(&EventInput {
                kind: EventKind::HmrUpdate,
                data: None,
                run_id: None,
                ts: Some(now),
            })
            .unwrap();
    }
    let reopened = Store::open_at(project, state.path()).unwrap();
    let errors = reopened.last_errors(100, None).unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].message, "fresh");
    assert_eq!(
        reopened
            .latest_events(&[EventKind::HmrUpdate], 100)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn gc_reports_deletions_and_vacuums() {
    let state = TempRoot::new();
    let store = Store::open_at(Path::new("/tmp/p"), state.path()).unwrap();
    let mut old = error(Surface::Reported, "old");
    old.ts = Some(1000);
    store.record_error(&old).unwrap();
    let outcome = store.gc(true).unwrap();
    assert_eq!(outcome.errors_deleted, 1);
    assert!(outcome.vacuumed);
}

#[test]
fn self_heals_corrupt_database() {
    let state = TempRoot::new();
    let project = Path::new("/tmp/heal");
    let dir = store_directory(project, state.path());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("errors-v1.sqlite"), "this is not a sqlite file").unwrap();
    let store = Store::open_at(project, state.path()).unwrap();
    let recorded = store
        .record_error(&error(Surface::Reported, "after heal"))
        .unwrap();
    assert_eq!(
        store.get_error(recorded.id).unwrap().unwrap().message,
        "after heal"
    );
}

#[test]
fn two_handles_share_one_wal_database() {
    let state = TempRoot::new();
    let project = Path::new("/tmp/wal");
    let a = Store::open_at(project, state.path()).unwrap();
    let b = Store::open_at(project, state.path()).unwrap();
    let from_a = a
        .record_error(&error(Surface::Reported, "written by a"))
        .unwrap();
    let from_b = b
        .record_error(&error(Surface::Reported, "written by b"))
        .unwrap();
    assert_eq!(
        a.get_error(from_b.id).unwrap().unwrap().message,
        "written by b"
    );
    assert_eq!(
        b.get_error(from_a.id).unwrap().unwrap().message,
        "written by a"
    );
}

#[test]
fn resolve_project_root_climbs_to_the_git_toplevel_or_keeps_the_start() {
    let outside = TempRoot::new();
    let canonical = outside.path().canonicalize().unwrap();
    assert_eq!(
        resolve_project_root(outside.path()).unwrap(),
        canonical,
        "outside a repository the canonical start is the root"
    );
    assert!(resolve_project_root(&canonical.join("missing")).is_err());

    let repo = TempRoot::new();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(repo.path())
        .status()
        .unwrap();
    assert!(status.success());
    let nested = repo.path().join("src").join("deep");
    fs::create_dir_all(&nested).unwrap();
    assert_eq!(
        resolve_project_root(&nested).unwrap(),
        repo.path().canonicalize().unwrap(),
        "a subdirectory resolves to the repository root"
    );
}

#[test]
fn raw_outputs_follow_the_error_retention() {
    const SEVEN_DAYS_MS: i64 = 604_800_000;
    let state = TempRoot::new();
    let store = Store::open_at(Path::new("/tmp/raw-retention"), state.path()).unwrap();
    let now = now_ms();
    for (source, ts) in [
        ("run:old", now - 8 * 86_400_000),
        ("run:cutoff", now - SEVEN_DAYS_MS),
        ("run:before-cutoff", now - SEVEN_DAYS_MS - 1),
        ("run:new", now),
    ] {
        store
            .record_raw_fallback(source, &format!("{source} output"), Some(ts))
            .unwrap();
    }
    store.retain(now).unwrap();
    let latest = |source: &str| store.latest_raw_fallback(source).unwrap();
    assert_eq!(latest("run:old"), None);
    assert_eq!(latest("run:before-cutoff"), None, "one millisecond too old");
    assert_eq!(
        latest("run:cutoff").as_deref(),
        Some("run:cutoff output"),
        "exactly seven days old is kept"
    );
    assert_eq!(latest("run:new").as_deref(), Some("run:new output"));
    // Two rows survive; 198 more make exactly 200 and keep the oldest.
    for n in 0..198 {
        store
            .record_raw_fallback("run:many", &format!("output {n}"), Some(now))
            .unwrap();
    }
    store.retain(now).unwrap();
    assert_eq!(latest("run:many").as_deref(), Some("output 197"));
    assert_eq!(
        latest("run:cutoff").as_deref(),
        Some("run:cutoff output"),
        "the oldest row stays at 200 rows"
    );
    store
        .record_raw_fallback("run:many", "output 198", Some(now))
        .unwrap();
    store.retain(now).unwrap();
    assert_eq!(
        latest("run:cutoff"),
        None,
        "the oldest row goes at 201 rows"
    );
    assert_eq!(latest("run:new").as_deref(), Some("run:new output"));
    assert_eq!(latest("run:many").as_deref(), Some("output 198"));
}

#[test]
fn state_root_prefers_sniper_root_then_xdg_then_home_skipping_empty_values() {
    use pixel_session::store::state_root_from;
    let lookup = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    };
    assert_eq!(
        state_root_from(lookup(&[
            ("PIXEL_SNIPER_STATE_ROOT", "/s"),
            ("XDG_STATE_HOME", "/x"),
        ])),
        PathBuf::from("/s")
    );
    assert_eq!(
        state_root_from(lookup(&[
            ("PIXEL_SNIPER_STATE_ROOT", ""),
            ("XDG_STATE_HOME", "/x"),
        ])),
        PathBuf::from("/x")
    );
    assert_eq!(
        state_root_from(lookup(&[("XDG_STATE_HOME", ""), ("HOME", "/h")])),
        PathBuf::from("/h/.local/state")
    );
}

#[test]
fn record_event_raw_returns_the_new_row_id() {
    let state = TempRoot::new();
    let project = Path::new("/tmp/raw-ev");
    let store = Store::open_at(project, state.path()).unwrap();
    let first = store.record_event_raw("note", None, None).unwrap();
    let second = store
        .record_event_raw("note", Some(&serde_json::json!({"a": 1})), Some("r"))
        .unwrap();
    let conn = rusqlite::Connection::open(store_path(project, state.path())).unwrap();
    let stored: Vec<(i64, Option<String>)> = conn
        .prepare("SELECT id, run_id FROM events WHERE kind = 'note' ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(stored, vec![(first, None), (second, Some("r".to_string()))]);
}

#[test]
fn store_error_display_names_its_source() {
    let json = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
    let shown = pixel_session::StoreError::from(json).to_string();
    assert!(shown.starts_with("json: "), "{shown}");
}

#[test]
fn gc_counts_rows_dropped_by_age_and_by_row_cap() {
    let state = TempRoot::new();
    let project = Path::new("/tmp/gc-cap");
    let store = Store::open_at(project, state.path()).unwrap();
    let mut old = error(Surface::Reported, "old");
    old.ts = Some(1000);
    store.record_error(&old).unwrap();
    store
        .record_event(&EventInput {
            kind: EventKind::HmrUpdate,
            data: None,
            run_id: None,
            ts: Some(1000),
        })
        .unwrap();
    let aged = store.gc(false).unwrap();
    assert_eq!(aged.errors_deleted, 1, "aged error");
    assert_eq!(aged.events_deleted, 1, "aged event");
    {
        let mut conn = rusqlite::Connection::open(store_path(project, state.path())).unwrap();
        let tx = conn.transaction().unwrap();
        let now = now_ms();
        for i in 0..5001 {
            tx.execute(
                "INSERT INTO errors (first_ts, last_ts, surface, message, dedup_hash)
                 VALUES (?1, ?1, 'reported', 'm', ?2)",
                rusqlite::params![now, format!("h{i}")],
            )
            .unwrap();
        }
        for _ in 0..20_001 {
            tx.execute(
                "INSERT INTO events (ts, kind) VALUES (?1, 'hmr-update')",
                rusqlite::params![now],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let capped = store.gc(false).unwrap();
    assert_eq!(capped.errors_deleted, 1, "one error past the row cap");
    assert_eq!(capped.events_deleted, 1, "one event past the row cap");
}
