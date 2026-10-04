//! Change detection — `git diff --unified=0` hunk ranges mapped onto indexed
//! symbols, each judged against its file's base symbols, with affected
//! processes, the call sites that may be affected, and depth-1 upstream
//! callers feeding risk.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;

use serde::Serialize;

use crate::build::{Indexability, graph_file_cap, indexability};
use crate::concept::is_test_path;
use crate::extract::{extract_file, lang_of};
use crate::impact::{file_path_by_id, processes_for_symbol, symbol_by_id};
use crate::store::{EdgeKind, GraphStore, SymbolRow};
use pixel_git::GitRunner;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Cap on `suggested_tests` entries (T2: the cap must surface in the report).
const SUGGESTED_TESTS_CAP: usize = 100;

/// Max upstream BFS depth when walking callers looking for test files.
const SUGGESTED_TESTS_MAX_DEPTH: u8 = 3;

/// Cap on `uncovered_changes` entries. A freshly added file contributes
/// one entry per run of lines between its symbols, so a large import
/// block alone can produce dozens; the cap keeps the report bounded and
/// surfaces in `uncovered_lower_bound` (T2: every cap surfaces).
const UNCOVERED_CAP: usize = 200;

/// Cap on the files whose base content is fetched and re-extracted to
/// judge the diff's old side. Each one costs a `git show` and a parse;
/// beyond the cap the old side is reported as uncovered rather than
/// assumed anchored, which abstains instead of over-claiming.
const BASE_EXTRACTION_CAP: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct ChangedSymbol {
    pub uid: String,
    pub name: String,
    pub path: String,
    /// "modified" | "added" | "deleted"
    pub change: String,
    /// What `change` was judged from: "symbol" when the file's base side was
    /// re-extracted and this symbol looked up in it, "file" when only the
    /// file's status is known (an added or deleted file, or a base side past
    /// the read cap, unreadable or without a grammar).
    pub change_basis: String,
    /// Whether the signature differs from the base symbol's, whitespace
    /// aside. Absent when there is no base symbol to compare with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_changed: Option<bool>,
    pub processes: Vec<String>,
}

/// A call site that may be affected by a changed or deleted symbol. A
/// consumer is potentially affected, never proven broken: a body change can
/// leave every caller's contract intact, and a same-name site may call
/// something else.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Consumer {
    /// uid of the changed or deleted symbol the site depends on.
    pub of: String,
    /// Repo-relative path of the site.
    pub path: String,
    /// Line of the call site.
    pub line: u32,
    /// uid of the symbol enclosing the site; absent when none does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    /// "calls" for a resolved call edge to a symbol the graph holds,
    /// "unresolved_name" for an unresolved site whose name matches a symbol
    /// the change deleted.
    pub basis: String,
    /// Resolution tier of a "calls" edge; absent for an unresolved site.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
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

/// Why a changed range carries no symbolic anchor. Mirrors
/// `pixel_proto::evaluate::UncoveredMotif`, which is what a `diff-reaches`
/// evaluation prints: the vocabulary is fixed by the contract, and the
/// daemon maps one onto the other with an exhaustive match, so a motif
/// added here cannot reach the wire unnamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UncoveredMotif {
    /// Inside an indexed file, but between its symbols: a module constant,
    /// an import, a comment, an attribute.
    OutsideSymbol,
    /// No grammar for this extension.
    UnsupportedLanguage,
    /// An indexable file the build's file cap left out.
    ExcludedByFileCap,
    /// Over the build's per-file size cap.
    ExcludedBySize,
    /// Indexable but absent from the graph for another reason (added since
    /// the build, a generated blob, or a base side that was not examined).
    NotIndexed,
    /// A binary patch or a mode-only change: no text to map.
    NonTextChange,
}

/// A changed range with no symbolic anchor. `old_lines` and `new_lines` are
/// both optional and at most one is set per entry: a range exists either in
/// the base (a deletion) or in the working tree (an addition).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UncoveredChange {
    pub path: String,
    pub motif: UncoveredMotif,
    /// `[start, end]`, inclusive, in the base file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_lines: Option<[u32; 2]>,
    /// `[start, end]`, inclusive, in the working-tree file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_lines: Option<[u32; 2]>,
}

/// A symbol the change touched in the base state that the current graph has
/// no anchor for: a deleted function, or one whose file is gone. Reported
/// apart from `uncovered_changes` because the cause differs — the range was
/// mappable, the symbol it mapped to no longer exists — and because a
/// reachability answer about it cannot be repaired by indexing anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnanchoredSymbol {
    /// `path#qualified#kind`, built exactly as the build builds it, so a
    /// reader can confirm the absence with a uid lookup.
    pub uid: String,
    pub name: String,
    pub path: String,
    /// `[start, end]` of the symbol in the base file.
    pub old_lines: [u32; 2],
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangesReport {
    pub base: String,
    pub changed_files: u64,
    pub symbols: Vec<ChangedSymbol>,
    pub affected_processes: Vec<String>,
    /// Call sites that may be affected by the change, sorted by
    /// (`of`, `path`, `line`): the resolved callers of every changed symbol,
    /// and the unresolved same-name sites of every deleted one.
    pub consumers: Vec<Consumer>,
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
    /// Changed ranges this report maps to no symbol, with the motif that
    /// explains each one. Empty means every changed line sits inside an
    /// indexed symbol on both sides of the diff — the only case in which
    /// a consumer may speak about "the whole change".
    pub uncovered_changes: Vec<UncoveredChange>,
    /// Symbols the change touched in the base state with no anchor in the
    /// current graph (deleted symbols, and symbols of deleted files).
    pub unanchored: Vec<UnanchoredSymbol>,
    /// True when a cap truncated `uncovered_changes`, or when the old side
    /// of some file was not examined: more uncovered ranges may exist.
    pub uncovered_lower_bound: bool,
    /// Which cap or blind spot `uncovered_lower_bound` stands for.
    pub uncovered_note: String,
}

#[derive(Debug, PartialEq)]
pub(crate) enum FileStatus {
    Added,
    Deleted,
    Modified,
}

/// One changed file's mapping. Field visibility is `pub(crate)` because the
/// `review` pass re-reads the same parsed diff for its own two walks
/// (added-line secret scan, caller containment); it changes nothing here.
#[derive(Debug)]
pub(crate) struct FileDiff {
    pub(crate) path: String,
    /// Where the file's base content lives, which a rename moves: the old
    /// side of the diff is read from this path, not from `path`.
    old_path: String,
    pub(crate) status: FileStatus,
    /// Changed line ranges in the NEW file's coordinates (inclusive),
    /// including the one-line anchor a pure deletion leaves behind so an
    /// adjacent symbol is still reported as changed.
    new_ranges: Vec<(u32, u32)>,
    /// The subset of `new_ranges` that genuinely holds added lines. Coverage
    /// of the new side is computed from these, so a deletion's anchor —
    /// which points at a line the hunk did not write — never turns into an
    /// uncovered addition.
    pub(crate) added_ranges: Vec<(u32, u32)>,
    /// Removed line ranges in the OLD file's coordinates (inclusive). Empty
    /// for a pure addition.
    old_ranges: Vec<(u32, u32)>,
    /// False when git described the change without a text hunk: a binary
    /// patch, or a mode-only change. Both are `non_text_change`.
    pub(crate) text: bool,
}

impl FileDiff {
    /// A change git reported with no `---`/`+++` pair: nothing to map.
    fn non_text(path: String) -> Self {
        FileDiff {
            old_path: path.clone(),
            path,
            status: FileStatus::Modified,
            new_ranges: Vec::new(),
            added_ranges: Vec::new(),
            old_ranges: Vec::new(),
            text: false,
        }
    }
}

/// The path a `diff --git a/<p> b/<p>` header names, taken from the `b`
/// side so a rename resolves to its destination.
///
/// The header is ambiguous when the path itself contains ` b/`, and neither
/// the first nor the last separator is right in every case. Prefer the split
/// whose two sides name the same path — which every change but a rename has
/// — and fall back to the last one, where the sides genuinely differ. `None`
/// for a header in another shape (`--no-prefix`, or a path git quoted),
/// where guessing would name a file the diff never mentioned.
fn header_path(rest: &str) -> Option<String> {
    let usable = |p: &str| (!p.is_empty() && !p.starts_with('"')).then(|| p.to_string());
    let mut last: Option<&str> = None;
    for (at, _) in rest.match_indices(" b/") {
        let new = &rest[at + 3..];
        if rest[..at].strip_prefix("a/") == Some(new) {
            return usable(new);
        }
        last = Some(new);
    }
    last.and_then(usable)
}

/// Parse one `@@` hunk-header side (`-a,b` or `+c,d`) into its inclusive
/// range, or `None` when that side of the hunk holds no line: a pure
/// addition has `-a,0` and a pure deletion `+c,0`.
fn hunk_range(spec: &str) -> Option<(u32, u32)> {
    let mut it = spec.splitn(2, ',');
    let start: u32 = it.next().and_then(|s| s.parse().ok())?;
    let len: u32 = it.next().map_or(Some(1), |s| s.parse().ok())?;
    if start == 0 || len == 0 {
        return None;
    }
    Some((start, start + len - 1))
}

/// Parse `git diff --unified=0` output into per-file changed ranges, on
/// both sides of the diff.
///
/// A `diff --git` header that never produces a `---`/`+++` pair described a
/// change with no text hunk — a binary patch, or a mode-only change — and is
/// kept as a `text: false` entry. Without it such a file is invisible to
/// change detection, which would let a report claim to cover a change it
/// never saw.
pub(crate) fn parse_diff(output: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut old_path: Option<String> = None;
    // The header being read, and whether it has produced a text hunk yet.
    let mut header: Option<String> = None;
    let mut header_has_text = false;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(path) = header.take().filter(|_| !header_has_text) {
                files.push(FileDiff::non_text(path));
            }
            header = header_path(rest);
            header_has_text = false;
        } else if let Some(rest) = line.strip_prefix("--- ") {
            old_path = Some(rest.trim().trim_start_matches("a/").to_string());
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            header_has_text = true;
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
            // A rename leaves the base content under the old path; anything
            // else reads its own path back.
            let old = old_path
                .clone()
                .filter(|p| p != "/dev/null")
                .unwrap_or_else(|| path.clone());
            files.push(FileDiff {
                path,
                old_path: old,
                status,
                new_ranges: Vec::new(),
                added_ranges: Vec::new(),
                old_ranges: Vec::new(),
                text: true,
            });
        } else if line.starts_with("@@") {
            // @@ -a[,b] +c[,d] @@
            if let Some(cur) = files.last_mut() {
                let mut sides = line.split(' ').skip(1);
                if let Some(minus) = sides.next().and_then(|t| t.strip_prefix('-'))
                    && let Some(range) = hunk_range(minus)
                {
                    cur.old_ranges.push(range);
                }
                if let Some(plus) = line.split(' ').find(|t| t.starts_with('+')) {
                    match hunk_range(&plus[1..]) {
                        Some(range) => {
                            cur.new_ranges.push(range);
                            cur.added_ranges.push(range);
                        }
                        None => {
                            // pure deletion: mark the position after which
                            // lines were removed so adjacent symbols are
                            // caught. Not an added range: nothing was
                            // written at that line.
                            let start: u32 = plus[1..]
                                .split(',')
                                .next()
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0);
                            let anchor = start.max(1);
                            cur.new_ranges.push((anchor, anchor));
                        }
                    }
                }
            }
        }
    }
    if let Some(path) = header.take().filter(|_| !header_has_text) {
        files.push(FileDiff::non_text(path));
    }
    files
}

/// The parts of `[start, end]` that no span covers, in order. `spans` may
/// overlap and need not be sorted. An empty result means the range is fully
/// covered; the whole range comes back when nothing covers it.
fn residues(start: u32, end: u32, spans: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut covering: Vec<(u32, u32)> = spans
        .iter()
        .copied()
        .filter(|&(a, b)| a <= end && start <= b)
        .collect();
    covering.sort_unstable();
    let mut out = Vec::new();
    let mut cursor = start;
    for (a, b) in covering {
        if a > cursor {
            out.push((cursor, a - 1));
        }
        cursor = cursor.max(b.saturating_add(1));
        if cursor > end {
            return out;
        }
    }
    out.push((cursor, end));
    out
}

/// The span a set of ranges covers, `None` when there is none.
fn span_of(ranges: &[(u32, u32)]) -> Option<[u32; 2]> {
    let lo = ranges.iter().map(|&(a, _)| a).min()?;
    let hi = ranges.iter().map(|&(_, b)| b).max()?;
    Some([lo, hi])
}

/// Whether a graph holding `files` files stopped at `cap` rather than at
/// the end of the tree. Conservative: a graph that exactly fills the cap is
/// reported as capped, because the walk cannot tell "the last file" from
/// "the first file it refused". Over-reporting widens the stated limits of
/// an absence; under-reporting would narrow them.
fn cap_reached(files: u64, cap: Option<usize>) -> bool {
    cap.is_some_and(|c| files >= c as u64)
}

/// Why a changed file the graph does not hold carries no symbol. Asks the
/// build's own policy (`indexability`) so the answer cannot drift from what
/// the build would actually do with the file.
fn absent_motif(root: &Path, rel: &str, file_cap_hit: bool) -> UncoveredMotif {
    match indexability(root, rel) {
        Indexability::UnsupportedLanguage => UncoveredMotif::UnsupportedLanguage,
        Indexability::TooLarge => UncoveredMotif::ExcludedBySize,
        Indexability::Binary => UncoveredMotif::NonTextChange,
        Indexability::Generated | Indexability::Absent => UncoveredMotif::NotIndexed,
        // Indexable and still absent: the cap is the only cause the build
        // records, and only when it was reached.
        Indexability::Indexable if file_cap_hit => UncoveredMotif::ExcludedByFileCap,
        Indexability::Indexable => UncoveredMotif::NotIndexed,
    }
}

/// The base-side content of `rel`, read from the same side the diff used:
/// a commit when `--base` named one, the index otherwise (a plain
/// `git diff` compares the working tree against the index).
fn base_blob(runner: &GitRunner, base_ref: Option<&str>, rel: &str) -> Option<Vec<u8>> {
    match base_ref {
        Some(r) => runner.show_blob(r, rel),
        None => runner.show_index_blob(rel),
    }
}

/// Cut `changes` down to `cap`, returning how many there were when it had
/// to. `None` means the list is whole — and a list of exactly `cap` entries
/// is whole, since nothing was dropped to reach that length. Separated from
/// the scan so the boundary is stated once and can be read on its own: it
/// decides whether the report calls itself a lower bound.
fn truncate_to_cap(changes: &mut Vec<UncoveredChange>, cap: usize) -> Option<usize> {
    let total = changes.len();
    (total > cap).then(|| {
        changes.truncate(cap);
        total
    })
}

/// What the change touches that no symbol covers.
struct Uncovered {
    changes: Vec<UncoveredChange>,
    unanchored: Vec<UnanchoredSymbol>,
    lower_bound: bool,
    note: String,
}

/// Map every changed range onto the symbols that cover it, on both sides of
/// the diff, and report what is left over.
///
/// The new side is compared against the graph, whose line ranges are
/// working-tree coordinates. The old side cannot be: the graph never held
/// the base state. It is re-extracted from the base blob instead, which is
/// also the only way to name a symbol the change deleted.
fn scan_uncovered(
    store: &GraphStore,
    root: &Path,
    runner: &GitRunner,
    base_ref: Option<&str>,
    file_diffs: &[FileDiff],
) -> Result<Uncovered, BoxError> {
    let mut changes: Vec<UncoveredChange> = Vec::new();
    let mut unanchored: Vec<UnanchoredSymbol> = Vec::new();
    let mut seen_uids: BTreeSet<String> = BTreeSet::new();
    let mut notes: Vec<String> = Vec::new();
    let mut base_reads = 0usize;
    let mut base_capped = 0usize;
    let cap_hit = cap_reached(store.counts()?.0, graph_file_cap());

    for fd in file_diffs {
        if !fd.text {
            changes.push(UncoveredChange {
                path: fd.path.clone(),
                motif: UncoveredMotif::NonTextChange,
                old_lines: None,
                new_lines: None,
            });
            continue;
        }

        // --- new side: the graph's own coordinates.
        if fd.status != FileStatus::Deleted {
            match store.file_by_path(&fd.path)? {
                Some(file) => {
                    let spans: Vec<(u32, u32)> = store
                        .symbols_in_file(file.id)?
                        .iter()
                        .map(|s| (s.start_line, s.end_line))
                        .collect();
                    for &(start, end) in &fd.added_ranges {
                        for (a, b) in residues(start, end, &spans) {
                            changes.push(UncoveredChange {
                                path: fd.path.clone(),
                                motif: UncoveredMotif::OutsideSymbol,
                                old_lines: None,
                                new_lines: Some([a, b]),
                            });
                        }
                    }
                }
                None => {
                    if let Some(span) = span_of(&fd.added_ranges) {
                        changes.push(UncoveredChange {
                            path: fd.path.clone(),
                            motif: absent_motif(root, &fd.path, cap_hit),
                            old_lines: None,
                            new_lines: Some(span),
                        });
                    }
                }
            }
        }

        // --- old side: re-extracted from the base blob.
        let Some(old_span) = span_of(&fd.old_ranges) else {
            continue;
        };
        let unexamined = |changes: &mut Vec<UncoveredChange>, motif| {
            changes.push(UncoveredChange {
                path: fd.old_path.clone(),
                motif,
                old_lines: Some(old_span),
                new_lines: None,
            });
        };
        if base_reads >= BASE_EXTRACTION_CAP {
            base_capped += 1;
            unexamined(&mut changes, UncoveredMotif::NotIndexed);
            continue;
        }
        base_reads += 1;
        let Some(content) = base_blob(runner, base_ref, &fd.old_path) else {
            unexamined(&mut changes, UncoveredMotif::NotIndexed);
            continue;
        };
        let Some(extraction) = extract_file(&fd.old_path, &content) else {
            let motif = if lang_of(&fd.old_path).is_none() {
                UncoveredMotif::UnsupportedLanguage
            } else {
                UncoveredMotif::NotIndexed
            };
            unexamined(&mut changes, motif);
            continue;
        };
        for &(start, end) in &fd.old_ranges {
            // Same overlap rule the new side uses, so a range touching a
            // symbol's first or last line anchors on it either way.
            let hits: Vec<&crate::extract::RawSymbol> = extraction
                .symbols
                .iter()
                .filter(|s| overlaps(&[(s.start_line, s.end_line)], start, end))
                .collect();
            if hits.is_empty() {
                changes.push(UncoveredChange {
                    path: fd.old_path.clone(),
                    motif: UncoveredMotif::OutsideSymbol,
                    old_lines: Some([start, end]),
                    new_lines: None,
                });
                continue;
            }
            for sym in hits {
                let uid = format!("{}#{}#{}", fd.old_path, sym.qualified, sym.kind.as_str());
                // A deleted file's symbols are unanchored whatever the graph
                // still holds: a graph built before the deletion answers for
                // a file that is gone.
                let gone = fd.status == FileStatus::Deleted || store.symbol_by_uid(&uid)?.is_none();
                if gone && seen_uids.insert(uid.clone()) {
                    unanchored.push(UnanchoredSymbol {
                        uid,
                        name: sym.name.clone(),
                        path: fd.old_path.clone(),
                        old_lines: [sym.start_line, sym.end_line],
                    });
                }
            }
        }
    }

    let truncated = truncate_to_cap(&mut changes, UNCOVERED_CAP);
    if let Some(total) = truncated {
        notes.push(format!(
            "lower bound: {total} uncovered ranges found, truncated to {UNCOVERED_CAP}"
        ));
    }
    if base_capped > 0 {
        notes.push(format!(
            "lower bound: the base side of {base_capped} file(s) was not examined \
             (cap {BASE_EXTRACTION_CAP}); their removed ranges are reported as not_indexed"
        ));
    }
    Ok(Uncovered {
        lower_bound: truncated.is_some() || base_capped > 0,
        note: notes.join("; "),
        changes,
        unanchored,
    })
}

fn overlaps(ranges: &[(u32, u32)], start: u32, end: u32) -> bool {
    ranges.iter().any(|&(a, b)| a <= end && start <= b)
}

/// `qualified#kind`: a symbol's identity within its file, which a rename
/// of the file keeps (the uid's path part does not).
fn symbol_key(qualified: &str, kind: &str) -> String {
    format!("{qualified}#{kind}")
}

/// The base side's symbols of `old_path`, keyed by [`symbol_key`] to their
/// signature; `None` when the blob cannot be read or parsed.
fn base_signatures(
    runner: &GitRunner,
    base_ref: Option<&str>,
    old_path: &str,
) -> Option<HashMap<String, String>> {
    let content = base_blob(runner, base_ref, old_path)?;
    let extraction = extract_file(old_path, &content)?;
    Some(
        extraction
            .symbols
            .into_iter()
            .map(|s| (symbol_key(&s.qualified, s.kind.as_str()), s.sig))
            .collect(),
    )
}

/// How a symbol the diff touched changed: `(change, change_basis,
/// signature_changed)`. A modified file's symbols are judged against its
/// base symbols when they are known; otherwise the file status stands in.
fn classify(
    status: &FileStatus,
    base: Option<&HashMap<String, String>>,
    key: &str,
    sig: &str,
) -> (&'static str, &'static str, Option<bool>) {
    let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    match (status, base) {
        (FileStatus::Added, _) => ("added", "file", None),
        (FileStatus::Deleted, _) => ("deleted", "file", None),
        (FileStatus::Modified, None) => ("modified", "file", None),
        (FileStatus::Modified, Some(base)) => match base.get(key) {
            None => ("added", "symbol", None),
            Some(old) => ("modified", "symbol", Some(words(old) != words(sig))),
        },
    }
}

/// The innermost symbol whose span holds `line`, the first on a tie.
fn innermost(symbols: &[SymbolRow], line: u32) -> Option<&SymbolRow> {
    symbols
        .iter()
        .filter(|s| s.start_line <= line && line <= s.end_line)
        .min_by_key(|s| s.end_line - s.start_line)
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
    pixel_git::validate_ref(r)
        .map_err(|e| -> BoxError { format!("invalid base ref {r:?}: {e}").into() })?;
    Ok(r)
}

pub fn detect(
    store: &GraphStore,
    root: &Path,
    base_ref: Option<&str>,
    include_tests: bool,
) -> Result<ChangesReport, BoxError> {
    let runner = GitRunner::new(root);
    let diff_bytes = match runner.diff_unified0(base_ref) {
        Ok(bytes) => bytes,
        Err(_) => {
            return Ok(ChangesReport {
                base: base_ref.unwrap_or("index").to_string(),
                changed_files: 0,
                symbols: Vec::new(),
                affected_processes: Vec::new(),
                consumers: Vec::new(),
                risk: "LOW".to_string(),
                envelope_note: "clean or non-git tree; no changes detected".to_string(),
                suggested_tests: Vec::new(),
                suggested_tests_lower_bound: false,
                suggested_tests_note: String::new(),
                uncovered_changes: Vec::new(),
                unanchored: Vec::new(),
                uncovered_lower_bound: false,
                uncovered_note: String::new(),
            });
        }
    };
    let diff = String::from_utf8_lossy(&diff_bytes).into_owned();
    let file_diffs = parse_diff(&diff);
    detect_diffs(store, root, base_ref, &file_diffs, include_tests)
}

/// Whether the working-tree diff removed a call site at `line` (old-file
/// coordinates) in `path`. An indexed `calls` edge is stale once its site no
/// longer exists; counting it after the edit deleted both a symbol and its
/// only call would re-raise a finished deletion to CONCERN on nothing but
/// stale index state.
fn call_site_removed(file_diffs: &[FileDiff], path: &str, line: u32) -> bool {
    // A deleted file's diff reports its whole old side as `old_ranges`, so
    // this covers a caller file removed outright and a modified file whose
    // call line the hunk actually wrote over.
    file_diffs
        .iter()
        .any(|fd| fd.old_path == path && fd.old_ranges.iter().any(|&(s, e)| line >= s && line <= e))
}

/// `detect` over an already-parsed diff: a caller that read the diff itself
/// (`review`) shares the same change set instead of running a second
/// `git diff` that could observe a newer tree. Without `--base` the diff is
/// `git diff`: the working tree against the index, so staged edits are not
/// part of it.
pub(crate) fn detect_diffs(
    store: &GraphStore,
    root: &Path,
    base_ref: Option<&str>,
    file_diffs: &[FileDiff],
    include_tests: bool,
) -> Result<ChangesReport, BoxError> {
    let base = base_ref.unwrap_or("index").to_string();
    let runner = GitRunner::new(root);

    let mut symbols: Vec<ChangedSymbol> = Vec::new();
    let mut proc_set: BTreeSet<String> = BTreeSet::new();
    let mut caller_ids: BTreeSet<i64> = BTreeSet::new();
    let mut lower_bound_names: BTreeSet<String> = BTreeSet::new();
    // (symbol rowid, name, path) of every affected symbol — the seeds for
    // the upstream test-file walk when `include_tests` is set.
    let mut changed_seeds: Vec<(i64, String, String)> = Vec::new();
    let mut consumers: BTreeSet<Consumer> = BTreeSet::new();
    let mut base_reads = 0usize;

    for fd in file_diffs {
        let file = match store.file_by_path(&fd.path)? {
            Some(f) => f,
            None => continue, // not indexed (e.g. new file before re-index)
        };
        let in_file = store.symbols_in_file(file.id)?;
        // Same cap as the uncovered scan: past it a symbol is judged by its
        // file status, which `change_basis: "file"` says.
        let base = if fd.status == FileStatus::Modified && base_reads < BASE_EXTRACTION_CAP {
            base_reads += 1;
            base_signatures(&runner, base_ref, &fd.old_path)
        } else {
            None
        };
        for sym in in_file {
            let hit = match fd.status {
                FileStatus::Deleted => true,
                _ => overlaps(&fd.new_ranges, sym.start_line, sym.end_line),
            };
            if !hit {
                continue;
            }
            let (change, change_basis, signature_changed) = classify(
                &fd.status,
                base.as_ref(),
                &symbol_key(&sym.qualified, sym.kind.as_str()),
                &sym.sig,
            );
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
                if let Some(caller) = symbol_by_id(store, e.src_id)? {
                    let caller_path = file_path_by_id(store, caller.file_id)?;
                    // A `calls` edge whose site the diff removed is stale:
                    // the reference no longer exists in the working tree,
                    // so it must not count as a live consumer (the deleted
                    // symbol's HIGH would then be passable only with
                    // `--no-verify`).
                    if !call_site_removed(file_diffs, &caller_path, e.site_line) {
                        consumers.insert(Consumer {
                            of: sym.uid.clone(),
                            path: caller_path,
                            line: e.site_line,
                            caller: Some(caller.uid),
                            basis: "calls".to_string(),
                            tier: Some(e.tier.as_str().to_string()),
                        });
                    }
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
                change: change.to_string(),
                change_basis: change_basis.to_string(),
                signature_changed,
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

    let uncovered = scan_uncovered(store, root, &runner, base_ref, file_diffs)?;
    // A deleted symbol has no edges left; its former callers are the
    // unresolved sites that still write its name.
    let mut file_symbols: HashMap<i64, Vec<SymbolRow>> = HashMap::new();
    for gone in &uncovered.unanchored {
        for site in store.unresolved_named(&gone.name)? {
            let symbols = match file_symbols.entry(site.file_id) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => entry.insert(store.symbols_in_file(site.file_id)?),
            };
            let caller = innermost(symbols, site.site_line);
            consumers.insert(Consumer {
                of: gone.uid.clone(),
                path: file_path_by_id(store, site.file_id)?,
                line: site.site_line,
                caller: caller.map(|s| s.uid.clone()),
                basis: "unresolved_name".to_string(),
                tier: None,
            });
        }
    }

    Ok(ChangesReport {
        base,
        changed_files: file_diffs.len() as u64,
        symbols,
        affected_processes,
        consumers: consumers.into_iter().collect(),
        risk,
        envelope_note,
        suggested_tests,
        suggested_tests_lower_bound,
        suggested_tests_note,
        uncovered_changes: uncovered.changes,
        unanchored: uncovered.unanchored,
        uncovered_lower_bound: uncovered.lower_bound,
        uncovered_note: uncovered.note,
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
    let record = |file: String,
                  depth: u8,
                  symbol: &str,
                  map: &mut BTreeMap<String, (u8, BTreeSet<String>)>| {
        let entry = map.entry(file).or_insert_with(|| (depth, BTreeSet::new()));
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

    /// A diff is a protocol fixture: built line by line so an escape or an
    /// indentation slip cannot silently parse as something else.
    fn diff(lines: &[&str]) -> String {
        lines.join("\n")
    }

    #[test]
    fn header_path_reads_the_destination_side() {
        assert_eq!(
            header_path("a/src/a.ts b/src/a.ts").as_deref(),
            Some("src/a.ts")
        );
        // A rename: the destination is what the working tree holds.
        assert_eq!(
            header_path("a/src/old.ts b/src/new.ts").as_deref(),
            Some("src/new.ts")
        );
        // A path that contains " b/" itself: the LAST occurrence separates.
        assert_eq!(
            header_path("a/x b/y.ts b/x b/y.ts").as_deref(),
            Some("x b/y.ts")
        );
        // Shapes this parser refuses rather than guessing at.
        assert_eq!(header_path("src/a.ts src/a.ts"), None);
        assert_eq!(header_path("a/x b/\"quoted path\""), None);
        assert_eq!(header_path(""), None);
    }

    #[test]
    fn hunk_range_is_inclusive_and_empty_sides_are_none() {
        assert_eq!(hunk_range("12"), Some((12, 12)));
        assert_eq!(hunk_range("12,1"), Some((12, 12)));
        assert_eq!(hunk_range("12,3"), Some((12, 14)));
        // An empty side: a pure addition's `-a,0`, a pure deletion's `+c,0`.
        assert_eq!(hunk_range("12,0"), None);
        assert_eq!(hunk_range("0,0"), None);
        assert_eq!(hunk_range("0"), None);
        assert_eq!(hunk_range("x,1"), None);
        assert_eq!(hunk_range("1,x"), None);
    }

    #[test]
    fn residues_are_the_lines_no_span_covers() {
        // Fully covered: nothing is left over. This is the case a covered
        // change depends on — a residue here would abstain on every edit.
        assert_eq!(residues(5, 7, &[(1, 10)]), vec![]);
        assert_eq!(residues(5, 7, &[(5, 7)]), vec![]);
        // Nothing covers it.
        assert_eq!(residues(5, 7, &[]), vec![(5, 7)]);
        assert_eq!(residues(5, 7, &[(8, 9), (1, 4)]), vec![(5, 7)]);
        // A span far past the range must not stretch the residue to reach
        // it: a symbol elsewhere in the file says nothing about these
        // lines, and a range that grew beyond `end` would name lines the
        // change never touched.
        assert_eq!(residues(5, 7, &[(20, 21)]), vec![(5, 7)]);
        assert_eq!(residues(5, 7, &[(1, 2), (20, 21)]), vec![(5, 7)]);
        // Partial: before, after, and a hole between two spans.
        assert_eq!(residues(1, 10, &[(4, 10)]), vec![(1, 3)]);
        assert_eq!(residues(1, 10, &[(1, 6)]), vec![(7, 10)]);
        assert_eq!(residues(1, 10, &[(1, 3), (8, 12)]), vec![(4, 7)]);
        // Overlapping and unsorted spans collapse; the hole they leave is
        // what comes back.
        assert_eq!(residues(1, 10, &[(6, 12), (1, 4)]), vec![(5, 5)]);
        assert_eq!(residues(1, 10, &[(6, 12), (1, 4), (3, 7)]), vec![]);
        // A single-line range against an adjacent symbol.
        assert_eq!(residues(4, 4, &[(5, 9)]), vec![(4, 4)]);
        assert_eq!(residues(4, 4, &[(4, 9)]), vec![]);
        // The last line of the range, left over by a span that stops one
        // line short: the sweep must not treat "reached the end" as "done".
        assert_eq!(residues(5, 7, &[(5, 6)]), vec![(7, 7)]);
    }

    /// The cap decides whether the report calls itself a lower bound, so
    /// the boundary matters: a list of exactly `cap` entries lost nothing
    /// and must not be announced as truncated.
    #[test]
    fn truncate_to_cap_only_reports_what_it_dropped() {
        let entry = |line: u32| UncoveredChange {
            path: "src/a.ts".to_string(),
            motif: UncoveredMotif::OutsideSymbol,
            old_lines: None,
            new_lines: Some([line, line]),
        };
        let mut none: Vec<UncoveredChange> = Vec::new();
        assert_eq!(truncate_to_cap(&mut none, 2), None);
        assert!(none.is_empty());

        let mut under = vec![entry(1)];
        assert_eq!(truncate_to_cap(&mut under, 2), None);
        assert_eq!(under.len(), 1);

        let mut exactly = vec![entry(1), entry(2)];
        assert_eq!(
            truncate_to_cap(&mut exactly, 2),
            None,
            "cap entries is whole"
        );
        assert_eq!(exactly.len(), 2);

        let mut over = vec![entry(1), entry(2), entry(3)];
        assert_eq!(truncate_to_cap(&mut over, 2), Some(3));
        assert_eq!(over.len(), 2);
        assert_eq!(
            over[0].new_lines,
            Some([1, 1]),
            "the first entries are kept"
        );
    }

    #[test]
    fn span_of_covers_every_range() {
        assert_eq!(span_of(&[]), None);
        assert_eq!(span_of(&[(3, 4)]), Some([3, 4]));
        assert_eq!(span_of(&[(9, 11), (3, 4), (6, 6)]), Some([3, 11]));
    }

    #[test]
    fn parse_diff_reads_both_sides_of_every_status() {
        let text = diff(&[
            "diff --git a/src/mod.ts b/src/mod.ts",
            "index 1111111..2222222 100644",
            "--- a/src/mod.ts",
            "+++ b/src/mod.ts",
            "@@ -10,2 +10,3 @@",
            "-old one",
            "-old two",
            "+new one",
            "+new two",
            "+new three",
            "@@ -40,3 +41,0 @@",
            "-gone one",
            "-gone two",
            "-gone three",
            "diff --git a/src/added.ts b/src/added.ts",
            "new file mode 100644",
            "--- /dev/null",
            "+++ b/src/added.ts",
            "@@ -0,0 +1,4 @@",
            "+a",
            "diff --git a/src/gone.ts b/src/gone.ts",
            "deleted file mode 100644",
            "--- a/src/gone.ts",
            "+++ /dev/null",
            "@@ -1,7 +0,0 @@",
            "-x",
            "",
        ]);
        let files = parse_diff(&text);
        assert_eq!(files.len(), 3, "{files:?}");

        let m = &files[0];
        assert_eq!(m.path, "src/mod.ts");
        assert_eq!(m.status, FileStatus::Modified);
        assert!(m.text);
        assert_eq!(m.old_ranges, vec![(10, 11), (40, 42)]);
        // The deletion's anchor is a new range (an adjacent symbol counts as
        // changed) but never an added one (nothing was written there).
        assert_eq!(m.new_ranges, vec![(10, 12), (41, 41)]);
        assert_eq!(m.added_ranges, vec![(10, 12)]);

        let a = &files[1];
        assert_eq!(a.path, "src/added.ts");
        assert_eq!(a.status, FileStatus::Added);
        assert_eq!(a.old_ranges, vec![]);
        assert_eq!(a.added_ranges, vec![(1, 4)]);

        let d = &files[2];
        assert_eq!(d.path, "src/gone.ts");
        assert_eq!(d.status, FileStatus::Deleted);
        assert_eq!(d.old_ranges, vec![(1, 7)]);
        assert_eq!(d.added_ranges, vec![]);
    }

    #[test]
    fn parse_diff_keeps_changes_that_carry_no_hunk() {
        // A binary patch and a mode-only change: git prints no `---`/`+++`
        // pair for either, so without the header they would be invisible and
        // the report would claim to cover a change it never read.
        let text = diff(&[
            "diff --git a/assets/logo.png b/assets/logo.png",
            "index 1111111..2222222 100644",
            "Binary files a/assets/logo.png and b/assets/logo.png differ",
            "diff --git a/scripts/run.sh b/scripts/run.sh",
            "old mode 100644",
            "new mode 100755",
            "diff --git a/src/mod.ts b/src/mod.ts",
            "--- a/src/mod.ts",
            "+++ b/src/mod.ts",
            "@@ -1 +1 @@",
            "-a",
            "+b",
            "",
        ]);
        let files = parse_diff(&text);
        assert_eq!(files.len(), 3, "{files:?}");
        let non_text: Vec<&str> = files
            .iter()
            .filter(|f| !f.text)
            .map(|f| f.path.as_str())
            .collect();
        assert_eq!(non_text, vec!["assets/logo.png", "scripts/run.sh"]);
        assert!(files.iter().any(|f| f.path == "src/mod.ts" && f.text));
    }

    #[test]
    fn parse_diff_reads_the_base_side_of_a_rename_from_the_old_path() {
        let text = diff(&[
            "diff --git a/src/old.ts b/src/new.ts",
            "similarity index 80%",
            "rename from src/old.ts",
            "rename to src/new.ts",
            "--- a/src/old.ts",
            "+++ b/src/new.ts",
            "@@ -3,1 +3,1 @@",
            "-a",
            "+b",
            "",
        ]);
        let files = parse_diff(&text);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/new.ts");
        // The base blob lives under the old path: reading `path` there would
        // miss the file and report the whole removal as unexamined.
        assert_eq!(files[0].old_path, "src/old.ts");
    }

    #[test]
    fn parse_diff_gives_an_added_file_its_own_path_as_base() {
        let text = diff(&[
            "diff --git a/src/added.ts b/src/added.ts",
            "--- /dev/null",
            "+++ b/src/added.ts",
            "@@ -0,0 +1,2 @@",
            "+a",
            "",
        ]);
        let files = parse_diff(&text);
        assert_eq!(files[0].old_path, "src/added.ts");
    }

    /// Both sides of the diff anchor a range on a symbol through this
    /// predicate, so its boundaries are the boundaries of the whole
    /// mapping: a deletion that removes exactly a function's closing line
    /// belongs to that function, and one line past it belongs to nobody.
    #[test]
    fn overlaps_includes_both_end_lines_and_stops_there() {
        let sym = [(6u32, 8u32)];
        assert!(overlaps(&sym, 6, 6), "the first line is inside");
        assert!(overlaps(&sym, 8, 8), "the last line is inside");
        assert!(overlaps(&sym, 7, 7));
        assert!(overlaps(&sym, 1, 6), "a range ending on the first line");
        assert!(overlaps(&sym, 8, 99), "a range starting on the last line");
        assert!(overlaps(&sym, 1, 99), "a range swallowing the symbol");
        assert!(!overlaps(&sym, 5, 5), "one line before");
        assert!(!overlaps(&sym, 9, 9), "one line after");
        assert!(!overlaps(&sym, 1, 5));
        assert!(!overlaps(&sym, 9, 99));
        assert!(!overlaps(&[], 6, 8), "no symbol, no anchor");
    }

    /// `call_site_removed` is true only when a matching file's removed span
    /// actually covers the line: a path-only match is nothing (another
    /// file's removed lines do not delete this call), and a line floated
    /// near but outside a range is not removed. Both disjuncts are asserted
    /// so a relaxed `&&` (widened to `||`) fails the contract.
    #[test]
    fn call_site_removed_needs_matching_path_and_covered_line() {
        let removed = |path: &str, old_ranges: Vec<(u32, u32)>| FileDiff {
            path: path.to_string(),
            old_path: path.to_string(),
            status: FileStatus::Modified,
            new_ranges: Vec::new(),
            added_ranges: Vec::new(),
            old_ranges,
            text: true,
        };
        let diffs = vec![removed("src/b.ts", vec![(10, 12)])];
        // Inside the matching file's removed range: removed, bounds inclusive.
        assert!(call_site_removed(&diffs, "src/b.ts", 11));
        assert!(call_site_removed(&diffs, "src/b.ts", 10), "first line");
        assert!(call_site_removed(&diffs, "src/b.ts", 12), "last line");
        // The path must match a diff that removed the line: this file's call
        // site is untouched even though another file lost those lines.
        assert!(!call_site_removed(&diffs, "src/a.ts", 11));
        // The line must fall inside the range, not merely be comparable to
        // one of its bounds.
        assert!(!call_site_removed(&diffs, "src/b.ts", 9), "one before");
        assert!(!call_site_removed(&diffs, "src/b.ts", 13), "one after");
        // No removed lines reported for a diff that deleted nothing.
        assert!(!call_site_removed(&[removed("src/b.ts", vec![])], "src/b.ts", 11));
    }

    /// The file cap is reported hit when the graph exactly fills it: the
    /// walk stops at the cap, so a full graph cannot be told apart from a
    /// truncated one, and claiming completeness there would be a lie.
    #[test]
    fn cap_reached_only_when_a_cap_applies() {
        assert!(!cap_reached(50_000, None), "no cap, nothing to hit");
        assert!(!cap_reached(u64::MAX, None));
        assert!(!cap_reached(9, Some(10)));
        assert!(
            cap_reached(10, Some(10)),
            "exactly at the cap counts as hit"
        );
        assert!(cap_reached(11, Some(10)));
        assert!(cap_reached(0, Some(0)));
    }

    /// The motif of a changed file the graph does not hold. Each one sends
    /// a different message to a reader — "index it", "nothing to index",
    /// "raise the cap" — so a wrong one misdirects the fix.
    #[test]
    fn absent_motif_names_the_cause_the_build_would_give() {
        let root = std::env::temp_dir().join(format!(
            "pixel-motif-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("ok.rs"), b"fn alpha() -> u32 { 1 }\n").unwrap();
        std::fs::write(root.join("notes.txt"), b"fn looks_like_rust() {}\n").unwrap();
        std::fs::write(root.join("blob.rs"), b"fn x() {}\n\0\0binary\n").unwrap();
        let oversize = usize::try_from(4 * 1024 * 1024u64).unwrap() + 1;
        std::fs::write(root.join("huge.rs"), vec![b'/'; oversize]).unwrap();
        let minified = format!("var x=\"{}\";\n", "0".repeat(70_000));
        std::fs::write(root.join("bundle.js"), minified).unwrap();

        // Indexable and absent: the cap is the cause only when one was hit.
        assert_eq!(
            absent_motif(&root, "ok.rs", false),
            UncoveredMotif::NotIndexed
        );
        assert_eq!(
            absent_motif(&root, "ok.rs", true),
            UncoveredMotif::ExcludedByFileCap
        );
        // The cap never explains a file the build would refuse anyway.
        assert_eq!(
            absent_motif(&root, "notes.txt", true),
            UncoveredMotif::UnsupportedLanguage
        );
        assert_eq!(
            absent_motif(&root, "blob.rs", true),
            UncoveredMotif::NonTextChange
        );
        assert_eq!(
            absent_motif(&root, "huge.rs", true),
            UncoveredMotif::ExcludedBySize
        );
        assert_eq!(
            absent_motif(&root, "bundle.js", true),
            UncoveredMotif::NotIndexed
        );
        assert_eq!(
            absent_motif(&root, "vanished.rs", true),
            UncoveredMotif::NotIndexed
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The motif names are the contract's, shared with
    /// `pixel_proto::evaluate::UncoveredMotif`: a rename here renames a
    /// published field value.
    #[test]
    fn motifs_serialize_as_the_wire_vocabulary() {
        let names: Vec<String> = [
            UncoveredMotif::OutsideSymbol,
            UncoveredMotif::UnsupportedLanguage,
            UncoveredMotif::ExcludedByFileCap,
            UncoveredMotif::ExcludedBySize,
            UncoveredMotif::NotIndexed,
            UncoveredMotif::NonTextChange,
        ]
        .iter()
        .map(|m| {
            serde_json::to_value(m)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
        assert_eq!(
            names,
            [
                "outside_symbol",
                "unsupported_language",
                "excluded_by_file_cap",
                "excluded_by_size",
                "not_indexed",
                "non_text_change",
            ]
        );
    }

    /// A range that no side holds is left out of the JSON entirely, so a
    /// reader never has to tell `null` from "the other side".
    #[test]
    fn an_uncovered_change_omits_the_side_it_has_no_lines_for() {
        let v = serde_json::to_value(UncoveredChange {
            path: "src/a.ts".to_string(),
            motif: UncoveredMotif::OutsideSymbol,
            old_lines: None,
            new_lines: Some([4, 9]),
        })
        .unwrap();
        assert_eq!(v["path"], "src/a.ts");
        assert_eq!(v["motif"], "outside_symbol");
        assert_eq!(v["new_lines"], serde_json::json!([4, 9]));
        assert!(v.get("old_lines").is_none(), "{v}");
    }

    #[test]
    fn classify_should_judge_a_modified_file_symbol_against_its_base() {
        let base: HashMap<String, String> = HashMap::from([(
            "keep#function".to_string(),
            "export function keep(n: number):  number {".to_string(),
        )]);
        let same_sig = "export function keep(n: number): number {";
        let new_sig = "export function keep(n: number, m: number): number {";
        let modified = FileStatus::Modified;
        assert_eq!(
            classify(&modified, Some(&base), "keep#function", same_sig),
            ("modified", "symbol", Some(false)),
            "whitespace alone is no signature change"
        );
        assert_eq!(
            classify(&modified, Some(&base), "keep#function", new_sig),
            ("modified", "symbol", Some(true))
        );
        assert_eq!(
            classify(&modified, Some(&base), "fresh#function", same_sig),
            ("added", "symbol", None)
        );
        assert_eq!(
            classify(&modified, None, "keep#function", same_sig),
            ("modified", "file", None)
        );
        assert_eq!(
            classify(&FileStatus::Added, Some(&base), "keep#function", same_sig),
            ("added", "file", None)
        );
        assert_eq!(
            classify(&FileStatus::Deleted, Some(&base), "keep#function", same_sig),
            ("deleted", "file", None)
        );
    }

    #[test]
    fn innermost_should_pick_the_narrowest_span_holding_the_line() {
        let row = |id: i64, start_line: u32, end_line: u32| SymbolRow {
            id,
            uid: format!("f.rs#s{id}#function"),
            file_id: 1,
            name: format!("s{id}"),
            qualified: format!("s{id}"),
            kind: crate::store::SymbolKind::Function,
            start_line,
            end_line,
            sig: String::new(),
        };
        let symbols = vec![row(1, 1, 10), row(2, 3, 5), row(3, 3, 5)];
        let at = |line| innermost(&symbols, line).map(|s| s.id);
        assert_eq!(at(4), Some(2), "narrowest wins, first on a tie");
        assert_eq!(at(3), Some(2));
        assert_eq!(at(5), Some(2));
        assert_eq!(at(6), Some(1));
        assert_eq!(at(1), Some(1));
        assert_eq!(at(10), Some(1));
        assert_eq!(at(11), None);
        // Narrowest by line count, not by any other ordering of the bounds.
        let overlapping = vec![row(5, 2, 5), row(6, 1, 3)];
        assert_eq!(innermost(&overlapping, 2).map(|s| s.id), Some(6));
        let inner_late = vec![row(7, 1, 10), row(8, 8, 9)];
        assert_eq!(innermost(&inner_late, 8).map(|s| s.id), Some(8));
    }

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
