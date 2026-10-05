// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Existing-graph impact is bounded and never repairs an index as a side effect.
use crate::support::{Scratch, git, pixel_command};
use pixel_graph::GraphStore;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Output;

const SOURCE: &str = "export function leaf() { return 1; }\nexport function caller() { return leaf(); }\nexport function outer() { return caller(); }\nexport function entry() { return outer(); }\n";

fn fixture(tag: &str, indexed: bool) -> Scratch {
    let dir = Scratch::for_test("pixel-impact-read", tag);
    git(&dir, &["init", "-q"]);
    std::fs::write(dir.join(".gitignore"), ".pixel/\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/example.ts"), SOURCE).unwrap();
    if indexed {
        build(&dir);
    }
    dir
}

fn database(dir: &Path) -> PathBuf {
    dir.join(".pixel").join(pixel_daemon::api::GRAPH_DB_FILE)
}

fn build(dir: &Path) {
    let output = pixel_command()
        .args(["rebuild-graph", "--metrics", "off"])
        .arg(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

fn query(dir: &Path, symbol: &str, args: &[&str]) -> Output {
    pixel_command()
        .args([
            "impact",
            symbol,
            "--no-refresh",
            "--json",
            "--metrics",
            "off",
        ])
        .arg(dir)
        .args(args)
        .current_dir(dir)
        .env_remove("PIXEL_SESSION_ID")
        .env_remove("PIXEL_AGENT_ID")
        .output()
        .unwrap()
}

fn success(output: &Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

fn rejected(output: &Output, reason: &str) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains(reason), "{text}");
}

#[test]
fn impact_read_existing_graph_preserves_depth_direction_and_epistemics() {
    let dir = fixture("fresh", true);
    let before = std::fs::read(database(&dir)).unwrap();
    let direct = success(&query(&dir, "leaf", &["--depth", "1"]));
    assert_eq!(direct["counts_by_depth"], serde_json::json!([1, 0, 0]));
    assert_eq!(direct["d1_will_break"][0]["name"], "caller");
    assert_eq!(direct["d1_will_break"][0]["path"], "src/example.ts");
    assert_eq!(direct["d1_will_break"][0]["line"], 2);
    assert_eq!(direct["direction"], "upstream");
    assert_eq!(direct["epistemics"]["closed_world"], false);
    assert_eq!(direct["epistemics"]["lower_bound"], true);
    assert_eq!(
        direct["epistemics"]["basis"],
        "existing graph; source signature checked"
    );
    let default_depth = success(&query(&dir, "leaf", &[]));
    assert_eq!(
        default_depth["counts_by_depth"],
        serde_json::json!([1, 1, 0])
    );
    let full = success(&query(
        &dir,
        "src/example.ts#leaf#function",
        &["--depth", "3"],
    ));
    assert_eq!(full["counts_by_depth"], serde_json::json!([1, 1, 1]));
    assert_eq!(full["d3_may_need_tests"][0]["name"], "entry");
    let down = success(&query(
        &dir,
        "entry",
        &["--direction", "downstream", "--depth", "3"],
    ));
    assert_eq!(down["direction"], "downstream");
    assert_eq!(down["d1_will_break"][0]["name"], "outer");
    assert_eq!(down["d3_may_need_tests"][0]["name"], "leaf");
    assert_eq!(std::fs::read(database(&dir)).unwrap(), before);
}

#[test]
fn impact_read_missing_graph_does_not_build_one() {
    let dir = fixture("missing", false);
    let output = query(&dir, "leaf", &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(!database(&dir).exists());
    assert!(!dir.join(".pixel/base.shard").exists());
}

#[test]
fn impact_read_locked_database_fails_without_waiting_or_repairing() {
    let dir = fixture("locked", true);
    let store = GraphStore::open(&database(&dir)).unwrap();
    store
        .conn()
        .execute_batch("PRAGMA journal_mode=DELETE")
        .unwrap();
    let before = std::fs::read(database(&dir)).unwrap();
    // Closing an unrelated descriptor for this file releases this process's
    // POSIX locks, so take the byte snapshot before acquiring the lock.
    store.conn().execute_batch("BEGIN EXCLUSIVE").unwrap();
    let start = std::time::Instant::now();
    rejected(&query(&dir, "leaf", &[]), "database is locked");
    assert!(start.elapsed() < std::time::Duration::from_millis(900));
    store.conn().execute_batch("ROLLBACK").unwrap();
    assert_eq!(std::fs::read(database(&dir)).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn impact_read_process_exits_when_graph_open_never_returns() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let dir = fixture("deadline", false);
    std::fs::create_dir_all(dir.join(".pixel")).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(database(&dir))
            .status()
            .unwrap()
            .success()
    );
    let mut child = pixel_command()
        .args(["impact", "leaf", "--no-refresh", "--metrics", "off"])
        .arg(&*dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("query process exceeded its deadline: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    rejected(&output, "impact query unavailable within 1500 ms");
}

#[test]
fn impact_read_detects_added_deleted_and_equal_length_changed_source() {
    let dir = fixture("stale", true);
    let before = std::fs::read(database(&dir)).unwrap();
    let source = dir.join("src/example.ts");
    let modified = std::fs::metadata(&source).unwrap().modified().unwrap();
    std::fs::write(&source, SOURCE.replace("return 1", "return 2")).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    rejected(&query(&dir, "leaf", &[]), "graph is stale");
    std::fs::write(&source, SOURCE).unwrap();
    let added = dir.join("src/added.ts");
    std::fs::write(&added, "export function extra() { return leaf(); }\n").unwrap();
    rejected(&query(&dir, "leaf", &[]), "graph is stale");
    std::fs::remove_file(added).unwrap();
    std::fs::remove_file(source).unwrap();
    rejected(&query(&dir, "leaf", &[]), "graph is stale");
    assert_eq!(std::fs::read(database(&dir)).unwrap(), before);
}

#[test]
fn impact_read_rejects_missing_and_ambiguous_symbols_but_accepts_uid() {
    let dir = fixture("ambiguous", true);
    rejected(
        &query(&dir, "missing", &[]),
        "symbol is missing or ambiguous",
    );
    rejected(
        &query(&dir, "src/example.ts#missing#function", &[]),
        "symbol is absent",
    );
    std::fs::write(
        dir.join("src/other.ts"),
        "export function leaf() { return 0; }\n",
    )
    .unwrap();
    build(&dir);
    rejected(&query(&dir, "leaf", &[]), "symbol is missing or ambiguous");
    let explicit = success(&query(&dir, "src/example.ts#leaf#function", &[]));
    assert_eq!(explicit["target"], "src/example.ts#leaf#function");
}

#[test]
fn impact_read_rejects_unknown_freshness_and_incompatible_extractor() {
    let dir = fixture("metadata", true);
    {
        let store = GraphStore::open(&database(&dir)).unwrap();
        store
            .meta_set(pixel_graph::build::EXTRACTOR_VERSION_KEY, "old-extractor")
            .unwrap();
    }
    let before = std::fs::read(database(&dir)).unwrap();
    rejected(
        &query(&dir, "leaf", &[]),
        "extractor is unavailable or outdated",
    );
    assert_eq!(std::fs::read(database(&dir)).unwrap(), before);
    build(&dir);
    {
        let store = GraphStore::open(&database(&dir)).unwrap();
        store
            .conn()
            .execute(
                "DELETE FROM meta WHERE key = ?1",
                [pixel_graph::build::FRESHNESS_KEY],
            )
            .unwrap();
    }
    let before = std::fs::read(database(&dir)).unwrap();
    rejected(&query(&dir, "leaf", &[]), "freshness is unknown");
    assert_eq!(std::fs::read(database(&dir)).unwrap(), before);
}

#[test]
fn impact_read_rejects_depth_outside_budget_and_workspace_fanout() {
    let dir = fixture("options", false);
    for depth in ["0", "4"] {
        rejected(
            &query(&dir, "leaf", &["--depth", depth]),
            "depth 1 through 3",
        );
    }
    let output = query(&dir, "leaf", &["--workspace"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
    assert!(!database(&dir).exists());
}

#[test]
fn impact_read_result_budget_accepts_exact_boundary_then_refuses_one_byte_more() {
    let dir = fixture("bytes", true);
    // A direct caller's indexed path is emitted once. Grow it to exercise the
    // serialized wire boundary without enormous source fixtures or graph walks.
    let initial = success(&query(&dir, "leaf", &["--depth", "1"]));
    let bytes = serde_json::to_vec(&initial).unwrap().len();
    let path = format!("src/example.ts{}", "x".repeat(32_768 - bytes));
    {
        let store = GraphStore::open(&database(&dir)).unwrap();
        store
            .conn()
            .execute("UPDATE files SET path = ?1", [&path])
            .unwrap();
    }
    let exact = success(&query(&dir, "leaf", &["--depth", "1"]));
    assert_eq!(serde_json::to_vec(&exact).unwrap().len(), 32_768);
    {
        let store = GraphStore::open(&database(&dir)).unwrap();
        store
            .conn()
            .execute("UPDATE files SET path = path || 'x'", [])
            .unwrap();
    }
    rejected(
        &query(&dir, "leaf", &["--depth", "1"]),
        "exceeds 32768 bytes",
    );
}
