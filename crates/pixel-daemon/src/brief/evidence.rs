// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The evidence of one brief, read from the stores the daemon holds
//! (`specs/brief.md`): the text index in memory, the published graph through a
//! read-only [`Store`] opened once, the history db as it stands, the work
//! tree's files. Nothing here builds, refreshes or starts anything, and every
//! read gives up when its `deadline` has passed.
//!
//! The brief reads like a `stale_ok` caller: when the graph lags the working
//! tree (a watcher batch has waited, or the writer has not yet published the
//! last one) the evidence is still used and the brief says it may predate the
//! last edit.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use pixel_brief::chain::{
    CallerHit, Evidence, Flow, Found, HistoryHit, MeaningHit, PathHop, RelevanceAnswer, RichFound,
    RichHit, SEARCH_ROWS, Staleness, StatusProbe, SymbolHit,
};
use pixel_brief::kinds::{ChangeScope, Changes, FlowNode, RepoMap};
use pixel_brief::words::{self, FileRow, SymbolRow};
use pixel_graph::analysis::concept;
use pixel_graph::analysis::impact::resolve_target;
use pixel_graph::analysis::trace::trace;
use pixel_graph::store::{Store, StoreError};
use pixel_index::history::FactsStore;
use pixel_index::history::search::{SearchFacet, search};
use pixel_io::{GitOptions, GitRunner};
use pixel_model::{Symbol, SymbolKind, Tier, credential_path, is_test_path};

use crate::service::Shared;

use super::graph_reads;
use super::relevance::{is_french, relevance_answer, relevance_on};

/// The longest call path searched, in calls.
const FLOW_DEPTH: usize = 8;
/// The git commands `Evidence::git_read` runs.
const READ_ONLY_GIT: &[&str] = &["log", "show", "rev-list", "tag"];
/// Symbols a name lookup reads.
const SYMBOL_ROWS: usize = 50;
/// Characters of a source line a bounded read keeps.
const LINE_CHARS: usize = 200;
/// Characters of the one-line "what is it" read.
const HEAD_CHARS: usize = 140;
/// Rows a concept search reads from the text index before ranking them.
const CONCEPT_ROWS: usize = 200;
/// Leads a concept search keeps.
const CONCEPT_LEADS: usize = 20;

/// The evidence source of one brief.
pub(crate) struct BriefEvidence {
    shared: Arc<Shared>,
    /// The graph lags the working tree: the evidence may predate the last
    /// edit, and a successful read says so.
    behind: bool,
    staleness: Staleness,
    /// The graph, opened read-only on first use; the failure is kept so the
    /// next operation does not repeat the open.
    graph: Mutex<Option<Result<Store, String>>>,
}

impl BriefEvidence {
    pub(crate) fn new(shared: Arc<Shared>, behind: bool) -> Self {
        Self {
            shared,
            behind,
            staleness: Staleness::default(),
            graph: Mutex::new(None),
        }
    }

    fn root(&self) -> &Path {
        &self.shared.root
    }

    fn fresh_enough(&self, deadline: Instant) -> Result<(), String> {
        if Instant::now() >= deadline {
            return Err("out of time".to_owned());
        }
        Ok(())
    }

    /// Run `read` on the graph.
    fn with_graph<T>(&self, read: impl FnOnce(&Store) -> Result<T, String>) -> Result<T, String> {
        let mut slot = self.graph.lock().unwrap_or_else(PoisonError::into_inner);
        let opened = slot.get_or_insert_with(|| match Store::open(self.root()) {
            Ok(store) => Ok(store),
            Err(StoreError::NoIndex) => Err("the code graph is not built yet".to_owned()),
            Err(error) => Err(error.to_string()),
        });
        match opened {
            Ok(store) => {
                let result = read(store);
                if result.is_ok() && self.behind {
                    self.staleness.mark();
                }
                result
            }
            Err(reason) => Err(reason.clone()),
        }
    }

    /// The symbols of the graph, when there is one.
    fn graph_symbols(&self) -> Option<Vec<Symbol>> {
        self.with_graph(graph_reads::declarations).ok()
    }

    fn text(&self, path: &str) -> Result<String, String> {
        if credential_path(Path::new(path)) {
            return Err("credential-shaped path".to_owned());
        }
        std::fs::read_to_string(self.root().join(path)).map_err(|error| error.to_string())
    }
}

fn sql(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn one_symbol(store: &Store, token: &str) -> Result<Symbol, String> {
    if token.contains('#') {
        return store
            .symbol(token)
            .map_err(sql)?
            .ok_or_else(|| format!("no symbol with uid '{token}'"));
    }
    let mut found = resolve_target(store, token).map_err(sql)?;
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(format!("no symbol named '{token}'")),
        _ => Err("the name is ambiguous, a uid is needed".to_owned()),
    }
}

/// The callers of `symbol` at depth one: once per file and symbol, the
/// earliest site of each, `Likely` ones included.
fn direct_callers(store: &Store, symbol: &Symbol) -> Result<Vec<CallerHit>, String> {
    let mut out: Vec<CallerHit> = Vec::new();
    for caller in store.callers(&symbol.uid, Tier::Likely).map_err(sql)? {
        if caller.site.uid == symbol.uid {
            continue;
        }
        let (path, via) = (caller.site.file, caller.site.name);
        if out.iter().any(|hit| hit.path == path && hit.via == via) {
            continue;
        }
        out.push(CallerHit {
            path,
            via,
            line: u64::from(caller.edge.line),
        });
    }
    Ok(out)
}

impl Evidence for BriefEvidence {
    fn staleness(&self) -> Staleness {
        self.staleness.clone()
    }

    fn route(&self) -> &'static str {
        "daemon"
    }

    fn files_with(&self, anchor: &str, deadline: Instant) -> Result<Found, String> {
        self.files_matching(anchor, deadline).map(Found::from)
    }

    fn files_matching(&self, anchor: &str, deadline: Instant) -> Result<RichFound, String> {
        self.grep(&regex::escape(anchor), SEARCH_ROWS, deadline)
    }

    fn grep(&self, pattern: &str, limit: usize, deadline: Instant) -> Result<RichFound, String> {
        self.fresh_enough(deadline)?;
        let index = self
            .shared
            .index
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let (rows, stats) = index
            .search_page_filtered(pattern, 0, Some(limit), None, None)
            .map_err(sql)?;
        if self.behind {
            self.staleness.mark();
        }
        Ok(RichFound {
            hits: rows
                .into_iter()
                .filter(|row| !credential_path(Path::new(&row.path)))
                .map(|row| RichHit {
                    path: row.path,
                    line: row.line_number,
                    text: (!row.line.is_empty()).then_some(row.line),
                })
                .collect(),
            capped: stats.truncated,
        })
    }

    fn concept(&self, phrase: &str, deadline: Instant) -> Result<Found, String> {
        self.concept_matching(phrase, deadline).map(Found::from)
    }

    fn concept_matching(&self, phrase: &str, deadline: Instant) -> Result<RichFound, String> {
        let words = concept::words(phrase);
        let pattern = concept::pattern(&words).ok_or_else(|| "no words to look for".to_owned())?;
        let found = self.grep(&pattern, CONCEPT_ROWS, deadline)?;
        let leads = concept::rank(
            phrase,
            found.hits.into_iter().filter_map(|hit| {
                let text = hit.text?;
                Some((hit.path, u32::try_from(hit.line).ok()?, text))
            }),
            CONCEPT_LEADS,
        );
        Ok(RichFound {
            hits: leads
                .into_iter()
                .map(|lead| RichHit {
                    path: lead.path,
                    line: u64::from(lead.line),
                    text: Some(lead.text),
                })
                .collect(),
            capped: found.capped,
        })
    }

    fn symbols(&self, name: &str, deadline: Instant) -> Result<Vec<SymbolHit>, String> {
        self.fresh_enough(deadline)?;
        self.with_graph(|store| {
            Ok(resolve_target(store, name)
                .map_err(sql)?
                .iter()
                .take(SYMBOL_ROWS)
                .map(graph_reads::hit)
                .collect())
        })
    }

    fn callers(&self, target: &str, deadline: Instant) -> Result<Vec<CallerHit>, String> {
        self.fresh_enough(deadline)?;
        self.with_graph(|store| {
            let symbol = one_symbol(store, target)?;
            direct_callers(store, &symbol)
        })
    }

    /// The picked definition's body from the file itself, bounded to the
    /// token budget's four characters each.
    fn context(
        &self,
        hit: &SymbolHit,
        budget_tokens: usize,
        deadline: Instant,
    ) -> Result<(String, Vec<String>), String> {
        self.fresh_enough(deadline)?;
        let text = self.text(&hit.path)?;
        let max_chars = budget_tokens.saturating_mul(4);
        let start = usize::try_from(hit.start_line)
            .unwrap_or(0)
            .saturating_sub(1);
        let rows = usize::try_from(hit.end_line.max(hit.start_line))
            .unwrap_or(0)
            .saturating_sub(start);
        let mut body = String::new();
        for line in text.lines().skip(start).take(rows.max(1)) {
            if body.len() + line.len() + 1 > max_chars {
                break;
            }
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(line);
        }
        if body.is_empty() {
            return Err("empty definition".to_owned());
        }
        Ok((body, Vec::new()))
    }

    fn test_files(
        &self,
        uid: &str,
        deadline: Instant,
    ) -> Result<(Vec<String>, Vec<String>), String> {
        self.fresh_enough(deadline)?;
        self.with_graph(|store| {
            let symbol = one_symbol(store, uid)?;
            let mut files: Vec<String> = direct_callers(store, &symbol)?
                .into_iter()
                .map(|hit| hit.path)
                .filter(|path| is_test_path(path))
                .collect();
            files.sort();
            files.dedup();
            Ok((files, Vec::new()))
        })
    }

    fn flow(
        &self,
        from: &str,
        to: &str,
        _budget_ms: u64,
        deadline: Instant,
    ) -> Result<Flow, String> {
        self.fresh_enough(deadline)?;
        self.with_graph(|store| {
            let start = one_symbol(store, from)?;
            let end = one_symbol(store, to)?;
            let Some(hops) = trace(store, &start, &end, FLOW_DEPTH).map_err(sql)? else {
                return Ok(Flow::Absent);
            };
            let mut names = vec![start.name.clone()];
            names.extend(hops.iter().map(|hop| hop.symbol.name.clone()));
            let notes = hops
                .iter()
                .any(|hop| hop.tier == Tier::Likely)
                .then(|| "path crosses a name-matched (Likely) call".to_owned())
                .into_iter()
                .collect();
            Ok(Flow::Path { hops: names, notes })
        })
    }

    fn call_path(
        &self,
        from: &str,
        to: &str,
        deadline: Instant,
    ) -> Result<Option<Vec<PathHop>>, String> {
        self.fresh_enough(deadline)?;
        self.with_graph(|store| {
            let start = one_symbol(store, from)?;
            let end = one_symbol(store, to)?;
            let Some(hops) = trace(store, &start, &end, FLOW_DEPTH).map_err(sql)? else {
                return Ok(None);
            };
            let mut path = vec![PathHop {
                symbol: graph_reads::hit(&start),
                call_line: 0,
                likely: false,
            }];
            path.extend(hops.iter().map(|hop| PathHop {
                symbol: graph_reads::hit(&hop.symbol),
                call_line: u64::from(hop.line),
                likely: hop.tier == Tier::Likely,
            }));
            Ok(Some(path))
        })
    }

    fn git_read(
        &self,
        args: &[&str],
        max_bytes: usize,
        deadline: Instant,
    ) -> Result<String, String> {
        if !args
            .first()
            .is_some_and(|command| READ_ONLY_GIT.contains(command))
        {
            return Err("only log, show, rev-list and tag are read".to_owned());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("out of time".to_owned());
        }
        let git = GitRunner::with_options(
            self.root(),
            GitOptions {
                timeout: Some(left),
                max_output_bytes: Some(max_bytes),
            },
        );
        let out = git.run(args).map_err(|error| error.to_string())?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    fn repo_map(&self, deadline: Instant) -> Result<RepoMap, String> {
        self.fresh_enough(deadline)?;
        self.with_graph(graph_reads::read_repo_map)
    }

    fn changes(&self, scope: ChangeScope, deadline: Instant) -> Result<Changes, String> {
        pixel_brief::git_changes::changes(self.root(), scope, deadline)
    }

    fn find_symbols(
        &self,
        terms: &[String],
        limit: usize,
        deadline: Instant,
    ) -> Result<Vec<SymbolHit>, String> {
        self.fresh_enough(deadline)?;
        self.with_graph(|store| graph_reads::read_symbols_like(store, terms, limit))
    }

    fn flow_tree(
        &self,
        uid: &str,
        depth: usize,
        limit: usize,
        deadline: Instant,
    ) -> Result<Vec<FlowNode>, String> {
        self.fresh_enough(deadline)?;
        self.with_graph(|store| graph_reads::read_callee_tree(store, uid, depth, limit))
    }

    fn skeleton(&self, file: &str, deadline: Instant) -> Result<Vec<SymbolHit>, String> {
        self.fresh_enough(deadline)?;
        self.with_graph(|store| {
            let mut symbols = store.symbols_in(file).map_err(sql)?;
            symbols.retain(|symbol| !matches!(symbol.kind, SymbolKind::File | SymbolKind::Package));
            if symbols.is_empty()
                && !store
                    .file_rows()
                    .map_err(sql)?
                    .iter()
                    .any(|row| row.path == file)
            {
                return Err(format!("no indexed file matching '{file}'"));
            }
            Ok(symbols.iter().map(graph_reads::hit).collect())
        })
    }

    fn status(&self, deadline: Instant) -> Result<StatusProbe, String> {
        self.fresh_enough(deadline)?;
        let facts_fresh = match FactsStore::open_existing(self.root()) {
            Ok(Some(store)) => Some(store.index_state().fresh),
            Ok(None) => Some(false),
            Err(error) => return Err(error.to_string()),
        };
        Ok(StatusProbe { facts_fresh })
    }

    /// Read only: an absent history db is an error, never a reason to write.
    fn history(
        &self,
        phrase: &str,
        limit: usize,
        deadline: Instant,
    ) -> Result<(Vec<HistoryHit>, Vec<String>), String> {
        self.fresh_enough(deadline)?;
        let store = FactsStore::open_existing(self.root())
            .map_err(sql)?
            .ok_or_else(|| "no history db: the first history query builds it".to_owned())?;
        let result = search(&store, phrase, SearchFacet::All, limit).map_err(sql)?;
        let hits = result
            .candidates
            .into_iter()
            .map(|hit| HistoryHit {
                sha: hit.oid.chars().take(10).collect(),
                subject: hit.subject.lines().next().unwrap_or("").trim().to_owned(),
                at: hit.at,
                path: hit.path.filter(|path| !path.is_empty()),
                snippet: hit.snippet.filter(|snippet| !snippet.is_empty()),
            })
            .collect();
        Ok((hits, Vec::new()))
    }

    fn relevance(&self, typed: &str, deadline: Instant) -> Result<RelevanceAnswer, String> {
        self.fresh_enough(deadline)?;
        let symbols = self.graph_symbols();
        let block = {
            let index = self
                .shared
                .index
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            relevance_on(&index, symbols.as_deref(), typed)?
        };
        if self.behind {
            self.staleness.mark();
        }
        Ok(relevance_answer(&block, is_french(typed)))
    }

    fn meaning(
        &self,
        query: &str,
        limit: usize,
        deadline: Instant,
    ) -> Result<Vec<MeaningHit>, String> {
        let published = self
            .shared
            .published
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let at = crate::meaning::Serving {
            generation: published.generation,
            graph_fresh: published.graph_fresh,
        };
        drop(published);
        let hits = crate::meaning::leads(&self.shared, query, limit, at, deadline)?;
        Ok(hits
            .into_iter()
            .map(|hit| MeaningHit {
                path: hit.path,
                start_line: hit.start_line,
                end_line: hit.end_line,
                symbol: hit.symbol,
                score: hit.score,
                snippet: hit.snippet,
            })
            .collect())
    }

    fn word_leads(
        &self,
        query: &str,
        limit: usize,
        deadline: Instant,
    ) -> Result<Vec<MeaningHit>, String> {
        self.fresh_enough(deadline)?;
        let files = self.with_graph(|store| {
            let rows = store.file_rows().map_err(sql)?;
            let mut files: Vec<FileRow> = rows
                .iter()
                .map(|row| FileRow {
                    path: row.path.clone(),
                    symbols: Vec::new(),
                })
                .collect();
            let at: std::collections::HashMap<&str, usize> = rows
                .iter()
                .enumerate()
                .map(|(i, row)| (row.path.as_str(), i))
                .collect();
            for symbol in graph_reads::declarations(store)?
                .into_iter()
                .take(words::MAX_SYMBOLS)
            {
                if let Some(&i) = at.get(symbol.file.as_str()) {
                    files[i].symbols.push(SymbolRow {
                        name: symbol.name,
                        signature: symbol.signature,
                        start_line: symbol.line,
                        end_line: symbol.end_line,
                    });
                }
            }
            Ok(files)
        })?;
        words::leads(&files, query, limit, deadline)
    }

    fn lines_at(
        &self,
        path: &str,
        start: u64,
        end: u64,
        deadline: Instant,
    ) -> Result<Vec<String>, String> {
        self.fresh_enough(deadline)?;
        let text = self.text(path)?;
        let skip = usize::try_from(start.saturating_sub(1)).unwrap_or(0);
        let take = usize::try_from(end.saturating_sub(start) + 1).unwrap_or(0);
        Ok(text
            .lines()
            .skip(skip)
            .take(take)
            .map(|line| line.chars().take(LINE_CHARS).collect())
            .collect())
    }

    fn tracked_files(&self, deadline: Instant) -> Result<Vec<String>, String> {
        self.fresh_enough(deadline)?;
        let mut files = pixel_io::GitRunner::new(self.root())
            .ls_files_or_err()
            .map_err(|error| error.to_string())?;
        files.retain(|file| !credential_path(Path::new(file)));
        files.sort_unstable();
        Ok(files)
    }

    fn read_file(&self, path: &str, max_bytes: u64, deadline: Instant) -> Result<String, String> {
        self.fresh_enough(deadline)?;
        if credential_path(Path::new(path)) {
            return Err("credential-shaped path".to_owned());
        }
        let file = pixel_io::repo_path::confine(self.root(), path).map_err(|e| e.to_string())?;
        pixel_io::nofollow::read_text_capped(&file, max_bytes).map_err(|e| e.to_string())
    }

    fn line_at(&self, path: &str, line: u64, deadline: Instant) -> Result<String, String> {
        self.fresh_enough(deadline)?;
        let text = self.text(path)?;
        // One line is the smallest read that answers "what is it": a header
        // like `export const X = defineMultiStyleConfig(` carries the
        // right-hand side on the line the index recorded.
        let line = text
            .lines()
            .nth(usize::try_from(line.saturating_sub(1)).unwrap_or(0))
            .ok_or_else(|| "line out of range".to_owned())?
            .trim();
        if line.is_empty() {
            return Err("empty line".to_owned());
        }
        let mut head: String = line.chars().take(HEAD_CHARS).collect();
        if line.chars().count() > HEAD_CHARS {
            head.push('…');
        }
        Ok(head)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pixel_brief::chain::Evidence;

    use super::*;
    use crate::Service;
    use crate::testutil::{open, repo};

    const FILES: &[(&str, &str)] = &[
        (
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        ),
        ("src/lib.rs", "pub mod a;\npub mod b;\n"),
        (
            "src/a.rs",
            "pub fn run(x: u32) -> u32 {\n    x\n}\n\npub fn helper() -> u32 {\n    run(2)\n}\n",
        ),
        (
            "src/b.rs",
            "use crate::a::run;\n\npub fn go() -> u32 {\n    run(1)\n}\n",
        ),
        (
            "tests/it.rs",
            "use demo::b::go;\n\nfn t() {\n    go();\n}\n",
        ),
        (".env", "TOKEN=hunter2\n"),
    ];

    /// An evidence source over a built fixture; `behind` as the replica sets it.
    fn evidence(behind: bool) -> (tempfile::TempDir, BriefEvidence) {
        let (dir, root) = repo(FILES);
        let _ = open(&root);
        let service = Service::open(&root).expect("the service opens");
        (dir, BriefEvidence::new(service.shared(), behind))
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    #[test]
    fn a_name_resolves_to_its_declarations_and_a_miss_is_an_empty_list() {
        let (_dir, evidence) = evidence(false);
        let found = evidence.symbols("run", deadline()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(
            (
                found[0].uid.as_str(),
                found[0].path.as_str(),
                found[0].start_line,
                found[0].end_line
            ),
            ("src/a.rs#run#function", "src/a.rs", 1, 3)
        );
        assert!(evidence.symbols("nobody", deadline()).unwrap().is_empty());
    }

    #[test]
    fn callers_are_the_direct_sites_once_per_file_and_symbol() {
        let (_dir, evidence) = evidence(false);
        let callers = evidence.callers("run", deadline()).unwrap();
        let rows: Vec<(&str, &str, u64)> = callers
            .iter()
            .map(|hit| (hit.path.as_str(), hit.via.as_str(), hit.line))
            .collect();
        assert_eq!(rows, [("src/a.rs", "helper", 6), ("src/b.rs", "go", 4)]);
        assert!(
            evidence
                .callers("nobody", deadline())
                .unwrap_err()
                .contains("no symbol named")
        );
    }

    #[test]
    fn the_tests_that_call_a_symbol_are_the_test_files_among_its_callers() {
        let (_dir, evidence) = evidence(false);
        let (files, caps) = evidence
            .test_files("src/b.rs#go#function", deadline())
            .unwrap();
        assert_eq!(files, ["tests/it.rs"]);
        assert!(caps.is_empty());
        let (none, _) = evidence
            .test_files("src/a.rs#helper#function", deadline())
            .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn a_flow_is_the_chain_of_resolved_calls_and_absent_when_there_is_none() {
        let (_dir, evidence) = evidence(false);
        let path = evidence.flow("go", "run", 100, deadline()).unwrap();
        assert_eq!(
            path,
            Flow::Path {
                hops: vec!["go".into(), "run".into()],
                notes: Vec::new()
            }
        );
        assert_eq!(
            evidence.flow("run", "go", 100, deadline()).unwrap(),
            Flow::Absent
        );
        assert!(evidence.flow("nobody", "go", 100, deadline()).is_err());
    }

    #[test]
    fn a_skeleton_lists_a_files_declarations_and_refuses_an_unindexed_file() {
        let (_dir, evidence) = evidence(false);
        let skeleton = evidence.skeleton("src/a.rs", deadline()).unwrap();
        let names: Vec<&str> = skeleton.iter().map(|hit| hit.name.as_str()).collect();
        assert_eq!(names, ["run", "helper"]);
        assert!(
            evidence
                .skeleton("src/none.rs", deadline())
                .unwrap_err()
                .contains("no indexed file")
        );
    }

    #[test]
    fn the_map_the_symbol_search_and_the_call_tree_read_the_same_generation() {
        let (_dir, evidence) = evidence(false);
        let map = evidence.repo_map(deadline()).unwrap();
        assert!(
            map.files
                .iter()
                .any(|file| file.path == "src/a.rs" && file.symbols == 2)
        );
        assert!(
            map.tops
                .iter()
                .any(|top| top.name == "run" && top.path == "src/a.rs")
        );
        let like = evidence
            .find_symbols(&["HELP".to_owned()], 5, deadline())
            .unwrap();
        assert_eq!(like.len(), 1);
        assert_eq!(like[0].name, "helper");
        let tree = evidence
            .flow_tree("src/a.rs#helper#function", 2, 10, deadline())
            .unwrap();
        let names: Vec<&str> = tree.iter().map(|node| node.name.as_str()).collect();
        assert_eq!(names, ["helper", "run"]);
    }

    #[test]
    fn a_search_returns_the_matching_line_and_never_a_credential_file() {
        let (_dir, evidence) = evidence(false);
        let found = evidence.grep("fn run", 10, deadline()).unwrap();
        assert_eq!(found.hits.len(), 1);
        assert_eq!(
            (found.hits[0].path.as_str(), found.hits[0].line),
            ("src/a.rs", 1)
        );
        assert_eq!(
            found.hits[0].text.as_deref(),
            Some("pub fn run(x: u32) -> u32 {")
        );
        assert!(
            evidence
                .grep("hunter2", 10, deadline())
                .unwrap()
                .hits
                .is_empty()
        );
        assert!(evidence.lines_at(".env", 1, 1, deadline()).is_err());
        assert!(evidence.line_at(".env", 1, deadline()).is_err());
    }

    #[test]
    fn source_reads_are_bounded_lines_and_a_trimmed_head() {
        let (_dir, evidence) = evidence(false);
        assert_eq!(
            evidence.lines_at("src/a.rs", 5, 6, deadline()).unwrap(),
            ["pub fn helper() -> u32 {", "    run(2)"]
        );
        assert_eq!(
            evidence.line_at("src/a.rs", 6, deadline()).unwrap(),
            "run(2)"
        );
        assert_eq!(
            evidence.line_at("src/a.rs", 4, deadline()).unwrap_err(),
            "empty line"
        );
        assert_eq!(
            evidence.line_at("src/a.rs", 99, deadline()).unwrap_err(),
            "line out of range"
        );
        let hit = SymbolHit {
            uid: "src/a.rs#run#function".into(),
            name: "run".into(),
            kind: "function".into(),
            path: "src/a.rs".into(),
            start_line: 1,
            end_line: 3,
        };
        let (body, _) = evidence.context(&hit, 400, deadline()).unwrap();
        assert_eq!(body, "pub fn run(x: u32) -> u32 {\n    x\n}");
        let (cut, _) = evidence.context(&hit, 8, deadline()).unwrap();
        assert_eq!(cut, "pub fn run(x: u32) -> u32 {", "cut at a line");
        assert_eq!(
            evidence.context(&hit, 1, deadline()).unwrap_err(),
            "empty definition"
        );
    }

    #[test]
    fn a_read_after_the_deadline_gives_up_before_touching_a_store() {
        let (_dir, evidence) = evidence(false);
        let past = Instant::now() - Duration::from_millis(1);
        assert_eq!(evidence.symbols("run", past).unwrap_err(), "out of time");
        assert_eq!(evidence.grep("run", 1, past).unwrap_err(), "out of time");
        assert_eq!(
            evidence.line_at("src/a.rs", 1, past).unwrap_err(),
            "out of time"
        );
        assert_eq!(evidence.repo_map(past).unwrap_err(), "out of time");
    }

    #[test]
    fn history_and_status_read_the_db_as_it_stands() {
        let (_dir, evidence) = evidence(false);
        assert_eq!(
            evidence.status(deadline()).unwrap(),
            StatusProbe {
                facts_fresh: Some(false)
            },
            "no history db yet"
        );
        assert!(
            evidence
                .history("fixture", 3, deadline())
                .unwrap_err()
                .contains("no history db")
        );
    }

    #[test]
    fn the_meaning_search_says_it_is_not_built_without_a_model() {
        let (_dir, evidence) = evidence(false);
        assert!(evidence.meaning("login flow", 4, deadline()).is_err());
        assert_eq!(evidence.semantic_hint("login flow"), None);
        assert_eq!(evidence.route(), "daemon");
    }

    #[test]
    fn a_concept_phrase_finds_the_string_literal_before_the_code_that_mentions_it() {
        let (_dir, root) = repo(&[
            (
                "Cargo.toml",
                "[package]\nname = \"shop\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub mod cart;\n"),
            (
                "src/cart.rs",
                "pub fn add_to_cart(item: u32) -> u32 {\n    item\n}\n\npub fn banner() -> &'static str {\n    \"Add to cart failed\"\n}\n",
            ),
        ]);
        let _ = open(&root);
        let service = Service::open(&root).expect("the service opens");
        let evidence = BriefEvidence::new(service.shared(), false);
        let found = evidence
            .concept_matching("add to cart", deadline())
            .unwrap();
        let lines: Vec<(u64, &str)> = found
            .hits
            .iter()
            .map(|hit| (hit.line, hit.text.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(lines[0], (6, "\"Add to cart failed\""), "{lines:?}");
        assert!(
            lines.iter().any(|(line, _)| *line == 1),
            "the function is a lead too: {lines:?}"
        );
        assert_eq!(
            evidence.concept("add to cart", deadline()).unwrap().hits[0].line,
            6
        );
        assert!(
            evidence.concept("? !", deadline()).is_err(),
            "no words, no search"
        );
    }

    #[test]
    fn plain_word_leads_rank_the_files_whose_names_hold_the_prompts_words() {
        let (_dir, evidence) = evidence(false);
        let leads = evidence.word_leads("run helper", 3, deadline()).unwrap();
        assert_eq!(
            leads.first().map(|lead| lead.path.as_str()),
            Some("src/a.rs")
        );
    }

    #[test]
    fn the_relevance_answer_counts_the_prompts_words_in_this_repository() {
        let (_dir, evidence) = evidence(false);
        let answer = evidence
            .relevance("where is the helper that runs run", deadline())
            .unwrap();
        assert!(answer.input.graph);
        assert!(answer.input.files_considered >= FILES.len());
        let helper = answer
            .input
            .keywords
            .iter()
            .find(|row| row.keyword == "helper")
            .expect("a row for helper");
        assert!(helper.weight > 0.0);
        assert!(
            answer
                .input
                .cofiles
                .iter()
                .any(|file| file.path == "src/a.rs")
        );
        assert!(answer.input.cofiles.iter().all(|file| file.path != ".env"));
    }

    #[test]
    fn evidence_read_while_the_graph_lags_marks_the_brief_stale_and_a_fresh_one_does_not() {
        let (_dir, behind) = evidence(true);
        let _ = behind.symbols("run", deadline()).unwrap();
        assert!(behind.staleness().line().is_some());
        let (_dir, fresh) = evidence(false);
        let _ = fresh.symbols("run", deadline()).unwrap();
        let _ = fresh.grep("run", 3, deadline()).unwrap();
        assert!(fresh.staleness().line().is_none());
        // A failed read says nothing about the age of the evidence.
        let (_dir, failing) = evidence(true);
        let _ = failing.concept("x", deadline());
        assert!(failing.staleness().line().is_none());
    }

    #[test]
    fn a_repository_without_a_graph_still_answers_the_text_reads() {
        let (_dir, root) = repo(FILES);
        let service = Service::open(&root).expect("the service opens");
        let evidence = BriefEvidence::new(service.shared(), false);
        assert_eq!(
            evidence.symbols("run", deadline()).unwrap_err(),
            "the code graph is not built yet"
        );
        let found = evidence.grep("fn run", 10, deadline()).unwrap();
        assert_eq!(found.hits.len(), 1);
        let answer = evidence
            .relevance("where is run defined", deadline())
            .unwrap();
        assert!(!answer.input.graph, "no graph answered");
    }

    #[test]
    fn tracked_files_are_sorted_and_never_list_a_credential_file() {
        let (_dir, evidence) = evidence(false);
        let files = evidence.tracked_files(deadline()).unwrap();
        let mut sorted = files.clone();
        sorted.sort_unstable();
        assert_eq!(files, sorted);
        for tracked in [
            "Cargo.toml",
            "src/a.rs",
            "src/b.rs",
            "src/lib.rs",
            "tests/it.rs",
        ] {
            assert!(files.iter().any(|file| file == tracked), "{tracked}");
        }
        assert!(!files.iter().any(|file| file == ".env"));
    }

    #[test]
    fn a_file_read_is_confined_to_regular_non_credential_files_under_the_cap() {
        let (dir, root) = repo(FILES);
        std::os::unix::fs::symlink("/etc/hostname", root.join("link.txt")).unwrap();
        std::fs::write(root.join("big.txt"), "x".repeat(2_000)).unwrap();
        let _ = open(&root);
        let service = Service::open(&root).expect("the service opens");
        let evidence = BriefEvidence::new(service.shared(), false);

        assert_eq!(
            evidence.read_file("src/b.rs", 1_000, deadline()).unwrap(),
            "use crate::a::run;\n\npub fn go() -> u32 {\n    run(1)\n}\n"
        );
        for refused in [
            ".env",
            "../outside.txt",
            "/etc/passwd",
            "link.txt",
            "big.txt",
            "src",
            "absent.rs",
        ] {
            assert!(
                evidence.read_file(refused, 1_000, deadline()).is_err(),
                "{refused} was read"
            );
        }
        drop(dir);
    }
}
