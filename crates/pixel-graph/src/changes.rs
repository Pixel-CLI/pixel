//! Change detection — `git diff --unified=0` hunk ranges mapped onto indexed
//! symbols, with affected processes and depth-1 upstream callers feeding risk.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;

use crate::impact::processes_for_symbol;
use crate::store::{EdgeKind, GraphStore};
use pixel_git::GitRunner;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone, Serialize)]
pub struct ChangedSymbol {
    pub uid: String,
    pub name: String,
    pub path: String,
    /// "modified" | "added" | "deleted"
    pub change: String,
    pub processes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangesReport {
    pub base: String,
    pub changed_files: u64,
    pub symbols: Vec<ChangedSymbol>,
    pub affected_processes: Vec<String>,
    pub risk: String,
    pub envelope_note: String,
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

    Ok(ChangesReport {
        base,
        changed_files: file_diffs.len() as u64,
        symbols,
        affected_processes,
        risk,
        envelope_note,
    })
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
