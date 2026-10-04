// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Human-run diagnostic over `unresolved_calls` in a built graph db.
//!
//! The epistemic envelope says *that* N call sites went unresolved; it does
//! not say which of them are workspace code a better resolver could link and
//! which are std/dependency names no name-only resolver can reach. This test
//! prints that split, so a resolver change can be scoped to the subset that
//! matters and its effect measured instead of argued.
//!
//! Run it against this repository:
//!
//! ```sh
//! pixel rebuild-graph .
//! cargo test -p pixel-graph --test all -- --ignored unresolved_breakdown --nocapture
//! ```
//!
//! `PIXEL_GRAPH_DIAG_DB=<path>` points it at another `graph.db`. The test
//! only reads, asserts nothing, and is ignored by default. Every table is
//! sorted, so two runs over the same db print identical text.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use pixel_graph::resolve::{Decision, ResolveIndex};
use pixel_graph::store::GraphStore;

/// Callable kinds `ResolveIndex` keeps, mirroring `resolve::callable`.
fn callable(kind: &str) -> bool {
    matches!(kind, "function" | "method" | "class" | "struct")
}

/// Lower number wins, mirroring `resolve::kind_priority`.
fn kind_priority(kind: &str) -> u8 {
    match kind {
        "function" => 0,
        "method" => 1,
        "class" => 2,
        _ => 3,
    }
}

fn is_ident(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `std::fs::read` yes; `std::env::var(name).ok()?` no.
fn is_pure_path(text: &str) -> bool {
    text.contains("::") && text.split("::").all(is_ident)
}

/// The receiver shapes the resolver's type tiebreak accepts: a plain
/// identifier (`Store`, `w`) or a `::`-separated path (`pixel::Store`).
fn is_value_path(text: &str) -> bool {
    let text = text.trim();
    is_ident(text) || is_pure_path(text)
}

fn receiver_shape(receiver: Option<&str>) -> &'static str {
    match receiver.map(str::trim) {
        None | Some("") => "none",
        Some("self" | "Self" | "this" | "crate" | "super" | "Self::") => "self-like",
        Some(r) if is_pure_path(r) => "path",
        Some(r) if is_ident(r) => "ident",
        Some(_) => "expression",
    }
}

/// Leading segment of a pure path, for the std/external split: `std::fs::read`
/// is std, `serde_json::Value` is a dependency, `pixel_git::GitRunner` is not
/// (its crate is in the graph).
fn path_root(text: &str) -> &str {
    text.split("::").next().unwrap_or(text)
}

#[derive(Default)]
struct Def {
    symbols: u64,
    files: BTreeSet<i64>,
    /// Best candidate by `(kind_priority, start_line, symbol_id)`, mirroring
    /// `resolve::best`, so the report describes the symbol a tier would pick.
    best: Option<(u8, u32, i64, String, String, bool)>,
    /// Every candidate's qualified name, for the path-prefix probe.
    qualifieds: BTreeSet<String>,
}

impl Def {
    fn add(
        &mut self,
        file_id: i64,
        kind: &str,
        trait_impl: bool,
        qualified: &str,
        id: i64,
        start_line: u32,
    ) {
        self.symbols += 1;
        self.files.insert(file_id);
        self.qualifieds.insert(qualified.to_string());
        let candidate = (
            kind_priority(kind),
            start_line,
            id,
            kind.to_string(),
            qualified.to_string(),
            trait_impl,
        );
        if self.best.as_ref().is_none_or(|b| candidate < *b) {
            self.best = Some(candidate);
        }
    }
}

#[derive(Default)]
struct NameStats {
    calls: u64,
    references: u64,
    shadowed: u64,
    top_level: u64,
    /// Sites whose call has an enclosing symbol but no rule reaches a tier.
    no_candidate: u64,
    /// Raw (receiver-ignoring) resolution succeeds but the stored row is
    /// unresolved *and* the receiver-aware decision is not the shadow veto:
    /// a stale row that re-resolution should have deleted.
    stale: u64,
    shapes: BTreeSet<&'static str>,
    /// Pure-path receivers whose last segment prefixes exactly one candidate's
    /// qualified name — what a path-aware tier could link.
    path_matches: u64,
    path_example: Option<String>,
}

#[derive(Default)]
struct Bucket {
    calls: u64,
    references: u64,
    shadow: u64,
    top_level: u64,
    unreached: u64,
    stale: u64,
    names: BTreeSet<String>,
}

/// One `unresolved_calls` row with everything the report groups it by.
struct Site {
    name: String,
    file_id: i64,
    path: String,
    site_line: u32,
    receiver: Option<String>,
    kind: String,
    enclosing: Option<i64>,
}

impl Bucket {
    fn add(&mut self, name: &str, references: bool, outcome: &str) {
        self.names.insert(name.to_string());
        if references {
            self.references += 1;
        } else {
            self.calls += 1;
        }
        match outcome {
            "shadow" => self.shadow += 1,
            "top-level" => self.top_level += 1,
            "unreached" => self.unreached += 1,
            _ => self.stale += 1,
        }
    }
}

/// The real-receiver test `resolve::has_real_receiver` applies: `self` and
/// the module pseudo-receivers resolve against the enclosing scope.
fn has_real_receiver(receiver: Option<&str>) -> bool {
    receiver.map(str::trim).is_some_and(|r| {
        !r.is_empty()
            && !matches!(
                r,
                "self" | "Self" | "this" | "crate" | "super" | "Self::" | "self."
            )
    })
}

fn line(name: &str, s: &NameStats) -> String {
    let shapes: Vec<&str> = s.shapes.iter().copied().collect();
    format!(
        "{name:<28} calls={:<5} refs={:<4} shadow={:<4} top={:<4} unreached={:<4} shapes={}",
        s.calls,
        s.references,
        s.shadowed,
        s.top_level,
        s.no_candidate + s.stale,
        shapes.join(",")
    )
}

#[test]
#[ignore = "diagnostic report; run with a built graph db and --nocapture"]
fn unresolved_breakdown() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let db = std::env::var("PIXEL_GRAPH_DIAG_DB")
        .map_or_else(|_| root.join(".pixel").join("graph.db"), PathBuf::from);
    assert!(
        db.is_file(),
        "{} is missing: run `pixel rebuild-graph .` (or set PIXEL_GRAPH_DIAG_DB)",
        db.display()
    );
    let store = GraphStore::open(&db).expect("open graph db");
    let idx = ResolveIndex::build(&store).expect("build resolver index");

    let sites: Vec<Site> = {
        let mut stmt = store
            .conn()
            .prepare(
                "SELECT u.name, u.file_id, f.path, u.site_line, u.receiver, u.kind, u.enclosing_symbol_id
                   FROM unresolved_calls u
                   JOIN files f ON f.id = u.file_id
                  ORDER BY f.path, u.site_line, u.name, u.id",
            )
            .expect("prepare");
        let rows = stmt
            .query_map([], |r| {
                Ok(Site {
                    name: r.get(0)?,
                    file_id: r.get(1)?,
                    path: r.get(2)?,
                    site_line: r.get(3)?,
                    receiver: r.get(4)?,
                    kind: r.get(5)?,
                    enclosing: r.get(6)?,
                })
            })
            .expect("query");
        rows.collect::<Result<_, _>>().expect("collect")
    };

    let mut defs: BTreeMap<String, Def> = BTreeMap::new();
    {
        let mut stmt = store
            .conn()
            .prepare(
                "SELECT name, file_id, kind, trait_impl, qualified, id, start_line FROM symbols",
            )
            .expect("prepare");
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, bool>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, u32>(6)?,
                ))
            })
            .expect("query");
        for row in rows {
            let (name, file_id, kind, trait_impl, qualified, id, start_line) = row.expect("row");
            if !callable(&kind) {
                continue;
            }
            defs.entry(name)
                .or_default()
                .add(file_id, &kind, trait_impl, &qualified, id, start_line);
        }
    }

    let mut total_by_kind: BTreeMap<&str, u64> = BTreeMap::new();
    let mut per_name: BTreeMap<String, NameStats> = BTreeMap::new();
    let mut per_file: BTreeMap<String, Bucket> = BTreeMap::new();
    let mut class_total: BTreeMap<&str, Bucket> = BTreeMap::new();
    let mut class_workspace: BTreeMap<&str, Bucket> = BTreeMap::new();
    let mut class_external_std: BTreeMap<&str, Bucket> = BTreeMap::new();

    for site in &sites {
        *total_by_kind.entry(site.kind.as_str()).or_default() += 1;
        let reference = site.kind == "references";
        let def = defs.get(&site.name);
        let class = match def {
            None => "none",
            Some(d) if d.symbols == 1 => "one-symbol",
            Some(d) if d.files.len() == 1 => "several-symbols-one-file",
            Some(_) => "several-files",
        };
        let stats = per_name.entry(site.name.clone()).or_default();
        if reference {
            stats.references += 1;
        } else {
            stats.calls += 1;
        }
        stats
            .shapes
            .insert(receiver_shape(site.receiver.as_deref()));

        // Classify against the *shadow veto as documented* (real receiver +
        // a same-name definition in the caller's own file), not against the
        // current resolver: a report on a db built by an older resolver must
        // still say which rows that resolver refused to guess.
        let outcome = if site.enclosing.is_none() {
            stats.top_level += 1;
            "top-level"
        } else if has_real_receiver(site.receiver.as_deref())
            && def.is_some_and(|d| d.files.contains(&site.file_id))
        {
            stats.shadowed += 1;
            "shadow"
        } else if matches!(
            idx.decide_at(site.file_id, &site.name, None, site.site_line),
            Decision::Unresolved
        ) {
            stats.no_candidate += 1;
            "unreached"
        } else {
            // Resolution succeeds without the receiver: a stale row that
            // re-resolution should have deleted.
            stats.stale += 1;
            "stale"
        };

        if let (Some(receiver), Some(def)) = (site.receiver.as_deref(), def)
            && is_value_path(receiver)
        {
            let segment = receiver.rsplit("::").next().unwrap_or(receiver);
            let prefix = format!("{segment}::");
            let hits: Vec<&str> = def
                .qualifieds
                .iter()
                .filter(|q| q.starts_with(&prefix))
                .map(String::as_str)
                .collect();
            if hits.len() == 1 {
                stats.path_matches += 1;
                stats
                    .path_example
                    .get_or_insert_with(|| format!("{receiver}::{} -> {}", site.name, hits[0]));
            }
        }

        class_total
            .entry(class)
            .or_default()
            .add(&site.name, reference, outcome);
        if def.is_some() {
            class_workspace
                .entry(class)
                .or_default()
                .add(&site.name, reference, outcome);
        } else {
            let root = site
                .receiver
                .as_deref()
                .filter(|r| is_pure_path(r))
                .map_or("?", path_root);
            let external_class = if matches!(root, "std" | "core" | "alloc") {
                "std-path"
            } else {
                "no-workspace-definition"
            };
            class_external_std
                .entry(external_class)
                .or_default()
                .add(&site.name, reference, outcome);
        }

        per_file
            .entry(site.path.clone())
            .or_default()
            .add(&site.name, reference, outcome);
    }

    let total: u64 = total_by_kind.values().sum();
    println!("== unresolved_calls breakdown ==");
    println!("db: {}", db.display());
    println!("total={total} by kind={total_by_kind:?}");

    println!("\n-- by workspace-definition class (zero / one / several symbols with that name) --");
    for (class, b) in &class_total {
        println!(
            "{class:<26} calls={:<6} refs={:<5} names={:<4} shadow={:<5} top={:<4} unreached={:<5} stale={}",
            b.calls,
            b.references,
            b.names.len(),
            b.shadow,
            b.top_level,
            b.unreached,
            b.stale
        );
    }
    println!("-- workspace-defined subset --");
    for (class, b) in &class_workspace {
        println!(
            "{class:<26} calls={:<6} refs={:<5} names={:<4} shadow={:<5} top={:<4} unreached={:<5} stale={}",
            b.calls,
            b.references,
            b.names.len(),
            b.shadow,
            b.top_level,
            b.unreached,
            b.stale
        );
    }
    println!("-- no workspace definition --");
    for (class, b) in &class_external_std {
        println!(
            "{class:<26} calls={:<6} refs={:<5} names={:<4} shadow={:<5} top={:<4} unreached={:<5} stale={}",
            b.calls,
            b.references,
            b.names.len(),
            b.shadow,
            b.top_level,
            b.unreached,
            b.stale
        );
    }

    let rank =
        |filter: &dyn Fn(&str, &NameStats, Option<&Def>) -> bool| -> Vec<(String, NameStats)> {
            let mut out: Vec<(String, NameStats)> = per_name
                .iter()
                .filter(|(name, s)| filter(name, s, defs.get(*name)))
                .map(|(name, s)| {
                    let s = NameStats {
                        calls: s.calls,
                        references: s.references,
                        shadowed: s.shadowed,
                        top_level: s.top_level,
                        no_candidate: s.no_candidate,
                        stale: s.stale,
                        shapes: s.shapes.clone(),
                        path_matches: s.path_matches,
                        path_example: s.path_example.clone(),
                    };
                    (name.clone(), s)
                })
                .collect();
            out.sort_by(|a, b| b.1.calls.cmp(&a.1.calls).then(a.0.cmp(&b.0)));
            out
        };

    let top = |list: &[(String, NameStats)], n: usize| {
        for (name, s) in list.iter().take(n) {
            println!("{}", line(name, s));
        }
    };

    println!("\n-- top 30 workspace-defined names --");
    let workspace = rank(&|_, _, def| def.is_some());
    top(&workspace, 30);

    println!("\n-- the definable subset: one callable symbol with that name --");
    let single = rank(&|_, _, def| def.is_some_and(|d| d.symbols == 1));
    println!(
        "(all {} names, {} calls)",
        single.len(),
        single.iter().map(|(_, s)| s.calls).sum::<u64>()
    );
    top(&single, 40);

    println!("\n-- several files define it (receiver type / import is the only tiebreak) --");
    let ambiguous = rank(&|_, _, def| def.is_some_and(|d| d.files.len() > 1));
    println!(
        "(all {} names, {} calls)",
        ambiguous.len(),
        ambiguous.iter().map(|(_, s)| s.calls).sum::<u64>()
    );
    top(&ambiguous, 30);

    println!("\n-- no workspace definition (std/deps/macros/dynamic) --");
    let external = rank(&|_, _, def| def.is_none());
    println!(
        "(all {} names, {} calls)",
        external.len(),
        external.iter().map(|(_, s)| s.calls).sum::<u64>()
    );
    top(&external, 30);

    println!(
        "\n-- path-qualified candidates (receiver path segment == candidate qualified prefix) --"
    );
    let pathy = rank(&|_, s, _| s.path_matches > 0);
    for (name, s) in pathy.iter().take(20) {
        println!(
            "{name:<28} matches={:<4} example={}",
            s.path_matches,
            s.path_example.as_deref().unwrap_or("-")
        );
    }
    println!(
        "(all {} names, {} sites)",
        pathy.len(),
        pathy.iter().map(|(_, s)| s.path_matches).sum::<u64>()
    );

    println!("\n-- top 20 call-site files (all unresolved) --");
    let mut files: Vec<(String, Bucket)> = per_file.into_iter().collect();
    files.sort_by(|a, b| {
        (b.1.calls + b.1.references)
            .cmp(&(a.1.calls + a.1.references))
            .then(a.0.cmp(&b.0))
    });
    for (path, b) in files.iter().take(20) {
        println!(
            "{path:<58} calls={:<5} refs={:<4} names={}",
            b.calls,
            b.references,
            b.names.len()
        );
    }
}
