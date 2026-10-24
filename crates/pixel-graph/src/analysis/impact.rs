// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The blast radius of one symbol: a breadth-first walk over the `Call` edges
//! of the generation, bucketed by depth (`specs/graph.md`).
//!
//! Depth one will break, depth two is likely affected, depth three may need
//! tests. An item's tier is the weakest edge on its way from the target. A
//! symbol that is only *named* (passed as a callback, registered) is listed
//! apart as `referenced_by`: it may be invoked, not will break.

use std::collections::{BTreeSet, HashSet};

use pixel_model::{EdgeKind, Symbol, Tier, leaf};

use crate::store::{Store, StoreError};

/// How many levels a walk goes; the buckets of [`Impact`] are one per level.
pub const DEPTHS: usize = 3;

/// The most items listed in one bucket; the counts stay exact.
pub const DEFAULT_LIMIT_PER_DEPTH: usize = 30;

/// The most `referenced_by` items listed.
pub const MAX_REFERENCED_BY: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Who reaches the symbol.
    Upstream,
    /// What the symbol reaches.
    Downstream,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Upstream => "upstream",
            Direction::Downstream => "downstream",
        }
    }

    pub fn parse(text: &str) -> Option<Direction> {
        match text.to_ascii_lowercase().as_str() {
            "upstream" | "up" | "callers" => Some(Direction::Upstream),
            "downstream" | "down" | "callees" => Some(Direction::Downstream),
            _ => None,
        }
    }
}

/// How much a change to the symbol disturbs. `Unknown` when an upstream walk
/// found no caller at all: an empty caller set is as consistent with "unused"
/// as with "reached only through something the index does not record".
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    Low,
    Medium,
    High,
    Critical,
    Unknown,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Low => "LOW",
            Risk::Medium => "MEDIUM",
            Risk::High => "HIGH",
            Risk::Critical => "CRITICAL",
            Risk::Unknown => "UNKNOWN",
        }
    }
}

/// The risk of `direct` depth-one callers, one step higher when sites of the
/// same name stayed unresolved (`lower_bound`).
fn risk_of(direct: u64, lower_bound: bool) -> Risk {
    let level: u8 = match direct {
        0..=3 => 0,
        4..=15 => 1,
        16..=50 => 2,
        _ => 3,
    };
    match level + u8::from(lower_bound) {
        0 => Risk::Low,
        1 => Risk::Medium,
        2 => Risk::High,
        _ => Risk::Critical,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Levels walked, at most [`DEPTHS`].
    pub max_depth: usize,
    pub limit_per_depth: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_depth: DEPTHS,
            limit_per_depth: DEFAULT_LIMIT_PER_DEPTH,
        }
    }
}

/// A symbol the walk reached, and how sure the way to it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub symbol: Symbol,
    pub tier: Tier,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Impact {
    pub target: Symbol,
    pub direction: Direction,
    /// Upstream only.
    pub risk: Option<Risk>,
    /// The items by depth, each capped at `limit_per_depth`.
    pub depths: [Vec<Item>; DEPTHS],
    /// Every item found by depth, before the cap.
    pub counts: [u64; DEPTHS],
    /// Distinct files holding an item.
    pub affected_files: u64,
    /// Upstream only: symbols that name the target without calling it.
    pub referenced_by: Vec<Item>,
    pub referenced_by_total: u64,
    /// Sites no rule resolved whose callee shares the target's last name
    /// segment: the upstream counts are a lower bound by this many.
    pub open_sites: u64,
    /// One entry per list that was cut, so a consumer can say so.
    pub caps: Vec<String>,
}

/// The symbols a token (a qualified name or its last segment) names, in uid
/// order; `File` and `Package` rows are not what anyone asks the blast
/// radius of.
pub fn resolve_target(store: &Store, token: &str) -> Result<Vec<Symbol>, StoreError> {
    let mut named = store.named_symbols(token)?;
    named.retain(|s| {
        !matches!(
            s.kind,
            pixel_model::SymbolKind::File | pixel_model::SymbolKind::Package
        )
    });
    Ok(named)
}

/// The weaker of two tiers (`Proved` < `Likely` < `Open`).
fn weaker(a: Tier, b: Tier) -> Tier {
    if a.as_i64() >= b.as_i64() { a } else { b }
}

pub fn impact(
    store: &Store,
    target: &Symbol,
    direction: Direction,
    options: &Options,
) -> Result<Impact, StoreError> {
    let max_depth = options.max_depth.min(DEPTHS);
    let mut seen: HashSet<String> = HashSet::from([target.uid.clone()]);
    let mut levels: [Vec<Item>; DEPTHS] = Default::default();
    // The frontier: symbols reached at the previous depth, with the weakest
    // tier on their way.
    let mut frontier: Vec<(Symbol, Tier)> = vec![(target.clone(), Tier::Proved)];
    for level in levels.iter_mut().take(max_depth) {
        let mut next: Vec<(Symbol, Tier)> = Vec::new();
        for (from, path_tier) in &frontier {
            for (symbol, tier) in step(store, from, direction)? {
                if seen.insert(symbol.uid.clone()) {
                    let tier = weaker(*path_tier, tier);
                    level.push(Item {
                        symbol: symbol.clone(),
                        tier,
                    });
                    next.push((symbol, tier));
                }
            }
        }
        level.sort_by(|a, b| {
            (&a.symbol.file, a.symbol.line, &a.symbol.uid).cmp(&(
                &b.symbol.file,
                b.symbol.line,
                &b.symbol.uid,
            ))
        });
        frontier = next;
    }
    let counts = [0, 1, 2].map(|i| levels[i].len() as u64);
    let affected_files = levels
        .iter()
        .flatten()
        .map(|item| item.symbol.file.as_str())
        .collect::<BTreeSet<_>>()
        .len() as u64;
    let mut caps = Vec::new();
    for (i, level) in levels.iter_mut().enumerate() {
        if level.len() > options.limit_per_depth {
            level.truncate(options.limit_per_depth);
            caps.push(format!(
                "depth {} listed {} of {} items",
                i + 1,
                options.limit_per_depth,
                counts[i]
            ));
        }
    }
    let (risk, referenced_by, referenced_by_total, open_sites) = match direction {
        Direction::Upstream => {
            let open_sites = store
                .open_sites(leaf(&target.name))?
                .iter()
                .filter(|edge| edge.src != target.uid)
                .count() as u64;
            let (mut refs, total) = referenced_by(store, target)?;
            if total as usize > MAX_REFERENCED_BY {
                refs.truncate(MAX_REFERENCED_BY);
                caps.push(format!(
                    "referenced_by listed {MAX_REFERENCED_BY} of {total} symbols"
                ));
            }
            let risk = if counts[0] == 0 {
                Risk::Unknown
            } else {
                risk_of(counts[0], open_sites > 0)
            };
            (Some(risk), refs, total, open_sites)
        }
        Direction::Downstream => (None, Vec::new(), 0, 0),
    };
    Ok(Impact {
        target: target.clone(),
        direction,
        risk,
        depths: levels,
        counts,
        affected_files,
        referenced_by,
        referenced_by_total,
        open_sites,
        caps,
    })
}

/// One step of the walk: the symbols directly reached from `from`, each
/// with the tier of the edge that reaches it. Only `Call` edges.
fn step(
    store: &Store,
    from: &Symbol,
    direction: Direction,
) -> Result<Vec<(Symbol, Tier)>, StoreError> {
    let mut out = Vec::new();
    match direction {
        Direction::Upstream => {
            for caller in store.callers(&from.uid, Tier::Likely)? {
                if caller.edge.kind == EdgeKind::Call {
                    out.push((caller.site, caller.edge.tier));
                }
            }
        }
        Direction::Downstream => {
            for edge in store.callees(&from.uid)? {
                if edge.kind != EdgeKind::Call || edge.tier == Tier::Open {
                    continue;
                }
                if let Some(symbol) = edge
                    .dst
                    .as_deref()
                    .map(|uid| store.symbol(uid))
                    .transpose()?
                    .flatten()
                {
                    out.push((symbol, edge.tier));
                }
            }
        }
    }
    Ok(out)
}

/// The distinct symbols that name `target` without calling it.
fn referenced_by(store: &Store, target: &Symbol) -> Result<(Vec<Item>, u64), StoreError> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut items = Vec::new();
    for caller in store.callers(&target.uid, Tier::Likely)? {
        if caller.edge.kind == EdgeKind::Reference
            && caller.site.uid != target.uid
            && seen.insert(caller.site.uid.clone())
        {
            items.push(Item {
                symbol: caller.site,
                tier: caller.edge.tier,
            });
        }
    }
    let total = items.len() as u64;
    Ok((items, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::fixture::project;

    fn target(store: &Store, name: &str) -> Symbol {
        resolve_target(store, name).expect("named").remove(0)
    }

    fn names(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.symbol.name.as_str()).collect()
    }

    #[test]
    fn upstream_buckets_callers_by_distance() {
        let (_repo, store) = project();
        let found = impact(
            &store,
            &target(&store, "run"),
            Direction::Upstream,
            &Options::default(),
        )
        .unwrap();
        assert_eq!(names(&found.depths[0]), ["go"]);
        assert_eq!(names(&found.depths[1]), ["t"]);
        assert!(found.depths[2].is_empty());
        assert_eq!(found.counts, [1, 1, 0]);
        assert_eq!(found.affected_files, 2);
        assert_eq!(found.depths[0][0].tier, Tier::Proved);
        assert_eq!(found.risk, Some(Risk::Low));
        assert_eq!(found.open_sites, 0);
    }

    #[test]
    fn downstream_lists_what_the_symbol_reaches() {
        let (_repo, store) = project();
        let found = impact(
            &store,
            &target(&store, "go"),
            Direction::Downstream,
            &Options::default(),
        )
        .unwrap();
        assert_eq!(names(&found.depths[0]), ["run", "other"]);
        assert_eq!(found.risk, None);
        assert!(found.referenced_by.is_empty());
    }

    #[test]
    fn nothing_calling_a_symbol_is_unknown_never_low() {
        let (_repo, store) = project();
        let found = impact(
            &store,
            &target(&store, "lonely"),
            Direction::Upstream,
            &Options::default(),
        )
        .unwrap();
        assert_eq!(found.counts, [0, 0, 0]);
        assert_eq!(found.risk, Some(Risk::Unknown));
    }

    #[test]
    fn a_depth_limit_stops_the_walk_and_a_bucket_cap_cuts_the_list_not_the_count() {
        let (_repo, store) = project();
        let run = target(&store, "run");
        let shallow = impact(
            &store,
            &run,
            Direction::Upstream,
            &Options {
                max_depth: 1,
                ..Options::default()
            },
        )
        .unwrap();
        assert_eq!(shallow.counts, [1, 0, 0]);
        let capped = impact(
            &store,
            &target(&store, "go"),
            Direction::Downstream,
            &Options {
                limit_per_depth: 1,
                ..Options::default()
            },
        )
        .unwrap();
        assert_eq!(capped.counts[0], 2);
        assert_eq!(capped.depths[0].len(), 1);
        assert_eq!(capped.caps, ["depth 1 listed 1 of 2 items"]);
    }

    #[test]
    fn risk_climbs_with_callers_and_with_unresolved_sites() {
        assert_eq!(risk_of(3, false), Risk::Low);
        assert_eq!(risk_of(4, false), Risk::Medium);
        assert_eq!(risk_of(15, false), Risk::Medium);
        assert_eq!(risk_of(16, false), Risk::High);
        assert_eq!(risk_of(50, false), Risk::High);
        assert_eq!(risk_of(51, false), Risk::Critical);
        assert_eq!(risk_of(1, true), Risk::Medium);
        assert_eq!(risk_of(60, true), Risk::Critical);
    }

    #[test]
    fn a_token_names_symbols_but_never_files() {
        let (_repo, store) = project();
        assert!(resolve_target(&store, "src/a.rs").unwrap().is_empty());
        assert_eq!(resolve_target(&store, "RUN").unwrap().len(), 1);
        assert_eq!(Direction::parse("callers"), Some(Direction::Upstream));
        assert_eq!(Direction::parse("sideways"), None);
    }
}
