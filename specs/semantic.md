# Semantic search (feature `semantic`)

Status: normative for `pixel-index`'s `semantic` feature and `pixel search-meaning`.
Lifted from `pixel-recall`: `code_search.rs`, `code_resident.rs`,
`code_chunks.rs`, `code_vectors.rs`, `embed.rs`, `model.rs`, `vector.rs`,
`hybrid.rs`, plus the daemon's `meaning.rs`. The transcript half of
`pixel-recall` (ingestion for 8 agent CLIs) is not carried.

- **Ranking.** Embed a natural-language question and rank files by fusing
  semantic rank and BM25 lexical rank (`pixel-model`'s one BM25 and one RRF, K=60)
  over symbol chunks (tree-sitter symbols with doc comments; windows for
  unparsed files). Tests, config/data and docs rank below code unless the
  question names them.
- **One parse.** The chunker reads symbol spans from the graph store instead of
  parsing again (Pixel's `code_chunks.rs` called `extract_file` a second time).
- **Backend** behind the `semantic` feature (`pixel-index/model2vec`): Model2Vec,
  pure Rust (`model2vec-rs` with `local-only`, `tokenizers` with `fancy-regex`
  only; no ONNX, no OpenSSL), so the musl release lanes can carry it once the
  tokenizer stack is in the release build. The ONNX `fastembed` backend is not
  carried. The code-search model is `potion-code-16M-v2`, looked up in
  `$XDG_CACHE_HOME/pixel/models/<repo--name>/` (a `.complete` marker written
  last); `pixel search-meaning --download` is the only code that fetches it,
  printing repository, revision and destination first. The offline opener reads
  cached bytes and refuses missing files without any network access.
- **Store.** `.pixel/code-vectors/` (`manifest.json`, `seg-*.vec`, `lock`): vectors
  keyed by the xxh3-128 hash of the chunk text seeded with model id, embedder
  revision and `CHUNKER_VERSION`; `flock` on `lock`, immutable segments, manifest
  replaced by rename; rewritten when unreachable rows exceed a quarter. Written
  only when the root carries a base shard and is not `$HOME`, never by the
  daemon's fallback.
- **Brief use.** The brief's fused leads read the `Meaning` op when resident
  vectors exist; otherwise plain-word BM25 leads take their place.
  Without the feature the binary is offline-only and `search-meaning` says so.

Tests: lifted vector/segment suites; chunk keys change with model id and
chunker version; offline opener makes no network call; hybrid ranking fixture.

## Status

Built (`pixel-index::semantic`, `pixel-daemon::meaning`, `pixel search-meaning`):
the chunker (symbol spans in, chunks out; windows for unparsed files), the
`i8` vector store with its lock, manifest and compaction, BM25 plus RRF with
path classes, the resident pool with the `warming` / `refreshing` /
`model_missing` / `failed` states, the offline opener, `--download`, the
`Meaning` op and the brief's `Evidence::meaning`. Tests run the whole path with
a deterministic hashing embedder; the real model has not been run in this
container (no route to the model host), so retrieval quality of
`potion-code-16M-v2` on Pixel's chunks is **unmeasured**.

Deliberately not carried: the `fastembed` (ONNX) backend. Model2Vec is pure Rust
and enough for the code-search model; ONNX needed a vendored OpenSSL and has no
musl build. The `semantic` feature is off in the default build until the
cross-build lanes carry the tokenizer stack; `cargo build -p pixel --features
semantic` enables it. The model is fetched from the repository's default
branch (unpinned) and not verified by digest; pinning a revision and digest is
a follow-up once a first download has been inspected.
The measurement is step 4b of `docs/maintainer-runbook.md`.
