//! `excavate.rs` — history-wide discovery ("rescue v2"). Returns candidates
//! as (commit, path, hunk span) INCLUDING deleted files (`status='D'` rows
//! carry removed text). `last_good` = newest commit where the path exists with
//! the phrase present. Plans carry `source: "<oid>:<path>"`.

use serde::{Deserialize, Serialize};

use crate::search::covering_hashes;
use crate::store::{FactsStore, Result, short_oid, subject_of};

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
}

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
        let mut candidates: Vec<ExcavateCandidate> = Vec::new();

        if !phrase.is_empty() {
            candidates = self.excavate_by_phrase(&phrase, path, from, to, limit)?;
        } else if let Some(p) = path {
            candidates = self.excavate_by_path(p, from, to, limit)?;
        } else {
            // No phrase and no path: list all changed paths (most recent first).
            candidates = self.excavate_recent(from, to, limit)?;
        }

        // last_good = newest candidate whose path exists at HEAD with phrase.
        let deleted_paths = self.paths_deleted_from_head()?;
        let mut last_good: Option<ExcavateCandidate> = None;
        for c in candidates.iter() {
            if !deleted_paths.contains(&c.path) && c.phrase_present {
                if let Some(lg) = &last_good {
                    if c.at > lg.at {
                        last_good = Some(c.clone());
                    }
                } else {
                    last_good = Some(c.clone());
                }
            }
        }

        let plan: Vec<String> = candidates
            .iter()
            .map(|c| format!("{}:{}", c.oid, c.path))
            .collect();

        Ok(ExcavateResult {
            phrase,
            path: path.map(|p| p.to_string()),
            candidates,
            last_good,
            plan,
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
        let sql = format!(
            "SELECT h.id FROM diff_grams WHERE hash IN ({placeholders})"
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
            let row: Option<(String, String, String, String, String, String, String)> = self
                .conn
                .query_row(
                    "SELECT c.oid, c.committed_at, c.author, c.message,
                            h.path, h.added, h.removed
                     FROM hunks h JOIN commits c ON c.id = h.commit_id
                     WHERE h.id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
                )
                .ok();
            if let Some((oid, at, _author, message, hpath, added, removed)) = row {
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
                // status = D if this path's only recent history is a removal.
                let status = if deleted_from_head { "D" } else { "M" };
                out.push(ExcavateCandidate {
                    oid: short_oid(&oid),
                    path: hpath,
                    status: status.to_string(),
                    at,
                    subject: subject_of(&message).to_string(),
                    phrase_present: true,
                    deleted_from_head,
                    span: snippet(&text, phrase),
                });
            }
        }
        out.sort_by(|a, b| b.at.cmp(&a.at));
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
            "SELECT c.oid, c.committed_at, c.message, f.status, f.path
             FROM file_changes f JOIN commits c ON c.id = f.commit_id
             WHERE f.path = ?1 OR f.old_path = ?1
             ORDER BY c.committed_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![path, limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (oid, at, message, status, p) = row?;
            out.push(ExcavateCandidate {
                oid: short_oid(&oid),
                path: p,
                status,
                at,
                subject: subject_of(&message).to_string(),
                phrase_present: true,
                deleted_from_head,
                span: String::new(),
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
            "SELECT c.oid, c.committed_at, c.message, f.status, f.path
             FROM file_changes f JOIN commits c ON c.id = f.commit_id
             ORDER BY c.committed_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (oid, at, message, status, p) = row?;
            out.push(ExcavateCandidate {
                oid: short_oid(&oid),
                path: p.clone(),
                status,
                at,
                subject: subject_of(&message).to_string(),
                phrase_present: false,
                deleted_from_head: deleted.contains(&p),
                span: String::new(),
            });
        }
        Ok(out)
    }

    /// Paths that are deleted from HEAD (their latest file_changes status is D
    /// and they are not present at HEAD).
    fn paths_deleted_from_head(&self) -> Result<Vec<String>> {
        // A path is "deleted from HEAD" if it does not exist at HEAD.
        let mut stmt = self.conn().prepare(
            "SELECT DISTINCT f.path FROM file_changes f
             WHERE f.status = 'D'
             AND NOT EXISTS (SELECT 1 FROM file_changes f2
                             WHERE f2.path = f.path AND f2.status IN ('A','M'))",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut v = Vec::new();
        for row in rows {
            v.push(row?);
        }
        Ok(v)
    }
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
