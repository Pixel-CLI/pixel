//! Bulk-export integration tests against a small fake ingested corpus.

use std::path::Path;

use pixel_recall::export::{ExportFilters, ExportFormat, export};
use pixel_recall::model::{Role, TsSource, UnifiedSession, UnifiedTurn};
use pixel_recall::store::{IngestState, RecallStore};

const TS_OLD: i64 = 1_700_000_000_000; // 2023-11-14
const TS_NEW: i64 = 1_760_000_000_000; // 2025-10-09

fn mk_store(dir: &Path) -> RecallStore {
    RecallStore::open(&dir.join("recall.db")).expect("open store")
}

fn add_session(
    store: &mut RecallStore,
    agent: &'static str,
    source_session_id: &str,
    title: Option<&str>,
    ts_source: TsSource,
    turns: &[(Role, Option<i64>, &str)],
) -> i64 {
    let session = UnifiedSession {
        agent,
        source_session_id: source_session_id.to_string(),
        source_path: format!("/fake/{agent}/{source_session_id}.jsonl"),
        cwd: Some("/Users/livio/Documents/pixel".to_string()),
        git_branch: None,
        title: title.map(str::to_string),
        ts_source,
        is_subagent: false,
        parent_source_session_id: None,
    };
    let turns: Vec<UnifiedTurn> = turns
        .iter()
        .map(|(role, ts, text)| UnifiedTurn {
            role: *role,
            intent_source: None,
            ts: *ts,
            text: text.to_string(),
            truncated: false,
            source_byte_start: None,
            source_byte_len: None,
        })
        .collect();
    let state = IngestState {
        file_size: 1,
        mtime_ms: 1,
        bytes_ingested: 1,
        cursor: None,
    };
    store
        .replace_session(&session, &turns, source_session_id, &state)
        .expect("replace_session")
}

fn dir_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read out dir")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn md_export_all_sessions_and_ts_disclosure() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = mk_store(tmp.path());
    add_session(
        &mut store,
        "claude",
        "aaaa1111",
        Some("Fix the engine"),
        TsSource::Iso,
        &[
            (Role::User, Some(TS_NEW), "please fix the engine"),
            (Role::Assistant, Some(TS_NEW + 60_000), "done, engine fixed"),
        ],
    );
    add_session(
        &mut store,
        "cursor",
        "bbbb2222",
        Some("No clocks here"),
        TsSource::Absent,
        &[(Role::User, None, "timeless prompt")],
    );

    let out = tmp.path().join("export-md");
    let summary = export(&store, &ExportFilters::default(), &out, ExportFormat::Md).unwrap();
    assert_eq!(summary.sessions_exported, 2);
    assert_eq!(summary.turns, 3);
    assert!(summary.skipped_unresolvable_ts.is_empty());
    assert!(!summary.truncated);

    let files = dir_files(&out);
    assert_eq!(files.len(), 2);
    let claude_file = files.iter().find(|f| f.starts_with("claude-")).unwrap();
    assert!(claude_file.contains("fix-the-engine"), "got {claude_file}");
    assert!(claude_file.ends_with(".md"));
    assert!(claude_file.contains("20251009"), "got {claude_file}");
    let content = std::fs::read_to_string(out.join(claude_file)).unwrap();
    assert!(content.starts_with("# Fix the engine — claude @ "));
    assert!(content.contains("session: claude:aaaa1111"));
    assert!(content.contains("ts_source: iso"));
    assert!(content.contains("## [user] 2025-10-09"));
    assert!(content.contains("please fix the engine"));
    assert!(content.contains("## [assistant]"));

    // Absent-ts session must disclose it (T2) and carry the nodate stamp.
    let cursor_file = files.iter().find(|f| f.starts_with("cursor-")).unwrap();
    assert!(cursor_file.contains("nodate"), "got {cursor_file}");
    let content = std::fs::read_to_string(out.join(cursor_file)).unwrap();
    assert!(content.contains("ts_source: absent (no timestamps recorded in the source)"));
}

#[test]
fn jsonl_export_turn_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = mk_store(tmp.path());
    add_session(
        &mut store,
        "zcode",
        "cccc3333",
        Some("Engine talk"),
        TsSource::Mtime,
        &[
            (Role::User, Some(TS_NEW), "tell me about the engine"),
            (Role::Assistant, Some(TS_NEW + 1), "the engine goes brrr"),
        ],
    );
    let out = tmp.path().join("export-jsonl");
    let summary = export(&store, &ExportFilters::default(), &out, ExportFormat::Jsonl).unwrap();
    assert_eq!(summary.sessions_exported, 1);
    assert_eq!(summary.turns, 2);

    let files = dir_files(&out);
    assert_eq!(files.len(), 1);
    assert!(files[0].ends_with(".jsonl"));
    let content = std::fs::read_to_string(out.join(&files[0])).unwrap();
    let lines: Vec<serde_json::Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["agent"], "zcode");
    assert_eq!(lines[0]["session"], "cccc3333");
    assert_eq!(lines[0]["idx"], 0);
    assert_eq!(lines[0]["role"], "user");
    assert_eq!(lines[0]["ts"], TS_NEW);
    assert_eq!(lines[0]["ts_source"], "mtime");
    assert_eq!(lines[0]["text"], "tell me about the engine");
    assert_eq!(lines[1]["idx"], 1);
    assert_eq!(lines[1]["role"], "assistant");
}

#[test]
fn date_window_filters_and_reports_unresolvable_ts() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = mk_store(tmp.path());
    add_session(
        &mut store,
        "claude",
        "recent01",
        Some("Recent work"),
        TsSource::Iso,
        &[(Role::User, Some(TS_NEW), "recent prompt")],
    );
    add_session(
        &mut store,
        "devin",
        "ancient1",
        Some("Old work"),
        TsSource::Iso,
        &[(Role::User, Some(TS_OLD), "old prompt")],
    );
    add_session(
        &mut store,
        "cursor",
        "noclock1",
        Some("Timeless"),
        TsSource::Absent,
        &[(Role::User, None, "timeless prompt")],
    );

    // Window that only the recent session satisfies.
    let filters = ExportFilters {
        since_ms: Some(TS_NEW - 86_400_000),
        until_ms: Some(TS_NEW + 86_400_000),
        ..Default::default()
    };
    let out = tmp.path().join("export-window");
    let summary = export(&store, &filters, &out, ExportFormat::Md).unwrap();
    assert_eq!(summary.sessions_exported, 1);
    assert_eq!(summary.turns, 1);
    // The old session is excluded normally; the clock-less one is REPORTED.
    assert_eq!(summary.skipped_unresolvable_ts, vec!["cursor:noclock1"]);
    let files = dir_files(&out);
    assert_eq!(files.len(), 1);
    assert!(files[0].starts_with("claude-recent-work-"));

    // Without a date filter the clock-less session exports normally.
    let out_all = tmp.path().join("export-all");
    let summary = export(&store, &ExportFilters::default(), &out_all, ExportFormat::Md).unwrap();
    assert_eq!(summary.sessions_exported, 3);
    assert!(summary.skipped_unresolvable_ts.is_empty());
}

#[test]
fn agent_and_session_filters() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = mk_store(tmp.path());
    let claude_id = add_session(
        &mut store,
        "claude",
        "dddd4444",
        Some("Claude session"),
        TsSource::Iso,
        &[(Role::User, Some(TS_NEW), "hi claude")],
    );
    add_session(
        &mut store,
        "codex",
        "eeee5555",
        Some("Codex session"),
        TsSource::Iso,
        &[(Role::User, Some(TS_NEW), "hi codex")],
    );

    let out = tmp.path().join("by-agent");
    let filters = ExportFilters {
        agent: Some("codex".to_string()),
        ..Default::default()
    };
    let summary = export(&store, &filters, &out, ExportFormat::Md).unwrap();
    assert_eq!(summary.sessions_exported, 1);
    assert!(dir_files(&out)[0].starts_with("codex-"));

    let out = tmp.path().join("by-session");
    let filters = ExportFilters {
        session_id: Some(claude_id),
        ..Default::default()
    };
    let summary = export(&store, &filters, &out, ExportFormat::Md).unwrap();
    assert_eq!(summary.sessions_exported, 1);
    assert!(dir_files(&out)[0].starts_with("claude-"));

    // Session id + mismatching agent filter exports nothing.
    let out = tmp.path().join("mismatch");
    let filters = ExportFilters {
        agent: Some("codex".to_string()),
        session_id: Some(claude_id),
        ..Default::default()
    };
    let summary = export(&store, &filters, &out, ExportFormat::Md).unwrap();
    assert_eq!(summary.sessions_exported, 0);
}

#[test]
fn md_fences_grow_past_embedded_backticks_and_names_dedupe() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = mk_store(tmp.path());
    add_session(
        &mut store,
        "claude",
        "ffff6666",
        Some("Same title"),
        TsSource::Iso,
        &[(
            Role::Assistant,
            Some(TS_NEW),
            "code:\n```rust\nfn main() {}\n```\ndone",
        )],
    );
    add_session(
        &mut store,
        "claude",
        "gggg7777",
        Some("Same title"),
        TsSource::Iso,
        &[(Role::User, Some(TS_NEW), "second twin")],
    );

    let out = tmp.path().join("twins");
    let summary = export(&store, &ExportFilters::default(), &out, ExportFormat::Md).unwrap();
    assert_eq!(summary.sessions_exported, 2);
    let files = dir_files(&out);
    assert_eq!(files.len(), 2, "colliding names must not overwrite: {files:?}");

    let with_code = files
        .iter()
        .map(|f| std::fs::read_to_string(out.join(f)).unwrap())
        .find(|c| c.contains("fn main"))
        .expect("session with embedded fence exported");
    assert!(
        with_code.contains("````\n"),
        "fence must outgrow embedded ``` runs"
    );
}
