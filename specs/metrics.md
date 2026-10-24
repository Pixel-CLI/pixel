# Metrics

Status: normative for `pixel-metrics` and `pixel metrics`. Local only; nothing is
sent anywhere. Lifted from `pixel-actionlog`, `operation_metrics.rs`,
`hook_metrics.rs`, `usage_journal.rs`, `execution_brief/{decision_log,feedback}.rs`.

## Records

**`.pixel/actions.jsonl`**: one line per CLI invocation and per daemon request
served. Fields: command, args (credential-bearing flags masked in `logged_args`
before they reach any sink), outcome, error, measured duration, rendered output
bytes and answer (stdout) bytes, the serve route and phase timings, the
versioned workflow estimate (`workflow-v2`: `list-signatures` stands in for
reading the whole file, baseline = file size by `stat`), the `sequential-v1`
time estimate, and `provenance`: `schema_version`, version, build commit and
dirty flag (null when unknown), `source` (`cli`, `hook`, `brief`, `plugin`),
`invocation_id`, and the hashed session. Rotated: when the file passes 5 MiB at
start or every 50 events it is replaced by its last 5,000 lines. Writes go
through a channel to a writer thread; `finish` never blocks the caller beyond a
300 ms flush window; a late line is lost, not waited for.

**`.pixel/journal.jsonl`**: one typed record per hook decision, merged from Pixel's
`brief-decisions.jsonl` and `brief-feedback.jsonl`:

| `t` | Contents |
|---|---|
| `brief` | signal, gate and reason, tier, score, kind, steps that ran with ms (absent when the step's gate stayed closed), brief probes with their outcome and ms, bytes, cut, deadline, route, daemon launch outcome, masked typed text (≤600 chars) and its SHA-256 |
| `part` | bytes each hook part printed, and whether it gave up waiting (`late`) |
| `edit` | the file, the callers named, bytes, or the silence reason (`no_shape_change`, `no_external_callers`, `already_sent`, `no_proposed_text`, `index_stale`, `no_daemon`) |
| `stale` | the file, the sites added to the pending set, bytes, or the silence reason (`no_baseline`, `no_shape_change`, `nothing_new`) |
| `guard` | the Stop check: the sites blocked on, or `empty` / `already_blocked` / `no_daemon` |
| `stop` | whether it blocked, set hash |
| `tool` | tool name, whether it changed a file, repository paths it named |
| `turn` | per prompt: injected, read and edited paths and their overlap, tool calls before the first answer |

Paths and counts only, never prompt, command or output text beyond the masked
typed text. Mode 0600, the last 1,500 lines kept, written without following a
link, never creates `.pixel/`. `PIXEL_LOG=0` writes nothing to either file.

## Estimates

Bytes ÷ 4 approximate tokens and include reporting overhead. `workflow-v2` and
`sequential-v1` (2000 ms per sequential round trip, `native_command_ms = 0`;
`PIXEL_METRICS_ROUND_TRIP_MS` overrides) are workflow estimates, not measured
latency. Each record keeps the version it was written with and is never
recalculated. A missing comparison renders `unavailable: <reason>`, a baseline
that saves nothing renders `no estimated … saving`, zero and negative savings
are kept, and no monetary claim is made. Measured numbers (duration, bytes,
counts) and estimates are always labelled apart.

## Live lines

- **CLI:** after the result on stderr, `🟩 pixel <cmd> ❀ <ms> ❀ #<id>` plus the
  `tokens saved (workflow estimate)` and `saved (sequential estimate)` rows
  (JSON stdout is untouched; on failure the `pixel: <error>` line is repeated
  last).
- **Hooks:** a user-only `systemMessage` per brief, card and block
  (`🟩 pixel brief ❀ 2.1 KB ❀ 412 ms`, `🟩 pixel edit ❀ 3 callers`,
  `🟩 pixel stop ❀ 2 stale sites`); never part of the model's context.
- **Switches:** `--metrics=off`, `PIXEL_METRICS=0` and `pixel config metrics off`
  silence the lines, not the accounting.

## Relays

Where a host hides a command's stderr, the line is replayed through the host's
visible channel after a call that ran `pixel`. A global "latest record" is unsafe
under concurrency and is never used; each relay matches its own invocation.

| Host | Relay |
|---|---|
| Claude Code | PostToolUse `systemMessage` |
| Codex | requires the record's exact identity header (command, duration, short id) as a whole line in this `tool_response`, plus command and cwd match; rejects legacy records, colliding headers and results with several matching footers; silent otherwise |
| Antigravity | `PostToolUse` stores the line per `stepIdx` in the conversation's artifact dir, `PostInvocation` delivers it as an `ephemeralMessage` and clears it |
| pi | `ctx.ui.notify` |
| OpenCode | non-resuming `session.synthetic` message (carries metrics only, never the brief again) |
| Devin | `show_output` and a model-visible "repeat this line once" request |

## `pixel metrics [--since 24h] [--json]`

Replaces Pixel's `token-savings`/`action-log` and `brief-feedback-summary.py`.
Reports, from the two files: briefs delivered and declined by tier and kind;
the zero-call turn rate and the calls before the first answer; precision and
recall of injected vs read paths; edit cards, stale sites fixed before Stop,
Stop blocks; CLI calls with estimated tokens and time saved grouped by
estimator version and coverage. Output is deterministic for a fixed journal.

## Tests that prove it

Rotation at the size and event thresholds; estimator versions retained per
record; `unavailable: <reason>` never dropped; credential flags masked in every
sink (read back); Codex relay silent without a unique identity header;
Antigravity pending file consumed once; `pixel metrics` totals re-derived from a
fixture journal; hooks never create `.pixel/`.
