# Profiling a pixel binary

How to find the hot path before changing it. Wall-clock benches say
*whether* a change helped; a profile says *what* to change.

Build with the profiling profile first: `dev-release` is the release
optimisation with line tables and without thin LTO, so samples land on a
file and a line instead of an address.

```bash
cargo build --profile dev-release -p pixel-cli
```

Anything that takes a `--repo`/`.` argument works as the subject. Prefer a
bench fixture or a pinned corpus over a repository you care about.

## macOS

`samply` needs no `sudo` and opens its own viewer:

```bash
cargo install samply
samply record -- ./target/dev-release/pixel prepare-repo --no-daemon .
```

`cargo install flamegraph` also works if DTrace is available
(`cargo flamegraph --profile dev-release -- prepare-repo .`).

## Linux

`perf` with DWARF call graphs (the `dev-release` profile carries the line
tables `--call-graph dwarf` needs), then <https://profiler.firefox.com> or `samply` to read it:

```bash
perf record --call-graph dwarf -- ./target/dev-release/pixel prepare-repo --no-daemon .
perf script | samply import perf.data
```

Gungraun is the maintained successor to iai-callgrind (renamed upstream at
0.17.0). It reports **executed instruction counts**, not the static
instruction count of a disassembled function. The Linux
`instruction-counts.yml` PR lane pins Rust 1.99.0, gungraun 0.20.0 and
Valgrind 3.22.0 on Ubuntu 24.04. Its seven cases call the production
`read_capped_line_with`, `write_response`, `cosine`, and
`SparseGramExtractor<Crc32Weigher>::grams` implementations:

```bash
cargo install --locked --version 0.20.0 gungraun-runner
CARGO_PROFILE_BENCH_STRIP=none CARGO_PROFILE_BENCH_DEBUG=line-tables-only \
cargo bench --locked -p pixel-bench --bench instructions \
  --no-default-features --features instruction-counts
# Full comparison; both directories are checkouts, output must not exist.
python3 -I scripts/ci/instruction-counts.py \
  --base /path/to/base --head /path/to/candidate --out /tmp/instruction-run
```

The read cases are 64 and 8192 bytes, with an injected fixed clock; writes
serialize 64/8192-byte JSON payloads into reused memory buffers. Cosine uses
384 and 769 dimensions (including a tail). Sparse grams use 256 repetitions
of a fixed source snippet, independent of the extractor's own source.
Fixtures and buffer capacity are prepared outside collection; the measured
functions include their loop, output consumption and argument destruction.
There are no socket syscalls, model inference, or filesystem reads in the
measured region. These are CPU-work tripwires, not end-to-end latency claims.

`scripts/ci/instruction-counts.py` runs two unchanged repeats (at most 1%
variation in either direction), then doubles real production work in every
case. The same checker must exit 1 for that negative control and identify
all seven regressions. Missing, duplicate, zero or malformed counts fail.
It then compares the PR base and head, failing on more than 5% Ir growth in
any case; improvements cannot hide a regression elsewhere. The 5% budget is
deliberately coarse and does not certify smaller changes as equivalent.
Each checkout has its own Cargo target and every measurement has a fresh
iai output directory. `identity.json`, commands/build logs, per-case JSON and
raw Callgrind profiles are retained as the `instruction-counts` artifact.
The runner overrides the inherited bench profile's stripping and enables
line tables: Callgrind needs the benchmark function's symbol to delimit
collection. A stripped benchmark can exit successfully with zero counts;
the checker rejects that. The workflow uses explicit Bash with `pipefail`,
so logging through `tee` cannot turn a failed checker into a green job.

On initial adoption, a base without the benchmark explicitly reports
`BOOTSTRAP`: only repeatability and negative-control evidence are available.
Manual dispatch compares the selected revision with itself. Once present on
the base, a changed harness fails rather than comparing different inputs;
fixture or tool-version migrations require explicit review of comparability.
Normal tests do not require Valgrind; only the opt-in feature builds this
target. Native Criterion and end-to-end timings remain necessary for SIMD,
I/O, concurrency and release-profile decisions.

## Allocations

`dhat` counts blocks and bytes. Two ways in:

- the standing ceilings in `crates/pixel-bench/tests/alloc_budget.rs`, which
  fail when a path starts allocating more than it does today;
- a one-off heap profile: set `DHAT_OUTPUT=<dir>` on a binary built with
  `dhat::Alloc` installed, then open the JSON in <https://nnethercote.github.io/dh_view/>.

## The daemon

`PIXEL_TARGETS_PROFILE=1` makes `targets` and the index search under it print
a per-stage microsecond line to stderr. Use it to name the slow stage before
reaching for a sampler.

## Reading the numbers

A profile attributes cost; it does not justify a change. Take the top frame,
change one thing, and re-run the matching bench (`wire`, `m1_latency`,
`tree_delta`, `gram_extraction`, `ndcg_relevance`) — and, for anything that
ships, the weekly lane's `prepare-repo` receipt.
