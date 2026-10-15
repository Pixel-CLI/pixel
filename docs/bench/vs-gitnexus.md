# pixel vs GitNexus — measured comparison

Measured 2026-09-21 on an Apple M2 / 16 GiB / Darwin 25.6.0, pixel 0.4.0
(`5c9b4ad`) against GitNexus 1.6.12 (`737634705`, rebuilt from source with
`npm run build` before the run). Full environment, corpus commits and raw rows:
[`vs-gitnexus/raw/`](vs-gitnexus/raw/). Reproduce with the scripts in
[`scripts/bench/bench-vs/`](../../scripts/bench/bench-vs/).

Same house rule as [`measured-performance.md`](measured-performance.md): every
number here is a measurement, losses are printed in the same voice as wins, and
a result too noisy to call is labelled inconclusive rather than rounded into a
claim.

## What this measures, and what it does not

Only the capabilities **both** tools implement are benchmarked. Comparing a tool
against a capability its competitor does not have measures nothing.

Not measured, and therefore not claimed anywhere: agent-level task time (the
`claude -p` three-arm A/B that [`pixel-bench.sh`](../../scripts/bench/pixel-bench.sh)
would drive), semantic/concept search quality, and `rename` correctness. Cold
index build is compared on **one** corpus only — see the caveat in that section.

## Capability overlap

Derived from source on both sides: `gitnexus/src/mcp/tools.ts` (17 MCP tools)
plus `gitnexus --help`, and `pixel --help`.

| GitNexus | pixel | Overlap |
|---|---|---|
| `impact` | `impact`, `who-calls` | full — **benchmarked** |
| `trace` | `call-path` | full — **benchmarked** |
| `detect_changes` | `what-changed` | full — **benchmarked** |
| `context` | `pack-context`, `find-symbol` | full — not benchmarked |
| `query` | `find-code`, `search-meaning`, `list-flows` | partial — not benchmarked |
| `rename` | `rename` | full — not benchmarked |
| `list_repos` | `status` | trivial |
| `check` | `plan` (findings only) | partial |
| `cypher`, `pdg_query`, `explain` | — | **GitNexus only** |
| `route_map`, `tool_map`, `shape_check`, `api_impact` | — | **GitNexus only** |
| `group_list`, `group_sync` | — | **GitNexus only** |
| — | `search-content` (indexed regex) | pixel only |
| — | `search-history`, `dig-history`, `file-history`, `who-wrote`, `plan-rollback` | pixel only |
| — | `repo-state`, `review-changes`, `commit`, `push`, `sync-branch`, `list-branches`, … | pixel only |
| — | `scope-task`, `plan`, `repo-map`, `list-signatures`, `note` | pixel only |
| — | `recall` (agent transcript corpus) | pixel only |

GitNexus covers a program-analysis surface pixel has no answer to at all: raw
Cypher over the graph, a persisted PDG with taint findings, and the
web-API-shaped tools (`route_map`, `shape_check`, `api_impact`). pixel covers a
git-history and repo-operations surface GitNexus does not implement.

## Blast radius (`impact` / `impact`) — 29 cases, 4 repos, 3 languages

Ground truth is mechanical and re-derivable
([`gen-truth.py`](../../scripts/bench/bench-vs/gen-truth.py)): for a symbol with
exactly one definition, the truth set is the files containing a call site of it.
Both tools are scored at **depth 1** with the same scorer — the layer whose
expected answer is a fact about the source rather than about a graph.
Ambiguous names are excluded (both tools correctly refuse them; that is a
different question). Median of 5 reps after a discarded warm-up.

| Corpus | Lang | n | pixel recall | GN recall | pixel prec. | GN prec. | pixel p50 | GN p50 | pixel bytes | GN bytes |
|---|---|---|---|---|---|---|---|---|---|---|
| pixel | Rust | 8 | **1.00** | 0.87 | 1.00 | 1.00 | **179 ms** | 429 ms | **6 375** | 28 139 |
| GitNexus | TypeScript | 8 | **1.00** | 0.88 | **0.97** | 0.88 | **182 ms** | 437 ms | **6 212** | 8 100 |
| alonetone | Ruby | 5 | 0.90 | **1.00** | n/a | n/a | **127 ms** | 401 ms | **2 196** | 2 873 |
| dd-trace-rb | Ruby | 8 | 0.56 | **0.68** | n/a | n/a | **114 ms** | 449 ms | 2 417 | **1 869** |
| **All** | | **29** | **0.86** | 0.84 | **0.98** | 0.94 | **153 ms** | 432 ms | **4 518** | 11 008 |

Precision is a mean over the **16** cases whose truth set is complete, not all
29. The Ruby truth sets match call syntax with parentheses, which Ruby callers
routinely omit, so they are a strict subset of the real call sites: a tool that
correctly returns a paren-less caller would be scored as imprecise for being
right. Recall tolerates a subset of truth, precision does not.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="charts/impact-recall-dark.svg">
  <img alt="Blast radius recall at depth 1 by corpus, pixel against GitNexus." src="charts/impact-recall-light.svg">
</picture>

Readings, in order of how much weight they carry:

- **Recall overall is a tie** (0.86 vs 0.84 over 29 cases). Anyone quoting the
  aggregate as a pixel win is over-reading a 2-point gap at n=29.
- **The split by language is the real result.** pixel answers Rust and
  TypeScript completely (1.00 / 1.00, including on GitNexus' own TypeScript
  codebase); GitNexus answers Ruby better (1.00 / 0.68 vs 0.90 / 0.56).
- **pixel has the better precision on the corpora where precision is scorable**
  (0.98 vs 0.94 over 16 cases). An earlier revision of this page reported the
  reverse, 0.85 against 0.89, by averaging in the two Ruby corpora — whose
  precision this same page calls untrustworthy two paragraphs above. That number
  is withdrawn; it measured the fixture, not the tools.
- **The dynamic-Ruby precision problem is real even though it is not scorable.**
  On `application` in dd-trace-rb, pixel returned 8 depth-1 files and not one is
  a caller, while GitNexus returned none. Eight confident wrong answers are
  worse for an agent than silence, whatever the corpus-level number says.
- **Latency is pixel's, consistently**: 153 ms vs 432 ms p50, every corpus, no
  overlap. This includes each runtime's process-start floor (a ~45 MB Rust
  binary talking to a warm daemon vs a Node CLI boot) — that floor is a property
  of the products, not of the harness.
- **Answer size is pixel's on 3 of 4 corpora**, decisively on Rust (6.4 KB vs
  27.5 KB mean; one case, `acquire_with_state_root`, is 12.6 KB vs 76.3 KB).
  GitNexus is more compact on dd-trace-rb.

### The Ruby loss, diagnosed

pixel's worst case is `build_coverage_matrix` in dd-trace-rb: truth 10 files,
pixel found 0, GitNexus found 9. The cause is specific and reproducible:

```
$ pixel find-symbol build_coverage_matrix
note: lower bound — 126 same-name call site(s) unresolved
function  build_coverage_matrix  appraisal/generate.rb:99-128
```

The call sites are at the **top level of Ruby script files** (`appraisal/*.rb`),
not inside any method. pixel's text index contains them (`pixel search-content`
returns all of them), but its code graph has no enclosing symbol node to hang a
`CALLS` edge on, so `impact` reports nothing. `extract_metadata` in alonetone
fails the same way, missing a call in `db/seeds/`.

Two honest notes on that: the gap is real and costs pixel the Ruby corpora, and
pixel did *not* silently answer "no callers" — the `lower_bound` marker in its
own output announced the unresolved sites. Fixing it means modelling file-level
script scope as a graph node for Ruby. Tracked as an open gap; nothing in the
user-facing comparison claims Ruby parity.

## Working-tree change mapping (`detect_changes` / `what-changed`) — n=3

The harness edits three known functions across three crates, so the changed-symbol
set is constructed rather than inferred, then reverts them in a `finally` block
([`bench-changes.py`](../../scripts/bench/bench-vs/bench-changes.py)).

| | recall | p50 | bytes |
|---|---|---|---|
| pixel | 1.00 (3/3) | 185 ms | 1 988 |
| GitNexus | 1.00 (3/3) | 399 ms | **774** |

A tie on correctness. pixel is 2.2× faster; GitNexus' answer is 2.6× smaller
because its CLI prints a prose report where pixel emits JSON (including its
`epistemics` block). Via MCP, GitNexus returns structured content too — the
format gap is a CLI-surface difference, not a capability one.

## Call-path finding (`trace` / `call-path`) — n=6

Pairs whose call site was located in the source and recorded in the case file,
so "a path exists" is a fact about the code
([`gen-path-cases.py`](../../scripts/bench/bench-vs/gen-path-cases.py)).

| Outcome | pixel | GitNexus |
|---|---|---|
| Path found (3 unambiguous pairs) | 3/3 | 3/3 |
| Ambiguity reported instead of a guess (3 pairs) | 3/3 | 3/3 |
| p50 latency | **~155 ms** | ~341 ms |

A clean tie on behaviour, including the part that matters most — neither tool
guesses when a name is ambiguous. n=6 is too small to separate them on anything
but latency.

## Context tax

What each tool costs in the model's context window on **every turn**, before it
has answered anything. Measured from the live MCP handshake (`tools/list`) and
from the installed files; token figures are bytes/4, pixel's own accounting
convention.

| | Always-on bytes | ≈ tokens |
|---|---|---|
| GitNexus: 17 MCP tool schemas | 75 202 | ~18 800 |
| GitNexus: injected `CLAUDE.md` block | 3 727 | ~930 |
| **GitNexus total** | **78 929** | **~19 700** |
| pixel: installed agent prompt | 16 631 | ~4 160 |
| **pixel total** | **16 631** | **~4 160** |

pixel's permanent context cost is **4.7× smaller**. The largest single GitNexus
schema, `impact`, is 21 946 bytes on its own — more than pixel's entire doctrine.

Caveats that belong with this number: a user can disable individual MCP tools;
GitNexus also writes six skill files (45 679 bytes total) whose *descriptions*
are always-on for both tools' skill systems; and neither figure includes the
tokens an actual answer costs (measured in the impact table above).

## Index build and footprint — pixel's own repo only

Both tools cold (no prior index directory), serial, uncontended, 354 tracked
files.

| | Time | On-disk |
|---|---|---|
| pixel `build-index` + `rebuild-graph` | **9.3 s** (6.95 + 2.36) | **8.6 MB** (2.0 shard + 6.6 graph) |
| GitNexus `analyze --index-only` | 28.7 s | 184 MB (128 lbug + 55 parse caches) |
| pixel `--history` (opt-in, no GN counterpart) | +146 s | +849 MB (225 db + 624 WAL) |

**Caveat, load-bearing:** this is one corpus. The three other corpora were *not*
cold-matched — pixel reindexed over an existing `.pixel` while GitNexus built
from nothing — so their build times are recorded in the raw logs and are
deliberately **not** presented as a comparison. On dd-trace-rb the raw numbers
favour GitNexus (31.9 s vs pixel's 48.9 s across both steps); whether that
survives a cold-matched re-run is unknown and no claim is made either way.

pixel's history database is the one place its footprint is far larger, and it
buys a capability GitNexus does not offer at all (`search-history`,
`who-wrote`, `plan-rollback`). It is opt-in; the comparable state is 8.6 MB.

## Fairness fixes applied to the harness

Each of these initially produced a fake GitNexus loss or a fake pixel loss and
was fixed before any number above was recorded. Listed so a reader can audit
them:

1. **Node truncates its own stdout at exactly 64 KiB on a pipe.** The first run
   scored `acquire_with_state_root` as GitNexus recall 0.00 on truncated JSON.
   Both arms now redirect to a file. Real result: 1.00.
2. **`gitnexus detect-changes` has no `--json` flag** — it prints a report. The
   first run scored it 0.00 while its output listed all three symbols
   correctly. The harness now parses that report. Real result: 1.00.
3. **`gitnexus trace` signals a hit with `status: "ok"` + `hops`**, not a
   `found` key. The first parser scored 4 found paths as 4 misses.
4. **`#[cfg(test)]` call sites were polluting the Rust truth set.** Both tools
   exclude test code by default, so both "missed" `intent.rs`. Truth generation
   now strips in-file test modules; Rust recall moved 0.98 → 1.00 for pixel and
   0.85 → 0.87 for GitNexus.
5. **A `#[cfg(test)]` function had become a call-path case**, which pixel
   correctly does not index. Dropped as a harness artifact, not a result.

Remaining known weaknesses of the method: Ruby truth sets under-count
paren-less calls, so Ruby precision is **not reported at all** rather than
reported with a caveat; n is 5–8 per corpus; and the whole run is one machine on
one day.

## Open gaps this run produced

- Ruby top-level script call sites are invisible to pixel's code graph
  (mechanism above). Costs pixel both Ruby corpora.
- pixel's depth-1 precision on dynamic Ruby (`application`: 8 reported, 0 real)
  — visible per case, not measurable per corpus while the Ruby truth sets stay
  paren-only. A truth generator that parses Ruby rather than grepping it would
  make this scorable.
- No agent-level A/B was run: everything here is op-level.
- `context`, `query` and `rename` overlap but were not benchmarked.

## Raw data

- [`raw/summary.txt`](vs-gitnexus/raw/summary.txt) — aggregate + per-case head-to-head
- [`raw/impact-*.json`](vs-gitnexus/raw/) — one row per case and tool: the
  files reported at each depth, recall and precision, the median and fastest
  of the timed repetitions, and the answer size. The individual repetitions
  are summarised, not retained
- [`raw/changes-rust.json`](vs-gitnexus/raw/changes-rust.json), [`raw/path-rust.json`](vs-gitnexus/raw/path-rust.json)
- [`raw/environment.txt`](vs-gitnexus/raw/environment.txt) — machine, versions, corpus commits
- [`cases/`](vs-gitnexus/cases/) — the ground-truth fixtures, regenerable
