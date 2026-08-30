# pixel — Unified Deterministic Retrieval + Git Engine (Full Build Plan)

## Context

The user works with LLM coding agents daily and hit the same wall in ~90% of sessions: things that are mechanically retrievable — "the form", a pasted label, a dropped feature in git history, a branch sync — burn seconds-to-minutes of LLM spelunking, and sometimes destroy work (`git checkout` over uncommitted changes). The doctrine for pixel:

> **Regardless of what is asked, if it can be retrieved deterministically with stated certainty and no LLM involved, it must be.** AI handles only genuine ambiguity (e.g. real merge conflicts). Every op returns one of three outcomes: a complete deterministic answer, an explicitly-bounded partial answer (`lower_bound: true`, every cap named), or a structured ambiguity report. Tenets: **T1** — no performance/completeness claim without a checked-in benchmark artifact ("illustrative" baselines are doctrine violations); **T2** — every cap (LIMIT/truncation/exclusion/TTL/approximation) surfaces in the envelope as `lower_bound`/`truncated`/a warning (a silent cap is certainty theater — worse than using an LLM); **T3** — an op keeps MANDATORY status only while an A/B benchmark shows it non-inferior to the no-pixel baseline; measured-worse ops demote to ADVISORY until fixed and re-measured.

A live audit of the three existing tools found:
- **gitpixel** (Rust): solid index/graph/targets/rescue mechanics, but rescue/targets only see files that exist at HEAD (deleted code undiscoverable), no `log -S`/`--all`/reflog/stash reading, zero recency/churn/session ranking signals, search has no ranking at all, no string-literal/JSX/route extraction (so "the form" is unresolvable).
- **usable-git** (TS/Bun): 11 safe git ops with proven safety (960 trials, 0 losses), but `search`'s synchronous FTS5 ingest melted down on a large repo (69-minute call recorded; 711MB cumulative diff text; budget unenforced inside batches; poison generated-JSON files not skipped), branch sync takes 3 calls with manual OID transcription, diverged branches out of scope, `review` hides conflicted paths.
- **GitNexus**: not installed anywhere, yet CLAUDE.md and a SessionStart hook mandate its tools — active false context poisoning every session.

Decisions made with the user:
1. **One Rust binary from scratch** (`pixel`) absorbing both tools' features; both old tools eventually deprecated. (rusqlite + rmcp already proven in the gitpixel workspace; rewrite cost explicitly not a factor — mission fit only.)
2. **Resolve scope = code + git + session context**: static concept index (labels, components, routes, forms) + full history + live session signals (recent edits, active errors, current targets).
3. **Rollout includes the rules cleanup**: CLAUDE.md / hooks / agent-config rules rewritten to mandate pixel; GitNexus and codebase-memory blocks deleted.

## Evidence base (verified this session)

### gitpixel (/Users/livio/Documents/gitpixel) — what ports over
- 8-crate workspace (`core`, `cli`, `graph`, `context`, `serve`, `recall`, `bench`, `sniper`); all git access via `Command::new("git")` subprocess (no libgit2), 3 duplicated wrappers.
- 3-layer git-anchored index: base shard pinned to commit OID (blobs read from object store), delta shard base..HEAD, dirty overlay from fs watcher (`core/indexset.rs`); extractor trait `core/gram.rs:41-52`.
- Daemon: Unix socket per repo, NDJSON `Request`/`Response` (`serve/api.rs`, PROTOCOL_VERSION 6), `Corpus` trait, 500ms-debounced watcher, incremental `refresh_file`. New op = 4 touchpoints.
- `targets`: pure fusion core (`serve/targets.rs`) — 5 RRF signals (filename 3.0 / symbol 2.5 / content 1.5 / graph 1.0 / cluster 0.5, K=60), P0/P1/P2 tiering, manifest + guard hook. **No recency/churn/session signals anywhere** (grep-verified).
- `rescue` (`cli/rescue_cmd.rs`, CLI-only): candidates = targets P0 (live files only); `git log --follow` per file; suspect = subject substring; **apply layer is the solved part** (working-tree-only, 3-way `git merge-file`, dirty strategies) — keep it.
- Graph extraction (`graph/extract.rs`) captures declarations/calls/imports only — **no `jsx_text`, no string literals, no routes**; `.json/.yaml/.html/.vue/.svelte` invisible to the graph. Graph schema = one const DDL string (`store.rs:554-631`) + additive `migrate()`.
- `search` returns raw path/line order — no ranking (`core/indexset.rs:407-507`).
- `sniper` = live error sink with stdio MCP (errors_since/error_show) → the natural session-error signal source. `recall` = transcript embeddings (machine-wide daemon precedent).

### usable-git (/Users/livio/Documents/usable-git) — what ports over
- 11 ops (inspect, review, history, diff, publish, push, ship, branch, sync, update, search) via MCP + CLI through one `service.ts` dispatch; snapshot tokens (12-hex), repository lock + operation journal on mutations; crash-matrix tests; 960-trial benchmark: 0 fsck failures, 0 lost work.
- `update` = ff-only with expectedHead+targetOid → `NON_FAST_FORWARD` + merge base on divergence; `sync` = explicit-refspec fetch; `push` leased. "Sync my branch" today = inspect → sync → update with 2 OIDs hand-copied by the agent.
- `review` filters conflicted paths to zero items (`operations/review.ts:92-96`) — conflicts invisible to the agent.
- `search` defect (root-caused): synchronous ingest inside every call; 8s budget checked only at `while` loop tops; recursive `git show` batch-halving unbounded on `outputLimitExceeded`; poison files (19–26MB generated JSON under `src/generated/`, 16 commits) not matched by the path-based skip list; FTS5 `prefix='2 3'` amplification; query side unbudgeted (`tokenLifecycle` shells `git grep` over full HEAD tree). Ledger evidence: one call `durationMs: 4158032` (69 min), 78 subprocesses.
- New-op registration touches 13 sites (contracts enum, per-op schema, results maps, telemetry, dispatch, MCP, CLI, gain baselines (exhaustive Record — compile-enforced), doctor, journal union, benchmarks, impl, tests).

## Design — Part A: Architecture & unification

### A0. Doctrine
Every operation returns exactly one of three things (three-outcome contract):
1. A **complete deterministic answer**, served from pre-built state — daemon service-time sub-millisecond where possible.
2. An **explicitly-bounded partial answer** — `lower_bound: true`, with every cap that produced the bound named (LIMIT, truncation, exclusion, TTL, approximation). A partial answer that does not announce itself is a bug, not an answer (tenet T2).
3. A **structured ambiguity report** — the minimal machine-readable payload an LLM needs (conflict hunks with base/ours/theirs, ranked ties, missing-index notice). Never prose, never "go investigate".

Tenets: T1 — no performance/completeness claim without a checked-in benchmark artifact; T2 — every cap surfaces in the envelope; T3 — MANDATORY status is held only by ops measured non-inferior to baseline in an A/B benchmark, and lost (demoted to ADVISORY) when a measurement goes the other way.

Corollaries: all indexes are caches (rebuildable, never migrated); all mutations are journaled and provably crash-safe; the daemon is an accelerator, never a correctness dependency (every read op has an in-process fallback with identical answers).

### A1. Workspace / crate layout — seeded by COPY, not greenfield
New repo `~/Documents/pixel`, one distributed binary `~/.local/bin/pixel`. **The workspace is bootstrapped by copying the gitpixel workspace wholesale and renaming crates in place** — gitpixel's ~23k lines of proven Rust (shards, graph, daemon, fusion core, tree-sitter setup, tests, benches) are the starting tree, not a reference to retype. Copy map:

| copied from gitpixel | becomes | then |
|---|---|---|
| `crates/gitpixel-core` | `crates/pixel-index` | rename symbols/paths, `.gitpixel/`→`.pixel/` |
| `crates/gitpixel-graph` | `crates/pixel-graph` | + concepts pass (Engine 1) |
| `crates/gitpixel-serve` | split: `pixel-rank` (targets.rs fusion core) + `pixel-daemon` (daemon.rs, api.rs) | api.rs Request/Response types migrate into `pixel-proto` |
| `crates/gitpixel-cli` | `crates/pixel` (bin) | rescue_cmd.rs kept; CLI re-derived from proto over time |
| `crates/gitpixel-recall` | `crates/pixel-recall` | behind `recall` feature |
| `crates/gitpixel-sniper` | `crates/pixel-session` | store/parsers/mcp kept; grows session journal + gain ledger |
| `crates/gitpixel-bench` | `crates/pixel-bench` | parity + latency gates |
| root `Cargo.toml`, `rust-toolchain`, CI, `.gitignore` | copied | edition/profile/dep versions inherited as-is |

New crates written fresh (no Rust source exists to copy): `pixel-proto` (contracts — seeded from api.rs's enums + usable-git's `contracts/v1/*.ts` schemas as the spec), `pixel-git` (unifies the 3 copied wrappers), `pixel-facts`, `pixel-ops` (Rust port of usable-git semantics — the TS source and its tests are copied into `reference/usable-git/` in the repo as the porting spec, along with `specs/usable-git-v1/PRODUCT.md` + `TECH.md`).

The plan document itself is saved as `~/Documents/pixel/PLAN.md` (first commit).

Crate roles:

```
crates/
  pixel-proto/     # THE contract crate: Op enum, request/response structs, envelope, error
                   # codes, snapshot-token type, PROTOCOL_VERSION (serde + schemars). CLI args,
                   # MCP tool schemas, and daemon dispatch all DERIVE from these types — kills
                   # both gitpixel's 4-touchpoint and usable-git's 13-touchpoint op registration.
  pixel-git/       # ONE git subprocess wrapper (replaces gitpixel's 3 duplicates + usable-git's
                   # runner.ts). Typed plumbing, fingerprinting, repo discovery. Subprocess git,
                   # no libgit2 — the safety proof depends on real git semantics.
  pixel-index/     # port of gitpixel-core: trigram shards, delta, dirty overlay, extractor trait.
  pixel-graph/     # port of gitpixel-graph: tree-sitter symbols/edges/processes/clusters, SQLite.
  pixel-facts/     # NEW fact store (.pixel/facts.db): commit/path/diff FTS with lifecycle facets,
                   # recency/churn stats, concept/alias tables (Engine 1/2 data).
  pixel-rank/      # pure fusion core (port of serve/targets.rs): signal REGISTRY + weighted RRF
                   # (K=60), P0/P1/P2 tiering; slots for recency/churn/session signals.
  pixel-ops/       # port of usable-git semantics: snapshot store, repo lock, operation journal,
                   # publish recovery, the 11 ops + rescue/excavate/reconcile.
  pixel-session/   # sniper successor: error sink, session context, gain ledger telemetry.
  pixel-daemon/    # socket server, dispatch, fs watcher (500ms debounce), corpus lifecycle,
                   # background ingest scheduler with byte budgets.
  pixel-recall/    # [feature = "recall"] transcript embeddings port. Machine-daemon only.
  pixel/           # bin crate: CLI (clap), MCP server (rmcp stdio), install/doctor/hook/migrate,
                   # daemon auto-spawn, in-process fallback.
```

State: per-repo `<repo>/.pixel/` (gitignored, pure cache — sockets live outside); machine `~/.local/state/pixel/` (gain ledger, snapshot store, recall corpus, error sink, `sock/<xxh3(root)>.sock`).

### A2. Unified op surface
Flat verbs `pixel <verb>`. Housekeeping nested (`pixel daemon …`, `pixel hook …`, `pixel install|doctor|migrate`). *(The original design also exposed the ops as one MCP server named `pixel`; that was dropped — pixel is deliberately CLI + hooks only, so no transport can bypass the guard hook.)*

- **retrieve** (0ms path): `search` (ONE verb, `scope: code|history|concepts|all`; code = ranked trigram regex — fixes gitpixel's unranked output; history = facts FTS with first-seen/last-changed/removed-in; concepts = alias index), `resolve` (Engine 1), `symbol`, `uses`, `impact`, `trace`, `context`, `inspect`, `review` (**shows conflicted paths as structured conflict hunks** — reverses usable-git's filtering), `diff`, `history`.
- **scope**: `targets` (manifest at `.pixel/targets.json`), `changes`, `clusters`, `processes`, `graph`.
- **history-deep**: `excavate` (Engine 2 discovery: pickaxe/--all/reflog/stash), `rescue` (candidates from targets **or excavate** — removes the exists-at-HEAD limit; keeps the gated 3-way apply).
- **mutate** (snapshot-token gated, locked, journaled): `publish`, `push`, `ship`, `branch`, `update`, `sync`, `reconcile` (Engine 4), `rescue --apply`.
- **session/meta**: `errors` (sniper's errors_since/error_show unified), `status`, `doctor`, `install`, `daemon`, `hook`, `migrate`.

**Envelope v2** (extends usable-git v1): `{ok, op, protocol, requestId, snapshot: {token, head, branch, dirty}, epistemics: {closed_world, lower_bound, basis, staleness_ms}, budget: {byteCap, used, truncated, cursor}, result, warnings}`. Failure carries `error.{code,message,details,ambiguity?}` — `ambiguity` is the first-class LLM handoff payload. Error codes = usable-git's 18 verbatim + `INDEX_BUILDING`, `AMBIGUOUS`, `NOT_INDEXED`. Snapshot tokens keep the 12-hex scheme; every read returns one, every mutation requires `expectedSnapshot` (STALE_STATE on mismatch).

### A3. Daemons
**Two daemons, one NDJSON protocol** (version handshake on connect):
- **Repo daemon** (per repo root, auto-spawn, idle shutdown): trigram index with the 3-layer git-anchored freshness kept exactly as gitpixel, graph store, facts db + background ingest, targets state.
- **Machine daemon** (singleton): recall corpus, session error sink, gain ledger aggregation, repo registry.

**Mutations do NOT go through the daemon** — they execute in the client process under file lock + journal, preserving the crash-safety model that survived 960 trials; a daemon crash can never corrupt a mutation. Daemon watches `.git/HEAD` + refs as well as the worktree, so external git use can't cause silent staleness.

Latency: daemon service <1ms for retrieve; CLI end-to-end is spawn-dominated — micro-measurement showed a bare process-spawn floor of roughly 17ms for the ~45MB binary (see `crates/pixel-bench/benches/m1_latency.rs` comments), so no sub-5ms CLI claim is made; no daemon → in-process fallback, daemon spawned for next time.

**Search-scalability fix by construction** (kills the 69-minute-call class): (a) ingest is background + incremental in the daemon, never on the query path — queries return instantly with `lower_bound=true` + progress; (b) a `ByteMeter` threaded into writers, so budgets can't be "checked only at loop tops"; (c) content-based poison detection (size cap, max line length, entropy, generated-file heuristics) with recorded skip facts; (d) FTS explicit prefix config + query-side row/byte budgets.

## Design — Part B: Retrieval engines

*(Data ownership reconciliation with Part A: concepts live in `graph.db` next to symbols (extracted in the same tree-sitter pass); the pixel-facts crate owns `history.db` + trigram history segments; pixel-session owns `session.db` and the error sink. "facts.db" from the Part A sketch = history.db.)*

### Engine 1 — Concept index + `resolve` ("the form" → code, ~0ms, offline)

**Extraction** — a second "concept pass" alongside symbol extraction in `extract_file`, producing `RawConcept` rows with a closed kind enum:

| kind | source |
|---|---|
| `ui_text` | JSX/TSX text nodes; markup inner text in html/svelte/vue |
| `attr_text` | `placeholder,label,aria-label,title,alt,name,data-testid` attr strings |
| `string` | string literals + static template parts ≥3 words or ≥12 chars; ALL args to `Error()/throw/toast/alert/console.error` |
| `component` | uppercase JSX element names (usage side) |
| `form` | `<form>`/`<Form>` elements, `useForm/useFormik/createForm` calls, zod schemas in form files |
| `route` | file-path-derived (Next.js `app/**/route.ts` + method exports, `pages/api/**`, SvelteKit) + call-derived (`app.get('/x')`, `fetch('/api/…')`) — one row per HTTP method |
| `status` | integer literals 100–599 in status positions (`res.status(N)`, `{status:N}`, `StatusCode::`, `abort(N)`) |
| `config_key` | JSON/YAML dotted key paths + string leaf values (covers i18n locale files) |

Graph lang gate untouched; concepts get their own gate adding `.svelte/.vue/.html/.json/.yaml/.css`. Svelte/Vue: `<script>` blocks through the existing TS walker with line offset; markup through a small hand-rolled scanner (heuristic v1, upgradeable to tree-sitter-html). Normalization: lowercase, NFC, collapse whitespace, trim punctuation; skip norms >200 chars, files >1MB.

**Storage** — in `graph.db` via the additive `migrate()` precedent: `concepts(id, file_id, kind, raw, norm, detail, start_line, end_line, owner_symbol_id)` with `idx_concepts_norm`/`(kind,norm)`/`file`, plus inverted `concept_words(word, concept_id) WITHOUT ROWID`. **No FTS5** — exact-norm is a point lookup, word AND-intersection handles partial phrases, fuzzier falls to the trigram index. A `concepts_version` meta key forces one-time rebuild on extractor change.

**Resolution cascade** (`resolve "<phrase>"`), each tier short-circuiting, confidence explicit:
- **T0 exact-unique**: `WHERE norm = ?` — one row → `confidence:"resolved"` (the copy-pasted-label case is literally one index probe); 2–15 rows → ranked.
- **T1 kind-directed**: strip article, map head noun (`form→form`, `button|label|toast→ui/attr_text`, `endpoint|route|api→route`, `component|modal|page→component`, `[45]\d\d→status`, `error→{string:throw,status}`), match remaining tokens in `concept_words` restricted to kind + symbol names. Bare "the form" with one form concept resolves unique here.
- **T2 word intersection** all kinds (AND, degrade to OR).
- **T3 trigram fallback** (verified matches, low confidence).
- Miss → `confidence:"unresolved"` with tiers attempted (honest signal that real search/LLM is warranted).

Ranked candidates use Engine 3's shared reranker. Response: matches with path/span/kind/owner/score/reasons + token-budgeted snippet (reuses gitpixel-context), `index_state`, `inputs_digest`.

**Prompt hook (yes, narrowly)**: `UserPromptSubmit` hook → `pixel resolve --hook` extracts only *quoted* spans, injects `additionalContext` **only for resolved-unique hits** (≤300 tokens, one line per hit), <30ms warm with 200ms hard timeout, silent no-op on daemon-down. Ranked/unresolved inject nothing — a wrong auto-injection is worse than none.

Freshness: concept extraction runs inside the existing watcher `update_file`/`remove_file` path, same transaction as symbol refresh (concepts-only refresh path for non-graph files). Budget: resolve p50 <5ms warm; concept pass ≈ +10–20% graph build time.

### Engine 2 — History-wide discovery (`excavate` / history search / rescue v2)

**Architecture**: dedicated low-priority ingest thread in the repo daemon owns `.pixel/history.db` + trigram history segments. Queries never build; every response carries `index_state` (phase, commits_indexed/total, diff_indexed_pct, fresh). Ingest ticks ≤250ms of git wall-clock then yield; checkpointed per batch (`ingest_jobs` cursor), resumable; ref moves enqueue incremental ticks.

**Phases + poison rules** (structural fixes to the usable-git failure):
- **Phase A — refs+metadata first, always completes**: `for-each-ref` heads/remotes/tags + `refs/stash` + stash reflog + reflog-only commits (`rev-list --reflog --not --branches --remotes --tags`). Stash/reflog-only are **first-class** via a `commits.reach` bitmask (branch|remote|tag|stash|reflog_only) — "code that exists nowhere reachable" is a filter, not a special case. Keep usable-git's sound NUL parser.
- **Phase B — path changes + blob sizes**: `--name-status` (from A) + `git cat-file --batch-check` on changed OIDs — sizes known **before** any diff is requested.
- **Phase C — diff text with skips decided before spawning git**: (1) skipped paths passed as pathspec magic `:(exclude)…` so poison blobs are never emitted by git; (2) skip rules, each recorded in a `skips`/`skip_note` fact, never silent: usable-git's path list + `generated|__generated__|codegen` segments + big SVGs; **blob cap 512KB either side** (free, from Phase B); content heuristics on first 4KB (NUL→binary, mean line >400 or single line >2000 → minified, >30% non-ASCII json/js → generated); **learned poison** — any path that trips the cap joins a `poison_paths` table and the exclude list forever (the 16-commit poison file costs one lesson); (3) **bounded batching, no recursion**: fixed 25-commit batches, 8MB cap; overflow → remaining commits go `pending_single`, processed one-at-a-time with own caps → per-commit `skipped:over-cap` at worst; (4) keep usable-git's 32KB/file + 256KB/commit caps.

**Diff index: trigram segments, not FTS5.** Diff added/removed text uses gitpixel-core trigram shards with the recall rowid-in-path trick (segment "paths" carry `hunks` rowids): fixed overhead per byte (no `prefix='2 3'` amplification), regex/substring semantics (what pickaxe needs), append-only immutable segments (eviction never rewrites; stale rowids die at SQL fetch). FTS5 kept only for `messages_fts` (unicode61, **no prefix index**) over commit messages. Schema: `refs`, `commits(oid,parents,author,committed_at,message,reach,diff_state,skip_note)`, `file_changes(commit_id,path,status,old_path)`, `hunks(commit_id,path,added,removed,truncated)`, `poison_paths`, `ingest_jobs`.

**Query surface**:
- `search --scope history {query, facet: message|path|diff|all}` — trigram candidates verified against `hunks`; ranking = occurrence count then recency (usable-git's post-bm25 design kept). Budgeted: 200 candidates/scope, response byte caps.
- `lifecycle {path?|token?}` — first-seen/last-changed/removed-in/present-at-HEAD from `file_changes`/verified hunks.
- **Pickaxe hybrid**: indexed diff search answers in ~0ms when coverage includes the region; when partial/evicted AND zero indexed hits, one optional budgeted live probe `git log -S<term> --all -n 30` with 2s timeout, tagged `provenance:"live-pickaxe"`, returned alongside (never blocking) the instant indexed answer.

**Rescue v2**: phrase → Engine-1 resolve (live tree) AND history diff/message search → candidates as (commit, path, hunk span) **including deleted files** (`status='D'` rows carry removed text) → last-good = newest commit where path exists with phrase present → plan with `source: "<oid>:<path>"` restorable even when path ∉ HEAD → **existing gated apply unchanged** plus `--from <oid>:<oldpath> --to <path>` for deleted/renamed files (all safety invariants keep: worktree-only, dirty gate, --merge/--stash-first/--allow-dirty). Suspect detection upgraded from subject-substring to **diff-overlap** (hunks removed/added text intersecting phrase tokens or target spans); the ~11-subprocess-per-file `log --follow` fan-out becomes one indexed query per path (`old_path` chains supply follow semantics).

**Size/eviction**: metadata ≈1.5KB/commit kept forever; realistic diff residue after skips 30–80MB on the audit repo; hard budget 150MB (configurable); evict oldest by `committed_at` (`diff_state=3`, metadata retained, findable via message/path + live probe). **Never evict** commits that are the `removed-in` for a path deleted from HEAD — they're the rescue payload.

### Engine 3 — Ranking signals (activity + session)

**Rerankers, not candidate channels** — activity/session never generate candidates and never tier-promote (protects the closed-world claim):

```
final = rrf_score * (1 + 0.15*activity_norm + 0.35*session_norm) * test_penalty
test_penalty = 0.7 (1.0 if the query mentions test/spec)
```

Tier assignment runs on unmodified RRF families; rerank reorders **within** tiers. This fixes the audit's `.test.ts`-above-helper case without letting recency promote junk into P0.

- **Activity** (deterministic, cheap): per file `Σ exp(-age_days/14)` over commits from `history.db.file_changes` (fallback: one `git log --since=90.days --name-only` before history.db exists), +1.0 flat for dirty-overlay files, watcher mtime as tiebreak. Cached in-daemon, invalidated on HEAD move/watcher events; normalized over the candidate set.
- **Session**: `.pixel/session.db` `session_events(ts, session_id, kind: read|edit|resolve_hit|targets_hit|error, path, detail)`. Fed by (a) Claude Code PostToolUse hooks on Read/Edit/Write → `pixel journal <kind> <path>` (fire-and-forget, 200ms timeout); (b) pixel journaling its own resolve/targets hits; (c) **the sniper error sink joined live at query time** — an active error whose status/message/frames match a candidate's concepts adds weight + a reason string (the "live 503 boosts matching endpoints" path). Score: `Σ exp(-age_minutes/30)` over events ≤24h, edits 2× reads. New conversations inherit the repo's recent working set by default (yesterday's form is still "the form").
- **Integration**: `SignalInputs` gains `activity`/`session`/`session_reasons` maps — fusion core stays pure; one shared `rerank()` helper used by both `targets` (within-tier) and `resolve` (candidate ordering).
- **Determinism**: same repo state + session journal + error sink ⇒ byte-identical output. Every ranked response carries `inputs_digest = xxh3(head_oid ‖ dirty_set ‖ session_high_water ‖ error_sink_high_water ‖ weights_version)` and human-readable `reasons` ("recent churn", "edited 12m ago", "matches live error #42"). Hook coverage is best-effort: empty session map degrades to lexical+activity (today's behavior plus churn).

### Engine 4 — Deterministic `reconcile` (one-call branch sync)

One op under the ported lock+journal; **no OID transcription** — the snapshot inside the lock supplies all expectations, making the STALE_STATE class structurally impossible within the op. Flow: snapshot (branch, HEAD, upstream, sequencer markers, dirty paths) → explicit-refspec fetch (sync's port, idempotent, outside journal transitions) → classify via `rev-list --left-right --count HEAD...origin/X`:

| counts | state | action |
|---|---|---|
| 0,0 | `up_to_date` | none |
| 0,behind | `fast_forwarded` | update's full guard suite (sequencer, incoming∩dirty refusal) then internal ff under journal |
| ahead,0 | `ahead` | if `push:"auto"` (default): leased push with `--force-with-lease=<branch>:<this call's fetched oid>` — the one-call design makes the lease airtight |
| ahead,behind | `diverged` | report (default) or `rebase-if-clean` (opt-in) |

**Divergence policy**: default `strategy:"report"`; `rebase-if-clean` ships as explicit opt-in. Zero-textual-conflict rebase is deterministic work, not ambiguity — pixel's spec deliberately and narrowly supersedes the v1 "no rebase" non-goal. Never merge commits under any setting. Mechanics: clean worktree required; prove cleanliness per replayed commit with in-memory `git merge-tree --write-tree` (git ≥2.38; older git → feature unavailable, stated in envelope); only then non-interactive `git rebase` under journal transitions; any surprise conflict → `rebase --abort` + diverged report; backup ref `refs/pixel/reconcile-backup/<branch>` written first and reported; successful rebase continues to the push leg. (Engine 2's first-class reflog indexing means "restore what a rebase ate" also works.)

**Conflict report** (fixes review's blind spot — pixel must never repeat the `!conflicted` filtering): `{state:"diverged", merge_base, ahead, behind, clean_rebase_possible, conflicts:[{path, base_span, ours:{oid,hunk}, theirs:{oid,hunk}, conflict_kind}], non_conflicting:{ours_only_paths, theirs_only_paths}, next, backup_ref}` — hunks byte-capped 32KB/path, 256KB/report with truncated flags. The general `review` op likewise gains a `conflicted` item kind.

Lease race: remote moves between fetch and push → single re-fetch + reclassify, then terminal report — never a second blind retry.

### Engine build order (within milestones)
Engine 3's activity/session plumbing first (smallest; unlocks the shared reranker both other engines consume) → Engine 1 (needs reranker) → Engine 2 (feeds better activity data + rescue v2) → Engine 4 (independent; can proceed in parallel with the git-ops port).

## Milestones

- **M0 — Seed by copy + contract skeleton.** Create `~/Documents/pixel`: copy the gitpixel workspace per the copy map, rename crates/paths/state dirs (`.gitpixel/`→`.pixel/`), copy usable-git TS sources + tests + specs into `reference/`, commit `PLAN.md`, get `cargo build && cargo test` green on the renamed tree (this alone carries over all of gitpixel's existing tests). Then add pixel-proto (envelope, errors, tokens, Op enum), pixel-git (unify the copied wrappers), CLI/MCP scaffolds derived from proto. Gate: copied test suite green + contract tests (Rust ports of usable-git's result-contract tests), golden envelope snapshots frozen.
- **M1 — Read core.** Re-wire the copied pixel-index/pixel-graph/pixel-rank behind proto; `targets`; `search --scope code` ranked. Gate: golden parity vs old gitpixel binary (identical hit sets/graph edges/target tiers; order may differ deliberately), shard proptests, criterion latency gates.
- **M2 — Semantic git ops.** Read ops (inspect/review/history/diff, conflicts surfaced), then mutations with snapshot store + lock + journal + publish recovery. Gate: **crash matrix ported to Rust** (kill points at every journal step, `git fsck` per trial, zero-lost-work) + full safety benchmark re-run before usable-git may be retired.
- **M3 — Facts & history search.** Background ingest, `search --scope history` + lifecycle. Gate: adversarial replay of the pathological profile (4.4k commits, 711MB diff text, multi-MB generated JSON) — first response <100ms partial, ingest completes in background, poison skipped with recorded facts.
- **M4 — Engines.** In dependency order: Engine 3 signals + shared reranker → Engine 1 concepts + `resolve` (+ prompt hook) → Engine 2 `excavate`/history search/rescue v2 → Engine 4 `reconcile` (parallel-safe with the git-ops port). Gate: rescue golden tests on rewritten-history fixtures; reconcile classification matrix (up_to_date/ff/ahead/diverged × dirty/clean) + interrupted-journal resume + lease race + merge-tree-clean-but-rebase-conflicts abort path; audit golden cases ("the form", 503 ranking, dropped-svelte).
- **M5 — Session + machine daemon + install.** pixel-session (`errors`), gain ledger, `pixel install`, doctor, recall feature. Gate: install/doctor/telemetry tests, sniper MCP parity.
- **M6 — Rollout & deprecation.** See below; parity oracles retired here.

## Rollout / migration (clean cut, no shims)

1. `pixel install` (idempotent): installs the binary; registers NO MCP server (pixel is deliberately not an MCP server — CLI + hooks only) and **removes** stale usable-git/gitpixel/sniper MCP entries; replaces `~/.claude/hooks/gitpixel-targets-guard` with `exec pixel hook guard "$@"`; installs SessionStart hook → `pixel hook session-start`, which **emits its capability block from the binary's actual op registry** (structurally prevents "CLAUDE.md mandates nonexistent tools"); rewrites agent-config with managed markers (`<!-- pixel:managed:begin/end -->`), deleting stale GitNexus blocks (usable-git CLAUDE.md, AGENTS.md, `.claude/skills/gitnexus/*` references) and scrubbing settings.json entries pointing at the old guard.
2. State: **no index migration** — `pixel migrate` deletes `.gitpixel/` and rebuilds `.pixel/` fresh; only the gain ledger jsonl is carried over (appended with a source tag).
3. Deprecation gates: gitpixel retired after M1 parity + a week of daily use; usable-git only after M2's crash matrix + safety benchmark pass in Rust; sniper and recall at M5.

## Risks

1. **Safety regression in the mutation port** — tests-before-port (crash matrix precedes each mutation op), identical subprocess-git semantics, benchmark re-run as the hard retirement gate.
2. **Search scalability recurring** — fixed by construction (ByteMeter in writers, background-only ingest, poison detection, adversarial fixture in CI forever).
3. **Surface drift across daemon/CLI/MCP/hooks** — pixel-proto single source, all surfaces derive, frozen contract snapshots, socket version handshake.
4. **Daemon lifecycle poisoning freshness trust** — daemon never load-bearing for correctness; epistemics reports basis + staleness; watcher covers `.git` refs.
5. **Binary bloat/build fragility** — reuse gitpixel's proven grammar/dep versions; recall behind a cargo feature so the core never depends on ort/ONNX.

## Verification (end-to-end, per the audit's own scenarios)

Golden acceptance cases — each must pass live on real repos before the corresponding old tool is retired:

1. **"the form" (scenario 2b)**: fresh conversation on a real Next.js repo (ship-fast) → `pixel resolve "the form"` returns the primary form component as `resolved` or top-`ranked`, p50 <5ms warm, zero LLM involvement. Ambiguity fixture: two forms → `ranked` with both + reasons.
2. **Pasted-label (2a)**: unique string → one T0 index probe, `resolved`, <5ms.
3. **"I'm getting a 503" (2c)**: three routes with `status:503` concepts, error sink seeded with a live 503 on `/api/orders` → orders route ranked first with the error-sink reason attached.
4. **Dropped-svelte (scenario 1)**: fixture with "feat: svelte app shell" then "replace Svelte with HTML" → `search --scope history "svelte"` returns the replacement commit first; `lifecycle` reports `removedIn`; rescue plan recommends the pre-removal commit; apply restores the deleted `.svelte` file to the worktree with uncommitted work intact (dirty-gate + merge strategies). Also: stash-only and reflog-only recovery fixtures.
5. **Poison-repo ingest (search defect)**: replay the ship-fast profile (4.4k commits, 711MB cumulative diff, 25MB generated JSON touched by 16 commits) → first query response <100ms partial with honest `index_state`; ingest completes in background; poison path excluded after one blob-check with a recorded skip fact; **no query ever blocks on ingest** (regression fixture stays in CI forever).
6. **One-call sync (scenario 3)**: fixture remotes in all four states → single `reconcile` call lands the right terminal state with no OID transcription; diverged returns the full conflict report (conflicted paths PRESENT — regression test against usable-git review's filtering); `rebase-if-clean` opt-in proves via merge-tree, backs up, rebases, pushes leased; interrupted at every journal step → resume leaves repo consistent, `git fsck` clean.
7. **Safety (the non-negotiable)**: crash matrix ported to Rust passes; full 960-trial-style benchmark re-run: 0 fsck failures, 0 lost unrelated work — hard gate for retiring usable-git.
8. **Parity**: hit-set/graph-edge/target-tier parity harness vs old gitpixel binary (M1); read-op golden parity vs usable-git MCP (M2).
9. **Install/rollout**: `pixel install` on this machine → NO pixel MCP server registered (CLI + hooks only), old usable-git/gitpixel/sniper MCP servers removed, guard + SessionStart hooks emit from the binary's live op registry, GitNexus/codebase-memory blocks gone from CLAUDE.md/AGENTS.md/settings; `pixel doctor` green; a fresh Claude session sees no references to nonexistent tools.
10. **Latency**: criterion gates — daemon retrieve ops <1ms service time; CLI end-to-end bounded by the measured ~17ms process-spawn floor (`crates/pixel-bench/benches/m1_latency.rs`), so the CLI gate is spawn-floor + service-time, not a sub-5ms target; resolve hook <30ms warm.

## Key reference files for implementation

- `/Users/livio/Documents/gitpixel/crates/gitpixel-serve/src/api.rs` — daemon protocol/dispatch pattern to port into pixel-proto/pixel-daemon
- `/Users/livio/Documents/gitpixel/crates/gitpixel-serve/src/targets.rs` — pure fusion core / RRF weights → pixel-rank
- `/Users/livio/Documents/gitpixel/crates/gitpixel-cli/src/rescue_cmd.rs` — gated apply layer to keep
- `/Users/livio/Documents/usable-git/packages/usable-git/src/contracts/v1.ts` — envelope + error codes seeding pixel-proto
- `/Users/livio/Documents/usable-git/packages/usable-git/src/mutations/{operation-journal,snapshot-store,repository-lock}.ts` — the crash-safety model to reproduce exactly
- `/Users/livio/Documents/usable-git/packages/usable-git/tests/mutation-crash-matrix.test.ts` — test design to port as the M2 gate
- `/Users/livio/Documents/usable-git/packages/usable-git/src/search/{ingest,query}.ts` — the defect class the facts ingest must fix by construction
