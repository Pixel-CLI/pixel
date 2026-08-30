//! `gitpixel rescue` — surgical revert planner + gated apply.
//!
//! "Something was working before" is a retrieval problem, not a coding
//! problem: locate the files the problem points at (reusing the sniper-target
//! engine), list each file's recent versions with the likely-breaking commit
//! flagged, and recommend a last-known-good candidate. Plan by default —
//! nothing is written without `--apply <oid>`, and apply obeys hard safety
//! invariants: never `reset`, never touches the index or HEAD, never
//! overwrites uncommitted work unless explicitly told how (`--merge`,
//! `--stash-first`, or `--allow-dirty`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{Value, json};
use pixel_git::{GitError, GitRunner};

// ---------------------------------------------------------------------------
// git plumbing — now delegated to pixel_git::GitRunner (single wrapper,
// timeout + output-cap enforced, refs validated consistently).
// ---------------------------------------------------------------------------

/// Reject anything that is not a plain hex object id or simple ref name —
/// in particular anything starting with `-` (option injection). Delegates
/// to `pixel_git::validate_ref` (the single shared validator), which now
/// accepts mid-string dashes like `fix-bug`.
fn validate_ref(r: &str) -> Result<(), String> {
    pixel_git::validate_ref(r).map_err(|e| format!("invalid git ref {r:?}: {e}"))
}

/// Blob oid of `path` as it exists in `commit`. `commit` is validated via
/// `pixel_git::validate_ref` (the original wrapper passed it unvalidated to
/// `git rev-parse` — a ref-injection gap, now closed).
fn blob_oid(root: &Path, commit: &str, path: &str) -> Option<String> {
    GitRunner::new(root).rev_parse_at(commit, path)
}

/// Working-tree dirty map: path -> porcelain status (e.g. " M", "??").
///
/// Uses `status_porcelain_or_err`, NOT `status_porcelain`: this map decides
/// whether `apply` may overwrite a file, so an undetermined status (git
/// error, timeout, or output-cap overflow from e.g. a large untracked tree)
/// must abort the operation, never be silently read as "nothing is dirty".
/// The latter previously let `--apply` overwrite an actually-dirty file
/// with no strategy flag given, whenever a large untracked tree pushed
/// `status --porcelain` past the output cap.
fn dirty_map(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let entries = GitRunner::new(root).status_porcelain_or_err().map_err(|e| {
        format!(
            "could not determine working-tree status, refusing to proceed \
             (would otherwise risk treating dirty files as clean): {e}"
        )
    })?;
    let mut map = BTreeMap::new();
    for (xy, path) in entries {
        map.insert(path, xy.trim().to_string());
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// plan
// ---------------------------------------------------------------------------

struct VersionRow {
    oid: String,
    unix_date: u64,
    subject: String,
    suspect: bool,
    /// Why this version is suspect: `diff-content: ...` (primary — the
    /// commit's own content change removed phrase-bearing text) or
    /// `subject-keyword: ...` (secondary — used only when the content on
    /// either side of the commit could not be read).
    suspect_basis: Option<String>,
    blob: Option<String>,
}

/// Build the rescue plan for one repo. `target_paths` come from --file hints
/// or the targets engine (P0 slice). `keywords` come from the tokenized
/// problem text.
///
/// Suspect detection is DIFF-CONTENT based (shared heuristic:
/// `pixel_facts::excavate::phrase_removed_between`): a commit is suspect
/// because the file's content BEFORE it contained a keyword that its content
/// AFTER it no longer does — i.e. this commit removed the phrase/behavior.
/// Commit-subject keyword matching survives only as a secondary signal for
/// versions whose blob content could not be read (over-cap, timeout); when
/// diff content and subject disagree, diff content wins.
pub fn plan(
    root: &Path,
    problem: &str,
    target_paths: &[String],
    keywords: &[String],
    depth: usize,
) -> Result<Value, String> {
    let dirty = dirty_map(root)?;
    let mut targets: Vec<Value> = Vec::new();
    let mut caveats: Vec<String> = Vec::new();

    for path in target_paths {
        let runner = GitRunner::new(root);
        let log_rows = runner.log_follow(path, depth).unwrap_or_default();
        let log = log_rows
            .into_iter()
            .map(|(oid, ct, subject)| format!("{oid}\u{1f}{ct}\u{1f}{subject}"))
            .collect::<Vec<_>>()
            .join("\n");

        let head_blob = blob_oid(root, "HEAD", path);
        let mut versions: Vec<VersionRow> = Vec::new();
        for line in log.lines() {
            let mut parts = line.split('\u{1f}');
            let (Some(oid), Some(ct), Some(subject)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            versions.push(VersionRow {
                oid: oid.to_string(),
                unix_date: ct.parse().unwrap_or(0),
                subject: subject.to_string(),
                suspect: false,
                suspect_basis: None,
                blob: blob_oid(root, oid, path),
            });
        }

        // Whether `--depth` truncated this file's history: log_follow
        // returned exactly as many rows as asked for, so older commits may
        // exist unseen.
        let depth_cap_hit = versions.len() >= depth;

        // File content at each inspected version (newest-first). `None` =
        // unreadable (over-cap/timeout/absent at that commit) — an unknown,
        // never treated as evidence in either direction.
        let contents: Vec<Option<String>> = versions
            .iter()
            .map(|v| runner.show_blob_string(&v.oid, path).ok())
            .collect();

        for i in 0..versions.len() {
            // Content BEFORE version i = the previous version of the file
            // (versions[i+1]). For the oldest row: with history fully
            // enumerated (no depth cap) the file did not exist before, so
            // "before" is empty; under a hit depth cap the before-state is
            // genuinely unknown.
            let before: Option<String> = if i + 1 < versions.len() {
                contents[i + 1].clone()
            } else if !depth_cap_hit {
                Some(String::new())
            } else {
                None
            };
            let short = versions[i].oid[..7.min(versions[i].oid.len())].to_string();
            match (&contents[i], &before) {
                (Some(after), Some(before_text)) => {
                    if let Some(kw) = pixel_facts::excavate::phrase_removed_between(
                        before_text,
                        after,
                        keywords,
                    ) {
                        versions[i].suspect = true;
                        versions[i].suspect_basis =
                            Some(format!("diff-content: {kw:?} removed in {short}"));
                    }
                }
                _ => {
                    // Content unknown on at least one side — fall back to
                    // the secondary subject-keyword signal, labeled as such.
                    let subject_lc = versions[i].subject.to_lowercase();
                    if let Some(k) = keywords.iter().find(|k| subject_lc.contains(k.as_str())) {
                        versions[i].suspect = true;
                        versions[i].suspect_basis = Some(format!(
                            "subject-keyword: subject mentions {k:?} in {short} \
                             (content unreadable; weaker signal)"
                        ));
                    }
                }
            }
        }

        // Recommended last-known-good: newest version strictly older than the
        // oldest suspect commit; without suspects, the newest version whose
        // blob differs from HEAD's (i.e. the state before the last change).
        let oldest_suspect_idx = versions.iter().rposition(|v| v.suspect);
        let by_suspect = oldest_suspect_idx
            .filter(|i| i + 1 < versions.len())
            .and_then(|i| {
                versions.get(i + 1).map(|v| {
                    let s = &versions[i];
                    let basis = s
                        .suspect_basis
                        .clone()
                        .unwrap_or_else(|| "diff-content".to_string());
                    json!({
                        "oid": v.oid,
                        "reason": format!(
                            "last version before suspect commit {} ({basis})",
                            &s.oid[..7.min(s.oid.len())]
                        ),
                        "basis": basis,
                    })
                })
            });
        // Fallback (no usable suspect — none matched, or every version
        // matched): the newest version whose content differs from HEAD, i.e.
        // the state just before the file's most recent change.
        let recommended = by_suspect.or_else(|| {
            versions
                .iter()
                .find(|v| v.blob.is_some() && v.blob != head_blob)
                .map(|v| {
                    json!({
                        "oid": v.oid,
                        "reason": "newest version whose content differs from HEAD",
                        "basis": "content-differs: no suspect commit found; this is only the state before the file's most recent change",
                    })
                })
        });

        // Bounded-answer honesty: the default --depth silently truncates
        // history. When the cap was hit and no suspect surfaced within it,
        // say so instead of implying the breakage is not in history.
        let depth_note = (depth_cap_hit && oldest_suspect_idx.is_none()).then(|| {
            format!(
                "{path}: only the newest {depth} commits touching this file were inspected \
                 and none is suspect — older history was NOT examined; re-run with a larger \
                 --depth to look further back"
            )
        });
        if let Some(n) = &depth_note {
            caveats.push(n.clone());
        }

        targets.push(json!({
            "path": path,
            "dirty": dirty.contains_key(path),
            "depth_cap_hit": depth_cap_hit,
            "depth_note": depth_note,
            "versions": versions.iter().map(|v| json!({
                "oid": v.oid,
                "short": &v.oid[..7.min(v.oid.len())],
                "unix_date": v.unix_date,
                "subject": v.subject,
                "suspect": v.suspect,
                "suspect_basis": v.suspect_basis,
                "blob_differs_from_head": v.blob.is_some() && v.blob != head_blob,
            })).collect::<Vec<_>>(),
            "recommended": recommended.unwrap_or(Value::Null),
        }));
    }

    let dirty_in_plan: Vec<&String> = target_paths
        .iter()
        .filter(|p| dirty.contains_key(*p))
        .collect();
    for p in &dirty_in_plan {
        caveats.push(format!(
            "{p} has uncommitted changes; --apply needs --merge (3-way, keeps your edits), --stash-first, or --allow-dirty"
        ));
    }
    let rec_oids: BTreeSet<String> = targets
        .iter()
        .filter_map(|t| t["recommended"]["oid"].as_str().map(str::to_string))
        .collect();
    let revert_cmd = rec_oids.iter().next().map(|oid| {
        let files: Vec<String> = target_paths.iter().map(|p| format!("--file {p}")).collect();
        format!("pixel rescue --apply {oid} {} .", files.join(" "))
    });

    Ok(json!({
        "problem": problem,
        "root": root.display().to_string(),
        "dirty_files": dirty.iter().map(|(p, s)| json!({
            "path": p, "status": s, "in_plan": target_paths.contains(p),
        })).collect::<Vec<_>>(),
        "targets": targets,
        "decision": {
            "options": [
                {
                    "id": "revert",
                    "label": format!("Revert {} file(s) to the recommended version (working tree only, undoable)", target_paths.len()),
                    "command": revert_cmd,
                },
                {
                    "id": "fix_forward",
                    "label": "Keep current code; fix forward",
                },
            ],
            "caveats": caveats,
        },
    }))
}

// ---------------------------------------------------------------------------
// apply
// ---------------------------------------------------------------------------

pub struct ApplyOptions {
    pub merge: bool,
    pub stash_first: bool,
    pub allow_dirty: bool,
}

/// Restore `files` to their content at `oid`. Working tree only: the index
/// and HEAD are never touched, so every write shows up as an ordinary diff
/// the user can still undo. Dirty files are refused unless a strategy flag
/// says how to preserve the in-progress work.
pub fn apply(
    root: &Path,
    oid: &str,
    files: &[String],
    opts: &ApplyOptions,
) -> Result<Value, String> {
    validate_ref(oid)?;
    let runner = GitRunner::new(root);
    runner
        .rev_verify_commit(oid)
        .map_err(|_| format!("{oid} is not a commit in this repository"))?;
    if files.is_empty() {
        return Err("--apply requires at least one --file".to_string());
    }

    let dirty = dirty_map(root)?;
    let dirty_planned: Vec<&String> = files.iter().filter(|p| dirty.contains_key(*p)).collect();
    let has_strategy = opts.merge || opts.stash_first || opts.allow_dirty;
    if !dirty_planned.is_empty() && !has_strategy {
        return Err(format!(
            "refusing to overwrite uncommitted work in: {}. Choose a strategy: \
             --merge (3-way, keeps your edits, may leave conflict markers), \
             --stash-first (git stash push the planned files first), or \
             --allow-dirty (overwrite).",
            dirty_planned
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if opts.stash_first && !dirty_planned.is_empty() {
        let paths: Vec<String> = dirty_planned.iter().map(|s| (*s).clone()).collect();
        runner
            .stash_push_paths("pixel rescue backup", &paths)
            .map_err(|e| e.to_string())?;
    }

    let mut results: Vec<Value> = Vec::new();
    for path in files {
        let content = runner.show_blob_string(oid, path).map_err(|e| match e {
            // A real git failure reading `oid:path` (non-zero exit from
            // `git show`) is the one case that actually means "does not
            // exist at this commit" — every other error means the read
            // was never completed, and saying so would be misleading.
            GitError::NonZeroExit { .. } => format!("{path} does not exist at {oid}: {e}"),
            GitError::OutputTooLarge { cap, .. } => format!(
                "{path} at {oid} is larger than the {cap}-byte limit `rescue --apply` can \
                 restore; it was not modified"
            ),
            GitError::Timeout { .. } => {
                format!("timed out reading {path} at {oid}; it was not modified")
            }
            other => format!("failed to read {path} at {oid}: {other}"),
        })?;
        let abs = root.join(path);
        let was_dirty = dirty.contains_key(path);

        if opts.merge && was_dirty && !opts.stash_first {
            // Deterministic 3-way merge: ours = working tree (in-progress
            // work), base = HEAD's version, theirs = the rescued version.
            // Must propagate failure rather than `.unwrap_or_default()`:
            // silently treating an unreadable HEAD blob as an empty base
            // turns a normal 3-way merge into a whole-file conflict against
            // the in-progress edits, with no warning that anything went
            // wrong.
            let base = runner.show_blob_string("HEAD", path).map_err(|e| {
                format!("failed to read HEAD version of {path} for 3-way merge: {e}")
            })?;
            let tmp_base = abs.with_extension("gpx-rescue-base");
            let tmp_theirs = abs.with_extension("gpx-rescue-theirs");
            std::fs::write(&tmp_base, &base).map_err(|e| e.to_string())?;
            std::fs::write(&tmp_theirs, &content).map_err(|e| e.to_string())?;
            let status = runner
                .merge_file_with_labels(
                    &abs,
                    &tmp_base,
                    &tmp_theirs,
                    "in-progress",
                    "HEAD",
                    &format!("rescue:{}", &oid[..7.min(oid.len())]),
                )
                .map_err(|e| format!("spawn git merge-file: {e}"))?;
            std::fs::remove_file(&tmp_base).ok();
            std::fs::remove_file(&tmp_theirs).ok();
            let code = status.code().unwrap_or(-1);
            if code < 0 {
                return Err(format!(
                    "merge-file failed for {path}"
                ));
            }
            results.push(json!({
                "path": path,
                "action": "merged",
                "conflicts": code,
            }));
        } else {
            // Plain restore via temp file + atomic rename.
            let tmp = abs.with_extension("gpx-rescue-tmp");
            std::fs::write(&tmp, &content).map_err(|e| format!("write {}: {e}", tmp.display()))?;
            std::fs::rename(&tmp, &abs).map_err(|e| format!("publish {}: {e}", abs.display()))?;
            results.push(json!({
                "path": path,
                "action": if was_dirty { "overwritten" } else { "restored" },
            }));
        }
    }

    Ok(json!({
        "applied": oid,
        "files": results,
        "note": "working tree only — index and HEAD untouched; undo with `git checkout -- <file>` (or `git stash pop` if --stash-first was used)",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pixel-rescue-cmd-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
    }

    fn no_strategy() -> ApplyOptions {
        ApplyOptions {
            merge: false,
            stash_first: false,
            allow_dirty: false,
        }
    }

    /// CRITICAL regression test for the data-loss bug: `apply`'s dirty-file
    /// guard used to call `status_porcelain` (empty-on-any-failure,
    /// including 1 MiB output-cap overflow), so a large enough *untracked*
    /// tree made a genuinely dirty tracked file look clean — and `apply`
    /// would overwrite it with no strategy flag given. `dirty_map` now uses
    /// `status_porcelain_or_err`, which must abort the whole operation
    /// instead of silently proceeding as if the tree were clean.
    #[test]
    fn apply_refuses_dirty_file_even_with_a_large_untracked_tree_present() {
        let root = tmpdir("apply-large-untracked");
        init_repo(&root);
        std::fs::write(root.join("f.txt"), b"committed content\n").unwrap();
        git(&root, &["add", "f.txt"]);
        git(&root, &["commit", "-q", "-m", "add f"]);
        let head = GitRunner::new(&root).rev_parse_head().unwrap();

        // Dirty the tracked file — in-progress work that must never be
        // silently overwritten.
        std::fs::write(root.join("f.txt"), b"DIRTY IN-PROGRESS EDIT\n").unwrap();

        // A large untracked tree: previously pushed `status --porcelain -z`
        // past the 1 MiB output cap, which made `dirty_map` see an empty
        // (i.e. "clean") working tree.
        let dir = format!("untracked-{}", "x".repeat(240));
        std::fs::create_dir_all(root.join(&dir)).unwrap();
        for i in 0..5300 {
            std::fs::write(root.join(&dir).join(format!("g{i:05}.txt")), b"z").unwrap();
        }

        let result = apply(&root, &head, &["f.txt".to_string()], &no_strategy());
        assert!(
            result.is_err(),
            "apply must refuse to overwrite a dirty file, even when a large untracked tree is \
             present and no strategy flag was given; got: {result:?}"
        );
        let content = std::fs::read_to_string(root.join("f.txt")).unwrap();
        assert_eq!(
            content, "DIRTY IN-PROGRESS EDIT\n",
            "the dirty file must be left completely untouched by the refused apply"
        );
    }

    /// With a strategy flag (`--allow-dirty`), the same large-untracked-tree
    /// scenario must still correctly detect the dirty file and proceed only
    /// because the user explicitly authorized it — proving the guard now
    /// evaluates real status rather than merely happening to fail closed.
    #[test]
    fn apply_allow_dirty_overwrites_when_explicitly_authorized_with_large_untracked_tree() {
        let root = tmpdir("apply-large-untracked-allow");
        init_repo(&root);
        std::fs::write(root.join("f.txt"), b"committed content\n").unwrap();
        git(&root, &["add", "f.txt"]);
        git(&root, &["commit", "-q", "-m", "add f"]);
        let head = GitRunner::new(&root).rev_parse_head().unwrap();

        std::fs::write(root.join("f.txt"), b"DIRTY IN-PROGRESS EDIT\n").unwrap();

        let dir = format!("untracked-{}", "x".repeat(240));
        std::fs::create_dir_all(root.join(&dir)).unwrap();
        for i in 0..5300 {
            std::fs::write(root.join(&dir).join(format!("g{i:05}.txt")), b"z").unwrap();
        }

        let opts = ApplyOptions {
            merge: false,
            stash_first: false,
            allow_dirty: true,
        };
        let result = apply(&root, &head, &["f.txt".to_string()], &opts)
            .expect("apply with --allow-dirty must succeed when explicitly authorized");
        assert_eq!(result["files"][0]["action"], "overwritten");
        let content = std::fs::read_to_string(root.join("f.txt")).unwrap();
        assert_eq!(content, "committed content\n");
    }

    /// Regression test for the blob output cap: `rescue --apply` used to
    /// hard-fail on any file over 1 MiB with a misleading "does not exist
    /// at <oid>" error, even though the file existed. A ~1.5 MiB file must
    /// now restore correctly.
    #[test]
    fn apply_restores_a_file_over_the_old_1mib_cap() {
        let root = tmpdir("apply-1-5mib-file");
        init_repo(&root);
        let good_content = {
            let mut v = vec![b'g'; 1024 * 1024 + 512 * 1024]; // ~1.5 MiB
            v.extend_from_slice(b"GOOD_VERSION_MARKER");
            v
        };
        std::fs::write(root.join("big.txt"), &good_content).unwrap();
        git(&root, &["add", "big.txt"]);
        git(&root, &["commit", "-q", "-m", "good version"]);
        let good_oid = GitRunner::new(&root).rev_parse_head().unwrap();

        // A later, "bad" version — apply will roll back to `good_oid`.
        let bad_content = {
            let mut v = vec![b'b'; 1024 * 1024 + 512 * 1024];
            v.extend_from_slice(b"BAD_VERSION_MARKER");
            v
        };
        std::fs::write(root.join("big.txt"), &bad_content).unwrap();
        git(&root, &["add", "big.txt"]);
        git(&root, &["commit", "-q", "-m", "bad version"]);

        let result = apply(&root, &good_oid, &["big.txt".to_string()], &no_strategy())
            .unwrap_or_else(|e| {
                panic!(
                    "apply must restore a ~1.5 MiB file (previously misreported as \
                     \"does not exist\" above the old 1 MiB blob cap): {e}"
                )
            });
        assert_eq!(result["files"][0]["action"], "restored");
        let restored = std::fs::read(root.join("big.txt")).unwrap();
        assert_eq!(restored, good_content);
    }

    /// A file genuinely over the (raised) blob cap must fail with an
    /// accurate, actionable error — never the misleading "does not exist"
    /// message a blanket `map_err` used to produce for every failure mode.
    #[test]
    fn apply_reports_an_accurate_error_for_a_file_over_the_blob_cap() {
        let root = tmpdir("apply-over-cap");
        init_repo(&root);
        let content = vec![b'a'; 5 * 1024 * 1024]; // over the 4 MiB blob cap
        std::fs::write(root.join("huge.txt"), &content).unwrap();
        git(&root, &["add", "huge.txt"]);
        git(&root, &["commit", "-q", "-m", "huge"]);
        let oid = GitRunner::new(&root).rev_parse_head().unwrap();

        let err = apply(&root, &oid, &["huge.txt".to_string()], &no_strategy())
            .expect_err("a file over the blob cap must not silently succeed");
        assert!(
            !err.contains("does not exist"),
            "error for an over-cap file must not claim the file does not exist: {err}"
        );
        assert!(
            err.contains("limit") || err.contains("larger"),
            "error should explain the size limit was exceeded: {err}"
        );
    }
}
