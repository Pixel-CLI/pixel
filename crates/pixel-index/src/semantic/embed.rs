// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

//! The embedding seam: what the store and the pool need from a model, and the
//! one backend PIXEL ships (`model2vec`, a pure-Rust static embedder).

use std::path::{Path, PathBuf};

use super::Result;
#[cfg(feature = "model2vec")]
use super::SemanticError;

/// The code-search model PIXEL embeds with: a static Model2Vec distilled from a
/// code embedding model, 16M parameters.
pub const CODE_MODEL_REPO: &str = "minishlab/potion-code-16M-v2";

/// The files of a Model2Vec repository PIXEL needs, in the order they load.
pub const MODEL_FILES: [&str; 3] = ["tokenizer.json", "model.safetensors", "config.json"];

/// A file of the model directory that marks a finished download: the three
/// files are written first, then this one, so a half-fetched directory is
/// never read as a model.
pub const COMPLETE_MARKER: &str = ".complete";

/// Something that turns text into vectors of one fixed width.
///
/// `id` and `revision` name what the vectors mean: two embedders that differ in
/// either never share a store ([`super::store::VectorStore::check_model`]).
pub trait Embedder: Send + Sync {
    /// The model's name (the repository for a downloaded one).
    fn id(&self) -> &str;
    /// Bumped when the same `id` embeds differently (a tokenizer fix, a
    /// normalisation change): a store written at another revision is rebuilt.
    fn revision(&self) -> u32;
    /// The width of every vector.
    fn dim(&self) -> usize;
    /// One vector per text, in order, each of `dim()` floats.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Where the model of `repo` lives under the cache root: `<root>/models/<repo
/// with `/` as `--`>`. Not created here.
pub fn model_dir(cache_root: &Path, repo: &str) -> PathBuf {
    cache_root.join("models").join(repo.replace('/', "--"))
}

/// True when the model of `repo` finished downloading into `cache_root`.
pub fn model_cached(cache_root: &Path, repo: &str) -> bool {
    model_dir(cache_root, repo).join(COMPLETE_MARKER).is_file()
}

/// The first of [`MODEL_FILES`] missing from `dir`, if any.
pub fn missing_file(dir: &Path) -> Option<&'static str> {
    MODEL_FILES
        .into_iter()
        .find(|name| !dir.join(name).is_file())
}

/// A deterministic embedder for tests and for builds without a model: each
/// word of the text adds a hashed, signed bump to a few coordinates, so two
/// texts that share words are closer than two that do not. Not a language
/// model; it carries no meaning beyond shared words.
#[derive(Debug, Clone)]
pub struct HashingEmbedder {
    dim: usize,
}

impl HashingEmbedder {
    /// An embedder of `dim` coordinates (at least 8).
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(8) }
    }
}

impl Embedder for HashingEmbedder {
    fn id(&self) -> &str {
        "pixel-hashing"
    }

    fn revision(&self) -> u32 {
        1
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| {
                let mut out = vec![0.0_f32; self.dim];
                for word in pixel_model::terms(text) {
                    let hash = xxhash_rust::xxh3::xxh3_64(word.as_bytes());
                    for round in 0..3_u32 {
                        let bits = hash.rotate_left(round * 21);
                        let slot = (bits % self.dim as u64) as usize;
                        out[slot] += if bits >> 63 == 0 { 1.0 } else { -1.0 };
                    }
                }
                normalise(&mut out);
                out
            })
            .collect())
    }
}

/// Scales `vector` to unit length; an all-zero vector stays zero.
pub fn normalise(vector: &mut [f32]) {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in vector {
            *x /= norm;
        }
    }
}

/// Opens the model2vec model cached under `dir`, reading bytes only: this
/// function never touches the network, and a missing file is an error naming
/// it, so a caller can say "run `pixel search-meaning --download`".
#[cfg(feature = "model2vec")]
pub fn open_cached(dir: &Path, repo: &str) -> Result<Model2Vec> {
    if !dir.join(COMPLETE_MARKER).is_file() {
        return Err(SemanticError(format!(
            "the embedding model {repo} is not downloaded (nothing at {})",
            dir.display()
        )));
    }
    if let Some(name) = missing_file(dir) {
        return Err(SemanticError(format!(
            "the embedding model {repo} is incomplete: {} has no {name}",
            dir.display()
        )));
    }
    let read = |name: &str| {
        std::fs::read(dir.join(name))
            .map_err(|error| SemanticError(format!("{}: {error}", dir.join(name).display())))
    };
    let model = model2vec_rs::model::StaticModel::from_bytes(
        read("tokenizer.json")?,
        read("model.safetensors")?,
        read("config.json")?,
        None,
    )
    .map_err(|error| SemanticError(format!("the embedding model {repo} did not load: {error}")))?;
    let probe = model.encode_single("fn main");
    if probe.is_empty() {
        return Err(SemanticError(format!(
            "the embedding model {repo} loaded but embeds to nothing"
        )));
    }
    Ok(Model2Vec {
        id: repo.to_owned(),
        dim: probe.len(),
        model,
    })
}

/// The Model2Vec embedder: a lookup of token vectors, averaged and normalised.
/// Its `revision` is [`MODEL2VEC_REVISION`].
#[cfg(feature = "model2vec")]
pub struct Model2Vec {
    id: String,
    dim: usize,
    model: model2vec_rs::model::StaticModel,
}

/// Bumped when the `model2vec-rs` release or PIXEL's use of it changes what a
/// text embeds to.
#[cfg(feature = "model2vec")]
pub const MODEL2VEC_REVISION: u32 = 1;

#[cfg(feature = "model2vec")]
impl Embedder for Model2Vec {
    fn id(&self) -> &str {
        &self.id
    }

    fn revision(&self) -> u32 {
        MODEL2VEC_REVISION
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let vectors = self.model.encode(texts);
        if vectors.len() != texts.len() || vectors.iter().any(|v| v.len() != self.dim) {
            return Err(SemanticError(
                "the embedder returned vectors of the wrong shape".into(),
            ));
        }
        Ok(vectors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn model_dir_should_name_the_repository_without_a_slash() {
        let dir = model_dir(Path::new("/c"), "minishlab/potion-code-16M-v2");
        assert_eq!(dir, Path::new("/c/models/minishlab--potion-code-16M-v2"));
    }

    #[test]
    fn model_cached_should_need_the_marker_not_just_the_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = model_dir(root.path(), "a/b");
        std::fs::create_dir_all(&dir).unwrap();
        for name in MODEL_FILES {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        assert!(
            !model_cached(root.path(), "a/b"),
            "files without the marker"
        );
        assert_eq!(missing_file(&dir), None);
        std::fs::write(dir.join(COMPLETE_MARKER), b"").unwrap();
        assert!(model_cached(root.path(), "a/b"));
        std::fs::remove_file(dir.join("config.json")).unwrap();
        assert_eq!(missing_file(&dir), Some("config.json"));
    }

    #[test]
    fn hashing_embedder_should_put_shared_words_closer_than_unrelated_ones() {
        let embedder = HashingEmbedder::new(256);
        let texts = [
            "parse the retry limit from the config file".to_owned(),
            "retry limit config parse".to_owned(),
            "render the sidebar animation frame".to_owned(),
        ];
        let v = embedder.embed(&texts).unwrap();
        assert_eq!(v.len(), 3);
        assert!(v.iter().all(|x| x.len() == 256));
        assert!(cosine(&v[0], &v[1]) > cosine(&v[0], &v[2]) + 0.3);
        let norm = cosine(&v[0], &v[0]);
        assert!((norm - 1.0).abs() < 1e-4, "unit length, got {norm}");
    }

    #[test]
    fn hashing_embedder_should_be_deterministic_and_leave_empty_text_at_zero() {
        let embedder = HashingEmbedder::new(64);
        let a = embedder.embed(&["alpha beta".to_owned()]).unwrap();
        let b = embedder.embed(&["alpha beta".to_owned()]).unwrap();
        assert_eq!(a, b);
        let empty = embedder.embed(&[String::new()]).unwrap();
        assert!(empty[0].iter().all(|x| *x == 0.0));
        assert_eq!(HashingEmbedder::new(1).dim(), 8, "the width has a floor");
    }

    #[test]
    fn normalise_should_leave_a_zero_vector_alone() {
        let mut zero = [0.0_f32; 4];
        normalise(&mut zero);
        assert_eq!(zero, [0.0; 4]);
        let mut v = [3.0_f32, 4.0];
        normalise(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
    }
}
