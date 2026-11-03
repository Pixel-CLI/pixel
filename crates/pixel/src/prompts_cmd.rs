// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `pixel prompts`: read the central per-user prompt store
//! (`~/.local/share/pixel/prompts/*.jsonl`) back, grouped by project. Each
//! line the central log appends carries the repository root it came from, so
//! grouping is by that root: for every project the command prints the path,
//! how many decisions it holds and the last `--limit` of them, one line each
//! with the timestamp, the gate decision, the question kind and the masked
//! typed text. `--json` emits the same grouping as a structure.

use std::path::Path;

use serde_json::{Value, json};

/// Directory of the central per-user store under `~/.local/share/pixel`, the
/// same name the decision log writes under.
const CENTRAL_DIR: &str = "prompts";

/// One project's slice of the central store: the root every record named and
/// the records themselves, oldest first.
#[derive(Debug, PartialEq)]
pub(crate) struct Project {
    pub(crate) root: String,
    pub(crate) records: Vec<Value>,
}

/// Parse one JSONL file into the records it holds, skipping blank or
/// non-object lines (the store is best effort; a torn line must not lose the
/// rest of the file).
pub(crate) fn parse_file(path: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_lines(&text)
}

/// Parse JSONL text into record objects.
pub(crate) fn parse_lines(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| {
            let value = serde_json::from_str::<Value>(line.trim()).ok()?;
            (value.is_object()).then_some(value)
        })
        .collect()
}

/// Group records by the `root` each carries, in first-appearance order.
pub(crate) fn group(records: Vec<Value>) -> Vec<Project> {
    let mut projects: Vec<Project> = Vec::new();
    for record in records {
        let root = record
            .get("root")
            .and_then(Value::as_str)
            .unwrap_or("(unknown)")
            .to_string();
        match projects.iter_mut().find(|p| p.root == root) {
            Some(project) => project.records.push(record),
            None => projects.push(Project {
                root,
                records: vec![record],
            }),
        }
    }
    projects
}

/// Read every store file and group it, sorted by root for stable output.
pub(crate) fn read_all() -> Vec<Project> {
    let mut projects = read_from(&store_dir());
    projects.sort_by(|a, b| a.root.cmp(&b.root));
    projects
}

/// The central store directory, or an unlikely path when there is no home: a
/// missing directory reads as no projects, which is the truth there too.
fn store_dir() -> std::path::PathBuf {
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.is_empty())
        .map_or_else(
            || Path::new("/nonexistent-pixel-prompts").to_path_buf(),
            |home| {
                Path::new(&home)
                    .join(".local")
                    .join("share")
                    .join("pixel")
                    .join(CENTRAL_DIR)
            },
        )
}

/// Group every `*.jsonl` file under `dir`.
pub(crate) fn read_from(dir: &Path) -> Vec<Project> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    paths.sort();
    let records = paths
        .into_iter()
        .flat_map(|path| parse_file(&path))
        .collect();
    group(records)
}

/// One record as the single text line `pixel prompts` prints: timestamp,
/// gate, kind and the masked typed text, with the pieces the record leaves
/// out shown as `-`.
fn record_line(record: &Value) -> String {
    let field = |key: &str| {
        record
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_string()
    };
    let ts = match record.get("ts").and_then(Value::as_u64) {
        Some(ms) => iso8601(ms),
        None => "-".to_string(),
    };
    let kind = match record.get("kind") {
        Some(Value::String(kind)) => kind.clone(),
        Some(Value::Null) | None => "-".to_string(),
        Some(other) => other.to_string(),
    };
    let typed = record
        .get("typed")
        .and_then(Value::as_str)
        .unwrap_or("-")
        .replace('\n', " ");
    format!("{ts} {} {} {typed}", field("gate"), kind)
}

/// Milliseconds since the epoch as `YYYY-MM-DD HH:MM:SS` UTC, without pulling
/// in a time crate the tree does not already carry elsewhere: the math is
/// civil-from-days.
fn iso8601(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (h, min, s) = (
        secs_of_day / 3600,
        (secs_of_day / 60) % 60,
        secs_of_day % 60,
    );
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02} {h:02}:{min:02}:{s:02}")
}

/// Run the command: print the store grouped by project, as text or JSON.
pub(crate) fn run(json: bool, limit: usize) {
    let projects = read_all();
    if json {
        let out: Vec<Value> = projects
            .iter()
            .map(|project| {
                json!({
                    "root": project.root,
                    "count": project.records.len(),
                    "records": last_records(&project.records, limit),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "projects": out })).unwrap()
        );
    } else if projects.is_empty() {
        println!("no prompt records");
    } else {
        for project in &projects {
            println!("{}: {} record(s)", project.root, project.records.len());
            for record in last_records(&project.records, limit) {
                println!("  {}", record_line(&record));
            }
        }
    }
}

/// The last `limit` records of `records`, in the order they were written.
fn last_records(records: &[Value], limit: usize) -> Vec<Value> {
    let from = records.len().saturating_sub(limit);
    records[from..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pixel-prompts-cmd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record(root: &str, ts: u64, gate: &str, kind: &str, typed: &str) -> Value {
        json!({"root": root, "ts": ts, "gate": gate, "kind": kind, "typed": typed})
    }

    #[test]
    fn a_central_file_groups_by_the_root_each_line_carries() {
        let dir = scratch("group");
        let a = "/tmp/repo-a";
        let b = "/tmp/repo-b";
        let file = dir.join("abc.jsonl");
        std::fs::write(
            &file,
            format!(
                "{}\n{}\n{}\n",
                serde_json::to_string(&record(a, 1, "open", "lookup", "where is X?")).unwrap(),
                serde_json::to_string(&record(b, 2, "declined", "how-to", "how do Y?")).unwrap(),
                serde_json::to_string(&record(a, 3, "open", "action", "do Z")).unwrap(),
            ),
        )
        .unwrap();
        let projects = read_from(&dir);
        assert_eq!(projects.len(), 2, "two roots in one file");
        let pa = &projects[0];
        assert_eq!(pa.root, a);
        assert_eq!(pa.records.len(), 2);
        assert_eq!(pa.records[1]["typed"], "do Z");
        assert_eq!(projects[1].root, b);
    }

    #[test]
    fn a_torn_or_blank_line_loses_nothing_but_itself() {
        let records = parse_lines(
            "{\"root\":\"/a\",\"ts\":1,\"gate\":\"open\"}\n\nnot json\n{\"root\":\"/a\"}\n",
        );
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn the_limit_keeps_the_newest_records_in_written_order() {
        let records: Vec<Value> = (0..5)
            .map(|i| record("/a", i, "open", "lookup", "t"))
            .collect();
        let last = last_records(&records, 20);
        assert_eq!(last.len(), 5, "the limit does not trim a shorter file");
        let last = last_records(&records, 2);
        assert_eq!(last.len(), 2);
        assert_eq!(last[0]["ts"], 3);
        assert_eq!(last[1]["ts"], 4, "the tail keeps the written order");
    }

    #[test]
    fn one_record_prints_as_one_line_with_its_fields() {
        let line = record_line(&json!({
            "root": "/a",
            "ts": 1_700_000_000_000u64,
            "gate": "open",
            "kind": "lookup",
            "typed": "where is the gate checked?"
        }));
        assert_eq!(
            line,
            "2023-11-14 22:13:20 open lookup where is the gate checked?"
        );
    }

    #[test]
    fn a_record_without_the_shown_fields_prints_dashes() {
        let line = record_line(&json!({"root": "/a"}));
        assert_eq!(line, "- - - -");
    }

    #[test]
    fn a_missing_store_directory_reads_as_no_projects() {
        let projects = read_from(&scratch("missing").join("nope"));
        assert!(projects.is_empty());
    }
}
