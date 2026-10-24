# The code graph

Status: normative for `pixel-graph`. One pipeline, one store, one parse per file.

## Why one pipeline

The previous Pixel engine ran two: `pixel-graph` (name-based resolution with tiers) and
`mvp-index` (proof-only Rust and Ruby, installed as a second database and glued
in by `mvp_adapter` behind `PIXEL_GRAPH_SOURCE_*` switches). Measured on
Pixel: v1 Rust call precision 52.8% (it held all 72 judged-wrong edges), mvp
≥93.9% with none; v1 Ruby 60.0%. Pixel keeps mvp's architecture as the skeleton
and ports the other languages into it.

## Skeleton (lifted from mvp-index / mvp-store / mvp-model)

- `Rules` trait per language: `lang`, `version`, `wants(path)`,
  `accepts(path, head)`, `extract(path, content) → Extraction`, `unread`,
  `resolve`. Pure functions: no I/O, no global state. Adding a language is a
  rule module and a row in `RULES`.
- `tree::scan` lists the work tree through git (tracked and untracked).
  `link::plan` diffs it against the stored generation; `extract` parses the
  planned files with tree-sitter; `link::link` decides *every* site of the
  generation again so an incremental run stores exactly what a full run stores
  (**incremental == full**, checked by `truth.rs`).
- Store: one SQLite file `.pixel/graph.db`, one writer behind `.pixel/index.lock`,
  one generation published per transaction (a reader sees one generation whole;
  a crash leaves the last one). A corrupt or incompatible file is never read: the
  next writer rebuilds it. No `ALTER TABLE` migrations: a schema or extractor
  version change rebuilds.

## Rows

`files(path, hash, lang, indexed_at, facts, stat…)`,
`symbols(uid = path#name#kind[~n], file, kind, name, leaf, signature, shape, line, end_line)`,
`edges(src, dst, kind, line, tier, target, leaf, context)`, `meta`. Edge kinds:
calls, references, imports, extends/implements (and, for Ruby, routes and
associations). `symbols.shape` is new: the declaration header up to the body
opener, whitespace-normalised, capped (4 KiB). `signature` stays the first line
for display. `edges.tier` replaces `resolved`: `0 Proved`, `1 Likely`, `2 Open`.

## Resolution decisions

`Rules::resolve` returns `Proved(target)`, `Likely(target)` or `Open`.

- A language with proof rules (Rust, Ruby) never emits `Likely`: an edge exists
  only where its rules prove the target; every other site is `Open` and counted.
- Languages on the shared `names.rs` (TS/JS, Python, Go, Java, C#, the generic
  table) emit `Proved` only for T0 (same file, lexical, no receiver) and T1 (a
  named import binding, no receiver). T2 (unique name repo-wide) and any
  receiver tiebreak are `Likely`. Everything else is `Open`.
- Edges never cross languages: each language's `World` is resolved on its own.
- A `Likely` caller is always labelled and never blocks Stop (`hooks.md`).
- Open sites whose leaf matches a queried symbol feed the answer's
  `lower_bound` and `unresolved_same_name` fields. "0 callers" is a lower
  bound, never a proof of absence.

(`graph-resolver.md` carries the language rules: Rust `use` scope, shadowing,
alias, groups and globs; TS imports and re-exports; Ruby receivers.)

## Languages and port order

1. Rust (lift: modules, uses, types, methods, args, macros, cfg, manifests).
2. TS/TSX/JS (`walk_ts`, `ts_import_bindings`, `js_assignments`).
3. Python.
4. Go, Java, C#.
5. The generic table: PHP, C, Swift, Elixir, Lua, Kotlin: definitions and T0 only.
6. Ruby: lift mvp's proof-only Ruby, behind a latency gate (below).
7. Concept extractors for `find-code`: Svelte, Vue, HTML, JSON, YAML, CSS (**not built as extractors**: see "Concepts" below)
   (text-level, not tree-sitter).

**Ruby latency gate.** Ruby callers ship in v0.1 only if on the gumroad
repository: 20/20 briefs sent 1 s after an edit arrive; the post-edit check
stays ≤100 ms p95; graph freshness p95 ≤10 s. Otherwise Ruby ships
definitions-only (outlines and shape diffs, no callers) and the lift moves to
the next release. Pixel's evidence: `ruby-hybrid-daemon-2026-10-18` lost every
brief sent 1 s after an edit.

**Historical result (2026-10-22, RTO): the gate passed; Ruby ships with callers.** `scripts/verify/ruby-gate.py`
on `antiwork/gumroad` @ `acea9a7` (5,816 `.rb` files, graph of 6,905 files built in 15 s;
`dev-release` profile, a 4-CPU container), 20 edits each adding a method to a different
`app/` class, the daemon warm:

| Gate | Bar | Measured |
| --- | --- | --- |
| a brief asked 1 s after the edit arrives, non-empty | 20 of 20 | 20 of 20, slowest 0.04 s (it answers from the generation before the edit and says so) |
| `rto hook post-tool` (historical executable) | p95 ≤ 100 ms | p95 12 ms, median 9 ms |
| the new symbol is found by `find-symbol` | p95 ≤ 10 s | p95 6.84 s (median about 6 s) |

The graph's 6 s floor is the Ruby world being linked again after any Ruby edit; the gate
holds with margin, and an incremental link of the Ruby world is the follow-up if the bar
tightens.

## Incremental speed

Three changes over mvp's `link`, none relaxing incremental == full:

1. A language whose `World` did not change since the last generation is not
   resolved again (resolution is a pure function of its input).
2. The daemon keeps the current generation's rows in memory instead of loading
   them from SQLite every run.
3. Hooks never wait on `link`: readers serve the last published generation
   plus an in-memory overlay of files re-extracted since, so definitions and
   shapes are fresh at once and edges catch up when the writer publishes.

## Analyses (ported onto the new store)

`impact` (caller/callee breadth-first walk with depth and per-depth caps),
`changes` (hunk → symbol, `signature_changed`, suggested tests to depth 3),
`trace`/call-path, `review` (changed symbols whose callers did not change;
credential-shaped added lines), `rename` (graph-resolved sites verified against
a fresh parse before writing), `concept`/`concept_resolve` (`find-code`),
`context` (`pack-context` and `list-signatures`). Dropped: clusters, processes,
plans, cycles, predicates, targets.

## New API for the edit hooks

- `shape.rs` (pure): `diff_shapes(stored: &[Symbol], fresh: &[Symbol]) →
  Vec<ShapeChange>` with `Reshaped{old,new}`, `Removed`, `Renamed{to}` (a
  removed and an added symbol with the same shape in the same file).
- `store`: `signature_of(uid)`, `callers(uid, min_tier)`, `callees(uid)`.
- `edit.rs`: `check(store, root, path, proposed: Option<&[u8]>) → EditReport`
  with the changes, external callers split by tier, the open-site lower bound,
  and covering tests. uids exclude line numbers, so callers stored in other
  files keep pointing at the edited symbol across a re-extract.
- A file past the line cap falls back to a first-line signature diff.

## Tests that prove it

Lifted: mvp `truth.rs`, `grammar.rs`, the Rust and Ruby suites, the store
tests; Pixel's resolver contract tests per ported language
(`extract/contract_tests.rs`, `tests/all/unresolved_diagnostic.rs`). New: the
incremental == full property with a skipped language; `diff_shapes` cases
(wrapped parameters, rename, removal); the proof-language-never-Likely
invariant; the overlay serving a shape before the writer publishes.

## Concepts (`find-code`) and context (`pack-context`)

The previous Pixel engine kept a stored concept table (strings, UI text, routes, status codes, config keys,
environment reads) filled by per-language extractors and a resolver over it. Pixel does
not: `pixel_graph::analysis::concept` takes the lines the text index returns for a phrase
(`pattern`: the words in order, any separator between them, case-insensitive), says
what each line is by its extension and shape (`ConceptKind`: route, env read, UI text,
string, config key, code, comment), ranks them (kind, an exact phrase, test and docs
paths down, a short line up) and names the smallest enclosing symbol from the graph.
There is nothing to rebuild or to disagree with the text, and no `EXTRACTOR_VERSION`
to bump. What it gives up: normalisation across formatting (a phrase split over two
lines, camelCase against words), status-code and form concepts, and Svelte/Vue
`<script>` blocks parsed on their own; a concept in a minified or generated file is
whatever the index holds. `Evidence::concept` is the same function behind the brief's
concept fallback. `analysis::context::pack` fits one symbol (its doc comment, its
declaration, as much of the body as a four-bytes-a-token budget allows, the cut
counted) for `pixel pack-context`; the brief's definition blocks come from its own
region packs and do not use it.
