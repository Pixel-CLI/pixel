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
   (`pixel prepare . --history`) and its daemon warmed;
   `no-pixel/ws` has no `.pixel/` at all. A run reuses both. The binary is
   copied next to them, so rebuilding the original does not change a run.
2. **Two panes split right in the current tab** when run from a Herdr pane (`--here`, the default there), or **a
   new tab `arena:<case>`** in the focused Herdr workspace (or `$HERDR_WORKSPACE_ID`) with `--tab` or outside Herdr; the panes titled `arena:<case>:pixel` and
   `arena:<case>:no-pixel`. The user's own panes are never touched; focus
   stays where it was unless `--focus` is given.
3. **Codex in each pane**, one `CODEX_HOME` per arm per run
   (`/private/tmp/claude-501/arena/runs/<case>-<utc>/<arm>/codex-home`). The
   login `auth.json` is copied (never printed); `config.toml` copies the
   user's model, effort, sandbox and approval settings and nothing else (no
   hooks, MCP servers, plugins, notify). The pixel arm's `hooks.json` holds
   one hook, the `UserPromptSubmit` entry `pixel setup` writes for Codex
   (`pixel hook prompt-submit --host codex`) at the
   pinned binary; it is a global-home hook, so no repo review is needed. The
   pixel arm launches `codex --dangerously-bypass-hook-trust` so the one-time
   trust review can never block the prompt (the arena vets and writes the hook
   itself); `review_hooks_if_asked` still patrols as a fallback. The no-pixel
   arm has no hook and `PIXEL_BRIEF=0`. `pixel` on
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

## Standing rules (the user's, applied by default; do not ask or re-derive)

- **Standard invocation**, from a Herdr pane, in the foreground so the user watches the panes start:
  `S=.agents/skills/arena/arena.sh; $S run-set catalog --agent claude --pixel-bin "$PWD/target/debug/pixel"`
  (build first from this checkout with `cargo build -p pixel`).
- **Split right in the current tab** is the default inside Herdr (`HERDR_PANE_ID` set); `--tab` opens an arena tab instead.
- **The panes open first**, before any indexing or cloning, so the split is visible at once; each shows what it waits for.
- **Never in the pixel repository.** Panes start in the arena scratch directory, and each agent is launched inside its
  own clone under `runs/<case>-<utc>/<arm>/ws`. The user's checkout is never an agent's working directory.
- **Run once, do not restart.** A kill leaves indexers running; `abort` now stops them and clears the locks. Edit the
  script only when no set is running (bash reads a running script incrementally).
- **Consecutive cases of one repository keep their panes**: `/clear` between questions, a respawn only when the
  repository or its pinned commit changes (`--fresh` opens a new pair per case; it cannot be combined with `--keep-context`). The `single` set is one repository (ripgrep), so one spawn.
- Indexing the other repositories happens in the background while earlier cases run, one indexer per repository.

## Claude Code instead of Codex: `--agent claude`

`run` and `run-set` take `--agent claude`; the same panes, clones, preflight and receipt, with Claude Code
(`ARENA_CLAUDE_BIN`, default `~/.local/bin/claude`; the shell's `claude` is a wrapper function the script never
uses) in each pane. The login is the user's keychain OAuth, so no separate config dir: isolation is by flags,
`--settings <run>/<arm>/codex-home/settings.json --setting-sources project --strict-mcp-config --permission-mode
bypassPermissions`. The pixel arm's settings hold only the `UserPromptSubmit` prompt-submit hooks (`--provider
claude`, `--parts` of them); the no-pixel arm holds none and runs with `PIXEL_BRIEF=0`. The arena accepts Claude's
folder-trust and bypass-permissions dialogs itself. The receipt is read from the arm's transcript under
`~/.claude/projects/<workspace path>/`: `tool_use` blocks are the calls, the `hook_additional_context`
attachments are the brief, tokens are summed per message id. Both arms still load the user's global `CLAUDE.md`.

Each `run-set` result carries `timings`: `index_s` (waiting on `set_base`), `clone_settle_s` (copy-on-write clones
and the daemon settle), `spawn_s` (panes until the agent is ready), `prompt_s`. The distinct repositories of the
the paired runs proceed one case at a time.

## One session for every question: `--keep-context`

`run-set catalog --agent claude --keep-context` is the same set without `/clear`: one Claude session per arm
answers the catalog prompts in a row, so the transcript holds every brief and every tool call in order. At the end
`$SCRATCH/sets/<source>-<utc>/session/` has `pixel.turns.json` and `no-pixel.turns.json` (per turn: prompt, briefs
the hooks added, tool calls, last answer; `session-digest.py` makes them), `report.md` (both arms turn by turn,
briefs included) and raw transcripts in Arena's scratch directory. The per-case table counts each case's own turn.
This is useful for examining the calls, brief and answer from each arm. Claude Code only.

## A set in a row: `run-set`

```bash
$S run-set catalog --pixel-bin /path/to/pixel
$S run-set catalog [--only id,id] [--parts 3] [--timeout S] [--sha C]
$S abort
```

`catalog` runs the prompts below against the current checkout. Each arm gets an
isolated clone and agent home;
the Pixel arm runs `pixel prepare [--history]` and registers
`pixel hook prompt-submit --host <codex|claude|devin> [--part K/N]`. The hook
reads the host payload from stdin. A no-Pixel arm has no hook and sets
`PIXEL_BRIEF=0`. Before an agent starts, Arena checks that the current hook emits
a brief and that `PIXEL_BRIEF=0` suppresses it.

Receipts collect each arm's brief, ordered tool calls, final answer, token usage,
agent-reported duration and total wall time. `abort` interrupts only Arena-owned
panes, stops Arena's agent processes and clears run locks. `status` and `receipt`
are read-only. Arena creates per-arm clones under its scratch directory so the
Pixel checkout is never used as an agent's working directory.

## Prompt catalog

One case per run; `arena.sh` reads this list, so edit a prompt here and
nowhere else. Keep a prompt's contract intact when adapting it.

1. `lookup` — recover an exact identifier, constant, signature, or initializer.
   `What byte, operation-count and time limits apply to a prompt-submit brief? Cite the current contract and implementation.`
2. `impact` — find a definition's production callers and the tests that exercise it.
   `Trace the current prompt-submit brief from pixel hook through the engine to its rendered host response; name the tests for that path.`
3. `flow` — trace a behavior across registration, events, processing, and output.
   `Trace prompt-submit from registration through hook processing to the host context envelope.`
4. `diagnosis` — explain a failure, fallback, or stale-state behavior from source and tests.
   `Why can the brief retain text evidence when the graph is stale or unavailable, and what tests cover that fallback?`
5. `configuration` — identify the exact setting, command, event mapping, or doctor rule.
   `Which command and event mapping configure the Codex prompt-submit hook, and what does doctor validate?`
6. `tests` — identify the relevant test cases and the contract assertions they make.
   `Which tests cover provider routing and prompt-submit envelope behavior, and what does each assert?`
7. `history` — recover constraints, intent, or prior behavior from recorded commits.
   `What constraints did the commit that introduced the prompt-submit brief record, and which files changed with it?`
8. `architecture` — explain ownership boundaries and source handoffs between modules.
   `Which current modules own indexing, graph evidence, brief rendering and host context, and where are their handoffs?`
