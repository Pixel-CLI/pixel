//! `provenance` — per-region blame attribution for a single file.
//!
//! Answers "who introduced this?" / "was this touched by me or already like
//! this?" deterministically: `git blame --porcelain` parsed into contiguous
//! same-commit regions, plus file-level facts (introducing commit via
//! `log --follow --diff-filter=A`, last-modifying commit, per-author line
//! histogram) and an optional author verdict.
//!
//! Epistemics (T2): a truncated region list sets `lower_bound: true` and a
//! warning; the response always carries a `rename_follow` note because
//! blame regions do NOT follow renames — only the `introduced_by` /
//! `last_modified` facts do (`git log --follow`).

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Value};

use pixel_git::GitRunner;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";
/// Blame output on a large file can legitimately exceed the 1 MiB default
/// runner cap (every line carries a 40-hex header). Same rationale as
/// pixel-git's enumeration cap.
const BLAME_MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// Options for the `provenance` op.
#[derive(Debug, Clone)]
pub struct ProvenanceOptions {
    /// Repo-relative path of the file to attribute.
    pub file: String,
    /// Optional 1-based inclusive line range (`git blame -L a,b`).
    pub lines: Option<(u32, u32)>,
    /// Optional author query — case-insensitive substring matched against
    /// author name OR email; produces a `verdict` object when set.
    pub author: Option<String>,
    /// Maximum regions emitted in the response (default 200). Regions past
    /// the cap are dropped from the list (histogram/verdict still cover
    /// them) and the envelope sets `lower_bound: true` plus a warning.
    pub limit_regions: usize,
}

impl Default for ProvenanceOptions {
    fn default() -> Self {
        Self {
            file: String::new(),
            lines: None,
            author: None,
            limit_regions: 200,
        }
    }
}

/// Per-commit metadata cached while parsing porcelain blame output.
#[derive(Debug, Clone, Default)]
struct CommitMeta {
    author: String,
    author_mail: String,
    author_time: Option<i64>,
    author_tz: String,
    summary: String,
}

/// An attributed contiguous region (internal; carries the raw epoch for
/// chronological comparisons the ISO string cannot support).
#[derive(Debug, Clone)]
struct Region {
    start_line: u32,
    end_line: u32,
    oid: Option<String>,
    author: String,
    author_mail: Option<String>,
    author_time_epoch: Option<i64>,
    author_time_iso: Option<String>,
    summary: Option<String>,
}

impl Region {
    fn line_count(&self) -> u64 {
        u64::from(self.end_line - self.start_line) + 1
    }

    fn to_json(&self) -> Value {
        json!({
            "start_line": self.start_line,
            "end_line": self.end_line,
            "oid": self.oid,
            "author": self.author,
            "author_mail": self.author_mail,
            "author_time": self.author_time_iso,
            "summary": self.summary,
        })
    }
}

/// Run `provenance` on one file of the repo at `root`.
pub fn provenance(root: &Path, opts: &ProvenanceOptions) -> Result<Value, String> {
    let file = opts.file.as_str();
    if file.is_empty() {
        return Err("PROVENANCE_BAD_ARGS: file path must not be empty".to_string());
    }
    let runner = GitRunner::new(root);

    // 1. Tracked check — a structured error naming the path, not a raw
    //    git stderr dump.
    if runner
        .run(&["ls-files", "--error-unmatch", "--", file])
        .is_err()
    {
        return Err(format!(
            "FILE_NOT_TRACKED: '{file}' is not tracked by git in this repository"
        ));
    }

    // 2. Binary check — blame attribution is line-based and meaningless on
    //    binary content. `ls-files --eol` reports `i/-text` for binary
    //    index content (and `w/-text` for binary worktree content).
    if let Ok(eol_out) = runner.run(&["ls-files", "--eol", "--", file]) {
        let eol = String::from_utf8_lossy(&eol_out);
        if eol.contains("i/-text") || eol.contains("w/-text") {
            return Err(format!(
                "BINARY_FILE: '{file}' is binary; line-based blame attribution does not apply"
            ));
        }
    }

    // 3. Blame — porcelain, optionally line-restricted.
    let range_arg = opts.lines.map(|(a, b)| format!("{a},{b}"));
    let mut blame_args: Vec<&str> = vec!["blame", "--porcelain"];
    if let Some(range) = range_arg.as_deref() {
        blame_args.push("-L");
        blame_args.push(range);
    }
    blame_args.push("--");
    blame_args.push(file);

    let blame_runner = runner.with_max_output_bytes(Some(BLAME_MAX_OUTPUT_BYTES));
    let blame_out = blame_runner
        .run(&blame_args)
        .map_err(|e| format!("git blame: {e}"))?;
    let blame_text = String::from_utf8_lossy(&blame_out);

    let all_regions = parse_porcelain_regions(&blame_text);

    // 4. File-level facts. `--follow` crosses renames — the one place in
    //    this response that does.
    let introduced_by = introduced_by(&runner, file);
    let last_modified = last_modified(&runner, file);

    // 5. Authors histogram (name -> lines owned), computed over ALL parsed
    //    regions — never the truncated list.
    let mut authors: BTreeMap<String, u64> = BTreeMap::new();
    for region in &all_regions {
        *authors.entry(region.author.clone()).or_insert(0) += region.line_count();
    }

    // 6. Optional author verdict — also over ALL regions.
    let verdict = opts.author.as_deref().map(|query| {
        let q = query.to_lowercase();
        let matches = |name: &str, mail: Option<&str>| {
            name.to_lowercase().contains(&q)
                || mail
                    .map(|m| m.to_lowercase().contains(&q))
                    .unwrap_or(false)
        };
        let mut lines_owned = 0u64;
        let mut regions_owned = 0u64;
        let mut last_touch: Option<(i64, String)> = None;
        for region in &all_regions {
            if matches(&region.author, region.author_mail.as_deref()) {
                lines_owned += region.line_count();
                regions_owned += 1;
                if let (Some(epoch), Some(iso)) =
                    (region.author_time_epoch, region.author_time_iso.as_ref())
                    && last_touch.as_ref().map(|(e, _)| epoch > *e).unwrap_or(true)
                {
                    last_touch = Some((epoch, iso.clone()));
                }
            }
        }
        let introduced_file = introduced_by
            .as_ref()
            .map(|c| {
                matches(
                    c["author"].as_str().unwrap_or(""),
                    c["author_mail"].as_str(),
                )
            })
            .unwrap_or(false);
        json!({
            "author_query": query,
            "lines_owned": lines_owned,
            "regions_owned": regions_owned,
            "introduced_file": introduced_file,
            "last_touch": last_touch.map(|(_, iso)| iso),
        })
    });

    // 7. Truncation honesty (T2): the cap surfaces in the envelope.
    let total_regions = all_regions.len();
    let limit = opts.limit_regions.max(1);
    let truncated = total_regions > limit;
    let mut warnings: Vec<String> = Vec::new();
    if truncated {
        warnings.push(format!(
            "regions truncated at limit_regions={limit} ({total_regions} total); \
             histogram and verdict still cover all regions"
        ));
    }
    let emitted: Vec<Value> = all_regions
        .iter()
        .take(limit)
        .map(Region::to_json)
        .collect();

    Ok(json!({
        "file": file,
        "line_range": opts.lines.map(|(a, b)| json!([a, b])),
        "regions": emitted,
        "region_count": emitted.len(),
        "region_count_total": total_regions,
        "introduced_by": introduced_by,
        "last_modified": last_modified,
        "authors": authors,
        "verdict": verdict,
        "lower_bound": truncated,
        "warnings": warnings,
        "rename_follow": "log --follow only",
    }))
}

/// Parse `git blame --porcelain` output into contiguous same-commit
/// regions. In porcelain format every content line is preceded by a
/// `<40-hex-oid> <orig_line> <final_line> [<group_size>]` header; commit
/// metadata (author/author-mail/author-time/summary/...) appears the first
/// time a commit is referenced and is cached by oid.
fn parse_porcelain_regions(text: &str) -> Vec<Region> {
    let mut meta: BTreeMap<String, CommitMeta> = BTreeMap::new();
    let mut line_oids: Vec<(u32, String)> = Vec::new();
    let mut current_oid: Option<String> = None;
    let mut current_final: u32 = 0;

    for line in text.lines() {
        if let Some((oid, final_line)) = parse_header(line) {
            meta.entry(oid.clone()).or_default();
            current_oid = Some(oid);
            current_final = final_line;
            continue;
        }
        if let Some(rest) = line.strip_prefix('\t') {
            let _ = rest; // content itself is not part of the attribution
            if let Some(oid) = &current_oid {
                line_oids.push((current_final, oid.clone()));
            }
            continue;
        }
        // Metadata line for the most recent header's commit.
        if let Some(oid) = &current_oid {
            let entry = meta.entry(oid.clone()).or_default();
            if let Some(v) = line.strip_prefix("author ") {
                entry.author = v.to_string();
            } else if let Some(v) = line.strip_prefix("author-mail ") {
                entry.author_mail = v.trim_matches(['<', '>']).to_string();
            } else if let Some(v) = line.strip_prefix("author-time ") {
                entry.author_time = v.trim().parse::<i64>().ok();
            } else if let Some(v) = line.strip_prefix("author-tz ") {
                entry.author_tz = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("summary ") {
                entry.summary = v.to_string();
            }
        }
    }

    // Group contiguous runs: same commit AND consecutive line numbers.
    let mut regions: Vec<Region> = Vec::new();
    for (final_line, oid) in line_oids {
        let extend = regions
            .last()
            .map(|r| {
                r.end_line + 1 == final_line
                    && match (&r.oid, oid.as_str()) {
                        (Some(prev), o) => prev == o,
                        (None, o) => o == ZERO_OID,
                    }
            })
            .unwrap_or(false);
        if extend {
            regions.last_mut().expect("just checked").end_line = final_line;
            continue;
        }
        let m = meta.get(&oid).cloned().unwrap_or_default();
        let region = if oid == ZERO_OID {
            Region {
                start_line: final_line,
                end_line: final_line,
                oid: None,
                author: "uncommitted".to_string(),
                author_mail: None,
                author_time_epoch: None,
                author_time_iso: None,
                summary: None,
            }
        } else {
            Region {
                start_line: final_line,
                end_line: final_line,
                oid: Some(oid),
                author: m.author.clone(),
                author_mail: Some(m.author_mail.clone()),
                author_time_epoch: m.author_time,
                author_time_iso: m.author_time.map(|t| epoch_to_iso(t, &m.author_tz)),
                summary: Some(m.summary.clone()),
            }
        };
        regions.push(region);
    }
    regions
}

/// A porcelain header line: 40 hex chars, then orig and final line numbers.
fn parse_header(line: &str) -> Option<(String, u32)> {
    let mut parts = line.split(' ');
    let oid = parts.next()?;
    if oid.len() != 40 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let _orig: u32 = parts.next()?.parse().ok()?;
    let final_line: u32 = parts.next()?.parse().ok()?;
    Some((oid.to_string(), final_line))
}

/// First commit that ADDED the path, following renames: the LAST line of
/// `git log --follow --diff-filter=A` output is the oldest such commit.
fn introduced_by(runner: &GitRunner, file: &str) -> Option<Value> {
    let out = runner
        .run(&[
            "log",
            "--follow",
            "--diff-filter=A",
            "--format=%H|%an|%ae|%aI|%s",
            "--",
            file,
        ])
        .ok()?;
    let text = String::from_utf8_lossy(&out);
    let last = text.lines().filter(|l| !l.trim().is_empty()).last()?;
    parse_commit_line(last)
}

/// Newest commit touching the path (rename-following).
fn last_modified(runner: &GitRunner, file: &str) -> Option<Value> {
    let out = runner
        .run(&[
            "log",
            "--follow",
            "-1",
            "--format=%H|%an|%ae|%aI|%s",
            "--",
            file,
        ])
        .ok()?;
    let text = String::from_utf8_lossy(&out);
    let first = text.lines().find(|l| !l.trim().is_empty())?;
    parse_commit_line(first)
}

/// Parse a `%H|%an|%ae|%aI|%s` line. `splitn(5, ..)` keeps any `|` inside
/// the subject intact.
fn parse_commit_line(line: &str) -> Option<Value> {
    let fields: Vec<&str> = line.splitn(5, '|').collect();
    if fields.len() != 5 {
        return None;
    }
    Some(json!({
        "oid": fields[0],
        "author": fields[1],
        "author_mail": fields[2],
        "author_time": fields[3],
        "summary": fields[4],
    }))
}

/// Format a unix epoch + git tz string ("+0200") as ISO-8601 with offset,
/// e.g. `2026-08-31T14:03:05+02:00`. No chrono dependency: Howard
/// Hinnant's civil-from-days algorithm.
fn epoch_to_iso(epoch: i64, tz: &str) -> String {
    let offset_secs = parse_tz_offset(tz);
    let local = epoch + i64::from(offset_secs);
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let abs = offset_secs.unsigned_abs();
    format!(
        "{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}{sign}{:02}:{:02}",
        abs / 3600,
        (abs % 3600) / 60
    )
}

/// "+0200" -> 7200, "-0530" -> -19800; anything unparseable -> 0 (UTC).
fn parse_tz_offset(tz: &str) -> i32 {
    let tz = tz.trim();
    if tz.len() != 5 {
        return 0;
    }
    let sign = match tz.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return 0,
    };
    let (Ok(h), Ok(m)) = (tz[1..3].parse::<i32>(), tz[3..5].parse::<i32>()) else {
        return 0;
    };
    sign * (h * 3600 + m * 60)
}

/// Days-since-epoch to (year, month, day) — Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_to_iso_utc() {
        assert_eq!(epoch_to_iso(0, "+0000"), "1970-01-01T00:00:00+00:00");
    }

    #[test]
    fn epoch_to_iso_positive_offset() {
        // 2021-01-01T00:00:00Z == 1609459200; at +0200 it is 02:00 local.
        assert_eq!(
            epoch_to_iso(1_609_459_200, "+0200"),
            "2021-01-01T02:00:00+02:00"
        );
    }

    #[test]
    fn epoch_to_iso_negative_offset() {
        assert_eq!(
            epoch_to_iso(1_609_459_200, "-0530"),
            "2020-12-31T18:30:00-05:30"
        );
    }

    #[test]
    fn tz_offset_garbage_is_utc() {
        assert_eq!(parse_tz_offset("zzz"), 0);
        assert_eq!(parse_tz_offset(""), 0);
    }

    #[test]
    fn header_parse_rejects_metadata_lines() {
        assert!(parse_header("author Livio").is_none());
        assert!(parse_header("author-time 1693000000").is_none());
        let (oid, fin) =
            parse_header("0123456789abcdef0123456789abcdef01234567 3 7 2").unwrap();
        assert_eq!(oid, "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(fin, 7);
    }
}
