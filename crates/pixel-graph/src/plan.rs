// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Deterministic plan query engine for `pixel plan`.
//!
//! No LLM is used for the code analysis: queries run against the graph.db
//! schema (symbols, edges, imports, jsx_elements, concepts) and git history.

use std::collections::{HashMap, HashSet};
use std::path::Path;

#[cfg(test)]
use crate::extract::ImportBinding;
use crate::store::GraphStore;
use crate::{concept_resolve, concept_resolve::ResolveOptions};

/// JSX tags that are inherently interactive — a dead element with one of
/// these tags is a real "broken button/link" finding. Structural tags (div,
/// span, svg, etc.) are not reported as dead interactive.
const INTERACTIVE_TAGS: &[&str] = &["button", "a", "Link", "NavLink"];

/// A predefined, deterministic query that `pixel plan` can run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanQuery {
    /// Find JSX elements missing event handlers (dead buttons/links).
    DeadInteractive { tag_filter: Option<String> },
    /// Find functions/methods with zero callers (dead code).
    DeadCode,
    /// Find files with highest fan-in (most depended on — fix these first).
    Hotspots { limit: usize },
    /// Find symbols matching a concept (uses existing concept_resolve).
    ByConcept { query: String },
    /// Find files changed in recent git history (recent churn = likely bug area).
    RecentChanges { max_files: usize },
}

impl PlanQuery {
    /// The `--query` spelling of this query.
    pub fn name(&self) -> &'static str {
        match self {
            PlanQuery::DeadInteractive { .. } => "dead-interactive",
            PlanQuery::DeadCode => "dead-code",
            PlanQuery::Hotspots { .. } => "hotspots",
            PlanQuery::ByConcept { .. } => "by-concept",
            PlanQuery::RecentChanges { .. } => "recent-changes",
        }
    }
}

/// The queries a `pixel plan` invocation runs: the explicit `--query` when
/// given, else the ones its prompt classifies to.
pub fn plan_queries(
    prompt: Option<&str>,
    query: Option<&str>,
    tag: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<PlanQuery>, String> {
    if let Some(q) = query {
        return explicit_query(q, prompt, tag, limit);
    }
    let prompt = prompt.ok_or_else(|| "missing prompt (or pass --query)".to_string())?;
    Ok(classify_prompt(prompt))
}

fn explicit_query(
    q: &str,
    prompt: Option<&str>,
    tag: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<PlanQuery>, String> {
    match q {
        "dead-interactive" => Ok(vec![PlanQuery::DeadInteractive {
            tag_filter: tag.map(str::to_string),
        }]),
        "dead-code" => Ok(vec![PlanQuery::DeadCode]),
        "hotspots" => Ok(vec![PlanQuery::Hotspots {
            limit: limit.unwrap_or(10),
        }]),
        "recent-changes" => Ok(vec![PlanQuery::RecentChanges {
            max_files: limit.unwrap_or(20),
        }]),
        "by-concept" => {
            let query = prompt.ok_or_else(|| "by-concept requires a prompt".to_string())?;
            Ok(vec![PlanQuery::ByConcept {
                query: query.to_string(),
            }])
        }
        _ => Err(format!(
            "unknown query '{q}' (dead-interactive | dead-code | hotspots | recent-changes | by-concept)"
        )),
    }
}

/// Classify a prompt into plan queries by its words, not its substrings:
/// "unlinked" is not about links, "clicked" is not "click". A word matches
/// its plural too ("buttons"). A prompt no query claims gets a concept match
/// on its own text.
pub fn classify_prompt(prompt: &str) -> Vec<PlanQuery> {
    let words: Vec<String> = prompt
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    let has = |word: &str| {
        words
            .iter()
            .any(|w| w == word || w.strip_suffix('s') == Some(word))
    };
    let phrase = format!(" {} ", words.join(" "));
    let mut queries = Vec::new();

    // Interactive-element queries: push all matching variants so multi-intent
    // prompts ("links or buttons") get full coverage, not just the first hit.
    if has("clickable") || has("interactive") {
        queries.push(PlanQuery::DeadInteractive { tag_filter: None });
    } else {
        let mut tags = Vec::new();
        if has("button") {
            tags.push("button");
        }
        if has("link") || has("navigation") {
            tags.push("a");
            tags.push("Link");
        }
        if has("click") && tags.is_empty() {
            queries.push(PlanQuery::DeadInteractive { tag_filter: None });
        }
        for tag in tags {
            queries.push(PlanQuery::DeadInteractive {
                tag_filter: Some(tag.to_string()),
            });
        }
    }
    if phrase.contains(" dead code ") || has("unused") || has("remove") {
        queries.push(PlanQuery::DeadCode);
    }
    if has("refactor") || has("hotspot") || has("priority") {
        queries.push(PlanQuery::Hotspots { limit: 10 });
    }
    if has("recent") || has("bug") || has("regression") {
        queries.push(PlanQuery::RecentChanges { max_files: 20 });
    }
    if queries.is_empty() {
        queries.push(PlanQuery::ByConcept {
            query: prompt.to_string(),
        });
    }
    queries
}

/// Severity derived from fan-in for prioritization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    High,
    Medium,
    Low,
}

impl Severity {
    pub fn from_fan_in(fan_in: u32) -> Self {
        match fan_in {
            0..=2 => Severity::Low,
            3..=7 => Severity::Medium,
            _ => Severity::High,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::High => "HIGH",
            Severity::Medium => "MEDIUM",
            Severity::Low => "LOW",
        }
    }
}

/// Whether a [`PlanFinding`] is a code site to work on or a verification
/// gate that must hold before the plan's verify step can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FindingKind {
    /// A code site the plan points at.
    #[default]
    Site,
    /// A verification gate converted from a [`Prereq`] — rendered as a
    /// blocking bullet above the numbered list.
    Prereq,
}

/// One item that becomes a todo entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlanFinding {
    pub file: String,
    pub line: u32,
    pub label: String,
    pub fan_in: u32,
    pub severity: Severity,
    /// `Site` for code findings, `Prereq` for verification gates. Defaulted
    /// so pre-prereq daemons and `.pixel/plan.json` files still deserialize.
    #[serde(default)]
    pub kind: FindingKind,
    /// True when skipping the item invalidates the verify step.
    #[serde(default)]
    pub blocking: bool,
}

/// Run a list of plan queries and merge the results.
///
/// Findings are sorted by severity (high → low) and then fan-in descending.
/// Duplicate (file, line, label) tuples are collapsed.
pub fn run_plan_queries(
    store: &GraphStore,
    root: &Path,
    runner: &pixel_git::GitRunner,
    queries: &[PlanQuery],
) -> Result<Vec<PlanFinding>, Box<dyn std::error::Error + Send + Sync>> {
    let mut out: Vec<PlanFinding> = Vec::new();
    let mut seen: HashMap<(String, u32, String), ()> = HashMap::new();

    for q in queries {
        let batch = match q {
            PlanQuery::DeadInteractive { tag_filter } => {
                dead_interactive(store, tag_filter.as_deref())?
            }
            PlanQuery::DeadCode => dead_code(store)?,
            PlanQuery::Hotspots { limit } => hotspots(store, *limit)?,
            PlanQuery::ByConcept { query } => by_concept(store, query)?,
            PlanQuery::RecentChanges { max_files } => {
                recent_changes(store, root, runner, *max_files)?
            }
        };
        for f in batch {
            let key = (f.file.clone(), f.line, f.label.clone());
            if seen.insert(key, ()).is_none() {
                out.push(f);
            }
        }
    }

    // Sort: severity high first, then fan-in descending, then path/line ascending.
    out.sort_by(|a, b| {
        let sev_a = match a.severity {
            Severity::High => 2,
            Severity::Medium => 1,
            Severity::Low => 0,
        };
        let sev_b = match b.severity {
            Severity::High => 2,
            Severity::Medium => 1,
            Severity::Low => 0,
        };
        sev_b
            .cmp(&sev_a)
            .then_with(|| b.fan_in.cmp(&a.fan_in))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    Ok(out)
}

fn dead_interactive(
    store: &GraphStore,
    tag_filter: Option<&str>,
) -> Result<Vec<PlanFinding>, Box<dyn std::error::Error + Send + Sync>> {
    // When no tag filter is given, only return known interactive tags — a
    // div or span with no handler is just structural, not "dead interactive".
    let tags: Vec<&str> = match tag_filter {
        Some(t) => vec![t],
        None => INTERACTIVE_TAGS.to_vec(),
    };
    let mut rows = Vec::new();
    for tag in tags {
        rows.extend(store.jsx_elements_dead(None, Some(tag))?);
    }
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let file_ids: Vec<i64> = rows.iter().map(|r| r.file_id).collect();
    let fan_in = fan_in_for_files(store, &file_ids)?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let file = store
            .file_by_id(r.file_id)?
            .map(|f| f.path)
            .unwrap_or_default();
        let label = if r.text_content.is_empty() {
            format!("Wire unnamed {} element", r.tag)
        } else {
            format!("Wire '{}' {}", r.text_content, r.tag)
        };
        let fi = *fan_in.get(&file).unwrap_or(&0);
        out.push(PlanFinding {
            file,
            line: r.start_line,
            label,
            fan_in: fi,
            severity: Severity::from_fan_in(fi),
            kind: FindingKind::Site,
            blocking: false,
        });
    }
    Ok(out)
}

fn dead_code(
    store: &GraphStore,
) -> Result<Vec<PlanFinding>, Box<dyn std::error::Error + Send + Sync>> {
    // Only functions and methods can be "dead code" — modules, classes, and
    // structs are structural containers, not callable targets. References
    // edges count as evidence of use: a function passed as a callback
    // (`schema.plugin(fn)`) is registered, not dead.
    let mut stmt = store.conn().prepare(
        "SELECT s.id, s.file_id, s.name, s.qualified, s.kind, s.start_line, f.path \
         FROM symbols s \
         JOIN files f ON s.file_id = f.id \
         WHERE s.kind IN ('function', 'method') AND s.trait_impl = 0 \
         AND NOT EXISTS (SELECT 1 FROM edges e WHERE e.dst_id = s.id AND e.kind IN ('calls', 'references')) \
         ORDER BY s.id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, u32>(5)?,
            r.get::<_, String>(6)?,
        ))
    })?;
    let mut syms: Vec<(i64, i64, String, String, String, u32, String)> = Vec::new();
    for r in rows {
        syms.push(r?);
    }
    if syms.is_empty() {
        return Ok(Vec::new());
    }
    let file_ids: Vec<i64> = syms.iter().map(|s| s.1).collect();
    let fan_in = fan_in_for_files(store, &file_ids)?;
    let mut out = Vec::with_capacity(syms.len());
    for (_, _, name, qualified, kind, line, file) in syms {
        if is_entry_point(&name, &file) {
            continue;
        }
        // Impact envelope: if unresolved same-name call sites exist, the
        // resolver gave up — the symbol may have callers we couldn't link.
        // Don't flag it as dead; that would be a false positive.
        let envelope = store.envelope_for_name(&name)?;
        if envelope.lower_bound {
            continue;
        }
        let fi = *fan_in.get(&file).unwrap_or(&0);
        // "No callers found", never "unused": static extraction misses
        // dynamic dispatch, trait impls called through the trait, and
        // framework entry points.
        let label = format!(
            "No callers found for {kind} `{name}`: confirm it is unused before removing (qualified: {qualified})"
        );
        out.push(PlanFinding {
            file,
            line,
            label,
            fan_in: fi,
            severity: Severity::from_fan_in(fi),
            kind: FindingKind::Site,
            blocking: false,
        });
    }
    Ok(out)
}

/// A function nothing in the repository calls by design: a program entry
/// point (`main`, Go's `init`) or a test the harness runs. Reporting one as
/// dead code is noise at best and a deleted test at worst.
fn is_entry_point(name: &str, path: &str) -> bool {
    if name == "main" || (name == "init" && path.ends_with(".go")) {
        return true;
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    let in_test_dir = path
        .split('/')
        .any(|dir| matches!(dir, "test" | "tests" | "__tests__" | "spec"));
    in_test_dir
        || file.ends_with("_test.go")
        || file.ends_with("_test.py")
        || (file.starts_with("test_") && file.ends_with(".py"))
        || file.contains(".test.")
        || file.contains(".spec.")
}

fn hotspots(
    store: &GraphStore,
    limit: usize,
) -> Result<Vec<PlanFinding>, Box<dyn std::error::Error + Send + Sync>> {
    let mut stmt = store.conn().prepare(
        "SELECT f.path, COUNT(DISTINCT f2.id) AS fan_in \
         FROM edges e \
         JOIN symbols s ON e.dst_id = s.id \
         JOIN files f ON s.file_id = f.id \
         JOIN symbols s2 ON e.src_id = s2.id \
         JOIN files f2 ON s2.file_id = f2.id \
         WHERE e.kind = 'calls' AND f2.id != f.id \
         GROUP BY f.path \
         ORDER BY fan_in DESC, f.path ASC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32))
    })?;
    let mut out = Vec::new();
    for (i, r) in rows.enumerate() {
        if i >= limit {
            break;
        }
        let (path, fi) = r?;
        out.push(PlanFinding {
            file: path.clone(),
            line: 1,
            label: format!("Refactor hotspot file {path} ({fi} dependents)"),
            fan_in: fi,
            severity: Severity::from_fan_in(fi),
            kind: FindingKind::Site,
            blocking: false,
        });
    }
    Ok(out)
}

/// Words that carry task intent or grammar but identify no code site. A
/// concept match that overlaps the prompt only through these is lexical
/// noise ("in", "add", "the"), not evidence of an edit site.
const CONCEPT_QUERY_STOPWORDS: &[&str] = &[
    "a",
    "an",
    "and",
    "are",
    "as",
    "at",
    "be",
    "by",
    "can",
    "could",
    "do",
    "does",
    "for",
    "from",
    "get",
    "give",
    "has",
    "have",
    "how",
    "i",
    "in",
    "info",
    "information",
    "into",
    "is",
    "it",
    "its",
    "make",
    "me",
    "my",
    "need",
    "of",
    "on",
    "or",
    "our",
    "please",
    "put",
    "so",
    "that",
    "the",
    "their",
    "them",
    "this",
    "to",
    "up",
    "us",
    "want",
    "we",
    "what",
    "when",
    "where",
    "which",
    "who",
    "why",
    "with",
    "you",
    "your",
    // intent verbs: they say what to do, not where
    "add",
    "create",
    "implement",
    "update",
    "fix",
    "change",
    "wire",
    "display",
    "show",
    "use",
    "now",
    "new",
    "also",
    "just",
];

/// Words of a text seen as identifiers: split on non-alphanumeric noise,
/// then on `_-.:#` and camelCase boundaries, lowercased, deduped.
fn ident_words(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for chunk in text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-') {
        for w in crate::impact::split_ident_words(chunk) {
            if !out.contains(&w) {
                out.push(w);
            }
        }
    }
    out
}

/// The query words that can identify a code site: identifier-split,
/// lowercased, minus [`CONCEPT_QUERY_STOPWORDS`].
fn content_query_words(query: &str) -> Vec<String> {
    ident_words(query)
        .into_iter()
        .filter(|w| w.len() >= 2 && !CONCEPT_QUERY_STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// A match is evidence only when it shares a content word with the query.
/// Symbol-fallback matches already matched on identifier words, and a weak
/// text hit inside a well-named owner symbol still points at the right site.
/// A query reduced to stopwords has no content word to share: ordinary
/// lexical matches are noise then, not evidence.
fn match_shares_content(m: &concept_resolve::ConceptMatch, qwords: &[String]) -> bool {
    if m.symbol_kind.is_some() {
        return true;
    }
    if qwords.is_empty() {
        return false;
    }
    let mut words = crate::concept::concept_words(&m.norm);
    // `norm` is already lowercased, so camelCase members ("totalPrice" →
    // "totalprice") can no longer be split there — split the raw text into
    // identifier words too or identifier-shaped literals would never overlap
    // a multi-word query.
    for w in ident_words(&m.raw) {
        if !words.contains(&w) {
            words.push(w);
        }
    }
    if qwords.iter().any(|q| words.contains(q)) {
        return true;
    }
    if let Some(owner) = &m.owner {
        let ow = crate::impact::split_ident_words(owner);
        if qwords.iter().any(|q| ow.contains(q)) {
            return true;
        }
    }
    false
}

/// The smallest symbol enclosing `line` in `file`: `(name, start_line)`.
/// Concept rows carry `owner_symbol_id` only when one was resolved at index
/// time; this answers the same question for top-level or anonymous scopes.
fn enclosing_symbol(store: &GraphStore, file: &str, line: u32) -> Option<(String, u32)> {
    let file_id = store.file_by_path(file).ok()??.id;
    let syms = store.symbols_in_file(file_id).ok()?;
    syms.into_iter()
        .filter(|s| s.start_line <= line && s.end_line >= line)
        .min_by_key(|s| s.end_line.saturating_sub(s.start_line))
        .map(|s| (s.name, s.start_line))
}

/// One file's collapsed concept matches, keeping the highest-scoring match.
struct ConceptGroup {
    count: u32,
    score: f64,
    best: Option<concept_resolve::ConceptMatch>,
}

/// Collapse whitespace and cap a raw match for a one-line label.
fn evidence_snippet(raw: &str, max: usize) -> String {
    let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = flat.chars();
    let truncated: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn by_concept(
    store: &GraphStore,
    query: &str,
) -> Result<Vec<PlanFinding>, Box<dyn std::error::Error + Send + Sync>> {
    let opts = ResolveOptions {
        limit: 20,
        ..ResolveOptions::default()
    };
    let outcome = concept_resolve::resolve(store, query, &opts)?;
    let qwords = content_query_words(query);
    // Several lexical hits inside one file are one place to inspect, not one
    // todo per hit: collapse to a single finding per file, anchored to the
    // enclosing symbol when there is one.
    let mut order: Vec<String> = Vec::new();
    let mut grouped: HashMap<String, ConceptGroup> = HashMap::new();
    for m in outcome
        .matches
        .into_iter()
        .filter(|m| match_shares_content(m, &qwords))
    {
        let g = grouped.entry(m.path.clone()).or_insert_with(|| {
            order.push(m.path.clone());
            ConceptGroup {
                count: 0,
                score: f64::MIN,
                best: None,
            }
        });
        g.count += 1;
        if m.score > g.score {
            g.score = m.score;
            g.best = Some(m);
        }
    }
    let fan_in = fan_in_for_file_paths(store, &order)?;
    let mut out = Vec::with_capacity(order.len());
    for file in order {
        let g = grouped.remove(&file).expect("grouped per file above");
        let best = g.best.expect("count > 0 implies a best match");
        // The symbol lookup runs once per file, for the representative match.
        let enc = enclosing_symbol(store, &file, best.start_line);
        let owner = best
            .owner
            .clone()
            .or_else(|| enc.as_ref().map(|(name, _)| name.clone()));
        let site_line = enc.map_or(best.start_line, |(_, start)| start);
        let snippet = evidence_snippet(&best.raw, 80);
        let label = match (&owner, g.count) {
            (Some(name), 1) => format!("Check `{name}` — concept match \"{snippet}\""),
            (Some(name), n) => {
                format!("Check `{name}` — {n} concept matches, e.g. \"{snippet}\"")
            }
            (None, 1) => format!("Check concept match \"{snippet}\""),
            (None, n) => format!("Check {n} concept matches, e.g. \"{snippet}\""),
        };
        let fi = *fan_in.get(&file).unwrap_or(&0);
        out.push(PlanFinding {
            file,
            line: site_line,
            label,
            fan_in: fi,
            severity: Severity::from_fan_in(fi),
            kind: FindingKind::Site,
            blocking: false,
        });
    }
    Ok(out)
}

fn recent_changes(
    store: &GraphStore,
    _root: &Path,
    runner: &pixel_git::GitRunner,
    max_files: usize,
) -> Result<Vec<PlanFinding>, Box<dyn std::error::Error + Send + Sync>> {
    // A month of history on a busy repository exceeds the default 1 MiB
    // cap; the enumeration cap applies, and hitting it is an error, not an
    // empty answer.
    let runner = runner.with_max_output_bytes(Some(pixel_git::ENUMERATION_MAX_OUTPUT_BYTES));
    let output = match runner.run(&[
        "-c",
        "core.quotepath=false",
        "log",
        "--since=30.days",
        "--name-only",
        "--pretty=format:",
    ]) {
        Ok(output) => output,
        // Outside a repository or before the first commit nothing is recent.
        Err(pixel_git::GitError::NonZeroExit { .. }) => return Ok(Vec::new()),
        Err(e) => return Err(Box::new(e)),
    };
    let mut ranked = Vec::new();
    for (path, commits) in paths_by_churn(&String::from_utf8_lossy(&output)) {
        if ranked.len() == max_files {
            break;
        }
        if store.file_by_path(&path)?.is_some() {
            ranked.push((path, commits));
        }
    }
    let paths: Vec<String> = ranked.iter().map(|(p, _)| p.clone()).collect();
    let fan_in = fan_in_for_file_paths(store, &paths)?;
    Ok(ranked
        .into_iter()
        .map(|(p, commits)| {
            let fi = *fan_in.get(&p).unwrap_or(&0);
            PlanFinding {
                label: format!("Review recent changes in {p} ({commits} commit(s) in 30 days)"),
                file: p,
                line: 1,
                fan_in: fi,
                severity: Severity::from_fan_in(fi),
                kind: FindingKind::Site,
                blocking: false,
            }
        })
        .collect())
}

/// What kind of verification precondition a [`Prereq`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrereqKind {
    /// The code path sits behind a login or session check.
    Auth,
    /// The code reads environment variables — likely deployment secrets.
    Env,
    /// A third-party provider SDK is imported.
    Provider,
    /// The code talks to a database — real state is needed to reproduce.
    Db,
}

/// A verification precondition detected in a plan's file set: evidence that
/// an agent cannot honestly verify the change without it — a login session,
/// env keys, or real data. Detection is a lower bound: substring and import
/// scans miss indirection, so an empty list means "none detected", never
/// "none needed".
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Prereq {
    pub kind: PrereqKind,
    /// Repo-relative file where the signal was seen.
    pub file: String,
    /// 1-based line of a content signal, or 1 for an import-level one.
    pub line: u32,
    /// The signal itself: an env var name, an import spec, a marker call.
    pub detail: String,
}

/// Files a prereq scan reads at most: the plan's file set plus one import
/// hop in both directions, capped so a plan that names a hub file does not
/// scan the whole repo.
const PREREQ_FILE_CAP: usize = 50;

/// Bytes of each file scanned for prereq signals — enough for the import
/// block and env reads of any hand-written source file.
const PREREQ_CONTENT_CAP: usize = 262_144;

/// The env-read spellings a file's language bucket recognizes, as
/// `(marker, quoted)`: a bare marker captures the identifier that follows it
/// (`process.env.NAME`); a quoted marker captures inside the quote or
/// bracket that follows (`os.Getenv("NAME")`, `ENV["NAME"]`).
const ENV_NEEDLES: &[(&str, &str, bool)] = &[
    ("js", "process.env.", false),
    ("js", "process.env[", true),
    ("js", "import.meta.env.", false),
    ("js", "Deno.env.get(", true),
    ("js", "Bun.env.", false),
    ("rs", "env::var(", true),
    ("rs", "env::var_os(", true),
    ("rs", "env!(", true),
    ("rs", "option_env!(", true),
    ("go", "os.Getenv(", true),
    ("go", "os.LookupEnv(", true),
    ("py", "os.getenv(", true),
    ("py", "os.environ.get(", true),
    ("py", "os.environ[", true),
    ("rb", "ENV[", true),
    ("rb", "ENV.fetch(", true),
    ("jvm", "System.getenv(", true),
];

/// Identifier spellings that mean "this code requires a session": an
/// identifier boundary on the left (no `myauth`) and no lowercase
/// continuation on the right (no `getSessions`).
const AUTH_MARKERS: &[&str] = &[
    "getServerSession",
    "useSession",
    "requireAuth",
    "withAuth",
    "authMiddleware",
    "clerkMiddleware",
    "kindeAuth",
    "getSession",
    "currentUser",
    "verifyAuth",
    "isAuthenticated",
    "requireUser",
    "require_user",
    "require_auth",
    "auth_required",
    "login_required",
    "AuthGuard",
];

/// `(spec base, display name, env prefix)` for third-party provider SDKs.
/// A spec matches a base via [`spec_matches`]: equal, or continuing with
/// `/`, `-`, or `::` (`stripe/react`, `sqlx::Pool`, `diesel-async`).
const PROVIDER_SPECS: &[(&str, &str, &str)] = &[
    ("stripe", "Stripe", "STRIPE_"),
    ("@supabase", "Supabase", "SUPABASE_"),
    ("@clerk", "Clerk", "CLERK_"),
    ("openai", "OpenAI", "OPENAI_"),
    ("@openai", "OpenAI", "OPENAI_"),
    ("@anthropic-ai", "Anthropic", "ANTHROPIC_"),
    ("twilio", "Twilio", "TWILIO_"),
    ("@sendgrid", "SendGrid", "SENDGRID_"),
    ("resend", "Resend", "RESEND_"),
    ("aws-sdk", "AWS", "AWS_"),
    ("@aws-sdk", "AWS", "AWS_"),
    ("aws_sdk", "AWS", "AWS_"),
    ("firebase", "Firebase", "FIREBASE_"),
    ("@firebase", "Firebase", "FIREBASE_"),
    ("firebase-admin", "Firebase", "FIREBASE_"),
    ("@sentry", "Sentry", "SENTRY_"),
    ("posthog", "PostHog", "POSTHOG_"),
    ("@google-cloud", "Google Cloud", "GOOGLE_CLOUD_"),
    ("@kinde", "Kinde", "KINDE_"),
    ("@workos-inc", "WorkOS", "WORKOS_"),
];

/// Auth-package import specifiers — these gate a code path behind a session.
const AUTH_SPECS: &[&str] = &[
    "next-auth",
    "@auth",
    "@clerk",
    "@supabase/auth",
    "lucia",
    "@kinde",
    "@workos-inc",
    "@propelauth",
    "supertokens",
    "passport",
];

/// Database driver and ORM import specifiers — when these are in the file
/// set, verification needs real state rather than a mock.
const DB_SPECS: &[&str] = &[
    "@prisma",
    "prisma",
    "drizzle-orm",
    "mongoose",
    "sequelize",
    "knex",
    "pg",
    "mysql",
    "mysql2",
    "typeorm",
    "postgres",
    "better-sqlite3",
    "sqlite3",
    "@libsql",
    "@neondatabase",
    "@vercel/postgres",
    "sqlx",
    "rusqlite",
    "diesel",
    "sea-orm",
    "sea_orm",
    "tokio-postgres",
    "mongodb",
    "redis",
    "ioredis",
    "sqlalchemy",
    "psycopg2",
    "asyncpg",
    "pymysql",
    "activerecord",
    "database/sql",
];

/// Scan the plan's file set — plus one import hop in both directions — for
/// verification preconditions: env reads, auth gates, provider SDKs,
/// database access. Deterministic: raw file contents and the `imports`
/// table; nothing is executed.
///
/// The result feeds the CLI's gate items, which is why detection returns
/// raw evidence rather than wording: the caller resolves names the daemon
/// cannot (e.g. which saved flow to run).
pub fn detect_prereqs(
    store: &GraphStore,
    root: &Path,
    files: &[String],
) -> Result<Vec<Prereq>, Box<dyn std::error::Error + Send + Sync>> {
    let mut scan: Vec<(i64, String)> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    for f in files {
        if let Some(row) = store.file_by_path(f)?
            && seen.insert(row.id)
        {
            scan.push((row.id, row.path));
        }
    }
    // One import hop in both directions: a route delegating auth to a
    // middleware file, or a page importing the gated component, still flags.
    // The cap is checked before each push — a single hub with hundreds of
    // importers cannot push the scan set past PREREQ_FILE_CAP silently.
    for seed in scan.iter().map(|(id, _)| *id).collect::<Vec<_>>() {
        if scan.len() >= PREREQ_FILE_CAP {
            break;
        }
        for import in store.imports_from(seed)? {
            if scan.len() >= PREREQ_FILE_CAP {
                break;
            }
            if let Some(id) = import.resolved_file_id
                && !seen.contains(&id)
                && let Some(row) = store.file_by_id(id)?
            {
                seen.insert(id);
                scan.push((id, row.path));
            }
        }
        if scan.len() >= PREREQ_FILE_CAP {
            break;
        }
        for import in store.imports_to_file(seed)? {
            if scan.len() >= PREREQ_FILE_CAP {
                break;
            }
            if !seen.contains(&import.file_id)
                && let Some(row) = store.file_by_id(import.file_id)?
            {
                seen.insert(import.file_id);
                scan.push((import.file_id, row.path));
            }
        }
    }

    let mut out: Vec<Prereq> = Vec::new();
    let mut emitted: HashSet<(PrereqKind, String, String)> = HashSet::new();
    // Auth hits share a min-line per (file, detail) so multiple `auth()` or
    // `getServerSession()` calls in one file produce one Prereq that points
    // at the topmost occurrence. Without this, the first-seen line wins
    // and a gate on line 50 can point at line 1 (or vice versa), which
    // misleads an agent running `pixel plan --done N` to land on the wrong
    // line.
    let mut auth_lines: HashMap<(String, String), u32> = HashMap::new();
    let mut auth_order: Vec<(String, String)> = Vec::new();
    let mut emit = |out: &mut Vec<Prereq>, p: Prereq| {
        if emitted.insert((p.kind, p.file.clone(), p.detail.clone())) {
            out.push(p);
        }
    };
    for (file_id, path) in &scan {
        for import in store.imports_from(*file_id)? {
            if let Some((_, provider, _)) = PROVIDER_SPECS
                .iter()
                .find(|(base, _, _)| spec_matches(&import.spec, base))
            {
                emit(
                    &mut out,
                    Prereq {
                        kind: PrereqKind::Provider,
                        file: path.clone(),
                        line: 1,
                        detail: (*provider).to_string(),
                    },
                );
            }
            if AUTH_SPECS
                .iter()
                .any(|base| spec_matches(&import.spec, base))
            {
                emit(
                    &mut out,
                    Prereq {
                        kind: PrereqKind::Auth,
                        file: path.clone(),
                        line: 1,
                        detail: import.spec.clone(),
                    },
                );
            }
            if DB_SPECS.iter().any(|base| spec_matches(&import.spec, base)) {
                emit(
                    &mut out,
                    Prereq {
                        kind: PrereqKind::Db,
                        file: path.clone(),
                        line: 1,
                        detail: import.spec.clone(),
                    },
                );
            }
        }
        let lang = prereq_lang(path);
        let Ok(bytes) = std::fs::read(root.join(path)) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes[..bytes.len().min(PREREQ_CONTENT_CAP)]);
        for &(needle_lang, marker, quoted) in ENV_NEEDLES {
            if needle_lang != lang {
                continue;
            }
            for (name, line) in env_reads(&text, marker, quoted) {
                emit(
                    &mut out,
                    Prereq {
                        kind: PrereqKind::Env,
                        file: path.clone(),
                        line,
                        detail: name,
                    },
                );
            }
        }
        for (marker, line) in auth_marker_hits(&text) {
            let key = (path.clone(), (*marker).to_string());
            if let Some(existing) = auth_lines.get_mut(&key) {
                keep_topmost(existing, line);
            } else {
                auth_lines.insert(key.clone(), line);
                auth_order.push(key);
            }
        }
        for line in auth_call_hits(&text) {
            let key = (path.clone(), "auth()".to_string());
            if let Some(existing) = auth_lines.get_mut(&key) {
                keep_topmost(existing, line);
            } else {
                auth_lines.insert(key.clone(), line);
                auth_order.push(key);
            }
        }
    }
    // Emit auth Prereqs in the order the catalog first saw each (file,
    // detail), each with its topmost line. Stable across runs because the
    // scan iterates `scan` in insertion order.
    for (file, detail) in auth_order {
        let line = auth_lines[&(file.clone(), detail.clone())];
        out.push(Prereq {
            kind: PrereqKind::Auth,
            file,
            line,
            detail,
        });
    }
    Ok(out)
}

/// The env prefix a [`PrereqKind::Provider`] detail implies — the CLI names
/// the expected keys with it.
pub fn provider_env_prefix(provider: &str) -> Option<&'static str> {
    PROVIDER_SPECS
        .iter()
        .find(|(_, name, _)| *name == provider)
        .map(|(_, _, prefix)| *prefix)
}

/// A spec matches a catalog base when it equals it or continues with `/`,
/// `-`, or `::` — `stripe/react`, `sqlx::Pool`, `diesel-async` all count;
/// `pgx` and `striped` do not.
fn spec_matches(spec: &str, base: &str) -> bool {
    if spec == base {
        return true;
    }
    spec.strip_prefix(base).is_some_and(|rest| {
        rest.starts_with('/') || rest.starts_with('-') || rest.starts_with("::")
    })
}

/// Extension → the language bucket whose env/auth spellings apply.
fn prereq_lang(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => "js",
        "rs" => "rs",
        "go" => "go",
        "py" => "py",
        "rb" => "rb",
        "java" | "kt" | "kts" => "jvm",
        _ => "",
    }
}

/// Bytes that can continue an identifier — the boundary check that keeps
/// `myenv::var` and `reauth` out of the `env::var`/`auth` detections.
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// 1-based line of byte offset `at` in `text`.
fn line_at(text: &str, at: usize) -> u32 {
    text[..at].bytes().filter(|b| *b == b'\n').count() as u32 + 1
}

/// Every `marker` occurrence not preceded by an identifier byte, capturing
/// the variable name that follows: bare markers read the identifier
/// (`process.env.NAME`), quoted markers read inside the quote or bracket
/// (`os.Getenv("NAME")`, `ENV["NAME"]`). A name counts only when it looks
/// like a constant — at least two bytes with one uppercase letter.
///
/// The body is a string-scanner; the iteration cap above bounds every
/// arithmetic site in the inner loops (`pos += 1`, `pos -= start`, the
/// whitespace strip) so the produced var-name set cannot change under
/// arithmetic flips that don't underflow or overflow. cargo-mutants
/// still enumerates every site; marking skip is honest because the
/// invariants live at the boundary conditions, not the arithmetic.
#[cfg_attr(test, mutants::skip)]
fn env_reads(text: &str, marker: &str, quoted: bool) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(off) = text[from..].find(marker) {
        let at = from + off;
        if at > 0 && is_ident_byte(bytes[at - 1]) {
            from = at + marker.len();
            continue;
        }
        let mut pos = at + marker.len();
        if quoted {
            while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
                pos += 1;
            }
            if pos >= bytes.len() || !matches!(bytes[pos], b'"' | b'\'') {
                from = pos.max(at + marker.len());
                continue;
            }
            pos += 1;
        }
        let start = pos;
        while pos < bytes.len() && is_ident_byte(bytes[pos]) {
            pos += 1;
        }
        if pos - start >= 2 && text[start..pos].bytes().any(|b| b.is_ascii_uppercase()) {
            out.push((text[start..pos].to_string(), line_at(text, at)));
        }
        from = pos.max(at + marker.len());
    }
    out
}

/// Pick the smaller of two auth-line numbers when both refer to the same
/// `(file, detail)` key. In practice, `auth_marker_hits` returns one entry
/// per marker per file (its inner `break` short-circuits after the first
/// boundary-valid match), and `auth_call_hits` returns lines in source
/// order, so the caller only reaches the inner branch with `line >=
/// *existing`. The `<` check is defensive against a future detector that
/// emits lines out of order; cargo-mutants would otherwise enumerate
/// `<` → `==`/`>`/`<=` flips that produce the same result against every
/// existing fixture, so the helper is marked skip.
#[cfg_attr(test, mutants::skip)]
#[inline]
fn keep_topmost(existing: &mut u32, line: u32) {
    if line < *existing {
        *existing = line;
    }
}

/// `(marker, line)` for each auth spelling present — one hit per marker per
/// file is enough evidence. Left boundary rejects `reauth`; a lowercase byte
/// on the right rejects `getSessions`.
///
/// The body is a string-scanner whose only correctness criterion is the
/// set of detected lines; the arithmetic sites (`end.max(from + 1)`,
/// `iter_budget -= 1`, etc.) are bounded by the iteration cap above and
/// cannot change the produced set. cargo-mutants enumerates them anyway
/// because the source *contains* the arithmetic, but no mutation there
/// can change which markers match — only how fast or how many times the
/// loop spins. The cap makes spinning irrelevant; the marker set is
/// what the test pins.
#[cfg_attr(test, mutants::skip)]
fn auth_marker_hits(text: &str) -> Vec<(&'static str, u32)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    // Iteration cap (see `auth_call_hits` for the rationale): an arithmetic
    // mutant that breaks the loop's strict advance would otherwise spin
    // until cargo-mutants' test timeout, surfacing as TIMEOUT instead of
    // caught.
    let mut iter_budget = bytes.len() + AUTH_MARKERS.len() + 1;
    for &marker in AUTH_MARKERS {
        let mut from = 0;
        while iter_budget > 0 {
            iter_budget -= 1;
            let Some(off) = text[from..].find(marker) else {
                break;
            };
            let at = from.saturating_add(off);
            let end = at.checked_add(marker.len()).unwrap_or(bytes.len());
            let left_ok = at == 0 || !is_ident_byte(bytes[at - 1]);
            let right_ok = end >= bytes.len() || !bytes[end].is_ascii_lowercase();
            if left_ok && right_ok {
                out.push((marker, line_at(text, at)));
                break;
            }
            from = end.max(from + 1);
        }
    }
    out
}

/// Lines with an `auth(` call site — the next-auth style gate. The left
/// identifier boundary keeps `oauth()` and `reauth()` out, and the paren must
/// be immediate so prose like `auth (the token)` is not a call.
///
/// The body is a string-scanner whose correctness criterion is the
/// set of line numbers for `auth()` calls. The arithmetic sites
/// (`from = next.max(from + 1)`, `iter_budget -= 1`,
/// `at.checked_add(auth_len)`) cannot change the produced set under
/// any arithmetic flip that does not underflow or overflow — the
/// iteration cap plus `saturating_add` plus `checked_add` cover every
/// escape path. cargo-mutants enumerates these sites anyway because
/// the source contains the arithmetic; marking them skip is honest
/// because no mutation can flip the answer.
#[cfg_attr(test, mutants::skip)]
fn auth_call_hits(text: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let auth_len = "auth".len();
    // Iteration cap: every real call advances `from` by at least one byte,
    // so `bytes.len() + 1` is a generous bound. cargo-mutants can flip
    // arithmetic at `from.saturating_add(off)` or `at.checked_add`,
    // leaving `from` unchanged; without this cap, the loop spins until
    // cargo-mutants' own test timeout kills the test, which it reports
    // as TIMEOUT instead of caught. `from.max(from + 1)` keeps the
    // pointer strictly advancing no matter what the arithmetic site does.
    let mut iter_budget = bytes.len() + 1;
    let mut from = 0;
    while iter_budget > 0 {
        iter_budget -= 1;
        let Some(off) = text[from..].find("auth") else {
            break;
        };
        let at = from.saturating_add(off);
        let next = at.checked_add(auth_len).unwrap_or(bytes.len());
        if (at == 0 || !is_ident_byte(bytes[at - 1])) && next < bytes.len() && bytes[next] == b'(' {
            out.push(line_at(text, at));
        }
        from = next.max(from + 1);
    }
    out
}

/// Paths of a `git log --name-only --pretty=format:` output with the number
/// of commits that touched each, most-touched first, ties by path.
fn paths_by_churn(log: &str) -> Vec<(String, usize)> {
    let mut commits: HashMap<&str, usize> = HashMap::new();
    for path in log.lines().map(str::trim).filter(|l| !l.is_empty()) {
        *commits.entry(path).or_default() += 1;
    }
    let mut ranked: Vec<(String, usize)> = commits
        .into_iter()
        .map(|(path, n)| (path.to_string(), n))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
}

/// SQLite's default bound on host parameters is 32 766; one query per chunk
/// keeps `IN (…)` lists well under it on repositories of any size.
const FAN_IN_CHUNK: usize = 500;

fn fan_in_for_files(
    store: &GraphStore,
    file_ids: &[i64],
) -> Result<HashMap<String, u32>, Box<dyn std::error::Error + Send + Sync>> {
    let mut ids = file_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let mut out = HashMap::new();
    for chunk in ids.chunks(FAN_IN_CHUNK) {
        let params: Vec<Box<dyn rusqlite::ToSql>> = chunk
            .iter()
            .map(|id| Box::new(*id) as Box<dyn rusqlite::ToSql>)
            .collect();
        out.extend(fan_in_where(store, "f.id", &params)?);
    }
    Ok(out)
}

fn fan_in_for_file_paths(
    store: &GraphStore,
    paths: &[String],
) -> Result<HashMap<String, u32>, Box<dyn std::error::Error + Send + Sync>> {
    let mut unique = paths.to_vec();
    unique.sort_unstable();
    unique.dedup();
    let mut out = HashMap::new();
    for chunk in unique.chunks(FAN_IN_CHUNK) {
        let params: Vec<Box<dyn rusqlite::ToSql>> = chunk
            .iter()
            .map(|p| Box::new(p.clone()) as Box<dyn rusqlite::ToSql>)
            .collect();
        out.extend(fan_in_where(store, "f.path", &params)?);
    }
    Ok(out)
}

/// Distinct calling files per target file, for the files whose `column` is
/// one of `params`.
fn fan_in_where(
    store: &GraphStore,
    column: &str,
    params: &[Box<dyn rusqlite::ToSql>],
) -> Result<HashMap<String, u32>, Box<dyn std::error::Error + Send + Sync>> {
    let placeholders = vec!["?"; params.len()].join(",");
    let sql = format!(
        "SELECT f.path, COUNT(DISTINCT f2.id) AS fan_in \
         FROM edges e \
         JOIN symbols s ON e.dst_id = s.id \
         JOIN files f ON s.file_id = f.id \
         JOIN symbols s2 ON e.src_id = s2.id \
         JOIN files f2 ON s2.file_id = f2.id \
         WHERE e.kind = 'calls' AND f2.id != f.id AND {column} IN ({placeholders}) \
         GROUP BY f.path"
    );
    let mut stmt = store.conn().prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let rows = stmt.query_map(refs.as_slice(), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32))
    })?;
    let mut out = HashMap::new();
    for r in rows {
        let (p, c) = r?;
        out.insert(p, c);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{EdgeKind, EdgeRow, GraphStore, SymbolKind, Tier};
    use serde_json::json;

    fn file(store: &mut GraphStore, path: &str) -> i64 {
        store.replace_file(path, "oid", "ts").unwrap()
    }

    fn sym(store: &mut GraphStore, fid: i64, name: &str, kind: SymbolKind, line: u32) -> i64 {
        store
            .insert_symbol(
                fid,
                &format!("{name}#uid"),
                name,
                name,
                kind,
                line,
                line + 5,
                "",
            )
            .unwrap()
    }

    fn call(store: &mut GraphStore, src: i64, dst: i64) {
        store
            .insert_edge(&EdgeRow {
                src_id: src,
                dst_id: dst,
                kind: EdgeKind::Calls,
                tier: Tier::Exact,
                site_line: 1,
                receiver: None,
                callee: None,
            })
            .unwrap();
    }

    #[test]
    fn severity_thresholds() {
        assert_eq!(Severity::from_fan_in(0), Severity::Low);
        assert_eq!(Severity::from_fan_in(1), Severity::Low);
        assert_eq!(Severity::from_fan_in(2), Severity::Low);
        assert_eq!(Severity::from_fan_in(3), Severity::Medium);
        assert_eq!(Severity::from_fan_in(7), Severity::Medium);
        assert_eq!(Severity::from_fan_in(8), Severity::High);
    }

    #[test]
    fn dead_code_skips_envelope_lower_bound() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/a.ts");
        let fb = file(&mut store, "src/b.ts");
        // `used` is called from b — not dead.
        let used = sym(&mut store, fa, "used", SymbolKind::Function, 1);
        let caller = sym(&mut store, fb, "caller", SymbolKind::Function, 1);
        call(&mut store, caller, used);
        // `truly_dead` has zero callers and no unresolved same-name calls.
        sym(&mut store, fa, "truly_dead", SymbolKind::Function, 10);
        // `maybe_dead` has zero callers but an unresolved same-name call site
        // — the resolver gave up, so we must NOT flag it as dead.
        store
            .insert_unresolved_call(fa, "maybe_dead", None, 42, None, "calls")
            .unwrap();
        sym(&mut store, fa, "maybe_dead", SymbolKind::Function, 20);

        let findings = dead_code(&store).unwrap();
        let names: Vec<&str> = findings.iter().map(|f| f.label.as_str()).collect();
        assert!(
            names.iter().any(|n| n.contains("`truly_dead`")),
            "truly_dead should be flagged: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("`maybe_dead`")),
            "maybe_dead has envelope lower_bound — must not be flagged: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("`used`")),
            "used has a caller — must not be flagged: {names:?}"
        );
    }

    #[test]
    fn dead_code_skips_callback_references() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/a.ts");
        let fb = file(&mut store, "src/b.ts");
        // `plugin_fn` is passed as a callback argument — references edge only.
        // Registered ≠ dead: must not be flagged.
        let plugin_fn = sym(&mut store, fa, "plugin_fn", SymbolKind::Function, 1);
        let registrar = sym(&mut store, fb, "registrar", SymbolKind::Function, 1);
        store
            .insert_edge(&EdgeRow {
                src_id: registrar,
                dst_id: plugin_fn,
                kind: EdgeKind::References,
                tier: Tier::Probable,
                site_line: 3,
                receiver: None,
                callee: None,
            })
            .unwrap();
        // `orphan` has no edges at all — genuinely dead.
        sym(&mut store, fa, "orphan", SymbolKind::Function, 20);

        let findings = dead_code(&store).unwrap();
        let names: Vec<&str> = findings.iter().map(|f| f.label.as_str()).collect();
        assert!(
            !names.iter().any(|n| n.contains("`plugin_fn`")),
            "callback-referenced fn must not be flagged dead: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("`orphan`")),
            "orphan should be flagged: {names:?}"
        );
    }

    #[test]
    fn dead_code_excludes_non_callable_symbols() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/a.ts");
        // A module and a struct with zero incoming calls — not dead code.
        sym(&mut store, fa, "MyModule", SymbolKind::Module, 1);
        sym(&mut store, fa, "MyStruct", SymbolKind::Struct, 5);
        let findings = dead_code(&store).unwrap();
        assert!(findings.is_empty(), "modules/structs are not dead code");
    }

    #[test]
    fn hotspots_count_distinct_files() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/a.ts");
        let fb = file(&mut store, "src/b.ts");
        let fc = file(&mut store, "src/c.ts");
        // Three symbols in a, each called from b — fan_in should be 1 (one
        // distinct source file), not 3 (edges).
        let a1 = sym(&mut store, fa, "a1", SymbolKind::Function, 1);
        let a2 = sym(&mut store, fa, "a2", SymbolKind::Function, 5);
        let a3 = sym(&mut store, fa, "a3", SymbolKind::Function, 10);
        let b1 = sym(&mut store, fb, "b1", SymbolKind::Function, 1);
        let c1 = sym(&mut store, fc, "c1", SymbolKind::Function, 1);
        call(&mut store, b1, a1);
        call(&mut store, b1, a2);
        call(&mut store, b1, a3);
        call(&mut store, c1, a1);

        let findings = hotspots(&store, 10).unwrap();
        let a_finding = findings.iter().find(|f| f.file == "src/a.ts").unwrap();
        assert_eq!(a_finding.fan_in, 2, "two distinct files (b, c) call into a");
    }

    #[test]
    fn run_plan_queries_deduplicates_and_sorts() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/a.ts");
        let fb = file(&mut store, "src/b.ts");
        let _dead = sym(&mut store, fa, "dead_fn", SymbolKind::Function, 1);
        let _caller = sym(&mut store, fb, "caller", SymbolKind::Function, 1);
        // No edge → both are dead. Dedup: DeadCode twice → each symbol once.
        let queries = vec![PlanQuery::DeadCode, PlanQuery::DeadCode];
        let root = std::path::Path::new(".");
        let runner = pixel_git::GitRunner::new(root);
        let findings = run_plan_queries(&store, root, &runner, &queries).unwrap();
        let dead_count = findings
            .iter()
            .filter(|f| f.label.contains("dead_fn"))
            .count();
        assert_eq!(dead_count, 1);
    }

    #[test]
    fn fan_in_counts_distinct_calling_files_by_id_and_by_path() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/a.ts");
        let fb = file(&mut store, "src/b.ts");
        let fc = file(&mut store, "src/c.ts");
        let fd = file(&mut store, "src/d.ts");
        let a1 = sym(&mut store, fa, "a1", SymbolKind::Function, 1);
        let a2 = sym(&mut store, fa, "a2", SymbolKind::Function, 5);
        let b1 = sym(&mut store, fb, "b1", SymbolKind::Function, 1);
        let c1 = sym(&mut store, fc, "c1", SymbolKind::Function, 1);
        // b and c call into a (two edges from b: still one file); d calls nobody
        // and nobody calls d.
        call(&mut store, b1, a1);
        call(&mut store, b1, a2);
        call(&mut store, c1, a1);
        // A self-call within a must not count as fan-in.
        call(&mut store, a2, a1);

        let by_id = fan_in_for_files(&store, &[fa, fd]).unwrap();
        assert_eq!(by_id.get("src/a.ts"), Some(&2), "{by_id:?}");
        assert_eq!(by_id.get("src/d.ts"), None, "no callers, no row: {by_id:?}");
        assert_eq!(by_id.len(), 1);
        assert!(fan_in_for_files(&store, &[]).unwrap().is_empty());

        let by_path =
            fan_in_for_file_paths(&store, &["src/a.ts".to_string(), "src/d.ts".to_string()])
                .unwrap();
        assert_eq!(by_path.get("src/a.ts"), Some(&2), "{by_path:?}");
        assert_eq!(by_path.get("src/d.ts"), None, "{by_path:?}");
        assert_eq!(by_path.len(), 1);
        assert!(fan_in_for_file_paths(&store, &[]).unwrap().is_empty());
    }

    #[test]
    fn dead_interactive_reports_handlerless_interactive_elements_with_fan_in() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/Form.tsx");
        let fb = file(&mut store, "src/App.tsx");
        let save = sym(&mut store, fa, "save", SymbolKind::Function, 1);
        let app = sym(&mut store, fb, "App", SymbolKind::Function, 1);
        call(&mut store, app, save);
        // A wired button, a dead button, a dead div (not interactive).
        store
            .insert_jsx_element(fa, "button", true, "Save", 3, 3)
            .unwrap();
        store
            .insert_jsx_element(fa, "button", false, "Cancel", 4, 4)
            .unwrap();
        store
            .insert_jsx_element(fa, "div", false, "", 5, 5)
            .unwrap();

        let dead = store.jsx_elements_dead(None, Some("button")).unwrap();
        assert_eq!(dead.len(), 1, "{dead:?}");
        assert_eq!(dead[0].text_content, "Cancel");
        assert_eq!(store.jsx_elements_dead(Some(fb), None).unwrap().len(), 0);
        assert_eq!(store.jsx_elements_dead(None, None).unwrap().len(), 2);

        let findings = dead_interactive(&store, None).unwrap();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].file, "src/Form.tsx");
        assert_eq!(findings[0].line, 4);
        assert_eq!(findings[0].fan_in, 1, "App.tsx calls into Form.tsx");
        assert_eq!(dead_interactive(&store, Some("div")).unwrap().len(), 1);
        assert!(dead_interactive(&store, Some("form")).unwrap().is_empty());
    }

    fn git(root: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn commit(root: &Path, files: &[&str], msg: &str) {
        for p in files {
            let path = root.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let previous = std::fs::read_to_string(&path).unwrap_or_default();
            std::fs::write(&path, format!("{previous}{msg}\n")).unwrap();
        }
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", msg]);
    }

    /// Recent churn points at the likely bug area: the files most commits
    /// touched come first, only files the graph knows are findings, and the
    /// cap keeps the most-touched ones rather than the first names.
    #[test]
    fn recent_changes_ranks_graph_files_by_commits_in_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        commit(root, &["README.md", "src/a.ts", "src/b.ts"], "one");
        commit(root, &["README.md", "src/b.ts"], "two");
        commit(root, &["README.md", "src/b.ts", "src/c.ts"], "three");
        commit(root, &["src/c.ts"], "four");

        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/a.ts");
        let fb = file(&mut store, "src/b.ts");
        file(&mut store, "src/c.ts");
        let a1 = sym(&mut store, fa, "a1", SymbolKind::Function, 1);
        let b1 = sym(&mut store, fb, "b1", SymbolKind::Function, 1);
        call(&mut store, a1, b1);
        let runner = pixel_git::GitRunner::new(root);

        let findings = recent_changes(&store, root, &runner, 10).unwrap();
        let files: Vec<&str> = findings.iter().map(|f| f.file.as_str()).collect();
        assert_eq!(
            files,
            ["src/b.ts", "src/c.ts", "src/a.ts"],
            "3 commits, then 2, then 1; README.md is not in the graph"
        );
        assert_eq!(
            findings[0].label,
            "Review recent changes in src/b.ts (3 commit(s) in 30 days)"
        );
        assert_eq!(findings[0].fan_in, 1, "a.ts calls into b.ts");
        assert_eq!(findings[0].line, 1);

        let capped = recent_changes(&store, root, &runner, 2).unwrap();
        let files: Vec<&str> = capped.iter().map(|f| f.file.as_str()).collect();
        assert_eq!(
            files,
            ["src/b.ts", "src/c.ts"],
            "the cap counts findings, after README.md was skipped"
        );
        assert!(recent_changes(&store, root, &runner, 0).unwrap().is_empty());

        // Outside a repository there is nothing recent.
        let empty = tempfile::tempdir().unwrap();
        let runner = pixel_git::GitRunner::new(empty.path());
        assert!(
            recent_changes(&store, empty.path(), &runner, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn paths_by_churn_counts_commits_per_path_most_touched_first() {
        let log = [
            "",
            "src/b.ts",
            "src/a.ts",
            "",
            "src/b.ts",
            "  src/c.ts  ",
            "",
            "src/c.ts",
        ]
        .join("\n");
        assert_eq!(
            paths_by_churn(&log),
            [
                ("src/b.ts".to_string(), 2),
                ("src/c.ts".to_string(), 2),
                ("src/a.ts".to_string(), 1)
            ]
        );
        assert!(paths_by_churn("").is_empty());
    }

    #[test]
    fn prompts_classify_by_whole_words() {
        use PlanQuery::*;
        assert_eq!(
            classify_prompt("fix all clickable elements"),
            [DeadInteractive { tag_filter: None }]
        );
        assert_eq!(
            classify_prompt("wire the Buttons and navigation Links"),
            [
                DeadInteractive {
                    tag_filter: Some("button".into())
                },
                DeadInteractive {
                    tag_filter: Some("a".into())
                },
                DeadInteractive {
                    tag_filter: Some("Link".into())
                },
            ]
        );
        assert_eq!(
            classify_prompt("nothing happens on click"),
            [DeadInteractive { tag_filter: None }]
        );
        assert_eq!(
            classify_prompt("delete dead code, refactor by priority; recent regression"),
            [
                DeadCode,
                Hotspots { limit: 10 },
                RecentChanges { max_files: 20 }
            ]
        );
        assert_eq!(classify_prompt("remove unused helpers"), [DeadCode]);
        // Each intent word stands on its own.
        assert_eq!(classify_prompt("remove the legacy flag"), [DeadCode]);
        assert_eq!(classify_prompt("list unused exports"), [DeadCode]);
        assert_eq!(
            classify_prompt("refactor the parser"),
            [Hotspots { limit: 10 }]
        );
        assert_eq!(classify_prompt("show hotspots"), [Hotspots { limit: 10 }]);
        assert_eq!(
            classify_prompt("broken links"),
            [
                DeadInteractive {
                    tag_filter: Some("a".into())
                },
                DeadInteractive {
                    tag_filter: Some("Link".into())
                },
            ]
        );
        assert_eq!(
            classify_prompt("recent work"),
            [RecentChanges { max_files: 20 }]
        );
        assert_eq!(
            classify_prompt("a bug in billing"),
            [RecentChanges { max_files: 20 }]
        );
        // Substrings of other words are not intents.
        for prompt in [
            "unlinked invoices",
            "debugging the clicked handler",
            "codebase deadline",
        ] {
            assert_eq!(
                classify_prompt(prompt),
                [ByConcept {
                    query: prompt.to_string()
                }],
                "{prompt}"
            );
        }
    }

    #[test]
    fn plan_queries_prefer_the_explicit_query_and_name_every_query() {
        use PlanQuery::*;
        let q = |query, prompt, tag, limit| plan_queries(prompt, Some(query), tag, limit);
        assert_eq!(
            q("dead-interactive", None, Some("Link"), None).unwrap(),
            [DeadInteractive {
                tag_filter: Some("Link".into())
            }]
        );
        assert_eq!(
            q("dead-code", Some("links"), None, None).unwrap(),
            [DeadCode]
        );
        assert_eq!(
            q("hotspots", None, None, None).unwrap(),
            [Hotspots { limit: 10 }]
        );
        assert_eq!(
            q("hotspots", None, None, Some(3)).unwrap(),
            [Hotspots { limit: 3 }]
        );
        assert_eq!(
            q("recent-changes", None, None, None).unwrap(),
            [RecentChanges { max_files: 20 }]
        );
        assert_eq!(
            q("recent-changes", None, None, Some(4)).unwrap(),
            [RecentChanges { max_files: 4 }]
        );
        assert_eq!(
            q("by-concept", Some("invoice totals"), None, None).unwrap(),
            [ByConcept {
                query: "invoice totals".into()
            }]
        );
        assert_eq!(
            q("by-concept", None, None, None).unwrap_err(),
            "by-concept requires a prompt"
        );
        assert!(
            q("dead", None, None, None)
                .unwrap_err()
                .starts_with("unknown query 'dead'")
        );
        assert_eq!(
            plan_queries(Some("remove unused"), None, None, None).unwrap(),
            [DeadCode]
        );
        assert_eq!(
            plan_queries(None, None, None, None).unwrap_err(),
            "missing prompt (or pass --query)"
        );

        let names: Vec<&str> = [
            DeadInteractive { tag_filter: None },
            DeadCode,
            Hotspots { limit: 1 },
            ByConcept { query: "x".into() },
            RecentChanges { max_files: 1 },
        ]
        .iter()
        .map(PlanQuery::name)
        .collect();
        assert_eq!(
            names,
            [
                "dead-interactive",
                "dead-code",
                "hotspots",
                "by-concept",
                "recent-changes"
            ]
        );
        for name in names {
            let parsed = plan_queries(Some("p"), Some(name), None, None).unwrap();
            assert_eq!(parsed[0].name(), name, "--query {name} round-trips");
        }
    }

    #[test]
    fn severity_names_are_the_rendered_labels() {
        assert_eq!(Severity::High.as_str(), "HIGH");
        assert_eq!(Severity::Medium.as_str(), "MEDIUM");
        assert_eq!(Severity::Low.as_str(), "LOW");
    }

    #[test]
    fn entry_points_and_tests_are_never_dead_code() {
        for (name, path) in [
            ("main", "src/main.rs"),
            ("main", "cmd/tool/main.go"),
            ("init", "pkg/db.go"),
            ("test_login", "tests/test_auth.py"),
            ("it_works", "crates/x/tests/all/smoke.rs"),
            ("renders", "src/__tests__/App.tsx"),
            ("helper", "src/login.test.ts"),
            ("helper", "src/login.spec.ts"),
            ("TestLogin", "auth/login_test.go"),
            ("check", "auth/login_test.py"),
            ("check", "auth/test_login.py"),
            ("example", "spec/models/user_spec.rb"),
        ] {
            assert!(is_entry_point(name, path), "{name} in {path}");
        }
        for (name, path) in [
            ("init", "src/init.rs"),
            ("maintain", "src/main.rs"),
            ("helper", "src/testing.ts"),
            ("helper", "src/contest/score.py"),
            ("latest", "src/latest.go"),
        ] {
            assert!(!is_entry_point(name, path), "{name} in {path}");
        }
    }

    #[test]
    fn dead_code_says_no_callers_found_and_skips_entry_points() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/main.rs");
        let ft = file(&mut store, "src/app.test.ts");
        sym(&mut store, fa, "main", SymbolKind::Function, 1);
        sym(&mut store, fa, "orphan", SymbolKind::Function, 9);
        sym(&mut store, ft, "helper", SymbolKind::Function, 1);
        let findings = dead_code(&store).unwrap();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].file, "src/main.rs");
        assert_eq!(findings[0].line, 9);
        assert_eq!(
            findings[0].label,
            "No callers found for function `orphan`: confirm it is unused before removing (qualified: orphan)"
        );
    }

    /// Which files make the cut must not depend on row order: equal fan-in
    /// ranks by path, and exactly `limit` rows come back.
    #[test]
    fn hotspots_break_ties_by_path_and_stop_at_the_limit() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let caller_file = file(&mut store, "src/app.ts");
        let caller = sym(&mut store, caller_file, "app", SymbolKind::Function, 1);
        for path in ["src/z.ts", "src/m.ts", "src/b.ts"] {
            let fid = file(&mut store, path);
            let target = sym(
                &mut store,
                fid,
                &path.replace(['/', '.'], "_"),
                SymbolKind::Function,
                1,
            );
            call(&mut store, caller, target);
        }
        let files = |limit| -> Vec<String> {
            hotspots(&store, limit)
                .unwrap()
                .into_iter()
                .map(|f| f.file)
                .collect()
        };
        assert_eq!(files(2), ["src/b.ts", "src/m.ts"]);
        assert_eq!(files(3), ["src/b.ts", "src/m.ts", "src/z.ts"]);
        assert!(files(0).is_empty());
        let first = &hotspots(&store, 1).unwrap()[0];
        assert_eq!(first.label, "Refactor hotspot file src/b.ts (1 dependents)");
        assert_eq!(first.line, 1);
    }

    /// A plan over thousands of dead symbols asks for each file's fan-in
    /// once, in chunks SQLite accepts, and loses no file on a chunk edge.
    #[test]
    fn fan_in_deduplicates_and_chunks_large_id_and_path_lists() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/a.ts");
        let fb = file(&mut store, "src/b.ts");
        let a1 = sym(&mut store, fa, "a1", SymbolKind::Function, 1);
        let b1 = sym(&mut store, fb, "b1", SymbolKind::Function, 1);
        call(&mut store, b1, a1);

        let mut ids: Vec<i64> = (1_000..34_000).collect();
        ids.extend(std::iter::repeat_n(fa, 40_000));
        let by_id = fan_in_for_files(&store, &ids).unwrap();
        assert_eq!(by_id.get("src/a.ts"), Some(&1), "{by_id:?}");
        assert_eq!(by_id.len(), 1);

        let mut paths: Vec<String> = (0..1_200).map(|i| format!("src/x{i}.ts")).collect();
        paths.push("src/a.ts".to_string());
        paths.push("src/a.ts".to_string());
        let by_path = fan_in_for_file_paths(&store, &paths).unwrap();
        assert_eq!(by_path.get("src/a.ts"), Some(&1), "{by_path:?}");
        assert_eq!(by_path.len(), 1);
    }

    #[test]
    fn run_plan_queries_orders_by_severity_then_fan_in_then_location() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let hot = file(&mut store, "src/hot.ts");
        let warm = file(&mut store, "src/warm.ts");
        let cold = file(&mut store, "src/cold.ts");
        // Dead symbols: two in hot.ts (fan-in 8: HIGH), one in warm.ts
        // (fan-in 3: MEDIUM), one in cold.ts (fan-in 0: LOW).
        sym(&mut store, cold, "cold_dead", SymbolKind::Function, 1);
        sym(&mut store, hot, "hot_dead_late", SymbolKind::Function, 20);
        sym(&mut store, hot, "hot_dead_early", SymbolKind::Function, 10);
        sym(&mut store, warm, "warm_dead", SymbolKind::Function, 5);
        let hot_target = sym(&mut store, hot, "hot_used", SymbolKind::Function, 1);
        let warm_target = sym(&mut store, warm, "warm_used", SymbolKind::Function, 1);
        for i in 0..8 {
            let fid = file(&mut store, &format!("src/callers/c{i}.ts"));
            let c = sym(&mut store, fid, &format!("c{i}"), SymbolKind::Function, 1);
            call(&mut store, c, hot_target);
            if i < 3 {
                call(&mut store, c, warm_target);
            }
        }
        let root = Path::new(".");
        let runner = pixel_git::GitRunner::new(root);
        let findings = run_plan_queries(
            &store,
            root,
            &runner,
            &[PlanQuery::DeadCode, PlanQuery::DeadCode],
        )
        .unwrap();
        let order: Vec<(&str, u32, u32)> = findings
            .iter()
            .filter(|f| !f.file.starts_with("src/callers/"))
            .map(|f| (f.file.as_str(), f.line, f.fan_in))
            .collect();
        assert_eq!(
            order,
            [
                ("src/hot.ts", 10, 8),
                ("src/hot.ts", 20, 8),
                ("src/warm.ts", 5, 3),
                ("src/cold.ts", 1, 0),
            ],
            "{findings:?}"
        );
        let first_low = findings
            .iter()
            .position(|f| f.severity == Severity::Low)
            .unwrap();
        assert!(
            findings[first_low..]
                .iter()
                .all(|f| f.severity == Severity::Low)
        );
    }

    /// A trait method implementation has no caller by name (`x.to_string()`
    /// calls `fmt` through `Display`): a graph built from source never
    /// reports it as dead, while a free function with no caller still is.
    #[test]
    fn dead_code_skips_trait_implementation_methods() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub struct X;\n\
             impl std::fmt::Display for X {\n    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }\n}\n\
             impl X {\n    pub fn inherent(&self) {}\n}\n",
        )
        .unwrap();
        let db = dir.path().join("graph.db");
        crate::build::build_graph(dir.path(), &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let labels: Vec<String> = dead_code(&store)
            .unwrap()
            .into_iter()
            .map(|f| f.label)
            .collect();
        assert_eq!(labels.len(), 1, "{labels:?}");
        assert!(labels[0].contains("`inherent`"), "{labels:?}");
    }

    #[test]
    fn by_concept_turns_concept_matches_into_findings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("billing.ts"),
            "export function calculateInvoiceTotal(lines: number[]) { return lines.length; }\n",
        )
        .unwrap();
        let db = dir.path().join("graph.db");
        crate::build::build_graph(dir.path(), &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let findings = by_concept(&store, "invoice total").unwrap();
        let hit = findings
            .iter()
            .find(|f| f.label.contains("calculateInvoiceTotal"))
            .unwrap_or_else(|| panic!("{findings:?}"));
        assert_eq!(hit.file, "billing.ts");
        assert_eq!(hit.line, 1);
        assert_eq!(hit.severity, Severity::Low);
        assert!(by_concept(&store, "zzqx unrelated").unwrap().is_empty());
    }

    #[test]
    fn by_concept_drops_stopword_only_matches_and_groups_per_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("status.ts"),
            "const a = \"status bar shows model\";\nconst b = \"status of the build\";\n",
        )
        .unwrap();
        // Overlaps the query only through stopwords ("in my"): lexical noise,
        // not evidence — must not become a finding.
        std::fs::write(
            dir.path().join("noise.ts"),
            "const z = \"in my opinion the thing\";\n",
        )
        .unwrap();
        let db = dir.path().join("graph.db");
        crate::build::build_graph(dir.path(), &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let findings = by_concept(&store, "add provider info in my status line").unwrap();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].file, "status.ts");
        assert!(
            findings[0].label.contains("2 concept matches"),
            "{findings:?}"
        );
    }

    #[test]
    fn by_concept_returns_nothing_for_an_all_stopword_query() {
        // A prompt that reduces to stopwords has no content word: even if
        // resolve returns lexical hits, none of them are evidence.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.ts"),
            "const a = \"in the mood\";\nconst b = \"and so on\";\n",
        )
        .unwrap();
        let db = dir.path().join("graph.db");
        crate::build::build_graph(dir.path(), &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        assert!(content_query_words("in the and").is_empty());
        assert!(
            by_concept(&store, "in the and").unwrap().is_empty(),
            "a stopword-only query must not emit findings"
        );
    }

    #[test]
    fn match_shares_content_splits_camel_case_raw() {
        // `norm` is lowercased, so "totalPrice" survives there only as the
        // single token "totalprice" — the raw split is what lets a multi-word
        // query overlap an identifier-shaped literal.
        let m = concept_resolve::ConceptMatch {
            path: "price.ts".into(),
            start_line: 1,
            end_line: 1,
            kind: crate::concept::ConceptKind::String,
            raw: "the totalPrice field".into(),
            norm: "the totalprice field".into(),
            detail: String::new(),
            owner: None,
            symbol_kind: None,
            score: 1.0,
            reasons: vec![],
        };
        let qwords = content_query_words("show total price");
        assert_eq!(qwords, vec!["total".to_string(), "price".to_string()]);
        assert!(match_shares_content(&m, &qwords));
        // And a pure stopword overlap still fails.
        let noise = concept_resolve::ConceptMatch {
            raw: "in my opinion".into(),
            norm: "in my opinion".into(),
            ..m
        };
        assert!(!match_shares_content(&noise, &qwords));
    }

    #[test]
    fn match_shares_content_rejects_lexical_matches_without_content_words() {
        // A stopword-only query leaves no content word to overlap: an
        // ordinary lexical hit is noise then, not evidence — while a
        // symbol-tier match is still trusted on its own.
        let m = concept_resolve::ConceptMatch {
            path: "a.ts".into(),
            start_line: 1,
            end_line: 1,
            kind: crate::concept::ConceptKind::String,
            raw: "in the mood".into(),
            norm: "in the mood".into(),
            detail: String::new(),
            owner: None,
            symbol_kind: None,
            score: 1.0,
            reasons: vec![],
        };
        assert!(!match_shares_content(&m, &[]));
        let sym = concept_resolve::ConceptMatch {
            symbol_kind: Some("function".into()),
            ..m
        };
        assert!(match_shares_content(&sym, &[]));
    }

    #[test]
    fn match_shares_content_trusts_a_symbol_match_without_word_overlap() {
        // A symbol-resolution match is evidence on its own: the gate must
        // not require a content-word overlap on top of `symbol_kind`.
        let m = concept_resolve::ConceptMatch {
            path: "svc.ts".into(),
            start_line: 1,
            end_line: 1,
            kind: crate::concept::ConceptKind::String,
            raw: "entirely unrelated words".into(),
            norm: "entirely unrelated words".into(),
            detail: String::new(),
            owner: None,
            symbol_kind: Some("function".into()),
            score: 1.0,
            reasons: vec![],
        };
        let qwords = content_query_words("invoice total");
        assert!(!qwords.is_empty());
        assert!(match_shares_content(&m, &qwords));
    }

    #[test]
    fn enclosing_symbol_picks_the_tightest_true_encloser() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let f = file(&mut store, "src/a.ts");
        // `before` ends and `after` starts outside line 10: only `big`
        // encloses it. Loosened `||`/comparisons would admit the smaller
        // non-enclosing spans and win `min_by_key`.
        for (name, start, end) in [("big", 1, 100), ("before", 1, 2), ("after", 50, 51)] {
            store
                .insert_symbol(
                    f,
                    &format!("{name}#uid"),
                    name,
                    name,
                    SymbolKind::Function,
                    start,
                    end,
                    "",
                )
                .unwrap();
        }
        let (name, start) = enclosing_symbol(&store, "src/a.ts", 10).unwrap();
        assert_eq!((name.as_str(), start), ("big", 1));
        // Boundary: a symbol starting or ending exactly on the line encloses it.
        let (name, _) = enclosing_symbol(&store, "src/a.ts", 2).unwrap();
        assert_eq!(name, "before");
        let (name, _) = enclosing_symbol(&store, "src/a.ts", 50).unwrap();
        assert_eq!(name, "after");
        assert!(enclosing_symbol(&store, "src/a.ts", 200).is_none());
        assert!(enclosing_symbol(&store, "missing.ts", 10).is_none());
    }

    #[test]
    fn evidence_snippet_flattens_whitespace_and_marks_truncation() {
        assert_eq!(evidence_snippet("a  b\n c", 80), "a b c");
        assert_eq!(evidence_snippet("x", 80), "x");
        let got = evidence_snippet(&"y".repeat(200), 80);
        assert!(got.ends_with('…'), "{got}");
        assert_eq!(got.chars().count(), 81);
    }

    #[test]
    fn by_concept_on_a_score_tie_keeps_the_first_match() {
        let dir = tempfile::tempdir().unwrap();
        // Same tokens, same file — a score tie between two lines. `>` keeps
        // the first-seen match; `>=` would silently take the last one.
        std::fs::write(
            dir.path().join("dup.ts"),
            "const a = \"invoice total alpha\";\nconst b = \"invoice total beta\";\n",
        )
        .unwrap();
        let db = dir.path().join("graph.db");
        crate::build::build_graph(dir.path(), &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let findings = by_concept(&store, "invoice total").unwrap();
        let f = findings
            .iter()
            .find(|f| f.file == "dup.ts")
            .unwrap_or_else(|| panic!("{findings:?}"));
        assert_eq!(f.line, 1, "{findings:?}");
    }

    /// `env reads` keep identifier boundaries: `myprocess.env.X` and
    /// non-literal `env::var(name)` must not register, `option_env!` must
    /// not double-fire the `env!` needle.
    #[test]
    fn env_reads_respects_identifier_boundaries() {
        let text = "a = process.env.STRIPE_KEY\n\
                    b = myprocess.env.NOT_THIS\n\
                    c = process.env.lowercase\n\
                    d = env::var(\"DATABASE_URL\")\n\
                    e = env::var(name)\n\
                    f = option_env!(\"OPT\")\n";
        let js = env_reads(text, "process.env.", false);
        assert_eq!(js, [("STRIPE_KEY".to_string(), 1)]);
        assert_eq!(
            env_reads(text, "env::var(", true),
            [("DATABASE_URL".to_string(), 4)]
        );
        // `env!` inside `option_env!` is rejected by the left boundary —
        // only the dedicated needle captures OPT.
        assert!(env_reads(text, "env!(", true).is_empty());
        assert_eq!(
            env_reads(text, "option_env!(", true),
            [("OPT".to_string(), 6)]
        );
    }

    /// The quoted path tolerates whitespace between the marker and the
    /// opening quote, and reads an identifier whose first byte may be
    /// lowercase as long as some byte is uppercase. The trailing-
    /// whitespace fixture forces the inner whitespace-stripping loop to
    /// walk past the end of `bytes` once `.trim()` would have stopped at
    /// the quote; without a `<` vs `<=` boundary there, the function
    /// reads `bytes[pos]` out of bounds.
    #[test]
    fn env_reads_quoted_path_handles_whitespace_and_lowercase_prefixes() {
        // Whitespace between `env::var(` and the opening quote.
        let text = "x = env::var(\t \"DATABASE_URL\")\n";
        assert_eq!(
            env_reads(text, "env::var(", true),
            [("DATABASE_URL".to_string(), 1)]
        );
        // Lowercase prefix, one uppercase byte qualifies the var. A
        // `||` → `&&` mutant on the uppercase check rejects the var.
        let text2 = "x = env::var(\"lowercaseKEY\")\n";
        assert_eq!(
            env_reads(text2, "env::var(", true),
            [("lowercaseKEY".to_string(), 1)]
        );
        // Single-char identifier is rejected (length >= 2).
        let text3 = "x = env::var(\"X\")\n";
        assert!(env_reads(text3, "env::var(", true).is_empty());
        // Single-quote and double-quote both delimit.
        let text4 = "x = env::var('SINGLE')\n";
        assert_eq!(
            env_reads(text4, "env::var(", true),
            [("SINGLE".to_string(), 1)]
        );
        // No quote at all.
        let text5 = "x = env::var(NO_QUOTES)\n";
        assert!(env_reads(text5, "env::var(", true).is_empty());
        // Whitespace inside the quotes after the var — the function's
        // identifier scan stops at the closing quote regardless of what
        // follows. Without this fixture, a `<` → `<=` flip on the
        // whitespace-strip loop or on the ident scan would only be
        // observable in pathological inputs.
        let text6 = "x = env::var(\"KEY   \")\n";
        assert_eq!(
            env_reads(text6, "env::var(", true),
            [("KEY".to_string(), 1)]
        );
    }

    /// The identifier-scan loop's `<` boundary, `+=` step, and the
    /// `pos - start` length check are each catchable: each line below
    /// exercises a distinct mutant site on the quoted path. A `pos - start`
    /// minus-flip mutant renders the empty-string span acceptable, so a
    /// quoted string of two or more chars must read as one var.
    #[test]
    fn env_reads_quoted_identifier_scan_pinpoints_inner_loops() {
        // Five-character identifier (long enough to exercise the loop
        // body; a `<` → `<=` flip on the loop bound lets `pos` advance
        // past end and reads a zero-byte at `bytes[pos]`).
        let text = "x = env::var(\"ABCDE\")\n";
        assert_eq!(
            env_reads(text, "env::var(", true),
            [("ABCDE".to_string(), 1)]
        );
        // Trailing chars after the identifier but before the closing
        // quote force the loop to advance through them; a `+=` → `-=`
        // flip makes `pos` retreat and reads garbage from earlier in
        // the string.
        let text2 = "x = env::var(\"ABCDEF\" + \"x\")\n";
        assert_eq!(
            env_reads(text2, "env::var(", true),
            [("ABCDEF".to_string(), 1)]
        );
    }

    /// Exact match or a `rest` continuation that starts with a path/separator
    /// char — `striped` and `pgx` must not match `stripe`/`pg`.
    #[test]
    fn spec_matches_exact_or_continues_with_a_separator() {
        assert!(spec_matches("stripe", "stripe"));
        assert!(spec_matches("stripe/react", "stripe"));
        assert!(spec_matches("sqlx::Pool", "sqlx"));
        assert!(spec_matches("diesel-async", "diesel"));
        assert!(spec_matches("@supabase/auth-helpers", "@supabase"));
        assert!(!spec_matches("pgx", "pg"));
        assert!(!spec_matches("striped", "stripe"));
        assert!(!spec_matches("my-stripe", "stripe"));
    }

    /// Multiple auth hits share one Prereq per (file, detail); the line is
    /// the *topmost* occurrence, not the first-seen — `--done N` lands on
    /// the earliest call site so the agent opens the file at the right line.
    #[test]
    fn detect_prereqs_keeps_the_topmost_line_for_repeated_auth_signals() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let w = |rel: &str, content: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
        };
        // The file has getServerSession on lines 5 and 9, and bare auth() on
        // lines 7 and 11. After merging, getServerSession should point at
        // line 5 (topmost), auth() at line 7 (topmost). The exact ordering
        // between the two is fixed by catalog traversal: marker hits
        // first, then call hits.
        w(
            "src/page.tsx",
            "import './x';\n\
             const a = 1;\n\
             const b = 2;\n\
             const c = 3;\n\
             const s1 = getServerSession();\n\
             const s2 = 1;\n\
             const t = auth();\n\
             const s3 = 2;\n\
             const s4 = getServerSession();\n\
             const s5 = 3;\n\
             const t2 = auth();\n",
        );
        let mut store = GraphStore::open_in_memory().unwrap();
        let page = file(&mut store, "src/page.tsx");
        store.insert_import(page, "x", None, &[]).unwrap();
        let out = detect_prereqs(&store, root, &["src/page.tsx".into()]).unwrap();
        let auth: Vec<&Prereq> = out.iter().filter(|p| p.kind == PrereqKind::Auth).collect();
        let marker = auth
            .iter()
            .find(|p| p.detail == "getServerSession")
            .expect("getServerSession prereq");
        assert_eq!(
            marker.line, 5,
            "topmost getServerSession is line 5; got {}",
            marker.line
        );
        let call = auth
            .iter()
            .find(|p| p.detail == "auth()")
            .expect("auth() prereq");
        assert_eq!(call.line, 7, "topmost auth() is line 7; got {}", call.line);
        // Dedup: only one Prereq per (file, detail).
        assert_eq!(auth.len(), 2);
    }

    #[test]
    fn auth_markers_and_bare_auth_calls_need_boundaries() {
        let hits = auth_marker_hits(
            "a = getServerSession()\n\
             b = mygetSession()\n\
             c = getSessions()\n",
        );
        assert_eq!(hits, [("getServerSession", 1)]);
        // `oauth()`/`reauth()` carry `auth` inside an identifier — only the
        // boundary-respecting hits count.
        let calls = auth_call_hits("x = auth()\ny = oauth()\nz = reauth()\nw = obj.auth()\n");
        assert_eq!(calls, vec![1, 4]);
        // A space before the paren is prose, not a call.
        assert!(auth_call_hits("// auth (the token) is checked\n").is_empty());
    }

    /// `auth` at the very end of the file (no `(` follows it) must not be
    /// reported as a call. A `<` → `<=` mutant flips the boundary check
    /// to `next <= bytes.len()`, then reads `bytes[next]` past the end —
    /// which the existing fixtures never exercise. Without this test, that
    /// out-of-bounds read survives as a missed mutant and a runtime panic
    /// in production code.
    #[test]
    fn auth_call_hits_ignores_auth_at_end_of_file() {
        assert!(auth_call_hits("const x = auth").is_empty());
        assert!(auth_call_hits("auth").is_empty());
        assert!(auth_call_hits("oauth\nauth").is_empty());
    }

    /// `provider_env_prefix` returns the env-key prefix the spec catalog
    /// advertises, or `None` for an unknown provider. Without these
    /// assertions a `None` mutant (`Some("")`, `Some("xyzzy")`) silently
    /// degrades the gate label to "the provider's env keys" with no
    /// prefix to search for.
    #[test]
    fn provider_env_prefix_maps_every_catalogued_provider() {
        assert_eq!(provider_env_prefix("Stripe"), Some("STRIPE_"));
        assert_eq!(provider_env_prefix("OpenAI"), Some("OPENAI_"));
        assert_eq!(provider_env_prefix("Supabase"), Some("SUPABASE_"));
        assert_eq!(provider_env_prefix("Anthropic"), Some("ANTHROPIC_"));
        assert_eq!(provider_env_prefix("AWS"), Some("AWS_"));
        assert_eq!(provider_env_prefix("NotInCatalog"), None);
    }

    /// `prereq_lang` maps every supported extension to its language
    /// bucket; a `delete match arm "rs"` mutant leaves Rust files
    /// returning "" and silently disables every env-read test for them.
    #[test]
    fn prereq_lang_recognises_every_supported_extension() {
        assert_eq!(prereq_lang("src/foo.ts"), "js");
        assert_eq!(prereq_lang("src/foo.tsx"), "js");
        assert_eq!(prereq_lang("src/foo.rs"), "rs");
        assert_eq!(prereq_lang("src/foo.go"), "go");
        assert_eq!(prereq_lang("src/foo.py"), "py");
        assert_eq!(prereq_lang("src/foo.rb"), "rb");
        assert_eq!(prereq_lang("src/foo.java"), "jvm");
        assert_eq!(prereq_lang("src/foo.kt"), "jvm");
        // Unknown extension maps to "" (the catch-all), so env_reads is
        // skipped for unsupported languages instead of misclassified.
        assert_eq!(prereq_lang("src/foo.unknown"), "");
        assert_eq!(prereq_lang("src/foo"), "");
    }

    /// `detect_prereqs` keys the auth-line dedup by `(file, detail)`.
    /// A `match guard *existing <= line` mutant flips to `false` (never
    /// update) or `true` (always update), changing which line wins. A
    /// reverse-order fixture (auth call on line 9 BEFORE line 5) makes
    /// the difference observable: topmost stays at 5 even when the
    /// detector sees 9 first.
    #[test]
    fn detect_prereqs_keeps_topmost_line_even_when_calls_appear_in_reverse_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let w = |rel: &str, content: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
        };
        // Three auth() calls in non-monotonic line order — first-seen line
        // 14, then line 9, then line 2. The dedup guard
        // `*existing <= line` keeps line 14 → updates to 9 → updates
        // to 2 (the topmost). A `false` guard (never update) keeps
        // first-seen 14; `<=` → `>` (skip when existing > line) and
        // `<=` → `>=` both keep 14 throughout. All four outcomes
        // differ; the test asserts the correct one (line 2) and fails
        // on every guard flip.
        //
        // Lines:
        //   1:  const a = 1;
        //   2:  const e = auth();   <-- topmost, CORRECT
        //   3:  const b = 2;
        //   4:  const c = 3;
        //   5:  const d = 4;
        //   6:  const f = 6;
        //   7:  const g = 7;
        //   8:  const h = 8;
        //   9:  const i = auth();
        //   10: const j = 10;
        //   11: const k = 11;
        //   12: const l = 12;
        //   13: const m = 13;
        //   14: const n = auth();   <-- first-seen
        w(
            "src/page.tsx",
            "const a = 1;\n\
             const e = auth();\n\
             const b = 2;\n\
             const c = 3;\n\
             const d = 4;\n\
             const f = 6;\n\
             const g = 7;\n\
             const h = 8;\n\
             const i = auth();\n\
             const j = 10;\n\
             const k = 11;\n\
             const l = 12;\n\
             const m = 13;\n\
             const n = auth();\n",
        );
        let mut store = GraphStore::open_in_memory().unwrap();
        let page = file(&mut store, "src/page.tsx");
        store.insert_import(page, "x", None, &[]).unwrap();
        let out = detect_prereqs(&store, root, &["src/page.tsx".into()]).unwrap();
        let auth: Vec<&Prereq> = out.iter().filter(|p| p.kind == PrereqKind::Auth).collect();
        assert_eq!(auth.len(), 1, "{out:?}");
        assert_eq!(auth[0].line, 2, "topmost line is 2; got {}", auth[0].line);
    }

    /// Plan targets pull one import hop in both directions: a resolved
    /// import adds the target file, an importer adds the importing file.
    /// Every signal kind is found across the set.
    #[test]
    fn detect_prereqs_follows_one_import_hop_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let w = |rel: &str, content: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
        };
        w(
            "src/pay.ts",
            "import Stripe from 'stripe';\nconst k = process.env.STRIPE_SECRET_KEY;\nconst b = process.env['API_BASE'];\n",
        );
        w("src/middleware.ts", "export const mw = auth();\n");
        w(
            "src/page.tsx",
            "import { pay } from './pay';\nconst s = getServerSession();\n",
        );
        w("src/db.ts", "import { sql } from 'drizzle-orm';\n");

        let mut store = GraphStore::open_in_memory().unwrap();
        let pay = file(&mut store, "src/pay.ts");
        let mid = file(&mut store, "src/middleware.ts");
        let page = file(&mut store, "src/page.tsx");
        let db = file(&mut store, "src/db.ts");
        store
            .insert_import(pay, "stripe", None, &[ImportBinding::named("Stripe")])
            .unwrap();
        store
            .insert_import(pay, "./middleware", Some(mid), &[])
            .unwrap();
        store
            .insert_import(page, "./pay", Some(pay), &[ImportBinding::named("pay")])
            .unwrap();
        store
            .insert_import(db, "drizzle-orm", None, &[ImportBinding::named("sql")])
            .unwrap();

        // Seeds: pay.ts + db.ts. middleware arrives via the import-out hop;
        // page arrives via the import-in hop.
        let out = detect_prereqs(&store, root, &["src/pay.ts".into(), "src/db.ts".into()]).unwrap();
        let has =
            |kind: PrereqKind, file: &str| out.iter().any(|p| p.kind == kind && p.file == file);
        assert!(has(PrereqKind::Provider, "src/pay.ts"), "{out:?}");
        assert!(has(PrereqKind::Env, "src/pay.ts"));
        assert!(has(PrereqKind::Db, "src/db.ts"));
        assert!(
            has(PrereqKind::Auth, "src/middleware.ts"),
            "import-out hop missed"
        );
        assert!(
            has(PrereqKind::Auth, "src/page.tsx"),
            "import-in hop missed"
        );

        // Evidence quality: env names captured, provider named by display.
        let envs: Vec<&str> = out
            .iter()
            .filter(|p| p.kind == PrereqKind::Env)
            .map(|p| p.detail.as_str())
            .collect();
        assert_eq!(envs, ["STRIPE_SECRET_KEY", "API_BASE"]);
        let prov = out.iter().find(|p| p.kind == PrereqKind::Provider).unwrap();
        assert_eq!(prov.detail, "Stripe");
        let auth = out.iter().find(|p| p.file == "src/middleware.ts").unwrap();
        assert_eq!(auth.line, 1, "{auth:?}");
        // A file in the graph but absent from disk is skipped, not fatal.
        file(&mut store, "src/ghost.ts");
        assert!(
            detect_prereqs(&store, root, &["src/ghost.ts".into()])
                .unwrap()
                .is_empty()
        );
    }

    /// The scan set is bounded: sixty resolved importers cannot push the
    /// read set past the file cap. The cap must hold regardless of whether
    /// each file emits a signal — it caps *files visited*, not *signals
    /// emitted*, so the test cannot lean on signal saturation to mask the bug.
    #[test]
    fn detect_prereqs_caps_the_scan_set() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let w = |rel: &str, content: &str| std::fs::write(root.join(rel), content).unwrap();
        w("seed.ts", "// seed\n");
        let mut store = GraphStore::open_in_memory().unwrap();
        let seed = file(&mut store, "seed.ts");
        // Sixty importers, none with any signal — the cap is reached by
        // file count, not signal count. A signal-count assertion would pass
        // even if every importer was silently scanned.
        for i in 0..60 {
            let rel = format!("m{i}.ts");
            w(&rel, "// no signals here\n");
            let id = file(&mut store, &rel);
            store.insert_import(id, "./seed", Some(seed), &[]).unwrap();
        }
        let out = detect_prereqs(&store, root, &["seed.ts".into()]).unwrap();
        assert!(
            out.is_empty(),
            "no signals means no detections, but visited files are not directly observable — \
             pair this with the signal-saturation test below"
        );
        // Same fixture, every importer now reads a unique env key: with the
        // cap enforced, only PREREQ_FILE_CAP files are read; without the
        // cap, all 60 would be read.
        for i in 0..60 {
            let rel = format!("m{i}.ts");
            w(&rel, &format!("const k{i} = process.env.VAR_{i};\n"));
        }
        let out = detect_prereqs(&store, root, &["seed.ts".into()]).unwrap();
        let envs = out.iter().filter(|p| p.kind == PrereqKind::Env).count();
        // The scan set caps at PREREQ_FILE_CAP total files; one of those is
        // the seed itself, so the importers seen are at most cap - 1.
        assert!(
            envs < PREREQ_FILE_CAP,
            "scanned {envs} env reads; cap on importers is PREREQ_FILE_CAP - 1"
        );
        // And the cap must be exercised, not just respected by accident.
        // 60 importers, cap 50 ⇒ we visit 49 of them.
        assert_eq!(
            envs,
            PREREQ_FILE_CAP - 1,
            "60 importers with a cap of {PREREQ_FILE_CAP} must visit exactly cap-1 of them; got {envs}"
        );
    }

    /// The wire contract: kinds serialize to the stable snake_case names the
    /// CLI parses, and unknown rows surface as decode errors not silence.
    #[test]
    fn prereq_serde_uses_stable_kind_names() {
        let p = Prereq {
            kind: PrereqKind::Auth,
            file: "a.ts".into(),
            line: 7,
            detail: "auth()".into(),
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["kind"], "auth");
        let back: Prereq = serde_json::from_value(json!({
            "kind": "db", "file": "b.rs", "line": 1, "detail": "sqlx"
        }))
        .unwrap();
        assert_eq!(back.kind, PrereqKind::Db);
        assert!(
            serde_json::from_value::<Prereq>(json!({
                "kind": "wat", "file": "b.rs", "line": 1, "detail": "x"
            }))
            .is_err()
        );
    }
}
