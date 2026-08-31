//! Change detection — `git diff --unified=0` hunk ranges mapped onto indexed
//! symbols, with affected processes and depth-1 upstream callers feeding risk.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::Path;

use serde::Serialize;

use crate::concept::is_test_path;
use crate::impact::{file_path_by_id, processes_for_symbol, symbol_by_id};
use crate::store::{EdgeKind, GraphStore};
use pixel_git::GitRunner;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Cap on `suggested_tests` entries (T2: the cap must surface in the report).
const SUGGESTED_TESTS_CAP: usize = 100;

/// Max upstream BFS depth when walking callers looking for test files.
const SUGGESTED_TESTS_MAX_DEPTH: u8 = 3;

#[derive(Debug, Clone, Serialize)]
pub struct ChangedSymbol {
    pub uid: String,
    pub name: String,
    pub path: String,
    /// "modified" | "added" | "deleted"
    pub change: String,
    pub processes: Vec<String>,
}

/// A test file suggested for the current working-tree change set, found by
/// walking UPSTREAM callers of each affected symbol (tests call the code) or
/// because a changed symbol lives in a test file itself.
#[derive(Debug, Clone, Serialize)]
pub struct SuggestedTest {
    /// Repo-relative test file path.
    pub file: String,
    /// Changed symbols this test file was reached from (sorted, deduped).
    pub matched_symbols: Vec<String>,
    /// "direct" (a changed symbol lives in this test file, depth 0)
    /// | "direct-caller" (a test symbol calls a changed symbol, depth 1)
    /// | "transitive" (depth 2-3 through intermediate callers).
    pub via: String,
    /// Minimal call-graph distance from any changed symbol to this file.
    pub depth: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangesReport {
    pub base: String,
    pub changed_files: u64,
    pub symbols: Vec<ChangedSymbol>,
    pub affected_processes: Vec<String>,
    pub risk: String,
    pub envelope_note: String,
    /// Test files that exercise the changed symbols (empty unless
    /// `include_tests` was requested). Sorted by (depth, file).
    pub suggested_tests: Vec<SuggestedTest>,
    /// True when `suggested_tests` was truncated at the cap — more test
    /// files exist than are listed (T2: every cap surfaces).
    pub suggested_tests_lower_bound: bool,
    /// Honest limitations of the mapping (caps hit, extraction blind spots).
    pub suggested_tests_note: String,
}

#[derive(Debug, PartialEq)]
enum FileStatus {
    Added,
    Deleted,
    Modified,
}

#[derive(Debug)]
struct FileDiff {
    path: String,
    status: FileStatus,
    /// Changed line ranges in the NEW file's coordinates (inclusive).
    new_ranges: Vec<(u32, u32)>,
}

/// Parse `git diff --unified=0` output into per-file changed ranges.
fn parse_diff(output: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut old_path: Option<String> = None;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("--- ") {
            old_path = Some(rest.trim().trim_start_matches("a/").to_string());
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            let new = rest.trim();
            let (path, status) = if new == "/dev/null" {
                (old_path.clone().unwrap_or_default(), FileStatus::Deleted)
            } else {
                let p = new.trim_start_matches("b/").to_string();
                let status = if old_path.as_deref() == Some("/dev/null") {
                    FileStatus::Added
                } else {
                    FileStatus::Modified
                };
                (p, status)
            };
            files.push(FileDiff {
                path,
                status,
                new_ranges: Vec::new(),
            });
        } else if line.starts_with("@@") {
            // @@ -a[,b] +c[,d] @@
            if let Some(cur) = files.last_mut()
                && let Some(plus) = line.split(' ').find(|t| t.starts_with('+'))
            {
                let spec = &plus[1..];
                let mut it = spec.splitn(2, ',');
                let start: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                let len: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(1);
                if start > 0 {
                    let end = start + len.saturating_sub(1);
                    cur.new_ranges.push((start, end.max(start)));
                } else if len == 0 {
                    // pure deletion: mark the position after which lines
                    // were removed so adjacent symbols are caught.
                    let anchor = start.max(1);
                    cur.new_ranges.push((anchor, anchor));
                }
            }
        }
    }
    files
}

fn overlaps(ranges: &[(u32, u32)], start: u32, end: u32) -> bool {
    ranges.iter().any(|&(a, b)| a <= end && start <= b)
}

/// Validate a user-supplied git ref for `changes --base`. A ref may be a
/// commit oid, branch/tag name, or a rev expression (`HEAD~1`, `main@{1}`),
/// but it must never be parsed by git as an option: anything starting with
/// `-` is rejected to block option injection (e.g. `--output=/etc/passwd`).
///
/// Now delegates to `pixel_git::validate_ref`, which is the single shared
/// validator (and accepts mid-string dashes like `fix-bug`). The production
/// path (`detect`) uses `GitRunner::diff_unified0` which validates inline;
/// this wrapper is retained so the existing test
/// (`validate_base_ref_rejects_leading_dash`) continues to exercise the
/// contract without depending on pixel-git's internal error type.
#[cfg(test)]
fn validate_base_ref(r: &str) -> Result<&str, BoxError> {
    pixel_git::validate_ref(r).map_err(|e| -> BoxError {
        format!("invalid base ref {r:?}: {e}").into()
    })?;
    Ok(r)
}

pub fn detect(
    store: &GraphStore,
    root: &Path,
    base_ref: Option<&str>,
    include_tests: bool,
) -> Result<ChangesReport, BoxError> {
    let base = base_ref.unwrap_or("HEAD").to_string();
    let runner = GitRunner::new(root);
    let diff_bytes = runner.diff_unified0(base_ref)?;
    let diff = String::from_utf8_lossy(&diff_bytes).into_owned();
    let file_diffs = parse_diff(&diff);

    let mut symbols: Vec<ChangedSymbol> = Vec::new();
    let mut proc_set: BTreeSet<String> = BTreeSet::new();
    let mut caller_ids: BTreeSet<i64> = BTreeSet::new();
    let mut lower_bound_names: BTreeSet<String> = BTreeSet::new();
    // (symbol rowid, name, path) of every affected symbol — the seeds for
    // the upstream test-file walk when `include_tests` is set.
    let mut changed_seeds: Vec<(i64, String, String)> = Vec::new();

    for fd in &file_diffs {
        let file = match store.file_by_path(&fd.path)? {
            Some(f) => f,
            None => continue, // not indexed (e.g. new file before re-index)
        };
        let in_file = store.symbols_in_file(file.id)?;
        for sym in in_file {
            let hit = match fd.status {
                FileStatus::Deleted => true,
                _ => overlaps(&fd.new_ranges, sym.start_line, sym.end_line),
            };
            if !hit {
                continue;
            }
            let change = match fd.status {
                FileStatus::Added => "added",
                FileStatus::Deleted => "deleted",
                FileStatus::Modified => "modified",
            }
            .to_string();
            let procs = processes_for_symbol(store, sym.id)?;
            for p in &procs {
                proc_set.insert(p.clone());
            }
            // depth-1 upstream callers
            for e in store.edges_to(sym.id, Some(EdgeKind::Calls))? {
                caller_ids.insert(e.src_id);
                for p in processes_for_symbol(store, e.src_id)? {
                    proc_set.insert(p);
                }
            }
            let env = store.envelope_for_name(&sym.name)?;
            if env.lower_bound {
                lower_bound_names.insert(sym.name.clone());
            }
            if include_tests {
                changed_seeds.push((sym.id, sym.name.clone(), fd.path.clone()));
            }
            symbols.push(ChangedSymbol {
                uid: sym.uid,
                name: sym.name,
                path: fd.path.clone(),
                change,
                processes: procs,
            });
        }
    }

    let affected_processes: Vec<String> = proc_set.into_iter().collect();
    let d1 = caller_ids.len();
    let nproc = affected_processes.len();
    let lower_bound = !lower_bound_names.is_empty();
    let mut level: u8 = if d1 > 50 || nproc > 20 {
        3
    } else if d1 > 15 || nproc > 8 {
        2
    } else if d1 > 3 {
        1
    } else {
        0
    };
    if lower_bound && level < 3 {
        level += 1;
    }
    let risk = match level {
        0 => "LOW",
        1 => "MEDIUM",
        2 => "HIGH",
        _ => "CRITICAL",
    }
    .to_string();
    let envelope_note = if lower_bound {
        format!(
            "lower bound: unresolved same-name call sites exist for {}",
            lower_bound_names.into_iter().collect::<Vec<_>>().join(", ")
        )
    } else {
        "all call sites for changed symbols resolved".to_string()
    };

    let (suggested_tests, suggested_tests_lower_bound, suggested_tests_note) = if include_tests {
        suggest_tests(store, &changed_seeds)?
    } else {
        (Vec::new(), false, String::new())
    };

    Ok(ChangesReport {
        base,
        changed_files: file_diffs.len() as u64,
        symbols,
        affected_processes,
        risk,
        envelope_note,
        suggested_tests,
        suggested_tests_lower_bound,
        suggested_tests_note,
    })
}

/// Map affected symbols to the test files that exercise them.
///
/// Two sources, deduped by file with the minimal depth kept:
/// 1. depth 0, via "direct" — a changed symbol lives in a test file itself
///    (a changed test is its own suggested test);
/// 2. depth 1..=3 — UPSTREAM callers of each changed symbol (tests call the
///    code), via "direct-caller" at depth 1 and "transitive" beyond.
///
/// Honest limitation (surfaced in the note, never guessed around): the Rust
/// extractor skips `#[test]` functions and `#[cfg(test)]` modules entirely
/// (`extract::rust_is_test_container`), so in-file Rust unit tests have no
/// graph nodes and cannot be reached by the caller walk. Non-`#[test]`
/// helper symbols in `tests/` integration files ARE indexed and do resolve.
fn suggest_tests(
    store: &GraphStore,
    seeds: &[(i64, String, String)],
) -> Result<(Vec<SuggestedTest>, bool, String), BoxError> {
    // file -> (min depth, matched changed-symbol names)
    let mut by_file: BTreeMap<String, (u8, BTreeSet<String>)> = BTreeMap::new();
    let record = |file: String, depth: u8, symbol: &str, map: &mut BTreeMap<String, (u8, BTreeSet<String>)>| {
        let entry = map
            .entry(file)
            .or_insert_with(|| (depth, BTreeSet::new()));
        entry.0 = entry.0.min(depth);
        entry.1.insert(symbol.to_string());
    };

    for (seed_id, seed_name, seed_path) in seeds {
        // Source 1: the changed symbol is itself in a test file.
        if is_test_path(seed_path) {
            record(seed_path.clone(), 0, seed_name, &mut by_file);
        }
        // Source 2: BFS upstream over `calls` edges, depth ≤ 3.
        let mut visited: HashSet<i64> = HashSet::new();
        visited.insert(*seed_id);
        let mut queue: VecDeque<(i64, u8)> = VecDeque::new();
        queue.push_back((*seed_id, 0));
        while let Some((id, depth)) = queue.pop_front() {
            if depth >= SUGGESTED_TESTS_MAX_DEPTH {
                continue;
            }
            for e in store.edges_to(id, Some(EdgeKind::Calls))? {
                if !visited.insert(e.src_id) {
                    continue;
                }
                let d = depth + 1;
                if let Some(caller) = symbol_by_id(store, e.src_id)? {
                    let path = file_path_by_id(store, caller.file_id)?;
                    if is_test_path(&path) {
                        record(path, d, seed_name, &mut by_file);
                    }
                }
                queue.push_back((e.src_id, d));
            }
        }
    }

    let mut out: Vec<SuggestedTest> = by_file
        .into_iter()
        .map(|(file, (depth, matched))| SuggestedTest {
            file,
            matched_symbols: matched.into_iter().collect(),
            via: match depth {
                0 => "direct",
                1 => "direct-caller",
                _ => "transitive",
            }
            .to_string(),
            depth,
        })
        .collect();
    // Nearest tests first; BTreeMap already ordered by file for ties.
    out.sort_by(|a, b| a.depth.cmp(&b.depth).then(a.file.cmp(&b.file)));

    let total = out.len();
    let lower_bound = total > SUGGESTED_TESTS_CAP;
    if lower_bound {
        out.truncate(SUGGESTED_TESTS_CAP);
    }

    let mut notes: Vec<String> = Vec::new();
    if lower_bound {
        notes.push(format!(
            "lower bound: {total} test files matched, truncated to {SUGGESTED_TESTS_CAP}"
        ));
    }
    if seeds.iter().any(|(_, _, p)| p.ends_with(".rs")) {
        notes.push(
            "Rust #[test] functions and #[cfg(test)] modules are not in the graph \
             (extraction skips test containers); in-file Rust unit tests cannot be \
             suggested via the caller walk — tests/ integration helpers are covered"
                .to_string(),
        );
    }
    Ok((out, lower_bound, notes.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_base_ref_rejects_leading_dash() {
        // Option injection: a base ref starting with '-' must be rejected.
        assert!(validate_base_ref("--output=/etc/passwd").is_err());
        assert!(validate_base_ref("-x").is_err());
        assert!(validate_base_ref("").is_err());
        // Valid refs pass.
        assert!(validate_base_ref("HEAD").is_ok());
        assert!(validate_base_ref("HEAD~1").is_ok());
        assert!(validate_base_ref("main").is_ok());
        assert!(validate_base_ref("abcdef1234567890").is_ok());
    }
}
