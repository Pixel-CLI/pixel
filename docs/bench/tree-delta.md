# Cost of the whole-tree freshness check (`tree_delta`)

`pixel_graph::build::tree_delta(root, db)` is the check the daemon runs when
it reopens the graph, and the check the `pixel evaluate` design makes a
precondition of every answer: once before the traversal, once after, so a
negative answer is never certified against a working tree that moved. Its
cost is therefore the floor under every `evaluate` call. This page measures
it as it is called, on this repository and on a synthetic 50 000-file tree,
and reads the numbers against the design note's targets.

Re-run with one command (the harness prints these tables):

```sh
cargo bench -p pixel-bench --bench tree_delta
# knobs: PIXEL_BENCH_TREE_ROOT=<dir> PIXEL_BENCH_SYNTH_FILES=50000 PIXEL_BENCH_ITERS=25 PIXEL_BENCH_RAW=1
#        PIXEL_BENCH_JSON=<file> also writes the numbers as JSON
```

## Weekly CI tripwire

`.github/workflows/perf.yml` runs the bench every Monday with
`--profile dev-release`, on the `excalidraw/excalidraw` corpus pinned in
`.github/perf-baseline.json` and on a generated tree of 10 000 files (the
50 000-file build alone takes about 40 s). `scripts/ci/check-perf.py --label
tree_delta` fails the run when either `tree_delta` p50 passes three times its
baseline, or when a subject indexed fewer files or took fewer samples than
asked. Baseline, GitHub `ubuntu-26.04` runner, 2026-10-19, 25 warm iterations:

| Subject | Files | p50 ms | min–max ms |
| --- | ---: | ---: | ---: |
| excalidraw corpus | 708 | 10.5 | 10.4–10.9 |
| synthetic tree | 10 000 | 54.9 | 54.3–55.7 |

The runs below are the manual study on a laptop; refreshing the CI numbers is
a pull request that edits the baseline and this table together.

## Method

- Harness: `crates/pixel-bench/benches/tree_delta.rs`, `harness = false`,
  plain `Instant` timing (criterion's sampling adds nothing to a call this
  long). For each subject: `build_graph` into a temp db, one "first run"
  `tree_delta`, then N warm iterations alternating two lanes.
- Lane `tree_delta`: the real call, walk + content hash + comparison with
  the `files` table. Lane `freshness_signature`: the same walk and hash
  without the db. The API exposes no timer inside `tree_delta`, so the db
  share is reported as the difference of the two lanes' p50, labelled
  derived.
- Subject (a): this worktree's tree (`249` supported source files after
  the walk policy). Subject (b): `50 000` Rust files, each a small module
  with four real items padded with comment lines to 1 024–4 096 bytes,
  spread over 400 directories, written to a temp dir the harness removes.
  The 50 000 figure is exactly `DEFAULT_GRAPH_MAX_FILES`; the walk in
  `tree_delta` has no cap, so the two file sets match.
- "First run" is the first `tree_delta` after `build_graph` in the same
  process. It is **not** a cold-cache figure: `build_graph` has just read
  every file, so the page cache is warm. macOS gives no unprivileged way to
  drop it (`purge` needs root, and this bench does not ask for it). A true
  cold number was not measured.
- Machine: Apple M2, 8 cores, 16 GiB, Apple Fabric SSD, macOS 26.6.2,
  Spotlight indexing enabled on `/`. Rayon threads available: 8 (the walk
  does not use them, see Reading). Pixel commit `6bc8acf`. Release profile
  (`cargo bench`).

## Results, run 1 (25 warm iterations per lane)

### Repository tree, 249 files, 4 059 symbols, 7 211 edges

`build_graph` 2 525 ms · first `tree_delta` after build 10.7 ms

| lane | iters | min ms | p50 ms | p95 ms | max ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| tree_delta (walk + hash + db compare) | 25 | 13.0 | 16.2 | 17.4 | 19.0 |
| freshness_signature (walk + hash only) | 25 | 12.4 | 15.4 | 17.4 | 19.9 |
| db comparison share (derived) | — | — | 0.7 | — | — |

### Synthetic tree, 50 000 files, 200 000 symbols, 100 000 edges

Generated in 4 019 ms · `build_graph` 40 078 ms · first `tree_delta` after
build 6 378 ms

| lane | iters | min ms | p50 ms | p95 ms | max ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| tree_delta (walk + hash + db compare) | 25 | 1 317.7 | 1 616.9 | 8 312.5 | 8 430.2 |
| freshness_signature (walk + hash only) | 25 | 1 287.7 | 1 546.6 | 8 195.0 | 8 306.3 |
| db comparison share (derived) | — | — | 70.3 | — | — |

## Results, run 2 (40 warm iterations per lane, `PIXEL_BENCH_RAW=1`)

Same machine, about fifteen minutes later, **under load**: a sibling
worktree was running `cargo build`/`cargo test` for another change, plus a
Rails app's Sidekiq workers; `uptime` right after the run read load
averages 2.99 / 4.48 / 5.88 on 8 cores.

### Repository tree, 249 files

`build_graph` 2 488 ms · first `tree_delta` after build 10.2 ms

| lane | iters | min ms | p50 ms | p95 ms | max ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| tree_delta (walk + hash + db compare) | 40 | 13.2 | 16.7 | 24.6 | 30.0 |
| freshness_signature (walk + hash only) | 40 | 11.3 | 15.8 | 22.3 | 36.4 |
| db comparison share (derived) | — | — | 0.9 | — | — |

### Synthetic tree, 50 000 files

Generated in 4 470 ms · `build_graph` 39 262 ms · first `tree_delta` after
build 7 423 ms

| lane | iters | min ms | p50 ms | p95 ms | max ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| tree_delta (walk + hash + db compare) | 40 | 6 426.2 | 6 985.4 | 8 618.6 | 13 714.8 |
| freshness_signature (walk + hash only) | 40 | 6 291.7 | 6 818.6 | 11 314.9 | 22 390.8 |
| db comparison share (derived) | — | — | 166.9 | — | — |

Sorted `tree_delta` samples, run 2: 6 426 … 6 985 (median) … 7 557,
7 907, 8 161, 8 373, 8 403, 8 619, 8 709, 13 715 ms. No fast mode at all:
run 1's 1.3–1.6 s band did not reappear under load.

**The two runs disagree by 4× on the 50 000-file tree and agree within
noise on the 249-file tree.** Run 1's few 8 s samples are the same regime
run 2 lived in throughout. The serial walk (see Reading) competes for one
core with everything else on the machine, so the 50 000-file cost is best
stated as a range, 1.3 s idle to 7 s loaded, not a point.

## Reading against the design note's targets

The targets were warm p95 under 100 ms for a hook and under 300 ms for an
agent call, whole-tree check included, with the check run twice per
`evaluate` (before and after the traversal).

- **This repository (249 files): met.** Two checks cost about 32 ms at p50
  and 35 ms at p95; the traversal budget of 250 ms has room. The db
  comparison is under 1 ms; the walk and hashing are the whole cost.
- **The 50 000-file tree: not met, by one to two orders of magnitude.** One
  check is 1.6 s at p50 on an idle machine (run 1) and 7.0 s under ordinary
  developer load (run 2). Two checks per call put a 50 000-file repository
  between 3 s and 14 s. `evaluate` cannot run this check synchronously at
  that size; the design note's "measured, not claimed" clause applies and
  the note must say so.
- **Why it is this slow, from the code, not the profiler**: `tree_hashes`
  in `build.rs` walks, reads and hashes every file in one serial iterator
  (`walker.flatten().filter_map(..)`), then sorts. Its doc comment says the
  hashing is "parallelized via rayon"; the code does not do that. The
  build path (`collect_files` then `into_par_iter`) is parallel; the
  freshness walk is not. Parallelising the read + hash over the 8 cores is
  the obvious first lever, and the derived db share (70 ms) says the
  SQLite comparison is not the problem.
- **The slow regime is load, first of all.** Run 2 reproduced run 1's 8 s
  outliers as its steady state while other builds ran; a serial walk gets
  one core's worth of a contended machine. Spotlight (indexing enabled on
  `/`, the tree written under `/var/folders`) may add to it, since an
  indexer reading 50 000 fresh files would compete for the same I/O; that
  part is a hypothesis, not a measurement. `PIXEL_BENCH_RAW=1` prints every
  sample so the shape of a run is visible, and an idle-machine re-run is
  the way to separate the two.
- **Size cap**: files over `MAX_FILE_BYTES` (4 MiB) are skipped by both the
  build and the freshness walk; the synthetic files are far below it, so
  the cap did not shape these numbers.

What this changes for `evaluate`: the snapshot contract stays (whole-tree
before and after), the cost does not fit a synchronous hook above a few
thousand files today, and the two levers are a parallel `tree_hashes` and,
if that is not enough, reusing the daemon's watcher-maintained signature
with the after-check limited to files the watcher reported since the
before-check. Both are changes to `build.rs`, out of this page's scope.
