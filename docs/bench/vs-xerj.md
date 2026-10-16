# Pixel's brief vs a xerj brief: arena runs of the 13 census prompts

Which retrieval gives the agent the better context before it starts: Pixel's brief, or the passages
[xerj](https://github.com/xerj-org/xerj) ranks highest for the same prompt? Measured on 2026-10-16 in Claude Code,
one run per prompt and setup. **Pixel's brief: 26 and 25 tool calls in two runs. A xerj brief in its place: 37.**
Installed as xerj documents it (`xerj init`: MCP tools and a skill), the agent never called xerj.

- **Machine:** Apple M4 Max, 16 cores, 64 GiB, macOS 27.0.
- **Pixel:** 0.8.0, the installed `~/.local/bin/pixel`, built from `22c6a3ad` with uncommitted changes, sha256
  `e94e6d864f8fb77c992bbb5c550806c8b5605bbb316a19fdbf1c547ee24f4189`. The same binary served every run.
- **xerj:** `v1.0.0-rc.95`, the `aarch64-apple-darwin` release binary (checksum verified), default lexical embedder.
  One node per repository (`xerj --insecure --data-dir <dir> --port <p>`), each indexed with
  `xerj autoindex <repository>` under the default `ax` prefix, so a search sees that repository only.
- **Agent:** Claude Code in the arena panes ([`.agents/skills/arena`](../../.agents/skills/arena/SKILL.md)).
- **Prompts:** the 13 census prompts, 3 to 4 questions on each of 4 repositories at pinned commits: excalidraw
  `541653c7`, pixel `85bede7d`, requests `611c6162`, ripgrep `3fce3b5b`. The prompts are private (they come from
  `census.db`) and are not in this record; the case ids are.

## Setups

| setup | what reaches the agent before it starts | arena set |
|---|---|---|
| Pixel brief, run 1 | Pixel's prompt-submit brief | `census-20261016T094135Z` (Pixel arm) |
| Pixel brief, run 2 | Pixel's prompt-submit brief | `census-20261016T111954Z` (Pixel arm) |
| xerj brief | xerj's passages through the same hook, Pixel's brief removed | `census-20261016T113449Z` (hooked arm) |
| xerj installed | nothing; xerj's MCP tools and skill from `xerj init` | `census-20261016T111954Z` (other arm) |

- **xerj brief.** The arena's `--brief-extra` runs [`vs-xerj/xerj-brief.py`](vs-xerj/xerj-brief.py) after the Pixel
  hook. Its output starts with `@@REPLACE@@`, so `hook-wrap.py` replaces Pixel's brief with it whole. The script runs
  `xerj search "<prompt>" -k 10 --json` on the repository's node and finds each passage in the clone's own file. It
  prints each one as a brief block (`### path:lo-hi — xerj search hit`, then numbered lines, at most 60 lines a
  block), under the same `[PIXEL:BRIEF]` channel tag and the same closing line as Pixel's brief. It stops at
  13,000 bytes, the mean size of Pixel's census briefs, and replaces Pixel's brief even when xerj fails or finds
  nothing. [`vs-xerj/claude-xerj-brief-arm.sh`](vs-xerj/claude-xerj-brief-arm.sh) also takes the `pixel` shim off the
  agent's PATH.
- **xerj installed.** [`vs-xerj/claude-xerj-arm.sh`](vs-xerj/claude-xerj-arm.sh) runs `xerj init` in the clone of the
  arena's no-Pixel arm (`.mcp.json`, `.claude/skills/xerj/SKILL.md`, an `AGENTS.md` section). It points `XERJ_URL` at
  the repository's node and passes the `.mcp.json` to Claude Code, which the arena otherwise starts with
  `--strict-mcp-config`. That arm has no Pixel hook and no `pixel` on PATH.

```bash
S=.agents/skills/arena/arena.sh
$S run-set census --agent claude --pixel-bin ~/.local/bin/pixel                                     # run 1
ARENA_CLAUDE_BIN=docs/bench/vs-xerj/claude-xerj-arm.sh \
  $S run-set census --agent claude --pixel-bin ~/.local/bin/pixel                                   # run 2 + xerj installed
ARENA_CLAUDE_BIN=docs/bench/vs-xerj/claude-xerj-brief-arm.sh \
  $S run-set census --agent claude --pixel-bin ~/.local/bin/pixel \
  --brief-extra "python3 -I docs/bench/vs-xerj/xerj-brief.py"                                       # xerj brief
```

Tool calls are counted from each Claude Code transcript. Every answer was read back for the text "session limit":
none of the 52 answers above hit it. Two earlier census runs in which every arm read 0 calls had hit it.

What each brief held:
- **Pixel brief:** received in 13 of 13 sessions of both runs; 12,377 bytes on average in run 1, 13,099 in run 2.
- **xerj brief:** 5 to 10 xerj blocks per prompt, 11,745 bytes on average. No Pixel text: none of the 13 briefs
  contains `region pack`, `### repo map`, `kind:` or `next: pixel`.
- **Pixel CLI:** no session of any setup ran the `pixel` CLI.

## Results

| case | Pixel brief, run 1 | Pixel brief, run 2 | xerj brief | xerj installed | xerj calls in "installed" |
|---|---:|---:|---:|---:|---:|
| excalidraw-3 | 2 | 1 | 1 | 2 | 0 |
| excalidraw-4 | 4 | 3 | 7 | 12 | 0 |
| excalidraw-5 | 4 | 4 | 4 | 10 | 0 |
| pixel-2 | 3 | 3 | 3 | 6 | 0 |
| pixel-3 | 1 | 1 | 1 | 2 | 0 |
| pixel-4 | 0 | 0 | 3 | 3 | 0 |
| pixel-5 | 1 | 1 | 3 | 3 | 0 |
| requests-3 | 1 | 0 | 1 | 3 | 0 |
| requests-4 | 5 | 4 | 5 | 5 | 0 |
| requests-5 | 2 | 4 | 4 | 4 | 0 |
| ripgrep-3 | 1 | 2 | 1 | 3 | 0 |
| ripgrep-4 | 1 | 1 | 2 | 2 | 0 |
| ripgrep-5 | 1 | 1 | 2 | 3 | 0 |
| **tool calls** | **26** | **25** | **37** | **58** | **0** |
| tokens | 1,807,475 | 1,771,789 | 2,467,192 | 3,158,451 | |
| agent time | 348 s | 346 s | 374 s | 485 s | |

- **xerj brief against Pixel's two runs:**
  - Above both runs on 5 prompts: excalidraw-4, pixel-4, pixel-5, ripgrep-4, ripgrep-5.
  - Below both on none.
  - Within their range on the other 8.
  - Two prompts Pixel's brief answers with no tool call at all (pixel-4 in both runs, requests-3 in run 2) took 3
    and 1 with the xerj brief.
- **xerj installed:** its 58 calls were `sed` 24, `grep` 20, `git` 5, `ls` 5, `rg` 2, `wc` 1 and `python` 1, with
  xerj's 13 MCP tools listed in every session.
- **No brief at all** (the arena's no-Pixel arm, for reference): 59 calls in run 1, 54 in the xerj-brief run.
- **Raw rows** (ids, counts, brief bytes, timings; no prompt text):
  [run 1](vs-xerj/raw/run1-pixel-brief.results.jsonl),
  [run 2 with xerj installed](vs-xerj/raw/run2-pixel-brief-vs-xerj-init.results.jsonl) (the "installed" arm is its
  `no_pixel_calls`), [xerj brief](vs-xerj/raw/run3-xerj-brief.results.jsonl).

## Indexing time

`xerj autoindex` per repository, from its own `xerj-progress` phase timers (`phase_elapsed_s`), on one shared node,
one repository after another:

| repository | files | records | total | writing records (`index`) | `finalize-refresh` | `finalize-catalog` |
|---|---:|---:|---:|---:|---:|---:|
| ripgrep | 196 | 9,738 | 38 s | 2.5 s | 14.9 s | 13.5 s |
| requests | 94 | 1,673 | 58 s | 1.6 s | 14.6 s | 26.3 s |
| excalidraw | 969 | 47,255 | 184 s | 8.2 s | 143.3 s | 18.8 s |
| pixel | 1,012 | 20,449 | 368 s | 24.4 s | 298.0 s | 24.4 s |

- **Most of the time is xerj's finish steps:** writing the records takes seconds, the server's finish steps take the
  rest.
- **The pixel row is an upper bound:** that index ran while run 1 was going.
- **The four per-repository nodes the runs used** were indexed in parallel, all four at once: requests 50 s,
  ripgrep 95 s, pixel 390 s, excalidraw 495 s.

## Caveats

- **One run per prompt and setup.** A single agent run varies: on Pixel's brief, excalidraw-4 took 7, 11, 4 and 3
  calls in four runs between 2026-10-15 and these. Pixel's brief has two runs here and the xerj brief one; three runs
  per setup, read by their medians, would be the next step.
- **The xerj brief is our own wrapper, not a xerj feature.** xerj ships no prompt-submit hook. The brief here is
  `xerj search` on the whole prompt, top 10, in the slot and at the size of Pixel's brief. A different query (per
  identifier, `xerj def`) or more room could do better.
- **"xerj installed" measures adoption, not retrieval.** The agent had xerj's tools and never used them. `xerj init`
  puts its instruction in `AGENTS.md`, which Claude Code does not read; only the skill and the MCP tools reached it.
- **Not reproducible outside the maintainer's machine as is:** the census prompts are private. The public bench
  (`eval/brief-bench/`) would make the same comparison reproducible.
