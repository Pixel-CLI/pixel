// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The read ops: everything a published generation answers without writing
//! (`specs/ops.md`). The writer's `Service` and a read replica both build a
//! [`Reader`] over the same shared state, so a query has one implementation
//! and one answer.
//!
//! Every answer is a lower bound and says so in its epistemics: a site no
//! rule resolved is counted (`open_sites`), never listed as a caller.

use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use pixel_graph::analysis::changes::changes;
use pixel_graph::analysis::impact::{Direction, Impact, Item, Options, Risk, impact};
use pixel_graph::analysis::resolve::{Resolved, resolve};
use pixel_graph::store::{Store, StoreError};
use pixel_graph::tree;
use pixel_index::history::FactsStore;
use pixel_index::history::search::{SearchFacet, search};
use pixel_index::indexset::IndexSet;
use pixel_index::path_filter::PathFilter;
use pixel_model::{Symbol, SymbolKind, Tier, leaf};
use pixel_proto::{Envelope, Epistemics, ErrorCode, Op, PixelError, SnapshotInfo, Warning};
use serde_json::{Value, json};

use crate::api::{Response, failure_response};
use crate::brief::{self, Ask};
use crate::service::Shared;

/// Rows a `search` returns when the caller names no limit.
pub const SEARCH_DEFAULT_ROWS: usize = 100;

/// The most rows one `search` returns, whatever the caller asks.
pub const SEARCH_MAX_ROWS: usize = 1_000;

/// History hits returned when the caller names no limit.
pub const HISTORY_DEFAULT_ROWS: usize = 20;

/// Callers, callees and changed symbols listed per page.
pub const PAGE_ROWS: usize = 50;

/// Symbols a `symbol` lookup lists.
pub const SYMBOL_ROWS: usize = 50;

/// Files a `map` lists before it says it was cut.
pub const MAP_FILE_CAP: usize = 2_000;

/// What the reader attaches to every answer: the repository's state when
/// the generation was published.
#[derive(Clone, Debug, Default)]
pub struct Published {
    pub snapshot: SnapshotInfo,
    pub generation: u64,
    /// The graph holds every edit batch the writer has finished. A replica
    /// answers a graph read only while it does, or when the caller accepts
    /// the generation before a pending batch (`stale_ok`).
    pub graph_fresh: bool,
}

pub(crate) struct Reader<'a> {
    pub shared: &'a Arc<Shared>,
    pub root: &'a Path,
    pub index: &'a RwLock<IndexSet>,
    pub published: &'a Published,
    /// How long the oldest watcher batch not yet published has waited;
    /// `Some` marks the answer as from the generation before it.
    pub behind: Option<Duration>,
}

/// How a read ended.
pub(crate) enum Outcome {
    Done(Value, Basis),
    Fail(String),
    Ambiguous(String, Vec<Symbol>),
    /// There is no graph to read yet: the writer builds it and asks again.
    NoGraph,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Basis {
    Graph,
    Index,
    History,
}

impl Basis {
    fn name(self) -> &'static str {
        match self {
            Basis::Graph => "graph",
            Basis::Index => "index",
            Basis::History => "history",
        }
    }
}

/// What static analysis cannot see, named in every graph answer.
const EXTRACTION_LIMITS: [&str; 4] = [
    "callbacks passed as arguments",
    "dynamic dispatch",
    "macro-generated calls",
    "eval and reflection",
];

impl Reader<'_> {
    /// The answer to `op`, or `None` when this reader cannot give it (not a
    /// read op, or no graph is published): the writer then answers in order.
    pub(crate) fn answer(&self, op: &Op) -> Option<Response> {
        let outcome = match op {
            Op::Search {
                pattern,
                limit,
                offset,
                paths,
                globs,
                types,
                ..
            } => self.search(pattern, *limit, *offset, paths.as_deref(), globs, types),
            Op::Symbol { name, .. } => self.graph(|store| symbol(store, name)),
            Op::Callers {
                uid_or_name,
                role,
                offset,
                ..
            } => self.graph(|store| callers(store, uid_or_name, role, offset.unwrap_or(0))),
            Op::Impact {
                uid_or_name,
                direction,
                depth,
                ..
            } => self.graph(|store| impact_of(store, uid_or_name, direction, *depth)),
            Op::Changes {
                base,
                offset,
                include_tests,
            } => self.graph(|store| {
                changed(
                    store,
                    self.root,
                    base.as_deref(),
                    offset.unwrap_or(0),
                    *include_tests,
                )
            }),
            Op::Map { markdown } => self.graph(|store| map(store, *markdown)),
            Op::Meaning { query, limit, .. } => {
                let result =
                    self.shared
                        .meaning
                        .request(self.shared, query, *limit, self.serving());
                let value = serde_json::to_value(&result).unwrap_or(Value::Null);
                return Some(self.success(op, value, Basis::Index));
            }
            Op::History {
                query,
                facet,
                limit,
                read_only: true,
            } => history_read_only(self.root, query, facet.as_deref(), *limit),
            Op::Brief {
                prompt,
                session,
                host,
                parts,
                without,
            } => {
                let behind = self.behind.is_some() || !self.published.graph_fresh;
                let value = brief::run(
                    self.shared,
                    behind,
                    &Ask {
                        prompt,
                        session,
                        host,
                        parts: *parts,
                        without,
                    },
                );
                return Some(self.success(op, value, Basis::Graph));
            }
            _ => return None,
        };
        match outcome {
            Outcome::NoGraph => None,
            Outcome::Done(value, basis) => Some(self.success(op, value, basis)),
            Outcome::Fail(message) => Some(failure_response(op.op_name(), message)),
            Outcome::Ambiguous(message, candidates) => Some(Envelope::failure(
                op.op_name(),
                PixelError::new(ErrorCode::Ambiguous, message).with_ambiguity(
                    json!({"candidates": candidates.iter().map(symbol_json).collect::<Vec<_>>()}),
                ),
            )),
        }
    }

    /// What this reader serves, for the semantic pool.
    pub(crate) fn serving(&self) -> crate::meaning::Serving {
        crate::meaning::Serving {
            generation: self.published.generation,
            graph_fresh: self.published.graph_fresh,
        }
    }

    fn graph(&self, read: impl FnOnce(&Store) -> Result<Outcome, StoreError>) -> Outcome {
        if !self.published.graph_fresh && self.behind.is_none() {
            return Outcome::NoGraph;
        }
        match Store::open(self.root) {
            Ok(store) => match read(&store) {
                Ok(outcome) => outcome,
                Err(err) => Outcome::Fail(err.to_string()),
            },
            Err(StoreError::NoIndex) => Outcome::NoGraph,
            Err(err) => Outcome::Fail(err.to_string()),
        }
    }

    fn search(
        &self,
        pattern: &str,
        limit: Option<usize>,
        offset: Option<usize>,
        paths: Option<&[String]>,
        globs: &[String],
        types: &[String],
    ) -> Outcome {
        let filter = match PathFilter::new(globs, types) {
            Ok(filter) => filter,
            Err(message) => return Outcome::Fail(message),
        };
        let limit = limit.unwrap_or(SEARCH_DEFAULT_ROWS).min(SEARCH_MAX_ROWS);
        let offset = offset.unwrap_or(0);
        let paths = paths.filter(|paths| !paths.is_empty());
        let index = self.index.read().unwrap_or_else(PoisonError::into_inner);
        match index.search_page_filtered(pattern, offset, Some(limit), paths, filter.as_ref()) {
            Ok((matches, stats)) => {
                let rows: Vec<Value> = matches
                    .iter()
                    .map(|m| json!({"path": m.path, "line": m.line_number, "text": m.line}))
                    .collect();
                Outcome::Done(
                    json!({
                        "matches": rows,
                        "offset": offset,
                        "limit": limit,
                        "truncated": stats.truncated,
                        "next_offset": stats.truncated.then_some(offset + matches.len()),
                        "stats": {
                            "candidates": stats.candidates,
                            "scanned_all": stats.scanned_all,
                            "elapsed_us": stats.elapsed_us,
                        },
                    }),
                    Basis::Index,
                )
            }
            Err(err) => Outcome::Fail(err.to_string()),
        }
    }

    /// The success envelope: the result, the repository snapshot, and the
    /// epistemics of what produced it.
    fn success(&self, op: &Op, value: Value, basis: Basis) -> Response {
        let mut epistemics = Epistemics {
            closed_world: false,
            lower_bound: true,
            basis: basis.name().to_owned(),
            staleness_ms: None,
            confidence: None,
            extraction_limits: Vec::new(),
        };
        if basis == Basis::Graph {
            epistemics.extraction_limits = EXTRACTION_LIMITS.map(str::to_owned).to_vec();
        }
        let mut warnings = Vec::new();
        if let Some(behind) = self.behind {
            epistemics.staleness_ms = Some(behind.as_millis() as u64);
            warnings.push(Warning {
                code: "STALE_GENERATION".to_owned(),
                message: format!(
                    "answered from the generation before an edit batch that has waited {} ms",
                    behind.as_millis()
                ),
            });
        }
        Envelope::success(op.op_name(), value)
            .with_snapshot(self.published.snapshot.clone().compact())
            .with_epistemics(epistemics)
            .with_warnings(warnings)
    }
}

pub(crate) fn symbol_json(symbol: &Symbol) -> Value {
    json!({
        "uid": symbol.uid,
        "name": symbol.name,
        "kind": symbol.kind.to_string(),
        "file": symbol.file,
        "line": symbol.line,
        "end_line": symbol.end_line,
        "signature": symbol.signature,
    })
}

fn item_json(item: &Item) -> Value {
    let mut value = symbol_json(&item.symbol);
    value["tier"] = json!(tier_name(item.tier));
    value
}

fn tier_name(tier: Tier) -> &'static str {
    match tier {
        Tier::Proved => "proved",
        Tier::Likely => "likely",
        Tier::Open => "open",
    }
}

/// The one symbol `token` names, or the outcome that says why not.
fn one(store: &Store, token: &str) -> Result<Result<Symbol, Outcome>, StoreError> {
    Ok(match resolve(store, token)? {
        Resolved::One(symbol) => Ok(symbol),
        Resolved::None => Err(Outcome::Fail(format!("no symbol named {token:?}"))),
        Resolved::Many(mut found) => {
            let total = found.len();
            found.truncate(SYMBOL_ROWS);
            Err(Outcome::Ambiguous(
                format!("{token:?} names {total} symbols; pass a uid"),
                found,
            ))
        }
    })
}

fn symbol(store: &Store, name: &str) -> Result<Outcome, StoreError> {
    let found = match resolve(store, name)? {
        Resolved::None => return Ok(Outcome::Fail(format!("no symbol named {name:?}"))),
        Resolved::One(symbol) => vec![symbol],
        Resolved::Many(found) => found,
    };
    let total = found.len();
    let rows: Vec<Value> = found.iter().take(SYMBOL_ROWS).map(symbol_json).collect();
    Ok(Outcome::Done(
        json!({"symbols": rows, "total": total, "truncated": total > SYMBOL_ROWS}),
        Basis::Graph,
    ))
}

fn callers(store: &Store, token: &str, role: &str, offset: usize) -> Result<Outcome, StoreError> {
    let target = match one(store, token)? {
        Ok(symbol) => symbol,
        Err(outcome) => return Ok(outcome),
    };
    let rows: Vec<Value> = match role {
        "callers" => store
            .callers(&target.uid, Tier::Likely)?
            .into_iter()
            .map(|caller| {
                let mut row = symbol_json(&caller.site);
                row["site_line"] = json!(caller.edge.line);
                row["tier"] = json!(tier_name(caller.edge.tier));
                row["edge"] = json!(caller.edge.kind.as_str());
                row
            })
            .collect(),
        "callees" => {
            let mut rows = Vec::new();
            for edge in store.callees(&target.uid)? {
                if let Some(callee) = edge
                    .dst
                    .as_deref()
                    .map(|uid| store.symbol(uid))
                    .transpose()?
                    .flatten()
                {
                    let mut row = symbol_json(&callee);
                    row["site_line"] = json!(edge.line);
                    row["tier"] = json!(tier_name(edge.tier));
                    row["edge"] = json!(edge.kind.as_str());
                    rows.push(row);
                }
            }
            rows
        }
        other => {
            return Ok(Outcome::Fail(format!(
                "role must be \"callers\" or \"callees\", not {other:?}"
            )));
        }
    };
    let total = rows.len();
    let page: Vec<Value> = rows.into_iter().skip(offset).take(PAGE_ROWS).collect();
    let open_sites = if role == "callers" {
        store
            .open_sites(leaf(&target.name))?
            .iter()
            .filter(|edge| edge.src != target.uid)
            .count()
    } else {
        0
    };
    Ok(Outcome::Done(
        json!({
            "target": symbol_json(&target),
            "role": role,
            "items": page,
            "total": total,
            "offset": offset,
            "next_offset": (offset + PAGE_ROWS < total).then_some(offset + PAGE_ROWS),
            "open_sites": open_sites,
        }),
        Basis::Graph,
    ))
}

fn impact_of(
    store: &Store,
    token: &str,
    direction: &str,
    depth: Option<u32>,
) -> Result<Outcome, StoreError> {
    let Some(direction) = Direction::parse(direction) else {
        return Ok(Outcome::Fail(format!(
            "direction must be \"upstream\" or \"downstream\", not {direction:?}"
        )));
    };
    let target = match one(store, token)? {
        Ok(symbol) => symbol,
        Err(outcome) => return Ok(outcome),
    };
    let options = Options {
        max_depth: depth.map_or(Options::default().max_depth, |d| d as usize),
        ..Options::default()
    };
    let report = impact(store, &target, direction, &options)?;
    let tests = if direction == Direction::Upstream {
        pixel_graph::edit::covering_tests(store, &target)?
    } else {
        Vec::new()
    };
    Ok(Outcome::Done(impact_json(&report, &tests), Basis::Graph))
}

fn impact_json(report: &Impact, tests: &[String]) -> Value {
    json!({
        "target": symbol_json(&report.target),
        "direction": report.direction.as_str(),
        "risk": report.risk.map(Risk::as_str),
        "depths": report.depths.iter().map(|level| level.iter().map(item_json).collect::<Vec<_>>()).collect::<Vec<_>>(),
        "counts": report.counts,
        "affected_files": report.affected_files,
        "referenced_by": report.referenced_by.iter().map(item_json).collect::<Vec<_>>(),
        "referenced_by_total": report.referenced_by_total,
        "open_sites": report.open_sites,
        "caps": report.caps,
        "tests": tests,
    })
}

fn changed(
    store: &Store,
    root: &Path,
    base: Option<&str>,
    offset: usize,
    include_tests: bool,
) -> Result<Outcome, StoreError> {
    let diff = match base {
        Some(base) => tree::diff_against(root, base),
        None => tree::diff(root),
    };
    let diff = match diff {
        Ok(diff) => diff,
        Err(err) => return Ok(Outcome::Fail(err.to_string())),
    };
    let found = changes(store, root, &diff)?;
    let total = found.symbols.len();
    let rows: Vec<Value> = found
        .symbols
        .iter()
        .skip(offset)
        .take(PAGE_ROWS)
        .map(|changed| {
            let mut row = symbol_json(&changed.symbol);
            row["signature_changed"] = json!(changed.signature_changed);
            if include_tests {
                row["tests"] = json!(changed.tests);
            }
            row
        })
        .collect();
    Ok(Outcome::Done(
        json!({
            "base": base,
            "files": diff.files.len(),
            "symbols": rows,
            "total": total,
            "offset": offset,
            "next_offset": (offset + PAGE_ROWS < total).then_some(offset + PAGE_ROWS),
            "unindexed": found.unindexed.iter().map(|u| u.path.as_str()).collect::<Vec<_>>(),
        }),
        Basis::Graph,
    ))
}

fn map(store: &Store, markdown: bool) -> Result<Outcome, StoreError> {
    let files = store.file_rows()?;
    let total = files.len();
    let mut rows = Vec::new();
    let mut text = String::new();
    for file in files.iter().take(MAP_FILE_CAP) {
        let symbols: Vec<Symbol> = store
            .symbols_in(&file.path)?
            .into_iter()
            .filter(|s| !matches!(s.kind, SymbolKind::File | SymbolKind::Package))
            .collect();
        if markdown {
            text.push_str(&format!("## {}\n", file.path));
            for symbol in &symbols {
                text.push_str(&format!(
                    "- {} `{}` (line {})\n",
                    symbol.kind, symbol.name, symbol.line
                ));
            }
            text.push('\n');
        }
        rows.push(json!({
            "path": file.path,
            "lang": file.lang.as_str(),
            "symbols": symbols.iter().map(|s| json!({"name": s.name, "kind": s.kind.to_string(), "line": s.line})).collect::<Vec<_>>(),
        }));
    }
    let mut value = json!({
        "files": rows,
        "total": total,
        "truncated": total > MAP_FILE_CAP,
    });
    if markdown {
        value["markdown"] = json!(text);
    }
    Ok(Outcome::Done(value, Basis::Graph))
}

/// A history query that never ingests and never creates the db.
fn history_read_only(
    root: &Path,
    query: &str,
    facet: Option<&str>,
    limit: Option<usize>,
) -> Outcome {
    match FactsStore::open_existing(root) {
        Ok(Some(store)) => history_result(&store, query, facet, limit),
        Ok(None) => Outcome::Done(
            json!({"available": false, "reason": "no history db: the first history query builds it"}),
            Basis::History,
        ),
        Err(err) => Outcome::Fail(err.to_string()),
    }
}

/// The history rows for a query over an open store.
pub(crate) fn history_result(
    store: &FactsStore,
    query: &str,
    facet: Option<&str>,
    limit: Option<usize>,
) -> Outcome {
    let facet = facet.map_or(SearchFacet::All, SearchFacet::from);
    match search(store, query, facet, limit.unwrap_or(HISTORY_DEFAULT_ROWS)) {
        Ok(result) => match serde_json::to_value(&result) {
            Ok(mut value) => {
                value["available"] = json!(true);
                Outcome::Done(value, Basis::History)
            }
            Err(err) => Outcome::Fail(err.to_string()),
        },
        Err(err) => Outcome::Fail(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_symbol_and_an_edge_kind_have_stable_wire_names() {
        assert_eq!(tier_name(Tier::Proved), "proved");
        assert_eq!(tier_name(Tier::Likely), "likely");
        assert_eq!(tier_name(Tier::Open), "open");
    }
}
