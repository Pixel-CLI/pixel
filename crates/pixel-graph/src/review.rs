//! `review-gate` — a deterministic pre-review pass over the working-tree
//! diff, or the branch merge-base diff on a feature branch.
//!
//! Reuses [`crate::changes::detect_diffs`] for the graph side (uncovered ranges,
//! suggested tests, change-set risk, the lower-bound envelope) and adds the
//! mechanical rules a code review would otherwise start from scratch on:
//! a changed symbol whose callers were not themselves changed
//! (`producer-reader-divergence`), and an added line that carries a
//! credential-shaped value or a secret-named assignment
//! (`possible-secret`), plus the remap of the report's completeness signals
//! onto findings (`uncovered-change`, `unanchored-symbol`,
//! `changed-symbol-without-test`, `risk-climb`,
//! `unresolved-callers-lower-bound`).
//!
//! Findings never carry the matched secret value, only the pattern class
//! and the anchor; every finding names the witness that established it, and
//! every cap the pass fires is named in the report's `caps` (the daemon
//! mirrors it into the epistemics envelope and a `RESULT_CAPPED` warning).

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;

use crate::changes::{ChangedSymbol, ChangesReport};
use crate::changes::{FileDiff, FileStatus, parse_diff};
use crate::concept::is_test_path;
use crate::impact::{file_path_by_id, symbol_by_id};
use crate::store::{EdgeKind, GraphStore};
use pixel_git::GitRunner;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Cap on consumer-divergence findings per changed symbol. A hot function
/// has thousands of callers; the first `n` name the shape, and the cap is
/// surfaced in `caps` so the gate never suggests it saw all of them.
const CONSUMER_CAP: usize = 20;

/// Cap on `possible-secret` findings total: a vendored blob of secrets must
/// not flood the report past the first anchors.
const SECRET_CAP: usize = 50;

/// Cap on files read to scan added lines for secrets. Each one is a read of
/// the working tree; beyond the cap the pass abstains (surfaced in `caps`),
/// it never claims the unscanned files hold no secret.
const SECRET_FILES_CAP: usize = 200;

/// Total cap on findings after assembly: a change touching a thousand
/// symbols still produces a bounded report, CRITICAL and HIGH kept first.
const MAX_FINDINGS: usize = 200;

/// A credential-shaped value token: the token alone flags the line,
/// whatever surrounds it. `(pattern class, needles)`; the class names the
/// finding's evidence without printing the matched value.
const SECRET_VALUE_PATTERNS: &[(&str, &[&str])] = &[
    ("private-key", &["-----BEGIN ", "PRIVATE KEY-----"]),
    (
        "github-token",
        &["ghp_", "github_pat_", "gho_", "ghu_", "ghs_", "ghr_"],
    ),
    ("openai-key", &["sk-proj-", "sk-ant-", "sk-svca-"]),
    ("slack-token", &["xoxb-", "xoxp-", "xoxa-", "xoxr-"]),
];

/// A name shaped like a credential. Only flagged when the same line also
/// assigns it (`=` or `:`), so `fn parse_token()` is not a finding.
const SECRET_NAME_ASSIGN: &[&str] = &[
    "api_key",
    "api-key",
    "apikey",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "credential",
];

/// One deterministic finding: a rule that fired, its severity, the anchor it
/// points at, and the witness — the evidence text — that reconstructs it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewFinding {
    pub rule: String,
    /// "LOW" | "MEDIUM" | "HIGH" | "CRITICAL"
    pub severity: String,
    /// Repo-relative path; `None` for change-set-level findings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// 1-based line in the working tree; `None` when the rule has no line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub evidence: String,
    pub fix_hint: &'static str,
}

/// The full deterministic review verdict for one change set.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewReport {
    /// Findings, CRITICAL first, then by file/line/rule.
    pub findings: Vec<ReviewFinding>,
    /// The ref the diff ran against: `None` means the working tree's
    /// uncommitted diff against the index/HEAD; `Some(oid)` on a feature
    /// branch is the merge-base with the remote default branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Named caps the pass fired; the daemon mirrors them into
    /// `epistemics.lower_bound` and a `RESULT_CAPPED` envelope warning.
    pub caps: Vec<String>,
}

/// A non-default branch means "review the branch": the merge-base of HEAD
/// with the remote default (`origin/HEAD` refreshed from the remote when it
/// is reachable, falling back to `origin/main`). `None` on the default
/// branch itself, in detached HEAD, or when no remote default resolves.
fn branch_merge_base(runner: &GitRunner) -> Option<String> {
    let branch = runner.current_branch()?;
    // A stale or missing `origin/HEAD` picks the wrong branch as the
    // default; refresh it from the remote when the remote answers.
    runner.run_opt(&["remote", "set-head", "origin", "--auto"]);
    let remote_default = runner
        .run_opt(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .unwrap_or_default();
    let remote_default = if remote_default.is_empty() {
        "origin/main".to_string()
    } else {
        remote_default
    };
    if branch == remote_default || Some(branch.as_str()) == remote_default.strip_prefix("origin/") {
        return None;
    }
    runner
        .run_opt(&["merge-base", "HEAD", &remote_default])
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Run the deterministic review of the working tree against `base_ref`
/// (default HEAD). The diff is read once and shared with the graph side via
/// [`crate::changes::detect_diffs`]; the two extra passes walk the parsed
/// diff for added lines and read every changed symbol's callers.
pub fn review(
    store: &GraphStore,
    root: &Path,
    base_ref: Option<&str>,
) -> Result<ReviewReport, BoxError> {
    let runner = GitRunner::new(root);
    // No explicit base: a feature branch reviews the whole merge-base diff
    // (which already covers any uncommitted edits); anything else reviews
    // the working tree's uncommitted diff.
    let mut base = base_ref.map(str::to_string);
    let mut diff_bytes = runner.diff_unified0(base.as_deref())?;
    if base_ref.is_none()
        && let Some(merge_base) = branch_merge_base(&runner)
    {
        let branch_diff = runner.diff_unified0(Some(&merge_base))?;
        if !branch_diff.is_empty() {
            base = Some(merge_base);
            diff_bytes = branch_diff;
        }
    }
    let diff = String::from_utf8_lossy(&diff_bytes).into_owned();
    let file_diffs = parse_diff(&diff);
    let changed_paths: BTreeSet<String> = file_diffs.iter().map(|fd| fd.path.clone()).collect();
    let report = crate::changes::detect_diffs(store, root, base.as_deref(), &file_diffs, true)?;

    let mut findings = Vec::new();
    let mut caps = Vec::new();

    findings.extend(graph_findings(&report, &mut caps));

    let (consumers, consumers_capped) =
        consumers_outside_change(store, &report.symbols, &changed_paths)?;
    if consumers_capped {
        caps.push(format!(
            "consumer readership capped at {CONSUMER_CAP} per changed symbol; more readers exist"
        ));
    }
    findings.extend(consumers);

    let (secrets, secret_caps) = added_secret_findings(root, &file_diffs)?;
    caps.extend(secret_caps);
    findings.extend(secrets);

    findings.sort_by(|a, b| {
        severity_rank(&b.severity)
            .cmp(&severity_rank(&a.severity))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.rule.cmp(&b.rule))
    });
    if findings.get(MAX_FINDINGS).is_some() {
        caps.push(format!(
            "findings truncated at {MAX_FINDINGS}; lower-severity tail not listed"
        ));
        findings.truncate(MAX_FINDINGS);
    }

    Ok(ReviewReport {
        findings,
        base,
        caps,
    })
}

/// Findings that are the `detect` report re-stated as a review vocabulary,
/// plus the caps its completeness signals stand for. Pure over the report:
/// no store, no I/O, so the severity mappings get the bracket tests.
fn graph_findings(report: &ChangesReport, caps: &mut Vec<String>) -> Vec<ReviewFinding> {
    let mut out = Vec::new();

    if let Some(finding) = risk_finding(&report.risk) {
        out.push(finding);
    }

    // The envelope note is built by `detect` from the same-name site list;
    // its prefix is the stable marker that lower-bound sites exist. The
    // finding stays change-set-level: the report carries the count of
    // affected names, not their paths. MEDIUM like `risk-climb`: it names
    // where to look, and no edit of the change can clear it (#555).
    if report.envelope_note.starts_with("lower bound:") {
        out.push(ReviewFinding {
            rule: "unresolved-callers-lower-bound".into(),
            severity: REPORT_LEVEL_SEVERITY.into(),
            file: None,
            line: None,
            evidence: report.envelope_note.clone(),
            fix_hint: "resolve the same-name call sites (imports, aliases, dynamic dispatch) \
                       before trusting a caller-free graph answer",
        });
    }

    for u in &report.uncovered_changes {
        out.push(ReviewFinding {
            rule: "uncovered-change".into(),
            severity: motif_severity(&u.motif).into(),
            file: Some(u.path.clone()),
            line: u.new_lines.or(u.old_lines).map(|[a, _]| a),
            evidence: format!(
                "changed range maps to no symbol ({motif:?})",
                motif = u.motif
            ),
            fix_hint: "index the file, or add symbol anchors, so the change is attributable",
        });
    }
    if report.uncovered_lower_bound {
        caps.push(
            "uncovered ranges truncated, or the old side of some file was not examined".to_string(),
        );
    }

    for u in &report.unanchored {
        // A deleted symbol that nothing still names is a finished deletion:
        // it can never anchor, so a CONCERN could only be bypassed. Keep the
        // CONCERN for the case that can be fixed — a surviving same-name
        // call or reference site (`unresolved_name` consumer).
        let has_live_references = report.consumers.iter().any(|c| c.of == u.uid);
        let (severity, evidence, fix_hint) = if has_live_references {
            (
                "HIGH",
                format!(
                    "deleted symbol {name} has no anchor in the current graph",
                    name = u.name
                ),
                "confirm references to the deleted symbol were renamed or removed, or re-index",
            )
        } else {
            (
                "MEDIUM",
                format!(
                    "deleted symbol {name} has no anchor in the current graph; no references remain",
                    name = u.name
                ),
                "deletion left no references; nothing to fix",
            )
        };
        out.push(ReviewFinding {
            rule: "unanchored-symbol".into(),
            severity: severity.into(),
            file: Some(u.path.clone()),
            line: Some(u.old_lines[0]),
            evidence,
            fix_hint,
        });
    }

    // A truncated test walk cannot prove absence: abstain, and say why.
    if report.suggested_tests_lower_bound {
        caps.push(
            "suggested tests truncated; a changed symbol without a listed test may still \
                   have one"
                .to_string(),
        );
        return out;
    }
    let covered: BTreeSet<&str> = report
        .suggested_tests
        .iter()
        .flat_map(|t| t.matched_symbols.iter().map(String::as_str))
        .collect();
    for s in &report.symbols {
        if is_test_path(&s.path) || covered.contains(s.name.as_str()) {
            continue;
        }
        out.push(ReviewFinding {
            rule: "changed-symbol-without-test".into(),
            severity: "MEDIUM".into(),
            file: Some(s.path.clone()),
            line: None,
            evidence: format!("{name} changed with no suggested test", name = s.name),
            fix_hint: "add a test that exercises the new behaviour",
        });
    }

    out
}

/// Severity of the change-set-level findings (`risk-climb`,
/// `unresolved-callers-lower-bound`): a SUGGESTION, never a CONCERN. They
/// describe what the change touches (the blast radius of a hub such as the
/// CLI's `run`, a name defined twice), so no edit of the change can clear
/// them; at CONCERN the pre-push gate (`--fail-on concern`) refused every
/// push touching such a symbol and left `--no-verify` as the only way
/// through (#555). The level they reached stays in the evidence.
const REPORT_LEVEL_SEVERITY: &str = "MEDIUM";

/// The risk climb a change set reaches, as a finding: `HIGH` and above only,
/// so a quiet change set emits nothing.
fn risk_finding(risk: &str) -> Option<ReviewFinding> {
    match risk {
        "CRITICAL" | "HIGH" => Some(report_level("risk-climb", REPORT_LEVEL_SEVERITY, risk)),
        _ => None,
    }
}

/// The change-set-level finding shape shared by the report-remap rules.
fn report_level(rule: &str, severity: &str, risk: &str) -> ReviewFinding {
    ReviewFinding {
        rule: rule.into(),
        severity: severity.into(),
        file: None,
        line: None,
        evidence: format!("change-set risk reached {risk}"),
        fix_hint: "review the high-blast-radius change as a whole before landing it",
    }
}

/// The review word a finding's severity prints as: CRITICAL is a BLOCKER,
/// HIGH a CONCERN, MEDIUM a SUGGESTION, LOW a NIT. Anything else prints as
/// it came, so an unknown severity is never relabelled.
pub fn severity_label(severity: &str) -> &str {
    match severity {
        "CRITICAL" => "BLOCKER",
        "HIGH" => "CONCERN",
        "MEDIUM" => "SUGGESTION",
        "LOW" => "NIT",
        other => other,
    }
}

/// Severity of an uncovered range by what made it unanchored: a binary or
/// mode-only change is not a coverage gap, everything else is.
fn motif_severity(motif: &crate::changes::UncoveredMotif) -> &'static str {
    match motif {
        crate::changes::UncoveredMotif::NonTextChange => "LOW",
        _ => "MEDIUM",
    }
}

/// Total order for stabilising a report: CRITICAL first, then HIGH, then
/// MEDIUM, then LOW — the tail a truncation cuts first. The CLI's
/// `--fail-on` threshold compares against the same scale.
pub fn severity_rank(severity: &str) -> u8 {
    match severity {
        "CRITICAL" => 4,
        "HIGH" => 3,
        "MEDIUM" => 2,
        _ => 1,
    }
}

/// A changed symbol, its depth-1 callers walked: a caller reading the
/// changed behaviour whose own file is not part of the change set is a
/// divergence the change-propagation rule makes mechanical. Never claims
/// the caller is broken — only that it was not routed through the change.
/// The finding anchors at the changed producer's site, never at the
/// untouched reader's: an anchor there puts a file the branch never
/// touched into the review's file list, where it reads as reviewed code.
fn consumers_outside_change(
    store: &GraphStore,
    changed: &[ChangedSymbol],
    changed_paths: &BTreeSet<String>,
) -> Result<(Vec<ReviewFinding>, bool), BoxError> {
    let mut out = Vec::new();
    let mut capped = false;
    for cs in changed {
        let Some(sym) = store.symbol_by_uid(&cs.uid)? else {
            continue;
        };
        let edges = store.edges_to(sym.id, Some(EdgeKind::Calls))?;
        // Filter and deduplicate by caller first — skipped edges and repeat
        // call sites of one caller must not consume the cap or emit
        // duplicate findings.
        let mut seen_callers: BTreeSet<i64> = BTreeSet::new();
        let mut callers = Vec::new();
        for edge in edges {
            if !seen_callers.insert(edge.src_id) {
                continue;
            }
            let Some(caller) = symbol_by_id(store, edge.src_id)? else {
                continue;
            };
            let path = file_path_by_id(store, caller.file_id)?;
            if changed_paths.contains(&path) {
                continue;
            }
            callers.push((caller, path));
        }
        if callers.get(CONSUMER_CAP).is_some() {
            capped = true;
        }
        for (caller, path) in callers.into_iter().take(CONSUMER_CAP) {
            out.push(ReviewFinding {
                rule: "producer-reader-divergence".into(),
                severity: "MEDIUM".into(),
                file: Some(cs.path.clone()),
                line: Some(sym.start_line),
                evidence: format!(
                    "{producer} changed; {consumer} at {path}:{line} reads it and was not changed",
                    producer = cs.name,
                    consumer = caller.name,
                    path = path,
                    line = caller.start_line,
                ),
                fix_hint: "route this consumer through the new behaviour, or record why none \
                           is needed",
            });
        }
    }
    Ok((out, capped))
}

/// An AWS access key id is `AKIA` followed by 16 uppercase alphanumerics —
/// a bare `AKIA` substring in prose or a shorter token is not a credential.
fn has_aws_access_key(line: &str) -> bool {
    line.as_bytes().windows(20).any(|window| {
        window.starts_with(b"AKIA")
            && window[4..]
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    })
}

/// The pattern class an added line matches, or `None`. The tuple's second
/// field says whether the token is a credential-shaped *value* (strong) or
/// only a secret-named *assignment* (weak).
fn scan_line_for_secret(line: &str) -> Option<(&'static str, bool)> {
    for &(class, needles) in SECRET_VALUE_PATTERNS {
        if needles.iter().any(|needle| line.contains(needle)) {
            return Some((class, true));
        }
    }
    if has_aws_access_key(line) {
        return Some(("cloud-access-key", true));
    }
    let lower = line.to_ascii_lowercase();
    if SECRET_NAME_ASSIGN
        .iter()
        .any(|needle| lower.contains(needle))
        && (line.contains('=') || line.contains(':'))
    {
        return Some(("credential-named-assignment", false));
    }
    None
}

/// Test and fixture paths carry credential-shaped strings on purpose —
/// a table of patterns a scanner test feeds itself is not a leak. The
/// finding still fires one rung lower; a real key pasted into a test file
/// is still committed.
fn test_fixture_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.split('/')
        .any(|component| matches!(component, "tests" | "fixtures" | "fixture"))
        || name == "fixture-gen.py"
        || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains("_tests.")
        || name.contains(".test.")
        || name == "tests.rs"
}

/// Scan every added line of every changed file for credential-shaped
/// content. Only *added* ranges carry risk — a deletion removes, it does
/// not introduce — and the finding never echoes the matched value, only the
/// pattern class and the anchor.
fn added_secret_findings(
    root: &Path,
    file_diffs: &[FileDiff],
) -> Result<(Vec<ReviewFinding>, Vec<String>), BoxError> {
    let mut out = Vec::new();
    let mut capped = false;
    let mut files_read = 0usize;
    let mut files_unread = 0usize;
    'files: for fd in file_diffs {
        // One clause per `if`: `parse_diff` never gives a non-text or
        // deleted entry added ranges, so `||`/`&&` mutants between these
        // terms are unreachable through real diffs.
        if !fd.text {
            continue;
        }
        if fd.status == FileStatus::Deleted {
            continue;
        }
        if fd.added_ranges.is_empty() {
            continue;
        }
        if files_read >= SECRET_FILES_CAP {
            capped = true;
            break;
        }
        files_read += 1;
        let Ok(content) = std::fs::read_to_string(root.join(&fd.path)) else {
            // Gone or unreadable since the diff was written: the lines
            // were never scanned, and the report must say so.
            files_unread += 1;
            continue;
        };
        // Everything after `#[cfg(test)]` is test code: fixture strings in
        // an inline test module are data, not leaks.
        let test_region_start = content
            .lines()
            .position(|line| line.contains("#[cfg(test)]"))
            .map(|i| i as u32 + 1);
        let test_fixture_path = test_fixture_path(&fd.path);
        for &(start, end) in &fd.added_ranges {
            for (i, line) in content
                .lines()
                .skip((start - 1) as usize)
                .take((end - start + 1) as usize)
                .enumerate()
            {
                let Some((class, strong)) = scan_line_for_secret(line) else {
                    continue;
                };
                // Fixture strings legitimately match the patterns; a live
                // key in a test file is still a leak, so the finding is
                // kept but drops a rung: CRITICAL→MEDIUM, MEDIUM→LOW.
                // A bare literal (`"ghp_abc",`, `("github-token", &[…])`,
                // `&["sk-proj-", …]`) is a pattern-table entry, not an
                // assignment — same downgrade.
                let trimmed = line.trim_start();
                let bare_literal = !trimmed.contains('=')
                    && trimmed
                        .chars()
                        .next()
                        .is_some_and(|c| matches!(c, '"' | '(' | '&'));
                let test_fixture = test_fixture_path
                    || bare_literal
                    || test_region_start.is_some_and(|t| start + i as u32 >= t);
                out.push(ReviewFinding {
                    rule: "possible-secret".into(),
                    severity: match (strong, test_fixture) {
                        (true, false) => "CRITICAL",
                        (true, true) | (false, false) => "MEDIUM",
                        (false, true) => "LOW",
                    }
                    .into(),
                    file: Some(fd.path.clone()),
                    line: Some(start + i as u32),
                    evidence: format!("added line matches the {class} pattern"),
                    fix_hint: if strong {
                        "confirm this is not a live credential; rotate it if it is"
                    } else {
                        "confirm the assigned value is not a real secret; prefer env/config \
                         injection"
                    },
                });
                if out.len() >= SECRET_CAP {
                    capped = true;
                    break 'files;
                }
            }
        }
    }
    let mut caps = Vec::new();
    if capped {
        caps.push(format!(
            "secret scan capped at {SECRET_FILES_CAP} files / {SECRET_CAP} findings; the rest was not scanned"
        ));
    }
    if files_unread > 0 {
        caps.push(format!(
            "{files_unread} changed file(s) unreadable; their added lines were not scanned"
        ));
    }
    Ok((out, caps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::UncoveredMotif;

    /// An added-line fixture is a protocol fixture: each case names the
    /// pattern class it must land in, built as data so the scanner is
    /// tested against the table, not against a narrative. Provider-shaped
    /// literals are split with `concat!` so external secret scanners do not
    /// read the table itself as leaked keys.
    const SECRET_CASES: &[(&str, Option<(&str, bool)>)] = &[
        ("const K: &str = \"ghp_abc\";", Some(("github-token", true))),
        (
            concat!("sku = sk-", "proj-abcdef"),
            Some(("openai-key", true)),
        ),
        (
            concat!("-----BEGIN RSA ", "PRIVATE KEY-----"),
            Some(("private-key", true)),
        ),
        (
            concat!("value = \"AKIA", "IOSFODNN7EXAMPLE\""),
            Some(("cloud-access-key", true)),
        ),
        // A bare or short `AKIA` substring is not an access key id.
        ("AKIA appears in this prose", None),
        ("value = \"AKIA1234567890\"", None),
        ("token = \"ghr_abcdef\"", Some(("github-token", true))),
        ("xoxb-1234 = \"ignored\"", Some(("slack-token", true))),
        (
            "api_key = env(\"API_KEY\")",
            Some(("credential-named-assignment", false)),
        ),
        (
            "API_KEY = env(\"API_KEY\")",
            Some(("credential-named-assignment", false)),
        ),
        (
            "PASSWORD = \"hunter2\"",
            Some(("credential-named-assignment", false)),
        ),
        (
            "ApiKey: \"abc\"",
            Some(("credential-named-assignment", false)),
        ),
        (
            "Secret = \"abc\"",
            Some(("credential-named-assignment", false)),
        ),
        ("let login_token = \"abc\";", None),
        // A name-shaped token without an assignment on the line is not a
        // finding: a type or a function is named after the concept.
        ("fn parse_token(line: &str)", None),
        // A plain `=` with none of the names: no finding either.
        ("let size = 40;", None),
    ];

    #[test]
    fn scan_line_for_secret_lands_each_case_in_its_class() {
        for (line, expected) in SECRET_CASES {
            assert_eq!(scan_line_for_secret(line), *expected, "line: {line:?}");
        }
    }

    #[test]
    fn severity_label_names_each_level_and_keeps_an_unknown_one() {
        assert_eq!(severity_label("CRITICAL"), "BLOCKER");
        assert_eq!(severity_label("HIGH"), "CONCERN");
        assert_eq!(severity_label("MEDIUM"), "SUGGESTION");
        assert_eq!(severity_label("LOW"), "NIT");
        assert_eq!(severity_label("?"), "?");
    }

    #[test]
    fn severity_rank_orders_critical_first_and_low_last() {
        assert!(severity_rank("CRITICAL") > severity_rank("HIGH"));
        assert!(severity_rank("HIGH") > severity_rank("MEDIUM"));
        assert!(severity_rank("MEDIUM") > severity_rank("LOW"));
        assert_eq!(severity_rank("LOW"), 1);
    }

    #[test]
    fn risk_finding_only_fires_high_and_critical() {
        assert_eq!(risk_finding("LOW"), None);
        assert_eq!(risk_finding("MEDIUM"), None);
        let high = risk_finding("HIGH").expect("HIGH fires");
        assert_eq!(high.severity, "MEDIUM");
        assert_eq!(high.evidence, "change-set risk reached HIGH");
        assert_eq!(high.file, None);
        let critical = risk_finding("CRITICAL").expect("CRITICAL fires");
        assert_eq!(critical.severity, "MEDIUM");
        assert_eq!(critical.evidence, "change-set risk reached CRITICAL");
    }

    #[test]
    fn motif_severity_treats_non_text_as_low_only() {
        assert_eq!(motif_severity(&UncoveredMotif::NonTextChange), "LOW");
        assert_eq!(motif_severity(&UncoveredMotif::OutsideSymbol), "MEDIUM");
        assert_eq!(motif_severity(&UncoveredMotif::NotIndexed), "MEDIUM");
    }

    // --- integration: a real git repo and a built graph ---

    fn tmpdir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("review-gate-{tag}-"))
            .tempdir()
            .unwrap()
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "review")
            .env("GIT_AUTHOR_EMAIL", "review@test")
            .env("GIT_COMMITTER_NAME", "review")
            .env("GIT_COMMITTER_EMAIL", "review@test")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// Build a fresh graph over `root` and open it: the fixture commits
    /// first, so the store is a snapshot of the base state and the working
    /// tree edits after it are what the diff sees.
    fn store_for(root: &Path) -> GraphStore {
        let db = root.join("graph.db");
        crate::build::build_graph(root, &db).expect("fixture graph builds");
        GraphStore::open(&db).expect("fixture store opens")
    }

    /// A two-module crate: `src/a.rs` is changed, `src/b.rs` (which calls
    /// into it) is not. The consumer-in-another-file shape is what the
    /// divergence rule exists for — a caller inside the changed file is by
    /// definition part of the change.
    fn divergence_and_secret_fixture() -> tempfile::TempDir {
        let dir = tmpdir("divergence");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod a;\npub mod b;\n").unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn produce() -> i32 { 1 }\n").unwrap();
        std::fs::write(
            root.join("src/b.rs"),
            "use crate::a::produce;\npub fn consume() -> i32 { produce() }\n",
        )
        .unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        dir
    }

    #[test]
    fn review_flags_divergence_secrets_and_untested_changes() {
        let dir = divergence_and_secret_fixture();
        let root = dir.path();
        let store = store_for(root);
        // Change `produce`, append a credential-shaped value (strong) and a
        // secret-named assignment (weak). `src/b.rs` is untouched.
        std::fs::write(
            root.join("src/a.rs"),
            "pub fn produce() -> i32 { 2 }\n\
             const TRAILING_KEY: &str = \"ghp_1234567890abcdef\";\n\
             const api_key_value: &str = env!(\"API_KEY\");\n",
        )
        .unwrap();

        let report = review(&store, root, None).expect("review runs on a changed tree");
        let rules: Vec<&str> = report.findings.iter().map(|f| f.rule.as_str()).collect();
        // The cap the rules ride must not have fired on this tiny change set.
        assert!(report.caps.is_empty(), "{report:?}");

        let divergence: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "producer-reader-divergence")
            .collect();
        assert!(
            !divergence.is_empty(),
            "changed `produce` read by unchanged `consume` must be flagged: {rules:?}"
        );
        let d = &divergence[0];
        assert_eq!(d.severity, "MEDIUM");
        // The finding anchors at the changed producer, so the review's file
        // list only ever names files the change set touched; the untouched
        // reader stays locatable through the evidence.
        assert_eq!(d.file.as_deref(), Some("src/a.rs"));
        assert_eq!(d.line, Some(1), "{}", d.evidence);
        assert!(d.evidence.contains("consume"), "{}", d.evidence);
        assert!(d.evidence.contains("produce"), "{}", d.evidence);
        assert!(
            d.evidence.contains("src/b.rs:2"),
            "the untouched reader's site is named: {}",
            d.evidence
        );

        // Strong and weak secret shapes both surface, at their exact lines,
        // and neither echoes the matched value.
        let secrets: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "possible-secret")
            .collect();
        assert_eq!(secrets.len(), 2, "strong + weak: {rules:?}");
        let s = &secrets[0];
        assert_eq!(s.severity, "CRITICAL");
        assert_eq!(s.file.as_deref(), Some("src/a.rs"));
        assert_eq!(s.line, Some(2));
        let weak = &secrets[1];
        assert_eq!(weak.severity, "MEDIUM");
        assert_eq!(weak.line, Some(3));
        assert!(
            weak.evidence.contains("credential-named-assignment"),
            "{}",
            weak.evidence
        );
        for f in &secrets {
            assert!(
                !f.evidence.contains("ghp_") && !f.evidence.contains("API_KEY"),
                "evidence must not echo the matched value: {}",
                f.evidence
            );
        }

        assert!(
            rules.contains(&"changed-symbol-without-test"),
            "changed `produce` reached by no test: {rules:?}"
        );
        // The added const lines sit between symbols on line 2: uncovered.
        let uncovered: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "uncovered-change")
            .collect();
        assert!(
            !uncovered.is_empty(),
            "added module-level const is outside every symbol: {rules:?}"
        );
        assert_eq!(uncovered[0].file.as_deref(), Some("src/a.rs"));
        assert_eq!(uncovered[0].line, Some(2));
    }

    /// A strong token in a `#[cfg(test)]` region or a bare literal in a
    /// pattern table is fixture data, not a leak — one rung lower. The same
    /// token assigned in production code stays CRITICAL.
    #[test]
    fn review_downgrades_secret_matches_in_test_regions_and_pattern_tables() {
        let dir = divergence_and_secret_fixture();
        let root = dir.path();
        let store = store_for(root);
        std::fs::write(
            root.join("src/a.rs"),
            "pub fn produce() -> i32 { 2 }\n\
             const REAL: &str = \"ghp_1234567890abcdef\";\n\
             const TABLE: &[&str] = &[\n\
             \x20   \"ghp_abcdef\",\n\
             ];\n\
             #[cfg(test)]\nmod tests {\n\
             \x20   const CASE: &str = \"ghp_feedface\";\n\
             }\n",
        )
        .unwrap();

        let report = review(&store, root, None).expect("review runs");
        let secrets: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "possible-secret")
            .collect();
        let at = |line: u32| {
            secrets
                .iter()
                .find(|f| f.line == Some(line))
                .unwrap_or_else(|| panic!("no possible-secret at line {line}: {secrets:?}"))
        };
        assert_eq!(at(2).severity, "CRITICAL", "assigned in prod code");
        assert_eq!(at(4).severity, "MEDIUM", "bare literal in a table");
        assert_eq!(at(8).severity, "MEDIUM", "inside the cfg(test) region");
    }

    /// A changed symbol whose file name is a test path is its own test: the
    /// `changed-symbol-without-test` rule must not flag it.
    #[test]
    fn review_skips_a_changed_symbol_inside_a_test_file() {
        let dir = tmpdir("test-path");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod a_test;\n").unwrap();
        std::fs::write(root.join("src/a_test.rs"), "pub fn helper() -> i32 { 1 }\n").unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        let store = store_for(root);

        std::fs::write(root.join("src/a_test.rs"), "pub fn helper() -> i32 { 2 }\n").unwrap();
        let report = review(&store, root, None).expect("review runs");
        let seen: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "changed-symbol-without-test")
            .collect();
        assert!(
            seen.is_empty(),
            "a change inside a _test file is its own test: {seen:?}"
        );
    }

    /// Two same-named definitions plus an importless call site is an
    /// unresolved same-name site: the lower-bound cap must surface as a
    /// change-set finding, at SUGGESTION level since no edit clears it.
    #[test]
    fn review_surfaces_an_unresolved_same_name_lower_bound() {
        let dir = tmpdir("lower-bound");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        for (name, body) in [
            ("a", "pub fn produce() -> i32 { 1 }\n"),
            ("b", "pub fn produce() -> i32 { 2 }\n"),
            ("c", "pub fn consumer() -> i32 { produce() }\n"),
        ] {
            std::fs::write(root.join(format!("src/{name}.rs")), body).unwrap();
        }
        std::fs::write(
            root.join("src/lib.rs"),
            "pub mod a;\npub mod b;\npub mod c;\n",
        )
        .unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        let store = store_for(root);

        std::fs::write(root.join("src/a.rs"), "pub fn produce() -> i32 { 3 }\n").unwrap();
        let report = review(&store, root, None).expect("review runs");
        let lb: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "unresolved-callers-lower-bound")
            .collect();
        assert_eq!(
            lb.len(),
            1,
            "unresolved `produce` must be surfaced: {report:?}"
        );
        assert_eq!(lb[0].severity, "MEDIUM", "{report:?}");
        assert_eq!(lb[0].file, None, "{report:?}");
        assert!(
            lb[0].evidence.contains("produce"),
            "the note names the affected symbol: {}",
            lb[0].evidence
        );
    }

    /// A deleted symbol nothing still names is a finished deletion: it can
    /// never anchor, so the finding is a SUGGESTION, not a CONCERN that only
    /// `--no-verify` could clear. The evidence keeps the deletion.
    #[test]
    fn review_marks_a_fully_removed_symbol_a_suggestion() {
        let dir = tmpdir("unanchored");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod del;\npub mod a;\n").unwrap();
        std::fs::write(root.join("src/del.rs"), "pub fn doomed() -> i32 { 1 }\n").unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn idle() -> i32 { 0 }\n").unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        let store = store_for(root);

        std::fs::remove_file(root.join("src/del.rs")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn idle() -> i32 { 1 }\n").unwrap();
        let report = review(&store, root, None).expect("review runs");
        let unanchored: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "unanchored-symbol")
            .collect();
        assert_eq!(unanchored.len(), 1, "{report:?}");
        assert_eq!(unanchored[0].severity, "MEDIUM");
        assert_eq!(unanchored[0].file.as_deref(), Some("src/del.rs"));
        assert!(
            unanchored[0].evidence.contains("doomed"),
            "{}",
            unanchored[0].evidence
        );
        // A deletion introduces nothing: no `possible-secret` for the file
        // the diff only removed.
        assert!(
            report
                .findings
                .iter()
                .all(|f| f.rule != "possible-secret" || f.file.as_deref() != Some("src/del.rs")),
            "{report:?}"
        );
    }

    /// A deleted symbol a call site still names is fixable — rename or
    /// remove the reference — so it stays a CONCERN.
    #[test]
    fn review_marks_a_deleted_symbol_with_surviving_references_a_concern() {
        let dir = tmpdir("unanchored");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod del;\npub mod a;\n").unwrap();
        std::fs::write(root.join("src/del.rs"), "pub fn doomed() -> i32 { 1 }\n").unwrap();
        std::fs::write(
            root.join("src/a.rs"),
            "pub fn idle() -> i32 { crate::del::doomed() }\n",
        )
        .unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        let store = store_for(root);

        std::fs::remove_file(root.join("src/del.rs")).unwrap();
        let report = review(&store, root, None).expect("review runs");
        let unanchored: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "unanchored-symbol")
            .collect();
        assert_eq!(unanchored.len(), 1, "{report:?}");
        assert_eq!(unanchored[0].severity, "HIGH");
        assert_eq!(unanchored[0].file.as_deref(), Some("src/del.rs"));
        assert!(
            unanchored[0].evidence.contains("doomed"),
            "{}",
            unanchored[0].evidence
        );
    }

    /// When the diff removes BOTH the symbol and its only call site, the
    /// deleted symbol has no surviving reference: it stays a SUGGESTION even
    /// though the stale index still holds the old `calls` edge (the fix
    /// reconciles that edge with the working tree and drops it).
    #[test]
    fn review_marks_a_symbol_removed_with_its_only_call_a_suggestion() {
        let dir = tmpdir("unanchored");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod del;\npub mod a;\n").unwrap();
        std::fs::write(root.join("src/del.rs"), "pub fn doomed() -> i32 { 1 }\n").unwrap();
        std::fs::write(
            root.join("src/a.rs"),
            "pub fn idle() -> i32 { crate::del::doomed() }\n",
        )
        .unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        let store = store_for(root);

        std::fs::remove_file(root.join("src/del.rs")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn idle() -> i32 { 0 }\n").unwrap();
        let report = review(&store, root, None).expect("review runs");
        let unanchored: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "unanchored-symbol")
            .collect();
        // The index still carries the idle->doomed edge; reconcile it with
        // the working tree so the removed call site is not counted as a
        // surviving reference.
        assert_eq!(unanchored.len(), 1, "{report:?}");
        assert_eq!(unanchored[0].severity, "MEDIUM", "{report:?}");
        assert_eq!(unanchored[0].file.as_deref(), Some("src/del.rs"));
    }

    /// A consumer that names a DIFFERENT deleted symbol must not raise an
    /// unrelated deletion: `has_live_references` keys on the consumer's
    /// `of`, not on the mere presence of any consumer. Hoisting that
    /// predicate to a constant `true` would loop every unanchored symbol
    /// into HIGH the moment ANY consumer survives — this test pins the
    /// per-symbol cost of that mutant.
    #[test]
    fn review_only_clears_the_concern_for_a_symbol_whose_own_references_survived() {
        let dir = tmpdir("unanchored");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub mod a;\npub mod b;\npub fn keeper() -> i32 { crate::b::beta() }\n",
        )
        .unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn alpha() -> i32 { 1 }\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "pub fn beta() -> i32 { 2 }\n").unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        let store = store_for(root);

        // Delete both files: `beta` keeps a live call site in the untouched
        // `lib.rs` (a surviving consumer), `alpha` has none anywhere.
        std::fs::remove_file(root.join("src/a.rs")).unwrap();
        std::fs::remove_file(root.join("src/b.rs")).unwrap();
        let report = review(&store, root, None).expect("review runs");
        let unanchored: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "unanchored-symbol")
            .collect();
        assert_eq!(unanchored.len(), 2, "{report:?}");
        // `report.consumers` is non-empty (the surviving beta call site), so
        // a predicate coerced to `true` would wrongly make EVERY deletion
        // HIGH. `alpha` has no surviving reference and must stay MEDIUM.
        assert_eq!(
            unanchored
                .iter()
                .find(|f| f.file.as_deref() == Some("src/a.rs"))
                .map(|f| f.severity.as_str()),
            Some("MEDIUM"),
            "{report:?}"
        );
        assert_eq!(
            unanchored
                .iter()
                .find(|f| f.file.as_deref() == Some("src/b.rs"))
                .map(|f| f.severity.as_str()),
            Some("HIGH"),
            "{report:?}"
        );
    }

    #[test]
    fn review_on_a_clean_tree_emits_no_findings() {
        let dir = tmpdir("clean");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn idle() {}\n").unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        let store = store_for(root);

        let report = review(&store, root, None).expect("review runs on a clean tree");
        assert!(
            report.findings.is_empty(),
            "nothing changed, nothing to flag: {report:?}"
        );
        assert!(report.caps.is_empty(), "{report:?}");
        // No origin/HEAD and no merge-base fallback resolves: base stays the
        // uncommitted-diff `None`.
        assert_eq!(report.base, None, "{report:?}");
    }

    /// A clean tree on a feature branch reviews the whole branch diff: the
    /// merge-base with the remote default becomes the report's base.
    #[test]
    fn review_on_a_clean_feature_branch_diffs_the_merge_base() {
        let dir = tmpdir("branch-base");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn produce() -> i32 { 1 }\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod a;\n").unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        let base_oid = {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        // The remote default branch, without a remote: a remote-tracking ref.
        git(root, &["update-ref", "refs/remotes/origin/main", &base_oid]);
        git(root, &["checkout", "-qb", "feat/x"]);
        std::fs::write(root.join("src/a.rs"), "pub fn produce() -> i32 { 2 }\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "change"]);
        let store = store_for(root);

        let report = review(&store, root, None).expect("review runs on a clean branch");
        assert_eq!(
            report.base.as_deref(),
            Some(base_oid.as_str()),
            "{report:?}"
        );
        assert!(
            !report.findings.is_empty(),
            "the committed branch change is what gets reviewed: {report:?}"
        );
    }

    #[test]
    fn test_fixture_path_names_test_and_fixture_files() {
        for yes in [
            "crates/pixel/src/main_tests/x_tests.rs",
            "src/foo_test.rs",
            "src/foo.test.ts",
            "test_helpers.rs",
            "src/tests/unit.rs",
            "src/fixtures/data.rs",
            "scripts/fixture-gen.py",
        ] {
            assert!(test_fixture_path(yes), "{yes}");
        }
        for no in [
            "src/lib.rs",
            "crates/pixel/src/main.rs",
            "src/latest.rs",
            "src/attest.rs",
            "src/fixtures_prod/key.rs",
            "src/contests.rs",
        ] {
            assert!(!test_fixture_path(no), "{no}");
        }
    }

    /// The default branch never takes the merge-base path: on `main` with
    /// `origin/main` behind, a clean tree reviews the (empty) working diff,
    /// not the branch. A `||`→`&&` mutant at the default-branch check would
    /// diff HEAD against origin/main and report the committed change.
    #[test]
    fn review_on_the_default_branch_never_diffs_the_remote() {
        let dir = tmpdir("default-branch");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod a;\n").unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn produce() -> i32 { 1 }\n").unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        git(root, &["branch", "-M", "main"]);
        let base_oid = {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        git(root, &["update-ref", "refs/remotes/origin/main", &base_oid]);
        // main carries a new commit the remote has not seen.
        std::fs::write(root.join("src/a.rs"), "pub fn produce() -> i32 { 2 }\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "ahead"]);
        let store = store_for(root);

        let report = review(&store, root, None).expect("review runs");
        assert_eq!(report.base, None, "{report:?}");
        assert!(
            report.findings.is_empty(),
            "a clean tree on main has nothing uncommitted to review: {report:?}"
        );
    }

    /// A dirty feature branch reviews the merge-base diff: the committed
    /// part of the branch is in scope, and the uncommitted edit rides the
    /// same diff (`git diff <merge-base>` covers both). A `&&`→`||` mutant
    /// at the base-selection condition would try to diff the merge-base on
    /// `main` too.
    #[test]
    fn a_dirty_feature_branch_reviews_the_merge_base_diff() {
        let dir = tmpdir("dirty-branch");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod a;\n").unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn produce() -> i32 { 1 }\n").unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        let base_oid = {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        git(root, &["update-ref", "refs/remotes/origin/main", &base_oid]);
        git(root, &["checkout", "-qb", "feat/x"]);
        std::fs::write(
            root.join("src/a.rs"),
            "pub fn produce() -> i32 { 2 }\nconst KEY: &str = \"ghp_committed\";\n",
        )
        .unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "branch"]);
        let store = store_for(root);
        std::fs::write(
            root.join("src/a.rs"),
            "pub fn produce() -> i32 { 2 }\nconst KEY: &str = \"ghp_committed\";\nconst LEAKED: &str = \"ghp_uncommitted\";\n",
        )
        .unwrap();

        let report = review(&store, root, None).expect("review runs");
        let secrets: Vec<u32> = report
            .findings
            .iter()
            .filter(|f| f.rule == "possible-secret")
            .filter_map(|f| f.line)
            .collect();
        assert_eq!(
            secrets,
            vec![2, 3],
            "the merge-base diff carries the committed and the uncommitted secret: {report:?}"
        );
        assert_eq!(
            report.base.as_deref(),
            Some(base_oid.as_str()),
            "{report:?}"
        );
    }

    /// A changed symbol exercised by a suggested test is not flagged as
    /// untested. The `||`→`&&` mutant would require it to be *both* a test
    /// path and covered, so `produce` would surface.
    #[test]
    fn a_changed_symbol_with_a_suggested_test_is_not_flagged() {
        let report = ChangesReport {
            base: "abc".into(),
            changed_files: 1,
            symbols: vec![ChangedSymbol {
                uid: "u1".into(),
                name: "produce".into(),
                path: "src/a.rs".into(),
                change: "modified".into(),
                change_basis: "symbol".into(),
                signature_changed: None,
                processes: vec![],
            }],
            affected_processes: vec![],
            consumers: vec![],
            risk: "LOW".into(),
            envelope_note: String::new(),
            suggested_tests: vec![crate::changes::SuggestedTest {
                file: "tests/produce_test.rs".into(),
                matched_symbols: vec!["produce".into()],
                via: "direct-caller".into(),
                depth: 1,
            }],
            suggested_tests_lower_bound: false,
            suggested_tests_note: String::new(),
            uncovered_changes: vec![],
            unanchored: vec![],
            uncovered_lower_bound: false,
            uncovered_note: String::new(),
        };
        let mut caps = Vec::new();
        let findings = graph_findings(&report, &mut caps);
        assert!(
            findings.is_empty(),
            "a suggested test covers the change: {findings:?}"
        );
    }

    /// The scan reads exactly the added ranges: secrets outside them — even
    /// unchanged context lines inside the same hunk — are not findings. The
    /// off-by-one mutants in `skip`/`take` and the line-number arithmetic
    /// all shift which lines land here.
    #[test]
    fn secret_scan_reads_exactly_the_added_lines() {
        let dir = tmpdir("scan-range");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        // Lines 1 and 6 carry credential-shaped strings but are unchanged
        // context; the diff adds only lines 2 and 5.
        std::fs::write(
            root.join("src/f.rs"),
            "const CTX1: &str = \"ghp_ctx1\";\nfn keep() {}\nfn mid() {}\nconst CTX2: &str = \"ghp_ctx2\";\n",
        )
        .unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        std::fs::write(
            root.join("src/f.rs"),
            "const CTX1: &str = \"ghp_ctx1\";\nconst LEAKED1: &str = \"ghp_a\";\nfn keep() {}\nfn mid() {}\nconst LEAKED2: &str = \"ghp_b\";\nconst CTX2: &str = \"ghp_ctx2\";\n",
        )
        .unwrap();
        let diff = GitRunner::new(root).diff_unified0(None).unwrap();
        let files = parse_diff(&String::from_utf8_lossy(&diff));
        assert_eq!(files[0].added_ranges, vec![(2, 2), (5, 5)], "{files:?}");
        let (findings, caps) = added_secret_findings(root, &files).expect("scan runs");
        assert_eq!(
            findings.iter().map(|f| f.line).collect::<Vec<_>>(),
            vec![Some(2), Some(5)],
            "{findings:?}"
        );
        assert!(caps.is_empty(), "{caps:?}");
    }

    /// A non-assigned, non-literal line that matches a token pattern is a
    /// real leak candidate: the bare-literal downgrade needs BOTH no `=` and
    /// a quote/paren/reference opener, so `helper(ghp_x)` stays CRITICAL. The
    /// `&&`→`||` mutant would downgrade any `=`-free match.
    #[test]
    fn a_token_in_plain_code_stays_critical() {
        let dir = tmpdir("bare-token");
        let root = dir.path();
        // Line 2: token in code, no `=`, no literal opener -> CRITICAL.
        // Line 3: an `=`-carrying line that still opens with a quote — the
        // deleted `!` in `!trimmed.contains('=')` would call it a bare
        // literal and downgrade it.
        std::fs::write(
            root.join("f.rs"),
            "fn go() {\n    helper(ghp_abcdef1234567890);\n    \"k\" = \"ghp_zz\";\n}\n",
        )
        .unwrap();
        let text = [
            "diff --git a/f.rs b/f.rs",
            "index 1111111..2222222 100644",
            "--- a/f.rs",
            "+++ b/f.rs",
            "@@ -1,1 +1,3 @@",
            " ctx",
            "+added",
            "+added2",
        ]
        .join("\n");
        let files = parse_diff(&text);
        let (findings, _) = added_secret_findings(root, &files).expect("scan runs");
        let leaked: Vec<&ReviewFinding> = findings
            .iter()
            .filter(|f| f.rule == "possible-secret")
            .collect();
        assert_eq!(
            leaked
                .iter()
                .map(|f| (f.line, f.severity.as_str()))
                .collect::<Vec<_>>(),
            vec![(Some(2), "CRITICAL"), (Some(3), "CRITICAL")],
            "{findings:?}"
        );
    }

    /// A secret on the line immediately above `#[cfg(test)]` is production
    /// code, not fixture data: the region boundary is the attribute's
    /// position plus one, so the line before it stays CRITICAL.
    #[test]
    fn a_secret_above_the_test_region_stays_critical() {
        let dir = tmpdir("region-edge");
        let root = dir.path();
        std::fs::write(
            root.join("f.rs"),
            "const REAL: &str = \"ghp_live\";\n#[cfg(test)] const EDGE: &str = \"ghp_edge\";\nmod tests {\n    const CASE: &str = \"ghp_fixture\";\n}\n",
        )
        .unwrap();
        let text = [
            "diff --git a/f.rs b/f.rs",
            "index 1111111..2222222 100644",
            "--- a/f.rs",
            "+++ b/f.rs",
            "@@ -0,0 +1,5 @@",
            "+a",
            "+b",
            "+c",
            "+d",
            "+e",
        ]
        .join("\n");
        let files = parse_diff(&text);
        let (findings, _) = added_secret_findings(root, &files).expect("scan runs");
        let at = |line: u32| {
            findings
                .iter()
                .find(|f| f.line == Some(line))
                .unwrap_or_else(|| panic!("no finding at line {line}: {findings:?}"))
        };
        assert_eq!(at(1).severity, "CRITICAL", "above cfg(test): {findings:?}");
        assert_eq!(
            at(2).severity,
            "MEDIUM",
            "on the cfg(test) line: {findings:?}"
        );
        assert_eq!(at(4).severity, "MEDIUM", "inside cfg(test): {findings:?}");
    }

    /// A credential assigned in a test/fixture path is fixture data — one
    /// rung lower. The `||`→`&&` mutant on the first term of the
    /// `test_fixture` chain would require a bare literal too and leave the
    /// assignment CRITICAL.
    #[test]
    fn an_assigned_secret_in_a_fixture_path_downgrades_to_medium() {
        let dir = tmpdir("fixture-secret");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/table_tests.rs"),
            "const K: &str = \"ghp_abc\";\n",
        )
        .unwrap();
        let text = [
            "diff --git a/src/table_tests.rs b/src/table_tests.rs",
            "index 1111111..2222222 100644",
            "--- a/src/table_tests.rs",
            "+++ b/src/table_tests.rs",
            "@@ -0,0 +1,1 @@",
            "+const K: &str = \"ghp_abc\";",
        ]
        .join("\n");
        let files = parse_diff(&text);
        let (findings, _) = added_secret_findings(root, &files).expect("scan runs");
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, "MEDIUM", "{findings:?}");
    }

    /// A real AKIA+16 key flags; an all-uppercase blob that is not
    /// `AKIA…` does not. `&&`/`||` mutants inside the window check flip
    /// both directions.
    #[test]
    fn aws_access_keys_flag_but_uppercase_text_does_not() {
        let dir = tmpdir("aws-key");
        let root = dir.path();
        std::fs::write(
            root.join("f.rs"),
            concat!(
                "const K: &str = \"AKIA",
                "IOSFODNN7EXAMPLE\";\nconst TABLE: &str = \"ABCDEFGHIJKLMNOPQRSTUV\";\n"
            ),
        )
        .unwrap();
        let text = [
            "diff --git a/f.rs b/f.rs",
            "index 1111111..2222222 100644",
            "--- a/f.rs",
            "+++ b/f.rs",
            "@@ -0,0 +1,2 @@",
            "+a",
            "+b",
        ]
        .join("\n");
        let files = parse_diff(&text);
        let (findings, _) = added_secret_findings(root, &files).expect("scan runs");
        assert_eq!(
            findings.iter().map(|f| f.line).collect::<Vec<_>>(),
            vec![Some(1)],
            "only the AKIA line: {findings:?}"
        );
        assert_eq!(findings[0].severity, "CRITICAL");
    }

    /// A changed file missing on disk is *unscanned*, not clean: its entry
    /// lands in the caps list. `+=` mutants and `>` boundary mutants on
    /// `files_unread` change whether the note appears at all.
    #[test]
    fn an_unreadable_changed_file_is_named_in_the_caps() {
        let dir = tmpdir("unread");
        let root = dir.path();
        let text = [
            "diff --git a/gone.rs b/gone.rs",
            "index 1111111..2222222 100644",
            "--- a/gone.rs",
            "+++ b/gone.rs",
            "@@ -1,1 +1,2 @@",
            " ctx",
            "+added",
        ]
        .join("\n");
        let files = parse_diff(&text);
        let (findings, caps) = added_secret_findings(root, &files).expect("scan runs");
        assert!(findings.is_empty(), "{findings:?}");
        assert!(
            caps.iter().any(|c| c.contains("unreadable")),
            "the unread file must surface: {caps:?}"
        );
    }

    /// The file cap counts only files that were scanned. A deleted entry is
    /// skipped before the counter moves; a `||`→`&&` mutant lets it consume
    /// budget, and `+=`→`*=` makes the cap unreachable.
    #[test]
    fn the_secret_scan_file_cap_counts_only_scanned_files() {
        let dir = tmpdir("secret-cap");
        let root = dir.path();
        let mut diff_lines = vec![
            "diff --git a/gone.rs b/gone.rs".to_string(),
            "deleted file mode 100644".to_string(),
            "--- a/gone.rs".to_string(),
            "+++ /dev/null".to_string(),
            "@@ -1,1 +0,0 @@".to_string(),
            "-x".to_string(),
            String::new(),
        ];
        // CAP+1 modified files; only the last *scanned* one carries a secret.
        // Normal: the deleted entry is skipped, files f0..f{CAP-1} are read,
        // f{CAP} hits the cap -> 1 finding, capped. `||`→`&&` counts the
        // deleted file, so f{CAP-1} (the secret) breaks on the cap instead.
        for i in 0..=SECRET_FILES_CAP {
            let name = format!("f{i}.rs");
            let body = if i == SECRET_FILES_CAP - 1 {
                "const K: &str = \"ghp_zz\";\n"
            } else {
                "fn keep() {}\n"
            };
            std::fs::write(root.join(&name), body).unwrap();
            diff_lines.extend([
                format!("diff --git a/{name} b/{name}"),
                "index 1111111..2222222 100644".to_string(),
                format!("--- a/{name}"),
                format!("+++ b/{name}"),
                "@@ -0,0 +1,1 @@".to_string(),
                "+added".to_string(),
            ]);
        }
        let files = parse_diff(&diff_lines.join("\n"));
        let (findings, caps) = added_secret_findings(root, &files).expect("scan runs");
        assert_eq!(findings.len(), 1, "f199 was scanned: {findings:?}");
        assert!(!caps.is_empty(), "the file past the cap is reported");
    }
}
