# Language Alternatives Audit Prompt

> **Reusable local audit prompt** — issue #526. This document is the *prompt*,
> not the audit. It defines how to compare language alternatives for Pixel
> while preserving runtime performance. The audit itself and any production
> migration are separate work. Audit artifacts remain local under this
> directory (see `.gitignore`).

## Purpose

Pixel is a code-intelligence and agent-retrieval CLI written in Rust. Before
any migration decision, this prompt guides a structured, evidence-based
comparison of language alternatives. The comparison is **local** (no external
services), **measured** (every claim carries a number and a command), and
**reusable** (the same criteria apply to any future re-audit).

## Pixel workload profile

The audit evaluates languages against the workloads Pixel actually runs. From
`ARCHITECTURE.md`:

| Workload | Crate | Why it matters for language choice |
| --- | --- | --- |
| Sparse trigram text index — gram extraction, window weighting, posting-list algebra, query planner | `pixel-index` | Hot path on every search; allocation-heavy, cache-sensitive |
| Code graph — tree-sitter extraction of symbols/imports/calls, tiered call resolution, SQLite storage | `pixel-graph` | Multi-language parsing, heavy data structures, persistence |
| History facts — FTS5 trigram indexes over diffs, bounded catch-up, eviction | `pixel-facts` | SQLite FTS5 usage, background ingestion, memory bounds |
| Ranking and fusion — task-to-file scoring, BM25 + semantic fusion | `pixel-rank` | Pure computation, latency-sensitive |
| Daemon — Unix socket NDJSON, filesystem watching, request dispatch | `pixel-daemon` | Concurrency, I/O multiplexing, long-running process |
| Git subprocess wrapper — timeout, output cap, redacted stderr, trust boundary | `pixel-git` | Process management, security-critical |
| Recall — transcript ingestion, embedding backends (ONNX, model2vec), semantic search | `pixel-recall` | ML inference, SQLite corpus, lazy loading |
| CLI + hooks — clap parsing, cross-platform binary, agent integrations | `pixel`, `pixel-install` | Startup time, distribution, cross-platform correctness |

**Performance envelope to preserve** (measure on the current Rust binary, record
as the baseline before comparing alternatives):

- Cold start (process spawn to first output): ___ ms
- Warm daemon request p50 / p95: ___ / ___ ms
- `pixel audit` on this repo (20 files): ___ ms
- `pixel scope-task` on a 5k-file repo: ___ ms
- Peak RSS during `pixel build-index`: ___ MiB
- Binary size (release, stripped): ___ MiB

## Languages to compare

| # | Language | Runtime / toolchain | Notes |
| --- | --- | --- | --- |
| 1 | **Rust** (current) | `cargo`, rustc 1.91+ | Baseline; `unsafe` blocks audited, `clippy -D warnings` |
| 2 | **TypeScript / Bun** | Bun 1.x | Single-binary via `bun build --compile`; native addons for hot paths |
| 3 | **Zig** | Zig 0.14+ | Manual memory, comptime, C interop; no mature async runtime |
| 4 | **Go** | Go 1.24+ | GC, goroutines, single static binary; FTS5 via cgo or modernc |
| 5 | **C++** | Clang 19+ / GCC 14+ | Manual memory, no GC; build complexity, no built-in async |
| 6 | **C# Native AOT** | .NET 9+ `PublishAot` | Single binary, no JIT startup; GC, limited `unsafe` |
| 7 | **Ruby / Spinel** | Ruby 3.4+ / Spinel (Crystal-like) | Spinel = Ruby syntax on a compiled runtime; GC, no true static binary |

## Evaluation criteria

Each language is scored against five gates. A gate **passes** only when every
sub-check passes with measured evidence. Record the command and its output
beside every number.

### Gate 1 — Correctness

Pixel's correctness rests on: multi-language tree-sitter parsing, SQLite
schema integrity, git subprocess safety, cross-platform file handling, and
symbol resolution that never guesses (unresolved sites are reported, not
assumed).

| # | Sub-check | Pass condition |
| --- | --- | --- |
| 1.1 | **Memory safety** — no use-after-free, double-free, or data races in the index/graph/daemon paths | Language enforces at compile time or runtime; no `unsafe` equivalent needed for core data structures |
| 1.2 | **Tree-sitter bindings** — can drive tree-sitter's C API (or a native parser) for Rust, TypeScript, Ruby, Python, Go, C/C++ | Binding exists and is maintained; or a native parser with equivalent query support |
| 1.3 | **SQLite access** — FTS5, WAL mode, migrations, prepared statements | Library supports FTS5 + WAL; migrations are type-safe or testable |
| 1.4 | **Git subprocess safety** — timeout, output cap, redacted stderr, no shell injection | Process spawning with arg arrays (no shell), timeout, stdout/stderr caps |
| 1.5 | **Cross-platform paths** — `/` vs `\`, symlink handling, case sensitivity, `.pixel/` trust boundary | Standard library or ecosystem handles Windows + macOS + Linux correctly |
| 1.6 | **Error model** — recoverable errors don't crash the daemon; partial index corruption is detected, not silently used | Typed errors or equivalent; no exceptions across FFI boundaries |

### Gate 2 — Runtime performance

The numbers that matter for agent iteration: startup latency, warm-request
throughput, memory ceiling, and index-build speed.

| # | Sub-check | Pass condition | How to measure |
| --- | --- | --- | --- |
| 2.1 | **Cold start** — process spawn to first stdout byte | ≤ 150% of Rust baseline | `hyperfine --warmup 3 --prepare 'kill %1 2>/dev/null || true' '<binary> --version'` (measure spawn → first byte only) |
| 2.2 | **Warm daemon p50** — median round-trip on a cached request | ≤ 120% of Rust baseline | `hyperfine --warmup 10 -N --prepare 'true' -m 200 'curl -sf http://127.0.0.1:<port>/api/search?q=foo'` against an already-running daemon |
| 2.3 | **Warm daemon p95** — 95th percentile round-trip | ≤ 150% of Rust baseline | Collect 200 samples, sort, take 190th |
| 2.4 | **Peak RSS during build-index** — max resident set size | ≤ 200% of Rust baseline | `/usr/bin/time -v <binary> build-index .` (binary and arguments as separate tokens) |
| 2.5 | **Binary size** — release, stripped | ≤ 300% of Rust baseline | `ls -la` on the shipped binary |
| 2.6 | **Index-build throughput** — files/second on a 5k-file repo | ≥ 50% of Rust baseline | Time `build-index` on a fixed corpus, count files |

### Gate 3 — Representative prototypes

A language that passes Gates 1–2 on paper still fails if the real workload
exposes a gap. Build a **minimal prototype** in each language that exercises
the three hottest Pixel paths. The prototype is throwaway; it proves the
language can carry the workload, not that it can ship the product.

| Prototype | What it proves | Minimum viable scope |
| --- | --- | --- |
| **P1: Trigram index** | N-gram extraction, posting-list intersection, query planning | Index 10k lines of code, run 100 queries, measure p50/p95 |
| **P2: Tree-sitter graph** | Parse Rust + TypeScript + Ruby, extract symbols/imports, resolve calls | Parse 500 files, build a symbol table, resolve 100 call sites |
| **P3: Daemon + SQLite** | Unix socket NDJSON server, SQLite FTS5 + WAL, concurrent requests | Serve 100 concurrent search requests against an FTS5 index |

Each prototype must: build from a clean checkout in one command, run without
a network connection, and print its own timing. Record: build time, run time,
peak RSS, and lines of code.

### Gate 4 — Measured agent iteration time

Pixel's users are agents. The metric that matters is **how fast an agent can
edit → build → test → deploy** a change. A language with a 2-second compile
and a 50ms test suite beats a language with a 30-second compile and a 5ms test
suite when the agent iterates 100 times per session.

| # | Sub-check | Pass condition | How to measure |
| --- | --- | --- | --- |
| 4.1 | **Incremental build** — one file changed, rebuild | ≤ 5s for a 500-crate-equivalent workspace | Touch one file, time the rebuild |
| 4.2 | **Test suite** — full unit tests | ≤ 60s wall clock | Time `cargo test` / `go test ./...` / etc. |
| 4.3 | **Cross-compile** — Linux → macOS (or reverse) | Possible with ≤ 2 config changes | Attempt the cross-compile, record blockers |
| 4.4 | **Debug build** — unoptimized build for development | ≤ 30s | Time a clean debug build |
| 4.5 | **IDE support** — LSP, go-to-definition, type inference on Pixel's data structures | Works for the core crates | Manual check: can an agent navigate `pixel-graph`? |

### Gate 5 — Migration economics

A technically superior language is not a better choice if the migration costs
more than the benefit. Estimate the full cost before recommending anything.

| # | Sub-check | What to estimate |
| --- | --- | --- |
| 5.1 | **Lines of code to migrate** | Total across all crates; identify the 20% of code that carries 80% of the complexity |
| 5.2 | **Ecosystem gaps** | For each crate, list the Rust crates it depends on and whether an equivalent exists in the target language |
| 5.3 | **Team expertise** | Current contributors' familiarity with each language (survey or self-assessment) |
| 5.4 | **Hiring pool** | Availability of developers who know the language *and* systems programming |
| 5.5 | **Distribution** | Can the language produce a single static binary for Linux, macOS, Windows? What is the story for code signing and notarization? |
| 5.6 | **Risk** | What breaks if the migration stalls at 50%? Is there a way to run both languages side by side (FFI, sidecar process)? |

## Audit procedure

Follow these steps in order. Record every command and its output in a local
artifact under `docs/audit/` (kept out of git by the `.gitignore`).

### Step 0 — Baseline

Build the current Rust binary in release mode. Measure and record the
performance envelope above. This is the number every alternative is compared
against.

```bash
cargo build --release -p pixel-cli
hyperfine --warmup 3 './target/release/pixel --version'
# ... record all five envelope metrics
```

### Step 1 — Gate 1 (correctness) screening

For each language, answer the six sub-checks from Gate 1. A language that
fails 1.1 (memory safety) or 1.2 (tree-sitter) is **eliminated** unless the
failure can be bounded (e.g., `unsafe` blocks audited line by line, or a
native parser with a compatibility test suite). Record the elimination reason.

### Step 2 — Gate 3 (prototypes)

For each surviving language, build the three prototypes. A prototype that
cannot be built in one command, or that crashes on the workload, eliminates
the language. Record build time, run time, peak RSS, and LOC.

### Step 3 — Gate 2 (runtime) measurement

For each language with a working prototype, measure the six runtime sub-checks.
Compare against the Rust baseline. A language that fails 2.1 (cold start) or
2.2 (warm p50) by more than the threshold is flagged; the flag is a decision
input, not an automatic elimination.

### Step 4 — Gate 4 (iteration time)

For each surviving language, measure the five iteration sub-checks. This gate
is scored, not pass/fail: a language that is slower to iterate but faster at
runtime may still win, depending on how often agents rebuild.

### Step 5 — Gate 5 (economics)

For each language that passes Gates 1–4, fill in the six economics sub-checks.
Assign a rough person-week estimate for the full migration. A migration that
estimates more than 52 person-weeks (one person for one year) requires
extraordinary justification.

### Step 6 — Decision matrix

Produce a final matrix. One row per language, one column per gate. Each cell:
`PASS`, `FAIL`, `SCORE: n/10`, or `ELIMINATED (reason)`. The recommendation
follows from the matrix; it is not an input to it.

## Output format

The audit produces a single local artifact: `docs/audit/language-alternatives-<date>.md`.
It contains:

1. **Baseline** — the five Rust envelope metrics with commands
2. **Gate 1 results** — one table per language, six sub-checks each
3. **Gate 3 results** — prototype metrics per language
4. **Gate 2 results** — runtime metrics per language, against baseline
5. **Gate 4 results** — iteration metrics per language
6. **Gate 5 results** — economics estimates per language
7. **Decision matrix** — the final table
8. **Recommendation** — one paragraph, evidence-linked, with the specific
   measurement that drove it

## Reusability

This prompt is versioned with the repo. When a new language becomes relevant
(e.g., a mature Go FTS5 binding, or a Zig async runtime), add it to the
language table and re-run the procedure. When Pixel's workload changes
significantly (e.g., a new hot path), update the workload profile and the
prototype scope. The gates themselves are stable: correctness, runtime,
prototypes, iteration, economics.

The audit is a point-in-time measurement. Re-run it when a language's
toolchain has a major release, when Pixel's performance envelope shifts by
>20%, or when a new candidate language reaches maturity.
