// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! The chunk vectors of one code tree, kept in memory between questions.
//!
//! [`crate::code_search::ask`] reads, cuts and tokenizes every file of the
//! tree for each question, which is what costs a CLI process its 250 ms to
//! 1.2 s. A long-lived process (the daemon) answers many, so a [`Resident`]
//! does that work once: every eligible file cut into chunks
//! ([`crate::code_chunks`]), each chunk with its unit-length vector, its term
//! counts for the lexical channel and the line range, symbol and one-line
//! snippet a hit reports. A question then costs one query embedding, one pass
//! over the vectors and one over the term counts.
//!
//! It ranks as `ask` does: the same file universe ([`collect_files`]), the
//! same chunk texts (so the vectors of `.pixel/code-vectors/` serve both),
//! the same BM25 over chunks ([`pixel_rank::bm25_scores`]) and the same
//! reciprocal-rank fusion and demotion of tests, configuration and docs
//! ([`rank_files`]). A hit is a file's best chunk by cosine.
//!
//! [`Resident::build`] with the previous index refreshes it: a file whose
//! bytes hash as before keeps its chunks, so a refresh after an edit parses
//! and embeds only what changed. The file list comes from a new walk every
//! time, so added, removed and newly ignored files need no change tracking.
//! Only a build without a previous index reads and writes the vector store;
//! a refresh embeds its changed chunks in memory, because a store write has
//! to know every live chunk of the tree.
//!
//! Embeddings rank, they do not decide relevance: the best cosine of an
//! unrelated question is within a few hundredths of a related one's.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use rayon::prelude::*;

use crate::code_chunks::named_chunks;
use crate::code_search::{
    AskCoverage, CHUNKER_VERSION, CorpusEntry, Lexical, MAX_FILE_BYTES, VectorCache, chunk_vectors,
    collect_files, for_each_word, lexical_chunk, make_snippet, rank_files, sorted_query_terms,
    validate_vector,
};
use crate::code_vectors::{ChunkKey, Namespace, Store};
use crate::embed::{EmbedKind, Embedder, embedder_revision};

/// Most eligible files a resident index holds. Measured on this repository,
/// the index takes 2 022 bytes per chunk at 256 dimensions (29 MB for 14 377
/// chunks of 851 files), so 5 000 files hold some 170 MB. Above it the index
/// holds a deterministic sample ([`ResidentStats`]).
pub const RESIDENT_MAX_FILES: usize = 5_000;

/// One ranked hit: the best chunk of a file for the question.
#[derive(Debug, Clone, PartialEq)]
pub struct ResidentHit {
    /// Repository-relative path.
    pub path: String,
    /// Inclusive 1-based line range of the chunk.
    pub start_line: u32,
    pub end_line: u32,
    /// The symbol the chunk belongs to ([`crate::code_chunks::NamedChunk`]).
    pub symbol: Option<String>,
    /// The fused ranking score that orders the hits ([`rank_files`]).
    pub score: f64,
    /// The head of the chunk on one line, 160 characters at most.
    pub snippet: String,
}

/// What a [`Resident`] holds and what its build left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResidentStats {
    pub model_id: String,
    pub dims: usize,
    /// Files and chunks held.
    pub files: usize,
    pub chunks: usize,
    /// Eligible files under the root, whether held or not.
    pub eligible_files: usize,
    /// More files were eligible than the build's limit: a deterministic
    /// sample of [`ResidentStats::sampled_files`] of them was read.
    pub file_limit_reached: bool,
    /// Files the build was handed to read: all eligible files, or the sample.
    pub sampled_files: usize,
    /// Eligible files left out: unreadable, over [`MAX_FILE_BYTES`], binary
    /// or not UTF-8.
    pub skipped_files: usize,
    /// Eligible files never read because their path is credential-shaped
    /// (`.env`, keys, anything named for a secret): a hit shows the head of
    /// its chunk, and this index serves agents.
    pub credential_files: usize,
    /// Files whose chunks came from the previous index.
    pub reused_files: usize,
    /// Chunks the build embedded, and chunks it took from the vector store.
    pub embedded_chunks: usize,
    pub cached_chunks: usize,
    /// Approximate memory held, in bytes.
    pub bytes: u64,
    /// Failures of the vector store that cost the build its saving, in words.
    pub vector_cache_errors: Vec<String>,
}

/// The chunks of one file at one content hash.
struct FileChunks {
    path: String,
    hash: u128,
    chunks: Vec<Chunk>,
}

struct Chunk {
    start_line: u32,
    end_line: u32,
    symbol: Option<String>,
    snippet: String,
    /// The embedding scaled to unit length, so a cosine is a dot product.
    vector: Box<[f32]>,
    /// `(token id, count)` sorted by id: the chunk as a BM25 document,
    /// filename-stem tokens included.
    terms: Box<[(u32, u32)]>,
    /// Tokens of the chunk, repeats and stem tokens included.
    len: u32,
}

/// The tokens of every chunk, numbered. Append-only across refreshes, so the
/// ids a reused file carries stay valid.
#[derive(Clone, Default)]
struct Vocab {
    ids: HashMap<Box<str>, u32>,
}

impl Vocab {
    fn intern(&mut self, token: String) -> u32 {
        if let Some(&id) = self.ids.get(token.as_str()) {
            return id;
        }
        let id = u32::try_from(self.ids.len()).expect("fewer than 2^32 distinct tokens");
        self.ids.insert(token.into_boxed_str(), id);
        id
    }

    fn get(&self, token: &str) -> Option<u32> {
        self.ids.get(token).copied()
    }
}

/// The chunk vectors, term counts and metadata of a code tree, in memory.
pub struct Resident {
    model_id: String,
    dims: usize,
    /// Sorted by path.
    files: Vec<Arc<FileChunks>>,
    vocab: Vocab,
    stats: ResidentStats,
}

/// One file as the parallel stage found it.
enum Loaded {
    /// Unreadable, over [`MAX_FILE_BYTES`], binary or not UTF-8.
    Skipped,
    /// Blank.
    Empty,
    /// Its bytes hash as in the previous index.
    Reused(Arc<FileChunks>),
    Parsed(ParsedFile),
}

struct ParsedFile {
    path: String,
    hash: u128,
    chunks: Vec<ParsedChunk>,
}

struct ParsedChunk {
    start_line: u32,
    end_line: u32,
    symbol: Option<String>,
    snippet: String,
    /// The text the model embeds, taken once the vectors are asked for.
    text: String,
    terms: Box<[(u32, u32)]>,
    len: u32,
}

impl Resident {
    /// Read `root` into a resident index with `embedder`'s vectors.
    ///
    /// `previous` is the index being refreshed: its files whose bytes are
    /// unchanged are reused when it was built with the same model, and its
    /// vocabulary is extended. `cache` says whether a build without a
    /// previous index reads and writes `<root>/.pixel/code-vectors/`;
    /// `max_files` bounds the files held.
    ///
    /// # Errors
    ///
    /// The embedder fails, or returns a vector the model cannot have meant
    /// (wrong dimensions, zero or not finite).
    pub fn build(
        root: &Path,
        previous: Option<&Resident>,
        embedder: &mut dyn Embedder,
        cache: VectorCache,
        max_files: usize,
    ) -> Result<Self, String> {
        let (files, walk) = collect_files(root, Some(max_files));
        let sampled_files = files.len();
        let (files, credential): (Vec<_>, Vec<_>) = files.into_iter().partition(|file| {
            !pixel_index::index::credential_path(file.strip_prefix(root).unwrap_or(file))
        });
        let model_id = embedder.model_id().to_string();
        let dims = embedder.dims();
        let namespace = Namespace::new(&model_id, embedder_revision(&model_id), CHUNKER_VERSION);
        let previous = previous.filter(|p| p.model_id == model_id && p.dims == dims);
        let reusable: HashMap<&str, &Arc<FileChunks>> = previous
            .into_iter()
            .flat_map(|p| &p.files)
            .map(|file| (file.path.as_str(), file))
            .collect();

        // Reading, hashing and parsing dominate a build: one file per task,
        // each numbering its tokens under one short hold of the vocabulary.
        let vocab = Mutex::new(previous.map(|p| p.vocab.clone()).unwrap_or_default());
        let loaded: Vec<Loaded> = files
            .par_iter()
            .map(|file| load_file(root, file, &reusable, &vocab))
            .collect();
        let vocab = vocab.into_inner().unwrap_or_else(PoisonError::into_inner);

        let mut stats = ResidentStats {
            model_id,
            dims,
            eligible_files: walk.candidate_files,
            file_limit_reached: walk.file_limit_reached,
            sampled_files,
            credential_files: credential.len(),
            ..ResidentStats::default()
        };
        let mut kept: Vec<Arc<FileChunks>> = Vec::with_capacity(loaded.len());
        let mut parsed: Vec<ParsedFile> = Vec::new();
        for file in loaded {
            match file {
                Loaded::Skipped => stats.skipped_files += 1,
                Loaded::Empty => {}
                Loaded::Reused(file) => {
                    stats.reused_files += 1;
                    kept.push(file);
                }
                Loaded::Parsed(file) => parsed.push(file),
            }
        }

        let (fresh, embedding) = embed_parsed(
            parsed,
            &namespace,
            embedder,
            // A store write needs every live chunk of the tree, which only a
            // build without a previous index has in hand.
            (cache == VectorCache::Persisted && previous.is_none()).then(|| {
                Store::at(
                    &root
                        .join(pixel_index::index::SHARD_DIR)
                        .join(crate::code_vectors::DIR),
                )
            }),
        )?;
        stats.embedded_chunks = embedding.embedded_chunks;
        stats.cached_chunks = embedding.cached_chunks;
        stats.vector_cache_errors = embedding.vector_cache_errors;
        kept.extend(fresh);
        kept.sort_by(|a, b| a.path.cmp(&b.path));

        stats.files = kept.len();
        stats.chunks = kept.iter().map(|file| file.chunks.len()).sum();
        stats.bytes = approximate_bytes(&kept, &vocab);
        Ok(Self {
            model_id: stats.model_id.clone(),
            dims,
            files: kept,
            vocab,
            stats,
        })
    }

    pub fn stats(&self) -> &ResidentStats {
        &self.stats
    }

    /// The `k` best files for `query`: each with the lines, symbol and
    /// snippet of its best chunk, best first, ties by path.
    ///
    /// # Errors
    ///
    /// `embedder` is not the model the index was built with, or fails, or
    /// returns a vector the model cannot have meant.
    pub fn search(
        &self,
        embedder: &mut dyn Embedder,
        query: &str,
        k: usize,
    ) -> Result<Vec<ResidentHit>, String> {
        if embedder.model_id() != self.model_id || embedder.dims() != self.dims {
            return Err(format!(
                "the index holds {} vectors of {} dimensions, the embedder is {} at {}",
                self.model_id,
                self.dims,
                embedder.model_id(),
                embedder.dims()
            ));
        }
        let query_vector = embedder
            .embed_batch(&[query], EmbedKind::Query)?
            .into_iter()
            .next()
            .ok_or("empty query embedding")?;
        self.rank(query, &query_vector, k)
    }

    /// [`Resident::search`] with the question's vector in hand.
    fn rank(
        &self,
        query: &str,
        query_vector: &[f32],
        k: usize,
    ) -> Result<Vec<ResidentHit>, String> {
        validate_vector(query_vector, self.dims)?;
        let mut unit = query_vector.to_vec();
        normalize(&mut unit);

        let terms = sorted_query_terms(query);
        let term_ids: Vec<Option<u32>> = terms.iter().map(|term| self.vocab.get(term)).collect();
        let mut docs: Vec<pixel_rank::Bm25Doc> = Vec::with_capacity(self.stats.chunks);
        // The best chunk of each file by cosine (the first of equals), and
        // the file of each document.
        let mut best: Vec<(f32, usize)> = Vec::with_capacity(self.files.len());
        let mut owners: Vec<usize> = Vec::with_capacity(self.stats.chunks);
        for (index, file) in self.files.iter().enumerate() {
            let mut top = (f32::MIN, 0);
            for (position, chunk) in file.chunks.iter().enumerate() {
                let cosine = dot(&unit, &chunk.vector);
                if cosine > top.0 {
                    top = (cosine, position);
                }
                docs.push(pixel_rank::Bm25Doc {
                    path: String::new(),
                    term_freqs: term_ids
                        .iter()
                        .map(|id| id.map_or(0, |id| count_of(&chunk.terms, id)))
                        .collect(),
                    len: chunk.len,
                });
                owners.push(index);
            }
            best.push(top);
        }

        let mut lexical = vec![Lexical::default(); self.files.len()];
        let scores =
            pixel_rank::bm25_scores(&terms, &docs).unwrap_or_else(|| vec![0.0; docs.len()]);
        for ((owner, doc), score) in owners.into_iter().zip(&docs).zip(scores) {
            let found = &mut lexical[owner];
            found.score = found.score.max(score);
            let matches = doc.term_freqs.iter().filter(|freq| **freq > 0).count();
            found.matches = found.matches.max(matches);
        }

        let lexical_by_path: HashMap<String, Lexical> = self
            .files
            .iter()
            .zip(lexical)
            .map(|(file, evidence)| (file.path.clone(), evidence))
            .collect();
        let best_by_path: HashMap<String, f32> = self
            .files
            .iter()
            .zip(&best)
            .map(|(file, (cosine, _))| (file.path.clone(), *cosine))
            .collect();
        Ok(
            rank_files(query, &lexical_by_path, best_by_path, HashMap::new(), k)
                .into_iter()
                .filter_map(|hit| {
                    let index = self
                        .files
                        .binary_search_by(|file| file.path.as_str().cmp(&hit.path))
                        .ok()?;
                    let chunk = &self.files[index].chunks[best[index].1];
                    Some(ResidentHit {
                        path: hit.path,
                        start_line: chunk.start_line,
                        end_line: chunk.end_line,
                        symbol: chunk.symbol.clone(),
                        score: hit.ranking_score,
                        snippet: chunk.snippet.clone(),
                    })
                })
                .collect(),
        )
    }
}

/// Read the file at `file` (below `root`) and, unless its bytes hash as the
/// index being refreshed knew them, cut and tokenize it.
fn load_file(
    root: &Path,
    file: &Path,
    reusable: &HashMap<&str, &Arc<FileChunks>>,
    vocab: &Mutex<Vocab>,
) -> Loaded {
    let Ok(bytes) = std::fs::read(file) else {
        return Loaded::Skipped;
    };
    if bytes.len() > MAX_FILE_BYTES || bytes.contains(&0) {
        return Loaded::Skipped;
    }
    let path = file
        .strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned();
    let hash = xxhash_rust::xxh3::xxh3_128(&bytes);
    if let Some(known) = reusable.get(path.as_str())
        && known.hash == hash
    {
        return Loaded::Reused(Arc::clone(known));
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Loaded::Skipped;
    };
    if text.trim().is_empty() {
        return Loaded::Empty;
    }
    // Filenames are evidence too, as for `ask`: the basename before its first
    // dot counts as if it were written once at the top of every chunk.
    let stem = file.file_name().map_or_else(String::new, |name| {
        let basename = name.to_string_lossy();
        basename.split('.').next().unwrap_or_default().to_string()
    });
    let stem_counts = token_counts(&stem);
    let counted: Vec<_> = named_chunks(&path, &text)
        .into_iter()
        .map(|chunk| {
            let mut counts = token_counts(lexical_chunk(&text, chunk.start, chunk.end));
            for (token, count) in &stem_counts {
                *counts.entry(token.clone()).or_insert(0) += count;
            }
            (chunk, counts)
        })
        .collect();
    if counted.is_empty() {
        return Loaded::Empty;
    }
    let mut vocab = vocab.lock().unwrap_or_else(PoisonError::into_inner);
    let chunks = counted
        .into_iter()
        .map(|(chunk, counts)| {
            let len = counts.values().sum();
            let mut terms: Vec<(u32, u32)> = counts
                .into_iter()
                .map(|(token, count)| (vocab.intern(token), count))
                .collect();
            terms.sort_unstable();
            ParsedChunk {
                start_line: chunk.first_line,
                end_line: chunk.last_line,
                symbol: chunk.symbol,
                snippet: make_snippet(&text[chunk.start..chunk.end]),
                text: text[chunk.start..chunk.end].to_string(),
                terms: terms.into_boxed_slice(),
                len,
            }
        })
        .collect();
    Loaded::Parsed(ParsedFile { path, hash, chunks })
}

/// How many times each token occurs in `text`.
fn token_counts(text: &str) -> HashMap<String, u32> {
    let mut counts = HashMap::new();
    for_each_word(text, |token| *counts.entry(token).or_insert(0) += 1);
    counts
}

/// What the model and the vector store did for a build.
struct Embedding {
    embedded_chunks: usize,
    cached_chunks: usize,
    vector_cache_errors: Vec<String>,
}

/// The parsed files as [`FileChunks`]: their chunks embedded (or read from
/// `store`) and scaled to unit length.
fn embed_parsed(
    mut parsed: Vec<ParsedFile>,
    namespace: &Namespace,
    embedder: &mut dyn Embedder,
    store: Option<Store>,
) -> Result<(Vec<Arc<FileChunks>>, Embedding), String> {
    let mut corpus: Vec<CorpusEntry> = Vec::new();
    for file in &mut parsed {
        for chunk in &mut file.chunks {
            let text = std::mem::take(&mut chunk.text);
            corpus.push(CorpusEntry {
                path: String::new(),
                key: namespace.key(&text),
                text,
            });
        }
    }
    let mut coverage = AskCoverage::default();
    let mut vectors: HashMap<ChunkKey, Vec<f32>> = if corpus.is_empty() {
        HashMap::new()
    } else {
        chunk_vectors(&corpus, namespace, embedder, store.as_ref(), &mut coverage)?
    };
    let dims = embedder.dims();

    // A text that occurs twice is embedded once: its vector is moved into the
    // last chunk that uses it and copied into the others.
    let mut uses: HashMap<ChunkKey, usize> = HashMap::new();
    for entry in &corpus {
        *uses.entry(entry.key).or_insert(0) += 1;
    }
    let mut keys = corpus.into_iter().map(|entry| entry.key);
    let mut files = Vec::with_capacity(parsed.len());
    for file in parsed {
        let mut chunks = Vec::with_capacity(file.chunks.len());
        for chunk in file.chunks {
            let key = keys.next().expect("one corpus entry per parsed chunk");
            let remaining = uses.get_mut(&key).expect("every key was counted");
            *remaining -= 1;
            let mut vector = if *remaining == 0 {
                vectors.remove(&key)
            } else {
                vectors.get(&key).cloned()
            }
            .ok_or("a chunk has no vector")?;
            validate_vector(&vector, dims)?;
            normalize(&mut vector);
            chunks.push(Chunk {
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                symbol: chunk.symbol,
                snippet: chunk.snippet,
                vector: vector.into_boxed_slice(),
                terms: chunk.terms,
                len: chunk.len,
            });
        }
        files.push(Arc::new(FileChunks {
            path: file.path,
            hash: file.hash,
            chunks,
        }));
    }
    Ok((
        files,
        Embedding {
            embedded_chunks: coverage.embedded_chunks,
            cached_chunks: coverage.cached_chunks,
            vector_cache_errors: coverage.vector_cache_errors,
        },
    ))
}

/// Scale `vector` to unit length; a zero vector is left as it is.
fn normalize(vector: &mut [f32]) {
    let norm = vector
        .iter()
        .map(|x| f64::from(*x).powi(2))
        .sum::<f64>()
        .sqrt();
    if norm > 0.0 {
        for x in vector {
            *x = (f64::from(*x) / norm) as f32;
        }
    }
}

/// The dot product of two vectors of equal length, in eight independent
/// lanes so the compiler vectorizes the sum.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut lanes = [0.0f32; 8];
    let (a_blocks, a_rest) = a.as_chunks::<8>();
    let (b_blocks, b_rest) = b.as_chunks::<8>();
    for (x, y) in a_blocks.iter().zip(b_blocks) {
        for ((lane, x), y) in lanes.iter_mut().zip(x).zip(y) {
            *lane += x * y;
        }
    }
    let tail: f32 = a_rest.iter().zip(b_rest).map(|(x, y)| x * y).sum();
    lanes.iter().sum::<f32>() + tail
}

/// How often the token `id` occurs in a chunk whose `terms` are sorted by id.
fn count_of(terms: &[(u32, u32)], id: u32) -> u32 {
    terms
        .binary_search_by_key(&id, |&(token, _)| token)
        .map_or(0, |index| terms[index].1)
}

/// The memory `files` and `vocab` hold, counting the payload of each chunk
/// and a flat overhead per allocation.
fn approximate_bytes(files: &[Arc<FileChunks>], vocab: &Vocab) -> u64 {
    const PER_CHUNK_OVERHEAD: usize = 96;
    const PER_TOKEN_OVERHEAD: usize = 40;
    let chunks: usize = files
        .iter()
        .flat_map(|file| &file.chunks)
        .map(|chunk| {
            chunk.vector.len() * 4
                + chunk.terms.len() * 8
                + chunk.snippet.len()
                + chunk.symbol.as_ref().map_or(0, String::len)
                + PER_CHUNK_OVERHEAD
        })
        .sum();
    let paths: usize = files.iter().map(|file| file.path.len() + 32).sum();
    let tokens: usize = vocab
        .ids
        .keys()
        .map(|key| key.len() + PER_TOKEN_OVERHEAD)
        .sum();
    (chunks + paths + tokens) as u64
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::code_search::{DEFAULT_LIMIT, ask_opening};

    const DIMS: usize = 512;

    /// Embeds a text as its words hashed into [`DIMS`] buckets, so texts
    /// sharing words are close, and counts the passages it is handed (a
    /// question is not counted).
    struct WordEmbedder {
        model: String,
        passages: Arc<AtomicUsize>,
    }

    impl WordEmbedder {
        fn new(model: &str) -> (Self, Arc<AtomicUsize>) {
            let passages = Arc::new(AtomicUsize::new(0));
            let embedder = Self {
                model: model.to_string(),
                passages: Arc::clone(&passages),
            };
            (embedder, passages)
        }
    }

    impl Embedder for WordEmbedder {
        fn model_id(&self) -> &str {
            &self.model
        }

        fn dims(&self) -> usize {
            DIMS
        }

        fn embed_batch(
            &mut self,
            texts: &[&str],
            kind: EmbedKind,
        ) -> Result<Vec<Vec<f32>>, String> {
            if matches!(kind, EmbedKind::Passage) {
                self.passages.fetch_add(texts.len(), Ordering::SeqCst);
            }
            Ok(texts
                .iter()
                .map(|text| {
                    let mut vector = vec![0.0f32; DIMS];
                    for_each_word(text, |word| {
                        let bucket = xxhash_rust::xxh3::xxh3_64(word.as_bytes()) % DIMS as u64;
                        vector[bucket as usize] += 1.0;
                    });
                    vector
                })
                .collect())
        }
    }

    /// Lines of a [`function`]: its doc comment, signature, steps, result
    /// and closing brace.
    const FUNCTION_LINES: u32 = STEPS as u32 + 4;
    const STEPS: usize = 10;

    /// A Rust function of [`FUNCTION_LINES`] lines and some 290 bytes,
    /// documented by `doc`: two of them (580 bytes) never share a chunk,
    /// which packs up to 400.
    fn function(doc: &str, name: &str) -> String {
        let steps: String = (0..STEPS)
            .map(|step| format!("    let step_{step} = {step} + 1;\n"))
            .collect();
        format!("/// {doc}\npub fn {name}() -> u32 {{\n{steps}    0\n}}\n")
    }

    fn tree(files: &[(&str, String)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in files {
            write(dir.path(), path, text);
        }
        dir
    }

    fn write(root: &Path, path: &str, text: &str) {
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, text).unwrap();
    }

    /// The index over `root` and the number of passages the model embedded.
    fn build(
        root: &Path,
        previous: Option<&Resident>,
        model: &str,
        cache: VectorCache,
    ) -> (Resident, usize) {
        let (mut embedder, passages) = WordEmbedder::new(model);
        let resident = Resident::build(root, previous, &mut embedder, cache, 100).unwrap();
        let embedded = passages.load(Ordering::SeqCst);
        assert_eq!(
            resident.stats().embedded_chunks,
            embedded,
            "the stats report what the model did"
        );
        (resident, embedded)
    }

    fn paths(hits: &[ResidentHit]) -> Vec<&str> {
        hits.iter().map(|hit| hit.path.as_str()).collect()
    }

    fn search(resident: &Resident, query: &str, k: usize) -> Vec<ResidentHit> {
        let (mut embedder, _) = WordEmbedder::new(&resident.model_id);
        resident.search(&mut embedder, query, k).unwrap()
    }

    fn billing_tree() -> tempfile::TempDir {
        tree(&[
            (
                "src/billing.rs",
                format!(
                    "{}\n{}",
                    function("Charge the customer card for an invoice", "charge_invoice"),
                    function("Refund a payment to the card", "refund_payment")
                ),
            ),
            (
                "src/weather.rs",
                function(
                    "Render the weather forecast for the week",
                    "render_forecast",
                ),
            ),
            (
                "README.md",
                "# Billing\n\nCharge the card for each invoice.\n".to_string(),
            ),
            ("config.toml", "invoice = \"card\"\n".to_string()),
        ])
    }

    /// The resident index is `ask` with the work done once: the same files,
    /// in the same order, with the same fused scores to the bit and the same
    /// snippets, for questions that favour code, docs and a named kind.
    #[test]
    fn search_should_rank_files_exactly_as_ask_does() {
        let dir = billing_tree();
        let (resident, _) = build(dir.path(), None, "fixture-a", VectorCache::Disabled);
        for (query, first) in [
            ("charge the invoice card", "src/billing.rs"),
            ("weather forecast for the week", "src/weather.rs"),
            ("readme billing", "README.md"),
        ] {
            let hits = search(&resident, query, DEFAULT_LIMIT);
            let asked = ask_opening(
                dir.path(),
                query,
                DEFAULT_LIMIT,
                None,
                VectorCache::Disabled,
                || Ok(Box::new(WordEmbedder::new("fixture-a").0)),
            )
            .unwrap()
            .hits;
            assert_eq!(hits[0].path, first, "{query}");
            assert_eq!(
                paths(&hits),
                asked
                    .iter()
                    .map(|hit| hit.path.as_str())
                    .collect::<Vec<_>>(),
                "{query}"
            );
            assert_eq!(
                hits.iter()
                    .map(|hit| hit.score.to_bits())
                    .collect::<Vec<_>>(),
                asked
                    .iter()
                    .map(|hit| hit.ranking_score.to_bits())
                    .collect::<Vec<_>>(),
                "{query}"
            );
            assert_eq!(
                hits.iter()
                    .map(|hit| hit.snippet.as_str())
                    .collect::<Vec<_>>(),
                asked
                    .iter()
                    .map(|hit| hit.snippet.as_str())
                    .collect::<Vec<_>>(),
                "{query}"
            );
        }
    }

    /// A hit names the chunk that matched: its lines, its symbol and the
    /// head of its text on one line.
    #[test]
    fn search_should_report_the_lines_symbol_and_snippet_of_the_best_chunk() {
        let dir = billing_tree();
        let (resident, _) = build(dir.path(), None, "fixture-a", VectorCache::Disabled);
        let hit = &search(&resident, "refund a payment to the card", 1)[0];
        assert_eq!(hit.path, "src/billing.rs");
        // A function and the blank line after it, then the second function.
        assert_eq!(
            (hit.start_line, hit.end_line),
            (FUNCTION_LINES + 2, 2 * FUNCTION_LINES + 1)
        );
        assert_eq!(hit.symbol.as_deref(), Some("refund_payment"));
        assert!(
            hit.snippet
                .starts_with("/// Refund a payment to the card pub fn refund_payment()"),
            "{}",
            hit.snippet
        );
        assert!(!hit.snippet.contains('\n') && hit.snippet.chars().count() <= 161);
    }

    /// Equal evidence orders by path, and among equal chunks of one file the
    /// first reports its lines.
    #[test]
    fn search_should_break_ties_by_path_then_first_chunk() {
        let twice = format!(
            "{}\n{}",
            function("Total the ledger", "total"),
            function("Total the ledger", "total")
        );
        let dir = tree(&[
            ("src/b.rs", twice.clone()),
            ("src/a.rs", twice),
            ("src/c.rs", function("Total the ledger", "total")),
        ]);
        let (resident, _) = build(dir.path(), None, "fixture-a", VectorCache::Disabled);
        let hits = search(&resident, "total the ledger", DEFAULT_LIMIT);
        assert_eq!(paths(&hits), ["src/a.rs", "src/b.rs", "src/c.rs"]);
        assert_eq!(
            (hits[0].start_line, hits[0].end_line),
            (1, FUNCTION_LINES + 1)
        );
        assert_eq!((hits[2].start_line, hits[2].end_line), (1, FUNCTION_LINES));
    }

    #[test]
    fn search_should_return_at_most_k_files() {
        let dir = billing_tree();
        let (resident, _) = build(dir.path(), None, "fixture-a", VectorCache::Disabled);
        let all = search(&resident, "charge the invoice card", DEFAULT_LIMIT);
        assert_eq!(all.len(), 4);
        let two = search(&resident, "charge the invoice card", 2);
        assert_eq!(paths(&two), paths(&all)[..2]);
        assert!(search(&resident, "charge the invoice card", 0).is_empty());
    }

    /// A refresh after nothing changed parses and embeds nothing; after an
    /// edit it embeds the chunks of the edited file alone, and the new text
    /// is found while the old one no longer is.
    #[test]
    fn build_should_embed_only_the_chunks_of_a_changed_file_when_refreshing() {
        let dir = billing_tree();
        let (first, embedded) = build(dir.path(), None, "fixture-a", VectorCache::Disabled);
        assert!(embedded > 0);
        assert_eq!(first.stats().reused_files, 0);

        let (second, embedded) =
            build(dir.path(), Some(&first), "fixture-a", VectorCache::Disabled);
        assert_eq!(embedded, 0, "nothing changed");
        assert_eq!(second.stats().reused_files, 4);
        assert_eq!(second.stats().chunks, first.stats().chunks);
        assert_eq!(
            search(&second, "render forecast", 1)[0].path,
            "src/weather.rs"
        );

        write(
            dir.path(),
            "src/weather.rs",
            &function("Predict the glacier avalanche risk", "predict_avalanche"),
        );
        let (third, embedded) = build(
            dir.path(),
            Some(&second),
            "fixture-a",
            VectorCache::Disabled,
        );
        assert_eq!(embedded, 1, "the one chunk of the edited file");
        assert_eq!(third.stats().reused_files, 3);
        let hits = search(&third, "glacier avalanche risk", 1);
        assert_eq!(hits[0].path, "src/weather.rs");
        assert_eq!(hits[0].symbol.as_deref(), Some("predict_avalanche"));
        let stale = search(&third, "render forecast", DEFAULT_LIMIT);
        assert_ne!(
            stale[0].path, "src/weather.rs",
            "the old text of the edited file is gone"
        );
    }

    /// The file list is walked again on every build: a removed file leaves
    /// the index and a new one enters it, with nothing reported as changed.
    #[test]
    fn build_should_drop_removed_files_and_pick_up_new_ones() {
        let dir = billing_tree();
        let (first, _) = build(dir.path(), None, "fixture-a", VectorCache::Disabled);
        std::fs::remove_file(dir.path().join("src/weather.rs")).unwrap();
        write(
            dir.path(),
            "src/orbit.rs",
            &function("Compute the orbit of the comet", "compute_orbit"),
        );
        let (second, embedded) =
            build(dir.path(), Some(&first), "fixture-a", VectorCache::Disabled);
        assert_eq!(embedded, 1);
        assert_eq!(second.stats().files, 4);
        let all = search(&second, "weather forecast orbit comet", DEFAULT_LIMIT);
        assert!(
            !paths(&all).contains(&"src/weather.rs"),
            "{:?}",
            paths(&all)
        );
        assert_eq!(
            paths(&search(&second, "orbit of the comet", 1)),
            ["src/orbit.rs"]
        );
    }

    /// Vectors of another model are not comparable: the previous index is
    /// ignored and everything is embedded again.
    #[test]
    fn build_should_ignore_the_previous_index_when_the_model_changes() {
        let dir = billing_tree();
        let (first, embedded_first) = build(dir.path(), None, "fixture-a", VectorCache::Disabled);
        let (second, embedded) =
            build(dir.path(), Some(&first), "fixture-b", VectorCache::Disabled);
        assert_eq!(second.stats().reused_files, 0);
        assert_eq!(embedded, embedded_first);
        assert_eq!(second.stats().model_id, "fixture-b");
    }

    #[test]
    fn build_should_hold_a_sample_and_say_so_above_the_file_limit() {
        let dir = tree(&[
            ("a.rs", function("One", "one")),
            ("b.rs", function("Two", "two")),
            ("c.rs", function("Three", "three")),
        ]);
        let (mut embedder, _) = WordEmbedder::new("fixture-a");
        let resident =
            Resident::build(dir.path(), None, &mut embedder, VectorCache::Disabled, 2).unwrap();
        let stats = resident.stats();
        assert_eq!((stats.files, stats.eligible_files), (2, 3));
        assert_eq!(stats.sampled_files, 2);
        assert!(stats.file_limit_reached);
        let (resident, _) = build(dir.path(), None, "fixture-a", VectorCache::Disabled);
        assert!(!resident.stats().file_limit_reached);
        assert_eq!(resident.stats().files, 3);
    }

    /// Unreadable text is counted, not silently absent; a blank file is
    /// simply empty.
    #[test]
    fn build_should_count_the_files_it_cannot_read_as_skipped() {
        let dir = tree(&[
            ("ok.rs", function("Fine", "fine")),
            ("blank.rs", " \n".into()),
        ]);
        std::fs::write(dir.path().join("binary.rs"), b"fn a() {}\0").unwrap();
        std::fs::write(dir.path().join("latin.rs"), b"fn caf\xe9() {}").unwrap();
        let (resident, _) = build(dir.path(), None, "fixture-a", VectorCache::Disabled);
        let stats = resident.stats();
        assert_eq!(stats.skipped_files, 2);
        assert_eq!((stats.files, stats.eligible_files), (1, 4));
    }

    /// The size cap is on the bytes read: a file of exactly the cap is held,
    /// one byte more is skipped.
    #[test]
    fn build_should_skip_a_file_only_above_the_size_cap() {
        let line = "one line of the manual, padded with some more plain words\n";
        let mut at_cap = line.repeat(MAX_FILE_BYTES / line.len() + 1);
        at_cap.truncate(MAX_FILE_BYTES);
        assert_eq!(at_cap.len(), MAX_FILE_BYTES);
        let dir = tree(&[("at_cap.md", at_cap.clone()), ("small.md", "small".into())]);
        write(dir.path(), "over_cap.md", &format!("{at_cap}x"));
        let (resident, _) = build(dir.path(), None, "fixture-a", VectorCache::Disabled);
        let stats = resident.stats();
        assert_eq!((stats.files, stats.skipped_files), (2, 1));
        assert!(resident.files.iter().any(|file| file.path == "at_cap.md"));
        assert!(resident.files.iter().all(|file| file.path != "over_cap.md"));
    }

    /// A hit shows the head of a chunk and the index serves agents, so a
    /// file whose path is credential-shaped is never read, embedded or
    /// found, and is counted.
    #[test]
    fn build_should_never_index_credential_shaped_paths() {
        let dir = tree(&[
            ("src/billing.rs", function("Charge the card", "charge")),
            (
                "config/secrets/prod.yaml",
                "token: hunter2 charge the card\n".into(),
            ),
            (
                "src/api_secret.json",
                "{\"token\": \"charge the card\"}\n".into(),
            ),
        ]);
        let (resident, embedded) = build(dir.path(), None, "fixture-a", VectorCache::Disabled);
        let stats = resident.stats();
        assert_eq!(
            (stats.files, stats.credential_files, stats.eligible_files),
            (1, 2, 3)
        );
        assert_eq!(embedded, 1, "only the one readable file was embedded");
        let hits = search(&resident, "charge the card token", DEFAULT_LIMIT);
        assert_eq!(paths(&hits), ["src/billing.rs"]);
        assert!(!hits.iter().any(|hit| hit.snippet.contains("hunter2")));
    }

    #[test]
    fn search_should_refuse_an_embedder_of_another_model_or_size() {
        let dir = billing_tree();
        let (resident, _) = build(dir.path(), None, "fixture-a", VectorCache::Disabled);
        let (mut other, _) = WordEmbedder::new("fixture-b");
        let error = resident.search(&mut other, "invoice", 3).unwrap_err();
        assert!(
            error.contains("fixture-a") && error.contains("fixture-b"),
            "{error}"
        );
        let error = resident.rank("invoice", &[1.0, 0.0], 3).unwrap_err();
        assert!(error.contains("invalid embedding vector"), "{error}");
    }

    /// The first build of a tree carrying an index reads and writes the
    /// store `pixel search-meaning` uses, under the same key, so each
    /// serves the other; a refresh leaves the store as it was.
    #[test]
    fn build_should_share_the_vector_store_with_ask_and_leave_it_on_refresh() {
        let dir = billing_tree();
        write(
            dir.path(),
            &format!(
                "{}/{}",
                pixel_index::index::SHARD_DIR,
                pixel_index::index::SHARD_FILE
            ),
            "",
        );
        let asked = ask_opening(
            dir.path(),
            "invoice",
            DEFAULT_LIMIT,
            None,
            VectorCache::Persisted,
            || Ok(Box::new(WordEmbedder::new("fixture-a").0)),
        )
        .unwrap();
        assert!(asked.coverage.embedded_chunks > 0);

        let (first, embedded) = build(dir.path(), None, "fixture-a", VectorCache::Persisted);
        assert_eq!(embedded, 0, "ask already stored every vector");
        assert_eq!(first.stats().cached_chunks, first.stats().chunks);

        let manifest = dir
            .path()
            .join(pixel_index::index::SHARD_DIR)
            .join(crate::code_vectors::DIR)
            .join("manifest.json");
        let before = std::fs::read(&manifest).unwrap();
        write(
            dir.path(),
            "src/weather.rs",
            &function("Predict the glacier avalanche risk", "predict_avalanche"),
        );
        let (_, embedded) = build(
            dir.path(),
            Some(&first),
            "fixture-a",
            VectorCache::Persisted,
        );
        assert_eq!(embedded, 1);
        assert_eq!(
            std::fs::read(&manifest).unwrap(),
            before,
            "a refresh never writes the store"
        );
    }

    #[test]
    fn dot_should_sum_whole_blocks_and_the_tail() {
        let a: Vec<f32> = (1..=19).map(|n| n as f32).collect();
        let b: Vec<f32> = (1..=19).map(|n| (n % 3) as f32 - 1.0).collect();
        let expected: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert_eq!(dot(&a, &b), expected);
        assert_eq!(
            dot(&a[..8], &b[..8]),
            a[..8].iter().zip(&b).map(|(x, y)| x * y).sum::<f32>()
        );
        assert_eq!(dot(&[], &[]), 0.0);
    }

    #[test]
    fn normalize_should_scale_to_unit_length_and_leave_zero_alone() {
        let mut vector = vec![3.0f32, 4.0];
        normalize(&mut vector);
        assert_eq!(vector, [0.6, 0.8]);
        let mut zero = vec![0.0f32, 0.0];
        normalize(&mut zero);
        assert_eq!(zero, [0.0, 0.0]);
    }

    #[test]
    fn count_of_should_find_a_token_by_id_in_sorted_terms() {
        let terms = [(2u32, 5u32), (7, 1), (9, 3)];
        assert_eq!(count_of(&terms, 2), 5);
        assert_eq!(count_of(&terms, 7), 1);
        assert_eq!(count_of(&terms, 9), 3);
        assert_eq!(count_of(&terms, 8), 0);
        assert_eq!(count_of(&[], 1), 0);
    }

    #[test]
    fn vocab_should_number_tokens_in_order_of_arrival_and_keep_the_numbers() {
        let mut vocab = Vocab::default();
        assert_eq!(vocab.intern("alpha".into()), 0);
        assert_eq!(vocab.intern("beta".into()), 1);
        assert_eq!(vocab.intern("alpha".into()), 0);
        assert_eq!(vocab.get("beta"), Some(1));
        assert_eq!(vocab.get("gamma"), None);
        let mut copy = vocab.clone();
        assert_eq!(copy.intern("gamma".into()), 2);
        assert_eq!(vocab.get("gamma"), None, "a refresh extends a copy");
    }

    /// The stats count what is held and its footprint grows with it.
    #[test]
    fn stats_should_count_files_chunks_and_memory() {
        let dir = billing_tree();
        let (resident, _) = build(dir.path(), None, "fixture-a", VectorCache::Disabled);
        let stats = resident.stats();
        assert_eq!((stats.files, stats.dims), (4, DIMS));
        assert_eq!(
            stats.chunks,
            resident.files.iter().map(|f| f.chunks.len()).sum::<usize>()
        );
        assert!(stats.bytes >= (stats.chunks * DIMS * 4) as u64);
        assert!(stats.bytes < (stats.chunks * DIMS * 4 * 2) as u64);
        let small = tree(&[("a.rs", function("One", "one"))]);
        let (small, _) = build(small.path(), None, "fixture-a", VectorCache::Disabled);
        assert!(small.stats().bytes < stats.bytes);
    }
}
