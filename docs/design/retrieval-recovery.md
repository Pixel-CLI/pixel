# Typed retrieval recovery hints — producer/consumer matrix and first slice

Status: partial (issue #625). The producer/consumer inventory and the shared
typed recovery-hint types in `pixel-proto` are implemented. The recovery flow
(daemon derivation, CLI rendering, integration tests) remains pending. This
note is the inventory the slice was chosen from: every retrieval command, every
way its answer can be less than the caller wanted, the authoritative producer
of that signal, and every consumer that reads it. The two gaps the first slice
closes are named at the end; everything else is recorded so the next slice
does not re-derive it.

## How to read this matrix

Each cell names **where the signal is produced** (the only place that may
author it) and **who consumes it**. A consumer that re-states the signal in
its own words is a second source of truth unless it renders the producer's
typed value; the rule this slice enforces is that a cause has one producer
and every other surface renders it.

The seven conditions are the ones an agent must tell apart before its next
call: a malformed request, a complete empty answer, a missing or unavailable
index, a stale snapshot, unsupported coverage, an ambiguous symbol, and the
row/byte/depth/time caps.

## The matrix

| Condition | `search-content` (op `search`) | `find-symbol` (op `symbol`) | `find-code` (op `resolve`) | `pack-context` (op `context`) | `evaluate` |
| --- | --- | --- | --- | --- | --- |
| **Invalid input** | clap exit 2 before the daemon (bad `-g`/`-t`, `--fallback-query` without `-F`); a malformed regex is the daemon's `INVALID_INPUT` failure envelope, exit 1. Producer: CLI arg parser + daemon classifier. Consumers: failure envelope on stdout (`--json`), `pixel: …` on stderr. | clap exit 2 (missing `name`). Producer: CLI. Consumers: stderr. | clap exit 2 (missing `phrase`). Producer: CLI. Consumers: stderr. | clap exit 2 (missing `uid`); a budget below the minimum response is a daemon error. Producer: CLI + daemon. Consumers: stderr. | `ErrorEnvelope` exit 2 (`invalid_argument`, `unsupported_predicate`) with the offending `argument`; the wire refuses a usage error masquerading as `unknown`. Producer: `evaluate.rs`. Consumers: stdout JSON, action log. |
| **Valid empty result** | exit 0, empty stdout, stderr `0 matches under {cwd}; repo root {root}` (text only; suppressed for `--json` and `--offset > 0`). Producer: CLI `no_match_note`. Consumers: stderr; the savings box reads empty stdout as "no answer". | exit 0, stdout `no symbols found`. Producer: CLI `finish_graph_cmd` pretty closure. Consumers: stdout. | exit 0, stdout `No matches found.` Producer: CLI `print_resolve_human`. Consumers: stdout. | exit 0, symbol line + empty `incoming:`/`outgoing:`. Producer: CLI pretty closure. Consumers: stdout. | exit 0, `absent_in_snapshot`, `answer: false`, `witness: none`, summary says `traversal exhaustive`. Producer: `evaluate.rs`. Consumers: stdout text + JSON. |
| **Missing / unavailable index** | daemon failure envelope (`no shard`, extractor mismatch). Producer: daemon `op_search` → index layer. Consumers: failure envelope, exit 1. | `ensure_graph` error before any answer. Producer: daemon `ensure_graph`. Consumers: stderr, exit 1. | graph-unavailable branch: `envelope.graph: "unavailable"`, `basis` names `pixel search-content`, semantic fallback may still answer. Producer: daemon `op_resolve`. Consumers: `basis` (epistemics channel), JSON. | `ensure_graph` error. Producer: daemon. Consumers: stderr, exit 1. | `Reason::GraphUnavailable` → `NextAction::BuildGraph { command: "pixel rebuild-graph" }`. Producer: `evaluate.rs`. Consumers: `reason` + `next_actions` JSON, summary `Next:` clause. |
| **Stale snapshot** | n/a — search answers the text index, not the graph; staleness is the index generation, not a git snapshot. | n/a (graph freshness is `ensure_graph`'s job before answering). | n/a. | `stale_response`: `truncated: true` + a `context truncated: source differs from graph snapshot…` cap when the source no longer matches the graph. Producer: daemon `op_context` `validated_context_source`. Consumers: `caps`, `truncated` JSON. | `Reason::GraphStale` → `RebuildGraph`; `Reason::SnapshotChanged` → `RetryOnce` + `EvaluateAtSnapshot`. Producer: `evaluate.rs`. Consumers: `reason` + `next_actions` JSON. |
| **Unsupported coverage** | `--scope` value other than `code`/`hybrid` is a daemon error naming the supported values. Producer: daemon `op_search`. Consumers: stderr, exit 1. | n/a (name lookup has no coverage axis). | `tier: "semantic"` hits are marked `unverified` in `reasons`. Producer: daemon `op_resolve` semantic fallback. Consumers: per-match `reasons`, `confidence`. | n/a. | `Reason::SymbolOutsideIndex { cause }` → `Terminal` for intrinsic causes (file cap, size cap, unsupported language), `RefreshGraph` for `NotYetIndexed`. Producer: `evaluate.rs`. Consumers: `reason` + `next_actions` JSON. |
| **Ambiguous symbol** | n/a. | `candidates` array; text prints `ambiguous name — re-run with one of these uids:` + the uid list. Producer: daemon `op_symbol` (via `symbols_by_name` + envelope). Consumers: stdout text, `candidates` JSON. | `confidence: "ranked"` + ordered `matches`; no hard ambiguity. Producer: daemon `op_resolve`. Consumers: `confidence`, `basis` JSON. | `Resolved::Many` → `candidates_value` (up to 50) under the response budget. Producer: daemon `op_context` `resolve_symbol`. Consumers: `candidates` JSON. | `Reason::AmbiguousSymbol { candidates }` → `NextAction::SelectSymbol`. Producer: `evaluate.rs`. Consumers: `reason` + `next_actions` JSON. |
| **Row / byte / depth / time cap** | `truncated: true` + `next_offset` + named `caps`/`cap_hits` (row limit, byte cap, ranked pool, credential hidden); stderr `⚠ results truncated: … Continue with --offset {next_offset} or pass --limit…`; envelope `RESULT_CAPPED` warnings; `epistemics.basis` names the caps. Producer: daemon `op_search`. Consumers: metadata line (JSON), stderr warning, envelope warnings, epistemics. | n/a. | `scan_capped` when a fallback table scan hits its row cap. Producer: daemon `op_resolve`. Consumers: `scan_capped`, epistemics. | token budget: `budget_tokens`, `budget_basis`, `truncated` when the budget cut the render. Producer: daemon `op_context`. Consumers: JSON fields. | `Reason::TraversalBudgetExhausted { parameter, current }` → `NextAction::RaiseBudget` (2× suggestion, saturating). Producer: `evaluate.rs`. Consumers: `reason` + `next_actions` JSON. |

## Overlap map (who may state what)

- **`Warning`** (`{code, message}`) is the mirror of a cap that fired, on the
  envelope. It states *what bound bit*, never *what to do about it*.
- **`Epistemics`** (`closed_world`/`lower_bound`/`basis`/`confidence`) is the
  honesty channel: *is this answer complete, and why not*. It never proposes
  an action.
- **`evaluate`'s `NextAction`** is the predicate-specific action channel for
  `evaluate` only. Its wire invariant (no `next_actions` on
  `established`/`absent_in_snapshot`) is preserved.
- **The recovery hint** (this slice) is the cross-command action channel:
  *what a valid next call looks like*. It is derived from the same markers as
  `Warning`/`Epistemics` so the three never disagree about the cause, and it
  renders to one minimal text line and one JSON field.

A cause has exactly one producer. `Warning` and `Epistemics` already exist
and keep their meanings; the recovery hint does not replace them — it is the
only channel that proposes a next call, so the ad-hoc "what to do" prose each
command invents today is retired in favour of it.

## The first two concrete gaps

The inventory shows two cells where an agent cannot make a valid next call
without guessing, and where no typed signal exists:

**Gap 1 — `search-content` capped pages: the continuation is prose-only and
drift-unbound.** The capped page says `Continue with --offset {next_offset}`
in a stderr sentence. That continuation does not preserve the original
filters (`--glob`, `--type`, `--scope`, paths), ordering, or snapshot
identity: an agent that re-runs it bare gets a different page, and if the
index was rebuilt between pages the offset can drift into duplicates or
omissions with nothing saying so. The JSON has `next_offset` but no snapshot
binding and no drift condition. This is the highest-friction path because
broad patterns hit the caps constantly.

**Gap 2 — `find-symbol` / `find-code` valid empty results are dead ends.**
`no symbols found` and `No matches found.` are complete answers, but they
carry no typed signal distinguishing them from a malformed request or missing
coverage, and no guidance toward the sibling command that could still answer
(`find-code` for a name `find-symbol` missed, `search-content` for a phrase
`find-code` missed). An agent that guesses the sibling command wastes a call;
an agent that does not guess stops. `find-code`'s graph-unavailable `basis`
already points at `search-content` — the valid-empty case has nothing.

## The slice

A shared typed representation, `RecoveryHint`, in `pixel-proto` (cause,
applicability, structured next-call argv or a continuation token, snapshot
binding, side effect). The daemon derives it at the same choke point as
`derive_epistemics` and attaches it as an additive `recovery` field; the CLI
renders it as one minimal stderr line in text mode and lets it ride the JSON
field in `--json` mode. `evaluate`'s `NextAction` contract is untouched.

- Gap 1: a capped `search` page carries a `RowCap`/`ByteCap` hint whose
  continuation preserves the effective filters, ordering and snapshot, and
  whose `on_drift` is an explicit `Restart` — the answer never promises no
  gaps across an index update.
- Gap 2: a valid-empty `find-symbol`/`find-code` carries an `EmptyResult`
  hint whose next call is the sibling command with the caller's own input.
  A valid-empty `search` carries **no** hint: a complete empty answer is not
  a condition to recover from, so it stays quiet.

The frozen recovery-task set that measures these lives in
`crates/pixel/tests/cli/fixtures/recovery-tasks.json`; the measurement
protocol that scores it (correct recovery, extra tool calls, token cost) is
owned by #626.
