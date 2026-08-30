//! Transport-agnostic service: one `Request` in, one `Response` out.
//!
//! All cross-crate contract calls (graph analyses, context rendering) are
//! centralized in the `bridge` module at the bottom so integration drift is
//! a one-line fix per call site.

use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Serialize;
use serde_json::{Value, json};

use pixel_index::TrigramExtractor;
use pixel_index::index::{MAX_FILE_BYTES, open_regular_bounded};
use pixel_index::indexset::{IndexSet, IndexSetError};
use pixel_facts::FactsStore;
use pixel_graph::{EdgeKind, EdgeRow, GraphStore, SymbolKind, SymbolRow};
use pixel_proto::{Envelope, Epistemics, ErrorCode, PixelError, SnapshotInfo, Warning};

pub const GRAPH_DB_FILE: &str = "graph.db";
/// Increment whenever the daemon request/response contract changes in a way
/// that an older process cannot safely serve to a newer CLI. Bumped from 6
/// to 7 with the Envelope v2 migration: the wire shape changed from
/// `{ok, error, data}` to the full `Envelope` (`ok, op, protocol, requestId,
/// snapshot, epistemics, budget, result, error, warnings`), gated by
/// `pixel_proto::ENVELOPE_PROTOCOL_VERSION`.
pub const PROTOCOL_VERSION: u64 = 7;

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ServeError {
    Index(IndexSetError),
    Io(std::io::Error),
    Msg(String),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeError::Index(e) => write!(f, "{e}"),
            ServeError::Io(e) => write!(f, "io error: {e}"),
            ServeError::Msg(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for ServeError {}

impl From<IndexSetError> for ServeError {
    fn from(e: IndexSetError) -> Self {
        ServeError::Index(e)
    }
}
impl From<std::io::Error> for ServeError {
    fn from(e: std::io::Error) -> Self {
        ServeError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// wire types — now derived from pixel-proto (the single contract crate)
// ---------------------------------------------------------------------------

/// The daemon request type. Re-exported from `pixel_proto::Op` so the daemon,
/// CLI, and MCP surfaces all share one enum — per PLAN.md A1, this kills the
/// N-touchpoint op-registration problem (adding an op is one variant here,
/// not edits across 4+ crates).
pub use pixel_proto::Op as Request;

/// The daemon response type: a `pixel_proto::Envelope<serde_json::Value>`.
/// Success → `Envelope::success(op_name, result)`; failure →
/// `Envelope::failure(op_name, error)`. The old ad-hoc `{ok, error, data}`
/// struct is gone; `resp.data()` reads the envelope's `result` field.
pub type Response = Envelope<Value>;

/// Classify a daemon read-op error string into the best-fit `ErrorCode`.
/// The message is always preserved verbatim in the envelope's `error.message`;
/// the code is for programmatic handling.
///
/// Bug 6 fix: the previous version had 4 branches, 3 of which were dead code
/// — no `Err(...)` path in this crate (or in the crates it wraps via
/// `.map_err(|e| e.to_string())`) ever produces a message containing
/// `"index"` + `"build"`/`"rebuild"`, `"not indexed"`/`"no index"`, or
/// `"ambiguous"` (verified by grepping every `format!`/literal `Err` site in
/// this workspace: `IndexBuilding`/`NotIndexed` are never surfaced as
/// errors — `ensure_graph` builds lazily instead of failing when the graph
/// is absent, and `IndexSet::open_or_build` does the same for the text
/// index; the one "ambiguous" case, `resolve_symbol`'s multi-candidate
/// result, is returned as `Ok(candidates_value(...))`, never an `Err`). So
/// in practice every error fell through to `InvalidInput`, and a genuine
/// not-found lookup (bad uid/name) was indistinguishable from a malformed
/// request.
///
/// Fixed by routing the two *actually reachable* not-found message shapes
/// (`resolve_symbol` and `op_context`, both in this file) to `NotFound`.
/// Everything else — malformed regex, bad params, opaque messages
/// forwarded from other crates — stays `InvalidInput`, which is the
/// correct default for "the request itself was not satisfiable."
///
/// `IndexBuilding`/`NotIndexed`/`Ambiguous` remain defined in `ErrorCode`
/// for ops that may legitimately need them later; this function just no
/// longer pretends to reach them via string-sniffing when nothing produces
/// a matching message today.
fn classify_error(msg: &str) -> ErrorCode {
    let lower = msg.to_lowercase();
    if lower.starts_with("no symbol named") || lower.starts_with("no symbol with uid") {
        ErrorCode::NotFound
    } else {
        ErrorCode::InvalidInput
    }
}

/// Build a failure envelope from a plain error string (the shape `dispatch`
/// returns). Used by `handle` and the daemon transport for pre-parse errors.
pub fn failure_response(op: &str, msg: impl Into<String>) -> Response {
    let msg = msg.into();
    let code = classify_error(&msg);
    Envelope::failure(op, PixelError::new(code, msg))
}

// ---------------------------------------------------------------------------
// service
// ---------------------------------------------------------------------------

pub struct Service {
    root: PathBuf,
    index: IndexSet,
    graph: Option<GraphStore>,
}

impl Service {
    /// Open (building layers if needed) the text index; graph db is lazy.
    pub fn open(root: &Path) -> Result<Self, ServeError> {
        let root = root
            .canonicalize()
            .map_err(|e| ServeError::Msg(format!("bad root {}: {e}", root.display())))?;
        let index = IndexSet::open_or_build(&root, Box::new(TrigramExtractor))?;
        Ok(Service {
            root,
            index,
            graph: None,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn graph_db_path(&self) -> PathBuf {
        self.root
            .join(pixel_index::index::SHARD_DIR)
            .join(GRAPH_DB_FILE)
    }

    /// Watcher hook: refresh one file in index + graph (best effort).
    pub fn refresh_file(&mut self, rel: &str) {
        self.index.refresh_file(rel);
        let db = self.graph_db_path();
        if db.exists() {
            bridge::update_file(&self.root, &db, rel);
            // Drop the cached handle so the next read sees the update.
            self.graph = None;
        }
    }

    /// Watcher hook: file deleted.
    pub fn remove_file(&mut self, rel: &str) {
        self.index.remove_file(rel);
        let db = self.graph_db_path();
        if db.exists() {
            if let Ok(mut store) = GraphStore::open(&db) {
                let _ = store.remove_file(rel);
            }
            self.graph = None;
        }
    }

    /// Make sure `self.graph` is populated; builds graph.db on first use and
    /// rebuilds it when the working tree has drifted from the indexed state
    /// (detected via the build-time freshness signature). Returns build info
    /// (stats + timing) when a build/rebuild happened.
    fn ensure_graph(&mut self) -> Result<Option<Value>, String> {
        if self.graph.is_some() {
            return Ok(None);
        }
        let db = self.graph_db_path();
        // An existing db is only reused if its freshness signature matches the
        // current working tree; otherwise it is stale (files added/removed/
        // edited since it was built) and is rebuilt from scratch.
        let stale = db.exists() && !bridge::is_fresh(&self.root, &db);
        let built = if !db.exists() || stale {
            let (stats, build_ms) = self.rebuild_graph()?;
            Some(json!({
                "graph_built": true,
                "build_ms": build_ms,
                "stats": stats,
            }))
        } else {
            None
        };
        if self.graph.is_none() {
            self.graph = Some(GraphStore::open(&db).map_err(|e| e.to_string())?);
        }
        Ok(built)
    }

    fn rebuild_graph(&mut self) -> Result<(Value, u64), String> {
        let db = self.graph_db_path();
        let tmp = db.with_file_name(format!(".graph-rebuild-{}.db", std::process::id()));
        self.graph.take();
        remove_sqlite_files(&tmp)?;
        let started = Instant::now();
        let stats = bridge::build_graph(&self.root, &tmp)?;
        {
            let checkpoint = GraphStore::open(&tmp).map_err(|error| error.to_string())?;
            checkpoint
                .conn()
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .map_err(|error| error.to_string())?;
        }
        remove_sqlite_sidecars(&db)?;
        std::fs::rename(&tmp, &db).map_err(|error| {
            format!(
                "publish graph {} -> {}: {error}",
                tmp.display(),
                db.display()
            )
        })?;
        remove_sqlite_sidecars(&tmp)?;
        self.graph = Some(GraphStore::open(&db).map_err(|error| error.to_string())?);
        Ok((stats, started.elapsed().as_millis() as u64))
    }

    /// Open a fresh, on-disk `GraphStore` handle purely for the search
    /// ranking symbol signal (Bug 2 fix). Deliberately independent of
    /// `self.graph`: that field is populated only as a side effect of some
    /// OTHER op (`targets`, `symbol`, `impact`, ...) having called
    /// `ensure_graph()` earlier in this same daemon process, so reading it
    /// here would make ranking depend on which unrelated ops happened to
    /// run first — the exact non-determinism this fixes (and it is always
    /// `None` for `--no-daemon`/in-process CLI runs, since a fresh
    /// `Service::open` never populates it). Basing the decision solely on
    /// "does graph.db exist on disk" makes the same search call produce the
    /// same ranking regardless of prior daemon activity or transport.
    ///
    /// This never builds or rebuilds the graph — mirrors `op_status`'s
    /// existing pattern of opening the db directly without going through
    /// `ensure_graph()`. A search must stay within its latency budget and
    /// can never pay graph-build cost (which can take minutes on a large or
    /// dirty repo); when no graph.db exists yet, ranking simply proceeds
    /// without the symbol signal, same as today.
    fn open_graph_for_ranking(&self) -> Option<GraphStore> {
        let db = self.graph_db_path();
        if db.exists() {
            GraphStore::open(&db).ok()
        } else {
            None
        }
    }

    pub fn handle(&mut self, req: Request) -> Response {
        let op_name = req.op_name();
        // Ops that return repo state attach a `snapshot` envelope field so
        // callers can correlate the answer with the exact working-tree state
        // it was computed against (HEAD, branch, dirty file list). This
        // covers the git-state ops AND every retrieval-class op: a retrieval
        // answer is only meaningful relative to the repo state it was
        // computed against.
        let attach_snapshot = matches!(
            op_name,
            "inspect" | "review" | "diff" | "status" | "changes"
        ) || is_retrieval_op(op_name);
        match self.dispatch(req) {
            Ok(v) => {
                let mut env = Envelope::success(op_name, v);
                if attach_snapshot {
                    env = env.with_snapshot(self.repo_snapshot());
                }
                // Epistemics choke point: EVERY successful retrieval-class
                // response carries an `epistemics` object — this is the ONLY
                // place retrieval envelopes are built, so an op cannot ship
                // without one. Ops that fired caps have them named in
                // `basis` and mirrored as envelope warnings; ops that
                // attested nothing get a conservative not-closed-world
                // default rather than an implied claim of completeness.
                if is_retrieval_op(op_name) {
                    let (epistemics, cap_warnings) =
                        derive_epistemics(op_name, env.result.as_ref().unwrap_or(&Value::Null));
                    env = env.with_epistemics(epistemics);
                    if !cap_warnings.is_empty() {
                        env = env.with_warnings(cap_warnings);
                    }
                }
                env
            }
            Err(msg) => failure_response(op_name, msg),
        }
    }

    /// Build an Envelope v2 `SnapshotInfo` from the current working-tree
    /// state: HEAD oid, branch name, and the list of dirty (modified /
    /// staged / untracked) repo-relative paths. `token` is left `None`
    /// here — pixel-ops computes the validated snapshot token separately.
    fn repo_snapshot(&self) -> SnapshotInfo {
        let head = pixel_index::gitsync::rev_parse_head(&self.root);
        let branch = pixel_index::gitsync::current_branch(&self.root);
        let dirty: Vec<String> = pixel_index::gitsync::status_porcelain(&self.root)
            .into_iter()
            .map(|(_xy, path)| path)
            .collect();
        SnapshotInfo {
            token: None,
            head,
            branch,
            dirty,
        }
    }

    fn dispatch(&mut self, req: Request) -> Result<Value, String> {
        match req {
            Request::Ping => Ok(json!({
                "pong": true,
                "root": self.root.display().to_string(),
                "protocol_version": PROTOCOL_VERSION,
            })),
            Request::Shutdown => Ok(json!({"shutting_down": true})),
            Request::Recall { .. } => Err(
                "recall ops are served by the recall daemon (`gitpixel recall daemon start`), not a repository daemon"
                    .to_string(),
            ),
            Request::Search {
                pattern,
                json: _,
                limit,
                offset,
                paths,
                scope,
            } => self.op_search(&pattern, limit, offset, paths.as_deref(), scope.as_deref()),
            Request::Targets { task, limit } => self.op_targets(&task, limit),
            Request::Symbol { name } => self.op_symbol(&name),
            Request::Context { uid, budget_tokens } => self.op_context(&uid, budget_tokens),
            Request::Impact {
                uid_or_name,
                direction,
                depth,
            } => self.op_impact(&uid_or_name, &direction, depth),
            Request::Uses {
                uid_or_name,
                role,
                offset,
            } => self.op_uses(&uid_or_name, &role, offset),
            Request::Trace { from, to } => self.op_trace(&from, &to),
            Request::Processes { offset } => self.op_processes(offset),
            Request::Clusters { offset } => self.op_clusters(offset),
            Request::Changes { base, offset } => self.op_changes(base.as_deref(), offset),
            Request::Graph {} => self.op_graph(),
            Request::Status {} => self.op_status(),
            Request::Resolve { phrase, limit } => self.op_resolve(&phrase, limit),
            Request::History { query, facet, limit } => {
                self.op_history(&query, facet.as_deref(), limit)
            }
            Request::Lifecycle { path, token } => self.op_lifecycle(path.as_deref(), token.as_deref()),
            Request::Excavate { phrase, path, from, to, limit } => {
                self.op_excavate(phrase.as_deref(), path.as_deref(), from.as_deref(), to.as_deref(), limit)
            }
            Request::Reconcile { strategy, push } => {
                self.op_reconcile(strategy.as_deref(), push.as_deref())
            }
            Request::Journal { kind, path, detail } => {
                self.op_journal(&kind, path.as_deref(), detail.as_deref())
            }
            Request::Inspect { .. } => {
                pixel_ops::inspect::inspect(&self.root)
            }
            Request::Review { cursor, byte_cap } => {
                pixel_ops::review::review(&self.root, cursor.as_deref(), byte_cap)
            }
            Request::Diff { from, to, paths, byte_cap } => {
                pixel_ops::diff::diff(&self.root, &from, to.as_deref(), paths.as_deref(), byte_cap)
            }
            Request::HistoryOp { ref_name, limit, detail, cursor, byte_cap } => {
                pixel_ops::history::history(
                    &self.root,
                    ref_name.as_deref(),
                    limit,
                    detail.as_deref().unwrap_or("compact"),
                    cursor.as_deref(),
                    byte_cap,
                )
            }
            Request::Publish {
                message,
                files,
                expected_head,
                push,
                amend,
                request_id,
            } => {
                let opts = pixel_ops::publish::PublishOptions {
                    message,
                    files,
                    expected_head,
                    expected_fingerprints: std::collections::BTreeMap::new(),
                    push: push.unwrap_or(false),
                    amend: amend.unwrap_or(false),
                    request_id,
                };
                pixel_ops::publish::publish(&self.root, &opts, None)
            }
            Request::Push {
                remote,
                refspec,
                force_with_lease,
                request_id,
            } => {
                let opts = pixel_ops::push::PushOptions {
                    remote,
                    refspec,
                    request_id,
                    force_with_lease: force_with_lease.unwrap_or(false),
                };
                pixel_ops::push::push(&self.root, &opts, None)
            }
            Request::Ship {
                message,
                files,
                remote,
                refspec,
                request_id,
            } => {
                pixel_ops::ship::ship(&self.root, &message, &files, &remote, &refspec, &request_id)
            }
            Request::BranchOp { name, from, request_id } => {
                let opts = pixel_ops::branch::BranchOptions {
                    name,
                    from,
                    request_id,
                };
                pixel_ops::branch::branch(&self.root, &opts)
            }
            Request::Update {
                expected_head,
                target_oid,
                request_id,
            } => {
                let opts = pixel_ops::update::UpdateOptions {
                    expected_head,
                    target_oid,
                    request_id,
                };
                pixel_ops::update::update(&self.root, &opts)
            }
            Request::Sync { remote, refspec } => {
                pixel_ops::sync::sync(&self.root, &remote, refspec.as_deref())
            }
        }
    }

    // -- ops ---------------------------------------------------------------

    fn op_search(
        &self,
        pattern: &str,
        limit: Option<usize>,
        offset: Option<usize>,
        paths: Option<&[String]>,
        scope: Option<&str>,
    ) -> Result<Value, String> {
        // Default row limit and byte cap protect against broad patterns
        // (`.*`, short literals that hit every file) returning unbounded
        // output. A caller-provided limit overrides the row cap; the byte cap
        // always applies as a safety valve.
        const DEFAULT_LIMIT: usize = 100;
        const MAX_LIMIT: usize = 10_000;
        const BYTE_CAP: usize = 64 * 1024;
        /// Per-match text cap: a single match line is truncated to this many
        /// bytes so one oversized line cannot bypass the byte cap.
        const PER_MATCH_TEXT_CAP: usize = 4096;
        let row_limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = offset.unwrap_or(0);

        // `scope` selects match ORDER, not a different data source. `None`/
        // `""` is unranked path/line order; `"code"` (case-insensitive) is
        // ranked via pixel-rank-family RRF (M1 gate per PLAN.md). Any other
        // value used to fall through silently to unranked search (Bug 5) —
        // a typo'd or unimplemented scope gave the caller zero signal their
        // request wasn't honored.
        let scope_normalized = scope.map(str::to_lowercase);
        let ranked = match scope_normalized.as_deref() {
            None | Some("") => false,
            Some("code") => true,
            Some(other) => {
                return Err(format!(
                    "unsupported search scope {other:?}; supported values are \"code\" \
                     (rank matches by file-level signals) or omitting `scope` for unranked \
                     path/line order"
                ));
            }
        };

        // Epistemics: the ranked branch's candidate pool is itself capped —
        // when it fires, ranking never even saw the overflow candidates.
        let mut ranked_pool_capped = false;
        let (matches, stats) = if ranked {
            // Ranked search cannot simply rerank the (offset, limit)-sliced
            // page the unranked branch fetches below: that page is sliced
            // in PATH order BEFORE ranking exists, so (a) the single
            // best-ranked match is invisible unless it happens to land
            // inside that slice (e.g. an exact filename match sorting
            // alphabetically last is never even fetched at `--limit 5`),
            // and (b) `next_offset` walks pre-rank order while the emitted
            // rows are in post-rank order — a page boundary and a rank
            // reordering disagreeing means paging duplicates or drops rows
            // (confirmed: 31 true matches paged via limit=40/offset=40
            // previously yielded 31 rows but only 30 distinct).
            //
            // Fix: fetch one bounded candidate POOL (bounded by
            // RANK_CANDIDATE_CAP — the same style of safety cap `row_limit`
            // already enforces everywhere else in this function; never
            // "fetch everything"), rank that whole pool ONCE, then serve
            // `offset`/`row_limit` as a plain slice over the resulting
            // stable array. Because the pool and its rank order are
            // recomputed identically on every call against the same repo
            // state (see `open_graph_for_ranking` for the accompanying
            // determinism fix), `offset` now indexes one coherent sequence:
            // paging through it can neither skip nor repeat a row.
            const RANK_CANDIDATE_CAP: usize = MAX_LIMIT;
            let (pool, pool_stats) = self
                .index
                .search_page_in(pattern, 0, Some(RANK_CANDIDATE_CAP), paths)
                .map_err(|e| e.to_string())?;
            if pool_stats.truncated {
                ranked_pool_capped = true;
            }
            let ranking_graph = self.open_graph_for_ranking();
            let ranked_pool = rank_search_matches(&pool, pattern, &ranking_graph);

            let start = offset.min(ranked_pool.len());
            // More ranked rows remain beyond this page (independent of the
            // byte cap, which is applied below). Folded into `stats.truncated`
            // — the same meaning the unranked branch's `stats.truncated`
            // already carries: "the candidate/index layer says this page
            // isn't everything."
            let more_beyond_page = start.saturating_add(row_limit) < ranked_pool.len();
            let mut stats = pool_stats;
            stats.truncated = stats.truncated || more_beyond_page;
            (ranked_pool[start..].to_vec(), stats)
        } else {
            self.index
                .search_page_in(pattern, offset, Some(row_limit), paths)
                .map_err(|e| e.to_string())?
        };

        // Render matches until either the row limit or the byte cap is hit.
        let mut arr: Vec<Value> = Vec::with_capacity(matches.len().min(row_limit));
        let mut bytes = 0usize;
        let mut byte_capped = false;
        for m in &matches {
            // Cap per-match text so a single oversized line cannot dominate.
            let text = if m.line.len() > PER_MATCH_TEXT_CAP {
                let mut end = PER_MATCH_TEXT_CAP;
                while !m.line.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…[truncated]", &m.line[..end])
            } else {
                m.line.clone()
            };
            let entry = json!({"path": m.path, "line": m.line_number, "text": text});
            let entry_bytes = serde_json::to_vec(&entry)
                .map_err(|error| error.to_string())?
                .len();
            if bytes.saturating_add(entry_bytes) > BYTE_CAP {
                byte_capped = true;
                break;
            }
            bytes += entry_bytes;
            arr.push(entry);
            if arr.len() >= row_limit {
                break;
            }
        }
        // `truncated` is authoritative from the index layer (which knows if
        // more candidates exist) OR from the byte cap. Do NOT use
        // `arr.len() < matches.len()` alone — it can be true when exactly
        // `limit` matches were found but the byte cap reduced the output.
        let truncated = stats.truncated || byte_capped;
        let next_offset = truncated.then_some(offset.saturating_add(arr.len()));
        // Named caps for the envelope epistemics: every bound that actually
        // fired on THIS response, so a partial answer is explicitly bounded
        // instead of silently truncated.
        let mut caps: Vec<String> = Vec::new();
        if byte_capped {
            caps.push(format!(
                "output truncated by the {BYTE_CAP}-byte response cap; continue via next_offset"
            ));
        }
        if stats.truncated {
            caps.push(format!(
                "match list truncated at row limit {row_limit}; more matches exist — continue \
                 via next_offset"
            ));
        }
        if ranked_pool_capped {
            caps.push(format!(
                "ranked candidate pool capped at {MAX_LIMIT} matches; ranking never saw \
                 candidates beyond the cap"
            ));
        }
        Ok(json!({
            "matches": arr,
            "caps": caps,
            "truncated": truncated,
            "offset": offset,
            "next_offset": next_offset,
            "limit": row_limit,
            "byte_cap": BYTE_CAP,
            "match_count": arr.len(),
            "ranked": ranked,
            "stats": {
                "candidates": stats.candidates,
                "scanned_all": stats.scanned_all,
                "matches": stats.matches,
                "elapsed_us": stats.elapsed_us as u64,
                "truncated": stats.truncated,
            }
        }))
    }

    /// Sniper target list: tokenize the task, gather lexical + graph signals,
    /// fuse, tier. Graph failure degrades to lexical-only (envelope says so)
    /// instead of erroring — a scoping request must never die on a broken
    /// graph build.
    fn op_targets(&mut self, task: &str, limit: Option<usize>) -> Result<Value, String> {
        use pixel_rank as engine;
        use pixel_graph::targets as graph_targets;

        let started = Instant::now();
        let query = engine::tokenize_task(task)?;

        let ensured = self.ensure_graph();
        let graph_available = ensured.is_ok();
        let build_info = ensured.ok().flatten();

        let all_paths = self.index.paths();

        // S3: per-keyword content match counts (capped probes keep this ms-scale).
        const CONTENT_PROBE_LIMIT: usize = 500;
        let mut content_hits: BTreeMap<String, Vec<(String, u32)>> = BTreeMap::new();
        // Epistemics: every probe cap that fires is NAMED here and forces
        // lower_bound on the report envelope — a truncated probe must never
        // feed an "exhaustive" claim.
        let mut probe_caps: Vec<String> = Vec::new();
        // Phase 3 item 1 (targets evidence): keep the first ~2 match lines per
        // (file, keyword) so the caller can verify a target's content match
        // without re-searching. Near-zero cost — the lines are already fetched.
        let mut evidence: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for kw in &query.keywords {
            // Word-bounded so "auth" cannot count every "author" as signal.
            // Keywords are [a-z0-9_]+ by construction (tokenize_task), but
            // escape defensively anyway.
            let pattern = format!(r"(?i)\b{}\b", regex_escape_keyword(kw));
            if let Ok((matches, probe_stats)) =
                self.index
                    .search_page_in(&pattern, 0, Some(CONTENT_PROBE_LIMIT), None)
            {
                if probe_stats.truncated {
                    probe_caps.push(format!(
                        "content probe truncated at {CONTENT_PROBE_LIMIT} matches for keyword \
                         '{kw}'; files beyond the cap carry no content signal"
                    ));
                }
                let mut counts: BTreeMap<String, u32> = BTreeMap::new();
                let mut kept_per_file: HashMap<String, usize> = HashMap::new();
                for m in matches {
                    *counts.entry(m.path.clone()).or_default() += 1;
                    let kept = kept_per_file.entry(m.path.clone()).or_insert(0);
                    if *kept < 2 {
                        *kept += 1;
                        evidence.entry(m.path.clone()).or_default().push(json!({
                            "line": m.line_number,
                            "text": m.line,
                            "keyword": kw,
                        }));
                    }
                }
                if !counts.is_empty() {
                    content_hits.insert(kw.clone(), counts.into_iter().collect());
                }
            }
        }

        let mut symbol_hits = Vec::new();
        let mut graph_neighbors: Vec<(String, String)> = Vec::new();
        let mut cluster_neighbors: Vec<(String, String)> = Vec::new();
        let mut envelope = None;
        if graph_available {
            let store = self.graph.as_ref().unwrap();
            symbol_hits = graph_targets::symbol_hits(store, &query.keywords, &query.exact_tokens)
                .map_err(|e| e.to_string())?;

            // Graph expansion is seeded from the lexical pre-fuse so every
            // P1/P2 neighbor traces back to a lexical anchor.
            const MAX_SEED_FILES: usize = 8;
            const MAX_SEED_SYMBOLS: usize = 24;
            let seed_paths: Vec<String> =
                engine::lexical_rank(&all_paths, &query.keywords, &symbol_hits, &content_hits)
                    .into_iter()
                    .take(MAX_SEED_FILES)
                    .collect();
            let seed_set: HashSet<&str> = seed_paths.iter().map(String::as_str).collect();
            let mut seed_symbol_ids: Vec<i64> = Vec::new();
            for hit in &symbol_hits {
                if seed_set.contains(hit.path.as_str()) {
                    for (sym, _) in &hit.symbols {
                        if seed_symbol_ids.len() < MAX_SEED_SYMBOLS {
                            seed_symbol_ids.push(sym.id);
                        }
                    }
                }
            }

            let mut seen: HashSet<String> = HashSet::new();
            for (path, reason) in graph_targets::neighbor_files(store, &seed_symbol_ids)
                .map_err(|e| e.to_string())?
                .into_iter()
                .chain(
                    graph_targets::import_adjacent_files(store, &seed_paths)
                        .map_err(|e| e.to_string())?,
                )
            {
                if seen.insert(path.clone()) {
                    graph_neighbors.push((path, reason));
                }
            }
            cluster_neighbors =
                graph_targets::cluster_co_files(store, &seed_symbol_ids, &query.keywords)
                    .map_err(|e| e.to_string())?;

            let mut names: Vec<&str> = query.exact_tokens.iter().map(String::as_str).collect();
            for hit in &symbol_hits {
                for (sym, _) in &hit.symbols {
                    names.push(sym.name.as_str());
                }
            }
            envelope =
                Some(graph_targets::envelope_for_names(store, &names).map_err(|e| e.to_string())?);
        }

        let opts = engine::TargetsOptions {
            limit: limit.unwrap_or(engine::DEFAULT_LIMIT),
        };
        let mut report = engine::compute_targets(
            task,
            &query,
            engine::SignalInputs {
                all_paths,
                content_hits,
                symbol_hits,
                graph_neighbors,
                cluster_neighbors,
                graph_available,
                envelope,
                caps: probe_caps,
            },
            &opts,
        );
        // Phase 1c: rerank within tiers via the Engine-3 reranker (first
        // production call site). v1 = activity-only signals; session/error
        // channels land in Phase 3. The per-path test penalty demotes test
        // files only when the task does NOT mention tests/specs (a test file
        // is a worse target for a non-test task).
        let target_paths: Vec<String> = report.targets.iter().map(|t| t.path.clone()).collect();
        let signals = self.engine_signals(&target_paths);
        // Per-path test penalty: demote a test file only when the task does
        // NOT mention tests/specs (a test file is a *worse* target for a
        // non-test task, but a *better* one for a test task).
        let mentions_tests = task
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|t| {
                matches!(
                    t.to_ascii_lowercase().as_str(),
                    "test" | "tests" | "spec" | "specs"
                )
            });
        let penalty = |path: &str| -> f64 {
            if pixel_rank::signals::is_test_path(path) && !mentions_tests {
                0.7
            } else {
                1.0
            }
        };
        report.targets = pixel_rank::rerank::rerank_targets(report.targets, &signals, &penalty);
        let mut out = serde_json::to_value(&report).map_err(|e| e.to_string())?;
        // Phase 3 item 1: attach per-file content evidence to each target so
        // the caller can trust a content match without re-searching (S2).
        if let Some(targets) = out.get_mut("targets").and_then(Value::as_array_mut) {
            for t in targets {
                if let Some(path) = t.get("path").and_then(Value::as_str) {
                    if let Some(ev) = evidence.get(path) {
                        t["evidence"] = json!(ev);
                    }
                }
            }
        }
        if let Some(stats) = out.get_mut("stats") {
            stats["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
            stats["commit_oid"] = json!(self.index.status().commit_oid);
        }
        merge_build_info(&mut out, build_info);
        Ok(out)
    }

    fn op_symbol(&mut self, name: &str) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let files = file_map(store)?;
        let syms = store.symbols_by_name(name, 50).map_err(|e| e.to_string())?;
        let envelope = store.envelope_for_name(name).map_err(|e| e.to_string())?;
        let mut out = json!({
            "symbols": syms.iter().map(|s| symbol_json(s, &files)).collect::<Vec<_>>(),
            "envelope": envelope,
        });
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_context(&mut self, uid: &str, budget_tokens: Option<usize>) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let files = file_map(store)?;
        let sym = store
            .symbol_by_uid(uid)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no symbol with uid {uid:?}"))?;
        let envelope = store
            .envelope_for_name(&sym.name)
            .map_err(|e| e.to_string())?;
        let sym_json = symbol_json(&sym, &files);
        use pixel_context::estimate_tokens;

        let budget = budget_tokens.unwrap_or(2000);
        let value_tokens =
            |value: &Value| estimate_tokens(&serde_json::to_string(value).unwrap_or_default());
        // `budget_basis` declares the approximation behind the token cap:
        // `estimate_tokens` is a bytes/4 heuristic, not a real tokenizer, so
        // the fit is approximate and the response says so instead of
        // presenting the heuristic as an exact token count.
        let minimum_response = json!({
            "budget_tokens": budget,
            "budget_basis": pixel_context::BUDGET_BASIS,
            "rendered_tokens": 0,
            "budgeted": true,
            "truncated": false,
            "text": "",
        });
        let minimum = value_tokens(&minimum_response);
        if budget < minimum {
            return Err(format!(
                "context budget {budget} is below the minimum response size of {minimum} tokens"
            ));
        }

        let mut incoming = store.edges_to(sym.id, None).map_err(|e| e.to_string())?;
        let mut outgoing = store.edges_from(sym.id, None).map_err(|e| e.to_string())?;
        let edge_order = |a: &EdgeRow, b: &EdgeRow| {
            a.kind
                .as_str()
                .cmp(b.kind.as_str())
                .then(a.tier.as_str().cmp(b.tier.as_str()))
                .then(a.site_line.cmp(&b.site_line))
                .then(a.src_id.cmp(&b.src_id))
                .then(a.dst_id.cmp(&b.dst_id))
        };
        incoming.sort_by(edge_order);
        outgoing.sort_by(edge_order);
        incoming.dedup_by(|a, b| {
            a.src_id == b.src_id
                && a.dst_id == b.dst_id
                && a.kind == b.kind
                && a.tier == b.tier
                && a.site_line == b.site_line
        });
        outgoing.dedup_by(|a, b| {
            a.src_id == b.src_id
                && a.dst_id == b.dst_id
                && a.kind == b.kind
                && a.tier == b.tier
                && a.site_line == b.site_line
        });

        const EDGE_LIMIT: usize = 20;
        const MAX_CONTEXT_ITEMS: usize = 41;
        const MAX_CONTEXT_SOURCE_BYTES: usize = 256 * 1024;
        const MAX_TARGET_SNIPPET_BYTES: usize = 32 * 1024;
        const MAX_NEIGHBOR_SNIPPET_BYTES: usize = 4 * 1024;

        // Bound source retained before rendering. The target gets priority;
        // neighbors share only the remaining aggregate allowance.
        let mut source_remaining = budget.saturating_mul(4).min(MAX_CONTEXT_SOURCE_BYTES);
        let mut items = Vec::new();
        let target_cap = source_remaining.min(MAX_TARGET_SNIPPET_BYTES);
        let target = context_item(&self.root, &sym, &files, target_cap);
        source_remaining = source_remaining.saturating_sub(target.snippet.len());
        items.push(target);
        let mut seen_context = std::collections::HashSet::from([sym.id]);
        let neighbors = incoming
            .iter()
            .map(|edge| edge.src_id)
            .chain(outgoing.iter().map(|edge| edge.dst_id));
        let mut source_elided_items = 0usize;
        for symbol_id in neighbors {
            if !seen_context.insert(symbol_id) {
                continue;
            }
            if items.len() >= MAX_CONTEXT_ITEMS || source_remaining == 0 {
                source_elided_items += 1;
                continue;
            }
            if let Some(other) = symbol_by_id(store, symbol_id) {
                let cap = source_remaining.min(MAX_NEIGHBOR_SNIPPET_BYTES);
                let item = context_item(&self.root, &other, &files, cap);
                source_remaining = source_remaining.saturating_sub(item.snippet.len());
                items.push(item);
            }
        }

        let incoming_total = incoming.len();
        let outgoing_total = outgoing.len();
        let incoming_compact = compact_edges(store, &incoming, &files, false, EDGE_LIMIT)?;
        let outgoing_compact = compact_edges(store, &outgoing, &files, true, EDGE_LIMIT)?;
        let mut response = minimum_response;
        response["truncated"] = json!(
            incoming_total > EDGE_LIMIT || outgoing_total > EDGE_LIMIT || source_elided_items > 0
        );
        for (key, value) in [
            ("symbol", sym_json),
            (
                "envelope",
                serde_json::to_value(envelope).unwrap_or(Value::Null),
            ),
            ("incoming", incoming_compact),
            ("outgoing", outgoing_compact),
            ("incoming_total", json!(incoming_total)),
            ("outgoing_total", json!(outgoing_total)),
            ("context_items_total", json!(seen_context.len())),
            ("context_items_loaded", json!(items.len())),
        ] {
            let mut candidate = response.clone();
            candidate[key] = value;
            if value_tokens(&candidate) <= budget {
                response = candidate;
            } else {
                response["truncated"] = json!(true);
            }
        }
        if let Some(build_info) = built {
            let mut candidate = response.clone();
            candidate["graph_build"] = build_info;
            if value_tokens(&candidate) <= budget {
                response = candidate;
            } else {
                response["truncated"] = json!(true);
            }
        }

        let overhead = value_tokens(&response);
        let (mut text, layer, fit_elided_items) =
            bridge::render_context(&items, budget.saturating_sub(overhead));
        for (key, value) in [
            ("context_layer", json!(layer)),
            (
                "elided_items",
                json!(source_elided_items.saturating_add(fit_elided_items)),
            ),
        ] {
            let mut candidate = response.clone();
            candidate[key] = value;
            if value_tokens(&candidate) <= budget {
                response = candidate;
            } else {
                response["truncated"] = json!(true);
            }
        }
        if layer != "L2" || fit_elided_items > 0 {
            response["truncated"] = json!(true);
        }
        loop {
            response["text"] = json!(text);
            response["rendered_tokens"] = json!(estimate_tokens(
                response["text"].as_str().unwrap_or_default()
            ));
            if value_tokens(&response) <= budget {
                break;
            }
            let chars = response["text"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .count();
            if chars == 0 {
                break;
            }
            response["truncated"] = json!(true);
            text = text.chars().take(chars.saturating_sub(1)).collect();
        }
        Ok(response)
    }

    fn op_impact(
        &mut self,
        uid_or_name: &str,
        direction: &str,
        depth: Option<u32>,
    ) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let sym = match resolve_symbol(store, uid_or_name)? {
            Resolved::One(s) => s,
            Resolved::Many(v) => return candidates_value(store, &v),
        };
        let mut out = bridge::impact(store, &sym.uid, direction, depth.unwrap_or(3))?;
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_uses(
        &mut self,
        uid_or_name: &str,
        role: &str,
        offset: Option<usize>,
    ) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let sym = match resolve_symbol(store, uid_or_name)? {
            Resolved::One(s) => s,
            Resolved::Many(v) => return candidates_value(store, &v),
        };
        let files = file_map(store)?;
        let (edges, other_is_src) = match role {
            "callees" => (store.edges_from(sym.id, Some(EdgeKind::Calls)), false),
            _ => (store.edges_to(sym.id, Some(EdgeKind::Calls)), true),
        };
        const EDGE_LIMIT: usize = 20;
        let mut edges = edges.map_err(|e| e.to_string())?;
        edges.sort_by(|a, b| {
            a.site_line
                .cmp(&b.site_line)
                .then(a.src_id.cmp(&b.src_id))
                .then(a.dst_id.cmp(&b.dst_id))
                .then(a.tier.as_str().cmp(b.tier.as_str()))
        });
        let total_edges = edges.len();
        let offset = offset.unwrap_or(0).min(total_edges);
        let mut arr = Vec::with_capacity(total_edges.min(EDGE_LIMIT));
        for e in edges.iter().skip(offset).take(EDGE_LIMIT) {
            let other_id = if other_is_src { e.src_id } else { e.dst_id };
            let other = symbol_by_id(store, other_id);
            arr.push(json!({
                "symbol": other.as_ref().map(|s| symbol_json(s, &files)),
                "tier": e.tier.as_str(),
                "site_line": e.site_line,
            }));
        }
        let envelope = store
            .envelope_for_name(&sym.name)
            .map_err(|e| e.to_string())?;
        let returned_edges = arr.len();
        let has_more = offset.saturating_add(returned_edges) < total_edges;
        let mut out = json!({
            "symbol": symbol_json(&sym, &files),
            "role": if role == "callees" { "callees" } else { "callers" },
            "edges": arr,
            "total_edges": total_edges,
            "returned_edges": returned_edges,
            "edge_limit": EDGE_LIMIT,
            "offset": offset,
            "next_offset": has_more.then_some(offset.saturating_add(returned_edges)),
            "truncated": has_more,
            "envelope": envelope,
        });
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_trace(&mut self, from: &str, to: &str) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let from_sym = match resolve_symbol(store, from)? {
            Resolved::One(s) => s,
            Resolved::Many(v) => return candidates_value(store, &v),
        };
        let to_sym = match resolve_symbol(store, to)? {
            Resolved::One(s) => s,
            Resolved::Many(v) => return candidates_value(store, &v),
        };
        let mut out = bridge::trace(store, &from_sym.uid, &to_sym.uid)?;
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_processes(&mut self, offset: Option<usize>) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_mut().unwrap();
        let mut out = bridge::processes(store, offset.unwrap_or(0))?;
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_graph(&mut self) -> Result<Value, String> {
        let (stats, build_ms) = self.rebuild_graph()?;
        Ok(json!({
            "files": stats.get("files").cloned().unwrap_or(Value::Null),
            "symbols": stats.get("symbols").cloned().unwrap_or(Value::Null),
            "edges": stats.get("edges").cloned().unwrap_or(Value::Null),
            "unresolved": stats.get("unresolved").cloned().unwrap_or(Value::Null),
            "elapsed_ms": build_ms,
        }))
    }

    fn op_clusters(&mut self, offset: Option<usize>) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_mut().unwrap();
        let mut out = bridge::clusters(store, offset.unwrap_or(0))?;
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_changes(&mut self, base: Option<&str>, offset: Option<usize>) -> Result<Value, String> {
        const SYMBOL_LIMIT: usize = 20;
        const PROCESS_LIMIT: usize = 20;
        const PROCESSES_PER_SYMBOL_LIMIT: usize = 10;
        let offset = offset.unwrap_or(0);
        let built = self.ensure_graph()?;
        let root = self.root.clone();
        let store = self.graph.as_ref().unwrap();
        let mut out = bridge::changes(store, &root, base)?;
        let mut nested_processes_truncated = false;
        let symbols_total = out["symbols"].as_array().map_or(0, Vec::len);
        if let Some(symbols) = out["symbols"].as_array_mut() {
            symbols.sort_by(|a, b| {
                a["path"]
                    .as_str()
                    .cmp(&b["path"].as_str())
                    .then(a["uid"].as_str().cmp(&b["uid"].as_str()))
            });
            let start = offset.min(symbols.len());
            let end = start.saturating_add(SYMBOL_LIMIT).min(symbols.len());
            *symbols = symbols.drain(start..end).collect();
            for symbol in symbols {
                let process_total = symbol["processes"].as_array().map_or(0, Vec::len);
                if let Some(processes) = symbol["processes"].as_array_mut() {
                    processes.truncate(PROCESSES_PER_SYMBOL_LIMIT);
                }
                if let Some(object) = symbol.as_object_mut() {
                    object.insert("processes_total".into(), json!(process_total));
                    object.insert(
                        "processes_truncated".into(),
                        json!(process_total > PROCESSES_PER_SYMBOL_LIMIT),
                    );
                }
                nested_processes_truncated |= process_total > PROCESSES_PER_SYMBOL_LIMIT;
            }
        }
        let affected_processes_total = out["affected_processes"].as_array().map_or(0, Vec::len);
        if let Some(processes) = out["affected_processes"].as_array_mut() {
            let start = offset.min(processes.len());
            let end = start.saturating_add(PROCESS_LIMIT).min(processes.len());
            *processes = processes.drain(start..end).collect();
        }
        let returned_symbols = out["symbols"].as_array().map_or(0, Vec::len);
        let returned_processes = out["affected_processes"].as_array().map_or(0, Vec::len);
        let has_more = offset.saturating_add(returned_symbols) < symbols_total
            || offset.saturating_add(returned_processes) < affected_processes_total;
        if let Some(object) = out.as_object_mut() {
            object.insert("symbols_total".into(), json!(symbols_total));
            object.insert("returned_symbols".into(), json!(returned_symbols));
            object.insert("symbol_limit".into(), json!(SYMBOL_LIMIT));
            object.insert("offset".into(), json!(offset));
            object.insert(
                "next_offset".into(),
                json!(
                    has_more
                        .then_some(offset.saturating_add(returned_symbols.max(returned_processes)))
                ),
            );
            object.insert(
                "affected_processes_total".into(),
                json!(affected_processes_total),
            );
            object.insert("process_limit".into(), json!(PROCESS_LIMIT));
            object.insert(
                "returned_affected_processes".into(),
                json!(returned_processes),
            );
            object.insert(
                "truncated".into(),
                json!(has_more || nested_processes_truncated),
            );
        }
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_status(&mut self) -> Result<Value, String> {
        let s = self.index.status();
        let db = self.graph_db_path();
        let graph = if db.exists() {
            match GraphStore::open(&db) {
                Ok(store) => {
                    let (files, symbols, edges, unresolved) =
                        store.counts().map_err(|e| e.to_string())?;
                    json!({
                        "present": true,
                        "files": files,
                        "symbols": symbols,
                        "edges": edges,
                        "unresolved_calls": unresolved,
                    })
                }
                Err(e) => json!({"present": true, "error": e.to_string()}),
            }
        } else {
            json!({"present": false})
        };
        Ok(json!({
            "root": self.root.display().to_string(),
            "index": {
                "commit_oid": s.commit_oid,
                "base_files": s.base_files,
                "delta_files": s.delta_files,
                "overlay_files": s.overlay_files,
                "tombstones": s.tombstones,
            },
            "graph": graph,
            "facts": self.facts_visibility(),
        }))
    }

    /// Facts/history visibility for `op_status`: enough counters to tell a
    /// healthy db from a dead or poisoned one at a glance. Read-only — never
    /// triggers ingest (status must stay cheap).
    fn facts_visibility(&self) -> Value {
        let facts = match FactsStore::open(&self.root) {
            Ok(f) => f,
            Err(e) => return json!({"present": false, "error": e.to_string()}),
        };
        let state = facts.index_state();
        let count = |sql: &str| -> i64 {
            facts.conn().query_row(sql, [], |r| r.get(0)).unwrap_or(0)
        };
        let phase_a_done: bool = facts
            .conn()
            .query_row(
                "SELECT status FROM ingest_jobs WHERE phase = 'A'",
                [],
                |r| r.get::<_, String>(0),
            )
            .map(|s| s == "done")
            .unwrap_or(false);
        // Full repo commit count via rev-list so a frozen enumeration is
        // visible as commits_indexed < total_commits. The facts universe also
        // covers stash/reflog-only commits that `--all` doesn't count, so take
        // the max — indexed exceeding rev-list is healthy, not suspicious.
        let total_commits = std::process::Command::new("git")
            .args(["rev-list", "--count", "--all"])
            .current_dir(&self.root)
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok())
            .unwrap_or(0)
            .max(state.total_commits);
        json!({
            "present": true,
            "schema_version": state.schema_version,
            "phase": state.phase,
            "phase_a_done": phase_a_done,
            "commits_indexed": state.commits_indexed,
            "total_commits": total_commits,
            "diff_indexed_pct": state.diff_indexed_pct,
            "hunks_with_text": count(
                "SELECT count(*) FROM hunks WHERE length(added) > 0 OR length(removed) > 0"
            ),
            "diff_grams": count("SELECT count(*) FROM diff_grams"),
            "fresh": state.fresh,
        })
    }

    // -- Engine 1 / M3 / M4 / M5 ops --------------------------------------

    /// Engine 1: concept-index resolution cascade.
    fn op_resolve(&mut self, phrase: &str, limit: Option<usize>) -> Result<Value, String> {
        self.ensure_graph()?;
        let store = self.graph.as_ref().ok_or("graph store unavailable")?;
        // Phase 1c: feed activity-only rerank signals (git churn) over the
        // candidate universe; session/error channels land in Phase 3.
        let all_paths = self.index.paths();
        let signals = self.engine_signals(&all_paths);
        let opts = pixel_graph::concept_resolve::ResolveOptions {
            limit: limit.unwrap_or(8),
            // Phase 1c: wire the real Engine-3 reranker (per-path test
            // penalty) instead of the default LexicalReranker.
            reranker: Some(Box::new(EngineReranker::new(phrase))),
            signals: pixel_graph::concept_resolve::SignalBundle {
                activity: signals.activity,
                session: signals.session,
                session_reasons: signals.session_reasons,
                error_reasons: signals.error_reasons,
            },
        };
        let outcome = pixel_graph::concept_resolve::resolve(store, phrase, &opts)
            .map_err(|e| e.to_string())?;
        serde_json::to_value(&outcome).map_err(|e| e.to_string())
    }

    /// Lazy ingest on the query path: when the facts index is not fresh
    /// (never built, poisoned-and-rebuilt, or refs moved), run bounded ingest
    /// ticks before serving so a CLI with no daemon still gets real answers.
    /// Budget: `PIXEL_FACTS_QUERY_BUDGET_MS` (default 3000ms) — a query is
    /// never blocked longer than that; the attached `index_state` tells the
    /// caller whether coverage is complete.
    fn facts_open_and_catch_up(&self) -> Result<FactsStore, String> {
        let mut facts = FactsStore::open(&self.root).map_err(|e| e.to_string())?;
        if !facts.index_state().fresh {
            let _ = pixel_facts::ingest::lazy_ingest(&mut facts);
        }
        Ok(facts)
    }

    /// M3 / Engine 2: history-wide fact + diff search.
    fn op_history(
        &mut self,
        query: &str,
        facet: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Value, String> {
        let facts = self.facts_open_and_catch_up()?;
        let result = pixel_facts::search::search(
            &facts,
            query,
            facet.unwrap_or("all").into(),
            limit.unwrap_or(200),
        )
        .map_err(|e| e.to_string())?;
        let mut value = serde_json::to_value(&result).map_err(|e| e.to_string())?;
        value["index_state"] = serde_json::to_value(facts.index_state()).map_err(|e| e.to_string())?;
        Ok(value)
    }

    /// Engine 2: lifecycle of a path or token.
    fn op_lifecycle(
        &mut self,
        path: Option<&str>,
        token: Option<&str>,
    ) -> Result<Value, String> {
        let facts = self.facts_open_and_catch_up()?;
        let result = match (path, token) {
            (Some(p), _) => facts.path_lifecycle(p).map_err(|e| e.to_string())?,
            (None, Some(t)) => facts.token_lifecycle(t).map_err(|e| e.to_string())?,
            (None, None) => return Err("lifecycle requires a path or token".to_string()),
        };
        let mut value = serde_json::to_value(&result).map_err(|e| e.to_string())?;
        value["index_state"] =
            serde_json::to_value(facts.index_state()).map_err(|e| e.to_string())?;
        Ok(value)
    }

    /// Engine 2: history-wide discovery (rescue v2).
    fn op_excavate(
        &mut self,
        phrase: Option<&str>,
        path: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Value, String> {
        let facts = self.facts_open_and_catch_up()?;
        let result = facts
            .excavate(phrase, path, from, to, limit.unwrap_or(200))
            .map_err(|e| e.to_string())?;
        serde_json::to_value(&result).map_err(|e| e.to_string())
    }

    /// Engine 4: one-call deterministic branch sync (delegates to pixel-ops).
    fn op_reconcile(
        &mut self,
        strategy: Option<&str>,
        push: Option<&str>,
    ) -> Result<Value, String> {
        let opts = pixel_ops::reconcile::ReconcileOptions {
            strategy: strategy.unwrap_or("report").to_string(),
            push: push.unwrap_or("auto").to_string(),
            request_id: String::new(),
        };
        pixel_ops::reconcile::reconcile(&self.root, &opts)
    }

    /// M5: journal a session event into the session db (fire-and-forget).
    fn op_journal(
        &mut self,
        kind: &str,
        path: Option<&str>,
        detail: Option<&str>,
    ) -> Result<Value, String> {
        let store = pixel_session::store::Store::open(&self.root).map_err(|e| e.to_string())?;
        let data = match (path, detail) {
            (Some(p), Some(d)) => Some(json!({"path": p, "detail": d})),
            (Some(p), None) => Some(json!({"path": p})),
            (None, Some(d)) => Some(json!({"detail": d})),
            (None, None) => None,
        };
        let id = store
            .record_event_raw(kind, data.as_ref(), None)
            .map_err(|e| e.to_string())?;
        Ok(json!({"recorded": true, "id": id, "kind": kind}))
    }

    /// Engine-3 rerank signals shared by `op_resolve` and `op_targets`.
    /// v1 = activity-only: git churn over the last 90 days (via the one-shot
    /// `git log --name-only` fallback) plus the current dirty set. Session +
    /// error-sink channels land in Phase 3. Deterministic for a fixed repo
    /// state; degrades to an empty bundle on any git failure (the reranker
    /// then applies only the per-path test penalty).
    fn engine_signals(&self, candidates: &[String]) -> pixel_rank::signals::SignalBundle {
        use pixel_rank::signals::{SignalOptions, compute_signals};
        let runner = pixel_git::GitRunner::new(&self.root);
        let dirty: Vec<String> = pixel_index::gitsync::status_porcelain(&self.root)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let opts = SignalOptions {
            now_ms,
            ..Default::default()
        };
        compute_signals(&runner, None, &[], None, &dirty, candidates, &opts).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// epistemics — the honesty layer every retrieval-class response carries
// ---------------------------------------------------------------------------

/// The retrieval-class ops: answers computed FROM repo state (index/graph/
/// working tree) whose completeness can silently degrade under caps. Every
/// one of these MUST ship an `epistemics` object (enforced in
/// `Service::handle` + the `every_retrieval_op_response_carries_epistemics`
/// test). Mutation and admin ops (publish/push/ping/…) are not listed: they
/// report what they DID, not what exists, so completeness honesty does not
/// apply the same way.
pub const RETRIEVAL_OPS: &[&str] = &[
    "search",
    "resolve",
    "targets",
    "impact",
    "uses",
    "trace",
    "changes",
    "context",
    "symbol",
    "processes",
    "clusters",
];

fn is_retrieval_op(op_name: &str) -> bool {
    RETRIEVAL_OPS.contains(&op_name)
}

/// Backslash-escape regex metacharacters in a probe keyword. Keywords from
/// `tokenize_task` are `[a-z0-9_]+` by construction, so this is a no-op in
/// practice, but the probe must stay literal even if that invariant ever
/// changes upstream. Equivalent to `regex::escape` for the ASCII range
/// without pulling the `regex` crate into this crate's dependency set.
fn regex_escape_keyword(kw: &str) -> String {
    let mut out = String::with_capacity(kw.len());
    for c in kw.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if c.is_ascii() {
            out.push('\\');
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}

/// Derive the envelope [`Epistemics`] (and mirrored cap warnings) for a
/// retrieval-class response from the completeness markers the op's result
/// carries.
///
/// Why derivation-at-the-choke-point instead of a compile-time typed builder
/// per op: `dispatch` funnels 30+ ops through `Result<Value, String>`, and
/// several result shapes are produced by crates other agents own
/// (pixel-ops) — a full typed-response refactor ripples across ownership
/// boundaries. This function plus the response-walk test gives the same
/// guarantee mechanically: a retrieval response cannot ship without
/// epistemics, and an op that attests nothing is published as
/// `closed_world: false` (conservative) rather than silently complete.
///
/// Markers consumed (ops embed these in their result JSON):
/// - `caps: [string]` — named caps the op fired (search, targets via
///   `envelope.caps`).
/// - `truncated: bool` + `next_offset` — pagination/byte caps.
/// - `envelope.lower_bound` / `envelope.unresolved_same_name` — the graph
///   honesty envelope (impact/uses/symbol/context/targets).
/// - `scan_capped` + `basis` — resolve's bounded fallback scans.
/// - `graph_build.build_ms` presence — the graph was rebuilt for THIS answer,
///   so staleness is 0ms (the one cheap staleness signal available).
fn derive_epistemics(op_name: &str, v: &Value) -> (Epistemics, Vec<Warning>) {
    let mut caps: Vec<String> = Vec::new();

    // Op-declared named caps (top-level and inside the targets envelope).
    for path in ["/caps", "/envelope/caps"] {
        if let Some(arr) = v.pointer(path).and_then(Value::as_array) {
            caps.extend(arr.iter().filter_map(Value::as_str).map(String::from));
        }
    }

    // Generic pagination / byte-cap truncation.
    if v.get("truncated").and_then(Value::as_bool) == Some(true) {
        // Search already names its caps in `caps`; avoid a duplicate
        // generic entry when specific ones exist for this marker.
        let already_named = caps.iter().any(|c| c.contains("truncated"));
        if !already_named {
            if v.get("next_offset").is_some_and(|n| !n.is_null()) {
                caps.push(
                    "results truncated by row/byte cap; more exist — continue via next_offset"
                        .to_string(),
                );
            } else {
                caps.push("results truncated by an output cap".to_string());
            }
        }
    }

    // Graph honesty envelope (impact/uses/symbol/context; targets folds its
    // graph state into envelope.caps + note instead).
    let graph_lower =
        v.pointer("/envelope/lower_bound").and_then(Value::as_bool) == Some(true);
    if graph_lower {
        let unresolved = v
            .pointer("/envelope/unresolved_same_name")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if unresolved > 0 {
            caps.push(format!(
                "graph lower bound: {unresolved} unresolved same-name call site(s) — \
                 edges beyond this answer may exist"
            ));
        } else if caps.is_empty() {
            caps.push("graph lower bound: resolver could not close the world".to_string());
        }
    }

    // Resolve's bounded fallback scans.
    if v.get("scan_capped").and_then(Value::as_bool) == Some(true) {
        caps.push("fallback table scan hit its row cap; unscanned rows were never considered"
            .to_string());
    }

    let source = match op_name {
        "search" => "text index",
        "targets" | "resolve" => "text index + code graph",
        "changes" => "code graph + working-tree diff",
        _ => "code graph",
    };
    let mut basis = String::from(source);
    if let Some(tier_basis) = v.get("basis").and_then(Value::as_str) {
        // resolve: which tier produced the answer.
        basis.push_str("; ");
        basis.push_str(tier_basis);
    }
    if !caps.is_empty() {
        basis.push_str("; caps: ");
        basis.push_str(&caps.join("; "));
    }

    // The one cheap staleness signal: a graph rebuilt for this very answer
    // is 0ms stale. Anything else is left unmeasured (None), never guessed.
    let staleness_ms = v
        .get("graph_build")
        .and_then(|b| b.get("build_ms"))
        .and_then(Value::as_u64)
        .map(|_| 0u64);

    let epistemics = Epistemics {
        closed_world: caps.is_empty(),
        lower_bound: !caps.is_empty(),
        basis,
        staleness_ms,
    };
    let warnings = caps
        .into_iter()
        .map(|message| Warning {
            code: "RESULT_CAPPED".to_string(),
            message,
        })
        .collect();
    (epistemics, warnings)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Adapter wiring pixel-rank's Engine-3 reranker into pixel-graph's pluggable
/// `Reranker` trait. pixel-graph cannot depend on pixel-rank (circular), so
/// the daemon adapts `pixel_rank::rerank::rerank` into the trait here.
///
/// v1 = activity-only signals (the bundle is passed through as-is; the daemon
/// has no git/session signal source yet). Session + error-sink channels land
/// in Phase 3.
#[derive(Clone)]
struct EngineReranker {
    /// Whether the resolve phrase mentions tests/specs — gates the per-path
    /// test penalty (a test file is demoted only when the task is NOT about
    /// tests).
    mentions_tests: bool,
    /// The test-penalty multiplier (0.7).
    test_penalty: f64,
}

impl EngineReranker {
    fn new(task: &str) -> Self {
        EngineReranker {
            mentions_tests: task
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|t| {
                    matches!(
                        t.to_ascii_lowercase().as_str(),
                        "test" | "tests" | "spec" | "specs"
                    )
                }),
            test_penalty: 0.7,
        }
    }
}

impl pixel_graph::concept_resolve::Reranker for EngineReranker {
    fn rerank(
        &self,
        candidates: Vec<pixel_graph::concept_resolve::RankedCandidate>,
        signals: &pixel_graph::concept_resolve::SignalBundle,
    ) -> Vec<pixel_graph::concept_resolve::RankedCandidate> {
        use pixel_rank::rerank::RankedCandidate as PrCandidate;

        let pr_candidates: Vec<PrCandidate> = candidates
            .iter()
            .map(|c| PrCandidate {
                id: c.id,
                path: c.path.clone(),
                rrf_score: c.rrf_score,
                tier: c.tier.clone(),
            })
            .collect();
        let pr_signals = pixel_rank::signals::SignalBundle {
            activity: signals.activity.clone(),
            session: signals.session.clone(),
            session_reasons: signals.session_reasons.clone(),
            error_reasons: signals.error_reasons.clone(),
        };
        // Per-candidate test penalty: demote a test/spec file only when the
        // phrase itself is NOT about tests (per-path, via `is_test_path`).
        let penalty = |path: &str| -> f64 {
            if pixel_rank::signals::is_test_path(path) && !self.mentions_tests {
                self.test_penalty
            } else {
                1.0
            }
        };
        let reordered = pixel_rank::rerank::rerank(pr_candidates, &pr_signals, &penalty);

        // Restore the pixel-graph candidate shape (incl. `id`) by id — the
        // reranker only reorders, it never adds/removes candidates. Keying by
        // id (not path) keeps same-file concepts distinct (concept_resolve.rs
        // requires the adapter to preserve `id` through the round-trip).
        let by_id: HashMap<u64, &pixel_graph::concept_resolve::RankedCandidate> =
            candidates.iter().map(|c| (c.id, c)).collect();
        reordered
            .into_iter()
            .map(|c| {
                let orig = by_id[&c.id];
                pixel_graph::concept_resolve::RankedCandidate {
                    id: orig.id,
                    path: orig.path.clone(),
                    rrf_score: c.rrf_score,
                    tier: orig.tier.clone(),
                }
            })
            .collect()
    }

    fn clone_box(&self) -> Box<dyn pixel_graph::concept_resolve::Reranker> {
        Box::new(self.clone())
    }
}

enum Resolved {
    One(SymbolRow),
    Many(Vec<SymbolRow>),
}

/// `uid_or_name` protocol: '#' means uid; otherwise a name, with the
/// disambiguation protocol (`{candidates: [...], hint}`) on ambiguity.
fn resolve_symbol(store: &GraphStore, uid_or_name: &str) -> Result<Resolved, String> {
    if uid_or_name.contains('#') {
        return store
            .symbol_by_uid(uid_or_name)
            .map_err(|e| e.to_string())?
            .map(Resolved::One)
            .ok_or_else(|| format!("no symbol with uid {uid_or_name:?}"));
    }
    let syms = store
        .symbols_by_name(uid_or_name, 50)
        .map_err(|e| e.to_string())?;
    match syms.len() {
        0 => Err(format!("no symbol named {uid_or_name:?}")),
        1 => Ok(Resolved::One(syms.into_iter().next().unwrap())),
        _ => Ok(Resolved::Many(syms)),
    }
}

fn candidates_value(store: &GraphStore, syms: &[SymbolRow]) -> Result<Value, String> {
    let files = file_map(store)?;
    Ok(json!({
        "candidates": syms.iter().map(|s| symbol_json(s, &files)).collect::<Vec<_>>(),
        "hint": "ambiguous name; re-call with uid",
    }))
}

fn file_map(store: &GraphStore) -> Result<HashMap<i64, String>, String> {
    Ok(store
        .files()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|f| (f.id, f.path))
        .collect())
}

fn symbol_json(s: &SymbolRow, files: &HashMap<i64, String>) -> Value {
    json!({
        "uid": s.uid,
        "name": s.name,
        "qualified": s.qualified,
        "kind": s.kind.as_str(),
        "path": files.get(&s.file_id).cloned().unwrap_or_default(),
        "start_line": s.start_line,
        "end_line": s.end_line,
        "sig": s.sig,
    })
}

/// Public-API `GraphStore` has uid/name lookups only; edge rows carry raw
/// ids, so resolve them through the sanctioned `conn()` escape hatch.
fn symbol_by_id(store: &GraphStore, id: i64) -> Option<SymbolRow> {
    store
        .conn()
        .query_row(
            "SELECT id, uid, file_id, name, qualified, kind, start_line, end_line, sig
             FROM symbols WHERE id = ?1",
            rusqlite::params![id],
            |r| {
                Ok(SymbolRow {
                    id: r.get(0)?,
                    uid: r.get(1)?,
                    file_id: r.get(2)?,
                    name: r.get(3)?,
                    qualified: r.get(4)?,
                    kind: SymbolKind::parse(&r.get::<_, String>(5)?),
                    start_line: r.get(6)?,
                    end_line: r.get(7)?,
                    sig: r.get(8)?,
                })
            },
        )
        .ok()
}

#[expect(
    dead_code,
    reason = "retained for the full-detail graph response shape while context uses compact edges"
)]
fn edges_by_kind(
    store: &GraphStore,
    edges: &[EdgeRow],
    files: &HashMap<i64, String>,
    other_is_dst: bool,
) -> Result<Value, String> {
    let mut grouped: BTreeMap<&'static str, Vec<Value>> = BTreeMap::new();
    for e in edges {
        let other_id = if other_is_dst { e.dst_id } else { e.src_id };
        let other = symbol_by_id(store, other_id);
        grouped.entry(e.kind.as_str()).or_default().push(json!({
            "symbol": other.as_ref().map(|s| symbol_json(s, files)),
            "tier": e.tier.as_str(),
            "site_line": e.site_line,
        }));
    }
    Ok(serde_json::to_value(grouped).unwrap_or(Value::Null))
}

/// Compact edge representation for budgeted responses: just name, path, and
/// tier per edge, grouped by kind. Preserves relationship metadata without
/// the full symbol JSON (no sig, no uid, no line range).
fn compact_edges(
    store: &GraphStore,
    edges: &[EdgeRow],
    files: &HashMap<i64, String>,
    other_is_dst: bool,
    limit: usize,
) -> Result<Value, String> {
    let mut grouped: BTreeMap<&'static str, Vec<Value>> = BTreeMap::new();
    let mut seen = HashSet::new();
    for e in edges {
        if seen.len() >= limit {
            break;
        }
        let other_id = if other_is_dst { e.dst_id } else { e.src_id };
        let other = symbol_by_id(store, other_id);
        let entry = if let Some(s) = &other {
            let path = files.get(&s.file_id).cloned().unwrap_or_default();
            if !seen.insert((
                e.kind.as_str(),
                s.name.clone(),
                path.clone(),
                e.tier.as_str(),
            )) {
                continue;
            }
            json!({
                "name": s.name,
                "path": path,
                "tier": e.tier.as_str(),
            })
        } else {
            if !seen.insert((
                e.kind.as_str(),
                String::new(),
                String::new(),
                e.tier.as_str(),
            )) {
                continue;
            }
            json!({ "tier": e.tier.as_str() })
        };
        grouped.entry(e.kind.as_str()).or_default().push(entry);
    }
    Ok(serde_json::to_value(grouped).unwrap_or(Value::Null))
}

fn context_item(
    root: &Path,
    s: &SymbolRow,
    files: &HashMap<i64, String>,
    max_snippet_bytes: usize,
) -> bridge::Item {
    let path = files.get(&s.file_id).cloned().unwrap_or_default();
    let snippet = read_snippet(
        &root.join(&path),
        s.start_line,
        s.end_line,
        60,
        max_snippet_bytes,
    );
    bridge::Item {
        name: s.name.clone(),
        kind: s.kind.as_str().to_string(),
        path,
        start_line: s.start_line,
        end_line: s.end_line,
        sig: s.sig.clone(),
        snippet,
    }
}

fn read_snippet(
    abs: &Path,
    start_line: u32,
    end_line: u32,
    max_lines: usize,
    max_bytes: usize,
) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    let Ok(file) = open_regular_bounded(abs, MAX_FILE_BYTES) else {
        return String::new();
    };
    let start = start_line.saturating_sub(1) as usize;
    let mut snippet = String::new();
    for line in BufReader::new(file.take(MAX_FILE_BYTES.saturating_add(1)))
        .lines()
        .skip(start)
        .take(
            ((end_line as usize).saturating_sub(start))
                .min(max_lines)
                .max(1),
        )
        .filter_map(Result::ok)
    {
        let separator = usize::from(!snippet.is_empty());
        let remaining = max_bytes.saturating_sub(snippet.len() + separator);
        if remaining == 0 {
            break;
        }
        if separator == 1 {
            snippet.push('\n');
        }
        if line.len() <= remaining {
            snippet.push_str(&line);
            continue;
        }
        let mut end = remaining;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        snippet.push_str(&line[..end]);
        break;
    }
    snippet
}

fn merge_build_info(out: &mut Value, built: Option<Value>) {
    if let (Some(info), Some(obj)) = (built, out.as_object_mut()) {
        obj.insert("graph_build".into(), info);
    }
}

fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn remove_sqlite_sidecars(path: &Path) -> Result<(), String> {
    for sidecar in [sqlite_sidecar(path, "-wal"), sqlite_sidecar(path, "-shm")] {
        match std::fs::remove_file(&sidecar) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove {}: {error}", sidecar.display())),
        }
    }
    Ok(())
}

fn remove_sqlite_files(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("remove {}: {error}", path.display())),
    }
    remove_sqlite_sidecars(path)
}

// ---------------------------------------------------------------------------
// bridge — the ONLY place that calls concurrently-developed crate APIs.
// Each fn is one call deep so contract drift is a one-line fix.
// ---------------------------------------------------------------------------

mod bridge {
    use super::{Value, es, to_val};
    use pixel_graph::GraphStore;
    use std::path::Path;

    /// Neutral mirror of `pixel_context::ContextItem`.
    pub struct Item {
        pub name: String,
        pub kind: String,
        pub path: String,
        pub start_line: u32,
        pub end_line: u32,
        pub sig: String,
        pub snippet: String,
    }

    pub fn build_graph(root: &Path, db: &Path) -> Result<Value, String> {
        let s = pixel_graph::build::build_graph(root, db).map_err(es)?;
        Ok(serde_json::json!({
            "files": s.files,
            "symbols": s.symbols,
            "edges": s.edges,
            "unresolved": s.unresolved,
            "elapsed_ms": s.elapsed_ms as u64,
        }))
    }

    /// True iff the on-disk graph is fresh relative to `root`'s working tree.
    pub fn is_fresh(root: &Path, db: &Path) -> bool {
        pixel_graph::build::is_fresh(root, db)
    }

    pub fn update_file(root: &Path, db: &Path, rel: &str) {
        let _ = pixel_graph::build::update_file(root, db, rel);
    }

    pub fn impact(
        store: &GraphStore,
        uid: &str,
        direction: &str,
        depth: u32,
    ) -> Result<Value, String> {
        use pixel_graph::impact::{Direction, impact};
        let dir = if direction == "downstream" {
            Direction::Downstream
        } else {
            Direction::Upstream
        };
        impact(store, uid, dir, depth, 50).map(to_val).map_err(es)
    }

    pub fn trace(store: &GraphStore, from_uid: &str, to_uid: &str) -> Result<Value, String> {
        pixel_graph::trace::trace(store, from_uid, to_uid, 8)
            .map(to_val)
            .map_err(es)
    }

    pub fn processes(store: &mut GraphStore, offset: usize) -> Result<Value, String> {
        use pixel_graph::process;
        const PROCESS_LIMIT: usize = 5;
        const STEP_LIMIT: usize = 10;
        let (listed, persisted_total) =
            process::list(store, PROCESS_LIMIT, STEP_LIMIT, offset).map_err(es)?;
        let (mut v, total_processes) = if persisted_total == 0 {
            const DISCOVERY_LIMIT: usize = 100;
            let discovered = process::discover(store, 6, 3, 3, DISCOVERY_LIMIT).map_err(es)?;
            let total = discovered.len();
            (
                discovered
                    .into_iter()
                    .skip(offset)
                    .take(PROCESS_LIMIT)
                    .collect(),
                total,
            )
        } else {
            (listed, persisted_total)
        };
        v.truncate(PROCESS_LIMIT);
        let mut steps_truncated = v
            .iter()
            .any(|summary| summary.step_count as usize > STEP_LIMIT);
        for summary in &mut v {
            if summary.steps.len() > STEP_LIMIT {
                summary.steps.truncate(STEP_LIMIT);
                steps_truncated = true;
            }
        }
        let returned_processes = v.len();
        let has_more = offset.saturating_add(returned_processes) < total_processes;
        Ok(serde_json::json!({
            "processes": to_val(v),
            "total_processes": total_processes,
            "returned_processes": returned_processes,
            "process_limit": PROCESS_LIMIT,
            "step_limit": STEP_LIMIT,
            "offset": offset,
            "next_offset": has_more.then_some(offset.saturating_add(returned_processes)),
            "truncated": has_more || steps_truncated,
        }))
    }

    pub fn clusters(store: &mut GraphStore, offset: usize) -> Result<Value, String> {
        use pixel_graph::cluster;
        const CLUSTER_LIMIT: usize = 50;
        let (listed, persisted_total) = cluster::list(store, CLUSTER_LIMIT, offset).map_err(es)?;
        let (mut v, total_clusters) = if persisted_total == 0 {
            let computed = cluster::compute(store).map_err(es)?;
            let total = computed.len();
            (
                computed
                    .into_iter()
                    .skip(offset)
                    .take(CLUSTER_LIMIT)
                    .collect(),
                total,
            )
        } else {
            (listed, persisted_total)
        };
        v.truncate(CLUSTER_LIMIT);
        let returned_clusters = v.len();
        let has_more = offset.saturating_add(returned_clusters) < total_clusters;
        Ok(serde_json::json!({
            "clusters": to_val(v),
            "total_clusters": total_clusters,
            "returned_clusters": returned_clusters,
            "cluster_limit": CLUSTER_LIMIT,
            "offset": offset,
            "next_offset": has_more.then_some(offset.saturating_add(returned_clusters)),
            "truncated": has_more,
        }))
    }

    pub fn changes(store: &GraphStore, root: &Path, base: Option<&str>) -> Result<Value, String> {
        pixel_graph::changes::detect(store, root, base)
            .map(to_val)
            .map_err(es)
    }

    /// Budget-fitted text rendering via pixel-context; empty on any miss.
    pub fn render_context(items: &[Item], budget_tokens: usize) -> (String, &'static str, usize) {
        use pixel_context::{ContextItem, Layer, fit_to_budget_detailed, render};
        let mapped: Vec<ContextItem> = items
            .iter()
            .map(|i| ContextItem {
                name: i.name.clone(),
                kind: i.kind.clone(),
                path: i.path.clone(),
                start_line: i.start_line,
                end_line: i.end_line,
                sig: i.sig.clone(),
                snippet: i.snippet.clone(),
            })
            .collect();
        if let Some((target, neighbors)) = mapped.split_first() {
            let target_text = render(std::slice::from_ref(target), Layer::L2);
            let target_tokens = pixel_context::estimate_tokens(&target_text);
            if target_tokens <= budget_tokens {
                let neighbors_fit =
                    fit_to_budget_detailed(neighbors, budget_tokens - target_tokens, Layer::L1);
                let layer = if neighbors_fit.text.is_empty() {
                    "L2"
                } else if neighbors_fit.layer == Layer::L1 {
                    "L2+L1"
                } else {
                    "L2+L0"
                };
                return (
                    format!("{target_text}{}", neighbors_fit.text),
                    layer,
                    neighbors_fit.elided_items,
                );
            }
        }
        let fitted = fit_to_budget_detailed(&mapped, budget_tokens, Layer::L2);
        (fitted.text, fitted.layer.as_str(), fitted.elided_items)
    }
}

fn to_val<T: Serialize>(t: T) -> Value {
    serde_json::to_value(t).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// ranked search — `--scope code` reranking via pixel-rank's RRF
// ---------------------------------------------------------------------------

/// Rerank search matches by file-level signals WITHOUT changing the hit set.
///
/// The hit set (set of (path, line) pairs from the index) is preserved
/// exactly — only the order changes. Per PLAN.md's M1 gate: "identical hit
/// sets; order may differ deliberately due to ranking."
///
/// Signals (same RRF family as `targets`, K=60):
/// - **Filename**: a word of the search pattern appears in the file's
///   basename (per-word via `split_ident_words`, so "gain ledger" matches
///   `ledger.ts`). Weight 3.0 (matches `targets`'s filename signal).
/// - **Symbol**: the search pattern matches a symbol name in that file
///   (via the graph, if available). Weight 2.5.
/// - **Content density**: files with more matches rank higher. Weight 1.5.
///
/// Files are ranked by fused RRF score; within a file, matches keep their
/// original line-number order (stable, deterministic). Graph failure
/// degrades to filename + content density only (same graceful-degradation
/// pattern as `op_targets`).
fn rank_search_matches(
    matches: &[pixel_index::verify::MatchLine],
    pattern: &str,
    graph: &Option<GraphStore>,
) -> Vec<pixel_index::verify::MatchLine> {
    use std::collections::BTreeMap;

    // Group matches by file, preserving within-file line order.
    let mut by_file: BTreeMap<String, Vec<pixel_index::verify::MatchLine>> = BTreeMap::new();
    for m in matches {
        by_file.entry(m.path.clone()).or_default().push(m.clone());
    }
    let files: Vec<String> = by_file.keys().cloned().collect();
    if files.is_empty() {
        return Vec::new();
    }

    // --- Signal 1: per-word filename match ---
    // Split the pattern into identifier words; a file whose basename contains
    // any of those words ranks by how many distinct words match. This fixes
    // S1: "gain ledger" now matches `ledger.ts` (the word "ledger" is a
    // basename component), whereas whole-pattern basename containment
    // (`basename.contains("gain ledger")`) matched nothing. Within a tier, a
    // shorter/more-specific basename outranks a longer one (Bug 4: previously
    // sorted by length DESCENDING, so the longest matching filename won).
    let words: Vec<String> = pixel_graph::split_ident_words(pattern)
        .into_iter()
        .map(|w| w.to_lowercase())
        .collect();
    let mut filename_rank: Vec<(String, usize)> = files
        .iter()
        .filter_map(|f| {
            let basename = std::path::Path::new(f)
                .file_name()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let matched = words
                .iter()
                .filter(|w| !w.is_empty() && basename.contains(w.as_str()))
                .count();
            if matched > 0 {
                Some((f.clone(), matched))
            } else {
                None
            }
        })
        .collect();
    filename_rank.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| {
                std::path::Path::new(&a.0)
                    .file_name()
                    .map(|s| s.to_string_lossy().len())
                    .unwrap_or(0)
                    .cmp(
                        &std::path::Path::new(&b.0)
                            .file_name()
                            .map(|s| s.to_string_lossy().len())
                            .unwrap_or(0),
                    )
            })
            .then_with(|| a.0.cmp(&b.0))
    });

    // --- Signal 2: symbol match (graph, if available) ---
    let pat_lower = pattern.to_lowercase();
    let symbol_rank: Vec<String> = if let Some(store) = graph {
        // For each file, check if any symbol name contains the pattern.
        let mut hits: Vec<(String, usize)> = files
            .iter()
            .filter_map(|f| {
                let file = store.file_by_path(f).ok().flatten()?;
                let syms = store.symbols_in_file(file.id).ok()?;
                let count = syms
                    .iter()
                    .filter(|s| {
                        let name_lc = s.name.to_lowercase();
                        name_lc.contains(&pat_lower)
                    })
                    .count();
                if count > 0 { Some((f.clone(), count)) } else { None }
            })
            .collect();
        hits.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        hits.into_iter().map(|(f, _)| f).collect()
    } else {
        Vec::new()
    };

    // --- Signal 3: content density (match count per file) ---
    let mut density_rank: Vec<(String, usize)> = files
        .iter()
        .map(|f| (f.clone(), by_file.get(f).map(|v| v.len()).unwrap_or(0)))
        .collect();
    density_rank.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // --- RRF fusion (K=60, same as targets) ---
    // Phase 1c: use the shared `pixel_rank::rrf_fuse` primitive instead of a
    // third independent reimplementation of weighted RRF. The weights are
    // pixel-rank's own pub constants, so a drift here can no longer silently
    // desync search ranking from `targets` ranking.
    let s1: Vec<String> = filename_rank.iter().map(|(f, _)| f.clone()).collect();
    let s2: Vec<String> = symbol_rank;
    let s3: Vec<String> = density_rank.iter().map(|(f, _)| f.clone()).collect();

    let mut file_order = pixel_rank::rrf_fuse(
        &[
            (&s1, pixel_rank::W_FILENAME),
            (&s2, pixel_rank::W_SYMBOL),
            (&s3, pixel_rank::W_CONTENT),
        ],
        pixel_rank::RRF_K,
    );

    // Preserve the full hit set: files with no signal still appear (score
    // 0.0), sorted by path — matching the pre-fusion behavior where every
    // file in the pool was emitted.
    let fused_set: HashSet<&str> = file_order.iter().map(|(p, _)| p.as_str()).collect();
    let mut rest: Vec<String> = files
        .iter()
        .filter(|f| !fused_set.contains(f.as_str()))
        .cloned()
        .collect();
    rest.sort();
    for f in rest {
        file_order.push((f, 0.0));
    }

    // Emit matches in file order, preserving within-file line order.
    let mut out: Vec<pixel_index::verify::MatchLine> = Vec::with_capacity(matches.len());
    for (f, _) in file_order {
        if let Some(file_matches) = by_file.remove(&f) {
            out.extend(file_matches);
        }
    }
    out
}

fn es<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pixel-daemon-{tag}-{}-{}",
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
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {:?}", out);
    }

    #[test]
    fn ping_reports_daemon_protocol_version() {
        let root = tmpdir("protocol-version");
        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Ping);
        assert!(resp.ok);
        assert_eq!(
            resp.data().get("protocol_version").and_then(Value::as_u64),
            Some(super::PROTOCOL_VERSION)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn uses_pages_cover_all_edges_without_overlap() {
        let root = tmpdir("uses-pages");
        let mut source = String::from("export function target(): number { return 1 }\n");
        for index in 0..25 {
            source.push_str(&format!(
                "export function caller{index:02}(): number {{ return target() }}\n"
            ));
        }
        std::fs::write(root.join("calls.ts"), source).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "calls"]);

        let mut svc = Service::open(&root).unwrap();
        let first = svc.handle(Request::Uses {
            uid_or_name: "target".into(),
            role: "callers".into(),
            offset: Some(0),
        });
        let second = svc.handle(Request::Uses {
            uid_or_name: "target".into(),
            role: "callers".into(),
            offset: Some(20),
        });
        assert!(first.ok && second.ok);
        let edge_uids = |response: &Response| {
            response.data()["edges"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|edge| edge["symbol"]["uid"].as_str().map(str::to_string))
                .collect::<std::collections::HashSet<_>>()
        };
        let first_uids = edge_uids(&first);
        let second_uids = edge_uids(&second);
        assert_eq!(first_uids.len(), 20);
        assert_eq!(second_uids.len(), 5);
        assert!(first_uids.is_disjoint(&second_uids));
        assert_eq!(first.data()["next_offset"].as_u64(), Some(20));
        assert!(second.data()["next_offset"].is_null());
        assert_eq!(second.data()["total_edges"].as_u64(), Some(25));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn changes_pages_cover_all_symbols_without_overlap() {
        let root = tmpdir("changes-pages");
        let make_source = |increment: usize| {
            (0..25)
                .map(|index| {
                    format!(
                        "export function changed{index:02}(x: number): number {{ return x + {increment} }}\n"
                    )
                })
                .collect::<String>()
        };
        std::fs::write(root.join("changed.ts"), make_source(1)).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "baseline"]);
        std::fs::write(root.join("changed.ts"), make_source(2)).unwrap();

        let mut svc = Service::open(&root).unwrap();
        let first = svc.handle(Request::Changes {
            base: None,
            offset: Some(0),
        });
        let second = svc.handle(Request::Changes {
            base: None,
            offset: Some(20),
        });
        assert!(first.ok && second.ok, "first={first:?} second={second:?}");
        let symbol_uids = |response: &Response| {
            response.data()["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|symbol| symbol["uid"].as_str().map(str::to_string))
                .collect::<std::collections::HashSet<_>>()
        };
        let first_uids = symbol_uids(&first);
        let second_uids = symbol_uids(&second);
        assert_eq!(first_uids.len(), 20);
        assert_eq!(second_uids.len(), 5);
        assert!(first_uids.is_disjoint(&second_uids));
        assert_eq!(first.data()["next_offset"].as_u64(), Some(20));
        assert!(second.data()["next_offset"].is_null());
        assert_eq!(second.data()["symbols_total"].as_u64(), Some(25));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: a `Context` request with a small token budget must produce
    /// a response whose total serialized size is bounded by the budget (in
    /// tokens), not just the inner `text` field. Previously `--budget 50`
    /// could emit thousands of bytes because the structured incoming/outgoing
    /// sections were not counted.
    #[test]
    fn context_budget_covers_whole_response() {
        let root = tmpdir("ctx-budget");
        std::fs::write(
            root.join("a.ts"),
            "export function alpha(x: number): number { return x + 1 }\n\
             export function beta(x: number): number { return alpha(x) }\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        // Find the uid for `alpha`.
        let sym = svc.handle(Request::Symbol {
            name: "alpha".into(),
        });
        assert!(sym.ok, "symbol lookup: {:?}", sym);
        let uid = sym
            .data()
            .get("symbols")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|v| v.get("uid"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("no uid in {sym:?}"))
            .to_string();

        // Request a tiny budget. The whole response must be bounded.
        let resp = svc.handle(Request::Context {
            uid: uid.clone(),
            budget_tokens: Some(50),
        });
        assert!(resp.ok, "context: {:?}", resp);
        let serialized = serde_json::to_string(resp.data()).unwrap();
        let tokens = pixel_context::estimate_tokens(&serialized);
        assert!(
            tokens <= 50,
            "whole-response budget exceeded: {tokens} tokens for budget 50 ({} bytes)",
            serialized.len()
        );
        // Text must be empty or very small when budget < overhead.
        let text = resp.data().get("text").and_then(Value::as_str).unwrap_or("");
        assert!(
            text.is_empty() || pixel_context::estimate_tokens(text) <= 50,
            "text should be empty or tiny when budget is 50, got {} tokens",
            pixel_context::estimate_tokens(text)
        );
        // budgeted flag must be set so callers know the cap applied.
        assert_eq!(
            resp.data().get("budgeted").and_then(Value::as_bool),
            Some(true),
            "budgeted flag must be true when a budget is set"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: a `Context` request with a moderate budget must produce a
    /// response whose total serialized size (including metadata + text) does
    /// not exceed the budget by more than a small rounding factor.
    #[test]
    fn context_moderate_budget_covers_whole_response() {
        let root = tmpdir("ctx-budget-moderate");
        std::fs::write(
            root.join("a.ts"),
            "export function alpha(x: number): number { return x + 1 }\n\
             export function beta(x: number): number { return alpha(x) }\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let sym = svc.handle(Request::Symbol {
            name: "alpha".into(),
        });
        let uid = sym
            .data()
            .get("symbols")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|v| v.get("uid"))
            .and_then(Value::as_str)
            .unwrap()
            .to_string();

        // Budget 500: metadata and text together must fit the hard limit.
        let resp = svc.handle(Request::Context {
            uid: uid.clone(),
            budget_tokens: Some(500),
        });
        assert!(resp.ok, "context: {:?}", resp);
        let serialized = serde_json::to_string(resp.data()).unwrap();
        let tokens = pixel_context::estimate_tokens(&serialized);
        assert!(
            tokens <= 500,
            "whole-response budget exceeded: {tokens} tokens for budget 500 ({} bytes)",
            serialized.len()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: a broad search (`.*` or a short literal hitting every file)
    /// must be bounded by a default row limit and a byte cap, not return
    /// unbounded output.
    #[test]
    fn search_broad_pattern_is_bounded() {
        let root = tmpdir("search-bound");
        git(&root, &["init", "-q"]);
        // 120 files, each with a common needle, so the default 100-row page
        // is exercised rather than merely reported.
        for i in 0..120 {
            std::fs::write(
                root.join(format!("f{i:03}.rs")),
                format!("fn commonBroadNeedle{i:03}() {{}}\n"),
            )
            .unwrap();
        }
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "many"]);

        let mut svc = Service::open(&root).unwrap();
        // Broad pattern with no explicit limit: default limit applies.
        let resp = svc.handle(Request::Search {
            paths: None,
            pattern: "commonBroadNeedle".into(),
            json: true,
            limit: None,
            offset: None,
            scope: None,
        });
        assert!(resp.ok, "search: {:?}", resp);
        let matches = resp.data().get("matches").and_then(Value::as_array).unwrap();
        // Default limit is 100; 120 matching files must return one full page
        // with an exact continuation offset.
        assert_eq!(
            resp.data().get("limit").and_then(Value::as_u64),
            Some(100),
            "default limit must be reported"
        );
        assert_eq!(matches.len(), 100, "default limit must cap matches");
        assert_eq!(
            resp.data().get("next_offset").and_then(Value::as_u64),
            Some(100)
        );
        // Now request a tiny limit: must truncate.
        let resp = svc.handle(Request::Search {
            paths: None,
            pattern: "commonBroadNeedle".into(),
            json: true,
            limit: Some(5),
            offset: None,
            scope: None,
        });
        assert!(resp.ok);
        let matches = resp.data().get("matches").and_then(Value::as_array).unwrap();
        let truncated = resp
            .data()
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        assert!(matches.len() <= 5, "explicit limit must cap matches");
        assert!(truncated, "truncated must be true when more matches exist");
        let first_page = matches.clone();

        let resp = svc.handle(Request::Search {
            paths: None,
            pattern: "commonBroadNeedle".into(),
            json: true,
            limit: Some(5),
            offset: Some(5),
            scope: None,
        });
        assert!(resp.ok);
        let second_page = resp.data().get("matches").and_then(Value::as_array).unwrap();
        assert_eq!(second_page.len(), 5);
        assert!(
            first_page.iter().all(|item| !second_page.contains(item)),
            "offset page must not repeat prior matches"
        );
        assert_eq!(resp.data().get("offset").and_then(Value::as_u64), Some(5));
        assert_eq!(
            resp.data().get("next_offset").and_then(Value::as_u64),
            Some(10)
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `search --scope code` must preserve the hit set (same (path, line)
    /// pairs as unranked) while reranking by file-level signals. A file
    /// whose basename matches the pattern should rank ahead of a file with
    /// the same match count but no filename/symbol signal.
    #[test]
    fn search_scope_code_preserves_hit_set_and_reranks() {
        let root = tmpdir("search-scope-code");
        // login.rs: basename matches "login", defines `login` symbol.
        std::fs::write(
            root.join("login.rs"),
            "pub fn login(user: &str) -> bool { !user.is_empty() }\n",
        )
        .unwrap();
        // caller.rs: contains "login" in content but not in filename/symbol.
        std::fs::write(
            root.join("caller.rs"),
            "use crate::login::login;\npub fn go() { login(\"a\"); }\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();

        // Unranked: path/line order → caller.rs before login.rs (alphabetical).
        let unranked = svc.handle(Request::Search {
            paths: None,
            pattern: "login".into(),
            json: true,
            limit: Some(50),
            offset: None,
            scope: None,
        });
        assert!(unranked.ok, "unranked: {:?}", unranked.error);
        assert_eq!(
            unranked.data().get("ranked").and_then(Value::as_bool),
            Some(false)
        );
        let unranked_matches = unranked.data().get("matches").and_then(Value::as_array).unwrap();
        let unranked_set: std::collections::HashSet<(String, u64)> = unranked_matches
            .iter()
            .map(|m| {
                (
                    m["path"].as_str().unwrap().to_string(),
                    m["line"].as_u64().unwrap(),
                )
            })
            .collect();

        // Ranked: same hit set, but login.rs should rank first (filename +
        // symbol signal), ahead of caller.rs.
        let ranked = svc.handle(Request::Search {
            paths: None,
            pattern: "login".into(),
            json: true,
            limit: Some(50),
            offset: None,
            scope: Some("code".into()),
        });
        assert!(ranked.ok, "ranked: {:?}", ranked.error);
        assert_eq!(
            ranked.data().get("ranked").and_then(Value::as_bool),
            Some(true)
        );
        let ranked_matches = ranked.data().get("matches").and_then(Value::as_array).unwrap();
        let ranked_set: std::collections::HashSet<(String, u64)> = ranked_matches
            .iter()
            .map(|m| {
                (
                    m["path"].as_str().unwrap().to_string(),
                    m["line"].as_u64().unwrap(),
                )
            })
            .collect();

        // Hit set must be identical (M1 parity gate).
        assert_eq!(
            unranked_set, ranked_set,
            "ranked search must preserve the hit set exactly"
        );

        // login.rs must rank first (filename + symbol signal beats content-only).
        let first_path = ranked_matches
            .first()
            .and_then(|m| m["path"].as_str())
            .unwrap_or("");
        assert_eq!(
            first_path, "login.rs",
            "filename+symbol signal must rank login.rs first, got {first_path}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// S1 fix: per-word filename scoring — "gain ledger" must match
    /// `ledger.ts` (the word "ledger" is a basename component), which
    /// whole-pattern basename containment (`basename.contains("gain ledger")`)
    /// never matched.
    #[test]
    fn rank_search_matches_scores_filename_per_word() {
        use pixel_index::verify::MatchLine;
        let matches = vec![
            MatchLine {
                path: "src/other.ts".into(),
                line_number: 1,
                line: "gain".into(),
            },
            MatchLine {
                path: "src/ledger.ts".into(),
                line_number: 1,
                line: "ledger".into(),
            },
        ];
        let ranked = rank_search_matches(&matches, "gain ledger", &None);
        assert_eq!(ranked[0].path, "src/ledger.ts");
        assert_eq!(ranked[1].path, "src/other.ts");
    }

    /// Bug 1a + Bug 4 regression: ranked search must consider the FULL
    /// bounded candidate pool, not just a path-order-sliced page, so a
    /// filename-signal match that sorts after every other candidate in
    /// plain path order still surfaces at a small `--limit`. Also pins the
    /// exact-basename-stem > longer-substring tie-break (Bug 4): before the
    /// fix, `bb.len().cmp(&ba.len())` sorted the LONGEST matching filename
    /// first, so `zzz_needle.rs` would have outranked `needle.rs`.
    #[test]
    fn search_scope_code_ranks_globally_beyond_small_page_and_prefers_exact_filename() {
        let root = tmpdir("search-scope-global-rank");
        for i in 0..30 {
            std::fs::write(
                root.join(format!("f{i:02}.rs")),
                format!("// needle mention {i}\n"),
            )
            .unwrap();
        }
        // Both sort AFTER all 30 `f*.rs` files alphabetically, so a
        // path-order bounded probe at `--limit 3` never even reaches them
        // pre-fix (candidates are visited in sorted path order and the
        // probe stops as soon as `limit + 1` matches are found among the
        // `f*.rs` files alone).
        std::fs::write(root.join("needle.rs"), "pub fn other() { /* needle */ }\n").unwrap();
        std::fs::write(
            root.join("zzz_needle.rs"),
            "pub fn another() { /* needle */ }\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Search {
            paths: None,
            pattern: "needle".into(),
            json: true,
            limit: Some(3),
            offset: None,
            scope: Some("code".into()),
        });
        assert!(resp.ok, "ranked search: {:?}", resp.error);
        let matches = resp.data().get("matches").and_then(Value::as_array).unwrap();
        let paths: Vec<&str> = matches.iter().filter_map(|m| m["path"].as_str()).collect();
        assert_eq!(
            paths.len(),
            3,
            "expected a full page of 3 ranked matches, got {paths:?}"
        );
        assert_eq!(
            &paths[..2],
            &["needle.rs", "zzz_needle.rs"],
            "exact filename match must surface first, substring match second, despite \
             both sorting after every `f*.rs` file in plain path order; got {paths:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Bug 1b regression: walking every ranked page via `next_offset` must
    /// cover every true match exactly once — no duplicates (previously
    /// possible when a byte/row-limit page boundary disagreed with a rank
    /// reordering computed only within that already-sliced page) and no
    /// gaps.
    #[test]
    fn search_scope_code_pagination_has_no_duplicates_or_gaps() {
        let root = tmpdir("search-scope-pagination");
        const TOTAL: usize = 37;
        for i in 0..TOTAL {
            std::fs::write(
                root.join(format!("file{i:03}.rs")),
                format!("// banana occurrence {i}\n"),
            )
            .unwrap();
        }
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let mut seen: Vec<String> = Vec::new();
        let mut offset = Some(0usize);
        let mut pages = 0;
        while let Some(o) = offset {
            let resp = svc.handle(Request::Search {
                paths: None,
                pattern: "banana".into(),
                json: true,
                limit: Some(5),
                offset: Some(o),
                scope: Some("code".into()),
            });
            assert!(resp.ok, "page at offset {o}: {:?}", resp.error);
            let matches = resp.data().get("matches").and_then(Value::as_array).unwrap();
            for m in matches {
                seen.push(m["path"].as_str().unwrap().to_string());
            }
            offset = resp
                .data()
                .get("next_offset")
                .and_then(Value::as_u64)
                .map(|v| v as usize);
            pages += 1;
            assert!(pages <= TOTAL, "pagination did not terminate: seen={seen:?}");
        }

        let unique: std::collections::HashSet<&String> = seen.iter().collect();
        assert_eq!(
            seen.len(),
            unique.len(),
            "ranked pagination must not repeat a row: {seen:?}"
        );
        assert_eq!(
            unique.len(),
            TOTAL,
            "ranked pagination must cover every match exactly once: got {} of {TOTAL}: {seen:?}",
            unique.len()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Bug 2 regression: the symbol-signal must be deterministic based on
    /// "does graph.db exist on disk", never on whether some unrelated op
    /// happened to warm `self.graph` earlier in this same daemon process.
    #[test]
    fn search_scope_code_ranking_is_deterministic_regardless_of_prior_daemon_activity() {
        let root = tmpdir("search-scope-determinism");
        // Defines a symbol literally named `needle` -- its rank depends
        // entirely on the graph symbol signal (no filename hit).
        std::fs::write(
            root.join("b_defines.rs"),
            "pub fn needle() -> bool { true }\n",
        )
        .unwrap();
        // Sorts first alphabetically; mentions "needle" once in a comment
        // -- same content-density score as the line inside `b_defines.rs`,
        // no symbol, no filename signal.
        std::fs::write(root.join("a_mentions.rs"), "// needle mentioned here\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        // Build graph.db via a throwaway Service so THIS test's Service
        // starts with `graph: None` in memory while graph.db already
        // exists on disk -- the `--no-daemon` / daemon-restart scenario
        // Bug 2 describes.
        {
            let mut builder = Service::open(&root).unwrap();
            let built = builder.handle(Request::Graph {});
            assert!(built.ok, "graph build: {:?}", built.error);
        }

        let mut svc = Service::open(&root).unwrap();
        let search = |svc: &mut Service| -> Vec<String> {
            let resp = svc.handle(Request::Search {
                paths: None,
                pattern: "needle".into(),
                json: true,
                limit: Some(10),
                offset: None,
                scope: Some("code".into()),
            });
            assert!(resp.ok, "search: {:?}", resp.error);
            resp.data()
                .get("matches")
                .and_then(Value::as_array)
                .unwrap()
                .iter()
                .map(|m| m["path"].as_str().unwrap().to_string())
                .collect()
        };

        // Call 1: `svc.graph` is still `None` in memory; only graph.db on
        // disk. The symbol signal must already apply.
        let first = search(&mut svc);
        assert_eq!(
            first.first().map(String::as_str),
            Some("b_defines.rs"),
            "symbol signal must apply from an on-disk graph.db even with no prior \
             in-process graph activity on this Service, got {first:?}"
        );

        // Unrelated daemon activity that happens to populate `self.graph`.
        let targets = svc.handle(Request::Targets {
            task: "needle".into(),
            limit: Some(5),
        });
        assert!(targets.ok, "targets: {:?}", targets.error);

        // Call 2: identical repo, identical query -- must be byte-for-byte
        // the same ranking as call 1, regardless of the intervening
        // `targets` call.
        let second = search(&mut svc);
        assert_eq!(
            first, second,
            "ranking must not depend on unrelated prior daemon activity"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Bug 5 regression: an unrecognized `scope` value must be a clear
    /// error, not a silent fallback to unranked search. Valid values stay
    /// case-insensitive.
    #[test]
    fn search_rejects_unknown_scope_instead_of_silently_falling_back() {
        let root = tmpdir("search-bad-scope");
        std::fs::write(root.join("a.rs"), "// needle\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Search {
            paths: None,
            pattern: "needle".into(),
            json: true,
            limit: Some(5),
            offset: None,
            scope: Some("banana".into()),
        });
        assert!(!resp.ok, "unknown scope must fail, not silently succeed");
        assert_eq!(
            resp.error.as_ref().map(|e| e.code),
            Some(pixel_proto::ErrorCode::InvalidInput)
        );

        let unranked = svc.handle(Request::Search {
            paths: None,
            pattern: "needle".into(),
            json: true,
            limit: Some(5),
            offset: None,
            scope: None,
        });
        assert!(unranked.ok, "no scope must remain valid: {:?}", unranked.error);

        let upper = svc.handle(Request::Search {
            paths: None,
            pattern: "needle".into(),
            json: true,
            limit: Some(5),
            offset: None,
            scope: Some("CODE".into()),
        });
        assert!(upper.ok, "scope must be case-insensitive: {:?}", upper.error);
        assert_eq!(
            upper.data().get("ranked").and_then(Value::as_bool),
            Some(true)
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Phase 3 item 1: `op_targets` must attach per-file content evidence
    /// (first ~2 match lines per keyword) so a caller can verify a target's
    /// content match without re-searching (S2 distrust loop).
    #[test]
    fn targets_attach_content_evidence() {
        let root = tmpdir("targets-evidence");
        std::fs::write(
            root.join("ledger.rs"),
            "// gain ledger entry\npub fn ledger() {}\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Targets {
            task: "gain ledger".into(),
            limit: Some(5),
        });
        assert!(resp.ok, "targets: {:?}", resp.error);
        let targets = resp.data().get("targets").and_then(Value::as_array).unwrap();
        let ledger = targets
            .iter()
            .find(|t| t["path"].as_str() == Some("ledger.rs"))
            .expect("ledger.rs should be a target");
        let evidence = ledger.get("evidence").and_then(Value::as_array).unwrap();
        assert!(!evidence.is_empty(), "expected content evidence on ledger.rs");
        assert!(
            evidence.iter().any(|e| {
                e["keyword"].as_str() == Some("ledger")
                    && e["text"].as_str().map_or(false, |t| t.contains("ledger"))
            }),
            "expected a 'ledger' evidence entry, got {evidence:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// S1 fix: ranked search must match a file whose basename contains a
    /// single word of a multi-word pattern ("gain ledger" → `ledger.ts`),
    /// not require the whole phrase to be a basename substring. Without the
    /// per-word filename signal, `ledger.ts` and `zzz_other.ts` (identical
    /// content) would tie on content density and sort by path — `zzz_other.ts`
    /// first. With it, `ledger.ts` ranks first on the filename word.
    #[test]
    fn search_scope_code_matches_per_word_filename() {
        let root = tmpdir("search-per-word-filename");
        std::fs::write(root.join("ledger.ts"), "// gain ledger here\n").unwrap();
        std::fs::write(root.join("zzz_other.ts"), "// gain ledger here\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Search {
            paths: None,
            pattern: "gain ledger".into(),
            json: true,
            limit: Some(10),
            offset: None,
            scope: Some("code".into()),
        });
        assert!(resp.ok, "ranked search: {:?}", resp.error);
        let matches = resp.data().get("matches").and_then(Value::as_array).unwrap();
        let paths: Vec<&str> = matches.iter().filter_map(|m| m["path"].as_str()).collect();
        assert_eq!(
            paths.first().copied(),
            Some("ledger.ts"),
            "per-word filename signal must rank ledger.ts first, got {paths:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Phase 3 item 1 — the epistemics choke point: EVERY retrieval-class
    /// op's successful response must carry an `epistemics` object and a
    /// repo `snapshot`. This is the mechanical walk over `RETRIEVAL_OPS`
    /// that makes shipping a retrieval answer without epistemics a test
    /// failure, given the choke-point enforcement in `Service::handle`.
    #[test]
    fn every_retrieval_op_response_carries_epistemics_and_snapshot() {
        let root = tmpdir("epistemics-walk");
        std::fs::write(
            root.join("a.ts"),
            "export function alpha(x: number): number { return x + 1 }\n\
             export function beta(x: number): number { return alpha(x) }\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);
        // Dirty edit so `changes` has something to report.
        std::fs::write(
            root.join("a.ts"),
            "export function alpha(x: number): number { return x + 2 }\n\
             export function beta(x: number): number { return alpha(x) }\n",
        )
        .unwrap();

        let mut svc = Service::open(&root).unwrap();
        let uid = {
            let sym = svc.handle(Request::Symbol { name: "alpha".into() });
            sym.data()["symbols"][0]["uid"].as_str().unwrap().to_string()
        };

        let requests: Vec<(& str, Request)> = vec![
            ("search", Request::Search {
                pattern: "alpha".into(),
                json: true,
                limit: Some(10),
                offset: None,
                paths: None,
                scope: None,
            }),
            ("resolve", Request::Resolve { phrase: "alpha".into(), limit: Some(5) }),
            ("targets", Request::Targets { task: "alpha beta".into(), limit: Some(5) }),
            ("impact", Request::Impact {
                uid_or_name: "alpha".into(),
                direction: "upstream".into(),
                depth: Some(2),
            }),
            ("uses", Request::Uses {
                uid_or_name: "alpha".into(),
                role: "callers".into(),
                offset: None,
            }),
            ("trace", Request::Trace { from: "beta".into(), to: "alpha".into() }),
            ("changes", Request::Changes { base: None, offset: None }),
            ("context", Request::Context { uid, budget_tokens: Some(2000) }),
            ("symbol", Request::Symbol { name: "alpha".into() }),
            ("processes", Request::Processes { offset: None }),
            ("clusters", Request::Clusters { offset: None }),
        ];

        // The walk itself must cover the registry exactly — a new retrieval
        // op added to RETRIEVAL_OPS without a row here fails loudly.
        let walked: std::collections::HashSet<&str> =
            requests.iter().map(|(name, _)| *name).collect();
        for op in super::RETRIEVAL_OPS {
            assert!(walked.contains(op), "RETRIEVAL_OPS entry {op:?} not exercised by this test");
        }

        for (name, req) in requests {
            assert_eq!(req.op_name(), name, "walk row mislabeled");
            let resp = svc.handle(req);
            assert!(resp.ok, "{name}: {:?}", resp.error);
            let epistemics = resp
                .epistemics
                .as_ref()
                .unwrap_or_else(|| panic!("{name}: retrieval response shipped WITHOUT epistemics"));
            assert!(
                !epistemics.basis.is_empty(),
                "{name}: epistemics.basis must name the answer's source"
            );
            // A response claiming closed_world must not simultaneously admit
            // a lower bound, and vice versa.
            assert_ne!(
                epistemics.closed_world, epistemics.lower_bound,
                "{name}: closed_world and lower_bound must be complementary here: {epistemics:?}"
            );
            let snapshot = resp
                .snapshot
                .as_ref()
                .unwrap_or_else(|| panic!("{name}: retrieval response shipped WITHOUT snapshot"));
            assert!(snapshot.head.is_some(), "{name}: snapshot must carry HEAD");
            assert!(
                snapshot.dirty.iter().any(|p| p == "a.ts"),
                "{name}: snapshot must list the dirty file, got {:?}",
                snapshot.dirty
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Phase 3 item 2 — targets honesty: when the 500-match content probe
    /// cap fires for a keyword, the targets envelope must say lower_bound
    /// and NAME the cap; the "exhaustive" sentence must not be emitted.
    #[test]
    fn targets_probe_cap_sets_lower_bound_and_names_the_cap() {
        let root = tmpdir("targets-probe-cap");
        // 6 files x 100 lines = 600 word-bounded matches of "needle" —
        // comfortably beyond the 500-match probe cap.
        for f in 0..6 {
            let body: String = (0..100)
                .map(|i| format!("// needle occurrence {f}-{i}\n"))
                .collect();
            std::fs::write(root.join(format!("f{f}.rs")), body).unwrap();
        }
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "many"]);

        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Targets {
            task: "needle probe".into(),
            limit: Some(20),
        });
        assert!(resp.ok, "targets: {:?}", resp.error);
        let envelope = &resp.data()["envelope"];
        assert_eq!(
            envelope["lower_bound"].as_bool(),
            Some(true),
            "probe cap must force lower_bound: {envelope:?}"
        );
        let caps = envelope["caps"].as_array().unwrap();
        assert!(
            caps.iter().any(|c| {
                let s = c.as_str().unwrap_or_default();
                s.contains("content probe truncated at 500") && s.contains("'needle'")
            }),
            "the fired probe cap must be NAMED with its keyword: {caps:?}"
        );
        let closed_world = resp.data()["closed_world"].as_str().unwrap();
        assert!(
            !closed_world.contains("This list is exhaustive"),
            "capped probe must not claim exhaustiveness: {closed_world}"
        );
        assert!(
            closed_world.contains("content probe truncated at 500"),
            "the bounded phrasing must name the cap: {closed_world}"
        );
        // And the envelope-level epistemics must agree.
        let epistemics = resp.epistemics.as_ref().unwrap();
        assert!(!epistemics.closed_world && epistemics.lower_bound);
        assert!(epistemics.basis.contains("content probe truncated at 500"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Phase 3 item 2 — the content probe is word-bounded: keyword "auth"
    /// must not count "authorized"/"oauthToken" mentions as content signal.
    #[test]
    fn targets_content_probe_is_word_bounded() {
        let root = tmpdir("targets-word-bound");
        // Only substring mentions of "auth" — no word-bounded occurrence.
        std::fs::write(
            root.join("substr.rs"),
            "// authorized oauthToken authentication\n",
        )
        .unwrap();
        // A real word-bounded occurrence.
        std::fs::write(root.join("word.rs"), "// auth flow lives here\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Targets {
            task: "auth handling".into(),
            limit: Some(10),
        });
        assert!(resp.ok, "targets: {:?}", resp.error);
        let targets = resp.data()["targets"].as_array().unwrap();
        let content_reason = |path: &str| -> bool {
            targets
                .iter()
                .filter(|t| t["path"].as_str() == Some(path))
                .flat_map(|t| t["reasons"].as_array().cloned().unwrap_or_default())
                .any(|r| {
                    r.as_str().unwrap_or_default().contains("content matches")
                        && r.as_str().unwrap_or_default().contains("auth")
                })
        };
        assert!(
            content_reason("word.rs"),
            "word-bounded 'auth' occurrence must count as content signal: {targets:?}"
        );
        assert!(
            !content_reason("substr.rs"),
            "substring-only mentions must NOT count as 'auth' content signal: {targets:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Phase 3 item 3 — resolve surfaces the tier and scan-cap state in its
    /// serialized output, and an exact-unique identifier resolves as
    /// `resolved` (not permanently `ranked`).
    #[test]
    fn resolve_reports_tier_basis_and_resolved_confidence() {
        let root = tmpdir("resolve-honesty");
        std::fs::write(
            root.join("a.ts"),
            "export function uniqueTargetFn(): number { return 1 }\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Resolve {
            phrase: "uniqueTargetFn".into(),
            limit: Some(5),
        });
        assert!(resp.ok, "resolve: {:?}", resp.error);
        let data = resp.data();
        assert_eq!(
            data["confidence"].as_str(),
            Some("resolved"),
            "exact-unique identifier must resolve: {data:?}"
        );
        assert_eq!(data["scan_capped"].as_bool(), Some(false));
        assert!(
            data["basis"].as_str().unwrap_or_default().contains("ident"),
            "output must say which tier matched: {data:?}"
        );
        let epistemics = resp.epistemics.as_ref().unwrap();
        assert!(
            epistemics.basis.contains("ident"),
            "envelope epistemics must carry the tier basis: {epistemics:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
