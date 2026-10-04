// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Embedding seam: the pluggable model behind semantic search.
//!
//! The corpus side only ever sees this trait; the concrete model (fastembed
//! ONNX today) lives behind the `fastembed` cargo feature so the crate
//! builds and every lexical feature works with no ML dependency at all.

/// E5-family models embed queries and passages with different prefixes;
/// the kind travels with every batch so the impl can prepend correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedKind {
    Query,
    Passage,
}

pub trait Embedder: Send {
    /// Stable id recorded in the vector store — model swaps are detected,
    /// never silently mixed.
    fn model_id(&self) -> &str;
    fn dims(&self) -> usize;
    fn embed_batch(&mut self, texts: &[&str], kind: EmbedKind) -> Result<Vec<Vec<f32>>, String>;
}

/// Chunking: turns longer than this embed as several windows.
pub const CHUNK_MAX: usize = 1500;
pub const CHUNK_OVERLAP: usize = 200;

/// Deterministic chunk windows (byte offsets, char-boundary aligned) for a
/// turn's text — reproducible at query time for snippets.
pub fn chunk_offsets(text: &str) -> Vec<(usize, usize)> {
    if text.len() <= CHUNK_MAX {
        return vec![(0, text.len())];
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    loop {
        let mut end = (start + CHUNK_MAX).min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        out.push((start, end));
        if end >= text.len() {
            break;
        }
        let mut next = end.saturating_sub(CHUNK_OVERLAP);
        while next > 0 && !text.is_char_boundary(next) {
            next -= 1;
        }
        // Guarantee forward progress.
        start = next.max(start + 1);
    }
    out
}

/// The text actually embedded for a chunk: a tiny context header improves
/// "which session did X" queries at negligible cost.
pub fn embed_text(agent: &str, cwd: Option<&str>, role: &str, chunk: &str) -> String {
    let repo = cwd.and_then(|c| c.rsplit('/').next()).unwrap_or("-");
    format!("[{agent}] [{repo}] {role}: {chunk}")
}

pub const POTION_MODEL_ID: &str = "potion-multilingual-128m";
pub const E5_MODEL_ID: &str = "multilingual-e5-small-q";

/// The Hugging Face repository of the default potion model: the id a
/// model2vec embedder records in the vector store (`PIXEL_RECALL_MODEL_REPO`
/// can name another one).
pub const POTION_REPO: &str = "minishlab/potion-multilingual-128M";

/// Revision of what a model loaded through model2vec-rs embeds. Bump it when
/// a library change moves the vectors of unchanged text, with a line below:
/// a store written at another revision re-embeds itself (`reset_if_stale`).
///
/// History: 0 = model2vec-rs 0.2; 1 = model2vec-rs 0.3 drops the unknown
/// token of Unigram tokenizers (the default potion tokenizer is Unigram
/// without byte fallback), which moved 133 of 300 sampled turns (cosine
/// median 1.0000, minimum 0.928).
pub const MODEL2VEC_REVISION: u32 = 1;

/// Revision of what the fastembed E5 model (`E5_MODEL_ID`) embeds.
pub const E5_REVISION: u32 = 0;

/// The revision of what `model_id` embeds today. Two embedders exist: the
/// fastembed E5 one records `E5_MODEL_ID`, and every model2vec one records
/// its repository (the default potion, one named by
/// `PIXEL_RECALL_MODEL_REPO`, the potion-code models), all at
/// `MODEL2VEC_REVISION`.
pub fn embedder_revision(model_id: &str) -> u32 {
    if model_id == E5_MODEL_ID {
        E5_REVISION
    } else {
        MODEL2VEC_REVISION
    }
}

/// Empty a vector store written at another revision of its model and mark
/// every turn for embedding again, so the next backfill rebuilds it with no
/// command to run. Returns whether it reset.
///
/// The turns are queued before the vectors go: a failure (or a stop)
/// between the two leaves the store still stale, so the next pass resets it
/// again. The other order could leave an empty store marked current with
/// every turn still marked embedded, which nothing would ever rebuild.
pub fn reset_if_stale(
    store: &crate::store::RecallStore,
    vectors: &mut crate::vector::VectorStore,
) -> Result<bool, String> {
    if !vectors.stale_revision() {
        return Ok(false);
    }
    store.reset_embeddings().map_err(|e| e.to_string())?;
    vectors.reset_for_reembed()?;
    Ok(true)
}

/// The model id the existing vector store was built with, if any.
fn stored_model_id() -> Option<String> {
    let bytes = std::fs::read(crate::vectors_dir().join("meta.json")).ok()?;
    let meta: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let id = meta.get("model_id")?.as_str()?.to_string();
    (!id.is_empty()).then_some(id)
}

/// Open the default embedding model from the shared model cache.
///
/// Resolution: `PIXEL_RECALL_MODEL` env ("potion" | "e5") → the model
/// the existing vector store was built with → potion (the fast static
/// tier; ~3 orders of magnitude faster than transformer inference, which
/// makes the full-corpus backfill minutes instead of hours). Errors when
/// the build lacks embedding support or the model is absent and
/// `download` is false — callers treat that as "semantic channel
/// unavailable", never as a crash.
pub fn open_default_embedder(download: bool) -> Result<Box<dyn Embedder>, String> {
    open_embedder_with_potion_repo(download, None)
}

pub(crate) fn open_embedder_with_potion_repo(
    download: bool,
    potion_repo: Option<&str>,
) -> Result<Box<dyn Embedder>, String> {
    let choice = match std::env::var("PIXEL_RECALL_MODEL") {
        Ok(v) if !v.is_empty() => v,
        _ => stored_model_id().unwrap_or_else(|| POTION_MODEL_ID.to_string()),
    };
    open_selected_embedder(&choice, download, potion_repo)
}

fn open_selected_embedder(
    choice: &str,
    download: bool,
    potion_repo: Option<&str>,
) -> Result<Box<dyn Embedder>, String> {
    if choice.contains("e5") {
        #[cfg(feature = "fastembed")]
        {
            return fast::FastEmbedder::open(&crate::models_dir(), download)
                .map(|e| Box::new(e) as Box<dyn Embedder>);
        }
        #[cfg(not(feature = "fastembed"))]
        return Err("e5 requested but this build lacks the fastembed feature".to_string());
    }
    #[cfg(feature = "model2vec")]
    {
        match potion_repo {
            Some(repo) => potion::PotionEmbedder::open_repo(&crate::models_dir(), download, repo),
            None => potion::PotionEmbedder::open(&crate::models_dir(), download),
        }
        .map(|e| Box::new(e) as Box<dyn Embedder>)
    }
    #[cfg(not(feature = "model2vec"))]
    {
        let _ = (download, potion_repo);
        Err("this build has no embedding support (model2vec feature disabled)".to_string())
    }
}

#[derive(Debug, Default)]
pub struct BackfillReport {
    pub turns_embedded: usize,
    pub chunks_written: usize,
    pub segments_written: usize,
    pub backlog_remaining: i64,
    pub elapsed_ms: u128,
}

const BATCH_TURNS: usize = 128;
const FLUSH_CHUNKS: usize = 8192;

/// Drain the embed backlog into vector segments. Resumable: an interrupted
/// run leaves turns unmarked and orphan chunk rows, both healed on entry.
/// A store written at another revision of its model is emptied first and
/// rebuilt (`reset_if_stale`).
pub fn run_backfill(
    store: &crate::store::RecallStore,
    vectors: &mut crate::vector::VectorStore,
    embedder: &mut dyn Embedder,
    progress: impl FnMut(usize, i64),
) -> Result<BackfillReport, String> {
    run_backfill_limited(store, vectors, embedder, usize::MAX, progress)
}

/// `run_backfill` that stops once `max_turns` turns are embedded (whole
/// batches, so it may pass the limit by up to one batch): the daemon drains
/// a large backlog a slice per pass instead of blocking its socket.
pub fn run_backfill_limited(
    store: &crate::store::RecallStore,
    vectors: &mut crate::vector::VectorStore,
    embedder: &mut dyn Embedder,
    max_turns: usize,
    mut progress: impl FnMut(usize, i64),
) -> Result<BackfillReport, String> {
    let started = std::time::Instant::now();
    let mut report = BackfillReport::default();
    reset_if_stale(store, vectors)?;
    store
        .drop_orphan_chunks(vectors.meta.last_chunk_id)
        .map_err(|e| e.to_string())?;
    vectors.check_model(embedder.model_id(), embedder.dims())?;

    let mut pending_rows: Vec<(i64, Vec<f32>)> = Vec::new();
    let mut pending_turns: Vec<i64> = Vec::new();
    store.mark_policy_skips().map_err(|e| e.to_string())?;
    // Keyset cursor over turn ids: rows behind it are either flushed or
    // sitting in `pending_rows` awaiting flush — never re-fetched.
    let mut after_id = 0i64;
    while report.turns_embedded < max_turns {
        let batch = store
            .pending_embed(after_id, BATCH_TURNS)
            .map_err(|e| e.to_string())?;
        if batch.is_empty() {
            // Ingest may have added policy-excluded turns concurrently;
            // sweep once more and only stop when both queues are empty.
            let swept = store.mark_policy_skips().map_err(|e| e.to_string())?;
            if swept == 0 {
                break;
            }
            continue;
        }
        after_id = batch.last().map_or(after_id, |t| t.turn_id);
        let mut texts: Vec<String> = Vec::new();
        let mut chunk_ids: Vec<i64> = Vec::new();
        for turn in &batch {
            let offsets = chunk_offsets(&turn.text);
            let ids = store
                .insert_chunks(turn.turn_id, &offsets)
                .map_err(|e| e.to_string())?;
            for ((start, end), id) in offsets.iter().zip(&ids) {
                texts.push(embed_text(
                    &turn.agent,
                    turn.cwd.as_deref(),
                    &turn.role,
                    &turn.text[*start..*end],
                ));
                chunk_ids.push(*id);
            }
        }
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let vecs = embedder.embed_batch(&refs, EmbedKind::Passage)?;
        if vecs.len() != chunk_ids.len() {
            return Err("embedding count mismatch".to_string());
        }
        pending_rows.extend(chunk_ids.into_iter().zip(vecs));
        pending_turns.extend(batch.iter().map(|t| t.turn_id));
        report.turns_embedded += batch.len();

        if pending_rows.len() >= FLUSH_CHUNKS {
            flush(
                store,
                vectors,
                embedder,
                &mut pending_rows,
                &mut pending_turns,
                &mut report,
            )?;
            let backlog = store.embed_backlog().map_err(|e| e.to_string())?;
            progress(report.turns_embedded, backlog);
        }
    }
    if !pending_rows.is_empty() {
        flush(
            store,
            vectors,
            embedder,
            &mut pending_rows,
            &mut pending_turns,
            &mut report,
        )?;
    }
    report.backlog_remaining = store.embed_backlog().map_err(|e| e.to_string())?;
    report.elapsed_ms = started.elapsed().as_millis();
    Ok(report)
}

fn flush(
    store: &crate::store::RecallStore,
    vectors: &mut crate::vector::VectorStore,
    embedder: &dyn Embedder,
    rows: &mut Vec<(i64, Vec<f32>)>,
    turns: &mut Vec<i64>,
    report: &mut BackfillReport,
) -> Result<(), String> {
    report.chunks_written += rows.len();
    vectors.append_segment(embedder.model_id(), embedder.dims(), rows)?;
    report.segments_written += 1;
    store.mark_embedded(turns).map_err(|e| e.to_string())?;
    rows.clear();
    turns.clear();
    Ok(())
}

#[cfg(feature = "model2vec")]
pub mod potion {
    use super::{EmbedKind, Embedder};
    use std::path::Path;

    /// Static-embedding fast tier (Model2Vec potion-multilingual-128M,
    /// 256d, distilled from bge-m3). No transformer inference: token
    /// lookups + pooling, so bulk backfill runs at tens of thousands of
    /// texts per second on CPU.
    pub struct PotionEmbedder {
        model: model2vec_rs::model::StaticModel,
        dims: usize,
        model_id: String,
    }

    const REPO: &str = super::POTION_REPO;

    /// Resolve the transcript model override. Repository `ask` passes its own
    /// model repository explicitly, without changing this process-wide choice.
    fn resolved_repo() -> String {
        match std::env::var("PIXEL_RECALL_MODEL_REPO") {
            Ok(v) if !v.is_empty() => v,
            _ => REPO.to_string(),
        }
    }

    /// Refuse a load that may not download when `repo` was never set up.
    /// Set up means its download finished, or a legacy marker exists at all:
    /// that marker proved some model was set up and named only the last one,
    /// so it keeps admitting every model as it did before per-repository
    /// markers.
    pub(crate) fn require_set_up(
        cache_dir: &Path,
        repo: &str,
        download: bool,
    ) -> Result<(), String> {
        if download
            || crate::potion_cached(cache_dir, repo)
            || cache_dir.join(crate::LEGACY_POTION_MARKER).is_file()
        {
            Ok(())
        } else {
            Err("embedding model not present — run `pixel recall setup` first".to_string())
        }
    }

    /// The step after a model loaded with `dims`-wide embeddings: refuse a
    /// model that embeds nothing, then, for a download, write `repo`'s own
    /// marker, the one [`crate::potion_cached`] reads. A load that failed
    /// never reaches it, so it leaves no marker.
    pub(crate) fn finish_load(
        cache_dir: &Path,
        repo: &str,
        download: bool,
        dims: usize,
    ) -> Result<usize, String> {
        if dims == 0 {
            return Err("potion model produced empty embeddings".to_string());
        }
        if download {
            // The model is loaded and usable either way: a marker that cannot
            // be written only costs a later cache check, so it is reported,
            // not turned into a load failure.
            let marker = crate::potion_marker(cache_dir, repo);
            if let Err(error) = std::fs::write(&marker, repo) {
                eprintln!(
                    "pixel: potion model loaded, but its download marker {} could not be written: {error}",
                    marker.display()
                );
            }
        }
        Ok(dims)
    }

    impl PotionEmbedder {
        pub fn open(cache_dir: &Path, download: bool) -> Result<Self, String> {
            Self::open_repo(cache_dir, download, &resolved_repo())
        }

        /// Select a repository without mutating transcript recall's process-wide model choice.
        pub(crate) fn open_repo(
            cache_dir: &Path,
            download: bool,
            repo: &str,
        ) -> Result<Self, String> {
            require_set_up(cache_dir, repo, download)?;
            let _ = std::fs::create_dir_all(cache_dir);
            // Route the HF hub cache under pixel's model dir.
            // SAFETY: set_var is process-global; both CLI and daemon call this
            // before any thread that reads the environment exists.
            unsafe {
                std::env::set_var("HF_HOME", cache_dir.join("hf"));
            }
            let model = model2vec_rs::model::StaticModel::from_pretrained(repo, None, None, None)
                .map_err(|e| format!("potion model load: {e}"))?;
            let dims = finish_load(
                cache_dir,
                repo,
                download,
                model.encode_single("probe").len(),
            )?;
            Ok(Self {
                model,
                dims,
                model_id: repo.to_string(),
            })
        }
    }

    impl Embedder for PotionEmbedder {
        fn model_id(&self) -> &str {
            &self.model_id
        }

        fn dims(&self) -> usize {
            self.dims
        }

        // Runs the real model (downloaded or cached on disk); the
        // `Embedder` trait is what the pipeline and its tests drive.
        #[cfg_attr(test, mutants::skip)]
        fn embed_batch(
            &mut self,
            texts: &[&str],
            _kind: EmbedKind, // static embeddings have no query/passage split
        ) -> Result<Vec<Vec<f32>>, String> {
            let owned: Vec<String> = texts.iter().map(ToString::to_string).collect();
            Ok(self.model.encode_with_args(&owned, Some(512), 1024))
        }
    }
}

#[cfg(feature = "fastembed")]
pub mod fast {
    use super::{EmbedKind, Embedder};

    pub struct FastEmbedder {
        model: fastembed::TextEmbedding,
        model_id: String,
        dims: usize,
    }

    impl FastEmbedder {
        /// Load multilingual-e5-small (int8 ONNX) from the local cache dir;
        /// `download` controls whether a missing model may be fetched.
        pub fn open(cache_dir: &std::path::Path, download: bool) -> Result<Self, String> {
            if !download && !cache_dir.exists() {
                return Err(
                    "embedding model not present — run `pixel recall setup` first".to_string(),
                );
            }
            let options =
                fastembed::TextInitOptions::new(fastembed::EmbeddingModel::MultilingualE5Small)
                    .with_cache_dir(cache_dir.to_path_buf())
                    .with_show_download_progress(download);
            let model = fastembed::TextEmbedding::try_new(options)
                .map_err(|e| format!("embedding model init: {e}"))?;
            Ok(Self {
                model,
                model_id: "multilingual-e5-small-q".to_string(),
                dims: 384,
            })
        }
    }

    impl Embedder for FastEmbedder {
        fn model_id(&self) -> &str {
            &self.model_id
        }

        fn dims(&self) -> usize {
            self.dims
        }

        fn embed_batch(
            &mut self,
            texts: &[&str],
            kind: EmbedKind,
        ) -> Result<Vec<Vec<f32>>, String> {
            // E5 prefix convention — encoded here, never at call sites.
            let prefix = match kind {
                EmbedKind::Query => "query: ",
                EmbedKind::Passage => "passage: ",
            };
            let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
            self.model
                .embed(prefixed, None)
                .map_err(|e| format!("embed: {e}"))
        }
    }
}

#[cfg(all(test, feature = "model2vec"))]
mod potion_gate_tests {
    use super::potion::{finish_load, require_set_up};

    const CODE_16M: &str = "minishlab/potion-code-16M-v2";
    const CODE_64M: &str = "minishlab/potion-code-64M-v2";

    #[test]
    fn require_set_up_should_refuse_only_a_model_never_set_up_without_download() {
        let dir = tempfile::tempdir().unwrap();
        let err = require_set_up(dir.path(), CODE_16M, false).unwrap_err();
        assert!(err.contains("run `pixel recall setup`"), "{err}");
        assert_eq!(require_set_up(dir.path(), CODE_16M, true), Ok(()));

        std::fs::write(crate::potion_marker(dir.path(), CODE_16M), CODE_16M).unwrap();
        assert_eq!(require_set_up(dir.path(), CODE_16M, false), Ok(()));
        assert!(require_set_up(dir.path(), CODE_64M, false).is_err());
    }

    /// A download that loaded writes this repository's marker, which is what
    /// makes the daemon see its model as cached; a load without download,
    /// or one that produced no embedding, writes none.
    #[test]
    fn finish_load_should_mark_only_a_successful_download() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(finish_load(dir.path(), CODE_64M, true, 256), Ok(256));
        assert_eq!(
            std::fs::read_to_string(crate::potion_marker(dir.path(), CODE_64M)).unwrap(),
            CODE_64M,
            "the repository's own marker, not the shared legacy one"
        );
        assert!(!dir.path().join(crate::LEGACY_POTION_MARKER).exists());
        assert!(!crate::potion_cached(dir.path(), CODE_16M));

        assert_eq!(finish_load(dir.path(), CODE_16M, false, 256), Ok(256));
        assert!(!crate::potion_cached(dir.path(), CODE_16M));

        assert!(finish_load(dir.path(), CODE_16M, true, 0).is_err());
        assert!(!crate::potion_cached(dir.path(), CODE_16M));
    }

    /// Before per-repository markers, one `potion.ok` admitted every model;
    /// an upgrade must not refuse a model that loaded the day before.
    #[test]
    fn a_legacy_marker_should_still_admit_any_model() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(crate::LEGACY_POTION_MARKER), CODE_16M).unwrap();
        assert_eq!(require_set_up(dir.path(), CODE_64M, false), Ok(()));
    }
}

#[cfg(all(test, not(feature = "fastembed")))]
mod model_selection_tests {
    #[test]
    fn explicit_e5_is_not_replaced_by_code_potion_override() {
        let error = super::open_selected_embedder(
            "multilingual-e5-small",
            false,
            Some("minishlab/potion-code-16M-v2"),
        )
        .err()
        .unwrap();
        assert!(error.contains("e5 requested"), "{error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubEmbedder;

    impl Embedder for StubEmbedder {
        fn model_id(&self) -> &str {
            "stub"
        }
        fn dims(&self) -> usize {
            4
        }
        fn embed_batch(
            &mut self,
            texts: &[&str],
            _kind: EmbedKind,
        ) -> Result<Vec<Vec<f32>>, String> {
            Ok(texts.iter().map(|_| vec![0.5; 4]).collect())
        }
    }

    /// Regression: turns are only marked embedded at segment flush, so the
    /// backfill must page by id — a head query would re-chunk and re-embed
    /// the same batch until the flush threshold (once produced a 65×
    /// duplication of every chunk on the real corpus).
    #[test]
    fn backfill_embeds_each_turn_exactly_once() {
        use crate::model::{Role, TsSource, UnifiedSession, UnifiedTurn};
        let dir = std::env::temp_dir().join(format!("gpx-embed-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = crate::store::RecallStore::open(&dir.join("recall.db")).unwrap();
        let session = UnifiedSession {
            agent: "claude",
            source_session_id: "s1".into(),
            source_path: "test".into(),
            cwd: Some("/tmp/x".into()),
            git_branch: None,
            title: None,
            ts_source: TsSource::Iso,
            is_subagent: false,
            parent_source_session_id: None,
        };
        // More turns than one batch so the loop must page past BATCH_TURNS
        // without a flush in between (FLUSH_CHUNKS >> turn count here).
        let turns: Vec<UnifiedTurn> = (0..300)
            .map(|i| UnifiedTurn {
                role: Role::Assistant,
                intent_source: None,
                ts: Some(i),
                text: format!("turn number {i}"),
                truncated: false,
                source_byte_start: None,
                source_byte_len: None,
            })
            .collect();
        let st = crate::store::IngestState {
            file_size: 1,
            mtime_ms: 1,
            bytes_ingested: 1,
            cursor: None,
        };
        store.replace_session(&session, &turns, "u1", &st).unwrap();

        let mut vectors = crate::vector::VectorStore::open(&dir.join("vectors")).unwrap();
        let mut embedder = StubEmbedder;
        let report = run_backfill(&store, &mut vectors, &mut embedder, |_, _| {}).unwrap();
        assert_eq!(report.turns_embedded, 300);
        let chunk_count: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM vector_chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            chunk_count, 300,
            "each short turn must yield exactly one chunk"
        );
        assert_eq!(store.embed_backlog().unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An embedder that answers under a given model id.
    struct NamedStub(&'static str);

    impl Embedder for NamedStub {
        fn model_id(&self) -> &str {
            self.0
        }
        fn dims(&self) -> usize {
            4
        }
        fn embed_batch(
            &mut self,
            texts: &[&str],
            _kind: EmbedKind,
        ) -> Result<Vec<Vec<f32>>, String> {
            Ok(texts.iter().map(|_| vec![0.5; 4]).collect())
        }
    }

    /// A store of `n` short turns in one session, plus its vector dir.
    fn corpus(
        tag: &str,
        n: i64,
    ) -> (
        tempfile::TempDir,
        crate::store::RecallStore,
        std::path::PathBuf,
    ) {
        use crate::model::{Role, TsSource, UnifiedSession, UnifiedTurn};
        let dir = tempfile::Builder::new().prefix(tag).tempdir().unwrap();
        let mut store = crate::store::RecallStore::open(&dir.path().join("recall.db")).unwrap();
        let session = UnifiedSession {
            agent: "claude",
            source_session_id: "s1".into(),
            source_path: "test".into(),
            cwd: Some("/tmp/x".into()),
            git_branch: None,
            title: None,
            ts_source: TsSource::Iso,
            is_subagent: false,
            parent_source_session_id: None,
        };
        let turns: Vec<UnifiedTurn> = (0..n)
            .map(|i| UnifiedTurn {
                role: Role::Assistant,
                intent_source: None,
                ts: Some(i),
                text: format!("turn number {i}"),
                truncated: false,
                source_byte_start: None,
                source_byte_len: None,
            })
            .collect();
        let st = crate::store::IngestState {
            file_size: 1,
            mtime_ms: 1,
            bytes_ingested: 1,
            cursor: None,
        };
        store.replace_session(&session, &turns, "u1", &st).unwrap();
        let vectors = dir.path().join("vectors");
        (dir, store, vectors)
    }

    /// Rewrite the store's meta as an older build left it: no revision.
    fn age_meta(vectors: &std::path::Path) {
        let path = vectors.join("meta.json");
        let mut meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        meta.as_object_mut().unwrap().remove("revision");
        std::fs::write(&path, serde_json::to_vec(&meta).unwrap()).unwrap();
    }

    /// The ids are what the embedders record: a model2vec model's is its
    /// repository (the default, or any other potion one), E5's its own id.
    #[test]
    fn embedder_revision_is_bumped_for_model2vec_models_only() {
        assert_eq!(embedder_revision(POTION_REPO), 1);
        assert_eq!(embedder_revision("minishlab/potion-code-16M-v2"), 1);
        assert_eq!(embedder_revision(E5_MODEL_ID), 0);
    }

    /// Vectors of an older revision of the same model are dropped and every
    /// turn is queued again, the model kept; a current store is left alone.
    #[test]
    fn reset_if_stale_requeues_every_turn_of_a_store_from_an_older_revision() {
        let (_dir, store, vdir) = corpus("gpx-stale", 5);
        let mut vectors = crate::vector::VectorStore::open(&vdir).unwrap();
        run_backfill(&store, &mut vectors, &mut NamedStub(POTION_REPO), |_, _| {}).unwrap();
        assert_eq!(vectors.meta.revision, MODEL2VEC_REVISION);
        assert!(
            !reset_if_stale(&store, &mut vectors).unwrap(),
            "current store"
        );
        assert_eq!(store.embed_backlog().unwrap(), 0);

        age_meta(&vdir);
        let mut vectors = crate::vector::VectorStore::open(&vdir).unwrap();
        assert!(reset_if_stale(&store, &mut vectors).unwrap());
        assert!(vectors.meta.segments.is_empty());
        assert_eq!(vectors.meta.model_id, POTION_REPO, "the model stays bound");
        assert_eq!(vectors.meta.revision, MODEL2VEC_REVISION);
        assert_eq!(store.embed_backlog().unwrap(), 5);
        assert!(
            !reset_if_stale(&store, &mut vectors).unwrap(),
            "once is enough"
        );
    }

    /// A failed re-queue leaves the vectors untouched and the store stale,
    /// so the next pass tries again instead of trusting an empty store.
    #[test]
    fn reset_if_stale_keeps_the_vectors_when_the_turns_cannot_be_requeued() {
        let (_dir, store, vdir) = corpus("gpx-requeue", 5);
        let mut vectors = crate::vector::VectorStore::open(&vdir).unwrap();
        run_backfill(&store, &mut vectors, &mut NamedStub(POTION_REPO), |_, _| {}).unwrap();
        age_meta(&vdir);
        let mut vectors = crate::vector::VectorStore::open(&vdir).unwrap();
        store
            .connection()
            .execute_batch("DROP TABLE vector_chunks")
            .unwrap();
        assert!(reset_if_stale(&store, &mut vectors).is_err());
        assert_eq!(vectors.meta.segments.len(), 1, "vectors kept");
        let reread = crate::vector::VectorStore::open(&vdir).unwrap();
        assert!(
            reread.stale_revision(),
            "still stale on disk: the next pass retries"
        );
    }

    /// The rebuild needs no command: a backfill over a stale store embeds
    /// the whole corpus again at the current revision.
    #[test]
    fn run_backfill_rebuilds_a_store_from_an_older_revision() {
        let (_dir, store, vdir) = corpus("gpx-rebuild", 5);
        let mut vectors = crate::vector::VectorStore::open(&vdir).unwrap();
        run_backfill(&store, &mut vectors, &mut NamedStub(POTION_REPO), |_, _| {}).unwrap();
        age_meta(&vdir);
        let mut vectors = crate::vector::VectorStore::open(&vdir).unwrap();
        let report =
            run_backfill(&store, &mut vectors, &mut NamedStub(POTION_REPO), |_, _| {}).unwrap();
        assert_eq!(report.turns_embedded, 5);
        assert!(!vectors.stale_revision());
        assert_eq!(vectors.meta.segments.len(), 1, "old segments gone, one new");
    }

    /// The limit stops the drain after the batch that reaches it, so the
    /// daemon embeds a bounded slice per pass and the rest stays queued.
    #[test]
    fn run_backfill_limited_stops_after_the_batch_that_reaches_the_limit() {
        let (_dir, store, vdir) = corpus("gpx-limited", 300);
        let mut vectors = crate::vector::VectorStore::open(&vdir).unwrap();
        let first = run_backfill_limited(
            &store,
            &mut vectors,
            &mut StubEmbedder,
            BATCH_TURNS,
            |_, _| {},
        )
        .unwrap();
        assert_eq!(first.turns_embedded, BATCH_TURNS);
        assert_eq!(first.backlog_remaining, 300 - BATCH_TURNS as i64);
        let rest = run_backfill(&store, &mut vectors, &mut StubEmbedder, |_, _| {}).unwrap();
        assert_eq!(rest.turns_embedded, 300 - BATCH_TURNS);
        assert_eq!(store.embed_backlog().unwrap(), 0);
    }

    #[test]
    fn chunking_covers_and_overlaps() {
        let text = "x".repeat(4000);
        let chunks = chunk_offsets(&text);
        assert!(chunks.len() >= 3);
        assert_eq!(chunks[0].0, 0);
        assert_eq!(chunks.last().unwrap().1, 4000);
        for w in chunks.windows(2) {
            assert!(w[1].0 < w[0].1, "windows must overlap");
        }
        assert_eq!(chunk_offsets("short"), vec![(0, 5)]);
    }
}
