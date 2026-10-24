// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The resident pool: every chunk of a root with its vector in memory, built
//! from the corpus and the vector store, answering one question at a time.
//!
//! A hit is a lead, never a verdict: embedding similarity ranks chunks, it
//! does not separate related code from unrelated code.

use std::collections::HashSet;
use std::time::Instant;

use super::Result;
use super::chunk::{Chunk, chunk_key};
use super::embed::Embedder;
use super::rank::{Bm25, OFF_CLASS_FACTOR, PathClass, rrf};
use super::store::VectorStore;

/// How many chunks go to the embedder at once.
pub const EMBED_BATCH: usize = 64;

/// Each ranking contributes at most this many chunks to the fusion.
const POOL_DEPTH: usize = 200;

/// A ranked lead: the best chunk of one file.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub symbol: Option<String>,
    /// The fused score; comparable within one answer only.
    pub score: f64,
    pub snippet: String,
}

/// What a built pool covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStats {
    pub model: String,
    pub dims: usize,
    pub files: usize,
    pub chunks: usize,
    pub resident_bytes: u64,
    /// Chunks embedded by this build (the rest came from the store).
    pub embedded: usize,
    /// Chunks read from the store.
    pub reused: usize,
}

/// Chunks, their vectors and a lexical index, held together.
#[derive(Debug)]
pub struct Pool {
    chunks: Vec<Chunk>,
    vectors: Vec<f32>,
    dim: usize,
    lexical: Bm25,
    pub stats: PoolStats,
    pub generation: u64,
    pub built_at: Instant,
}

impl Pool {
    /// Builds the pool for `chunks`. With a `store`, vectors already stored
    /// under a chunk's key are reused and the missing ones embedded and
    /// appended (under the writer lock); without one, everything is embedded
    /// in memory.
    pub fn build(
        chunks: Vec<Chunk>,
        embedder: &dyn Embedder,
        store: Option<&mut VectorStore>,
        generation: u64,
    ) -> Result<Self> {
        let dim = embedder.dim();
        let keys: Vec<u128> = chunks
            .iter()
            .map(|c| chunk_key(embedder.id(), embedder.revision(), &c.text))
            .collect();
        let mut vectors = vec![0.0_f32; chunks.len() * dim];
        let mut have = vec![false; chunks.len()];
        let mut reused = 0;
        let mut guard = None;
        let mut store = store;
        if let Some(store) = store.as_deref_mut() {
            guard = Some(store.lock()?);
            if store
                .check_model(embedder.id(), embedder.revision(), dim)
                .is_err()
            {
                store.reset(embedder.id(), embedder.revision(), dim)?;
            }
            let stored = store.load()?;
            for (i, key) in keys.iter().enumerate() {
                if let Some(q) = stored.get(key) {
                    vectors[i * dim..(i + 1) * dim].copy_from_slice(&q.to_f32());
                    have[i] = true;
                    reused += 1;
                }
            }
        }
        let missing: Vec<usize> = (0..chunks.len()).filter(|i| !have[*i]).collect();
        let mut fresh: Vec<(u128, Vec<f32>)> = Vec::with_capacity(missing.len());
        for batch in missing.chunks(EMBED_BATCH) {
            let texts: Vec<String> = batch.iter().map(|i| chunks[*i].text.clone()).collect();
            let out = embedder.embed(&texts)?;
            for (i, vector) in batch.iter().zip(out) {
                vectors[i * dim..(i + 1) * dim].copy_from_slice(&vector);
                fresh.push((keys[*i], vector));
            }
        }
        if let Some(store) = store {
            store.append(embedder.id(), embedder.revision(), dim, &fresh)?;
            store.compact(&keys.iter().copied().collect::<HashSet<u128>>())?;
        }
        drop(guard);
        let lexical = Bm25::build(chunks.iter().map(|c| c.text.as_str()));
        let files = chunks
            .iter()
            .map(|c| c.path.as_str())
            .collect::<HashSet<_>>()
            .len();
        let stats = PoolStats {
            model: embedder.id().to_owned(),
            dims: dim,
            files,
            chunks: chunks.len(),
            resident_bytes: (vectors.len() * 4 + chunks.iter().map(|c| c.text.len()).sum::<usize>())
                as u64,
            embedded: fresh.len(),
            reused,
        };
        Ok(Self {
            chunks,
            vectors,
            dim,
            lexical,
            stats,
            generation,
            built_at: Instant::now(),
        })
    }

    /// The best chunk of each of the top `limit` files for `question`.
    pub fn query(&self, embedder: &dyn Embedder, question: &str, limit: usize) -> Result<Vec<Hit>> {
        if self.chunks.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let q = embedder.embed(&[question.to_owned()])?.remove(0);
        let mut semantic: Vec<(usize, f32)> = (0..self.chunks.len())
            .map(|i| (i, dot(&self.vectors[i * self.dim..(i + 1) * self.dim], &q)))
            .filter(|(_, s)| *s > 0.0)
            .collect();
        semantic.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        semantic.truncate(POOL_DEPTH);
        let lexical = self.lexical.top(question, POOL_DEPTH);
        let fused = rrf(&[
            semantic.iter().map(|(i, _)| *i).collect(),
            lexical.iter().map(|(i, _)| *i).collect(),
        ]);
        let mut scored: Vec<(usize, f64)> = fused
            .into_iter()
            .map(|(i, score)| {
                let class = PathClass::of(&self.chunks[i].path);
                let demote = class != PathClass::Code && !class.named_in(question);
                (
                    i,
                    if demote {
                        score * OFF_CLASS_FACTOR
                    } else {
                        score
                    },
                )
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut seen = HashSet::new();
        let mut hits = Vec::new();
        for (i, score) in scored {
            let chunk = &self.chunks[i];
            if !seen.insert(chunk.path.as_str()) {
                continue;
            }
            hits.push(Hit {
                path: chunk.path.clone(),
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                symbol: chunk.symbol.clone(),
                score,
                snippet: chunk.snippet(160),
            });
            if hits.len() == limit {
                break;
            }
        }
        Ok(hits)
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::super::chunk::{SymbolSpan, chunk_file};
    use super::super::embed::HashingEmbedder;
    use super::super::store::STORE_DIR;
    use super::*;

    fn corpus() -> Vec<Chunk> {
        let retry =
            "/// Reads the retry limit from the config file.\nfn retry_limit() -> u32 { 3 }\n";
        let draw = "/// Draws the sidebar animation frame.\nfn draw_frame() {}\n";
        let test = "/// Tests that the retry limit is read from the config file.\nfn retry_limit_is_read() {}\n";
        let sp = |n: &str, e: u32| SymbolSpan {
            name: n.to_owned(),
            line: 1,
            end_line: e,
        };
        // Symbols start at line 1 here, with the doc comment inside their span.
        let mut all = chunk_file("src/retry.rs", retry, &[sp("retry_limit", 2)]);
        all.extend(chunk_file("src/draw.rs", draw, &[sp("draw_frame", 2)]));
        all.extend(chunk_file(
            "tests/retry.rs",
            test,
            &[sp("retry_limit_is_read", 2)],
        ));
        all
    }

    #[test]
    fn query_should_put_the_code_file_that_matches_first_and_the_test_below_it() {
        let embedder = HashingEmbedder::new(256);
        let pool = Pool::build(corpus(), &embedder, None, 7).unwrap();
        assert_eq!(
            (pool.stats.files, pool.stats.chunks, pool.generation),
            (3, 3, 7)
        );
        let hits = pool
            .query(&embedder, "where is the retry limit read from config", 5)
            .unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths[0], "src/retry.rs", "{hits:?}");
        let test_at = paths.iter().position(|p| *p == "tests/retry.rs").unwrap();
        assert!(test_at > 0, "a test ranks below the code unless named");
        assert_eq!(hits[0].symbol.as_deref(), Some("retry_limit"));
        assert!(hits[0].snippet.starts_with("/// Reads the retry limit"));
    }

    #[test]
    fn query_should_not_demote_a_test_the_question_names() {
        let embedder = HashingEmbedder::new(256);
        let pool = Pool::build(corpus(), &embedder, None, 1).unwrap();
        let hits = pool
            .query(
                &embedder,
                "which test checks the retry limit is read from the config file",
                3,
            )
            .unwrap();
        assert_eq!(hits[0].path, "tests/retry.rs", "{hits:?}");
    }

    #[test]
    fn query_should_return_one_hit_per_file_and_honour_the_limit() {
        let embedder = HashingEmbedder::new(64);
        let mut chunks = corpus();
        chunks.extend(chunk_file(
            "src/retry.rs",
            "fn other_retry() {}\n",
            &[SymbolSpan {
                name: "other_retry".into(),
                line: 1,
                end_line: 1,
            }],
        ));
        let pool = Pool::build(chunks, &embedder, None, 1).unwrap();
        let hits = pool.query(&embedder, "retry", 10).unwrap();
        let mut paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        let before = paths.len();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), before);
        assert_eq!(pool.query(&embedder, "retry", 1).unwrap().len(), 1);
        assert!(pool.query(&embedder, "retry", 0).unwrap().is_empty());
    }

    #[test]
    fn build_should_reuse_stored_vectors_and_embed_only_what_is_new() {
        let embedder = HashingEmbedder::new(64);
        let root = tempfile::tempdir().unwrap();
        let mut store = VectorStore::open(&root.path().join(STORE_DIR)).unwrap();
        let first = Pool::build(corpus(), &embedder, Some(&mut store), 1).unwrap();
        assert_eq!((first.stats.embedded, first.stats.reused), (3, 0));
        let second = Pool::build(corpus(), &embedder, Some(&mut store), 2).unwrap();
        assert_eq!((second.stats.embedded, second.stats.reused), (0, 3));
        let mut changed = corpus();
        changed.extend(chunk_file(
            "src/new.rs",
            "fn brand_new() {}\n",
            &[SymbolSpan {
                name: "brand_new".into(),
                line: 1,
                end_line: 1,
            }],
        ));
        let third = Pool::build(changed, &embedder, Some(&mut store), 3).unwrap();
        assert_eq!((third.stats.embedded, third.stats.reused), (1, 3));
        assert_eq!(store.manifest.rows, 4);
        // A pool rebuilt from the store answers as the one built in memory.
        let q = "retry limit config";
        assert_eq!(
            second
                .query(&embedder, q, 3)
                .unwrap()
                .iter()
                .map(|h| &h.path)
                .collect::<Vec<_>>(),
            Pool::build(corpus(), &embedder, None, 0)
                .unwrap()
                .query(&embedder, q, 3)
                .unwrap()
                .iter()
                .map(|h| &h.path)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn build_should_rebind_a_store_written_by_another_model() {
        let root = tempfile::tempdir().unwrap();
        let mut store = VectorStore::open(&root.path().join(STORE_DIR)).unwrap();
        Pool::build(corpus(), &HashingEmbedder::new(32), Some(&mut store), 1).unwrap();
        let wider = Pool::build(corpus(), &HashingEmbedder::new(48), Some(&mut store), 2).unwrap();
        assert_eq!((wider.stats.embedded, wider.stats.reused), (3, 0));
        assert_eq!(store.manifest.dim, 48);
    }

    #[test]
    fn an_empty_pool_should_answer_nothing() {
        let embedder = HashingEmbedder::new(16);
        let pool = Pool::build(Vec::new(), &embedder, None, 0).unwrap();
        assert!(pool.query(&embedder, "anything", 5).unwrap().is_empty());
        assert_eq!(pool.stats.chunks, 0);
    }
}
