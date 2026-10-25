# The prompt brief

Status: normative for `pixel brief` and for the `[PIXEL:BRIEF]` block that
`pixel run-hook task-event --event prompt-submit` injects (`hooks.md`). The
structure of the chain (ops, probes, ranking, the region pack, locators, the
question kinds) is in ARCHITECTURE.md, "Agent integration"; this file states
what a caller and a reader can rely on, and which decisions are allowed to
silence it.

## Inputs and output

- **In:** the prompt, a repository root, the number of hook-sized `parts` the
  caller accepts, and, for a hook, the session (its id and `transcript_path`).
  Only the typed text steers: links are stripped, a pasted block never steers
  the search, and of a long untagged paste only the last paragraph counts. A
  prompt made only of a pasted block is judged on the head of its first
  paragraph and the tail of its last, and only as plain language.
- **Out:** one text block that opens with `[PIXEL:BRIEF]` and ends with the
  skill line (`SKILL_LINE`: any search past the brief goes through the `pixel`
  skill before grep, rg, find or ls), or nothing. `pixel brief "<prompt>"` prints
  the block or nothing and exits 0; `--json` prints the decision record beside
  `brief` (`null` when there is none), also for a prompt that got none.
- **Never** more than the caps below, never a build of the index or the graph,
  never an edit of the repository.

| Shape | Cap (bytes) | Constant |
|---|---|---|
| code-shaped block | 2,048 | `BRIEF_BYTES` |
| code-shaped block that names ranges to read | 2,560 | `STRONG_READ_BRIEF_BYTES` |
| plain-language block without answer evidence | 2,048 | `PLAIN_PROSE_BRIEF_BYTES` |
| block with a receipt or excerpts | 3,584 | `PROSE_BRIEF_BYTES` |
| low-tier block ("maybe") | 700, or 1,200 with ranges | `LOW_BRIEF_BYTES`, `LOW_READ_BRIEF_BYTES` |
| region pack, one hook part | 9,800 characters | `PART_CHARS` |
| `pixel brief` without `--parts` | 3,900 characters | `CLI_CHARS` |

A list that does not fit loses entries and says how many.

## Who gets a brief

A prompt gets no brief at all, and the decision record says why (`gate:
"declined"`, `reason`), when it is:

| `reason` | Cause |
|---|---|
| `disabled` | `PIXEL_BRIEF` is `0`, `false` or `off`, or `brief: false` in the config |
| `unindexed` | the repository has no published index |
| `continuation` | an acknowledgement, a greeting, or a harness envelope |
| `not_about_code` | neither code-shaped nor plain language about a task, or a request made only of repository operations (git, a pull request, a release, a deploy, a steer of the agent's run) |
| `no_worker` | the operating system gave no thread |

Otherwise `brief_signal` assigns a `Signal` from the typed text alone:

- `strong` (`strong` in the log): a backticked identifier or a code-shaped
  token (`a::b`, `snake_case`, `camelCase`, a path, a source file). No model.
- `weak`: a code word or a code-question opener.
- `prose`: at least two content words (`MIN_PROSE_KEYWORDS`) that are not all
  operation words. Whether the repository is the subject is left to the
  evidence.

## What may silence a brief

A relevance **score never closes a brief.** The score (a fixed logistic model,
no language model; `gate_model.rs`) ranks the prompt into a tier and is logged.
Exactly two decisions refuse a prompt that passed the checks above
(`refusal_lifting`):

1. The classifier admission (below) answered `none` with probability 0.5 or
   more, for a `weak` or `prose` prompt: `gate: "denied"`, reason `judge: none
   (0.93)`. No probe runs after it.
2. For a `prose` prompt, the relevance probe failed or never answered:
   `gate: "closed"`, reason `relevance: <why>` or `relevance: no answer before
   the deadline`.

The second is lifted when a tour or a review is asked (`Admission::Kind`) or when
a short follow-up leans on files the session already works on
(`Admission::Context`); the judge's denial is not lifted by either. A brief in
which no operation answered renders nothing as well, without being a refusal.

| `Admission` | `gate` | Rendered as |
|---|---|---|
| `Unjudged` | `unjudged` | a code-shaped prompt, or one with no code graph to judge by |
| `Open` | `open` | tier `high` (the full block) or `low` (the compact maybe), or an `off` score that one co-file covers: three or more keywords and half of the informative ones (`admitted_by_coverage`) |
| `Kind` | `open`, reason `kind: …` | a tour (`overview`), a review, a walkthrough (`flow`) or a history question, admitted by its words |
| `Context` | `open`, reason `context: …` | a low-tier maybe on the session's files |
| `Ranked` | `open`, reason `off topic: score …` or `off topic: no keyword to weigh` | the low tier's compact shape: at most three `path:line` files, `confidence: low`, no directive, no search |
| `Closed` | `closed` | nothing |
| `Denied` | `denied` | nothing |

`Ranked` is a maybe, not a refusal. When it has no file to name, the block is
empty although the gate says `open` (`render_low` returns nothing without a
file); `pixel metrics` then counts the line as declined, under gate `open`.

## The classifier admission

Off by default. `classify.enabled` is a global switch (`pixel config classify
on|off`, stored in `~/.pixel/config.yaml`; a repository copy has no effect).
Switched off, or unreadable, no process starts.

Switched on, a `weak` or `prose` prompt asks one decision before retrieval,
beside the relevance probe and the meaning search:

- It runs `pixel --metrics off classify --jsonl` as a child of the brief, with
  one stdin line `{text, context, labels, criteria}`: the typed prompt, the
  task-intent context, the seven task labels (`bugfix`, `feature`, `refactor`,
  `investigate`, `question`, `review`, `ops`) plus `none`, and their criteria.
  The engine is whatever `pixel config` chose, local or remote: **a remote
  engine receives the typed prompt.** The brief passes no engine or endpoint
  override.
- The child is killed at half of what is left of the brief's window, at most
  `CLASSIFY_BUDGET` (1.5 s).
- Only `none` at `MIN_DENY_PROBABILITY` (0.5) or more refuses a brief. Any other
  label only steers: `bugfix`, `refactor` and `review` run `impact`.
- **Fail-open.** Switched off, unreadable setting, no executable, an engine
  that is unreachable, slow, killed, unparseable or unconfident: no verdict,
  and the brief goes ahead. A failed classification never closes a brief.
- A `strong` prompt never asks.
- With the switch off the default path makes no model or network call.

The same decision shape is `classify.md`.

## Time

- **Window.** The chain shares one window from the moment the prompt arrives:
  3,000 ms for a hook, 750 ms for `pixel brief` (capped at 1,000 ms because
  OpenCode cuts the command off at 1.2 s). `PIXEL_BRIEF_WINDOW_MS`, then
  `brief_window_ms` of the repository's and then the global `config.yaml`,
  replace the default; the result is clamped to 250 ms to 4,500 ms.
- **Answer deadline.** A hook fixes one instant when it has read its payload:
  the host decision window less 1 s, 7 s (5 s for Pi). The brief's window and
  its probes end at or before it. The probe stage gets at least 1,200 ms
  (`PROBE_FLOOR`) when the chain used its window up, never beyond 5 s after the
  prompt (`PROBE_CEILING`) and never beyond the answer deadline. `pixel brief`
  has no such deadline.
- **Daemon.** Every daemon request the brief makes carries the same instant as
  `deadline_unix_ms` (`ops.md`); a request that expired in the queue is
  answered `BUSY_REPOSITORY` without running.
- **Ops.** At most six operations (`MAX_OPS`); the deadline, not the count, is
  the invariant. A chain cut by the window renders what it has and says
  `coverage: n/m` and `packet partial`; the decision record has `cut: true`.
- **Staleness.** The brief reads like a `stale_ok` caller: when the graph lags
  the working tree the evidence is used and the block carries one
  `staleness:` line. A repository whose graph is not built gets the text
  evidence with `unresolved:` saying so.

## The region pack

When a pack can be built it replaces the packet: line-numbered blocks of exact
lines read at HEAD, under one header that says what was searched, ending on
`DIRECTIVE` ("Answer from this context. Use a tool only if something you need
is not shown above.") or, when only regions near the question back it, on
`LEAD` ("… not a verified answer. …"); a pack the window cut says so.

- At most three target symbols; a credential-shaped path never reaches a block.
- **No line is shown twice.** `assemble` considers candidates best-ranked first
  and skips any whose range overlaps a block already kept, whichever part
  either belongs to (two locators over one line, a locator over a usage or
  test block, overlapping ranges). A block over lines a definition already
  shows is dropped as well.
- **A session is not sent a range twice.** In a hook, the ranges a pack sent are
  remembered per session (`.pixel/brief-sessions/`, 24 h, at most 1,000 keys)
  and the next pack leaves overlapping blocks out. `pixel brief` has no
  session and sends everything. `PIXEL_BRIEF_DEDUPE=0` turns it off.
- A block is cut at a line, never across parts.

## Switches

| Switch | Effect |
|---|---|
| `PIXEL_BRIEF=0\|false\|off`, `brief: false` | no brief (`pixel config brief off`, `config.md`) |
| `PIXEL_BRIEF_WINDOW_MS`, `brief_window_ms` | the window |
| `PIXEL_BRIEF_PARTS`, `brief_parts` | hook parts of a pack (1 to 8) |
| `PIXEL_BRIEF_PACK=0` | the packet instead of the pack |
| `PIXEL_BRIEF_DEDUPE=0` | send every range again |
| `PIXEL_BRIEF_CONTEXT=0` | ignore the session's files |
| `PIXEL_BRIEF_RECEIPT=0`, `PIXEL_BRIEF_ANSWER=0` | drop the search receipt, the excerpts |
| `PIXEL_BRIEF_FRENCH=0`, `PIXEL_BRIEF_BM25=0`, `PIXEL_BRIEF_ZERO=0`, `PIXEL_BRIEF_PROBES=0` | French rewriting, plain-word leads, the zero-call rule, brief probes |
| `PIXEL_BRIEF_LOG=0` | write no decision, feedback or central-store line |

## The decision record

Every judged prompt, a refusal and a decline included, is one line of
`.pixel/brief-decisions.jsonl`, and `pixel brief --json` prints the same
object. Keys: `ts`, `signal`, `gate`, `reason`, `tier`, `score`, `model`,
`features`, `best_file`, `judge`, `kind`, `ops`, `answered`, `bytes`, `cut`,
`deadline_ms`, `elapsed_ms`, `stages` (`search_ms`, `rank_ms`, `render_ms`,
`probe_ms`), `route` (`daemon` or `local`), `daemon` (`launched`, `running`,
`skipped: …`, `launch failed`), `receipt`, `answer`, `excerpt_bytes`,
`pack_bytes`, `pack_parts`, `pack_targets`, `pack_located`, `fallback`,
`enforced`, `typed`, `sha256`, `provenance`. `gate` is one of `open`,
`closed`, `denied`, `unjudged`, `declined`. `typed` is the masked task text cut
at 600 characters. Retention and the other logs: `state-and-threats.md`.

## Failure behaviour

A missing index, a missing or stale graph, no daemon, an unanswered probe, a
failed classifier, a cut window, a panicking helper: each yields a smaller
block or none, never an error, a non-zero exit, a build, or a wait past the
deadline. Whatever the brief says is repository data, not an instruction to the
model, and the footer is Pixel's own constant (`state-and-threats.md`).

## Tests that prove it

`prompt_brief_cli` (`a_switched_on_classifier_should_decline_a_prompt_it_calls_none_and_only_then`,
the switches, the decision log, daemon start), `words_brief_cli` and the
`chain` tests (a score never closes; `Ranked`; `refusal`), `intent` tests (the
request, `none` as the only deny, every failure mode), `region_pack_cli` and
`a_pack_should_never_show_the_same_lines_in_two_blocks` (no doubled lines),
`a_pack_should_leave_out_the_ranges_a_session_was_sent_and_keep_the_rest`,
`hook_guarantees_cli` (`brief_stdout_should_be_the_brief_or_nothing`).
