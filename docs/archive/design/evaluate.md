# `pixel evaluate` — bounded predicate evaluation with witnesses (design note, v4)

Status: proposal, not implemented. Supersedes v1 (Jev-style question +
choices + probabilities), v2 (three predicates, fall-through) and v3
(formal rule, witness, snapshot by manifest). v3 went through a
three-reviewer pass on 2026-09-21 whose verdict was: not ready to code
until the snapshot contract stops letting a witness-only check certify
the working tree. v4 fixes that and the other partially-resolved points,
and closes the three open questions. Every "today" claim below was read
in the code of this worktree.

## What Jev is, in pixel terms

Jev (TypeSafe AI) is a runtime classifier: `state + question + schema` in,
a typed answer with a probability per option out, no token generation.
Laya approximates it with a fully fine-tuned ModernBERT-large (~421M) and
a learned decision head, with a per-question-type temperature calibration
it recommends re-fitting on one's own data. Neither validates a cheap
embedding shortcut; neither is reproducible independently.

Pixel takes the **typed interface** (one proposition in, one bounded
conclusion out, machine-readable) and refuses the **probabilistic
promise**. The conclusion is a witness, or a typed reason for not having
one.

## Ground truth: what the implementation does today

| Fact | Where | Consequence |
| --- | --- | --- |
| `call-path` is a BFS, depth cap 8, outgoing `calls` + `has_method` edges only; returns `found: bool`, hops, `furthest_reachable`; nothing says whether the depth cap cut the frontier | `pixel-graph/src/trace.rs`, `api.rs` (`bridge::trace(…, 8)`) | today's `found: false` is not a refutation |
| `impact` clamps depth to 1..3 and records `bucket_truncated` per depth | `impact.rs` | precedent: caps are surfaced, never silent |
| every stored edge carries `src`, `dst`, `kind`, `tier`, `site_line`, `receiver`; `Tier::Exact` = same-file scope or import-resolved, `Tier::Probable` = unique name in the import-connected component | `store.rs` `EdgeRow`, `Tier` | per-edge witness is constructible; a `Probable` edge's premise (uniqueness in a component) lives outside the path |
| each graph file row has `blob_oid`, which is **xxh3 of the content**, not a git oid | `build.rs` (`format!("{:016x}", xxh3_64(&content))`) | rename to `content_hash` in any new output |
| the graph stores a **content-aware freshness signature** in `meta.freshness`: xxh3 over sorted `(path, xxh3(content))` of every supported source file under the walk policy, plus `meta.extractor_version` | `build.rs` `FRESHNESS_KEY`, `EXTRACTOR_VERSION_KEY`, `input_signature`, `freshness_signature` | the snapshot digest the reviewers asked to cache **already exists**, and it already binds extraction rules |
| `build_graph` recomputes the tree signature after extraction and refuses to publish as fresh if it moved | `build.rs` (`source changed during graph build`) | the check-before / check-after pattern exists for full builds |
| `tree_delta(root, db)` does one full parallel walk with content hashes, returns `fresh`, `changed`, `removed`, `indexed_files`, `signature`; `None` if no signature or other extractor version | `build.rs` `TreeDelta` | a **whole-tree** working-tree match check exists; cost = one content-hash walk, bounded by `MAX_FILE_BYTES` (4 MiB) per file |
| the daemon's `ensure_graph` runs `tree_delta` whenever it (re)opens the graph handle; drift under `PIXEL_GRAPH_INCREMENTAL_MAX_PCT` is applied incrementally, above it a full rebuild is published by atomic rename; a `notify` watcher refreshes changed files and drops the handle | `api.rs` `ensure_graph`, `refresh_files`, `daemon.rs` | graph ops already refresh incrementally before answering; "no implicit refresh" must be reconciled with this (see Budget) |
| **gap 1** (fixed in PR #195, pending merge): `apply_tree_delta` and `update_files` wrote `meta.freshness` with a separate `meta_set` after the row updates | `build.rs` | now one `BEGIN IMMEDIATE … COMMIT` around rows and signature (`update_files_in_one_transaction`); `replace_file`/`remove_file`/`replace_concepts` nest as savepoints |
| **gap 2** (fixed in PR #195, pending merge): the watcher path signed with `freshness_signature(root)` computed after the rows were written, without re-checking the parsed bytes | `build.rs` `update_files` | now returns `Publication::{Signed, Withheld}`; the signature is published only when `rows_match_tree` holds after one tree walk; a withheld publication overwrites `meta.freshness` with the sentinel `FRESHNESS_WITHHELD = "withheld"` in the same transaction (a missing key would make `tree_delta` return `None` and force a full rebuild; a non-matching value keeps the incremental repair). This closes the ABA case tree A → rows B → tree A. Batches are reduced to one final action per path (`final_actions`, last occurrence wins). **Consequence**: a repo holding a permanently unparseable source file outside the batch makes every watcher update `Withheld`; freshness is then restored by `ensure_graph`'s `apply_tree_delta` on the next open. Cheap, but the watcher path alone no longer keeps such repos signed; `evaluate` must not assume the watcher keeps `fresh` true |
| symbol uid is `path#qualified#kind`, stable, human-readable | `build.rs` | the retry identifier exists |
| `lower_bound` in `derive_epistemics` ⇔ caps (unresolved same-name sites, capped scan); `closed_world` always false | `api.rs` | resolver uncertainty, not a verdict qualifier |
| `changes::detect` maps `git diff --unified=0` hunks (new-file coordinates) onto symbols; hunks or hunk portions matching no symbol are not reported | `changes.rs` | needs `uncovered_changes` |
| `.pixel/` is git-ignored | `.gitignore` | policy tables cannot live there |
| `--help` ↔ ARCHITECTURE.md drift is a test | `crates/pixel/tests/cli/docs_drift.rs` | a new command lands in the table in the same PR |
| **the mutation gate never covered the graph builder**: `.cargo/mutants.toml` excluded `**/build.rs` for build scripts, which also matched `crates/pixel-graph/src/build.rs`, the 2 240-line source module holding `build_graph`, `tree_delta`, `apply_tree_delta`, `update_files`, `tree_hashes`, `freshness_signature`. Narrowing it to `crates/*/build.rs` takes that file from 0 to **113** listed mutants. cargo-mutants skips real build scripts natively (`crates/pixel/build.rs` lists 0 even with no exclusion), so the glob never protected anything it was written for | `.cargo/mutants.toml`, verified 2026-09-21, fixed in PR #204 | every freshness guarantee `I` rests on shipped ungated. #195's merged diff carries 27 mutants of which only 9 ran (7 `store.rs`, 2 `api.rs`); the **18 in `build.rs` cover the transaction and signing logic** this contract depends on. `--in-diff` means they are not re-tested by an unrelated PR: closing them needs a deliberate one-off run over the file |
| **a green `Mutants in diff` check can mean zero mutants tested**: PR #203's check passed in 56 s having produced no `mutants.out` at all, because its whole diff sat in the excluded file | `.github/workflows/mutants.yml`, verified 2026-09-21, fixed in PR #204 | a vacuous green is worse than the exclusion: it makes an excluded-file PR look gated. #204 adds `scripts/mutants-gate.py`, which classifies a run `tested` / `not-applicable` / `vacuous` and annotates the vacuous case. Any evidence of the form "Mutants was green" in this design note or its PRs must name the tested count |

## Principles

1. **Translate, don't guess.** Explicit predicates only; no free question,
   no `--choices`, no fall-through to another predicate.
2. **Three different things, always named:** facts about the stored
   relation, coverage of that relation, claims about the program.
3. **Scope is inseparable from the verdict**, in the first sentence of the
   terminal output and in the JSON.
4. **`unknown` is an instruction.** Typed reason, typed next actions with
   their real parameters; terminal when nothing can be done.
5. **No probabilities, priors or encoder.**
6. **A contract exists from the first delivery.** "No launch" does not
   suspend compatibility: statuses, fields and defaults follow a
   compatibility policy from v1.0, versioned by `schema_version`.

## Naming

`pixel evaluate <predicate> …`. `decide` promised an action the contract
refuses; `check` reads as certification; `evaluate` says what happens.

## Formal decision rule

Snapshot `s`, graph `G_s = (V_s, E_s)`, selected relation `E_s^d`
(direction, edge kinds, tiers). Predicate-specific sources `S` and goals
`T`.

- **I** (coherence): the graph generation read is the one the freshness
  signature describes, and the working tree matched that signature
  **before and after** the traversal (see Snapshot). Under `--at-snapshot`
  the second half is waived and the output says so.
- **R** (resolution): every symbol argument resolved to exactly one
  existing symbol (a verbatim uid is looked up, not trusted).
- **witness_obtained**: the traversal actually produced a path from some
  `S` to some `T` in `E_s^d` and every edge of it was re-read from the
  store. Existence is not enough; discovery is required.
- **X** (exhaustion): the traversal's visited set contains all of `S`, is
  closed under `E_s^d` (every admissible adjacency of every visited node
  was inspected), and no depth or time budget stopped it with a non-empty
  frontier. A node sitting at the maximum depth does not by itself mean
  the cap cut anything: a leaf, or edges only to visited nodes, leave the
  traversal exhaustive; the flag is set only when a frontier node was
  dropped because of the cap.
- **M** (change coverage, `diff.reaches` only): every changed portion the
  contract promises to cover is anchored on a symbol present in `G_s`
  (`uncovered_changes` is empty and no deleted symbol lacks an anchor).
  `M` is true by definition for `path`.

```
established        ⇔ I ∧ R ∧ witness_obtained
absent_in_snapshot ⇔ I ∧ R ∧ ¬witness_obtained ∧ X ∧ M
unknown            otherwise
```

Stated consequences:

- A witness found before a cap fires is `established`.
- `absent_in_snapshot` is about `E_s^d` only. Files beyond the build cap
  or the 4 MiB size cap, unresolved same-name sites, edges of a tier not
  selected, and extraction blind spots are outside the domain and listed
  in `coverage`.
- A cap that stops the traversal with a non-empty frontier → `unknown`.
- Nothing here is about runtime reachability.

Boolean mapping: `established → true`, `absent_in_snapshot → false`,
`unknown → null`.

### Traversal definitions per predicate

| predicate, traversal | S | T | edges walked |
| --- | --- | --- | --- |
| `path`, `callees` | `{from}` | `{to}` | outgoing |
| `path`, `callers` | `{from}` | `{to}` | incoming (equivalent to `callees` from `to` to `from`; both directions are offered so the CLI reads naturally) |
| `diff-reaches`, `callers` (∃c ∈ C_s : t ⇝ c) | `C_s` (multi-source) | `{t}` | **incoming** from `C_s`, i.e. the closure of the changed set under "who calls me" |
| `diff-reaches`, `callees` (∃c ∈ C_s : c ⇝ t) | `C_s` | `{t}` | outgoing from `C_s` |

Multi-source reverse traversal is the one that must be spelled out: the
visited set is the incoming-closure of `C_s`; `X` requires that closure
to be complete. Walking from `t` on incoming edges would answer a
different proposition.

## Envelope

Top level is a tagged union: `{"kind": "evaluation", …}` or
`{"kind": "error", …}` (see Error protocol).

```json
{
  "kind": "evaluation",
  "schema_version": 1,
  "predicate": "path",
  "status": "established",
  "answer": true,
  "domain": {
    "relation": "indexed_call_graph",
    "edge_kinds": ["calls", "has_method"],
    "traversal": "callees",
    "tiers": ["exact"]
  },
  "snapshot": {
    "signature": "3f9c…",
    "extractor_version": "…",
    "generation_coherent": true,
    "working_tree_check": "full_before_and_after",
    "working_tree_matches": true
  },
  "coverage": {
    "traversal_exhausted": true,
    "depth_cap": 8, "depth_cap_dropped_frontier": false,
    "time_budget_ms": 250, "time_budget_hit": false,
    "graph_file_cap_hit": false,
    "files_excluded_by_size": 0,
    "unresolved_same_name_sites": 0,
    "extraction_limits": ["callbacks passed as arguments", "dynamic dispatch", "macro-generated calls", "eval"]
  },
  "witness": {
    "kind": "path",
    "probable_edges": 0,
    "edges": [
      {
        "from": {"uid": "src/a.rs#UserService::delete#method", "path": "src/a.rs", "lines": [40, 58], "content_hash": "…"},
        "to":   {"uid": "src/b.rs#AccountManager::close#method", "path": "src/b.rs", "lines": [12, 30], "content_hash": "…"},
        "edge": {"kind": "calls", "tier": "exact", "site": {"path": "src/a.rs", "line": 51}, "receiver": "manager"},
        "call_direction": "from→to",
        "traversal_step": 1,
        "premises": []
      }
    ]
  },
  "reason": null,
  "next_actions": [],
  "epistemics": { "…": "derive_epistemics output, unchanged" },
  "summary": "Path found in the indexed call graph, snapshot 3f9c…, relation calls+has_method, tiers exact, traversal callees: 1 edge, 0 probable. This does not establish that the call happens at runtime."
}
```

### Witness variants

- `path`: as above.
- `identity`: for zero-length paths (`path --from t --to t` after `t` is
  verified to exist; `diff-reaches` when `t ∈ C_s`). Carries the symbol,
  its `content_hash`, and for `diff-reaches` the hunk ranges that put it
  in `C_s`. An empty `edges` array is never emitted as a witness.
- `none`: only with `absent_in_snapshot` or `unknown`.

### Two verification levels

- **Graph membership**: every edge and node of the witness can be
  re-read from the store at `snapshot.signature`. Always possible from
  the envelope.
- **Source-level resolution**: an `exact` edge is checkable from the two
  files named (same-file scope or an import statement listed in
  `premises`). A `probable` edge's premise is uniqueness of the name in an
  import-connected component; `premises` for such an edge lists the
  component id, the file count, and the import edges that connect the
  call site's file to the definition's file. Content hashes are xxh3, so
  a verifier needs the working tree (or `--at-snapshot` with the files
  unchanged); uncommitted content cannot be fetched from git. The docs say
  this plainly.

### Summary templates (fixed per predicate × status)

The first sentence is autonomous and carries snapshot, relation, tiers and
traversal. Examples:

- `established`: "Path found in the indexed call graph, snapshot S,
  relation R, tiers T, traversal D: N edges, P probable. This does not
  establish that the call happens at runtime."
- `absent_in_snapshot`: "No path in the indexed call graph, snapshot S,
  relation R, tiers T, traversal D, traversal exhaustive. This says
  nothing about calls outside that relation (other tiers, callbacks,
  dynamic dispatch, macros, files beyond caps) or at runtime."
- `unknown`: "Not evaluated: <reason sentence>. Next: <action>."

## Reason codes

| code | meaning | next action (with real parameters) |
| --- | --- | --- |
| `ambiguous_symbol` | several candidates, none exact-unique | `select_symbol {argument, candidates[{uid,kind,path,line}], accepts: "uid", narrow: "--in <path-prefix>"}` |
| `symbol_not_found` | no candidate at any tier; a verbatim uid that does not exist also lands here | `search_symbol {suggest: ["find-symbol", "find-code"]}` |
| `symbol_outside_index` | file exists on disk but not in the graph, and the cause is known | cause `file_cap` or `size_cap` → `terminal: true`; cause `unsupported_language` → `terminal: true`; cause `not_yet_indexed` → `refresh_graph` |
| `traversal_budget_exhausted` | depth or time cap dropped a frontier node | `raise_budget {parameter: "--max-depth" \| "--time-budget-ms", current, suggested}` |
| `graph_unavailable` | no graph db | `build_graph {command: "pixel rebuild-graph"}` |
| `graph_stale` | drift above the incremental threshold | `rebuild_graph` |
| `snapshot_changed` | working tree diverged before or during evaluation | `retry_once` (the daemon will have refreshed) or `evaluate_at_snapshot {flag: "--at-snapshot"}` |
| `unmapped_changes` | `diff-reaches`: uncovered changed portions | `terminal: true` unless every motif is `not_yet_indexed`; the output lists motifs and ranges |
| `unanchored_changes` | `diff-reaches`: a deleted symbol has no anchor in the current graph | `terminal: true`; two-snapshot analysis is out of scope |

Usage and protocol errors (`unsupported_predicate`, `invalid_argument`,
unknown `--tiers` value) are **not** `unknown` reasons; they are errors
with exit code 2. Every `next_actions` entry is either executable with the
parameters shown or `terminal: true`; an identical retry is never advised
except as `retry_once` after `snapshot_changed`.

## Error protocol

- Exit 0: evaluation completed, any status. **0 means evaluated, not
  true.**
- Exit 2: usage error. Exit 3: technical failure (daemon, IO, db).
- With `--json`, exits 2 and 3 still print one JSON object on stdout:
  `{"kind":"error","code":"invalid_argument","message":"…","argument":"--tiers"}`.
  Human diagnostics go to stderr.

## Resolution of symbol arguments

- A uid is looked up verbatim (no ranking) and must exist.
- A name auto-resolves only on an exact, unique match within the scope
  (`--in <path-prefix>` optional). Method vs free function homonyms are
  never auto-picked.
- `Ranked` results feed `ambiguous_symbol` candidates; the list is capped
  and says so.
- `symbol_not_found` never yields `absent_in_snapshot`.

## Snapshot (the indispensable change)

Three separate claims, three separate fields:

1. **Identity** — `snapshot.signature` = `meta.freshness` (already stored),
   `snapshot.extractor_version` = `meta.extractor_version` (already
   stored). Two builds of identical inputs produce the same signature;
   this is a content digest, not an opaque build id. **Decision C1: use
   the cached signature; do not recompute per call.**
2. **Generation coherence** — the rows read and the signature read belong
   to the same generation. Requires closing gap 1 and gap 2 above:
   `apply_tree_delta` and `update_files` must write rows and
   `meta.freshness` in one SQLite transaction, and the watcher path must
   re-check parsed `content_hash`es before signing, as `apply_tree_delta`
   already does. These two fixes are a **prerequisite PR** to v1.0.
3. **Working-tree match** — `tree_delta(root, db)` before the traversal
   and `freshness_signature(root)` (same walk) after it, compared to
   `snapshot.signature`. Both must match for `working_tree_matches: true`
   with `working_tree_check: "full_before_and_after"`. A mismatch either
   side → `unknown/snapshot_changed`. This is whole-tree, so a negative is
   protected against a file outside the witness having added the path. A
   witness-only check is never performed and the value
   `working_tree_check: "witness_only"` does not exist in the schema.

`--at-snapshot`: skips the after-check and reports
`working_tree_check: "before_only"`, `working_tree_matches` as measured;
the summary says the answer is about the stored snapshot.

**Measured cost of the whole-tree check** (PR #199, `docs/bench/tree-delta.md`,
Apple M2 8 cores, release profile, page cache warm; re-run with
`cargo bench -p pixel-bench --bench tree_delta`):

| tree | `tree_delta` p50 | p95 |
| --- | ---: | ---: |
| this repo, 249 files, idle | 16 ms | 17 ms |
| this repo, loaded machine | 17 ms | 25 ms |
| synthetic 50 000 files, idle | 1.6 s | 8.3 s |
| synthetic 50 000 files, loaded | 7.0 s | 8.6 s |

The db comparison is under 1 ms on this repo and 70–170 ms at 50k; the
walk plus hashing is the whole cost. Root cause, verified in `build.rs`:
`tree_hashes` walks, reads and hashes in one **serial** iterator; its doc
comment claims rayon parallelism, which is false (the build path
parallelises extraction after a serial `collect_files`). A before/after
pair therefore costs 3–14 s on a 50k tree: **the v4 Budget targets are
met on repos this size and missed by one to two orders of magnitude on
large ones.** This changes the plan (see Budget).

## Tiers (decision C2)

- Default `--tiers exact`. `--tiers exact,probable` selects the union.
  Any other value is a usage error. **`--tiers` selects the relation, not
  a confidence threshold; there is no automatic widening.**
- No fourth status. A path with `probable` edges in the widened relation
  is `established`; every edge carries its tier, `witness.probable_edges`
  is the aggregate, and the summary's first sentence names the count.
- Only a probable path exists: `exact` mode with exhaustive traversal →
  `absent_in_snapshot` **in the exact relation**; widened mode with a
  witness → `established`; traversal cut without witness → `unknown`.
- `exact` is never described as "guaranteed at runtime".

## Predicates and delivery

### Delivery 1 — `path`

Progress: PR 1 (`pixel_graph::predicate`, PR #198) implements the
evaluator: multi-source BFS, both traversals, tier selection, frontier-drop
tracking, injectable clock, witness re-read from the store, `identity`
variant, import-row premises for `probable` edges (`available: false` when
the store holds no linking import; the component id is not stored).
Reviewed against the rule above: conforming. Two caveats for PR 3:
`coverage.unresolved_same_name_sites` counts the endpoints' names only, not
every visited node's, and the evaluator returns a `StoreError` for a
missing symbol id (resolution reasons belong upstream, as intended).

PR 2 (`pixel_proto::evaluate`, PR #202, stacked on #198) carries the wire
contract: `Output::{Evaluation, Error}`, `Outcome::{Established{witness},
AbsentInSnapshot, Unknown{reason, next_actions}}` — `Established` cannot be
built without a witness and `AbsentInSnapshot` cannot carry one, so the
illegal tuples are unrepresentable and refused on deserialize
(`IllegalEvaluation`). `default_next_actions` implements the reason table,
`summary` the fixed templates. Two deliberate deviations from the JSON
above, documented in the module header: `premises` is a tagged enum
(`not_required` | `imports` | `unavailable`) so an exact edge, a probable
edge with its import rows, and a probable edge with no justification are
distinct; `Output::Evaluation` boxes the envelope. `WorkingTreeCheck` has
only `full_before_and_after` and `before_only` — no witness-only variant
(forbidden) and no `watcher_signed` until the Budget decision below.

Stack note: at the time, CodeRabbit did not review a pull request whose
base was another feature branch ("reviews are disabled for this base
branch"), so a stacked PR was only reviewed once the branch below merged
and GitHub retargeted it to `main`. #265 set `base_branches: [".*"]`, and a
stacked PR is now reviewed against its base when it opens. #202
was retargeted by hand and merged before that happened, so it never got a
CodeRabbit pass; #203 was merged without a rebase, so its 10 mutants never
ran either. Both are on the p9 list.

PR 3 (`pixel_daemon::evaluate` + `evaluate_cmd`, PR #206) carries the op,
the resolution, the snapshot checks and the CLI. Read against this note:
faithful. The ordering that matters, verified in `op_evaluate_probed`:
gate (whole-tree `tree_delta`) → identity from one handle → resolution →
**test seam** → traversal → after-check (`freshness_signature` over the
whole tree compared to the identity's signature). `envelope` hardcodes
`working_tree_matches: true` because it is only reachable once both checks
passed; the failure path goes through `halted`, which takes the flag.

Four deviations, each deliberate:

1. **`symbol_outside_index` is never produced.** The build records no
   per-file reason for a file's absence, so the cause cannot be verified,
   and this contract reserves that reason for a known cause. Those cases
   answer `symbol_not_found`. To make it real, extraction would have to
   record why it dropped a file.
2. **`TierSelection` lives in `pixel-daemon`, not `pixel-proto`.** The
   contract crate has `Tier` but no selection type; widening #202 was not
   worth it. Move it if a second caller appears.
3. **`coverage.files_excluded_by_size` is 0 meaning "none observed"**, not
   "none exist" — counting them needs a second whole-tree walk, doubling
   the command's cost. `graph_file_cap_hit` is derived conservatively and
   can only over-report, which widens a stated limit rather than narrowing
   it. This is the weakest honesty claim in the implementation; the
   absence summary names "files beyond caps" unconditionally, so no
   published sentence depends on the counter.
4. **Drift is repaired, not refused, below the threshold.** `evaluate_gate`
   applies an incremental update when the drift fits
   `PIXEL_GRAPH_INCREMENTAL_MAX_PCT` (**default 20**) and answers
   `graph_stale` only above it, or when the graph carries no usable
   signature. So a refusal needs more than a fifth of the indexed files to
   have moved — a branch switch or a large pull, not an ordinary edit. On a
   test fixture of four files one edit is enough, which is why the daemon
   tests exercise that path constantly; on a real repository they do not.

```
pixel evaluate path --from <uid|name> --to <uid|name>
  [--traversal callees|callers] [--tiers exact|exact,probable]
  [--max-depth N] [--time-budget-ms N] [--in <path-prefix>] [--at-snapshot] [--json]
```

A close relative of `call-path`, stated as such. It replaces `found:
false` with `absent_in_snapshot` vs `unknown/traversal_budget_exhausted`,
adds the per-edge tier, call site and premises, and the snapshot
contract. Also serves "does any indexed call path cross this boundary": a
witness is a crossing; absence is bounded; the docs call it a call-path
check across a boundary, not an architectural dependency proof.

### Delivery 2 — `diff-reaches` (launch)

```
pixel evaluate diff-reaches --to <uid|name> --traversal callers|callees
  [--base <ref>] [--tiers …] [--max-depth N] [--time-budget-ms N] [--at-snapshot] [--json]
```

`--traversal` is mandatory in the protocol; a CLI shorthand may default
to `callers` but the value is always echoed in `domain`.

Defined cases: `t ∈ C_s` → `identity` witness; `added` symbols are in
`C_s`; `deleted` symbols cannot anchor a path in the current graph → any
negative becomes `unknown/unanchored_changes` (a positive found elsewhere
stays valid); empty diff → `absent_in_snapshot` with `changed_symbols: 0`
and `M` trivially true; `uncovered_changes` non-empty → any negative
becomes `unknown/unmapped_changes`. *Superseded by the 2026-09-26
measurement (C3 below): the narrowed predicate lists uncovered portions in
`coverage` and counts them in the summary instead of downgrading; the
deleted-symbol case stands.*

Every `diff-reaches` summary ends with: "This follows indexed call
relations only; it does not cover constants, types, schemas, imports,
configuration or shared data."

### `uncovered_changes` (decision C3)

Computed in `changes::detect` and added to `ChangesReport` as an additive
field (also improving `what-changed`); the evaluator consumes it. Two
inventories would diverge. Consumers that reject unknown fields are
adapted in the same PR and the contract change is in the changelog.

A changed portion is uncovered when it has no symbolic anchor under the
declared mapping rules, examined on **both sides** of the diff (old and
new coordinates), keeping residues **inside partially mapped hunks**: a
hunk touching a function and an adjacent constant contributes the
constant's lines.

| motif | example |
| --- | --- |
| `outside_symbol` | module constant, import, comment, attribute |
| `unsupported_language` | config, or a language the extractor lacks |
| `excluded_by_file_cap` | eligible file beyond `DEFAULT_GRAPH_MAX_FILES` |
| `excluded_by_size` | file over `MAX_FILE_BYTES` |
| `not_indexed` | other, cause named when known |
| `non_text_change` | binary or mode-only change |

Deleted symbols are reported as `unanchored` (mappable in the old state,
no anchor in the current graph), separately from `uncovered_changes`.

All motifs downgrade a negative while the promise is "the whole change".
Before Delivery 2 the abstention rate per motif is measured on reference
repos; if it makes the command unusable, the predicate is explicitly
narrowed to mapped symbols in its name and summary, never by hiding
omissions.

**Measured 2026-09-26, and the rule applies**
([docs/bench/evaluate-abstention.md](../bench/evaluate-abstention.md)).
Under the whole-change promise, 96 % to 100 % of commits abstain on pixel,
GitNexus and openclaw, and 68 % on a Rails monolith. Even if docs, tests,
comments, imports, attributes and assets stopped downgrading, 56 % to 90 %
still would, on module-level code outside any symbol and on configuration.
Both are real behaviour the call graph cannot see, so no refinement of the
motif list rescues the promise. Delivery 2 therefore ships the narrowed
predicate:

- **Name**: `changed-symbols-reach` (`Predicate::ChangedSymbolsReach`, wire
  tag `changed_symbols_reach`), renamed from `diff-reaches` in the Delivery 2
  PR together with the proto variant and its summary constant. The earlier
  sections keep the old name until then.
- **Claim**: a path exists, or none exists, from or to the changed symbols
  the graph maps (`C_s`), within the stored relation. Nothing is said about
  the rest of the change.
- **Unmapped portions**: `uncovered_changes` no longer downgrades a
  negative. Every portion is listed in `coverage` with its motif, and the
  summary's first sentence counts them ("…; 3 changed portions have no
  symbol and are not covered"). `unmapped_changes` is never returned by this
  predicate; the reason stays reserved for a future whole-change predicate.
- **Deleted symbols**: `unanchored` still turns a negative into
  `unknown/unanchored_changes`. A deleted symbol was part of the change the
  predicate is about, and the current graph cannot anchor it. Measured cost:
  8 % to 42 % of commits.

### Later — `diff-touches`, policy

`diff-touches --paths <glob>` (pure diff) first, then `policy` split into
applicability and resolution, rules in a tracked `pixel-rules.toml`,
content-hashed into the trace, no built-in policy, no heuristics. Not a
launch argument until adoption is measured.

## Budget and refresh

The measurement above forces a decision before PR 3; the contract does not
change, the way `I` is established does. Options, cheapest first:

1. **Parallelise `tree_hashes`** — **done in PR #203**, measured:

   | tree | serial p50 | parallel p50 |
   | --- | ---: | ---: |
   | this repo, 249 files | 9.7 ms | 7.1 ms |
   | synthetic, 50 000 files | 6 934 ms | 924 ms |

   Across runs the parallel version held a 0.9–1.1 s band at 50k while the
   serial one ranged 1.3–6.9 s, so the honest claim is that it removes
   sensitivity to machine load rather than giving a clean 8×. A
   before/after pair at 50k therefore still costs ~2 s, well above the
   300 ms agent target: **necessary, not sufficient.** Instrumentation
   splits the 50k cost into 65–125 ms of directory walk against
   780–1 450 ms of read+hash, which is syscall-bound: `read_source_file`
   issues `lstat`, `open`, `fstat`, `lstat`, `read` per file, four of them
   to close a TOCTOU race. An `O_NOFOLLOW` variant collapsing those was
   measured and deliberately not shipped (it changes symlink semantics and
   belongs in its own PR); even with it, a 50k pair stays far above the
   target.
2. **Stat-gated hashing**: store `(path, size, mtime_ns, content_hash)` per
   file at build time; a check walks and stats, hashes only entries whose
   size or mtime changed, and compares. A stat walk of 50k files is in the
   tens to low hundreds of ms. This is the version-control index strategy.
   It weakens the "detects `touch -t` with same size" property the current
   comment defends; for an agent tool the threat is staleness, not an
   adversary, but the maintainer decides. Requires a schema addition to
   `files`.
3. **Trust the watcher between full checks**: while the daemon's `notify`
   watcher is alive and has applied every event, the signature it
   maintains is the truth and a call needs no walk; a full walk runs when
   the watcher was down, dropped events, or on `--verify`. With #195 the
   watcher path withholds on any drift, so a `Signed` state is
   trustworthy. Cheapest per call, but `I` then depends on daemon
   liveness, which the envelope must report
   (`working_tree_check: "watcher_signed"`).

Proposal: do 1 now (prerequisite, alongside #195/#199), design 2 as its
own PR with the maintainer's call on the `touch -t` trade-off, keep 3 as
the fast path only if 2 is refused. Until 2 or 3 lands, PR 3 ships
`evaluate path` with the full before/after pair and documents that repos
beyond a few thousand files pay seconds per call; the hook placement rule
below already forbids a synchronous hook in that case.

- The evaluator uses the daemon's existing `ensure_graph` path: drift
  under the incremental threshold is applied incrementally (as every
  graph op does today), then the before-check runs. Drift above the
  threshold → `unknown/graph_stale`, no implicit full rebuild. Missing
  graph → `unknown/graph_unavailable`.
- Two budgets reported separately with the phase that exhausted them:
  traversal (`--max-depth`, visited nodes) and wall time
  (`--time-budget-ms`, default 250).
- Targets to measure, not to claim: warm p95 < 100 ms in a hook, < 300 ms
  for an agent call on the reference repo, whole-tree check included. If
  missed, the command is not placed in any synchronous hook by default.
  Measured on openclaw (43 839 graph files, 2026-09-26): `what-changed`,
  which the diff predicate builds on, took about 100 s per call end to end,
  12 to 20 s of it the incremental graph update
  ([evaluate-abstention.md](../bench/evaluate-abstention.md#cost)).

## `call-path` migration

- Delivery 1: bundled agent prompts and skills point to `evaluate path`;
  `call-path` output gains a one-line note naming the successor.
- Delivery 2: `call-path` documented as deprecated; format untouched.
- Kept compatible for at least two minor versions; removed or changed only
  in a major, after an inventory of consumers (`recall` can count them).

## Testing that fails for the right reason

- **Independent oracle**: a transitive-closure implementation in the test
  crate, sharing no helper with production, on small generated graphs
  (exhaustive enumeration); compared for both traversals, both tier
  selections, each edge kind, with cycles, self-path, disconnected pairs,
  homonyms, and multi-source sets.
- **Abstention fixtures**: controlled graphs where the depth cap must drop
  a frontier node, where a witness sits exactly at the cap, and where a
  depth-max node is a leaf (must stay exhaustive); a **simulated clock**
  for the time budget.
- **Table tests** over every legal (status, answer, reason, witness kind)
  tuple; illegal tuples unrepresentable in the types where Rust allows.
- **Witness verification**: each edge walked back to the stored edge and
  its `content_hash`; `identity` witness distinguished from `none`.
- **Snapshot**: a file rewritten between before-check and after-check
  yields `snapshot_changed`; a file outside the witness rewritten yields
  `snapshot_changed` on a negative; generation coherence tested against
  gaps 1 and 2 with an interleaved reader.
- **Diff**: add, delete, rename, partially mapped hunk, unsupported file,
  file beyond cap, moving base.
- **Consumer tests**: bundled agent prompts exercised so no client treats
  `unknown` as `false` or as permission.
- **Mutants**: status flip, direction flip, dropped mandatory reason,
  dropped after-check, each met by a targeted assertion; CI `Mutants` is
  the gate.

## Public promise, forbidden claims, demo

README sentence:

> Pixel searches its indexed call graph for paths and returns the
> witnesses it found, with their limits.

Never claim: "proves a change is safe", "finds all impacts / all
callers", "certifies absence of dependency", "determines the only tests
needed", "deterministic, therefore correct", "zero hallucination".

Demo transcript (Delivery 2, 30 s, every step shown):

1. Frozen base: `pixel status` shows the graph fresh.
2. Edit `PaymentGateway.charge`.
3. `pixel evaluate diff-reaches --to checkout_handler --traversal callers`:
   the daemon's incremental refresh is visible in `graph_build`, then
   `established` with the hunk and two clickable call sites.
4. Same command with `--tiers exact` on a variant where one hop is
   `probable`: `absent_in_snapshot` **in the exact relation**, first
   sentence naming the relation. This is a bounded absence, shown as such,
   not an abstention.
5. Edit a constant next to `charge`: `absent_in_snapshot` for the changed
   symbols the graph maps, with the first sentence counting one changed
   portion without a symbol (motif `outside_symbol`, range listed). This is
   the reserve, stated in the answer. Then delete `charge`:
   `unknown/unanchored_changes`. This is the real refusal, and it changes
   the answer. (Revised after the C3 measurement; before it, step 5 showed
   the constant edit itself as `unknown/unmapped_changes`.)

## Arbitrations

| Disagreement | Decision |
| --- | --- |
| partition repaired? | partially in v3; v4 uses `witness_obtained` and adds `M` |
| snapshot resolved? | not in v3; v4 mandates whole-tree before/after and the two transaction fixes |
| ship `path` as is? | no; the prerequisite PR (gaps 1 and 2) and the snapshot section above come first |
| `refuted` vs `absent_in_snapshot` | `absent_in_snapshot` |
| default tiers | `exact` |
| `uncovered_changes` location | `ChangesReport` |
| `diff-reaches` direction | mandatory in protocol, echoed always |
| "technical release" wording | "Delivery 1 / Delivery 2"; compatibility from Delivery 1 |

## Prerequisite PR (before any `evaluate` code)

Items 1 and 2 are implemented in PR #195
(`fix/graph-freshness-signature-atomic`, 2026-09-21), CI pending at the
time of writing.

1. `apply_tree_delta` and `update_files`: rows and `meta.freshness` in one
   transaction. **Done in #195.**
2. `update_files`: re-check parsed `content_hash`es before signing, as
   `apply_tree_delta` does; on mismatch, leave the signature stale rather
   than publish. **Done in #195** (`Publication::Withheld`).
3. Measure `tree_delta` cost (p50/p95, warm/cold) on the reference repo
   and a 50 000-file tree; publish the numbers in `docs/bench`. **Done in
   PR #199**; targets missed at 50k, see Budget.
3b. Parallelise `tree_hashes` and correct its doc comment (new, from #199's
   finding). Then decide between stat-gated hashing and watcher trust
   before PR 3.
4. Rename `blob_oid` to `content_hash` in new public output only; the
   store column keeps its name.

## Proof checklist before public communication (v4 additions)

1. A negative never certifies the current tree when any indexed file
   changed or was added outside the witness (whole-tree after-check).
2. Incremental update and extractor bump cannot publish an incoherent
   signature (gaps 1 and 2 closed, tested with an interleaved reader).
3. The filesystem coherence point is the pair of full walks, documented
   as such; no atomicity beyond it is claimed.
4. `identity`, `none` and an empty path are three distinct outputs.
5. A `probable` edge's premises are in the witness and checkable beyond
   the path's files.
6. Deletions alone and partially mapped hunks never produce a whole-change
   negative.
7. `--json` on exits 2 and 3 yields one parseable object.
8. Abstention rate of `diff-reaches` per motif, measured, leaves real use.
   **Measured 2026-09-26**: the whole-change promise does not leave real use,
   so the predicate is narrowed to mapped symbols (C3,
   `changed-symbols-reach`).
9. The demo transcript includes the refresh and distinguishes bounded
   absence, reserve, and abstention.
10. The `call-path` migration and the compatibility policy from Delivery 1
    are published.

## Remaining open questions

1. `path` on drift above the incremental threshold: `graph_stale` (as
   specified) or an opt-in `--rebuild` flag that runs the full rebuild
   synchronously? Proposal: `graph_stale` only; rebuilding is a separate,
   visible command.
2. `--time-budget-ms` default 250: measured against the whole-tree check
   cost, or excluded from it? Proposal: the budget covers traversal only;
   the snapshot checks are reported separately in `timings`.
