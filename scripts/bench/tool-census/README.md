# Tool census runner

Issue #905. Twenty real-demand prompts, run as parallel **vanilla** Claude Code
sessions (no Pixel at all) over four pinned repositories, with the full raw
`stream-json` of every session kept. The extractor pairs each `tool_use` with
its result from those streams; this directory only produces them.

| File | What |
| --- | --- |
| `run.py` | `prepare`, `probe`, `run`, `--self-test`. Standard library only. |
| `manifest.json` | the 20 prompts: `id`, `repo`, `sha`, `category`, `mode`, `prompt`, `expected`. **Private and git-ignored**: write it yourself or set `PIXEL_CENSUS_MANIFEST`; nothing else needs it |
| `question_source.py` | the question sources of the zero-call test tools: the public bench `eval/brief-bench/` (default) and the optional private census (`--census-db`) |
| `score_brief.py` | offline scorer of how much of a question's oracle context (issue #906) a `pixel brief` reproduces: `--bin <pixel> [--questions Q.jsonl \| --census-db DB] [--sessions answer\|all]`, `compare`, `--self-test`; results in `docs/bench/brief-oracle-coverage.md` |
| `oracle.py` | the oracle experiment (private census only): `cap-probe`, `build`, `run`, `report`; results in `docs/bench/tool-census-oracle.md` |
| `extract.py`, `classify.py` | the transcript extractor into `census.db` and its call classifier; rule tests in `test_classify.py`, `test_extract.py` |

The runner reuses `claude_argv`, `run_claude`, the `ENV_KEEP` whitelist, the
thread pool and the `rl_five_hour` quota guard of `scripts/bench/ab-brief-live.py`
(copied with attribution, adapted: edit tools on, no Pixel hook).

## Run it

```bash
python3 scripts/bench/tool-census/run.py prepare                      # pinned clone cache, idempotent
python3 scripts/bench/tool-census/run.py probe                        # isolation evidence; 12 checks, run before a campaign
python3 scripts/bench/tool-census/run.py run --run-id census-2 --concurrency 20
python3 scripts/bench/tool-census/run.py run --run-id census-3 --skip-done --only pixel-1,ripgrep-2 --concurrency 10
python3 scripts/bench/tool-census/run.py --self-test
```

State lives under `$PIXEL_CENSUS_ROOT` (default `$TMPDIR/pixel-tool-census`, `--root`):

| Path | Content |
| --- | --- |
| `cache/<repo>` | one pinned clone per repo (pixel `85bede7d`, excalidraw `541653c7`, requests `611c6162`, ripgrep `3fce3b5b`) |
| `ws/<id>/` | one `cp -c` (APFS copy-on-write) clone per session, on the pinned commit; delete after a run |
| `raw/<id>.stream.jsonl` | the session's `claude -p --output-format stream-json` output, untouched |
| `diffs/<id>.diff` | edit sessions: everything that differs from the pinned commit (commits, edits, new files) |
| `runs/<run-id>.json` | run record: quota before/after, cost, argv, env, pins, per-session metrics |
| `census.db` | `sessions` table (source `census`), columns in the `SCHEMA` of `extract.py`, which also owns `calls` |

`sessions.tokens_in` is all input tokens (fresh + cache creation + cache read);
`ok` is 1 when the result is `success`, the exit code is 0, no hook event
appeared, the session stayed on the pinned commit and the answer is not empty.

## Method

- **Settings** an empty JSON object, `--setting-sources project,local` (the user's
  own settings and `CLAUDE.md` stay out; the clone's own `CLAUDE.md` and `.claude/`
  load, as they would for a developer), `--strict-mcp-config`,
  `--disable-slash-commands`, `--no-session-persistence`, `--permission-mode dontAsk`.
- **Tools** `Read,Grep,Glob,Bash,Edit,Write` (`--tools` and `--allowedTools`);
  `Agent,Task,WebFetch,WebSearch,NotebookEdit` denied, so every call is the one
  agent's own. `dontAsk` is enough: Edit and Write in the clone and any Bash worked.
- **Model** the CLI default, no `--model`: `claude-opus-5-5`, Claude Code 2.1.295
  (the first `claude` on `PATH`, or `$PIXEL_CLAUDE_BIN`; sha256 in the run record; the shell `claude`
  is a wrapper function and is not used). `--max-budget-usd 2` per session.
- **Environment** only `HOME USER LOGNAME TMPDIR LANG LC_* TERM TZ` pass through
  (no `CLAUDE_*`, no `ANTHROPIC_*`: `ANTHROPIC_API_KEY` is banned and an assertion
  guards it), plus `DISABLE_AUTOUPDATER=1` and `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`
  (auto-memory would write into the real `~/.claude`). `PATH` starts with a shim
  directory whose `pixel` and `pixel-dev` print `command not found` and exit 127.
- **Repos** cloned once into `cache/`, then `cp -c` per session. The pixel clone
  keeps history up to the pin only (later refs are deleted and pruned), `.pixel/`
  removed. Clones have no `origin` (excalidraw keeps it: a blobless clone fetches
  blobs lazily). The first campaign's `pixel-1` followed the repo's own rule to
  fetch the actual base and rebased onto the live `main`; with no remote that cannot
  happen, and `left_pin` in the run record flags a session whose `HEAD` stops
  descending from the pin.
- **Quota guard** reads `rate_limit_event` live. Abort before launch above 0.75
  five-hour utilisation (`--start-ceiling`), stop launching at 0.9
  (`--max-utilization`), kill the running sessions at 0.97 (`--kill-utilization`).
  A one-token session reads the utilisation before and after.

## Isolation evidence

`run.py probe` writes `probes.json` and checks, on a real session in an
excalidraw clone (all 12 pass on 2026-10-14):

1. `init` lists exactly `Bash Edit Glob Grep Read Write`, 0 MCP servers, 0 skills.
2. No `hook_started` / `hook_response` event in a stream recorded with
   `--include-hook-events`.
3. Sensitivity: the same command with a canary settings file (hooks on
   `SessionStart PreToolUse PostToolUse Stop`) does show those events, so check 2
   could have failed.
4. `command -v pixel` is `.../work/shim-bin/pixel`; `pixel --version` prints
   `pixel: command not found`, exit 127.
5. Write and Edit change a file in the clone; Bash sees it in `git status`.
6. The clone's own `CLAUDE.md` is read; the model reports no `rtk`/Bun instruction
   (the user's global `CLAUDE.md` and rules are not in context).
7. API-key source `none`, model is the unflagged default.

Campaign `census-2`: 0 hook events, 0 `pixel` invocations and 0 shim hits in 20
streams (checked by scanning every Bash `tool_use` and result).

Known deviations from a clean room, kept on purpose: the Bash tool runs the user's
zsh snapshot (aliases `grep` and `ls` with colour flags; `PATH` order puts the shim
first and `command -v pixel` proves it); Claude Code's own system prompt applies;
`SHELL=/bin/bash` was tried to avoid the rc file and hangs the Bash tool, so it is not set.

## Cost and size

`census-2`: 20 sessions, **$2.96**, 93 s wall at concurrency 20 (no rate limit
hit), five-hour utilisation 0.31 to 0.34 (about 3 points). Per session $0.04 to
$0.43, median $0.10. 121 tool calls in total, 1 to 16 per session (median 6).
A full rerun is cheap enough to repeat after a runner fix.
