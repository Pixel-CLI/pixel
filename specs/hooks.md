# Hooks: what runs inside the agent's turn

Status: normative for `pixel run-hook` and for the command lines `pixel setup`
registers. Structure (which crate does what, the install writers, the per-host
extension points) is in ARCHITECTURE.md, "Agent integration"; this file is the
contract a host and a user can rely on.

## Entry points

| Command | Reads | Prints |
|---|---|---|
| `pixel run-hook task-event --provider <p> --event <e> [--part K/N]` | one host payload on stdin | at most one JSON object on one line |
| `pixel run-hook metrics --provider <p> [--event post-tool-use\|post-invocation]` | a tool-call payload on stdin | at most one JSON object on one line |
| `pixel run-hook edit-event --provider <p> --event pre-edit\|post-edit` | a tool-call payload on stdin | at most one JSON object on one line: a `[PIXEL:EDIT]` card before an edit that changes or removes a declaration other files still call, a `[PIXEL:STALE-CALLERS]` note after it; nothing otherwise |

- `--provider` of `task-event`: `claude`, `codex`, `pi`, `devin`, `gemini`,
  `antigravity` (`TaskProvider`). `metrics` takes `claude`, `codex`, `devin`,
  `pi`, `antigravity`; Gemini has no metrics relay.
- `--event` of `task-event` (`TaskHookEvent`): `session-start`, `prompt-submit`,
  `pre-tool-use`, `post-tool-use`, `tool-failure`, `stop`, `session-end`,
  `interrupt`, `subagent-start`, `subagent-stop`, `model-response`, `user-bash`.
  The same list is what `pixel run-hook task-event --help` prints; a drift test
  holds this file to it.
- `--part K/N` is meaningful on `prompt-submit` only (see "A brief split over
  several hooks"); on any other event it is ignored.
- `pixel run-hook` is a protected command: it prints no live `🟩` metrics line
  on stderr. A delivered brief earns one user-only `systemMessage` instead
  (`metrics.md`).

## What `pixel setup` registers

Every hook command ends in ` || true` (`routing::FAIL_OPEN`). Recognition
(doctor, reinstall, uninstall) strips the tail, so an entry with and without it
is the same hook, and re-running setup rewrites an old-form entry in place.
The tail exists because a host treats exit 2 as a block (a dropped prompt on
Claude Code's `UserPromptSubmit`, a refused `PreToolUse`, a held `Stop`, a
blocked Gemini turn), and an older or newer `pixel` behind the same path exits
2 on a verb or flag it does not know. Pixel's own blocks travel in the JSON it
prints, never in an exit status, so the tail removes no intended block.

| Host | Registered (from `tests/setup/goldens/`) |
|---|---|
| Claude Code, `~/.claude/settings.json` | `task-event` for `session-start`, `pre-tool-use`, `post-tool-use`, `tool-failure`, `stop`, `session-end`, `subagent-start`, `subagent-stop`; `prompt-submit --part K/N` for K in 1..N (N = `brief_parts`, default 3); `metrics --provider claude` on `PostToolUse` matching `Bash`. Timeout 10 s (`HOOK_TIMEOUT`), 3 s for `session-end`. |
| Codex, `$CODEX_HOME/hooks.json` | the same set with `interrupt` instead of `tool-failure`, and `metrics --provider codex`. Codex runs a hook only after `/hooks` review, so a changed command line needs one review. |
| Devin | `session-start`, `pre-tool-use`, `post-tool-use`, `stop`, `session-end`, one `prompt-submit` (no parts). |
| Gemini CLI | one `prompt-submit` on `BeforeAgent`. |
| Antigravity | one `prompt-submit` on `PreInvocation` (first invocation only), `metrics --provider antigravity` on `PostToolUse` and `--event post-invocation` on `PostInvocation`. |
| Pi | no hook registration: the package `extensions/pixel-brief.ts` runs `task-event --provider pi` with `brief_only: true` and `metrics --provider pi`. |
| OpenCode | no hook: `plugins/pixel-brief.js` runs `pixel brief` (`brief.md`). |

With `edit_hooks` on in the global layer (`config.md`, default off), `pixel setup` also registers `edit-event --event pre-edit` on `PreToolUse` and `--event post-edit` on `PostToolUse` for Claude Code, Codex and Devin, one entry each, no matcher, ending in ` || true`; turning it off and re-running setup removes them. The edit hooks read the existing graph only (1 s, nothing built), never block, and send each card once per declaration change per session.

## Guarantees

These hold for every provider and event, and `hook_guarantees_cli` proves them
on hostile, random, oversized and broken-state inputs.

1. **Exit 0.** A hook exits 0 on every failure it handles: malformed, binary,
   oversized (more than `MAX_HOOK_INPUT`, 1,048,576 bytes) or empty payloads, a
   repository never indexed, broken or hostile state under `.pixel/`, no
   daemon, a panic (`hook_input::exit_0_after` catches the unwind). A clap
   parse error of a `run-hook` (or the old `hook`) invocation prints the error
   on stderr and exits 0 (`fails_open` in `main.rs`) instead of clap's 2.
2. **One object or nothing.** Stdout carries exactly one JSON object on one
   line, or nothing. Never a log line, a metrics box or a partial object.
3. **Bounded.** Stdin is read for at most `STDIN_WAIT` (1 s) and an open pipe
   that holds no whole JSON object yields no payload. The decision window is
   8 s (`DECISION_TIMEOUT`), 6 s for Pi (`PI_DECISION_TIMEOUT`), under the 10 s
   the host kills at. One answer deadline is fixed when the payload is read:
   the decision window less `PRINT_RESERVE` (1 s), so 7 s (5 s for Pi). The
   brief's window and its probes end at or before it (`brief.md`).
4. **Late is dropped.** A decision that misses its window is replaced by the
   `unavailable` answer below; a late brief is not waited for.

## Blocking is explicit, and it is not the same as failing

A hook never blocks through its exit status. It blocks only through the JSON it
prints, and only the task gate does:

- `pre-tool-use` may print `permissionDecision: "deny"`; `stop` and
  `subagent-stop` may print `{"decision":"block"}` (a correction) or
  `{"continue":false,"stopReason":…}` (`envelope` in `task_hook.rs`).
- Gates apply when the repository asks for enforcement (`task.enforcement:
  enforce` in `.pixel/config.yaml`, default advisory) or the session is marked
  enforced (`.pixel/tasks/enforced-sessions/`). A missing completion
  requirement triggers at most three automatic corrections, and at most two for
  identical unresolved state.
- When the ledger cannot answer inside the window, only an enforced session
  (marked, or `task.enforcement: enforce`) keeps edits and completion denied;
  otherwise the answer is `{"decision":"observe"}` and the host proceeds.
- **Unreadable input counts as enforced.** A payload that is empty, oversized or
  not JSON, on `pre-tool-use`, `stop` or `subagent-stop`, prints the deny /
  `continue:false` answer whatever `task.enforcement` says, still with exit 0.
  The other events print `{}`. (`unavailable(event, None, true)`; a payload that
  parses but whose ledger times out goes through `enforcement_applies` instead.)
  Observed on this head with `echo garbage | pixel run-hook task-event
  --provider claude --event stop`.

## Prompt: the brief

On `prompt-submit` the hook starts the brief beside the ledger, on its own
thread, so a slow ledger does not use up the brief's window and a slow brief
does not hold the ledger (`brief.md` is the contract of the block itself).

- Briefed hosts: every prompt-hook host. Pi only when the payload says
  `"brief_only": true` (`briefed`); a Pi payload without it gets the ledger
  alone and no brief. Antigravity briefs only `invocationNum == 0`, reading the
  last explicit user turn from `transcriptPath`.
- Output: the block as `additionalContext` of the host's envelope
  (`hookSpecificOutput`, named `UserPromptSubmit`, or `BeforeAgent` for Gemini);
  Antigravity gets `injectSteps[].ephemeralMessage`; Pi gets the bare decision
  `{"decision":"observe","context":<brief>}`. No brief is `{}`.
- A delivered brief adds `systemMessage` `🟩 pixel prompt-submit ❀ <ms>ms`
  when metrics are on (`metrics.md`); an abstention adds nothing.
- Every `prompt-submit`, `post-tool-use`, `tool-failure` and `stop` leaves a
  line in `.pixel/brief-feedback.jsonl` after the host has its answer
  (`metrics.md`).

### A brief split over several hooks

Claude Code and Codex cap what one hook may inject, so `pixel setup` registers
`brief_parts` hooks (1 to 8, default 3) that differ only in `--part K/N`.

- Part 1 computes the brief, stores the whole text under
  `.pixel/brief-parts/<session>-<prompt>.txt` (an empty file for a prompt with
  no brief), prints slice 1, and leaves an empty `.done` marker once its own
  output is written.
- Parts 2 to N poll for the stored file and then the marker, and print their
  slice in the same envelope: `{}` for a brief too short to reach them, nothing
  when the file does not arrive within `decision_timeout` plus slack
  (`follow_bound`). They run no ledger, so one prompt is one ledger event.
- A slice takes whole lines up to `PART_CHARS` (9,800 characters; Codex also
  9,800 bytes) and a line longer than a part is cut at a character boundary.
  The slices in order concatenate to the stored brief; a brief longer than N
  parts loses the lines after the last one.

## Session start and daemon

`session-start` of a briefed host starts the repository's daemon in the
background (one detached `pixel daemon start <root> --warm-meaning`, waited for
at most `HOOK_BOUND`, 100 ms) when the brief is on, an index exists and
auto-start is on (`daemon_auto_start`, `PIXEL_DAEMON_AUTO_START`). A
`prompt-submit` that finds no daemon briefs from the index in process and
starts one for the next prompt. With source `clear` or `compact`, `session-start`
also forgets what the region packs already sent the session
(`.pixel/brief-sessions/`); `resume` keeps it. No hook builds an index or a
graph.

## Tests that prove it

`hook_guarantees_cli` (exit 0, one object or nothing, in time with an open
stdin, panic, broken state, argument errors), `hook_stdin_cap_cli` (the 1 MiB
cap), `brief_parts::tests` and `task_hook` tests (split delivery, late first
part), `setup_goldens_cli` (the registered command lines, tail included),
`install_should_replace_prompt_entries_without_the_fail_open_tail` and
`rerunning_setup_should_replace_every_old_form_entry_in_place` (the tail
migration), and `specs_drift` (the provider and event lists above).
