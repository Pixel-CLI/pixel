---
name: arena
description: Run one of eight standard prompts in two Codex panes side by side in Herdr, one with Pixel's prompt-submit brief and one without, then compare tool calls, answers and wall time. One command runs, one aborts.
---

# arena: Codex with and without Pixel

`arena.sh` (next to this file) runs one case of the catalog below in two
Codex sessions side by side and writes a receipt. It needs a running Herdr
server, `jq`, `git`, and Codex at `/Users/livio/.local/bin/codex` (the
`codex` in an interactive shell is a wrapper function; the script never uses
it). Override with `ARENA_CODEX_BIN`.

```bash
S=.agents/skills/arena/arena.sh
$S list                                   # number, name, prompt
$S run lookup --pixel-bin /path/to/pixel  # or: $S run 1 ...
$S status                                 # arena panes and agent state
$S receipt <run>                          # rebuild a run's receipt after follow-up prompts
$S abort [lookup]                         # interrupt, close the panes
```

`run <n|name> [--pixel-bin PATH] [--sha COMMIT] [--timeout SECONDS] [--focus]`:

1. **Workspace per arm**, keyed by commit and binary hash, under
   `/private/tmp/claude-501/arena/ws/<sha12>-<bin12>/`. Two clones at `--sha`
   (default `origin/main`): `pixel/ws` is indexed with the pinned binary
   (`prepare-repo .`, `build-index --history .`) and its daemon warmed;
   `no-pixel/ws` has no `.pixel/` at all. A run reuses both. The binary is
   copied next to them, so rebuilding the original does not change a run.
2. **A new tab `arena:<case>`** in the focused Herdr workspace (or
   `$HERDR_WORKSPACE_ID`) with two panes titled `arena:<case>:pixel` and
   `arena:<case>:no-pixel`. The user's own panes are never touched; focus
   stays where it was unless `--focus` is given.
3. **Codex in each pane**, one `CODEX_HOME` per arm per run
   (`/private/tmp/claude-501/arena/runs/<case>-<utc>/<arm>/codex-home`). The
   login `auth.json` is copied (never printed); `config.toml` copies the
   user's model, effort, sandbox and approval settings and nothing else (no
   hooks, MCP servers, plugins, notify). The pixel arm's `hooks.json` holds
   one hook, the `UserPromptSubmit` entry `pixel setup` writes for Codex
   (`run-hook task-event --provider codex --event prompt-submit`) at the
   pinned binary; it is a global-home hook, so no repo review is needed (Codex
   still asks for the one-time hook trust in that private home; the script
   answers it). The no-pixel arm has no hook and `PIXEL_BRIEF=0`. `pixel` on
   `PATH` is a shim: the pinned binary in the pixel arm, "command not found"
   in the no-pixel arm.
4. **Preflight without a model**: the hook must return `[PIXEL:BRIEF]` and
   `PIXEL_BRIEF=0` must return nothing, else the run stops.
5. The case prompt goes to both agents at once (`herdr agent prompt --wait`);
   the wait ends when both settle or `--timeout` passes.
6. **Receipt** `runs/<run>/receipt.json`: per arm the brief text found in the
   Codex rollout, the ordered tool calls, the final answer, token counts and
   Codex's own turn time; plus total wall time and an `isolation` block
   (pixel arm saw the brief, no-pixel arm did not). A short side-by-side
   summary is printed.

Both panes stay open for inspection and for follow-up prompts. `abort`
sends Ctrl-C twice to each arena agent, closes only panes titled `arena:*`
(`abort <case>` only that case) and stops the per-home Codex app-server daemons and deletes the copied logins
of those runs; it is idempotent. A run still waiting in another shell ends
with a partial receipt (`prompt_exit` 1, no final answer). `status` lists the arena panes with their state.

Caveats. The first run for a new commit or binary indexes the history
(`build-index --history`), which takes minutes; later runs reuse it. Herdr
can reset a pane label when the shell retitles it, so `status` and `abort`
also match the pane ids recorded in `runs/*/run.json`. The repository's own `AGENTS.md` is loaded by both arms (the same
clone). The copied login shares its refresh token with `~/.codex`; a refresh
during a run can rotate it, so run `codex login` again if the main session
reports a signed-out state. Do not store receipts with credentials: they
hold none, but the raw rollouts under `codex-home/sessions` hold the full
transcripts.

## Prompt catalog

One case per run; `arena.sh` reads this list, so edit a prompt here and
nowhere else. Keep a prompt's contract intact when adapting it.

1. `lookup` — recover an exact identifier, constant, signature, or initializer.
   `What is the maximum rendered byte budget for the prompt-submitted execution brief, and what operation/time limits enforce it?`
2. `impact` — find a definition's production callers and the tests that exercise it.
   `Where is start_brief defined, which production path calls it, and which named tests cover that path?`
3. `flow` — trace a behavior across registration, events, processing, and output.
   `Trace prompt-submit from registration through task-event processing to the host context envelope.`
4. `diagnosis` — explain a failure, fallback, or stale-state behavior from source and tests.
   `Why can a stale graph still leave text evidence available, and what tests prove the fallback behavior?`
5. `configuration` — identify the exact setting, command, event mapping, or doctor rule.
   `Which command and event mapping configure the Codex prompt-submit hook, and what does doctor validate?`
6. `tests` — identify the relevant test cases and the contract assertions they make.
   `Which tests cover Pi exclusion, provider routing, and the execution-brief envelope, and what does each assert?`
7. `history` — recover constraints, intent, or prior behavior from recorded commits.
   `What constraints did the commit that introduced the execution brief record, and which files changed with it?`
8. `architecture` — explain ownership boundaries and source handoffs between modules.
   `Which modules own retrieval, rendering, and host context, and where are the handoffs between them?`
