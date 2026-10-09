// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The resident index behind the `meaning` request.
//!
//! [`Meaning`] owns one [`Resident`]: the chunk vectors and term counts of
//! the repository, built on a background thread and then answering from
//! memory. A request never builds, embeds a chunk, reads a file or waits for a
//! build: it embeds the question, ranks, and returns. When there is nothing to
//! rank with (cold, still building, built for an older generation, no model on
//! disk, a failed build) it says which in a typed `unavailable` answer and
//! the caller falls back.
//!
//! Freshness is the daemon's publication generation, the counter the watcher
//! batches and graph builds already advance ([`crate::api::Service`]). A
//! build is stamped with the generation seen before it reads the first file,
//! so an edit that lands mid-build leaves it stale, never wrongly fresh. Once
//! the vectors have been asked for, every watcher batch starts the rebuild at
//! once, so the next question finds it done (a graph build, which advances
//! the generation without changing a file, is caught by the next question
//! instead); a daemon nobody asks pays nothing. Builds reuse every file whose
//! bytes are unchanged ([`Resident::build`]) and coalesce: changes that arrive
//! during a build are covered by one more.
//!
//! The embedding model is the one `pixel search-meaning` uses and is never
//! downloaded here: without it on disk the answer is `model_missing`.

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use pixel_proto::{MeaningHit, MeaningPool, MeaningResult, MeaningUnavailableReason};
use pixel_recall::code_resident::{RESIDENT_MAX_FILES, Resident, ResidentStats};
use pixel_recall::code_search::{DEFAULT_LIMIT, SEMANTIC_LEADS_UNVERIFIED, VectorCache};
use pixel_recall::embed::Embedder;
use serde_json::Value;

/// Most leads one request returns, whatever `limit` asks for.
pub(crate) const MEANING_MAX_LIMIT: usize = 50;

/// Longest question the request embeds and scores, in bytes: its terms cost a
/// pass over every chunk each, so a pasted page would not stay within a
/// brief's window.
pub(crate) const MEANING_QUERY_MAX_BYTES: usize = 1_024;

/// How long a failed build (a model that will not load) is left alone before
/// the next request tries again. A model that is not on disk is checked on
/// every request: that costs a `stat`.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// Most builds one background run chains while newer generations keep
/// arriving.
const MAX_BUILDS_PER_RUN: usize = 8;

/// Why an embedder could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenFailure {
    /// The model has not been downloaded.
    ModelMissing,
    /// It is on disk and does not load.
    Failed(String),
}

/// Opens the embedder the vectors are built with, from disk and never from
/// the network.
pub(crate) type Opener = Arc<dyn Fn() -> Result<Box<dyn Embedder>, OpenFailure> + Send + Sync>;

/// The opener over an embedding model: [`OpenFailure::ModelMissing`] without
/// it on disk, else whatever `open` returns.
fn open_when_on_disk(
    on_disk: bool,
    open: impl FnOnce() -> Result<Box<dyn Embedder>, String>,
) -> Result<Box<dyn Embedder>, OpenFailure> {
    if on_disk {
        open().map_err(OpenFailure::Failed)
    } else {
        Err(OpenFailure::ModelMissing)
    }
}

/// The production opener: the model of `pixel search-meaning`, offline.
#[cfg_attr(test, mutants::skip)] // one-line adapter over the on-disk model; `open_when_on_disk` holds the rule and is tested
fn open_code_model() -> Result<Box<dyn Embedder>, OpenFailure> {
    open_when_on_disk(
        pixel_recall::code_search::code_model_on_disk(),
        pixel_recall::code_search::open_code_embedder_offline,
    )
}

/// A finished build and the generation it was made for.
struct Built {
    resident: Resident,
    generation: u64,
    at: Instant,
}

/// Why the last build failed.
struct Failure {
    reason: MeaningUnavailableReason,
    detail: Option<String>,
    at: Instant,
}

impl Failure {
    fn failed(detail: impl Into<String>) -> Self {
        Self {
            reason: MeaningUnavailableReason::Failed,
            detail: Some(detail.into()),
            at: Instant::now(),
        }
    }
}

impl From<OpenFailure> for Failure {
    fn from(failure: OpenFailure) -> Self {
        match failure {
            OpenFailure::ModelMissing => Self {
                reason: MeaningUnavailableReason::ModelMissing,
                detail: None,
                at: Instant::now(),
            },
            OpenFailure::Failed(detail) => Self::failed(detail),
        }
    }
}

#[derive(Default)]
struct State {
    ready: Option<Arc<Built>>,
    building: bool,
    failure: Option<Failure>,
}

/// Where a request stands against the state.
enum Availability {
    Fresh(Arc<Built>),
    Unavailable {
        reason: MeaningUnavailableReason,
        detail: Option<String>,
    },
}

/// The resident vectors of one repository, shared by the daemon's request
/// loop and the read replicas of the evidence bridge.
pub(crate) struct Meaning {
    root: PathBuf,
    open: Opener,
    cache: VectorCache,
    max_files: usize,
    retry_after: Duration,
    state: Mutex<State>,
    /// The embedder that embeds the questions; a build takes it while it
    /// embeds chunks, and puts it back.
    embedder: Mutex<Option<Box<dyn Embedder>>>,
    /// The newest generation any request or watcher batch has reported.
    latest: AtomicU64,
}

impl Meaning {
    /// The production index of `root`: the `search-meaning` model, and the
    /// vector store of `root` when it carries a pixel index.
    #[cfg_attr(test, mutants::skip)] // adapter reading $HOME; `vector_cache_for` and `with` hold the rules and are tested
    pub(crate) fn new(root: &Path) -> Arc<Self> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::with(
            root,
            Arc::new(open_code_model),
            pixel_recall::code_search::vector_cache_for(root, home.as_deref()),
            RESIDENT_MAX_FILES,
            RETRY_AFTER,
        )
    }

    pub(crate) fn with(
        root: &Path,
        open: Opener,
        cache: VectorCache,
        max_files: usize,
        retry_after: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            root: root.to_path_buf(),
            open,
            cache,
            max_files,
            retry_after,
            state: Mutex::new(State::default()),
            embedder: Mutex::new(None),
            latest: AtomicU64::new(0),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn embedder(&self) -> MutexGuard<'_, Option<Box<dyn Embedder>>> {
        self.embedder.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Start the background build when the vectors are not fresh for
    /// `generation`, none is running and the last failure is not recent.
    pub(crate) fn ensure(self: &Arc<Self>, generation: u64) {
        self.latest.fetch_max(generation, Ordering::AcqRel);
        let mut state = self.lock();
        let fresh = state
            .ready
            .as_ref()
            .is_some_and(|built| built.generation >= generation);
        let backing_off = state.failure.as_ref().is_some_and(|failure| {
            failure.reason == MeaningUnavailableReason::Failed
                && failure.at.elapsed() < self.retry_after
        });
        if fresh || state.building || backing_off {
            return;
        }
        state.building = true;
        drop(state);
        self.spawn_build();
    }

    /// A new `generation` was published (a watcher batch): rebuild now when
    /// the vectors have been asked for, so the next question finds them
    /// fresh. An index nobody asked for stays cold.
    pub(crate) fn nudge(self: &Arc<Self>, generation: u64) {
        self.latest.fetch_max(generation, Ordering::AcqRel);
        let state = self.lock();
        let asked_for = state.ready.is_some() || state.building;
        drop(state);
        if asked_for {
            self.ensure(generation);
        }
    }

    #[cfg_attr(test, mutants::skip)] // detached thread; `run_builds` holds the loop and is tested
    fn spawn_build(self: &Arc<Self>) {
        let meaning = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("pixel-meaning".to_string())
            .spawn(move || meaning.run_builds());
        if let Err(error) = spawned {
            let mut state = self.lock();
            state.building = false;
            state.failure = Some(Failure::failed(format!("build thread: {error}")));
        }
    }

    /// Build for the newest generation seen, and again while newer ones keep
    /// arriving (at most [`MAX_BUILDS_PER_RUN`] in a row: a tree edited
    /// without pause is rebuilt by the next request), then clear the running
    /// flag. A panic is a failed build, not a flag that stays set. Returns
    /// the builds run.
    pub(crate) fn run_builds(&self) -> usize {
        let mut builds = 0;
        loop {
            builds += 1;
            let target = self.latest.load(Ordering::Acquire);
            let previous = self.lock().ready.clone();
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                self.build(previous.as_deref(), target)
            }))
            .unwrap_or_else(|_| Err(Failure::failed("the build panicked")));
            let mut state = self.lock();
            match outcome {
                Ok(built) => {
                    state.ready = Some(Arc::new(built));
                    state.failure = None;
                    if self.latest.load(Ordering::Acquire) > target && builds < MAX_BUILDS_PER_RUN {
                        continue;
                    }
                }
                Err(failure) => state.failure = Some(failure),
            }
            state.building = false;
            return builds;
        }
    }

    /// One build for `generation`, refreshing `previous`.
    fn build(&self, previous: Option<&Built>, generation: u64) -> Result<Built, Failure> {
        let taken = self.embedder().take();
        let mut embedder = match taken {
            Some(embedder) => embedder,
            None => (self.open)()?,
        };
        let built = Resident::build(
            &self.root,
            previous.map(|built| &built.resident),
            embedder.as_mut(),
            self.cache,
            self.max_files,
        );
        *self.embedder() = Some(embedder);
        built
            .map(|resident| Built {
                resident,
                generation,
                at: Instant::now(),
            })
            .map_err(Failure::failed)
    }

    fn availability(&self, generation: u64) -> Availability {
        let state = self.lock();
        if let Some(built) = &state.ready
            && built.generation >= generation
        {
            return Availability::Fresh(Arc::clone(built));
        }
        let unavailable = |reason, detail| Availability::Unavailable { reason, detail };
        if state.building {
            return unavailable(
                if state.ready.is_some() {
                    MeaningUnavailableReason::Refreshing
                } else {
                    MeaningUnavailableReason::Warming
                },
                None,
            );
        }
        if let Some(failure) = &state.failure {
            return unavailable(failure.reason, failure.detail.clone());
        }
        unavailable(
            if state.ready.is_some() {
                MeaningUnavailableReason::Stale
            } else {
                MeaningUnavailableReason::Cold
            },
            None,
        )
    }

    /// The answer to a `meaning` request at `generation`, as the wire value.
    /// It reads the state and ranks; it never starts or waits for a build
    /// ([`Meaning::ensure`] does the first).
    ///
    /// # Errors
    ///
    /// A blank question, or an embedder that fails on a built index.
    pub(crate) fn answer(
        &self,
        generation: u64,
        query: &str,
        limit: Option<usize>,
    ) -> Result<Value, String> {
        serde_json::to_value(self.result(generation, query, limit)?).map_err(|e| e.to_string())
    }

    fn result(
        &self,
        generation: u64,
        query: &str,
        limit: Option<usize>,
    ) -> Result<MeaningResult, String> {
        let query = query.trim();
        if query.is_empty() {
            return Err("meaning needs a question: the query is empty".to_string());
        }
        let built = match self.availability(generation) {
            Availability::Fresh(built) => built,
            Availability::Unavailable { reason, detail } => {
                return Ok(unavailable(reason, detail));
            }
        };
        let (query, truncated) = bounded_query(query);
        let (k, capped) = bounded_limit(limit);
        let hits = {
            let mut slot = self.embedder();
            // A build that started after the state was read holds the
            // embedder: the vectors are being replaced.
            let Some(embedder) = slot.as_mut() else {
                return Ok(unavailable(MeaningUnavailableReason::Refreshing, None));
            };
            built
                .resident
                .search(embedder.as_mut(), query, k)
                .map_err(|error| format!("meaning: {error}"))?
        };
        let stats = built.resident.stats();
        let mut caps = caps_for(stats, self.max_files);
        caps.extend(capped.map(|requested| {
            format!("limit capped at {MEANING_MAX_LIMIT} (requested {requested})")
        }));
        if truncated {
            caps.push(format!(
                "query truncated to its first {MEANING_QUERY_MAX_BYTES} bytes"
            ));
        }
        Ok(MeaningResult::Ready {
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
            pool: MeaningPool {
                model: stats.model_id.clone(),
                dims: stats.dims,
                files: stats.files,
                chunks: stats.chunks,
                eligible_files: stats.eligible_files,
                generation: built.generation,
                age_ms: u64::try_from(built.at.elapsed().as_millis()).unwrap_or(u64::MAX),
                resident_bytes: stats.bytes,
            },
            caps,
        })
    }
}

/// `query` cut to [`MEANING_QUERY_MAX_BYTES`] at a character boundary, and
/// whether it was.
fn bounded_query(query: &str) -> (&str, bool) {
    if query.len() <= MEANING_QUERY_MAX_BYTES {
        return (query, false);
    }
    (
        &query[..query.floor_char_boundary(MEANING_QUERY_MAX_BYTES)],
        true,
    )
}

/// The leads to return for a requested `limit`, and the request when it was
/// capped. Zero is honoured: a question that only asks whether the vectors
/// are ready.
fn bounded_limit(limit: Option<usize>) -> (usize, Option<usize>) {
    let requested = limit.unwrap_or(DEFAULT_LIMIT);
    (
        requested.min(MEANING_MAX_LIMIT),
        (requested > MEANING_MAX_LIMIT).then_some(requested),
    )
}

/// The caps a `Ready` answer names: that its leads are unverified, and what
/// the index left out of the repository.
fn caps_for(stats: &ResidentStats, max_files: usize) -> Vec<String> {
    let mut caps = vec![SEMANTIC_LEADS_UNVERIFIED.to_string()];
    if stats.file_limit_reached {
        caps.push(format!(
            "the index holds a deterministic sample of {} of {} eligible files (resident limit {max_files})",
            stats.sampled_files, stats.eligible_files
        ));
    }
    if stats.skipped_files > 0 {
        caps.push(format!(
            "{} eligible file(s) are not indexed: unreadable, over 512 KiB, binary or not UTF-8",
            stats.skipped_files
        ));
    }
    if stats.credential_files > 0 {
        caps.push(format!(
            "{} eligible file(s) are never indexed: credential-shaped paths (.env, keys, secrets)",
            stats.credential_files
        ));
    }
    if !stats.vector_cache_errors.is_empty() {
        caps.push(format!(
            "vector cache: {}; the affected chunks were embedded again",
            stats.vector_cache_errors.join("; ")
        ));
    }
    caps
}

/// The `unavailable` answer for `reason`.
fn unavailable(reason: MeaningUnavailableReason, detail: Option<String>) -> MeaningResult {
    MeaningResult::Unavailable {
        reason,
        detail,
        caps: vec![format!(
            "semantic index unavailable ({}): no semantic leads",
            reason.as_str()
        )],
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! The model stand-in the daemon's tests share: no weights, no download.

    use super::*;
    use pixel_recall::embed::EmbedKind;

    pub(crate) const DIMS: usize = 256;

    /// Embeds a text as its words hashed into [`DIMS`] buckets; counts the
    /// passages it is handed and can be told to fail on questions.
    pub(crate) struct WordEmbedder {
        pub(crate) passages: Arc<AtomicU64>,
        pub(crate) fail_queries: bool,
    }

    impl Embedder for WordEmbedder {
        fn model_id(&self) -> &str {
            "fixture-words"
        }

        fn dims(&self) -> usize {
            DIMS
        }

        fn embed_batch(
            &mut self,
            texts: &[&str],
            kind: EmbedKind,
        ) -> Result<Vec<Vec<f32>>, String> {
            if matches!(kind, EmbedKind::Query) && self.fail_queries {
                return Err("fixture model failed".into());
            }
            if matches!(kind, EmbedKind::Passage) {
                self.passages
                    .fetch_add(texts.len() as u64, Ordering::SeqCst);
            }
            Ok(texts
                .iter()
                .map(|text| {
                    let mut vector = vec![0.0f32; DIMS];
                    for word in text.split(|c: char| !c.is_alphanumeric()) {
                        if !word.is_empty() {
                            let bucket = xxhash_rust::xxh3::xxh3_64(word.to_lowercase().as_bytes())
                                % DIMS as u64;
                            vector[bucket as usize] += 1.0;
                        }
                    }
                    vector
                })
                .collect())
        }
    }

    /// An index over `root` whose model is a [`WordEmbedder`], with the
    /// vector store off and a backoff no test waits out.
    pub(crate) fn meaning(root: &Path) -> Arc<Meaning> {
        counting(root).0
    }

    /// [`meaning`] and the number of times its model was opened.
    pub(crate) fn counting(root: &Path) -> (Arc<Meaning>, Arc<AtomicU64>) {
        let opened = Arc::new(AtomicU64::new(0));
        let passages = Arc::new(AtomicU64::new(0));
        let meaning = Meaning::with(
            root,
            {
                let opened = Arc::clone(&opened);
                Arc::new(move || {
                    opened.fetch_add(1, Ordering::SeqCst);
                    Ok(Box::new(WordEmbedder {
                        passages: Arc::clone(&passages),
                        fail_queries: false,
                    }) as Box<dyn Embedder>)
                })
            },
            VectorCache::Disabled,
            100,
            Duration::from_secs(3600),
        );
        (meaning, opened)
    }

    /// An index whose model opens only once the returned sender is used or
    /// dropped: its build stays `warming` for as long as a test needs.
    pub(crate) fn gated(root: &Path) -> (Arc<Meaning>, std::sync::mpsc::Sender<()>) {
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let passages = Arc::new(AtomicU64::new(0));
        let meaning = Meaning::with(
            root,
            Arc::new(move || {
                // A dropped sender also ends the wait.
                let _ = gate.lock().unwrap_or_else(PoisonError::into_inner).recv();
                Ok(Box::new(WordEmbedder {
                    passages: Arc::clone(&passages),
                    fail_queries: false,
                }) as Box<dyn Embedder>)
            }),
            VectorCache::Disabled,
            100,
            Duration::from_secs(3600),
        );
        (meaning, release)
    }

    /// Build the index for `generation` on this thread, as the background
    /// thread would.
    pub(crate) fn build(meaning: &Meaning, generation: u64) {
        meaning.latest.store(generation, Ordering::SeqCst);
        meaning.run_builds();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::testing::{DIMS, WordEmbedder};
    use super::*;

    /// A scratch directory removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "pixel-daemon-meaning-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Fixture {
        root: Scratch,
        passages: Arc<AtomicU64>,
        opened: Arc<AtomicU64>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = Scratch::new();
            write(
                root.path(),
                "src/billing.rs",
                &function("Charge the customer card for an invoice", "charge_invoice"),
            );
            write(
                root.path(),
                "src/weather.rs",
                &function(
                    "Render the weather forecast for the week",
                    "render_forecast",
                ),
            );
            Self {
                root,
                passages: Arc::new(AtomicU64::new(0)),
                opened: Arc::new(AtomicU64::new(0)),
            }
        }

        fn meaning(&self) -> Arc<Meaning> {
            self.meaning_with(100, Duration::from_secs(3600))
        }

        fn meaning_with(&self, max_files: usize, retry_after: Duration) -> Arc<Meaning> {
            let passages = Arc::clone(&self.passages);
            let opened = Arc::clone(&self.opened);
            Meaning::with(
                self.root.path(),
                Arc::new(move || {
                    opened.fetch_add(1, Ordering::SeqCst);
                    Ok(Box::new(WordEmbedder {
                        passages: Arc::clone(&passages),
                        fail_queries: false,
                    }) as Box<dyn Embedder>)
                }),
                VectorCache::Disabled,
                max_files,
                retry_after,
            )
        }

        fn meaning_failing(&self, failure: OpenFailure, retry_after: Duration) -> Arc<Meaning> {
            let opened = Arc::clone(&self.opened);
            Meaning::with(
                self.root.path(),
                Arc::new(move || {
                    opened.fetch_add(1, Ordering::SeqCst);
                    Err(failure.clone())
                }),
                VectorCache::Disabled,
                100,
                retry_after,
            )
        }
    }

    fn write(root: &Path, path: &str, text: &str) {
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, text).unwrap();
    }

    fn function(doc: &str, name: &str) -> String {
        let steps: String = (0..10)
            .map(|step| format!("    let step_{step} = {step} + 1;\n"))
            .collect();
        format!("/// {doc}\npub fn {name}() -> u32 {{\n{steps}    0\n}}\n")
    }

    fn result(
        meaning: &Meaning,
        generation: u64,
        query: &str,
        limit: Option<usize>,
    ) -> MeaningResult {
        meaning.result(generation, query, limit).unwrap()
    }

    fn reason(result: &MeaningResult) -> MeaningUnavailableReason {
        match result {
            MeaningResult::Unavailable { reason, .. } => *reason,
            ready @ MeaningResult::Ready { .. } => panic!("expected unavailable, got {ready:?}"),
        }
    }

    fn hits(result: MeaningResult) -> (Vec<MeaningHit>, MeaningPool, Vec<String>) {
        match result {
            MeaningResult::Ready { hits, pool, caps } => (hits, pool, caps),
            unavailable @ MeaningResult::Unavailable { .. } => {
                panic!("expected ready, got {unavailable:?}")
            }
        }
    }

    /// Wait for the background build to clear its flag, for a bounded time.
    fn wait_until_idle(meaning: &Meaning) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while meaning.lock().building {
            assert!(Instant::now() < deadline, "the build did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A cold index answers `cold` without building anything: reading is not
    /// asking for a build.
    #[test]
    fn answer_should_report_cold_and_build_nothing_before_anything_asked_for_it() {
        let fixture = Fixture::new();
        let meaning = fixture.meaning();
        let answer = result(&meaning, 1, "charge the invoice", None);
        assert_eq!(reason(&answer), MeaningUnavailableReason::Cold);
        assert!(!meaning.lock().building);
        assert_eq!(fixture.opened.load(Ordering::SeqCst), 0, "no model opened");
        assert_eq!(fixture.passages.load(Ordering::SeqCst), 0);
        let MeaningResult::Unavailable { caps, detail, .. } = answer else {
            unreachable!()
        };
        assert_eq!(
            caps,
            ["semantic index unavailable (cold): no semantic leads"]
        );
        assert_eq!(detail, None);
    }

    /// `ensure` starts the build and the request that follows says so; once
    /// it finishes the same request ranks.
    #[test]
    fn ensure_should_build_in_the_background_and_then_answer_with_ranked_leads() {
        let fixture = Fixture::new();
        let meaning = fixture.meaning();
        meaning.ensure(1);
        let during = result(&meaning, 1, "charge the invoice", None);
        // The thread may already be done: only a cold answer is wrong.
        assert_ne!(
            reason_or_ready(&during),
            Some(MeaningUnavailableReason::Cold)
        );
        wait_until_idle(&meaning);

        let (found, pool, caps) = hits(result(
            &meaning,
            1,
            "charge the customer card invoice",
            None,
        ));
        assert_eq!(found[0].path, "src/billing.rs");
        assert_eq!(found[0].symbol.as_deref(), Some("charge_invoice"));
        assert_eq!((found[0].start_line, found[0].end_line), (1, 14));
        assert_eq!(found[1].path, "src/weather.rs");
        assert!(found[0].score > found[1].score);
        assert_eq!(pool.model, "fixture-words");
        assert_eq!((pool.files, pool.dims, pool.generation), (2, DIMS, 1));
        assert_eq!(pool.eligible_files, 2);
        assert!(pool.resident_bytes > (pool.chunks * DIMS * 4) as u64);
        assert_eq!(caps, [SEMANTIC_LEADS_UNVERIFIED]);
        assert_eq!(fixture.opened.load(Ordering::SeqCst), 1);
    }

    fn reason_or_ready(result: &MeaningResult) -> Option<MeaningUnavailableReason> {
        match result {
            MeaningResult::Unavailable { reason, .. } => Some(*reason),
            MeaningResult::Ready { .. } => None,
        }
    }

    /// The thread runs the same loop a test can call: the first build is
    /// `warming` while it runs and `ready` after.
    #[test]
    fn availability_should_say_warming_while_the_first_build_runs() {
        let fixture = Fixture::new();
        let meaning = fixture.meaning();
        meaning.lock().building = true;
        assert_eq!(
            reason(&result(&meaning, 1, "invoice", None)),
            MeaningUnavailableReason::Warming
        );
        assert_eq!(meaning.run_builds(), 1, "nothing newer arrived");
        assert!(!meaning.lock().building);
        assert!(reason_or_ready(&result(&meaning, 0, "invoice", None)).is_none());
    }

    /// Built for generation 1, a request at generation 2 is stale until a
    /// rebuild is running (`refreshing`) and fresh when it finished; the
    /// answer never mixes the two.
    #[test]
    fn availability_should_distinguish_stale_refreshing_and_fresh_generations() {
        let fixture = Fixture::new();
        let meaning = fixture.meaning();
        meaning.latest.store(1, Ordering::SeqCst);
        meaning.run_builds();
        assert!(reason_or_ready(&result(&meaning, 1, "invoice", None)).is_none());
        assert!(
            reason_or_ready(&result(&meaning, 0, "invoice", None)).is_none(),
            "a caller behind the build is served by it"
        );

        assert_eq!(
            reason(&result(&meaning, 2, "invoice", None)),
            MeaningUnavailableReason::Stale
        );
        meaning.lock().building = true;
        assert_eq!(
            reason(&result(&meaning, 2, "invoice", None)),
            MeaningUnavailableReason::Refreshing
        );
        meaning.lock().building = false;

        meaning.latest.store(2, Ordering::SeqCst);
        meaning.run_builds();
        let (_, pool, _) = hits(result(&meaning, 2, "invoice", None));
        assert_eq!(pool.generation, 2);
    }

    /// An edit between two questions is in the next answer, and the rebuild
    /// embeds only what changed.
    #[test]
    fn rebuild_should_see_an_edit_and_embed_only_the_changed_file() {
        let fixture = Fixture::new();
        let meaning = fixture.meaning();
        meaning.latest.store(1, Ordering::SeqCst);
        meaning.run_builds();
        let embedded = fixture.passages.load(Ordering::SeqCst);
        assert_eq!(embedded, 2);

        write(
            fixture.root.path(),
            "src/weather.rs",
            &function("Predict the glacier avalanche risk", "predict_avalanche"),
        );
        meaning.latest.store(2, Ordering::SeqCst);
        meaning.run_builds();
        assert_eq!(fixture.passages.load(Ordering::SeqCst), embedded + 1);
        let (found, _, _) = hits(result(&meaning, 2, "glacier avalanche risk", Some(1)));
        assert_eq!(found[0].symbol.as_deref(), Some("predict_avalanche"));
        assert_eq!(
            fixture.opened.load(Ordering::SeqCst),
            1,
            "the model stays loaded across builds"
        );
    }

    /// A watcher batch that lands while a build runs is covered by one more
    /// build in the same run: the vectors end fresh for the newest generation,
    /// not for the one the run started with.
    #[test]
    fn run_builds_should_build_again_when_a_newer_generation_arrives_mid_build() {
        let fixture = Fixture::new();
        let slot: Arc<OnceLock<std::sync::Weak<Meaning>>> = Arc::default();
        let passages = Arc::clone(&fixture.passages);
        let meaning = Meaning::with(
            fixture.root.path(),
            {
                let slot = Arc::clone(&slot);
                Arc::new(move || {
                    // The watcher publishes generation 2 while the model loads.
                    if let Some(meaning) = slot.get().and_then(std::sync::Weak::upgrade) {
                        meaning.latest.fetch_max(2, Ordering::SeqCst);
                    }
                    Ok(Box::new(WordEmbedder {
                        passages: Arc::clone(&passages),
                        fail_queries: false,
                    }) as Box<dyn Embedder>)
                })
            },
            VectorCache::Disabled,
            100,
            Duration::from_secs(3600),
        );
        slot.set(Arc::downgrade(&meaning)).unwrap();
        meaning.latest.store(1, Ordering::SeqCst);
        meaning.lock().building = true;
        assert_eq!(meaning.run_builds(), 2, "one build per generation seen");

        assert!(!meaning.lock().building);
        let (_, pool, _) = hits(result(&meaning, 2, "invoice", None));
        assert_eq!(pool.generation, 2, "the run ended on the newest generation");
        assert_eq!(
            fixture.passages.load(Ordering::SeqCst),
            2,
            "the second build reused every file of the first"
        );
    }

    /// No model on disk is reported as such on every request, and a model
    /// that appears is used by the next build without a restart.
    #[test]
    fn ensure_should_report_a_missing_model_and_retry_on_every_request() {
        let fixture = Fixture::new();
        let meaning = fixture.meaning_failing(OpenFailure::ModelMissing, Duration::from_secs(3600));
        meaning.ensure(1);
        wait_until_idle(&meaning);
        let answer = result(&meaning, 1, "invoice", None);
        assert_eq!(reason(&answer), MeaningUnavailableReason::ModelMissing);
        meaning.ensure(1);
        wait_until_idle(&meaning);
        assert_eq!(
            fixture.opened.load(Ordering::SeqCst),
            2,
            "a missing model is checked again on the next request"
        );
    }

    /// A model that fails to load is not retried within the backoff, and is
    /// once the backoff passed; the detail reaches the answer.
    #[test]
    fn ensure_should_back_off_after_a_failed_build() {
        let fixture = Fixture::new();
        let failing = fixture.meaning_failing(
            OpenFailure::Failed("model load: corrupt".into()),
            Duration::from_secs(3600),
        );
        failing.ensure(1);
        wait_until_idle(&failing);
        failing.ensure(1);
        wait_until_idle(&failing);
        assert_eq!(
            fixture.opened.load(Ordering::SeqCst),
            1,
            "within the backoff"
        );
        let answer = result(&failing, 1, "invoice", None);
        let MeaningResult::Unavailable { reason, detail, .. } = answer else {
            panic!("expected unavailable")
        };
        assert_eq!(reason, MeaningUnavailableReason::Failed);
        assert_eq!(detail.as_deref(), Some("model load: corrupt"));

        let retrying = fixture.meaning_failing(
            OpenFailure::Failed("model load: corrupt".into()),
            Duration::ZERO,
        );
        retrying.ensure(1);
        wait_until_idle(&retrying);
        retrying.ensure(1);
        wait_until_idle(&retrying);
        assert_eq!(fixture.opened.load(Ordering::SeqCst), 3, "past the backoff");
    }

    /// A panic in the build is a failed build with the flag cleared, never a
    /// daemon stuck on `warming`.
    #[test]
    fn run_builds_should_turn_a_panicking_build_into_a_failure() {
        let fixture = Fixture::new();
        let meaning = Meaning::with(
            fixture.root.path(),
            Arc::new(|| panic!("model exploded")),
            VectorCache::Disabled,
            100,
            Duration::from_secs(3600),
        );
        meaning.lock().building = true;
        meaning.run_builds();
        assert!(!meaning.lock().building);
        let MeaningResult::Unavailable { reason, detail, .. } =
            result(&meaning, 1, "invoice", None)
        else {
            panic!("expected unavailable")
        };
        assert_eq!(reason, MeaningUnavailableReason::Failed);
        assert_eq!(detail.as_deref(), Some("the build panicked"));
    }

    /// A watcher batch rebuilds at once only once the vectors have been
    /// asked for; a cold index stays cold.
    #[test]
    fn nudge_should_rebuild_only_an_index_that_was_asked_for() {
        let fixture = Fixture::new();
        let cold = fixture.meaning();
        cold.nudge(5);
        assert!(!cold.lock().building);
        assert_eq!(fixture.opened.load(Ordering::SeqCst), 0);
        assert_eq!(
            cold.latest.load(Ordering::SeqCst),
            5,
            "the generation is remembered"
        );

        let warm = fixture.meaning();
        warm.latest.store(1, Ordering::SeqCst);
        warm.run_builds();
        warm.nudge(2);
        wait_until_idle(&warm);
        let (_, pool, _) = hits(result(&warm, 2, "invoice", None));
        assert_eq!(
            pool.generation, 2,
            "already rebuilt before the next question"
        );
    }

    #[test]
    fn answer_should_refuse_a_blank_question_even_when_unavailable() {
        let fixture = Fixture::new();
        let meaning = fixture.meaning();
        for blank in ["", "   ", "\n\t"] {
            let error = meaning.answer(1, blank, None).unwrap_err();
            assert!(error.contains("query is empty"), "{error}");
        }
    }

    /// The limit: the default without one, honoured below the cap and at it,
    /// capped above it with the request named, and zero allowed (a probe).
    #[test]
    fn answer_should_apply_the_default_limit_and_cap_the_requested_one() {
        let fixture = Fixture::new();
        for index in 0..60 {
            write(
                fixture.root.path(),
                &format!("src/extra_{index:02}.rs"),
                &function(
                    &format!("Helper number {index}"),
                    &format!("helper_{index}"),
                ),
            );
        }
        let meaning = fixture.meaning_with(1000, Duration::from_secs(3600));
        meaning.latest.store(1, Ordering::SeqCst);
        meaning.run_builds();

        let (found, _, caps) = hits(result(&meaning, 1, "helper number", None));
        assert_eq!(found.len(), DEFAULT_LIMIT);
        assert_eq!(caps, [SEMANTIC_LEADS_UNVERIFIED]);

        let (found, _, caps) = hits(result(
            &meaning,
            1,
            "helper number",
            Some(MEANING_MAX_LIMIT),
        ));
        assert_eq!(found.len(), MEANING_MAX_LIMIT);
        assert_eq!(
            caps,
            [SEMANTIC_LEADS_UNVERIFIED],
            "at the cap is not over it"
        );

        let (found, _, caps) = hits(result(
            &meaning,
            1,
            "helper number",
            Some(MEANING_MAX_LIMIT + 1),
        ));
        assert_eq!(found.len(), MEANING_MAX_LIMIT);
        assert_eq!(
            caps.last().map(String::as_str),
            Some("limit capped at 50 (requested 51)")
        );

        let (found, pool, _) = hits(result(&meaning, 1, "helper number", Some(0)));
        assert!(found.is_empty());
        assert_eq!(pool.files, 62, "a zero limit still reports the pool");

        assert_eq!(bounded_limit(Some(3)), (3, None));
        assert_eq!(bounded_limit(None), (DEFAULT_LIMIT, None));
        assert_eq!(bounded_limit(Some(50)), (50, None));
        assert_eq!(bounded_limit(Some(51)), (50, Some(51)));
    }

    /// Hits come best first with ties by path, run to run.
    #[test]
    fn answer_should_break_ties_by_path_deterministically() {
        let fixture = Fixture::new();
        write(
            fixture.root.path(),
            "src/b_twin.rs",
            &function("Total the ledger", "total"),
        );
        write(
            fixture.root.path(),
            "src/a_twin.rs",
            &function("Total the ledger", "total"),
        );
        let meaning = fixture.meaning();
        meaning.latest.store(1, Ordering::SeqCst);
        meaning.run_builds();
        let first = hits(result(&meaning, 1, "total the ledger", None)).0;
        let second = hits(result(&meaning, 1, "total the ledger", None)).0;
        assert_eq!(first, second);
        let paths: Vec<&str> = first.iter().map(|hit| hit.path.as_str()).collect();
        assert_eq!(&paths[..2], ["src/a_twin.rs", "src/b_twin.rs"]);
    }

    /// A question longer than the cap is cut at a character boundary and the
    /// answer says so; one at the cap is whole.
    #[test]
    fn bounded_query_should_cut_at_a_character_boundary_above_the_cap_only() {
        let at_cap = "a".repeat(MEANING_QUERY_MAX_BYTES);
        assert_eq!(bounded_query(&at_cap), (at_cap.as_str(), false));
        let over = "a".repeat(MEANING_QUERY_MAX_BYTES + 1);
        assert_eq!(
            bounded_query(&over),
            (&over[..MEANING_QUERY_MAX_BYTES], true)
        );
        // A two-byte character straddling the cap is dropped whole.
        let wide = format!(
            "{}é{}",
            "a".repeat(MEANING_QUERY_MAX_BYTES - 1),
            "b".repeat(10)
        );
        let (kept, truncated) = bounded_query(&wide);
        assert!(truncated);
        assert_eq!(kept.len(), MEANING_QUERY_MAX_BYTES - 1);
    }

    #[test]
    fn answer_should_name_a_truncated_query_in_its_caps() {
        let fixture = Fixture::new();
        let meaning = fixture.meaning();
        meaning.latest.store(1, Ordering::SeqCst);
        meaning.run_builds();
        let long = format!("charge the invoice {}", "padding ".repeat(200));
        let (_, _, caps) = hits(result(&meaning, 1, &long, None));
        assert_eq!(
            caps.last().map(String::as_str),
            Some("query truncated to its first 1024 bytes")
        );
        let (_, _, caps) = hits(result(&meaning, 1, "charge the invoice", None));
        assert_eq!(caps, [SEMANTIC_LEADS_UNVERIFIED]);
    }

    /// Every way the index falls short of the repository is a cap in words.
    #[test]
    fn caps_for_should_name_the_sample_the_skips_and_the_store_failures() {
        let plain = ResidentStats::default();
        assert_eq!(caps_for(&plain, 5), [SEMANTIC_LEADS_UNVERIFIED]);
        let short = ResidentStats {
            files: 4,
            skipped_files: 1,
            credential_files: 2,
            eligible_files: 9,
            sampled_files: 5,
            file_limit_reached: true,
            vector_cache_errors: vec!["segment unreadable".into()],
            ..ResidentStats::default()
        };
        assert_eq!(
            caps_for(&short, 5),
            [
                SEMANTIC_LEADS_UNVERIFIED.to_string(),
                "the index holds a deterministic sample of 5 of 9 eligible files (resident limit 5)"
                    .to_string(),
                "1 eligible file(s) are not indexed: unreadable, over 512 KiB, binary or not UTF-8"
                    .to_string(),
                "2 eligible file(s) are never indexed: credential-shaped paths (.env, keys, secrets)"
                    .to_string(),
                "vector cache: segment unreadable; the affected chunks were embedded again"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn open_when_on_disk_should_report_a_missing_model_and_pass_a_load_error_through() {
        let missing = open_when_on_disk(false, || panic!("must not open"));
        assert_eq!(missing.err(), Some(OpenFailure::ModelMissing));
        let broken = open_when_on_disk(true, || Err("corrupt".to_string()));
        assert_eq!(broken.err(), Some(OpenFailure::Failed("corrupt".into())));
        let opened = open_when_on_disk(true, || {
            Ok(Box::new(WordEmbedder {
                passages: Arc::new(AtomicU64::new(0)),
                fail_queries: false,
            }) as Box<dyn Embedder>)
        });
        assert_eq!(opened.unwrap().model_id(), "fixture-words");
    }

    /// A model that fails on a question surfaces as an error, not as a
    /// `ready` answer without leads.
    #[test]
    fn answer_should_fail_when_the_model_fails_on_the_question() {
        let fixture = Fixture::new();
        let meaning = fixture.meaning();
        meaning.latest.store(1, Ordering::SeqCst);
        meaning.run_builds();
        *meaning.embedder() = Some(Box::new(WordEmbedder {
            passages: Arc::new(AtomicU64::new(0)),
            fail_queries: true,
        }));
        let error = meaning.answer(1, "charge the invoice", None).unwrap_err();
        assert!(error.contains("fixture model failed"), "{error}");
    }

    /// While a build holds the embedder a request cannot rank, and says the
    /// vectors are being replaced.
    #[test]
    fn answer_should_report_refreshing_when_a_build_holds_the_embedder() {
        let fixture = Fixture::new();
        let meaning = fixture.meaning();
        meaning.latest.store(1, Ordering::SeqCst);
        meaning.run_builds();
        let held = meaning.embedder().take();
        assert_eq!(
            reason(&result(&meaning, 1, "charge the invoice", None)),
            MeaningUnavailableReason::Refreshing
        );
        *meaning.embedder() = held;
        assert!(reason_or_ready(&result(&meaning, 1, "charge the invoice", None)).is_none());
    }
}
