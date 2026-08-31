//! `branches` — one-call read-only branch inventory.
//!
//! The deterministic answer to "did you push everything?" and "clean my
//! branches": one call enumerates every local branch with its upstream,
//! ahead/behind counts, last-commit metadata, staleness, and
//! merged-into-default status, plus a summary block naming the branches
//! that are unpushed, upstream-less, merge-candidates, or stale.
//!
//! Epistemics honesty (T2): when `fetch` is false the response carries
//! `"fetched": false` plus a warning that remote-tracking staleness
//! reflects the last fetch. Ahead/behind for branches without a (live)
//! upstream is `null`, never 0.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use pixel_git::GitRunner;

/// Options for the `branches` inventory op.
#[derive(Debug, Clone)]
pub struct BranchesOptions {
    /// Run `git fetch --prune <remote>` before inventorying, so
    /// remote-tracking refs (and therefore ahead/behind) are live.
    pub fetch: bool,
    /// Remote to fetch from / resolve the default branch against.
    pub remote: String,
    /// A branch whose last commit is older than this many days is stale.
    pub stale_days: u64,
}

impl Default for BranchesOptions {
    fn default() -> Self {
        Self {
            fetch: false,
            remote: "origin".to_string(),
            stale_days: 30,
        }
    }
}

/// Per-branch record parsed from one `for-each-ref` line.
struct BranchRef {
    name: String,
    head_oid: String,
    upstream: Option<String>,
    upstream_gone: bool,
    committer_unix: Option<i64>,
    committer_iso: String,
    author: String,
    subject: String,
}

/// Run the `branches` inventory on a repo root.
pub fn branches(root: &Path, opts: &BranchesOptions) -> Result<Value, String> {
    let runner = GitRunner::new(root);
    let mut warnings: Vec<String> = Vec::new();

    // Optional fetch-first so remote-tracking refs are live.
    if opts.fetch {
        pixel_git::validate_ref(&opts.remote).map_err(|e| e.to_string())?;
        runner
            .run(&["fetch", "--prune", "--end-of-options", &opts.remote])
            .map_err(|e| format!("git fetch --prune {}: {e}", opts.remote))?;
    } else {
        warnings.push(format!(
            "fetched=false: remote-tracking refs reflect the last fetch; \
             ahead/behind vs '{}' may be stale (use fetch=true for a live view)",
            opts.remote
        ));
    }

    // Current branch (None when detached).
    let current = runner
        .run_opt(&["symbolic-ref", "--short", "HEAD"])
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .filter(|s| !s.is_empty());

    // Default branch: <remote>/HEAD symref first, then main/master existence.
    let default_branch = detect_default_branch(&runner, &opts.remote);
    if default_branch.is_none() {
        warnings.push(format!(
            "default branch unknown ({}/HEAD unset and no main/master); \
             merged_into_default is null for every branch",
            opts.remote
        ));
    }

    // One for-each-ref over refs/heads with a NUL-separated rich format.
    let format = "%(refname:short)%00%(objectname:short)%00%(upstream:short)%00\
                  %(upstream:track)%00%(committerdate:unix)%00\
                  %(committerdate:iso-strict)%00%(authorname)%00%(subject)";
    let raw = runner
        .run(&[
            "for-each-ref",
            &format!("--format={format}"),
            "refs/heads",
        ])
        .map_err(|e| format!("git for-each-ref: {e}"))?;
    let refs = parse_for_each_ref(&String::from_utf8_lossy(&raw));

    // Worktree cleanliness feeds fully_pushed.
    let worktree_clean = runner.status_porcelain().is_empty();

    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let stale_cutoff = now_unix - (opts.stale_days as i64) * 86_400;

    let mut branch_values: Vec<Value> = Vec::new();
    let mut unpushed: Vec<String> = Vec::new();
    let mut no_upstream: Vec<String> = Vec::new();
    let mut merged_candidates: Vec<String> = Vec::new();
    let mut stale_names: Vec<String> = Vec::new();
    let mut all_tracked_pushed = true;

    for r in &refs {
        let is_current = current.as_deref() == Some(r.name.as_str());

        // Ahead/behind vs a live upstream only; null otherwise (never 0).
        let (ahead, behind): (Option<u64>, Option<u64>) = match (&r.upstream, r.upstream_gone) {
            (Some(up), false) => {
                let counts = runner
                    .run_opt(&[
                        "rev-list",
                        "--left-right",
                        "--count",
                        &format!("refs/heads/{}...{}", r.name, up),
                    ])
                    .map(|b| String::from_utf8_lossy(&b).trim().to_string());
                match counts.as_deref().map(parse_left_right) {
                    Some(Some((a, b))) => (Some(a), Some(b)),
                    _ => {
                        warnings.push(format!(
                            "could not compute ahead/behind for '{}' vs '{up}'",
                            r.name
                        ));
                        (None, None)
                    }
                }
            }
            _ => (None, None),
        };

        // Merged into the default branch = branch head is an ancestor of it.
        let merged_into_default: Value = match &default_branch {
            Some(default) => {
                let merged = runner
                    .run(&[
                        "merge-base",
                        "--is-ancestor",
                        &format!("refs/heads/{}", r.name),
                        &format!("refs/heads/{default}"),
                    ])
                    .is_ok();
                Value::Bool(merged)
            }
            None => Value::Null,
        };

        let stale = r
            .committer_unix
            .map(|t| t < stale_cutoff)
            .unwrap_or(false);

        // Summary bookkeeping.
        let has_live_upstream = r.upstream.is_some() && !r.upstream_gone;
        if has_live_upstream {
            if ahead.unwrap_or(0) > 0 {
                unpushed.push(r.name.clone());
                all_tracked_pushed = false;
            } else if ahead.is_none() {
                // Upstream exists but counts failed: cannot claim pushed.
                all_tracked_pushed = false;
            }
        } else {
            no_upstream.push(r.name.clone());
        }
        if merged_into_default == Value::Bool(true)
            && !is_current
            && default_branch.as_deref() != Some(r.name.as_str())
        {
            merged_candidates.push(r.name.clone());
        }
        if stale {
            stale_names.push(r.name.clone());
        }

        branch_values.push(json!({
            "name": r.name,
            "head_oid": r.head_oid,
            "upstream": r.upstream,
            "upstream_gone": r.upstream_gone,
            "ahead": ahead,
            "behind": behind,
            "last_commit": {
                "date": r.committer_iso,
                "author": r.author,
                "subject": r.subject,
            },
            "stale": stale,
            "merged_into_default": merged_into_default,
            "is_current": is_current,
        }));
    }

    let fully_pushed = all_tracked_pushed && worktree_clean;

    Ok(json!({
        "root": root.display().to_string(),
        "fetched": opts.fetch,
        "remote": opts.remote,
        "stale_days": opts.stale_days,
        "default_branch": default_branch,
        "worktree_clean": worktree_clean,
        "branches": branch_values,
        "summary": {
            "total": refs.len(),
            "current": current,
            "fully_pushed": fully_pushed,
            "unpushed": unpushed,
            "no_upstream": no_upstream,
            "merged_candidates": merged_candidates,
            "stale": stale_names,
        },
        "warnings": warnings,
    }))
}

/// `<remote>/HEAD` symref first (e.g. "origin/main" → "main"), then
/// fall back to whichever of main/master exists as a local branch.
fn detect_default_branch(runner: &GitRunner, remote: &str) -> Option<String> {
    if let Some(out) =
        runner.run_opt(&["symbolic-ref", "--short", &format!("refs/remotes/{remote}/HEAD")])
    {
        let short = String::from_utf8_lossy(&out).trim().to_string();
        if let Some(stripped) = short.strip_prefix(&format!("{remote}/"))
            && !stripped.is_empty()
        {
            return Some(stripped.to_string());
        }
    }
    for candidate in ["main", "master"] {
        if runner
            .run_opt(&[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{candidate}"),
            ])
            .is_some()
        {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Parse the NUL-separated for-each-ref output into branch records.
fn parse_for_each_ref(text: &str) -> Vec<BranchRef> {
    text.lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(8, '\0').collect();
            if parts.len() != 8 {
                return None;
            }
            let upstream = if parts[2].is_empty() {
                None
            } else {
                Some(parts[2].to_string())
            };
            Some(BranchRef {
                name: parts[0].to_string(),
                head_oid: parts[1].to_string(),
                upstream,
                upstream_gone: parts[3].contains("gone"),
                committer_unix: parts[4].trim().parse::<i64>().ok(),
                committer_iso: parts[5].to_string(),
                author: parts[6].to_string(),
                subject: parts[7].to_string(),
            })
        })
        .collect()
}

/// Parse `rev-list --left-right --count` output: "ahead\tbehind".
fn parse_left_right(s: &str) -> Option<(u64, u64)> {
    let mut it = s.split_whitespace();
    let ahead = it.next()?.parse::<u64>().ok()?;
    let behind = it.next()?.parse::<u64>().ok()?;
    Some((ahead, behind))
}
