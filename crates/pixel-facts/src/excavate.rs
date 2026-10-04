// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `excavate.rs` — history-wide discovery ("rescue v2"). Returns candidates
//! as (commit, path, hunk span) INCLUDING deleted files (`status='D'` rows
//! carry removed text). `last_good` = newest commit where the path exists with
//! the phrase present. Plans carry `source: "<oid>:<path>"`.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use pixel_git::GitRunner;

use crate::store::{FactsError, FactsStore, IndexState, Result, short_oid, subject_of};
use crate::text_index::{CANDIDATE_CAP, matching_hunks};

/// Diff-content suspect heuristic, shared with `pixel rescue`
/// (`crates/pixel/src/rescue_cmd.rs`): returns the first search unit
/// (phrase/keyword) that is present in `before` but absent from `after` —
/// i.e. the change from `before` to `after` REMOVED phrase-bearing content.
/// `None` means nothing phrase-bearing was removed. Matching is
/// case-insensitive substring (same semantics as `search::relevance_of`,
/// which backs excavate's own per-hunk `suspect` flag). This is the
/// principled replacement for subject-line keyword matching: a commit is
/// suspect because its CONTENT dropped the phrase, regardless of what the
/// commit message says.
pub fn phrase_removed_between(before: &str, after: &str, units: &[String]) -> Option<String> {
    let before_lc = before.to_lowercase();
    let after_lc = after.to_lowercase();
    for u in units {
        if u.is_empty() {
            continue;
        }
        let needle = u.to_lowercase();
        if before_lc.contains(&needle) && !after_lc.contains(&needle) {
            return Some(u.clone());
        }
    }
    None
}

/// Commit-set filter derived from an `[from..to]` rev range. Holds FULL
/// commit oids (as stored in `commits.oid`); candidates are filtered before
/// truncation so a narrowed query still fills up to `limit`.
enum RangeFilter {
    /// Only these commits are allowed (a `--to` bound, with or without
    /// `--from`: ancestors of `to`, minus `from`'s proper ancestors).
    Within(HashSet<String>),
    /// Everything EXCEPT these commits is allowed (a lone `--from` bound:
    /// `from`'s proper ancestors are excluded, `from` itself is included).
    Excluding(HashSet<String>),
}

impl RangeFilter {
    fn allows(&self, oid: &str) -> bool {
        match self {
            RangeFilter::Within(set) => set.contains(oid),
            RangeFilter::Excluding(set) => !set.contains(oid),
        }
    }
}

/// One excavate candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExcavateCandidate {
    pub oid: String,
    pub path: String,
    pub status: String,
    pub at: String,
    pub subject: String,
    pub phrase_present: bool,
    /// True when the path was deleted from HEAD and this commit is a candidate
    /// restore point.
    pub deleted_from_head: bool,
    /// The hunk span text (added/removed) — for `D` rows this is removed text.
    pub span: String,
    /// True when this commit's own diff **removed** phrase-bearing content
    /// (the phrase appears in the hunk's `removed` text). This is
    /// diff-content-overlap detection — it flags the commit that plausibly
    /// broke/deleted the feature by inspecting the actual hunk text, not by
    /// substring-matching the commit subject (the weaker predecessor
    /// heuristic in `pixel/src/rescue_cmd.rs`). A commit can be `suspect` even
    /// when its subject line never mentions the phrase at all.
    pub suspect: bool,
    /// Inline recovery payload: the matching hunk's stored text (the
    /// REMOVED side when this commit removed the phrase — a deletion's
    /// pre-deletion code — otherwise the ADDED side), centered on the
    /// phrase match and capped at `SNIPPET_MAX_LINES` lines /
    /// `SNIPPET_MAX_BYTES` bytes. Only the top `SNIPPET_TOP_N` candidates
    /// carry one (see `ExcavateResult::snippet_note`); use
    /// `excavate --show <oid> --file <path>` for the full file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Internal recency tiebreak: `commits.id`, which increases with
    /// insertion order (oldest-first per `enumerate_all_commits`). `at`
    /// (`committed_at`) only has whole-second precision from git, so two
    /// commits made within the same second — routine for scripted fixtures,
    /// rebases, and squash workflows — tie on `at` alone; comparing this
    /// field breaks the tie by real chronological/topological order instead
    /// of by incidental SQL row order. Not part of the wire contract.
    #[serde(default, skip_serializing)]
    seq: i64,
    /// True when the phrase appears on at least one non-comment line of the
    /// matched side — i.e. this looks like the actual declaration/usage, not
    /// a doc comment or prose mention of the same identifier. Ranked above
    /// comment-only matches within the same suspect/recency tier: two files
    /// touched by the identical commit otherwise tie on every other sort
    /// key, and a real-world case showed that tie landing on a doc-comment
    /// reference (`/// ...install::register_mcp_server`) instead of the
    /// actual `pub fn register_mcp_server(...)` it was describing — forcing
    /// the caller to dig through several more candidates for the real
    /// answer to "find the deleted function".
    pub is_definition: bool,
}

/// Single-line-comment prefixes checked when distinguishing a real
/// declaration/usage from a doc comment or prose mention. Deliberately
/// covers common line-comment styles across languages this tool indexes
/// (`//`/`///`/`//!` for Rust/JS/C-family, `#` for Python/shell/Ruby) rather
/// than only Rust's, since excavate has no language boundary.
const COMMENT_LINE_PREFIXES: &[&str] = &["///", "//!", "//", "#"];

fn phrase_outside_comment(text: &str, phrase: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        let is_comment = COMMENT_LINE_PREFIXES.iter().any(|p| trimmed.starts_with(p));
        !is_comment && line.contains(phrase)
    })
}

/// The result of an excavate query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExcavateResult {
    pub phrase: String,
    pub path: Option<String>,
    pub candidates: Vec<ExcavateCandidate>,
    pub last_good: Option<ExcavateCandidate>,
    /// Rescue plan sources: `"<oid>:<path>"` restorable even when path ∉ HEAD.
    pub plan: Vec<String>,
    /// Present when some candidates' snippets were withheld to stay inside
    /// the response size cap — those candidates are metadata-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet_note: Option<String>,
    /// The one-call follow-up: how to read FULL historical file content
    /// without falling back to raw `git show`/`git log`.
    #[serde(default)]
    pub next: String,
    /// How much of history the answer covers — callers MUST read `fresh`
    /// before treating an empty candidate list as "the code never existed".
    pub index_state: IndexState,
}

/// Per-candidate snippet caps and the top-N / whole-response budget.
const SNIPPET_MAX_LINES: usize = 60;
const SNIPPET_MAX_BYTES: usize = 6 * 1024;
const SNIPPET_TOP_N: usize = 5;
const SNIPPET_TOTAL_BUDGET: usize = 40 * 1024;

impl FactsStore {
    /// History-wide discovery. `phrase` may be empty (list by path/time), in
    /// which case every touched path's history is returned. When `phrase` is
    /// given, candidates are those hunks whose added/removed text contains the
    /// phrase (verified), including deleted files.
    pub fn excavate(
        &self,
        phrase: Option<&str>,
        path: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
        limit: usize,
    ) -> Result<ExcavateResult> {
        let phrase = phrase.unwrap_or("").to_string();
        let limit = limit.min(200);

        // Resolve `[from..to]` into a concrete commit filter BEFORE querying.
        // An unresolvable ref is a structured error, never a silently
        // unfiltered answer (the flags used to be accepted and discarded).
        let range = self.resolve_range(from, to)?;
        let range = range.as_ref();

        let mut candidates: Vec<ExcavateCandidate> = if !phrase.is_empty() {
            self.excavate_by_phrase(&phrase, path, range, limit)?
        } else if let Some(p) = path {
            self.excavate_by_path(p, range, limit)?
        } else {
            // No phrase and no path: list all changed paths (most recent first).
            self.excavate_recent(range, limit)?
        };

        // last_good = newest commit where the path existed WITH the phrase
        // present, per PLAN.md's Engine-2 spec. Deliberately does NOT require
        // the path to still exist at HEAD: that is exactly backwards for the
        // "restore a file deleted from HEAD" scenario (excavate's whole
        // reason to exist) — every historical row for a since-deleted path
        // would otherwise be permanently excluded from ever being the
        // recommended restore point. `phrase_present` already encodes
        // "the phrase was present in the tree right after this commit" (see
        // `excavate_by_phrase`), so a simple newest-first scan over it is
        // correct whether or not the path survives to HEAD.
        let mut last_good: Option<ExcavateCandidate> = None;
        for c in candidates.iter() {
            if c.phrase_present {
                if let Some(lg) = &last_good {
                    // Compare (committed_at, commit insertion order) so two
                    // commits sharing the same whole-second timestamp still
                    // resolve to the true newer one instead of whichever
                    // happened to be visited first.
                    if (&c.at, c.seq) > (&lg.at, lg.seq) {
                        last_good = Some(c.clone());
                    }
                } else {
                    last_good = Some(c.clone());
                }
            }
        }

        // Snippet budget: candidates are sorted newest-first, so keep the
        // inline payload on the top SNIPPET_TOP_N (while the running total
        // stays inside SNIPPET_TOTAL_BUDGET) and strip the rest to
        // metadata-only. `last_good` was cloned above, BEFORE stripping, so
        // the recommended restore point always keeps its snippet even when
        // it ranks below the top N.
        let mut total = 0usize;
        let mut stripped = 0usize;
        for (i, c) in candidates.iter_mut().enumerate() {
            match c.snippet.as_ref().map(String::len) {
                Some(len) if i < SNIPPET_TOP_N && total + len <= SNIPPET_TOTAL_BUDGET => {
                    total += len;
                }
                Some(_) => {
                    c.snippet = None;
                    stripped += 1;
                }
                None => {}
            }
        }
        let snippet_note = (stripped > 0).then(|| {
            format!(
                "{stripped} candidate(s) are metadata-only (snippets carry only the \
                 top {SNIPPET_TOP_N} matches, {} KB total); run \
                 `pixel dig-history --show <oid> --file <path>` for any of them",
                SNIPPET_TOTAL_BUDGET / 1024
            )
        });

        let plan: Vec<String> = candidates
            .iter()
            .map(|c| format!("{}:{}", c.oid, c.path))
            .collect();

        // Tell the calling agent the follow-up is ONE pixel call — not a
        // round of raw `git show`/`git log`.
        let next = match &last_good {
            Some(lg) => format!(
                "full original file: `pixel dig-history --show {} --file {}` \
                 (reads <oid>:<path>; on a deletion commit the parent's \
                 pre-deletion content is returned automatically, or pass \
                 --parent). No `git show` needed.",
                lg.oid, lg.path
            ),
            None => "full historical file content: `pixel dig-history --show <oid> \
                     --file <path>` (parent fallback for deletion commits; \
                     --parent forces <oid>^). No `git show` needed."
                .to_string(),
        };

        Ok(ExcavateResult {
            phrase,
            path: path.map(ToString::to_string),
            candidates,
            last_good,
            plan,
            snippet_note,
            next,
            index_state: self.index_state(),
        })
    }

    /// Resolve `--from`/`--to` refs into a [`RangeFilter`] over full commit
    /// oids, via the real git plumbing (`rev-parse --verify` + `rev-list`).
    /// Bounds are INCLUSIVE on both ends: `from` = older bound, `to` = newer
    /// bound, either may be absent. A ref that does not resolve to a commit
    /// is a structured `FactsError::Msg` — never silence.
    fn resolve_range(&self, from: Option<&str>, to: Option<&str>) -> Result<Option<RangeFilter>> {
        if from.is_none() && to.is_none() {
            return Ok(None);
        }
        let runner = GitRunner::new(self.root());
        let resolve = |label: &str, r: &str| -> Result<String> {
            pixel_git::validate_ref(r)
                .map_err(|e| FactsError::Msg(format!("invalid --{label} ref {r:?}: {e}")))?;
            let spec = format!("{r}^{{commit}}");
            let out = runner
                .run(&[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    &spec,
                ])
                .map_err(|_| {
                    FactsError::Msg(format!(
                        "--{label} ref {r:?} does not resolve to a commit in this repository"
                    ))
                })?;
            Ok(String::from_utf8_lossy(&out).trim().to_string())
        };
        let rev_list = |args: &[&str]| -> Result<HashSet<String>> {
            let out = runner.run(args).map_err(|e| {
                FactsError::Msg(format!(
                    "git rev-list failed while resolving --from/--to: {e}"
                ))
            })?;
            Ok(String::from_utf8_lossy(&out)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect())
        };
        match (from, to) {
            (Some(f), Some(t)) => {
                let f_oid = resolve("from", f)?;
                let t_oid = resolve("to", t)?;
                let mut set = rev_list(&["rev-list", &format!("{f_oid}..{t_oid}")])?;
                // Git's `a..b` excludes `a`; the CLI contract is inclusive
                // of the older bound.
                set.insert(f_oid);
                Ok(Some(RangeFilter::Within(set)))
            }
            (None, Some(t)) => {
                let t_oid = resolve("to", t)?;
                Ok(Some(RangeFilter::Within(rev_list(&["rev-list", &t_oid])?)))
            }
            (Some(f), None) => {
                let f_oid = resolve("from", f)?;
                // Exclude `from`'s PROPER ancestors; `from` itself stays in.
                let mut ancestors = rev_list(&["rev-list", &f_oid])?;
                ancestors.remove(&f_oid);
                Ok(Some(RangeFilter::Excluding(ancestors)))
            }
            (None, None) => unreachable!("early-returned above"),
        }
    }

    fn excavate_by_phrase(
        &self,
        phrase: &str,
        path: Option<&str>,
        range: Option<&RangeFilter>,
        limit: usize,
    ) -> Result<Vec<ExcavateCandidate>> {
        let units = vec![phrase.to_string()];
        let Some(ids) = matching_hunks(self.conn(), &units, CANDIDATE_CAP)? else {
            return Ok(Vec::new());
        };
        let deleted = self.paths_deleted_from_head()?;
        let mut out: Vec<ExcavateCandidate> = Vec::new();
        for id in ids {
            // Join `file_changes` for the REAL per-commit status (A/M/D) of
            // this (commit, path) pair. Previously this derived a blanket
            // status from "is this path ever deleted from HEAD" — which
            // mislabeled every add/modify commit for a since-deleted path as
            // `status:"D"` and, combined with the old `last_good` filter,
            // meant a currently-deleted file could NEVER produce a
            // `last_good` candidate at all (the exact dropped-file restore
            // case excavate exists to serve). `file_changes` is UNIQUE on
            // (commit_id, path), so this join is exact, not a guess.
            #[allow(clippy::type_complexity)]
            let row: Option<(
                String,
                String,
                String,
                String,
                String,
                String,
                String,
                String,
                i64,
            )> = self
                .conn
                .query_row(
                    "SELECT c.oid, c.committed_at, c.author, c.message,
                                h.path, h.added, h.removed, fc.status, c.id
                         FROM hunks h
                         JOIN commits c ON c.id = h.commit_id
                         LEFT JOIN file_changes fc
                                ON fc.commit_id = h.commit_id AND fc.path = h.path
                         WHERE h.id = ?1",
                    [id],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get::<_, Option<String>>(7)?
                                .unwrap_or_else(|| "M".to_string()),
                            r.get(8)?,
                        ))
                    },
                )
                .ok();
            if let Some((oid, at, _author, message, hpath, added, removed, status, seq)) = row {
                if let Some(rf) = range {
                    // `commits.oid` is the full oid — filter BEFORE the
                    // sort/truncate below so narrowing still fills `limit`.
                    if !rf.allows(&oid) {
                        continue;
                    }
                }
                if let Some(p) = path
                    && hpath != p
                {
                    continue;
                }
                let text = format!("{added}\n{removed}");
                let rel = crate::search::relevance_of(&text, &units);
                if rel == 0 {
                    continue;
                }
                let deleted_from_head = deleted.contains(&hpath);
                // "Present after this commit" = the phrase shows up on the
                // ADD side of this commit's diff — i.e. the resulting blob
                // right after this commit contains it. A pure-removal commit
                // (status D, or a modify that drops the phrase without
                // re-adding it) leaves `added` empty/phrase-free, so
                // `phrase_present` is correctly false there and such a
                // commit can never win `last_good`.
                let added_has_phrase = crate::search::relevance_of(&added, &units) > 0;
                let removed_has_phrase = crate::search::relevance_of(&removed, &units) > 0;
                let phrase_present = added_has_phrase;
                // Diff-content-overlap suspect detection: this commit is
                // "suspect" when its own hunk *removed* phrase-bearing text
                // and did NOT re-add it in the same commit — i.e. the diff
                // itself shows the phrase disappearing here, independent of
                // what the commit subject says (the weaker predecessor
                // heuristic in `pixel/src/rescue_cmd.rs` only ever looked at
                // the subject line). A modify that removes-then-re-adds the
                // same phrase (e.g. reformatting the line it lives on) is
                // correctly NOT suspect.
                let suspect = removed_has_phrase && !added_has_phrase;
                // Inline snippet side: a phrase-removing commit's payload is
                // the REMOVED text (the code the user wants back); otherwise
                // the ADDED text (the code as it landed). Falls back to the
                // combined hunk text when neither side matched individually
                // (can't happen for a verified candidate, but stay total).
                let side = if removed_has_phrase && !added_has_phrase {
                    &removed
                } else if added_has_phrase {
                    &added
                } else {
                    &text
                };
                let snip = snippet_block(side, phrase);
                let is_definition = phrase_outside_comment(side, phrase);
                out.push(ExcavateCandidate {
                    oid: short_oid(&oid),
                    path: hpath,
                    status,
                    at,
                    subject: subject_of(&message).to_string(),
                    phrase_present,
                    deleted_from_head,
                    span: snippet(&text, phrase),
                    suspect,
                    is_definition,
                    snippet: (!snip.is_empty()).then_some(snip),
                    seq,
                });
            }
        }
        // Suspect commits (diff-content proof the phrase was REMOVED here and
        // not re-added) rank first — that's precisely what "find the deleted
        // X" is asking for. Without this, a suspect buried under more-recent
        // non-suspect noise (e.g. another file's prose mentioning the same
        // identifier as plain text) forces the caller to read past false
        // leads before reaching the deterministic answer this field already
        // computed. `is_definition` breaks ties WITHIN a commit: one commit
        // routinely touches several files, and without this a real function
        // definition can tie on (suspect, at, seq) against a doc comment in
        // another file that merely mentions the same identifier, with the
        // loser decided by incidental SQL row order — measured: a "find the
        // deleted function" query surfaced a `/// ...install::X` doc-comment
        // reference ahead of the actual `pub fn X(...)` it described.
        // Recency remains the final tiebreaker within each (suspect,
        // is_definition) group.
        out.sort_by(|a, b| {
            (b.suspect, b.is_definition, &b.at, b.seq).cmp(&(
                a.suspect,
                a.is_definition,
                &a.at,
                a.seq,
            ))
        });
        out.truncate(limit);
        Ok(out)
    }

    fn excavate_by_path(
        &self,
        path: &str,
        range: Option<&RangeFilter>,
        limit: usize,
    ) -> Result<Vec<ExcavateCandidate>> {
        let deleted = self.paths_deleted_from_head()?;
        let deleted_from_head = deleted.iter().any(|d| d == path);
        // With a range filter, the SQL LIMIT must not pre-truncate rows the
        // filter would keep (`LIMIT -1` = unlimited in SQLite); truncation
        // happens after filtering instead.
        let sql_limit: i64 = if range.is_some() { -1 } else { limit as i64 };
        let mut stmt = self.conn().prepare(
            "SELECT c.oid, c.committed_at, c.message, f.status, f.path, c.id
             FROM file_changes f JOIN commits c ON c.id = f.commit_id
             WHERE f.path = ?1 OR f.old_path = ?1
             ORDER BY c.committed_at DESC, c.id DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![path, sql_limit], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (oid, at, message, status, p, seq) = row?;
            if let Some(rf) = range
                && !rf.allows(&oid)
            {
                continue;
            }
            out.push(ExcavateCandidate {
                oid: short_oid(&oid),
                path: p,
                status,
                at,
                subject: subject_of(&message).to_string(),
                phrase_present: true,
                deleted_from_head,
                span: String::new(),
                // No phrase given for a path-only query, so diff-overlap
                // suspect detection and definition-vs-mention detection have
                // nothing to check against.
                suspect: false,
                is_definition: false,
                snippet: None,
                seq,
            });
        }
        out.truncate(limit);
        Ok(out)
    }

    fn excavate_recent(
        &self,
        range: Option<&RangeFilter>,
        limit: usize,
    ) -> Result<Vec<ExcavateCandidate>> {
        let deleted = self.paths_deleted_from_head()?;
        let sql_limit: i64 = if range.is_some() { -1 } else { limit as i64 };
        let mut stmt = self.conn().prepare(
            "SELECT c.oid, c.committed_at, c.message, f.status, f.path, c.id
             FROM file_changes f JOIN commits c ON c.id = f.commit_id
             ORDER BY c.committed_at DESC, c.id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([sql_limit], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (oid, at, message, status, p, seq) = row?;
            if let Some(rf) = range
                && !rf.allows(&oid)
            {
                continue;
            }
            out.push(ExcavateCandidate {
                oid: short_oid(&oid),
                path: p.clone(),
                status,
                at,
                subject: subject_of(&message).to_string(),
                phrase_present: false,
                deleted_from_head: deleted.contains(&p),
                span: String::new(),
                suspect: false,
                is_definition: false,
                snippet: None,
                seq,
            });
        }
        out.truncate(limit);
        Ok(out)
    }

    /// Paths that are deleted from HEAD (their latest file_changes status is D
    /// and they are not present at HEAD).
    fn paths_deleted_from_head(&self) -> Result<Vec<String>> {
        // A path is "deleted from HEAD" iff its MOST RECENT file_changes row
        // (by `committed_at`, the recency field this module uses everywhere
        // else — see `excavate_by_path`/`excavate_recent`'s `ORDER BY
        // c.committed_at DESC`) has status 'D'.
        //
        // The previous query required a path to have a 'D' row AND to have
        // NEVER had an 'A' or 'M' row at all. That is backwards for every
        // realistically-lifecycled file: add -> modify* -> delete always
        // leaves prior 'A'/'M' rows for the same path, so the `NOT EXISTS`
        // clause excluded it and `paths_deleted_from_head` returned the
        // empty set for the normal case. Since `deleted_from_head` (and,
        // before the `last_good` fix above, `last_good` itself) depend on
        // this function, that bug silently defeated excavate's entire
        // reason to exist: restoring a file that was actually deleted.
        let mut stmt = self.conn().prepare(
            "SELECT DISTINCT f.path FROM file_changes f
             JOIN commits c ON c.id = f.commit_id
             WHERE f.status = 'D'
             AND c.committed_at = (
                 SELECT MAX(c2.committed_at)
                 FROM file_changes f2
                 JOIN commits c2 ON c2.id = f2.commit_id
                 WHERE f2.path = f.path
             )",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut v = Vec::new();
        for row in rows {
            v.push(row?);
        }
        Ok(v)
    }
}

/// Line-oriented inline snippet: up to `SNIPPET_MAX_LINES` whole lines of
/// `text` centered on the first (case-insensitive) occurrence of `needle`,
/// additionally capped at `SNIPPET_MAX_BYTES`. Elided ends are marked `…`.
/// Unlike `snippet` (the short one-line `span` teaser), this carries enough
/// of the hunk to hand back a full function body without a follow-up call.
fn snippet_block(text: &str, needle: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let hit_line = {
        let lower = text.to_lowercase();
        let n = needle.to_lowercase();
        match lower.find(&n) {
            Some(pos) => text[..pos].matches('\n').count().min(lines.len() - 1),
            None => 0,
        }
    };
    // Center the window on the hit, clamped to the text bounds.
    let start = hit_line
        .saturating_sub(SNIPPET_MAX_LINES / 2)
        .min(lines.len().saturating_sub(SNIPPET_MAX_LINES));
    let end = (start + SNIPPET_MAX_LINES).min(lines.len());
    let mut out = String::new();
    if start > 0 {
        out.push_str("…\n");
    }
    let mut truncated_by_bytes = false;
    for l in &lines[start..end] {
        if out.len() + l.len() + 1 > SNIPPET_MAX_BYTES {
            truncated_by_bytes = true;
            break;
        }
        out.push_str(l);
        out.push('\n');
    }
    if truncated_by_bytes || end < lines.len() {
        out.push('…');
    }
    out
}

fn snippet(text: &str, needle: &str) -> String {
    let lower = text.to_lowercase();
    let n = needle.to_lowercase();
    match lower.find(&n) {
        Some(pos) => {
            let s = pos.saturating_sub(20);
            let e = (pos + 120).min(text.len());
            let mut out = String::new();
            if s > 0 {
                out.push('…');
            }
            out.push_str(&text[s..e]);
            if e < text.len() {
                out.push('…');
            }
            out
        }
        None => text.chars().take(120).collect(),
    }
}
