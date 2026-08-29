//! `pixel-rank` — the pure fusion core for `pixel targets` and (later) ranked
//! `search`/`resolve`.
//!
//! Task text in, closed prioritized file list out. The `op_targets` service
//! op in `pixel-daemon` gathers `SignalInputs` from the trigram index and the
//! code graph; this crate tokenizes the task, fuses the per-signal ranked
//! lists with reciprocal-rank fusion (mirroring `pixel-recall`'s hybrid
//! channel fusion), and assigns P0/P1/P2 tiers. Everything here is
//! deterministic: total orders everywhere, ties broken by path ascending,
//! scores rounded for byte-stable JSON.
//!
//! No I/O, no daemon, no index — pure functions over caller-supplied
//! `SignalInputs`. Per `PLAN.md`'s crate table this holds "the pure fusion
//! core (port of serve/targets.rs): signal registry + weighted RRF (K=60),
//! P0/P1/P2 tiering; slots for recency/churn/session signals."

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::Serialize;
use serde_json::{Value, json};

pub mod rerank;
pub mod signals;

use pixel_graph::split_ident_words;
use pixel_graph::targets::SymbolHit;

// ---------------------------------------------------------------------------
// tokenizer
// ---------------------------------------------------------------------------

/// Pure-function words that carry no retrieval signal in a task description.
/// Domain-ish words (error, parse, auth, …) deliberately stay searchable.
const STOPWORDS: &[&str] = &[
    // articles / prepositions / pronouns / auxiliaries
    "the",
    "and",
    "for",
    "with",
    "that",
    "this",
    "its",
    "it",
    "a",
    "an",
    "to",
    "in",
    "of",
    "on",
    "at",
    "by",
    "is",
    "are",
    "be",
    "was",
    "were",
    "or",
    "as",
    "so",
    "we",
    "our",
    "my",
    "your",
    "into",
    "from",
    "when",
    "then",
    "than",
    "them",
    "they",
    "there",
    "all",
    "any",
    "can",
    "will",
    "should",
    "must",
    "not",
    "but",
    "how",
    "what",
    "why",
    // task verbs / filler
    "fix",
    "add",
    "implement",
    "support",
    "make",
    "create",
    "update",
    "remove",
    "change",
    "refactor",
    "improve",
    "ensure",
    "allow",
    "use",
    "need",
    "want",
    "please",
    "new",
    "also",
    "file",
    "files",
    "code",
    "feature",
    "bug",
    "task",
    "issue",
];

const MAX_KEYWORDS: usize = 12;
const MIN_KEYWORD_LEN: usize = 3;

#[derive(Debug, Clone, Default)]
pub struct TaskQuery {
    /// Backticked/quoted identifiers taken verbatim: `` `login_user` `` →
    /// exact symbol-name probe (case preserved).
    pub exact_tokens: Vec<String>,
    /// Lowercased keywords, first-occurrence order, deduped, len ≥ 3, ≤ 12.
    pub keywords: Vec<String>,
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Tokenize a task description. Errors when nothing searchable survives.
pub fn tokenize_task(task: &str) -> Result<TaskQuery, String> {
    let stop: HashSet<&str> = STOPWORDS.iter().copied().collect();
    let mut exact_tokens: Vec<String> = Vec::new();
    let mut keywords: Vec<String> = Vec::new();
    let mut seen_kw: HashSet<String> = HashSet::new();

    let push_words = |text: &str, keywords: &mut Vec<String>, seen: &mut HashSet<String>| {
        for chunk in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            for w in split_ident_words(chunk) {
                if w.len() >= MIN_KEYWORD_LEN
                    && !stop.contains(w.as_str())
                    && keywords.len() < MAX_KEYWORDS
                    && seen.insert(w.clone())
                {
                    keywords.push(w);
                }
            }
        }
    };

    // Pass 1: quoted spans become exact tokens AND feed the keyword pass.
    let mut rest = String::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in task.chars() {
        match quote {
            Some(q) if c == q => {
                if is_ident(&cur) && !exact_tokens.contains(&cur) {
                    exact_tokens.push(cur.clone());
                }
                rest.push(' ');
                rest.push_str(&cur);
                cur.clear();
                quote = None;
            }
            Some(_) => cur.push(c),
            None if c == '`' || c == '"' || c == '\'' => quote = Some(c),
            None => rest.push(c),
        }
    }
    if !cur.is_empty() {
        rest.push(' ');
        rest.push_str(&cur);
    }
    push_words(&rest, &mut keywords, &mut seen_kw);

    if exact_tokens.is_empty() && keywords.is_empty() {
        return Err("task description yields no searchable keywords".to_string());
    }
    Ok(TaskQuery {
        exact_tokens,
        keywords,
    })
}

// ---------------------------------------------------------------------------
// fusion inputs / outputs
// ---------------------------------------------------------------------------

pub struct TargetsOptions {
    pub limit: usize,
}

impl Default for TargetsOptions {
    fn default() -> Self {
        TargetsOptions { limit: 20 }
    }
}

pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 100;

/// Everything the fusion core needs, gathered by the caller so unit tests
/// need neither a real index nor a real graph.
#[derive(Default)]
pub struct SignalInputs {
    /// The full live file universe (`IndexSet::paths()`), sorted.
    pub all_paths: Vec<String>,
    /// keyword → per-file verified content-match counts (trigram search).
    pub content_hits: BTreeMap<String, Vec<(String, u32)>>,
    /// Files defining keyword-matching symbols (graph S2), pre-ranked.
    pub symbol_hits: Vec<SymbolHit>,
    /// 1-hop graph neighbors of the lexical seeds: (path, reason).
    pub graph_neighbors: Vec<(String, String)>,
    /// Cluster co-members of the lexical seeds: (path, reason).
    pub cluster_neighbors: Vec<(String, String)>,
    /// False when the graph could not be built — lexical-only mode.
    pub graph_available: bool,
    /// Aggregate unresolved-call envelope over matched symbol names.
    pub envelope: Option<pixel_graph::Envelope>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetFile {
    pub path: String,
    pub tier: String, // "P0" | "P1" | "P2"
    pub score: f64,
    pub reasons: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct TargetsReport {
    pub task: String,
    pub keywords: Vec<String>,
    pub exact_tokens: Vec<String>,
    pub targets: Vec<TargetFile>,
    pub envelope: Value,
    pub closed_world: String,
    pub stats: Value,
}

// RRF constants — mirrors pixel-recall/src/hybrid.rs, generalized to five
// channels. Filename and symbol-definition evidence dominate content TF;
// graph expansion and cluster co-membership are peripheral by construction.
const RRF_K: f64 = 60.0;
const W_FILENAME: f64 = 3.0;
const W_SYMBOL: f64 = 2.5;
const W_CONTENT: f64 = 1.5;
const W_GRAPH: f64 = 1.0;
const W_CLUSTER: f64 = 0.5;
const EXACT_NAME_BONUS: f64 = 0.05;
const P0_CAP: usize = 5;
const CONTENT_COUNT_CAP: u32 = 50;

// ---------------------------------------------------------------------------
// signal ranking
// ---------------------------------------------------------------------------

/// S1: rank the file universe by path-word keyword overlap. Filename-component
/// hits count double vs directory-component hits.
fn filename_rank(all_paths: &[String], keywords: &[String]) -> Vec<(String, Vec<String>)> {
    let kw: HashSet<&str> = keywords.iter().map(String::as_str).collect();
    let mut scored: Vec<(u32, String, Vec<String>)> = Vec::new();
    for path in all_paths {
        let mut matched: BTreeSet<String> = BTreeSet::new();
        let mut score = 0u32;
        let components: Vec<&str> = path.split('/').collect();
        let last = components.len().saturating_sub(1);
        for (i, comp) in components.iter().enumerate() {
            for w in split_ident_words(comp) {
                if kw.contains(w.as_str()) {
                    score += if i == last { 2 } else { 1 };
                    matched.insert(w);
                }
            }
        }
        if score > 0 {
            scored.push((score, path.clone(), matched.into_iter().collect()));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, p, m)| (p, m)).collect()
}

/// S3: combine per-keyword content hits into one ranked list:
/// distinct keywords desc, capped total matches desc, path asc.
fn content_rank(
    content_hits: &BTreeMap<String, Vec<(String, u32)>>,
) -> Vec<(String, Vec<(String, u32)>)> {
    struct Acc {
        per_kw: Vec<(String, u32)>,
        total: u64,
    }
    let mut by_path: BTreeMap<String, Acc> = BTreeMap::new();
    for (kw, files) in content_hits {
        for (path, count) in files {
            let capped = (*count).min(CONTENT_COUNT_CAP);
            let acc = by_path.entry(path.clone()).or_insert_with(|| Acc {
                per_kw: Vec::new(),
                total: 0,
            });
            acc.per_kw.push((kw.clone(), capped));
            acc.total += capped as u64;
        }
    }
    type ScoredRow = (usize, u64, String, Vec<(String, u32)>);
    let mut scored: Vec<ScoredRow> = by_path
        .into_iter()
        .map(|(p, acc)| (acc.per_kw.len(), acc.total, p, acc.per_kw))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    scored.into_iter().map(|(_, _, p, kw)| (p, kw)).collect()
}

/// Lexical-only pre-fuse used to pick graph-expansion seeds. Returns fused
/// paths, best first.
pub fn lexical_rank(
    all_paths: &[String],
    keywords: &[String],
    symbol_hits: &[SymbolHit],
    content_hits: &BTreeMap<String, Vec<(String, u32)>>,
) -> Vec<String> {
    let s1: Vec<String> = filename_rank(all_paths, keywords)
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    let s2: Vec<String> = symbol_hits.iter().map(|h| h.path.clone()).collect();
    let s3: Vec<String> = content_rank(content_hits)
        .into_iter()
        .map(|(p, _)| p)
        .collect();

    let mut scores: HashMap<String, f64> = HashMap::new();
    for (list, w) in [(&s1, W_FILENAME), (&s2, W_SYMBOL), (&s3, W_CONTENT)] {
        for (rank, path) in list.iter().enumerate() {
            *scores.entry(path.clone()).or_default() += w / (RRF_K + rank as f64 + 1.0);
        }
    }
    let mut out: Vec<(String, f64)> = scores.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out.into_iter().map(|(p, _)| p).collect()
}

// ---------------------------------------------------------------------------
// fusion + tiering
// ---------------------------------------------------------------------------

fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

fn symbol_value(sym: &pixel_graph::SymbolRow) -> Value {
    json!({
        "uid": sym.uid,
        "name": sym.name,
        "kind": sym.kind.as_str(),
        "line": sym.start_line,
    })
}

pub fn compute_targets(
    task: &str,
    query: &TaskQuery,
    inputs: SignalInputs,
    opts: &TargetsOptions,
) -> TargetsReport {
    let limit = opts.limit.clamp(1, MAX_LIMIT);

    // Per-signal ranked lists.
    let s1 = filename_rank(&inputs.all_paths, &query.keywords);
    let s2 = &inputs.symbol_hits;
    let s3 = content_rank(&inputs.content_hits);
    let s4 = &inputs.graph_neighbors;
    let s5 = &inputs.cluster_neighbors;

    // Fuse.
    #[derive(Default)]
    struct Entry {
        score: f64,
        families: u8, // bitmask S1..S5
        reasons: Vec<String>,
        symbols: Vec<Value>,
        exact_name_hit: bool,
    }
    let mut fused: HashMap<String, Entry> = HashMap::new();
    fn bump<'m>(
        fused: &'m mut HashMap<String, Entry>,
        path: &str,
        rank: usize,
        weight: f64,
        family: u8,
    ) -> &'m mut Entry {
        let e = fused.entry(path.to_string()).or_default();
        e.score += weight / (RRF_K + rank as f64 + 1.0);
        e.families |= family;
        e
    }

    for (rank, (path, matched)) in s1.iter().enumerate() {
        let e = bump(&mut fused, path, rank, W_FILENAME, 1);
        e.reasons
            .push(format!("filename match: {}", matched.join(", ")));
    }
    for (rank, hit) in s2.iter().enumerate() {
        let e = bump(&mut fused, &hit.path, rank, W_SYMBOL, 2);
        e.exact_name_hit |= hit.exact_name_hit;
        for (sym, _) in hit.symbols.iter().take(3) {
            e.reasons.push(format!("defines symbol `{}`", sym.name));
            e.symbols.push(symbol_value(sym));
        }
        if hit.symbols.len() > 3 {
            e.reasons
                .push(format!("+{} more matching symbols", hit.symbols.len() - 3));
        }
    }
    for (rank, (path, per_kw)) in s3.iter().enumerate() {
        let e = bump(&mut fused, path, rank, W_CONTENT, 4);
        let detail: Vec<String> = per_kw
            .iter()
            .take(3)
            .map(|(kw, n)| format!("{n} for \"{kw}\""))
            .collect();
        e.reasons
            .push(format!("content matches: {}", detail.join(", ")));
    }
    for (rank, (path, reason)) in s4.iter().enumerate() {
        let e = bump(&mut fused, path, rank, W_GRAPH, 8);
        e.reasons.push(reason.clone());
    }
    for (rank, (path, reason)) in s5.iter().enumerate() {
        let e = bump(&mut fused, path, rank, W_CLUSTER, 16);
        e.reasons.push(reason.clone());
    }

    // Exact-identifier definitions get a flat bonus: a backticked name that is
    // defined in the file is the strongest evidence a task can carry.
    for e in fused.values_mut() {
        if e.exact_name_hit {
            e.score += EXACT_NAME_BONUS;
        }
    }

    // Deterministic fused order.
    let mut ordered: Vec<(String, Entry)> = fused.into_iter().collect();
    ordered.sort_by(|a, b| b.1.score.total_cmp(&a.1.score).then(a.0.cmp(&b.0)));

    // Tier assignment. fams = number of signal families; lexical = S1|S2|S3.
    const LEXICAL_MASK: u8 = 1 | 2 | 4;
    const S1S2_MASK: u8 = 1 | 2;
    let p2_cap = limit.div_ceil(4);
    let mut targets: Vec<TargetFile> = Vec::new();
    let mut p0 = 0usize;
    let mut p2 = 0usize;
    for (path, e) in ordered {
        if targets.len() >= limit {
            break;
        }
        let fams = e.families.count_ones();
        let lexical = e.families & LEXICAL_MASK != 0;
        let tier = if lexical
            && p0 < P0_CAP.min(limit)
            && (e.exact_name_hit || (fams >= 2 && e.families & S1S2_MASK != 0))
        {
            p0 += 1;
            "P0"
        } else if lexical {
            "P1"
        } else {
            // Graph/cluster-only evidence: peripheral and droppable.
            if p2 >= p2_cap {
                continue;
            }
            p2 += 1;
            "P2"
        };
        targets.push(TargetFile {
            path,
            tier: tier.to_string(),
            score: round6(e.score),
            reasons: e.reasons,
            symbols: e.symbols,
        });
    }
    // Present P0 first, then P1, then P2, score order inside each tier
    // (already score-ordered globally; stable sort by tier preserves it).
    targets.sort_by(|a, b| a.tier.cmp(&b.tier));

    // Envelope + closed-world claim.
    let (lower_bound, unresolved, graph_state) = if inputs.graph_available {
        let env = inputs.envelope.clone().unwrap_or(pixel_graph::Envelope {
            lower_bound: false,
            unresolved_same_name: 0,
        });
        (env.lower_bound, env.unresolved_same_name, "fresh")
    } else {
        (true, 0, "unavailable")
    };
    let note = if graph_state == "unavailable" {
        "code graph unavailable — lexical signals only; graph-adjacent files may be missing"
            .to_string()
    } else if lower_bound {
        format!(
            "{unresolved} unresolved call site(s) share a matched symbol name; callers beyond this list may exist"
        )
    } else {
        String::new()
    };
    let mut closed_world = String::from(
        "Restrict reads and edits to the files listed. P2 entries are peripheral and droppable. \
         This list is exhaustive for the indexed tree",
    );
    if lower_bound {
        closed_world
            .push_str(" EXCEPT: envelope.lower_bound is true, so unlisted files may be involved.");
    } else {
        closed_world.push('.');
    }

    let signal_hits = json!({
        "filename": s1.len(),
        "symbol": s2.len(),
        "content": s3.len(),
        "graph": s4.len(),
        "cluster": s5.len(),
    });

    TargetsReport {
        task: task.to_string(),
        keywords: query.keywords.clone(),
        exact_tokens: query.exact_tokens.clone(),
        targets,
        envelope: json!({
            "lower_bound": lower_bound,
            "graph": graph_state,
            "unresolved_same_name": unresolved,
            "note": note,
        }),
        closed_world,
        stats: json!({
            "files_considered": inputs.all_paths.len(),
            "signal_hits": signal_hits,
            "limit": limit,
        }),
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pixel_graph::store::{SymbolKind, SymbolRow};

    fn hit(path: &str, name: &str, exact: bool, distinct: usize) -> SymbolHit {
        SymbolHit {
            path: path.to_string(),
            symbols: vec![(
                SymbolRow {
                    id: 1,
                    uid: format!("{path}#{name}#function"),
                    file_id: 1,
                    name: name.to_string(),
                    qualified: name.to_string(),
                    kind: SymbolKind::Function,
                    start_line: 1,
                    end_line: 5,
                    sig: String::new(),
                },
                name.to_string(),
            )],
            distinct_keywords: distinct,
            exact_name_hit: exact,
        }
    }

    #[test]
    fn tokenize_splits_idents_and_drops_stopwords() {
        let q = tokenize_task("fix loginUser session handling in AuthService").unwrap();
        assert_eq!(
            q.keywords,
            vec!["login", "user", "session", "handling", "auth", "service"]
        );
        assert!(q.exact_tokens.is_empty());
    }

    #[test]
    fn tokenize_extracts_backticked_identifiers() {
        let q = tokenize_task("`login_user` panics on empty token").unwrap();
        assert_eq!(q.exact_tokens, vec!["login_user"]);
        assert_eq!(
            q.keywords,
            vec!["login", "user", "panics", "empty", "token"]
        );
    }

    #[test]
    fn tokenize_all_stopwords_errors() {
        assert!(tokenize_task("fix the code in a file").is_err());
        assert!(tokenize_task("").is_err());
    }

    #[test]
    fn multi_signal_file_beats_single_signal_top() {
        // a.rs: filename + content. b.rs: content only (top of that channel).
        let mut content = BTreeMap::new();
        content.insert(
            "login".to_string(),
            vec![
                ("src/b.rs".to_string(), 40u32),
                ("src/login.rs".to_string(), 2u32),
            ],
        );
        let inputs = SignalInputs {
            all_paths: vec!["src/b.rs".into(), "src/login.rs".into()],
            content_hits: content,
            graph_available: true,
            ..Default::default()
        };
        let q = TaskQuery {
            exact_tokens: vec![],
            keywords: vec!["login".into()],
        };
        let report = compute_targets("t", &q, inputs, &TargetsOptions::default());
        assert_eq!(report.targets[0].path, "src/login.rs");
        assert_eq!(report.targets[0].tier, "P0"); // filename + content = 2 families
        assert_eq!(report.targets[1].path, "src/b.rs");
        assert_eq!(report.targets[1].tier, "P1"); // content only
    }

    #[test]
    fn exact_name_hit_is_p0_and_cluster_only_is_p2() {
        let inputs = SignalInputs {
            all_paths: vec!["src/a.rs".into(), "src/z.rs".into()],
            symbol_hits: vec![hit("src/a.rs", "login_user", true, 1)],
            cluster_neighbors: vec![("src/z.rs".into(), "same cluster 'auth'".into())],
            graph_available: true,
            ..Default::default()
        };
        let q = TaskQuery {
            exact_tokens: vec!["login_user".into()],
            keywords: vec!["login".into(), "user".into()],
        };
        let report = compute_targets("t", &q, inputs, &TargetsOptions::default());
        let a = report
            .targets
            .iter()
            .find(|t| t.path == "src/a.rs")
            .unwrap();
        assert_eq!(a.tier, "P0");
        assert!(
            a.reasons
                .iter()
                .any(|r| r.contains("defines symbol `login_user`"))
        );
        let z = report
            .targets
            .iter()
            .find(|t| t.path == "src/z.rs")
            .unwrap();
        assert_eq!(z.tier, "P2");
    }

    #[test]
    fn limit_is_hard_and_p2_capped() {
        let all: Vec<String> = (0..30).map(|i| format!("src/login_{i:02}.rs")).collect();
        let cluster: Vec<(String, String)> = (0..10)
            .map(|i| (format!("src/extra_{i:02}.rs"), "same cluster 'x'".into()))
            .collect();
        let inputs = SignalInputs {
            all_paths: all.clone(),
            cluster_neighbors: cluster,
            graph_available: true,
            ..Default::default()
        };
        let q = TaskQuery {
            exact_tokens: vec![],
            keywords: vec!["login".into()],
        };
        let report = compute_targets("t", &q, inputs, &TargetsOptions { limit: 8 });
        assert_eq!(report.targets.len(), 8);
        let p2 = report.targets.iter().filter(|t| t.tier == "P2").count();
        assert!(p2 <= 2); // ceil(8/4)
    }

    #[test]
    fn deterministic_output() {
        let mk = || {
            let mut content = BTreeMap::new();
            content.insert(
                "auth".to_string(),
                vec![
                    ("src/x.rs".to_string(), 3u32),
                    ("src/y.rs".to_string(), 3u32),
                ],
            );
            SignalInputs {
                all_paths: vec!["src/x.rs".into(), "src/y.rs".into()],
                content_hits: content,
                graph_available: true,
                ..Default::default()
            }
        };
        let q = TaskQuery {
            exact_tokens: vec![],
            keywords: vec!["auth".into()],
        };
        let a = serde_json::to_string(&compute_targets("t", &q, mk(), &TargetsOptions::default()))
            .unwrap();
        let b = serde_json::to_string(&compute_targets("t", &q, mk(), &TargetsOptions::default()))
            .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn graph_unavailable_sets_lower_bound() {
        let inputs = SignalInputs {
            all_paths: vec!["src/login.rs".into()],
            graph_available: false,
            ..Default::default()
        };
        let q = TaskQuery {
            exact_tokens: vec![],
            keywords: vec!["login".into()],
        };
        let report = compute_targets("t", &q, inputs, &TargetsOptions::default());
        assert_eq!(report.envelope["lower_bound"], true);
        assert_eq!(report.envelope["graph"], "unavailable");
        assert!(report.closed_world.contains("EXCEPT"));
    }
}
