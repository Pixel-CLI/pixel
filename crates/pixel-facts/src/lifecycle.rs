// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! `lifecycle.rs` — lifecycle of a path or token: first-seen, last-changed,
//! removed-in, present-at-HEAD.
//!
//! Path lifecycle reads `file_changes` directly. Token lifecycle reads verified
//! diff hunks (added/removed) via the trigram index (`text_index`), so it only
//! sees the commits whose diff is indexed and says how much it could not see
//! ([`DiffCoverage`]).

use serde::{Deserialize, Serialize};

use crate::search::relevance_of;
use crate::store::{
    CommitRef, DIFF_STATE_EVICTED, DIFF_STATE_PENDING, FactsStore, Result, subject_of,
};
use crate::text_index::matching_hunks;

/// A lifecycle summary for a path or token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lifecycle {
    pub what: String,
    pub first_seen: Option<CommitRef>,
    pub last_changed: Option<CommitRef>,
    pub removed_in: Option<CommitRef>,
    pub present_at_head: bool,
    pub total_touches: u64,
    /// How much of the diff history a token lifecycle was computed from.
    /// `None` for a path lifecycle: it reads `file_changes`, commit
    /// metadata that eviction never drops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<DiffCoverage>,
}

/// The diff history a token answer could not see: commits whose diff is not
/// indexed, because they are older than the window, did not fit the size
/// budget, or are not ingested yet. Skipped commits (merges, poisoned paths)
/// are left out by design, as for every diff search, and are not counted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffCoverage {
    /// Some commit has no indexed diff: the token may have touches the
    /// answer does not count, so `total_touches` is a lower bound.
    pub lower_bound: bool,
    /// Every commit authored up to `first_seen` (up to now when the token was
    /// not found) has its diff indexed, so no older touch can be missing.
    pub first_seen_exact: bool,
    /// Commits with no indexed diff, in the whole history.
    pub unindexed_commits: u64,
    /// Of those, the ones authored at or before `first_seen` (all of them
    /// when the token was not found). A commit date that does not parse
    /// counts here: its order is unknown.
    pub unindexed_before_first_seen: u64,
    /// The sentence to read before quoting the answer; `None` when nothing
    /// was missing.
    pub note: Option<String>,
}

impl DiffCoverage {
    /// Coverage of a token answer from the two counts; `found` tells a
    /// lifecycle with a `first_seen` from a token no indexed diff touches.
    pub fn from_counts(token: &str, found: bool, unindexed: u64, before: u64) -> Self {
        let check = format!(
            "Check the full history with: git log --reverse -S {}",
            shell_quote(token)
        );
        let note = if unindexed == 0 {
            None
        } else if !found {
            Some(format!(
                "No indexed diff touches the token, but {unindexed} commit(s) have no indexed \
                 diff (older than the history window, over the size budget, or not ingested \
                 yet): it may appear there. {check}"
            ))
        } else if before > 0 {
            Some(format!(
                "first_seen is the oldest touch among indexed diffs only: {before} commit(s) \
                 authored at or before it have no indexed diff (older than the history window, \
                 over the size budget, or not ingested yet), so the token may be older and \
                 total_touches may be short. {check}"
            ))
        } else {
            Some(format!(
                "first_seen is exact: every commit up to it has its diff indexed. {unindexed} \
                 newer commit(s) have no indexed diff yet, so last_changed and total_touches \
                 may lag."
            ))
        };
        DiffCoverage {
            lower_bound: unindexed > 0,
            first_seen_exact: before == 0,
            unindexed_commits: unindexed,
            unindexed_before_first_seen: before,
            note,
        }
    }
}

/// `text` as one POSIX shell word, so the suggested command pastes as is.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn reference(oid: &str, at: &str, message: &str) -> CommitRef {
    CommitRef {
        oid: crate::store::short_oid(oid),
        at: at.to_string(),
        subject: subject_of(message).to_string(),
    }
}

impl FactsStore {
    /// Coverage of a token answer whose oldest touch was authored at
    /// `first_seen_at` (`None`: the token was not found).
    pub fn token_coverage(&self, token: &str, first_seen_at: Option<&str>) -> Result<DiffCoverage> {
        let (unindexed, before): (i64, i64) = self.conn().query_row(
            "SELECT count(*),
                    coalesce(sum(unixepoch(committed_at) IS NULL
                                 OR unixepoch(?3) IS NULL
                                 OR unixepoch(committed_at) <= unixepoch(?3)), 0)
             FROM commits WHERE diff_state IN (?1, ?2)",
            rusqlite::params![DIFF_STATE_PENDING, DIFF_STATE_EVICTED, first_seen_at],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(DiffCoverage::from_counts(
            token,
            first_seen_at.is_some(),
            u64::try_from(unindexed).unwrap_or(0),
            u64::try_from(before).unwrap_or(0),
        ))
    }

    /// Lifecycle of a path: first-seen / last-changed / removed-in / present.
    /// Complete once phase A is: it reads commit metadata, never diff text,
    /// so it carries no [`DiffCoverage`].
    pub fn path_lifecycle(&self, path: &str) -> Result<Option<Lifecycle>> {
        let mut stmt = self.conn().prepare(
            "SELECT c.oid, c.committed_at, c.message, f.status
             FROM file_changes f
             JOIN commits c ON c.id = f.commit_id
             WHERE f.path = ?1 OR f.old_path = ?1
             ORDER BY c.committed_at, c.id",
        )?;
        let rows = stmt.query_map([path], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut touches: Vec<(String, String, String, String)> = Vec::new();
        for row in rows {
            touches.push(row?);
        }
        if touches.is_empty() {
            return Ok(None);
        }
        let first = touches.first().cloned();
        let last = touches.last().cloned();
        // removed-in = the newest touch whose status is D AND it is also the
        // last touch overall.
        let mut removed_in: Option<(String, String, String)> = None;
        if let Some(last) = &last
            && last.3 == "D"
        {
            removed_in = Some((last.0.clone(), last.1.clone(), last.2.clone()));
        }
        // present-at-head: check whether the blob exists at HEAD.
        let present = self
            .runner()
            .run(&["cat-file", "-e", &format!("HEAD:{path}")])
            .is_ok();
        Ok(Some(Lifecycle {
            what: path.to_string(),
            first_seen: first.map(|(o, a, m, _)| reference(&o, &a, &m)),
            last_changed: last.map(|(o, a, m, _)| reference(&o, &a, &m)),
            removed_in: removed_in.map(|(o, a, m)| reference(&o, &a, &m)),
            present_at_head: present,
            total_touches: touches.len() as u64,
            coverage: None,
        }))
    }

    /// Lifecycle of a token (substring): uses verified diff hunks.
    pub fn token_lifecycle(&self, token: &str) -> Result<Option<Lifecycle>> {
        let units = vec![token.to_string()];
        // Candidate hunks via trigrams, verified against text. Every
        // candidate: first-seen needs the oldest touch, not the newest few.
        let Some(ids) = matching_hunks(self.conn(), &units, usize::MAX)? else {
            // Token too short for trigram; fall back to a direct scan.
            return self.token_lifecycle_scan(token);
        };
        let mut rows: Vec<(String, String, String)> = Vec::new();
        for id in ids {
            if let Ok((oid, at, msg, added, removed)) = self.conn.query_row(
                "SELECT c.oid, c.committed_at, c.message, h.added, h.removed
                     FROM hunks h JOIN commits c ON c.id = h.commit_id
                     WHERE h.id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            ) {
                let text = format!("{added}\n{removed}");
                let rel = relevance_of(&text, &units);
                if rel == 0 {
                    continue;
                }
                rows.push((oid, at, msg));
            }
        }
        self.token_lifecycle_from(token, rows)
    }

    fn token_lifecycle_scan(&self, token: &str) -> Result<Option<Lifecycle>> {
        let mut stmt = self.conn().prepare(
            "SELECT c.oid, c.committed_at, c.message, h.added, h.removed
             FROM hunks h JOIN commits c ON c.id = h.commit_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut found: Vec<(String, String, String)> = Vec::new();
        for row in rows {
            let (oid, at, msg, added, removed) = row?;
            if added.contains(token) || removed.contains(token) {
                found.push((oid, at, msg));
            }
        }
        self.token_lifecycle_from(token, found)
    }

    /// The lifecycle of `token` from its touches `(oid, at, message)`, in
    /// any order: oldest first by date then oid, with the coverage of the
    /// diffs it was read from. Both token routes end here, so the trigram
    /// one and the short-token scan answer alike.
    fn token_lifecycle_from(
        &self,
        token: &str,
        mut rows: Vec<(String, String, String)>,
    ) -> Result<Option<Lifecycle>> {
        if rows.is_empty() {
            return Ok(None);
        }
        rows.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        let present = self
            .runner()
            .run(&[
                "grep",
                "-I",
                "-l",
                "--fixed-strings",
                "--end-of-options",
                token,
                "HEAD",
            ])
            .is_ok();
        let total = rows.len() as u64;
        let first = rows.first().cloned();
        let last = rows.last().cloned();
        let coverage = self.token_coverage(token, first.as_ref().map(|f| f.1.as_str()))?;
        Ok(Some(Lifecycle {
            what: token.to_string(),
            first_seen: first.map(|(o, a, m)| reference(&o, &a, &m)),
            last_changed: last.map(|(o, a, m)| reference(&o, &a, &m)),
            removed_in: None,
            present_at_head: present,
            total_touches: total,
            coverage: Some(coverage),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::DiffCoverage;
    use crate::FactsStore;
    use crate::ingest::tests::{ingest_with, ingest_within, no_limits};
    use crate::store::{DIFF_STATE_EVICTED, DIFF_STATE_PENDING, HistoryLimits, short_oid};
    use crate::testutil::{commit_at, days_ago, init_repo};

    /// `oid`'s stored date, as `first_seen.at` carries it.
    fn committed_at(store: &FactsStore, oid: &str) -> String {
        store
            .conn()
            .query_row(
                "SELECT committed_at FROM commits WHERE oid = ?1",
                [oid],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn set_diff_state(store: &FactsStore, oid: &str, state: i64) {
        store
            .conn()
            .execute(
                "UPDATE commits SET diff_state = ?1 WHERE oid = ?2",
                rusqlite::params![state, oid],
            )
            .unwrap();
    }

    /// Issue #487: a token added before the diff window and touched again
    /// inside it. The index holds only the second diff, so the oldest touch
    /// it can see is not the token's first appearance, and the answer must
    /// say so instead of presenting it as one. With the whole history
    /// indexed, the same question gets the real origin, stated as exact.
    #[test]
    fn token_lifecycle_flags_first_seen_as_a_bound_when_older_diffs_are_not_indexed() {
        let dir = init_repo();
        let root = dir.path();
        let origin = commit_at(
            root,
            &[("auth.py", b"def strip_marker():\n    pass\n")],
            "add strip_marker",
            days_ago(400),
        );
        let recent = commit_at(
            root,
            &[("auth.py", b"def strip_marker(url):\n    pass\n")],
            "type strip_marker",
            days_ago(1),
        );

        let mut windowed = FactsStore::open(root).unwrap();
        ingest_with(
            &mut windowed,
            HistoryLimits {
                window_days: Some(365),
                ..no_limits()
            },
        );
        let life = windowed.token_lifecycle("strip_marker").unwrap().unwrap();
        assert_eq!(life.first_seen.unwrap().oid, short_oid(&recent));
        assert_eq!(life.total_touches, 1);
        let coverage = life.coverage.expect("a token answer states its coverage");
        assert_eq!(
            coverage,
            DiffCoverage::from_counts("strip_marker", true, 1, 1),
            "{coverage:?}"
        );
        assert!(coverage.lower_bound && !coverage.first_seen_exact);
        drop(windowed);

        std::fs::remove_dir_all(root.join(".pixel")).unwrap();
        let mut whole = FactsStore::open(root).unwrap();
        ingest_with(&mut whole, no_limits());
        let life = whole.token_lifecycle("strip_marker").unwrap().unwrap();
        assert_eq!(life.first_seen.unwrap().oid, short_oid(&origin));
        assert_eq!(life.total_touches, 2);
        let coverage = life.coverage.expect("a token answer states its coverage");
        assert_eq!(
            coverage,
            DiffCoverage::from_counts("strip_marker", true, 0, 0)
        );
        assert!(!coverage.lower_bound && coverage.first_seen_exact);
        assert_eq!(coverage.note, None);
    }

    /// The count behind `first_seen_exact`: an unindexed commit authored in
    /// the same second as `first_seen` may be the older one, and one whose
    /// date does not parse (git keeps offsets like `+51800`) has no known
    /// order; both count as older. A newer one only makes the count short.
    #[test]
    fn token_coverage_counts_ties_and_unparseable_dates_as_older() {
        let dir = init_repo();
        let root = dir.path();
        let at = days_ago(10);
        let first = commit_at(root, &[("a.rs", b"tok_one\n")], "first", at);
        let tie = commit_at(root, &[("b.rs", b"b\n")], "tie", at);
        let newer = commit_at(root, &[("c.rs", b"c\n")], "newer", days_ago(5));
        let garbled = commit_at(root, &[("d.rs", b"d\n")], "garbled", days_ago(2));
        let mut store = FactsStore::open(root).unwrap();
        ingest_with(&mut store, no_limits());
        let first_at = committed_at(&store, &first);
        assert_eq!(
            store.token_coverage("tok_one", Some(&first_at)).unwrap(),
            DiffCoverage::from_counts("tok_one", true, 0, 0),
            "everything indexed"
        );

        set_diff_state(&store, &tie, DIFF_STATE_EVICTED);
        set_diff_state(&store, &newer, DIFF_STATE_PENDING);
        let coverage = store.token_coverage("tok_one", Some(&first_at)).unwrap();
        assert_eq!(coverage, DiffCoverage::from_counts("tok_one", true, 2, 1));

        set_diff_state(&store, &garbled, DIFF_STATE_EVICTED);
        store
            .conn()
            .execute(
                "UPDATE commits SET committed_at = '2011-09-08T02:38:50+518:00' WHERE oid = ?1",
                [&garbled],
            )
            .unwrap();
        let coverage = store.token_coverage("tok_one", Some(&first_at)).unwrap();
        assert_eq!(coverage, DiffCoverage::from_counts("tok_one", true, 3, 2));
        let coverage = store.token_coverage("tok_one", None).unwrap();
        assert_eq!(
            coverage,
            DiffCoverage::from_counts("tok_one", false, 3, 3),
            "a token found nowhere: every unindexed commit may hold it"
        );
    }

    /// A token below the trigram length takes the scan route, which once
    /// returned its touches in table order, newest diff first: first_seen
    /// was the latest touch. Both routes now share the ordering and the
    /// coverage.
    #[test]
    fn short_token_lifecycle_orders_touches_and_states_coverage() {
        let dir = init_repo();
        let root = dir.path();
        let added = commit_at(root, &[("a.rs", b"zq = 1\n")], "add", days_ago(3));
        let changed = commit_at(root, &[("a.rs", b"zq = 2\n")], "change", days_ago(1));
        let mut store = FactsStore::open(root).unwrap();
        ingest_within(&mut store);
        let life = store.token_lifecycle("zq").unwrap().expect("touched");
        assert_eq!(life.first_seen.unwrap().oid, short_oid(&added));
        assert_eq!(life.last_changed.unwrap().oid, short_oid(&changed));
        assert_eq!(life.total_touches, 2);
        assert_eq!(
            life.coverage,
            Some(DiffCoverage::from_counts("zq", true, 0, 0))
        );
        assert_eq!(store.token_lifecycle("qz").unwrap(), None);
    }

    /// The note is the human half of the marker: each case names what the
    /// answer cannot vouch for, and the command to check it pastes as one
    /// shell word whatever the token holds.
    #[test]
    fn coverage_note_names_what_is_unknown_and_how_to_check_it() {
        let complete = DiffCoverage::from_counts("tok", true, 0, 0);
        assert_eq!(
            (
                complete.lower_bound,
                complete.first_seen_exact,
                complete.note
            ),
            (false, true, None)
        );

        let older = DiffCoverage::from_counts("it's", true, 4945, 4945);
        assert_eq!((older.lower_bound, older.first_seen_exact), (true, false));
        assert_eq!(
            older.note.as_deref(),
            Some(
                "first_seen is the oldest touch among indexed diffs only: 4945 commit(s) \
                 authored at or before it have no indexed diff (older than the history window, \
                 over the size budget, or not ingested yet), so the token may be older and \
                 total_touches may be short. Check the full history with: \
                 git log --reverse -S 'it'\\''s'"
            )
        );

        let newer = DiffCoverage::from_counts("tok", true, 3, 0);
        assert_eq!((newer.lower_bound, newer.first_seen_exact), (true, true));
        assert_eq!(
            newer.note.as_deref(),
            Some(
                "first_seen is exact: every commit up to it has its diff indexed. 3 newer \
                 commit(s) have no indexed diff yet, so last_changed and total_touches may lag."
            )
        );

        let missing = DiffCoverage::from_counts("tok", false, 2, 2);
        assert_eq!(
            (missing.lower_bound, missing.first_seen_exact),
            (true, false)
        );
        assert_eq!(
            missing.note.as_deref(),
            Some(
                "No indexed diff touches the token, but 2 commit(s) have no indexed diff \
                 (older than the history window, over the size budget, or not ingested yet): \
                 it may appear there. Check the full history with: \
                 git log --reverse -S 'tok'"
            )
        );
    }

    /// Token lifecycle runs through the trigram index (its SQL once named a
    /// table alias that did not exist, so every token of three characters
    /// or more failed): first-seen is the oldest touch, last-changed the
    /// newest, and a hunk holding the trigrams apart is no touch.
    #[test]
    fn token_lifecycle_spans_the_oldest_and_newest_verified_touches() {
        let dir = init_repo();
        let root = dir.path();
        let added = commit_at(
            root,
            &[("a.rs", b"let retry_budget = 3;\n")],
            "add",
            days_ago(3),
        );
        commit_at(root, &[("b.rs", b"retry\nbudget\n")], "apart", days_ago(2));
        let removed = commit_at(root, &[("a.rs", b"let x = 3;\n")], "drop", days_ago(1));
        let mut store = FactsStore::open(root).unwrap();
        ingest_within(&mut store);
        let life = store
            .token_lifecycle("retry_budget")
            .unwrap()
            .expect("touched");
        assert_eq!(life.what, "retry_budget");
        assert_eq!(life.total_touches, 2, "{life:?}");
        assert_eq!(life.first_seen.unwrap().oid, short_oid(&added));
        assert_eq!(life.last_changed.unwrap().oid, short_oid(&removed));
        assert!(!life.present_at_head);
        assert_eq!(store.token_lifecycle("never_written").unwrap(), None);
    }
}
