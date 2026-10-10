// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! CLI measurement adapter for pixel-actionlog. No descriptor redirection,
//! comparison subprocesses, source reads, or changes to terminal detection;
//! the one file a whole-file reader stands in for is measured by `stat`.
use pixel_actionlog::{ComparisonGap, WorkflowEvidence};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
pub static OUTPUT_BYTES: AtomicU64 = AtomicU64::new(0);
/// Bytes this invocation rendered to **stdout** alone (`OUTPUT_BYTES` counts
/// both streams): the answer bytes. The CLI asks [`stdout_bytes`] before it
/// appends a failure envelope, because a command that already wrote part of
/// its answer keeps stdout for it.
pub static STDOUT_BYTES: AtomicU64 = AtomicU64::new(0);
pub struct Counted<W>(pub W);
impl<W: Write> Write for Counted<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.0.write(bytes)?;
        OUTPUT_BYTES.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// A stdout write: counts as rendered output (`OUTPUT_BYTES`) and as answer
/// bytes (`STDOUT_BYTES`). Every stdout path goes through this or
/// [`print`], so "the command already wrote something" is one answer for
/// both.
pub struct Stdout<W>(pub W);
impl<W: Write> Write for Stdout<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.0.write(bytes)?;
        OUTPUT_BYTES.fetch_add(n as u64, Ordering::Relaxed);
        STDOUT_BYTES.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// A reader that closed its end (`… | head -1`) is a success, the policy
/// `write_stdout` applies to the capped JSON path; every other write failure
/// keeps the ordinary `print!` contract.
fn write_absorbing_closed_reader<W: Write>(
    sink: &mut W,
    args: std::fmt::Arguments<'_>,
) -> io::Result<()> {
    match sink.write_fmt(args) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

pub fn print(args: std::fmt::Arguments<'_>) {
    // Retain the ordinary print! failure contract. Only successful writes count.
    if let Err(error) = write_absorbing_closed_reader(&mut Stdout(io::stdout().lock()), args) {
        panic!("failed printing to stdout: {error}");
    }
}

pub fn print_error(args: std::fmt::Arguments<'_>) {
    if let Err(error) = write_absorbing_closed_reader(&mut Counted(io::stderr().lock()), args) {
        panic!("failed printing to stderr: {error}");
    }
}

#[derive(Default)]
struct Evidence {
    root: PathBuf,
    files: HashSet<PathBuf>,
    relationships: HashSet<String>,
    partial: bool,
    gap: Option<ComparisonGap>,
}
static EVIDENCE: Mutex<Option<Evidence>> = Mutex::new(None);

pub fn begin(root: &Path) {
    OUTPUT_BYTES.store(0, Ordering::Relaxed);
    STDOUT_BYTES.store(0, Ordering::Relaxed);
    if let Ok(mut slot) = EVIDENCE.lock() {
        *slot = Some(Evidence {
            root: root.to_path_buf(),
            ..Evidence::default()
        });
    }
}

pub fn output_bytes() -> u64 {
    OUTPUT_BYTES.load(Ordering::Relaxed)
}

/// Bytes rendered to stdout: `0` means this invocation has not answered yet
/// on stdout, which is what decides whether a failure envelope is appended.
pub fn stdout_bytes() -> u64 {
    STDOUT_BYTES.load(Ordering::Relaxed)
}

pub fn unavailable() {
    if let Ok(mut slot) = EVIDENCE.lock()
        && let Some(e) = slot.as_mut()
    {
        e.gap = Some(ComparisonGap::OutputTruncated);
    }
}

fn collect(value: &Value, evidence: &mut Evidence, depth: usize) {
    if depth > 48 {
        evidence.gap = Some(ComparisonGap::EvidenceDepthCapped);
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                collect(item, evidence, depth + 1);
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                if (matches!(key.as_str(), "truncated" | "lower_bound" | "partial")
                    || key.ends_with("_truncated"))
                    && value == true
                {
                    evidence.partial = true;
                }
                if key == "offset" && value.as_u64().is_some_and(|v| v > 0) {
                    evidence.partial = true;
                }
                if matches!(key.as_str(), "path" | "file" | "file_path")
                    && let Some(path) = value.as_str().filter(|p| !p.is_empty())
                {
                    evidence.files.insert(evidence.root.join(path));
                }
                if matches!(
                    key.as_str(),
                    "callers"
                        | "callees"
                        | "edges"
                        | "d1_will_break"
                        | "d2_likely_affected"
                        | "d3_may_need_tests"
                ) && let Some(items) = value.as_array()
                {
                    for item in items {
                        // Only returned relationships, not total counts or unseen pages.
                        evidence.relationships.insert(format!("{key}:{item}"));
                    }
                }
                // Do not reinterpret source text, user annotations, or history hunks.
                if value.is_array() || value.is_object() {
                    collect(value, evidence, depth + 1);
                }
            }
        }
        _ => (),
    }
}

pub fn observe(value: &Value) {
    if let Ok(mut slot) = EVIDENCE.lock()
        && let Some(e) = slot.as_mut()
    {
        collect(value, e, 0);
    }
}

/// v1 command counts are explicit workflow policy, not measured executions.
fn native_commands(command: &str) -> Option<u64> {
    match command {
        "search-content" | "run-recipe" | "search-meaning" | "find-code" | "find-symbol"
        | "pack-context" | "list-signatures" | "repo-map" | "scope-task" | "who-calls"
        | "impact" | "call-path" | "what-changed" | "list-areas" | "list-flows"
        | "commit-history" | "search-history" | "dig-history" | "file-history" | "who-wrote"
        | "diff" | "list-branches" | "fetch" | "new-branch" | "push" => Some(1),
        "repo-state" | "review-changes" | "commit" => Some(3),
        "commit-and-push" => Some(4),
        // Recovery/task/flow/reconcile depend on the actual guarded plan; do not
        // invent the native steps for these or administrative commands.
        _ => None,
    }
}

pub fn evidence(command: &str, succeeded: bool) -> Result<WorkflowEvidence, ComparisonGap> {
    if !succeeded {
        return Err(ComparisonGap::OperationFailed);
    }
    let Some(commands) = native_commands(command) else {
        return Err(ComparisonGap::NoPolicy);
    };
    let slot = EVIDENCE.lock().map_err(|_| ComparisonGap::Uninitialized)?;
    let e = slot.as_ref().ok_or(ComparisonGap::Uninitialized)?;
    if let Some(gap) = e.gap {
        return Err(gap);
    }
    Ok(workflow_evidence(command, commands, e))
}

/// Commands whose native equivalent is reading one whole file, no command.
const WHOLE_FILE_READERS: &[&str] = &["list-signatures"];

/// The size of the one file a whole-file reader stood in for, from its metadata.
///
/// `None` when the command is not a whole-file reader, when its answer named
/// no file or several, or when the file cannot be measured: the caller then
/// keeps the policy estimate. A `stat`, never a read of the source.
fn whole_file_bytes(command: &str, files: &HashSet<PathBuf>) -> Option<u64> {
    if !WHOLE_FILE_READERS.contains(&command) || files.len() != 1 {
        return None;
    }
    let file = files.iter().next()?;
    std::fs::metadata(file)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map(|meta| meta.len())
}

/// The baseline of one successful invocation from what its answer returned.
fn workflow_evidence(command: &str, commands: u64, e: &Evidence) -> WorkflowEvidence {
    if let Some(bytes) = whole_file_bytes(command, &e.files) {
        return WorkflowEvidence {
            distinct_files: 1,
            relationships: 0,
            native_commands: 0,
            known_file_bytes: Some(bytes),
            partial: e.partial,
        };
    }
    let reads_evidence = matches!(
        command,
        "search-content"
            | "run-recipe"
            | "search-meaning"
            | "find-code"
            | "find-symbol"
            | "pack-context"
            | "list-signatures"
            | "repo-map"
            | "scope-task"
            | "who-calls"
            | "impact"
            | "call-path"
            | "what-changed"
            | "list-areas"
            | "list-flows"
    );
    WorkflowEvidence {
        distinct_files: if reads_evidence {
            e.files.len() as u64
        } else {
            0
        },
        relationships: if reads_evidence {
            e.relationships.len() as u64
        } else {
            0
        },
        native_commands: commands,
        known_file_bytes: None,
        partial: e.partial,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A sink that refuses every write with one fixed error kind.
    struct Refusing(io::ErrorKind);

    impl Write for Refusing {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(self.0.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A sink that keeps what it is asked to print.
    #[derive(Default)]
    struct Recording(Vec<u8>);

    impl Write for Recording {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_reader_is_success_but_other_write_failures_survive() {
        let closed = write_absorbing_closed_reader(
            &mut Refusing(io::ErrorKind::BrokenPipe),
            format_args!("line\n"),
        );
        assert!(
            closed.is_ok(),
            "a reader that closed its end is not a failed write: {closed:?}"
        );

        let failed = write_absorbing_closed_reader(
            &mut Refusing(io::ErrorKind::WriteZero),
            format_args!("line\n"),
        );
        assert_eq!(
            failed
                .expect_err("a real write failure must not be absorbed")
                .kind(),
            io::ErrorKind::WriteZero
        );
    }

    #[test]
    fn successful_writes_reach_the_sink_unchanged() {
        let count = 7;
        let mut sink = Recording::default();
        write_absorbing_closed_reader(&mut sink, format_args!("counted {count}\n")).unwrap();
        assert_eq!(sink.0, b"counted 7\n");
    }

    #[test]
    fn metadata_is_distinct_bounded_and_never_extrapolated() {
        let mut e = Evidence {
            root: PathBuf::from("/fixture"),
            ..Evidence::default()
        };
        collect(
            &json!({"matches":[{"path":"src/a.rs"},{"path":"src/a.rs"}],
            "match_count": 800_000, "truncated":true,
            "callees":[{"path":"src/b.rs","uid":"b"}]}),
            &mut e,
            0,
        );
        assert_eq!(e.files.len(), 2);
        assert_eq!(e.relationships.len(), 1);
        assert!(e.partial);
        assert!(e.gap.is_none());
        assert_eq!(native_commands("commit"), Some(3));
        assert_eq!(native_commands("push"), Some(1));
        assert_eq!(native_commands("status"), None);
        assert_eq!(native_commands("task-state"), None);
    }

    #[test]
    fn only_a_positive_offset_marks_a_page_as_partial() {
        for (offset, partial) in [(0, false), (1, true), (3, true)] {
            let mut e = Evidence::default();
            collect(&json!({"offset": offset}), &mut e, 0);
            assert_eq!(e.partial, partial, "offset {offset}");
        }
        assert_eq!(native_commands("commit-and-push"), Some(4));
    }

    /// A scratch directory holding one file of `len` bytes, unique per test.
    fn scratch_file(name: &str, len: usize) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "pixel-operation-metrics-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.py");
        std::fs::write(&file, "x".repeat(len)).unwrap();
        (dir, file)
    }

    /// `list-signatures` stands in for reading the file it lists, so its
    /// baseline is that file's measured size; every other shape (another
    /// command, no file, two files, a directory, a missing path) keeps the
    /// policy estimate instead of inventing a size.
    #[test]
    fn only_a_whole_file_reader_naming_one_real_file_is_measured() {
        let (dir, file) = scratch_file("measured", 41);
        let one = HashSet::from([file.clone()]);
        assert_eq!(whole_file_bytes("list-signatures", &one), Some(41));
        assert_eq!(whole_file_bytes("find-code", &one), None);
        assert_eq!(whole_file_bytes("list-signatures", &HashSet::new()), None);
        let two = HashSet::from([file, dir.join("other.py")]);
        assert_eq!(whole_file_bytes("list-signatures", &two), None);
        assert_eq!(
            whole_file_bytes("list-signatures", &HashSet::from([dir.clone()])),
            None
        );
        assert_eq!(
            whole_file_bytes("list-signatures", &HashSet::from([dir.join("gone.py")])),
            None
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The measured baseline replaces the policy: no assumed native command
    /// and no relationship on top of the file, so the live line can state it
    /// as a full read. Another command keeps the policy fields unchanged.
    #[test]
    fn a_measured_read_carries_the_file_size_and_no_assumed_command() {
        let (dir, file) = scratch_file("evidence", 41);
        let e = Evidence {
            root: dir.clone(),
            files: HashSet::from([file]),
            relationships: HashSet::from(["callers:x".to_owned()]),
            partial: true,
            gap: None,
        };
        assert_eq!(
            workflow_evidence("list-signatures", 1, &e),
            WorkflowEvidence {
                distinct_files: 1,
                relationships: 0,
                native_commands: 0,
                known_file_bytes: Some(41),
                partial: true,
            }
        );
        assert_eq!(
            workflow_evidence("find-code", 1, &e),
            WorkflowEvidence {
                distinct_files: 1,
                relationships: 1,
                native_commands: 1,
                known_file_bytes: None,
                partial: true,
            }
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn nested_value(depth: usize) -> serde_json::Value {
        let mut value = json!({"path": "src/leaf.rs"});
        for _ in 0..depth {
            value = json!({"nested": value});
        }
        value
    }

    #[test]
    fn evidence_state_machine_reports_gaps_and_counts() {
        // One test owns the process-wide slot, so its first probe runs against
        // the initial `None` however the harness orders the suite.
        assert_eq!(
            evidence("search-content", true),
            Err(ComparisonGap::Uninitialized)
        );
        begin(Path::new("/fixture"));
        unavailable();
        assert_eq!(
            evidence("search-content", true),
            Err(ComparisonGap::OutputTruncated)
        );
        begin(Path::new("/fixture"));
        observe(&json!({"matches": [{"path": "src/a.rs"}], "truncated": false}));
        let ok = evidence("search-content", true).unwrap();
        assert_eq!(ok.distinct_files, 1);
        assert_eq!(ok.native_commands, 1);
        assert!(!ok.partial);
        assert_eq!(evidence("status", true), Err(ComparisonGap::NoPolicy));
        assert_eq!(
            evidence("search-content", false),
            Err(ComparisonGap::OperationFailed)
        );
        begin(Path::new("/fixture"));
        observe(&nested_value(48));
        let ok = evidence("find-code", true).unwrap();
        assert_eq!(ok.distinct_files, 1);
        begin(Path::new("/fixture"));
        observe(&nested_value(49));
        assert_eq!(
            evidence("find-code", true),
            Err(ComparisonGap::EvidenceDepthCapped)
        );
    }
}
