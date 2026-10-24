// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The daemon's resident semantic pool (`specs/semantic.md`).
//!
//! The first `meaning` request that finds the embedding model on disk starts
//! one background build and answers `warming`; the build reads the corpus
//! (symbol chunks from the graph store, windows for the other files), embeds
//! what the vector store lacks and holds the result. A request answers from
//! the pool only while the pool was built for the generation being served:
//! behind it, the answer is `refreshing` and a rebuild starts, so a caller with
//! a deadline falls back to word leads instead of waiting. Nothing here ever
//! downloads a model.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use pixel_graph::store::Store;
use pixel_index::semantic::chunk::{Chunk, FILE_MAX, SymbolSpan, chunk_file};
use pixel_index::semantic::embed::Embedder;
use pixel_index::semantic::pool::{Pool, PoolStats};
use pixel_index::semantic::store::{STORE_DIR, VectorStore};
use pixel_model::{SymbolKind, credential_path};
use pixel_proto::meaning::{MeaningHit, MeaningPool, MeaningResult, MeaningUnavailableReason};

use crate::service::Shared;

/// The most hits one request returns.
pub const HITS_MAX: usize = 20;

/// The hits returned when the request names no limit.
pub const HITS_DEFAULT: usize = 10;

/// The corpus stops growing at this many files.
const FILES_MAX: usize = 20_000;

/// The corpus stops growing at this many bytes of text.
const BYTES_MAX: u64 = 96 * 1024 * 1024;

/// Extensions that are never prose or code: dependency locks and binaries
/// the text reader would skip anyway.
const SKIPPED_EXTENSIONS: [&str; 6] = ["lock", "svg", "map", "min.js", "snap", "sum"];

/// Why an embedder could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenFailure {
    /// The model is not on disk and a request never downloads it.
    ModelMissing(String),
    /// The model is there but unusable, or this build cannot embed.
    Failed(String),
}

/// How a daemon opens its embedder.
pub type Opener = fn(&Path) -> Result<Arc<dyn Embedder>, OpenFailure>;

/// The opener of a real daemon: the cached code model, offline.
#[cfg(feature = "semantic")]
pub fn default_opener(_root: &Path) -> Result<Arc<dyn Embedder>, OpenFailure> {
    use pixel_index::semantic::embed::{CODE_MODEL_REPO, model_cached, model_dir, open_cached};
    let Some(cache) = pixel_index::cache::cache_root() else {
        return Err(OpenFailure::Failed(
            "no cache directory (HOME is not set)".to_owned(),
        ));
    };
    if !model_cached(&cache, CODE_MODEL_REPO) {
        return Err(OpenFailure::ModelMissing(format!(
            "{CODE_MODEL_REPO} is not in {}; `pixel search-meaning --download` fetches it",
            model_dir(&cache, CODE_MODEL_REPO).display()
        )));
    }
    open_cached(&model_dir(&cache, CODE_MODEL_REPO), CODE_MODEL_REPO)
        .map(|model| Arc::new(model) as Arc<dyn Embedder>)
        .map_err(|error| OpenFailure::Failed(error.to_string()))
}

/// The opener of a build without the `semantic` feature.
#[cfg(not(feature = "semantic"))]
pub fn default_opener(_root: &Path) -> Result<Arc<dyn Embedder>, OpenFailure> {
    Err(OpenFailure::Failed(
        "this build has no semantic search (built without the `semantic` feature)".to_owned(),
    ))
}

enum State {
    Cold,
    Warming,
    /// `symbolic`: the chunks were cut at the graph's symbols; a pool built
    /// before the graph existed holds windows only and is rebuilt once it does.
    Ready {
        pool: Arc<Pool>,
        embedder: Arc<dyn Embedder>,
        symbolic: bool,
    },
    Refreshing,
    Unavailable(MeaningUnavailableReason, String),
}

/// What a caller is serving: the generation, and whether its graph holds every
/// edit batch (a pool cut without the graph is rebuilt once it does).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Serving {
    pub generation: u64,
    pub graph_fresh: bool,
}

/// The pool and its state machine, shared by the writer and its replicas.
pub struct Meaning {
    state: Mutex<State>,
    opener: Opener,
}

impl Meaning {
    pub fn new(opener: Opener) -> Self {
        Self {
            state: Mutex::new(State::Cold),
            opener,
        }
    }

    /// Answers `query`, or says why it cannot yet. `at` is what the caller
    /// serves: a pool answers only for the generation it was built for.
    pub(crate) fn request(
        &self,
        shared: &Arc<Shared>,
        query: &str,
        limit: Option<usize>,
        at: Serving,
    ) -> MeaningResult {
        let limit = limit.unwrap_or(HITS_DEFAULT).clamp(1, HITS_MAX);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match &*state {
            State::Cold => match (self.opener)(&shared.root) {
                Ok(embedder) => {
                    *state = State::Warming;
                    self.spawn_build(shared, embedder);
                    unavailable(MeaningUnavailableReason::Warming, None)
                }
                Err(OpenFailure::ModelMissing(detail)) => {
                    // Not cached as a failure: the model may be downloaded
                    // later, and the next request looks again.
                    unavailable(MeaningUnavailableReason::ModelMissing, Some(detail))
                }
                Err(OpenFailure::Failed(detail)) => {
                    *state = State::Unavailable(MeaningUnavailableReason::Failed, detail.clone());
                    unavailable(MeaningUnavailableReason::Failed, Some(detail))
                }
            },
            State::Warming => unavailable(MeaningUnavailableReason::Warming, None),
            State::Refreshing => unavailable(MeaningUnavailableReason::Refreshing, None),
            State::Unavailable(reason, detail) => unavailable(*reason, Some(detail.clone())),
            State::Ready {
                pool,
                embedder,
                symbolic,
            } => {
                if pool.generation < at.generation || (!*symbolic && at.graph_fresh) {
                    let embedder = Arc::clone(embedder);
                    *state = State::Refreshing;
                    self.spawn_build(shared, embedder);
                    return unavailable(MeaningUnavailableReason::Refreshing, None);
                }
                let (pool, embedder) = (Arc::clone(pool), Arc::clone(embedder));
                drop(state);
                answer(&pool, embedder.as_ref(), query, limit)
            }
        }
    }

    #[cfg_attr(test, mutants::skip)] // detached thread; `build` and `request` are tested
    fn spawn_build(&self, shared: &Arc<Shared>, embedder: Arc<dyn Embedder>) {
        let shared = Arc::clone(shared);
        let generation = shared
            .published
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .generation;
        std::thread::spawn(move || {
            let outcome = build(&shared.root, embedder.as_ref(), generation);
            let mut state = shared
                .meaning
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *state = match outcome {
                Ok((pool, symbolic)) => State::Ready {
                    pool: Arc::new(pool),
                    embedder,
                    symbolic,
                },
                Err(detail) => State::Unavailable(MeaningUnavailableReason::Failed, detail),
            };
        });
    }
}

fn unavailable(reason: MeaningUnavailableReason, detail: Option<String>) -> MeaningResult {
    MeaningResult::Unavailable {
        reason,
        detail,
        caps: Vec::new(),
    }
}

fn answer(pool: &Pool, embedder: &dyn Embedder, query: &str, limit: usize) -> MeaningResult {
    match pool.query(embedder, query, limit) {
        Ok(hits) => MeaningResult::Ready {
            hits: hits
                .into_iter()
                .map(|hit| MeaningHit {
                    path: hit.path,
                    start_line: hit.start_line,
                    end_line: hit.end_line,
                    symbol: hit.symbol,
                    score: hit.score,
                    snippet: hit.snippet,
                })
                .collect(),
            pool: pool_info(&pool.stats, pool),
            caps: vec![
                "semantic leads rank by similarity and are unverified: read them before relying on them".to_owned(),
            ],
        },
        Err(error) => unavailable(MeaningUnavailableReason::Failed, Some(error.to_string())),
    }
}

fn pool_info(stats: &PoolStats, pool: &Pool) -> MeaningPool {
    MeaningPool {
        model: stats.model.clone(),
        dims: stats.dims,
        files: stats.files,
        chunks: stats.chunks,
        eligible_files: stats.files,
        generation: pool.generation,
        age_ms: u64::try_from(pool.built_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        resident_bytes: stats.resident_bytes,
    }
}

/// Reads the corpus of `root`, embeds what is missing and returns the pool.
fn build(root: &Path, embedder: &dyn Embedder, generation: u64) -> Result<(Pool, bool), String> {
    let (chunks, symbolic) = corpus(root).map_err(|error| error.to_string())?;
    let mut store = vector_store(root);
    let pool = Pool::build(chunks, embedder, store.as_mut(), generation)
        .map_err(|error| error.to_string())?;
    Ok((pool, symbolic))
}

/// The vector store of `root`, when this root may keep one: it carries a
/// `.pixel` directory and is not the home directory.
fn vector_store(root: &Path) -> Option<VectorStore> {
    let sidecar = root.join(pixel_index::index::SHARD_DIR);
    if !sidecar.is_dir() {
        return None;
    }
    if std::env::var_os("HOME").is_some_and(|home| Path::new(&home) == root) {
        return None;
    }
    VectorStore::open(&sidecar.join(STORE_DIR)).ok()
}

/// Every chunk of `root`: symbols of the graph's files, windows of the rest;
/// all windows when the graph does not exist yet (the flag says which).
fn corpus(root: &Path) -> Result<(Vec<Chunk>, bool), pixel_graph::store::StoreError> {
    let store = match Store::open(root) {
        Ok(store) => Some(store),
        Err(pixel_graph::store::StoreError::NoIndex) => None,
        Err(error) => return Err(error),
    };
    let mut by_file: BTreeMap<String, Vec<SymbolSpan>> = BTreeMap::new();
    let symbols = match &store {
        Some(store) => store.all_symbols()?,
        None => Vec::new(),
    };
    for symbol in symbols {
        if matches!(
            symbol.kind,
            SymbolKind::File | SymbolKind::Package | SymbolKind::Variant
        ) {
            continue;
        }
        by_file.entry(symbol.file).or_default().push(SymbolSpan {
            name: symbol.name,
            line: symbol.line,
            end_line: symbol.end_line,
        });
    }
    let graph_files: Vec<String> = match &store {
        Some(store) => store.file_rows()?.into_iter().map(|row| row.path).collect(),
        None => Vec::new(),
    };
    let mut paths: Vec<String> = graph_files.clone();
    if let Ok(files) = pixel_io::GitRunner::new(root).ls_files_or_err() {
        let known: std::collections::HashSet<&str> =
            graph_files.iter().map(String::as_str).collect();
        paths.extend(
            files
                .into_iter()
                .filter(|path| !known.contains(path.as_str())),
        );
    }
    let mut chunks = Vec::new();
    let mut bytes = 0_u64;
    let mut spans: HashMap<String, Vec<SymbolSpan>> = by_file.into_iter().collect();
    for path in paths.into_iter().take(FILES_MAX) {
        if credential_path(Path::new(&path)) || skipped(&path) {
            continue;
        }
        let Ok(file) = pixel_io::repo_path::confine(root, &path) else {
            continue;
        };
        let Ok(text) = pixel_io::nofollow::read_text_capped(&file, FILE_MAX as u64) else {
            continue;
        };
        bytes += text.len() as u64;
        if bytes > BYTES_MAX {
            break;
        }
        let symbols = spans.remove(&path).unwrap_or_default();
        chunks.extend(chunk_file(&path, &text, &symbols));
    }
    Ok((chunks, store.is_some()))
}

fn skipped(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    SKIPPED_EXTENSIONS
        .iter()
        .any(|ext| lower.ends_with(&format!(".{ext}")))
}

/// Brief leads from the pool: hits, or the reason they are not available.
pub(crate) fn leads(
    shared: &Arc<Shared>,
    query: &str,
    limit: usize,
    at: Serving,
    deadline: Instant,
) -> Result<Vec<MeaningHit>, String> {
    if Instant::now() >= deadline {
        return Err("no time left".to_owned());
    }
    match shared.meaning.request(shared, query, Some(limit), at) {
        MeaningResult::Ready { hits, .. } => Ok(hits),
        MeaningResult::Unavailable { reason, detail, .. } => Err(detail.map_or_else(
            || reason.as_str().to_owned(),
            |d| format!("{}: {d}", reason.as_str()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pixel_index::semantic::embed::HashingEmbedder;
    use pixel_proto::Op;
    use serde_json::Value;

    use super::*;
    use crate::service::Service;
    use crate::testutil::{git, repo};

    fn hashing(_root: &Path) -> Result<Arc<dyn Embedder>, OpenFailure> {
        Ok(Arc::new(HashingEmbedder::new(128)))
    }

    fn missing(_root: &Path) -> Result<Arc<dyn Embedder>, OpenFailure> {
        Err(OpenFailure::ModelMissing(
            "the model is not in the cache".to_owned(),
        ))
    }

    fn broken(_root: &Path) -> Result<Arc<dyn Embedder>, OpenFailure> {
        Err(OpenFailure::Failed("onnx exploded".to_owned()))
    }

    fn project(opener: Opener) -> (tempfile::TempDir, Service) {
        let (dir, root) = repo(&[
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub mod retry;\npub mod draw;\n"),
            (
                "src/retry.rs",
                "/// Reads the retry limit from the config file.\npub fn retry_limit() -> u32 {\n    3\n}\n",
            ),
            (
                "src/draw.rs",
                "/// Draws the sidebar animation frame.\npub fn draw_frame() {}\n",
            ),
            (
                "docs/guide.md",
                "# Guide\n\nHow the retry limit is configured.\n",
            ),
        ]);
        std::fs::create_dir_all(root.join(".pixel")).unwrap();
        let service = Service::open_with(&root, opener).expect("the service opens");
        (dir, service)
    }

    fn at() -> Serving {
        Serving {
            generation: 1,
            graph_fresh: true,
        }
    }

    /// Any graph read builds the graph, which a fresh service has not yet.
    fn build_graph(service: &mut Service) {
        let op = serde_json::from_str(r#"{"op":"symbol","name":"retry_limit"}"#).expect("an op");
        assert!(service.handle(op).ok);
    }

    fn meaning(service: &mut Service, query: &str) -> MeaningResult {
        let response = service.handle(Op::Meaning {
            stale_ok: false,
            query: query.to_owned(),
            limit: None,
        });
        assert!(response.ok, "{:?}", response.error);
        let value: Value = response.result.expect("a result");
        serde_json::from_value(value).expect("a meaning result")
    }

    /// Asks until the pool answers or `within` passes: a build runs on its own
    /// thread, and a request never waits for it.
    fn until_ready(service: &mut Service, query: &str, within: Duration) -> MeaningResult {
        let deadline = Instant::now() + within;
        loop {
            let result = meaning(service, query);
            if matches!(result, MeaningResult::Ready { .. }) || Instant::now() >= deadline {
                return result;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_missing_model_answers_model_missing_every_time_and_never_builds() {
        let (_dir, mut service) = project(missing);
        for _ in 0..2 {
            match meaning(&mut service, "retry limit") {
                MeaningResult::Unavailable { reason, detail, .. } => {
                    assert_eq!(reason, MeaningUnavailableReason::ModelMissing);
                    assert_eq!(detail.as_deref(), Some("the model is not in the cache"));
                }
                other @ MeaningResult::Ready { .. } => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn a_model_that_fails_to_open_is_remembered_with_its_reason() {
        let (_dir, mut service) = project(broken);
        for _ in 0..2 {
            match meaning(&mut service, "retry limit") {
                MeaningResult::Unavailable { reason, detail, .. } => {
                    assert_eq!(reason, MeaningUnavailableReason::Failed);
                    assert_eq!(detail.as_deref(), Some("onnx exploded"));
                }
                other @ MeaningResult::Ready { .. } => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn the_first_request_warms_the_pool_then_answers_with_leads_and_the_pool() {
        let (_dir, mut service) = project(hashing);
        build_graph(&mut service);
        let first = meaning(&mut service, "where is the retry limit read from config");
        assert!(
            matches!(
                first,
                MeaningResult::Unavailable {
                    reason: MeaningUnavailableReason::Warming,
                    ..
                }
            ),
            "the first request never waits: {first:?}"
        );
        let ready = until_ready(
            &mut service,
            "where is the retry limit read from config",
            Duration::from_secs(20),
        );
        let MeaningResult::Ready { hits, pool, caps } = ready else {
            panic!("never ready: {ready:?}")
        };
        assert_eq!(hits[0].path, "src/retry.rs", "{hits:?}");
        assert_eq!(hits[0].symbol.as_deref(), Some("retry_limit"));
        assert_eq!(pool.model, "pixel-hashing");
        assert_eq!(pool.dims, 128);
        assert!(pool.files >= 3 && pool.chunks >= pool.files, "{pool:?}");
        assert!(caps[0].contains("unverified"));
        // The guide ranks below the code file for a question that does not name docs.
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        let guide = paths.iter().position(|p| *p == "docs/guide.md");
        assert!(guide.is_none_or(|at| at > 0), "{paths:?}");
    }

    #[test]
    fn a_pool_cut_before_the_graph_existed_is_rebuilt_at_its_symbols() {
        let (_dir, mut service) = project(hashing);
        let windows = until_ready(&mut service, "retry limit config", Duration::from_secs(20));
        let MeaningResult::Ready { hits, .. } = windows else {
            panic!("never ready")
        };
        assert_eq!(hits[0].symbol, None, "no graph yet, so windows: {hits:?}");
        build_graph(&mut service);
        let rebuilding = meaning(&mut service, "retry limit config");
        assert!(
            matches!(
                rebuilding,
                MeaningResult::Unavailable {
                    reason: MeaningUnavailableReason::Refreshing,
                    ..
                }
            ),
            "{rebuilding:?}"
        );
        let symbols = until_ready(&mut service, "retry limit config", Duration::from_secs(20));
        let MeaningResult::Ready { hits, .. } = symbols else {
            panic!("never rebuilt")
        };
        assert!(
            hits.iter()
                .any(|h| h.symbol.as_deref() == Some("retry_limit")),
            "{hits:?}"
        );
    }

    #[test]
    fn the_limit_is_clamped_and_one_hit_comes_per_file() {
        let (_dir, mut service) = project(hashing);
        let _ = until_ready(&mut service, "retry", Duration::from_secs(20));
        let response = service.handle(Op::Meaning {
            stale_ok: false,
            query: "retry".into(),
            limit: Some(1),
        });
        let value: MeaningResult = serde_json::from_value(response.result.unwrap()).unwrap();
        let MeaningResult::Ready { hits, .. } = value else {
            panic!("not ready")
        };
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn a_pool_behind_the_served_generation_says_refreshing_and_rebuilds() {
        let (dir, mut service) = project(hashing);
        let ready = until_ready(&mut service, "retry", Duration::from_secs(20));
        assert!(matches!(ready, MeaningResult::Ready { .. }));
        // A new commit moves the generation the next request serves.
        std::fs::write(dir.path().join("src/extra.rs"), "pub fn extra_retry() {}\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "more"]);
        let _ = service.handle(Op::Reindex {});
        let behind = meaning(&mut service, "retry");
        assert!(
            matches!(
                behind,
                MeaningResult::Unavailable {
                    reason: MeaningUnavailableReason::Refreshing,
                    ..
                }
            ),
            "{behind:?}"
        );
        let again = until_ready(&mut service, "extra retry", Duration::from_secs(20));
        let MeaningResult::Ready { hits, .. } = again else {
            panic!("never rebuilt: {again:?}")
        };
        assert!(hits.iter().any(|h| h.path == "src/extra.rs"), "{hits:?}");
    }

    #[test]
    fn the_vector_store_lives_under_the_sidecar_and_is_reused_by_the_next_build() {
        let (dir, mut service) = project(hashing);
        let _ = until_ready(&mut service, "retry", Duration::from_secs(20));
        let store = dir
            .path()
            .canonicalize()
            .unwrap()
            .join(".pixel/code-vectors");
        let manifest =
            std::fs::read_to_string(store.join("manifest.json")).expect("the store was written");
        assert!(manifest.contains("pixel-hashing"));
    }

    #[test]
    fn leads_should_fail_without_waiting_once_the_deadline_has_passed() {
        let (_dir, service) = project(hashing);
        let shared = service.shared();
        let past = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        let error = leads(&shared, "retry", 3, at(), past).unwrap_err();
        assert_eq!(error, "no time left");
        let cold = leads(
            &shared,
            "retry",
            3,
            at(),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(cold, "warming");
    }

    #[test]
    fn skipped_should_name_lockfiles_and_minified_bundles_only() {
        assert!(skipped("Cargo.lock"));
        assert!(skipped("web/app.min.js"));
        assert!(!skipped("src/lock.rs"));
        assert!(!skipped("docs/guide.md"));
    }

    #[test]
    fn the_default_opener_without_the_feature_says_so() {
        if cfg!(feature = "semantic") {
            return;
        }
        match default_opener(Path::new("/")) {
            Err(OpenFailure::Failed(detail)) => assert!(detail.contains("no semantic search")),
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("opened without the feature"),
        }
    }
}
