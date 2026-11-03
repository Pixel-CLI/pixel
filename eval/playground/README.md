# Brief playground

A local web page to look at what `pixel brief` returns for a prompt, and why
(issue [#901](https://github.com/Pixel-CLI/pixel/issues/901), phase 1). One
Bun server, one HTML page, no framework, no build step.

## Run

```bash
cd eval/playground
bun install        # only @types/bun
bun run dev        # http://127.0.0.1:4901  (PORT=... to change; check it is free first)
bun test           # API self-test against a fake pixel binary
```

`bun run start` is the same without hot reload.

## What it shows

A horizontal strip of test cards (90vw x 100vh, scroll-snap), a "+ new test"
card at the end. Cards and their results persist in `localStorage`.

1. **Prompt + Send.** Type a prompt or fill it from the 218-prompt corpus
   (filter box + dropdown). Send calls `/api/brief`.
2. **Pixel brief ready to hydrate the LLM.** The `[PIXEL:BRIEF]` header,
   `searched:` and `confidence:` lines are highlighted; `read:`, `also:` and
   `files:` entries are clickable `path:line` chips that open the cited range
   from the fixture. A collapsible "why" shows gate, tier, score, signal, kind,
   bytes against the 2048 B cap, latency and the gate features.
3. **Test.** Two panes run Codex on the same prompt: LEFT with the brief, RIGHT
   with the initial prompt only. Each streams one line per tool call (numbered,
   `rg`/`grep`/`find` counted as searches, `cat`/`sed -n`/`head` as reads), then
   the answer; the footer has tool calls, searches/reads, wall time and tokens.
   **Abort** kills both runs of the card (the whole process group).

## Codex runs

`POST /api/run {cardId, prompt, arm: "pixel"|"none", briefText}` streams SSE
(`meta`, `codex` = one JSONL event per frame, `end`); `POST /api/abort {cardId}`
kills the process group. It spawns the real binary
(`PLAYGROUND_CODEX`, default `~/.local/bin/codex`; the shell's `codex` is a
wrapper function) as `codex exec --json -s read-only --skip-git-repo-check -C <fixture>`
with stdin closed (Codex otherwise waits on it) and `PIXEL_BRIEF=0`.

Each arm has its own `CODEX_HOME` under `/private/tmp/claude-501/playground/codex-home-<arm>/`:
`auth.json` copied from `~/.codex` (mode 600, never read or logged), a
`config.toml` with only the model scalars of the user's config
(`model`, `model_reasoning_effort`, `approval_policy`, ...) and the fixture
trusted; no MCP servers, notify, plugins or marketplaces. The `none` home has
no hook. The `pixel` home has one `UserPromptSubmit` hook, `cat
"$PLAYGROUND_BRIEF_ENVELOPE"`, and the server writes the hook envelope
(`hookSpecificOutput.additionalContext` = the card's brief) to a per-run file
and passes its path in that variable, with `--dangerously-bypass-hook-trust`
so `exec` does not wait for a hook review.

Hooks fire under `codex exec` (0.162.1): the brief reaches the first turn as a
`developer` message in the pixel arm's rollout and is absent from the none
arm's. `PLAYGROUND_INJECT=prefix` switches to the fallback, the brief
prepended as `Context from Pixel:` / `Task:`, and the pane says so.

## Fixture and binaries

`POST /api/brief {prompt, bin}` runs `<bin> brief --json --metrics off "<prompt>" .`
with cwd = the fixture, timeout 5 s. A binary without `--json` is retried
without it and the raw text is shown, with no decision record.

- **Fixture:** `PLAYGROUND_REPO`, default `/private/tmp/claude-501/883-ab/zero/pixel`
  when it exists, else this checkout. It must be indexed. To build one:
  `bun run setup [dest] [pixel-binary]` clones the repository at `85bede7d`
  (`git clone --no-local`) and runs `prepare-repo`.
- **Binaries:** `bins.json` (gitignored), a JSON array:
  ```json
  [{ "name": "installed", "path": "/Users/me/.cargo/bin/pixel" },
   { "name": "PR 891", "path": "/private/tmp/claude-501/883-ab/zero-pixel" }]
  ```
  Without it, the defaults are the installed `pixel` and the 883-ab builds that
  exist. Only paths in this list can be run.

## API

| Route | |
| --- | --- |
| `GET /api/prompts` | corpus + local prompts |
| `POST /api/prompts {text, lang?, kind?, on_topic?}` | add a local prompt |
| `GET /api/bins` | binaries, whether each exists and supports `--json` |
| `POST /api/brief {prompt, bin}` | brief text, parsed ranges, decision record, bytes, latency |
| `GET /api/status?bin=` | graph/index `ready`/`stale`/`missing` from `status --json` (doctor fallback), cached 10 s |
| `POST /api/refresh {bin}` | `prepare-repo .` in the fixture, one at a time; state shows in `/api/status` |
| `GET /api/census/sessions?source=&category=&repo=` | census sessions: strip, counts, explore-before (prompts only for `census`) |
| `GET /api/census/session/:id` | ordered calls (output cut at 4 KB), final answer and diff (census only) |
| `GET /api/census/summary?source=` | calls by need/class, histograms, by category |
| `POST /api/run {cardId, prompt, arm, briefText}` | SSE stream of one Codex run |
| `POST /api/abort {cardId}` | kill the card's Codex process groups |
| `GET /api/file?path=&start=&end=` | a range of the fixture (paths outside it are refused) |

The server listens on 127.0.0.1 only.

## Census tab

Reads `/private/tmp/claude-501/census/census.db` read-only (`bun:sqlite`): what
agents do in a session before they answer. Summary bars for the selected source,
a session list with a colour strip of its calls by need, and a timeline of each
session's calls with their output (4 KB). History sources (claude, codex, pi)
show aggregates and strips; their prompts and final answers stay private.
