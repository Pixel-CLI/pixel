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

`iai-callgrind` runs under valgrind and reports **instruction counts**, which
is the deterministic number to quote for a codegen change; it needs Linux and
`valgrind` installed. It is the local counterpart of the wall-clock lanes,
which are noisy on a shared machine:

```bash
cargo install iai-callgrind-runner
cargo bench -p pixel-bench --bench wire   # under iai when wired for it
```

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
