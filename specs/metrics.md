# Metrics

Status: normative for the local usage records (`pixel-actionlog`, the brief's
decision and feedback logs), the live `🟩` lines, and `pixel metrics`. Local
only: nothing is sent anywhere, and no monetary claim is made. The account of
how the pieces are wired is ARCHITECTURE.md, "Invocation accounting and chat
delivery"; the on-disk files and their retention are in `state-and-threats.md`.

## Three records, three owners

| File under `.pixel/` | Written by | One line per |
|---|---|---|
| `actions.jsonl` | every `pixel` invocation that has a repository root, except `brief`, `search-like-rg` and `run-hook` (a hook leaves its feedback line instead) | CLI invocation, and the daemon requests it served |
| `brief-decisions.jsonl` | `pixel brief` and the prompt-submit hook | prompt the brief judged or declined (`brief.md`) |
| `brief-feedback.jsonl` | `pixel run-hook task-event` | `prompt`, `part`, `tool`, `stop`, `turn` event |

All three carry `provenance`: `schema_version` (1), `pixel_version`,
`build_commit`, `build_dirty` (null when unknown), `source` (`cli`, `hook`,
`brief`, `plugin`) and `invocation_id`; hook records also name the provider and
the session as the first 16 hex digits of its SHA-256, never the host's id. A
record without provenance stays readable and is never attributed to the current
binary.

**`actions.jsonl`** keys: `ts_ms`, `pid`, `command`, `args` (through
`logged_args`: credentials masked, free text not), `cwd`, `outcome` (`ok` or
`error`), `error`, `duration_ms`, `invocation_id`, `serve` (one step per request
with `route`, `reason`, `probe_ms`, `open_ms`, `handle_ms`), and `metrics`:
`duration_us`, `output_scope`, `output_bytes`, `answer_bytes`, `reporting_bytes`,
`estimator_version`, `native_workflow_bytes`, `evidence`, `comparison_gap`.
A writer checks the size when it starts and every 50 events (`ROTATE_CHECK_EVERY`);
past 5 MiB (`MAX_LOG_BYTES`) the file becomes its last 5,000 lines
(`MAX_KEPT_LINES`). Lines go through a writer thread; `finish` waits at most 300
ms (`SHUTDOWN_FLUSH_TIMEOUT`) for the drain, so a late line is lost, not waited
for. There is no switch that stops this log other than having no repository
root.

**`brief-feedback.jsonl`** event kinds: `prompt` (bytes, gate, kind, the
repository paths injected, best first, at most 32, and the SHA-256 of the typed
text that joins it to the decision), `part` (bytes each hook part printed, and
`late` when it gave up waiting), `tool` (tool name, whether it changed a file,
the repository files it named), `stop`, and `turn`, which closes a prompt:
`briefed`, `injected`, `read`, `edited`, `overlap`, `calls` (tool calls before
the first answer). Paths and counts only, never prompt, command or output text;
a path outside the repository, behind a link, or credential-shaped is never
named. The last 1,500 lines are kept (`feedback::MAX_LINES`); the decision log
keeps 500 (`decision_log::MAX_LINES`).

Both brief logs are written with mode 0600, never through a link, and never
create `.pixel/`. `PIXEL_BRIEF_LOG=0|false|off` writes neither of them nor the
central store. Writing is best effort: a failure never reaches the host.

## Estimates

Bytes divided by 4 approximate tokens and include reporting overhead.
`workflow-v2` (`ESTIMATOR_VERSION`) and `sequential-v1`
(`TIME_ESTIMATOR_VERSION`: `max(steps - 1, 0) × round_trip_ms - measured
duration`, 2,000 ms per round trip and `native_command_ms = 0` by default,
`PIXEL_METRICS_ROUND_TRIP_MS` overrides with an unsigned integer) are
**workflow estimates, not measured latency.** Each record keeps the version it
was written with and is never recalculated. A comparison that cannot be made
renders `unavailable: <reason>` and records `comparison_gap` (for example
`operation_failed`); a baseline that saves nothing renders `no estimated …
saving`; zero and negative savings are kept. Measured numbers (duration, bytes,
counts) and estimates are always labelled apart.

## Live lines

- **CLI:** after the result, on stderr, `🟩 pixel <command> ❀ <ms> ❀ #<id>` and
  the `tokens saved (workflow estimate)` and `saved (sequential estimate)` rows
  (`⏱`/`§` rows). JSON stdout is untouched; on failure the `pixel: <error>` line
  is repeated last.
- **Hooks:** a delivered brief adds a user-only `systemMessage`
  `🟩 pixel prompt-submit ❀ <ms>ms` (only the first part of a split brief, and
  only when a brief was actually delivered); it is never part of the model's
  context. Devin and Antigravity, which draw neither, also get a request to
  repeat the line once.
- **Switches:** `--metrics off` (global option), `PIXEL_METRICS=0`, and `pixel
  config metrics off` (repository, or `--global`) silence the lines and not the
  accounting. `pixel run-hook`, `search-like-rg`, the statusline and the
  hook-launched daemon start are protected and never print the line.

### Relays

Where a host hides a command's stderr, `pixel run-hook metrics` replays the
finalized line through the host's visible channel after a call that ran
`pixel`. A global "latest record" is unsafe under concurrency and is never used;
each relay matches its own invocation.

| Provider | Relay |
|---|---|
| `claude` | `PostToolUse` (matcher `Bash`) `systemMessage` |
| `codex` | `systemMessage` only when the record's exact identity header (command, duration, short id) is a whole line of this `tool_response` and command and cwd match; legacy records without an id, colliding headers and several matching footers are rejected; silent otherwise |
| `antigravity` | `PostToolUse` stores the line per `stepIdx` in the conversation's artifact directory and returns `{}`; `--event post-invocation` delivers it once as an `ephemeralMessage` and clears it |
| `pi` | `systemMessage`, shown by the package to the user only |
| `devin` | accepted by `run-hook metrics`, but `pixel setup` registers no metrics hook for Devin (`hooks.md`); its prompt-submit line asks the model to repeat it once |
| OpenCode | the plugin posts the brief's stderr block and each `pixel` call's footer as `session.synthetic` messages with `resume: false` |

## `pixel metrics [PATH] [--since 30m|24h|7d|all] [--json]`

Read-only: it creates no file, runs no daemon and **estimates nothing.** It
counts the retained lines of `brief-decisions.jsonl` and
`brief-feedback.jsonl` stamped inside the window (default 24 h; each log is read
up to 16 MiB; a line that is not JSON adds none).

- `briefs`: `delivered` (decision lines with `bytes` > 0) and `delivered_bytes`;
  `declined` (lines with `bytes` 0) grouped by `gate`. A line whose gate is
  `open` and whose bytes are 0 is a prompt that was admitted and had no file to
  name (`brief.md`); it is counted here, under `open`, not hidden.
- `turns`: over `turn` lines, split by `briefed`: counts, `zero_call` and
  `zero_call_rate`, `mean_calls`, and the same three for turns without a brief.
- `files`: `injected`, `read_or_edited`, `injected_and_read`, `precision`
  (overlap over injected) and `recall` (overlap over touched).
- A rate is a count over a count, `null` (`n/a` in text) when there is nothing to
  divide by.
- `--json` adds `epistemics` (`closed_world: false`; `lower_bound: true` and
  `confidence: "capped"` when either log holds exactly its line cap, because the
  window may then have lost lines) and `snapshot` (`since_ms`, the log sizes, the
  lines inside the window). Text mode prints the cap warning.
- A window other than `Nm`, `Nh`, `Nd` or `all` (or a zero count) exits 1 with
  `metrics: --since takes 30m, 24h, 7d or all, not "<value>"`. With neither log
  present it prints zeroes and says `PIXEL_BRIEF_LOG=0 switches them off`, exit
  0. The output is deterministic for fixed logs.

## Tests that prove it

`brief_metrics_cli` (counts, the JSON envelope, no logs and no writes, the text
rows, a bad window), `brief_feedback_cli` and the `feedback` tests (the `turn`
line, paths only, caps), `metrics_cli`, the rotation and estimator tests in
`pixel-actionlog`, `logged_args` tests (credential masking read back from the
log), `hook_metrics` tests (Codex silent without a unique header, Antigravity
pending file consumed once).
