//! `excavate.rs` — history-wide discovery ("rescue v2"). Returns candidates
//! as (commit, path, hunk span) INCLUDING deleted files (`status='D'` rows
//! carry removed text). `last_good` = newest commit where the path exists with
//! the phrase present. Plans carry `source: "<oid>:<path>"`.

use serde::{Deserialize, Serialize};

use crate::search::covering_hashes;
use crate::store::{FactsStore, IndexState, Result, short_oid, subject_of};

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

        let mut candidates: Vec<ExcavateCandidate> = if !phrase.is_empty() {
            self.excavate_by_phrase(&phrase, path, from, to, limit)?
        } else if let Some(p) = path {
            self.excavate_by_path(p, from, to, limit)?
        } else {
            // No phrase and no path: list all changed paths (most recent first).
            self.excavate_recent(from, to, limit)?
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
                 `pixel excavate --show <oid> --file <path>` for any of them",
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
                "full original file: `pixel excavate --show {} --file {}` \
                 (reads <oid>:<path>; on a deletion commit the parent's \
                 pre-deletion content is returned automatically, or pass \
                 --parent). No `git show` needed.",
                lg.oid, lg.path
            ),
            None => "full historical file content: `pixel excavate --show <oid> \
                     --file <path>` (parent fallback for deletion commits; \
                     --parent forces <oid>^). No `git show` needed."
                .to_string(),
        };

        Ok(ExcavateResult {
            phrase,
            path: path.map(|p| p.to_string()),
            candidates,
            last_good,
            plan,
            snippet_note,
            next,
            index_state: self.index_state(),
        })
    }

    fn excavate_by_phrase(
        &self,
        phrase: &str,
        path: Option<&str>,
        _from: Option<&str>,
        _to: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ExcavateCandidate>> {
        let units = vec![phrase.to_string()];
        let hashes = covering_hashes(&units);
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let deleted = self.paths_deleted_from_head()?;
        let placeholders = hashes.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        // DISTINCT: a single hunk's text commonly contains several matching
        // trigrams (e.g. "secret_token" alone covers multiple 3-byte grams),
        // so without it the same hunk_id repeats once per matching gram and
        // every downstream candidate/plan entry is duplicated accordingly.
        // search.rs's equivalent queries (`path_search`, `diff_search`) already
        // use DISTINCT for the same reason.
        let sql = format!(
            "SELECT DISTINCT hunk_id FROM diff_grams WHERE hash IN ({placeholders})"
        );
        let ids: Vec<i64> = {
            let mut stmt = self.conn().prepare(&sql)?;
            let mut q = stmt.query(rusqlite::params_from_iter(hashes.iter().map(|h| *h as i64)))?;
            let mut v = Vec::new();
            while let Some(r) = q.next()? {
                v.push(r.get::<_, i64>(0)?);
            }
            v
        };
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
            let row: Option<(String, String, String, String, String, String, String, String, i64)> =
                self.conn
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
                                r.get::<_, Option<String>>(7)?.unwrap_or_else(|| "M".to_string()),
                                r.get(8)?,
                            ))
                        },
                    )
                    .ok();
            if let Some((oid, at, _author, message, hpath, added, removed, status, seq)) = row {
                if let Some(p) = path {
                    if hpath != p {
                        continue;
                    }
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
                    snippet: (!snip.is_empty()).then_some(snip),
                    seq,
                });
            }
        }
        out.sort_by(|a, b| (&b.at, b.seq).cmp(&(&a.at, a.seq)));
        out.truncate(limit);
        Ok(out)
    }

    fn excavate_by_path(
        &self,
        path: &str,
        _from: Option<&str>,
        _to: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ExcavateCandidate>> {
        let deleted = self.paths_deleted_from_head()?;
        let deleted_from_head = deleted.iter().any(|d| d == path);
        let mut stmt = self.conn().prepare(
            "SELECT c.oid, c.committed_at, c.message, f.status, f.path, c.id
             FROM file_changes f JOIN commits c ON c.id = f.commit_id
             WHERE f.path = ?1 OR f.old_path = ?1
             ORDER BY c.committed_at DESC, c.id DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![path, limit as i64], |r| {
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
                // suspect detection has nothing to check against.
                suspect: false,
                snippet: None,
                seq,
            });
        }
        Ok(out)
    }

    fn excavate_recent(
        &self,
        _from: Option<&str>,
        _to: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ExcavateCandidate>> {
        let deleted = self.paths_deleted_from_head()?;
        let mut stmt = self.conn().prepare(
            "SELECT c.oid, c.committed_at, c.message, f.status, f.path, c.id
             FROM file_changes f JOIN commits c ON c.id = f.commit_id
             ORDER BY c.committed_at DESC, c.id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
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
                snippet: None,
                seq,
            });
        }
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
