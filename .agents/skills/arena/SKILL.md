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
   (`run-hook task-event --provider codex --event prompt-submit`) at the
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
  `S=.agents/skills/arena/arena.sh; $S run-set census --agent claude --pixel-bin "$PWD/target/dev-release/pixel"`
  (build first with `cargo build --profile dev-release -p pixel-cli`; "the census prompts" means all 13).
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
set index in the background, two at a time, while earlier cases run; the first case indexes inline with its panes already open.

## One session for every question: `--keep-context`

`run-set single --agent claude --keep-context` is the same set without `/clear`: one Claude session per arm
answers the 13 questions in a row, so the transcript holds every brief and every tool call in order. At the end
`$SCRATCH/sets/<source>-<utc>/session/` has `pixel.turns.json` and `no-pixel.turns.json` (per turn: prompt, briefs
the hooks added, tool calls, last answer; `session-digest.py` makes them), `report.md` (both arms turn by turn,
briefs included) and the raw `*.session.jsonl` (omitted for `census`, whose prompts are private: there the prompt is
replaced by its hash; the arena writes no copy of the prompt, but Claude Code's own transcript of each session stays under `~/.claude/projects/` as for any session, and the arena never deletes it). A set over several repositories opens one pair per repository: the first pair keeps these names, each later pair adds `-2`, `-3`, ... to its turn and transcript files, and `report.md` holds every pair one after the other. The per-case table counts each case's own turn. This is the input for improving the probes and
the brief until the questions need no tool calls: read `report.md`, pick a turn whose pixel arm still called tools,
and take it to the `probe` skill. Claude Code only.

## A set in a row: `run-set`

```bash
$S run-set census --pixel-bin /path/to/pixel --brief-extra "python3 -I scripts/bench/probe-chain.py"
$S run-set <catalog|census|bench> [--only id,id] [--parts 3] [--timeout S] [--sha C] [--baseline FILE]
$S abort                       # stops both panes AND the loop
```

One command runs every question of a source in a row, in the two panes `arena:<case>:pixel` and
`arena:<case>:no-pixel` that are opened per case and closed when the next case needs a new pair (in `--here` the previous pair stays open until then; with `--tab` each pair is closed at that moment, and a reused pair lives across the cases of one repository at one commit): `catalog` is the prompts above, `single` is the 13 questions of `single.tsv` (one per census category) about ripgrep, never this repository, `bench`
is `eval/brief-bench/questions.jsonl`, `census` reads the 13 private answer-only prompts from
`/private/tmp/claude-501/census/census.db` at run time (the arena itself writes them to no log, `run.json` or table; they reach the panes and the agent's own session transcript, which belongs to the agent). Per case, sequentially: a copy-on-write clone of the repository at its pinned commit (the
repository, commit and binary are indexed once, with history), a fresh `CODEX_HOME` per arm with the same login
copy, the same prompt sent to both, a wait until both settle or `--timeout` passes, and the tool calls counted from
each Codex rollout (parsed events, not the screen). The pixel arm registers `--parts` (default 3) prompt-submit
hooks the way `pixel setup` does. `--brief-extra CMD` puts `hook-wrap.py` between the hooks and the pinned binary:
it joins the brief of all parts, runs `CMD` (environment `CLONE`, `PROMPT`, `BRIEF_FILE`; a leading `@@REPLACE@@`
replaces the brief), and splits the result over the parts again; `scripts/bench/probe-chain.py` is the chain of gated
`probes/` until the brief runs probes itself. The end prints and saves (`$SCRATCH/sets/<source>-<utc>/table.md`)
`case | no-pixel calls | pixel calls | brief bytes | pixel 0?` with totals, flags "0 but NO BRIEF delivered", and
marks any case above its previous pixel baseline (`--baseline`; for `census` the last recorded live run).

Probes interact: the first combined chain (every prototype on every question) fixed three questions and
regressed four that were at zero. A combination is claimed only after a full-set run keeps every zero.

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

## Offline test

`python3 .agents/skills/arena/offline-protocol-test.py` checks the hook payload, the receipt built from a fixture rollout and `abort` (including its refusal of a path-shaped case name) with a fake `herdr`: no model, no Herdr, no login. CI runs it with the other script contracts.
