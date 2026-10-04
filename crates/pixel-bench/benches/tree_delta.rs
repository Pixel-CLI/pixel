// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Cost of the whole-tree freshness check.
//!
//! `pixel_graph::build::tree_delta(root, db)` is what the daemon runs when it
//! reopens the graph, and what the `evaluate` design note makes a
//! precondition of every answer (once before the traversal, once after).
//! Its cost is one parallel content-hash walk of every supported source
//! file plus a comparison against the `files` table. This bench measures it
//! as it is called, not a model of it.
//!
//! Two subjects, each after a `build_graph` so the db carries a signature:
//! - the tree at `PIXEL_BENCH_TREE_ROOT` (default: this workspace);
//! - a synthetic tree of `PIXEL_BENCH_SYNTH_FILES` (default 50 000) small
//!   Rust files in a temp dir, `0` skips it.
//!
//! `PIXEL_BENCH_ITERS` (default 25) warm iterations per lane;
//! `PIXEL_BENCH_RAW=1` also prints every sample, sorted, so a bimodal
//! distribution is visible instead of hidden behind p50/p95. Two lanes:
//! `tree_delta` (walk + hash + db comparison) and `freshness_signature`
//! (walk + hash alone, the same walk `tree_delta` makes). The API exposes
//! no timer inside `tree_delta`, so the db-comparison share is reported as
//! the difference of the two lanes' medians, and labelled derived.
//!
//! "first run" is the first `tree_delta` call after `build_graph` in this
//! process. It is not a cold-cache number: `build_graph` has just read every
//! file, so the page cache is warm, and macOS offers no unprivileged way to
//! drop it (`purge` needs root). The doc states this limit.
//!
//! Run with: cargo bench -p pixel-bench --bench tree_delta
//! Output: a markdown table on stdout, ready for docs/bench/tree-delta.md.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pixel_graph::build::{build_graph, freshness_signature, tree_delta};
use tempfile::tempdir;

const DEFAULT_ITERS: usize = 25;
const DEFAULT_SYNTH_FILES: usize = 50_000;
/// Directories the synthetic tree spreads its files over.
const SYNTH_DIRS: usize = 400;
/// Padding bounds (bytes) that keep synthetic files in the 1–4 KiB band.
const SYNTH_MIN_BYTES: usize = 1024;
const SYNTH_MAX_BYTES: usize = 4096;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

/// Deterministic LCG so two runs generate byte-identical trees.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

/// `n` Rust files across `SYNTH_DIRS` directories, each a small module
/// with real items (so extraction has work to do) padded with a comment
/// block to a size in the 1–4 KiB band.
fn write_synthetic_tree(root: &Path, n: usize) {
    let mut rng = Lcg(0x5EED);
    for i in 0..n {
        let dir = root.join(format!("mod_{:03}", i % SYNTH_DIRS));
        std::fs::create_dir_all(&dir).expect("synthetic dir");
        let mut body = format!(
            "//! Synthetic module {i}.\n\
             pub struct Item{i} {{ pub id: u64 }}\n\
             impl Item{i} {{\n    pub fn id(&self) -> u64 {{ self.id }}\n}}\n\
             pub fn make_{i}(id: u64) -> Item{i} {{ Item{i} {{ id }} }}\n\
             pub fn twice_{i}(id: u64) -> u64 {{ make_{i}(id).id() * 2 }}\n"
        );
        let span = SYNTH_MAX_BYTES - SYNTH_MIN_BYTES;
        let target = SYNTH_MIN_BYTES + usize::try_from(rng.next()).unwrap_or(0) % span;
        let line = "// padding: lorem ipsum dolor sit amet, consectetur adipiscing elit\n";
        while body.len() < target {
            body.push_str(line);
        }
        std::fs::write(dir.join(format!("item_{i}.rs")), body).expect("synthetic file");
    }
}

struct Lane {
    name: &'static str,
    samples: Vec<Duration>,
}

impl Lane {
    fn percentile(&self, p: f64) -> Duration {
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }
    fn min(&self) -> Duration {
        self.samples.iter().copied().min().unwrap_or_default()
    }
    fn max(&self) -> Duration {
        self.samples.iter().copied().max().unwrap_or_default()
    }
}

fn ms(d: Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1000.0)
}

fn time<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let t0 = Instant::now();
    let out = f();
    (out, t0.elapsed())
}

/// Build the graph for `root` into a temp db, then measure both lanes.
fn measure(label: &str, root: &Path, iters: usize) {
    let db_dir = tempdir().expect("db tempdir");
    let db = db_dir.path().join("graph.db");
    let (stats, build) = time(|| build_graph(root, &db).expect("build_graph"));
    let (first, first_ms) = time(|| tree_delta(root, &db).expect("tree_delta"));
    let first = first.expect("db carries a signature after build_graph");
    assert!(
        first.fresh,
        "{label}: tree_delta right after build_graph must be fresh (changed={} removed={})",
        first.changed.len(),
        first.removed.len()
    );

    let mut delta = Lane {
        name: "tree_delta (walk + hash + db compare)",
        samples: Vec::with_capacity(iters),
    };
    let mut sig = Lane {
        name: "freshness_signature (walk + hash only)",
        samples: Vec::with_capacity(iters),
    };
    for _ in 0..iters {
        let (d, t) = time(|| tree_delta(root, &db).expect("tree_delta"));
        assert!(
            d.is_some_and(|d| d.fresh),
            "{label}: tree drifted mid-bench"
        );
        delta.samples.push(t);
        let (_, t) = time(|| freshness_signature(root));
        sig.samples.push(t);
    }

    println!("\n### {label}\n");
    println!(
        "files indexed: {} · symbols: {} · edges: {} · build_graph: {} ms · first tree_delta after build: {} ms\n",
        stats.files,
        stats.symbols,
        stats.edges,
        build.as_millis(),
        ms(first_ms)
    );
    println!("| lane | iters | min ms | p50 ms | p95 ms | max ms |");
    println!("| --- | ---: | ---: | ---: | ---: | ---: |");
    for lane in [&delta, &sig] {
        println!(
            "| {} | {} | {} | {} | {} | {} |",
            lane.name,
            lane.samples.len(),
            ms(lane.min()),
            ms(lane.percentile(0.50)),
            ms(lane.percentile(0.95)),
            ms(lane.max())
        );
    }
    if std::env::var_os("PIXEL_BENCH_RAW").is_some() {
        for lane in [&delta, &sig] {
            let mut sorted = lane.samples.clone();
            sorted.sort_unstable();
            let raw: Vec<String> = sorted.into_iter().map(ms).collect();
            println!("\nsorted samples, {}: {}", lane.name, raw.join(" "));
        }
    }
    let compare = delta.percentile(0.50).saturating_sub(sig.percentile(0.50));
    println!(
        "| db comparison share (derived: p50 delta − p50 signature) | — | — | {} | — | — |",
        ms(compare)
    );
}

fn main() {
    let iters = env_usize("PIXEL_BENCH_ITERS", DEFAULT_ITERS);
    let synth = env_usize("PIXEL_BENCH_SYNTH_FILES", DEFAULT_SYNTH_FILES);
    let root = std::env::var_os("PIXEL_BENCH_TREE_ROOT").map_or_else(workspace_root, PathBuf::from);

    println!(
        "rayon threads: {} · iters per lane: {iters}",
        std::thread::available_parallelism().map_or(0, std::num::NonZero::get)
    );
    measure(
        &format!("repository tree `{}`", root.display()),
        &root,
        iters,
    );

    if synth > 0 {
        let dir = tempdir().expect("synthetic tempdir");
        let ((), generated) = time(|| write_synthetic_tree(dir.path(), synth));
        println!(
            "\nsynthetic tree: {synth} files in {SYNTH_DIRS} dirs, {SYNTH_MIN_BYTES}–{SYNTH_MAX_BYTES} bytes each, generated in {} ms",
            generated.as_millis()
        );
        measure(&format!("synthetic tree, {synth} files"), dir.path(), iters);
    }
}
