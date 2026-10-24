// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! What a reviewer would flag first in a diff, with no model
//! (`specs/graph.md`, "Analyses"). Lifted in spirit from Pixel's
//! `review-gate`, rebuilt on [`Changes`]:
//!
//! - `producer-reader-divergence`: a changed symbol has a `Proved` caller in
//!   a file the change does not touch. It never claims the caller is broken,
//!   only that the change did not route it.
//! - `changed-symbol-without-test`: no test file is within three callers.
//! - `unresolved-callers-lower-bound`: sites of the same name stayed open, so
//!   the callers above are a lower bound.
//! - `risk-climb`: the change set's blast radius reaches HIGH.
//! - `possible-secret`: an added line carries a credential-shaped value or a
//!   secret-named assignment. A finding names the pattern class and the
//!   line, never the value.
//!
//! Every cap the pass fires is named in [`Review::caps`].

use std::collections::BTreeSet;
use std::path::Path;

use pixel_model::{Diff, DiffFile, EdgeKind, Tier, is_test_path, leaf};

use super::changes::{Changed, Changes, changes};
use super::impact::{Direction, Options, Risk, impact};
use crate::store::{Store, StoreError};

/// Cap on divergence findings per changed symbol: the first few name the
/// shape, and the cap is surfaced in `caps`.
const CONSUMER_CAP: usize = 20;

/// Cap on `possible-secret` findings in all.
const SECRET_CAP: usize = 50;

/// Cap on files read for the secret scan; beyond it the pass abstains and
/// says so.
const SECRET_FILES_CAP: usize = 200;

/// Cap on findings after assembly; the lowest severities go first.
const MAX_FINDINGS: usize = 200;

/// A credential-shaped value token: the token alone flags the line.
const SECRET_VALUE_PATTERNS: &[(&str, &[&str])] = &[
    ("private-key", &["-----BEGIN ", "PRIVATE KEY-----"]),
    (
        "github-token",
        &["ghp_", "github_pat_", "gho_", "ghu_", "ghs_", "ghr_"],
    ),
    ("openai-key", &["sk-proj-", "sk-ant-", "sk-svca-"]),
    ("slack-token", &["xoxb-", "xoxp-", "xoxa-", "xoxr-"]),
];

/// A name shaped like a credential, flagged only on a line that assigns it.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// The review word: CRITICAL is a BLOCKER, HIGH a CONCERN, MEDIUM a
    /// SUGGESTION, LOW a NIT.
    pub fn label(self) -> &'static str {
        match self {
            Severity::Critical => "BLOCKER",
            Severity::High => "CONCERN",
            Severity::Medium => "SUGGESTION",
            Severity::Low => "NIT",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub rule: &'static str,
    pub severity: Severity,
    /// `None` for a finding about the change set as a whole.
    pub file: Option<String>,
    pub line: Option<u32>,
    pub evidence: String,
    pub fix_hint: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Review {
    /// Most severe first, then by file and line.
    pub findings: Vec<Finding>,
    /// The caps this pass fired.
    pub caps: Vec<String>,
}

/// Reviews `diff` against the generation `store` holds.
pub fn review(store: &Store, root: &Path, diff: &Diff) -> Result<Review, StoreError> {
    let found = changes(store, root, diff)?;
    let touched: BTreeSet<&str> = diff.files.iter().map(|f| f.path.as_str()).collect();
    let mut out = Review::default();
    for file in &found.unindexed {
        out.findings.push(Finding {
            rule: "unindexed-change",
            severity: Severity::Low,
            file: Some(file.path.clone()),
            line: None,
            evidence: "changed source file has no rows in the index".to_owned(),
            fix_hint: "run `pixel prepare` so the change is attributable to symbols",
        });
    }
    divergence(store, &found, &touched, &mut out)?;
    untested(&found, &mut out);
    blast_radius(store, &found, &mut out)?;
    secrets(root, &diff.files, &mut out);
    out.findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.rule.cmp(b.rule))
    });
    if out.findings.len() > MAX_FINDINGS {
        out.caps.push(format!(
            "findings truncated at {MAX_FINDINGS}; the lower severities are not listed"
        ));
        out.findings.truncate(MAX_FINDINGS);
    }
    Ok(out)
}

/// A changed symbol read by a file the change leaves alone.
fn divergence(
    store: &Store,
    found: &Changes,
    touched: &BTreeSet<&str>,
    out: &mut Review,
) -> Result<(), StoreError> {
    for changed in found
        .symbols
        .iter()
        .filter(|c| !is_test_path(&c.symbol.file))
    {
        let mut seen = BTreeSet::new();
        let mut readers = Vec::new();
        for caller in store.callers(&changed.symbol.uid, Tier::Proved)? {
            if caller.edge.kind == EdgeKind::Call
                && !touched.contains(caller.site.file.as_str())
                && seen.insert(caller.site.uid.clone())
            {
                readers.push(caller);
            }
        }
        if readers.len() > CONSUMER_CAP {
            out.caps.push(format!(
                "readers of {} capped at {CONSUMER_CAP}; more exist",
                changed.symbol.name
            ));
        }
        for reader in readers.into_iter().take(CONSUMER_CAP) {
            out.findings.push(Finding {
                rule: "producer-reader-divergence",
                severity: Severity::Medium,
                file: Some(changed.symbol.file.clone()),
                line: Some(changed.symbol.line),
                evidence: format!(
                    "{} changed; {} at {}:{} reads it and was not changed",
                    changed.symbol.name, reader.site.name, reader.site.file, reader.edge.line
                ),
                fix_hint: "route the reader through the new behaviour, or record why none is needed",
            });
        }
    }
    Ok(())
}

/// A changed symbol no test is within three callers of.
fn untested(found: &Changes, out: &mut Review) {
    for changed in &found.symbols {
        if is_test_path(&changed.symbol.file) || !changed.tests.is_empty() {
            continue;
        }
        out.findings.push(Finding {
            rule: "changed-symbol-without-test",
            severity: Severity::Medium,
            file: Some(changed.symbol.file.clone()),
            line: Some(changed.symbol.line),
            evidence: format!(
                "{} changed with no test within three callers",
                changed.symbol.name
            ),
            fix_hint: "add a test that exercises the new behaviour",
        });
    }
}

/// The change set's blast radius and the sites that stayed open.
fn blast_radius(store: &Store, found: &Changes, out: &mut Review) -> Result<(), StoreError> {
    let mut worst = Risk::Low;
    let mut open_names: BTreeSet<String> = BTreeSet::new();
    for Changed { symbol, .. } in found
        .symbols
        .iter()
        .filter(|c| !is_test_path(&c.symbol.file))
    {
        let report = impact(store, symbol, Direction::Upstream, &Options::default())?;
        if let Some(risk) = report.risk
            && risk != Risk::Unknown
        {
            worst = worst.max(risk);
        }
        if report.open_sites > 0 {
            open_names.insert(leaf(&symbol.name).to_owned());
        }
    }
    if worst >= Risk::High {
        out.findings.push(Finding {
            rule: "risk-climb",
            severity: Severity::Medium,
            file: None,
            line: None,
            evidence: format!("change-set risk reached {}", worst.as_str()),
            fix_hint: "review the high-blast-radius change as a whole before landing it",
        });
    }
    if !open_names.is_empty() {
        out.findings.push(Finding {
            rule: "unresolved-callers-lower-bound",
            severity: Severity::Medium,
            file: None,
            line: None,
            evidence: format!(
                "lower bound: unresolved sites share the name of {}",
                open_names.into_iter().collect::<Vec<_>>().join(", ")
            ),
            fix_hint: "resolve the same-name call sites before trusting a caller-free answer",
        });
    }
    Ok(())
}

/// An AWS access key id is `AKIA` and 16 uppercase alphanumerics; a bare
/// `AKIA` in prose is not a credential.
fn has_aws_access_key(line: &str) -> bool {
    line.as_bytes().windows(20).any(|window| {
        window.starts_with(b"AKIA")
            && window[4..]
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    })
}

/// The pattern class an added line matches and whether it is a credential
/// *value* (strong) or only a secret-named *assignment* (weak).
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

/// Tests and fixtures carry credential-shaped strings on purpose: the finding
/// still fires, one rung lower.
fn is_fixture(path: &str, line: &str, in_test_region: bool) -> bool {
    let trimmed = line.trim_start();
    let bare_literal = !trimmed.contains('=')
        && trimmed
            .chars()
            .next()
            .is_some_and(|c| matches!(c, '"' | '(' | '&'));
    is_test_path(path)
        || path.split('/').any(|c| matches!(c, "fixtures" | "fixture"))
        || bare_literal
        || in_test_region
}

fn secrets(root: &Path, files: &[DiffFile], out: &mut Review) {
    let mut found = 0usize;
    let mut unread = 0usize;
    'files: for (read, file) in files.iter().filter(|f| !f.added.is_empty()).enumerate() {
        if read >= SECRET_FILES_CAP {
            out.caps.push(format!(
                "secret scan capped at {SECRET_FILES_CAP} files; the rest was not scanned"
            ));
            break;
        }
        let Ok(content) = std::fs::read_to_string(root.join(&file.path)) else {
            unread += 1;
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        // Everything after `#[cfg(test)]` is test code.
        let test_region = lines
            .iter()
            .position(|l| l.contains("#[cfg(test)]"))
            .map(|i| i as u32 + 1);
        for &(start, end) in &file.added {
            let last = end.min(lines.len() as u32);
            for number in start.max(1)..=last {
                let line = lines[number as usize - 1];
                let Some((class, strong)) = scan_line_for_secret(line) else {
                    continue;
                };
                let fixture =
                    is_fixture(&file.path, line, test_region.is_some_and(|t| number >= t));
                out.findings.push(Finding {
                    rule: "possible-secret",
                    severity: match (strong, fixture) {
                        (true, false) => Severity::Critical,
                        (true, true) | (false, false) => Severity::Medium,
                        (false, true) => Severity::Low,
                    },
                    file: Some(file.path.clone()),
                    line: Some(number),
                    evidence: format!("added line matches the {class} pattern"),
                    fix_hint: if strong {
                        "confirm this is not a live credential; rotate it if it is"
                    } else {
                        "confirm the assigned value is not a real secret; prefer env or config injection"
                    },
                });
                found += 1;
                if found >= SECRET_CAP {
                    out.caps.push(format!(
                        "secret scan capped at {SECRET_CAP} findings; the rest was not scanned"
                    ));
                    break 'files;
                }
            }
        }
    }
    if unread > 0 {
        out.caps.push(format!(
            "{unread} changed file(s) unreadable; their added lines were not scanned"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::fixture::project;

    fn diff(files: &[(&str, &[(u32, u32)])]) -> Diff {
        Diff {
            files: files
                .iter()
                .map(|(path, hunks)| DiffFile {
                    path: (*path).to_owned(),
                    hunks: hunks.to_vec(),
                    added: hunks.to_vec(),
                })
                .collect(),
        }
    }

    fn rules(review: &Review) -> Vec<&'static str> {
        review.findings.iter().map(|f| f.rule).collect()
    }

    /// Provider-shaped literals are split so external scanners do not read
    /// this table as leaked keys.
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
        ("AKIA appears in this prose", None),
        ("value = \"AKIA1234567890\"", None),
        ("xoxb-1234 = \"ignored\"", Some(("slack-token", true))),
        (
            "PASSWORD = \"hunter2\"",
            Some(("credential-named-assignment", false)),
        ),
        ("fn parse_token(line: &str)", None),
        (
            "let password_len = 8",
            Some(("credential-named-assignment", false)),
        ),
        ("let n = 3;", None),
    ];

    #[test]
    fn each_line_lands_in_its_pattern_class() {
        for (line, expected) in SECRET_CASES {
            assert_eq!(scan_line_for_secret(line), *expected, "{line}");
        }
    }

    #[test]
    fn a_body_edit_to_a_called_function_flags_the_reader_that_was_not_changed() {
        let (repo, store) = project();
        let found = review(&store, repo.path(), &diff(&[("src/a.rs", &[(2, 2)])])).unwrap();
        let divergence: Vec<&str> = found
            .findings
            .iter()
            .filter(|f| f.rule == "producer-reader-divergence")
            .map(|f| f.evidence.as_str())
            .collect();
        assert_eq!(
            divergence,
            ["run changed; go at src/b.rs:5 reads it and was not changed"]
        );
        // `tests/it.rs` reaches `run` through `go`: the symbol is covered.
        assert!(!rules(&found).contains(&"changed-symbol-without-test"));
    }

    #[test]
    fn a_reader_in_the_same_change_is_not_a_divergence() {
        let (repo, store) = project();
        let found = review(
            &store,
            repo.path(),
            &diff(&[("src/a.rs", &[(2, 2)]), ("src/b.rs", &[(5, 5)])]),
        )
        .unwrap();
        // `go`, the reader of `run`, changed too; `go` has its own reader.
        let evidence: Vec<&str> = found
            .findings
            .iter()
            .filter(|f| f.rule == "producer-reader-divergence")
            .map(|f| f.evidence.as_str())
            .collect();
        assert_eq!(
            evidence,
            ["go changed; t at tests/it.rs:4 reads it and was not changed"]
        );
    }

    #[test]
    fn a_changed_function_no_test_reaches_is_a_suggestion() {
        let (repo, store) = project();
        let found = review(&store, repo.path(), &diff(&[("src/d.rs", &[(1, 1)])])).unwrap();
        assert_eq!(rules(&found), ["changed-symbol-without-test"]);
        assert_eq!(found.findings[0].severity, Severity::Medium);
        assert_eq!(found.findings[0].severity.label(), "SUGGESTION");
    }

    #[test]
    fn a_secret_on_an_added_line_is_critical_and_never_echoed() {
        let (repo, store) = project();
        std::fs::write(
            repo.path().join("src/d.rs"),
            "pub fn lonely() {}\nconst KEY: &str = \"ghp_live_token_value\";\n",
        )
        .unwrap();
        let found = review(&store, repo.path(), &diff(&[("src/d.rs", &[(2, 2)])])).unwrap();
        let secret = found
            .findings
            .iter()
            .find(|f| f.rule == "possible-secret")
            .expect("a finding");
        assert_eq!(secret.severity, Severity::Critical);
        assert_eq!(
            (secret.file.as_deref(), secret.line),
            (Some("src/d.rs"), Some(2))
        );
        assert_eq!(
            secret.evidence,
            "added line matches the github-token pattern"
        );
        assert!(!secret.evidence.contains("live_token"));
        // The most severe finding leads the report.
        assert_eq!(found.findings[0].rule, "possible-secret");
    }

    #[test]
    fn the_same_value_in_a_test_file_or_a_deleted_line_drops_a_rung_or_vanishes() {
        let (repo, store) = project();
        std::fs::write(
            repo.path().join("tests/it.rs"),
            "fn t() {\n    let k = \"ghp_fixture\";\n}\n",
        )
        .unwrap();
        let in_test = review(&store, repo.path(), &diff(&[("tests/it.rs", &[(2, 2)])])).unwrap();
        let secret = in_test
            .findings
            .iter()
            .find(|f| f.rule == "possible-secret")
            .unwrap();
        assert_eq!(secret.severity, Severity::Medium);
        // A hunk that adds nothing is not scanned.
        let deleted = Diff {
            files: vec![DiffFile {
                path: "tests/it.rs".to_owned(),
                hunks: vec![(2, 2)],
                added: Vec::new(),
            }],
        };
        let none = review(&store, repo.path(), &deleted).unwrap();
        assert!(!rules(&none).contains(&"possible-secret"));
    }

    #[test]
    fn a_source_file_missing_from_the_index_is_said_to_be() {
        let (repo, store) = project();
        let found = review(&store, repo.path(), &diff(&[("src/new.rs", &[(1, 3)])])).unwrap();
        assert_eq!(rules(&found), ["unindexed-change"]);
    }

    #[test]
    fn severities_order_blocker_first() {
        assert!(Severity::Critical > Severity::High);
        assert!(Severity::High > Severity::Medium);
        assert!(Severity::Medium > Severity::Low);
        assert_eq!(Severity::Critical.label(), "BLOCKER");
        assert_eq!(Severity::High.label(), "CONCERN");
        assert_eq!(Severity::Low.label(), "NIT");
    }
}
