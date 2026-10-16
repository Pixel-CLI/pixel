# Pixel vs Pixel + xerj: arena run of the 13 census prompts

Does adding [xerj](https://github.com/xerj-org/xerj)'s search passages to Pixel's prompt-submit brief cut the
agent's tool calls? Measured on 2026-10-16. On this run, no: **26 tool calls with the Pixel brief, 37 with the xerj
passages added**, and 1.81M → 2.51M tokens. Each setup ran each prompt once, so treat it as one sample, not a
verdict.

- **Machine:** Apple M4 Max, 16 cores, 64 GiB, macOS 27.0.
- **Pixel:** 0.8.0, the installed `~/.local/bin/pixel`, built from `22c6a3ad` with uncommitted changes, sha256
  `e94e6d864f8fb77c992bbb5c550806c8b5605bbb316a19fdbf1c547ee24f4189`. The same binary served both setups.
- **xerj:** `v1.0.0-rc.95`, the `aarch64-apple-darwin` release binary (checksum verified), one local node started
  with `xerj --insecure --data-dir <dir>`, default lexical embedder.
- **Agent:** Claude Code in the arena panes ([`.agents/skills/arena`](../../.agents/skills/arena/SKILL.md)).
- **Prompts:** the 13 census prompts, 3 to 4 questions on each of 4 repositories at pinned commits: excalidraw
  `541653c7`, pixel `85bede7d`, requests `611c6162`, ripgrep `3fce3b5b`. The prompts are private (they come from
  `census.db`) and are not in this record; the case ids are.

## Method

xerj is not integrated into Pixel. The second setup runs a throwaway hook step,
[`vs-xerj/xerj-extra.py`](vs-xerj/xerj-extra.py), through the arena's `--brief-extra`. The arena's
`hook-wrap.py` runs the real Pixel hook, then the script, and appends what the script prints. The script maps the
clone's commit to the xerj index of that repository and runs `xerj search "<prompt>" -k 5 --json`. It finds each
passage in the clone's own file and prints it as a brief block (`### path:lo-hi — xerj search hit`, then numbered
lines), at most 40 lines per block and 6,000 bytes in all. It prints nothing on any failure.

```bash
S=.agents/skills/arena/arena.sh
xerj autoindex <copy of each repository at its census commit> --prefix <one prefix per repository>
$S run-set census --agent claude --pixel-bin ~/.local/bin/pixel              # Pixel brief
$S run-set census --agent claude --pixel-bin ~/.local/bin/pixel \
  --brief-extra "python3 -I docs/bench/vs-xerj/xerj-extra.py"                # Pixel brief + xerj passages
```

The two sets ran one after the other (09:41–09:55 and 09:55–10:09 UTC), since two sets cannot share the arena's
panes. The arena also runs a no-Pixel side next to each Pixel side; its counts are in the raw files and not
discussed here. The tool calls are counted from each Claude Code transcript. All 26 Pixel-side answers were read
back for the text "session limit": none hit it. Two earlier census runs in which every arm read 0 calls had hit it.
In set B, every one of the 13 briefs the agent received carried the xerj blocks (3 to 5 each).

## Results

| case | tool calls, Pixel | tool calls, Pixel + xerj | tokens, Pixel | tokens, Pixel + xerj | brief bytes, Pixel | brief bytes, Pixel + xerj | xerj blocks |
|---|---:|---:|---:|---:|---:|---:|---:|
| excalidraw-3 | 2 | 2 | 115,355 | 120,363 | 6,117 | 8,787 | 3 |
| excalidraw-4 | 4 | 10 | 239,399 | 626,481 | 16,782 | 21,703 | 3 |
| excalidraw-5 | 4 | 3 | 222,336 | 195,440 | 9,082 | 28,488 | 4 |
| pixel-2 | 3 | 4 | 244,483 | 327,473 | 15,496 | 26,599 | 4 |
| pixel-3 | 1 | 0 | 123,924 | 62,726 | 20,895 | 25,271 | 5 |
| pixel-4 | 0 | 0 | 57,579 | 61,407 | 16,710 | 25,363 | 5 |
| pixel-5 | 1 | 1 | 113,010 | 122,930 | 12,956 | 18,677 | 3 |
| requests-3 | 1 | 0 | 78,308 | 41,098 | 8,014 | 12,589 | 3 |
| requests-4 | 5 | 3 | 262,904 | 179,731 | 18,583 | 23,608 | 5 |
| requests-5 | 2 | 7 | 118,337 | 350,694 | 9,592 | 13,371 | 5 |
| ripgrep-3 | 1 | 4 | 84,832 | 228,626 | 17,355 | 22,831 | 3 |
| ripgrep-4 | 1 | 2 | 75,377 | 120,430 | 7,991 | 13,918 | 3 |
| ripgrep-5 | 1 | 1 | 71,631 | 76,013 | 1,322 | 6,736 | 3 |
| **total** | **26** | **37** (+42%) | **1,807,475** | **2,513,412** (+39%) | 160,895 | 247,941 | |

xerj helped on 4 prompts (excalidraw-5, pixel-3, requests-3, requests-4), hurt on 5 (excalidraw-4, pixel-2,
requests-5, ripgrep-3, ripgrep-4) and changed nothing on 4. The agent's own time on the Pixel side added up to 348 s
against 476 s. Raw rows: [`vs-xerj/raw/pixel.results.jsonl`](vs-xerj/raw/pixel.results.jsonl) and
[`vs-xerj/raw/pixel-xerj.results.jsonl`](vs-xerj/raw/pixel-xerj.results.jsonl).

## Indexing time

`xerj autoindex` per repository, from its own `xerj-progress` phase timers (`phase_elapsed_s`):

| repository | files | records | total | writing records (`index`) | `finalize-refresh` | `finalize-catalog` |
|---|---:|---:|---:|---:|---:|---:|
| ripgrep | 196 | 9,738 | 38 s | 2.5 s | 14.9 s | 13.5 s |
| requests | 94 | 1,673 | 58 s | 1.6 s | 14.6 s | 26.3 s |
| excalidraw | 969 | 47,255 | 184 s | 8.2 s | 143.3 s | 18.8 s |
| pixel | 1,012 | 20,449 | 368 s | 24.4 s | 298.0 s | 24.4 s |

Writing the records takes seconds; the server's finish steps take most of the time. The pixel index ran while the
first arena set was running, so its time is an upper bound. A `--no-graph` re-index of ripgrep, to check whether
graph building was the cost, took 158 s with the other index and the arena running at the same time, so it does not
answer the question.

## Caveats

- **One run per prompt and setup.** A single agent run varies a lot. On the Pixel brief alone, excalidraw-4 took 7,
  11 and 4 calls in three runs between 2026-10-15 and this one. Three runs per setup, read by their medians, would be
  the next step before any claim.
- **Two variables moved, not one.** The xerj blocks are the intended change, but Pixel's own brief also differed in
  size between the two runs of a prompt: excalidraw-5 carried 9,082 bytes, then 28,488 bytes, which is near the
  28,500 that three hook parts carry. The xerj blocks add at most 6,000 bytes, so the rest is Pixel's own brief
  varying from run to run.
- **xerj was used as brief evidence, not as a tool.** xerj's own recommended use is an agent calling `xerj search`
  or its MCP tool during the task. This run puts xerj's top five passages for the whole prompt in the brief, before
  the model starts. It measures that use only.
- **Not reproducible outside the maintainer's machine as is:** the census prompts are private. The public bench
  (`eval/brief-bench/`) would make the same comparison reproducible.
