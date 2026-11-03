# Tool census oracle: does perfect injected context reach zero tool calls?

Issue #905, follow-up to the vanilla census (`scripts/bench/tool-census/`). Question: when the
prompt-submit hook injects exactly what the census agent itself found by exploring, does a
coding agent answer with **zero** tool calls, and does a framing sentence matter?

Driver: `scripts/bench/tool-census/oracle.py` (`cap-probe`, `build`, `run`, `report`, `--self-test`).
It reuses `run.py` (claude launch, env whitelist, `pixel` shim, per-session `cp -c` clone, quota
guard) and `classify.py` from the extractor lane (both in `scripts/bench/tool-census/`). It works on the
private census only (its recorded raw streams, `census.db` and the local `manifest.json`).
Raw streams, briefs, hook payloads and the run record live in
`<root>/oracle/` (`raw/`, `briefs/`, `hooks/`, `settings/`, `runs/oracle-1.json`).

## Method

**Sessions (13, every census session with `mode=answer`):** pixel-2 (configuration),
pixel-3 (overview), pixel-4 (lookup), pixel-5 (error-origin), excalidraw-3 (configuration),
excalidraw-4 (review), excalidraw-5 (flow), requests-3 (overview), requests-4 (impact),
requests-5 (tests), ripgrep-3 (diagnosis, `mode=answer`), ripgrep-4 (lookup), ripgrep-5 (history).

**Oracle** = the results of that census session's own exploration calls (class read, search,
list, git-read, plus a `git show` chain and a `pytest --collect-only` count that `classify`
files elsewhere), taken from the raw stream (not the 16 KB `calls.output`), in call order, exact
repeats and Reads inside an earlier Read of the same file dropped, workspace paths made
relative. Each entry is `### <tool> <args one-liner>` plus a fenced output. No test run, edit
or build is in any oracle (none of the 13 had one besides the collect-only count). The oracle
is perfect for what *that agent* chose to look at, not omniscient.

**Real hook limit (verified).** The hooks doc says a hook's `additionalContext` is capped at
**10,000 characters**, measured per string and per hook; beyond it Claude Code saves the text
to a file and injects the path plus the first 2,000 characters. `oracle.py cap-probe` (haiku,
tools off, line-numbered payload ending in `<tag>END-ZEBRA`) confirms all three:

| payload | model saw the end marker |
| --- | --- |
| 9,999 chars, 1 hook | yes |
| 10,000 chars, 1 hook | yes |
| 10,001 chars, 1 hook | no, preview only (lines 1-83, about 2,000 chars) |
| 12,000 chars, 1 hook | no, preview only |
| 2 hooks x 9,000 chars | yes, both markers |

**Arms** (fresh clone at the session's pinned sha, settings with ONLY a `UserPromptSubmit`
hook that prints `hookSpecificOutput.additionalContext`; 1 repetition per arm):

- **P** header `Pixel already read these for you (exact outputs, current as of this commit):` + oracle.
- **D** the same + footer `Answer from this context. Use a tool only if something you need is not shown above.`
- Both go through ONE hook, so they are limited to 10,000 characters. Five oracles exceed it
  (excalidraw-4 38,470, excalidraw-5 40,874, pixel-2 42,044, pixel-3 12,521, requests-4 15,212);
  they were cut to 9,351-9,997. Cut rule: first the lines each cited entry is cited for (at most
  about 2,500 characters per entry, around the line numbers the final answer cites), then the
  remaining room upgrades entries to whole, in call order. The path-citation test matched
  nearly every entry, so "cited first" mostly meant call order. Per-entry kept/total is in `build.json`.
- **PF / DF** (extra, only for those five): the same text **untruncated**, split at block
  boundaries over several hooks of at most 9,800 characters (2-7 hooks). Added because P/D
  mix two things, "perfect context" and "the 10,000-character delivery limit", and the
  question is about the first.

Isolation as in the census: empty settings plus the one hook, `--setting-sources project,local`,
tools `Read,Grep,Glob,Bash,Edit,Write`, CLI default model, whitelisted env, no `ANTHROPIC_*`, `pixel` shim.
Every session had exactly the expected `UserPromptSubmit` hook events (started + response,
outcome success, one pair per hook), 0 problems, no dirty tree, no shim hit. The hook text reaches the
model: 8 of the 13 D sessions answered with file:line citations and no tool call.

## Result per session (tool calls)

Calls = total tool calls; exploration = same here (every call was exploration except two test/collect runs, see reasons).

| session | category | oracle chars (cap 10,000) | census | P | D | PF | DF |
| --- | --- | --- | --- | --- | --- | --- | --- |
| excalidraw-3 | configuration | 3,266 | 2 | 0 | 0 | = P | = D |
| excalidraw-4 | review | 38,470 (cut) | 8 | 7 | 4 | 1 | 0 |
| excalidraw-5 | flow | 40,874 (cut) | 10 | 13 | 9 | 2 | 0 |
| pixel-2 | configuration | 42,044 (cut) | 7 | 4 | 3 | 0 | 0 |
| pixel-3 | overview | 12,521 (cut) | 2 | 1 | 1 | 0 | 0 |
| pixel-4 | lookup | 2,252 | 4 | 0 | 0 | = P | = D |
| pixel-5 | error-origin | 4,896 | 2 | 0 | 0 | = P | = D |
| requests-3 | overview | 6,279 | 2 | 0 | 0 | = P | = D |
| requests-4 | impact | 15,212 (cut) | 7 | 2 | 1 | 0 | 0 |
| requests-5 | tests | 6,907 | 5 | 1 | 0 | = P | = D |
| ripgrep-3 | diagnosis | 8,619 | 3 | 0 | 0 | = P | = D |
| ripgrep-4 | lookup | 4,391 | 1 | 0 | 0 | = P | = D |
| ripgrep-5 | history | 4,670 | 2 | 0 | 0 | = P | = D |
| **total** | | | **55** | **28** | **18** | **4** (full P) | **0** (full D) |

`= P` / `= D`: the oracle fits one hook, so PF/DF are identical to P/D and were not run.
"Full P" is PF where the oracle was cut and P elsewhere; likewise "full D".

**Sessions at zero calls:** P 7/13, D 8/13; with the untruncated oracle: full P 10/13, **full D 13/13**.

Totals over the same 13 sessions (single runs, so noise applies):

| | calls | cost | wall | input tokens (incl. cache) | output tokens |
| --- | --- | --- | --- | --- | --- |
| census (no hook) | 55 | $1.70 | 340 s | 979,608 | 29,894 |
| P | 28 | $1.83 | 280 s | 631,386 | 25,910 |
| D | 18 | $1.60 | 262 s | 411,246 | 23,504 |
| full P | 4 | $1.79 | 253 s | 297,209 | 24,263 |
| full D | 0 | $1.71 | 240 s | 232,375 | 22,839 |

Zero calls cut input tokens by 76 % and wall time by 29 %, but **not cost**: the injected context
is a cache write (the dearest input class), which the census sessions' many cache reads avoid.
(pixel-4 costs $0.14 at zero calls versus $0.10 for 4 calls in the census.)

## Calls that remained, and why

First call and its WHY. The stream carries no reasoning text (thinking is redacted) and the model
said nothing before 12 of the 13 first calls, so WHY is inferred by comparing the call with the
brief the model was given; the one quote is exact.

| arm | calls | first call | why |
| --- | --- | --- | --- |
| D excalidraw-4 | 4 | `git show HEAD -- frame.ts selection.ts actionClipboard.tsx ...` | truncation. Quote: "The context above cuts off the core of the diff, so I'll pull the parts I need." |
| P excalidraw-4 | 7 | `git show HEAD -- <same four files>` | truncation: the oracle entry for exactly this command was cut to 2,834 of 11,687 characters |
| PF excalidraw-4 | 1 | `sed -n 930,1010p frame.ts; sed -n 340,380p staticScene.ts; grep perform( manager.tsx` | missing info: none of the three regions was in the oracle (0 %, 15 %, 0 %), the census agent never read them |
| D / P excalidraw-5 | 9 / 13 | `Grep handleAppOnDrop\|EVENT.DROP\|insertImages(...` | truncation: the oracle's own first grep and the 20 KB App.tsx read (kept 2,515) were cut; calls 1-4 re-read lines 10358-10854, all in the full oracle |
| PF excalidraw-5 | 2 | `Read App.tsx 4900-4939`, then `renderElement.ts 512-531` | missing info: 0 % in the oracle |
| D / P pixel-2 | 3 / 4 | `grep fn global_config_path ... config_cmd.rs`, `sed decide_remote.rs` | truncation (each big entry kept about 2,400 of 9,898 / 12,727 characters), plus 1-2 regions never in the oracle |
| D / P pixel-3 | 1 / 1 | `Read ARCHITECTURE.md 57-65` | truncation: the entry was cut at 8,269 of 11,439 characters, this is the cut part |
| D / P requests-4 | 1 / 2 | `sed -n 831,870p sessions.py ...` | truncation: that range was in the cut tail of a bash entry; P's second call also reads `test_requests.py` 2645-2665 (not in the oracle) |
| P requests-5 | 1 | `pytest tests -k "redirect or ..." --collect-only -q \| tail -2` | verification habit: the same count was already in the oracle; re-ran with one extra `-k` term |

All 49 calls that remained (P 28, D 18, PF 3; DF 0), by individual call:

| reason | calls | where |
| --- | --- | --- |
| truncation (the region was in the full oracle, not in the delivered text) | 34 | P/D excalidraw-4 (7), excalidraw-5 (18), pixel-2 (5), pixel-3 (2), requests-4 (2) |
| missing info (the region was never in the oracle) | 12 | PF excalidraw-4 (1), PF excalidraw-5 (2), and 9 inside P/D calls: excalidraw-4 (3), excalidraw-5 (4), pixel-2 (1), requests-4 (1) |
| verification habit (a test run or a re-run of a result already shown) | 2 | P excalidraw-4 (`yarn vitest`, not installed here), P requests-5 |
| other | 1 | D pixel-2 third call, a wider env-var grep after two truncated ones |

The split is by region coverage and is approximate where one Bash call mixes several reads
(counted by the majority of its regions). In none of the 49 did the model call a tool although
everything it needed was shown.

## Answer quality

Judged against the census answer and the manifest `expected`: key facts from `expected` plus the
census answer's own load-bearing facts were checked by pattern in every answer, and the longer ones
(excalidraw-4 in all arms, pixel-2, ripgrep-3, requests-5) were read.

| session | P | D | PF / DF | note |
| --- | --- | --- | --- | --- |
| pixel-2 | worse | same | same / same | P: env var list shorter, no `PIXEL_FLOW_DIR` (its oracle was cut) |
| pixel-3 | same | same | same / same | all 18 crates named |
| pixel-4, pixel-5, excalidraw-3, requests-3, ripgrep-4, ripgrep-5 | same | same | n/a | every expected fact present, same file:line |
| excalidraw-4 | same | same | same / better | same headline risk (stale `frameId` child now clipped); DF adds the unawaited async copy before cut and a mis-named test |
| excalidraw-5 | same | same | same / same | none of the answers names `addFiles`/`FileManager`, and neither does the census answer |
| requests-4 | same | same | same / same | all 7 facts |
| requests-5 | same | same | n/a | P omits the `httpbin` fixture note the census answer has; D adds `tests/test_lowlevel.py` |
| ripgrep-3 | same | same | n/a | |

No arm hallucinated beyond its context: where P/D context was cut the model fetched the rest
instead of guessing. No answer was worse in D, PF or DF.

## Verdict

**Zero is reachable with perfect context, with the directive framing: 13/13 sessions at 0
tool calls (full D), against 55 in the census.** The conditions that matter:

1. **Delivery, not the model, set the floor.** With the oracle fitting one hook (8 sessions)
   P and D were at 0 for 7 and 8 sessions; the one exception (P requests-5) was a pytest re-run.
   Every other remaining call in P/D came from the 10,000-character cap cutting the five large oracles.
2. **Directive framing is worth it.** Without the footer (P/PF) 4 calls remained in
   the untruncated arms (3 reads of regions the census agent never saw, 1 re-run); with it, 0. Under truncation
   D needed fewer calls than P in four of the five cut sessions (4 vs 7, 9 vs 13, 3 vs 4, 1 vs 2; pixel-3 tied 1 vs 1).
   With one repetition per arm this is a consistent direction, not a measured effect size.
3. **Oversized oracles need more than one hook.** The cap is per string, so a context over 10,000
   characters must be split over several hooks (verified: two 9,000-character hooks both arrive), or ranked
   down to what the question needs. Cut oracles reproduced exploration, not removed it.
4. **Cost does not fall with calls.** 76 % fewer input tokens and 29 % less wall time, same dollars,
   because the injected context is billed as cache writes.

Caveats: one repetition per arm; the oracle is the census agent's own exploration, so a perfect
brief for it still lacks regions a different run would want (the 3 PF calls); 13 answer-mode
sessions over 4 repositories; the model is the CLI default (`claude-opus-5-5`); nothing here says
how a retrieval engine would *produce* such an oracle, only what a perfect one buys.

## Reproduce

```sh
python3 scripts/bench/tool-census/oracle.py cap-probe          # the limit, with a marker (haiku, about $0.01)
python3 scripts/bench/tool-census/oracle.py build              # briefs, hooks, settings under oracle/
python3 scripts/bench/tool-census/oracle.py run --run-id oracle-1 --concurrency 12   # 36 sessions
python3 scripts/bench/tool-census/oracle.py report --run-id oracle-1
python3 -I scripts/bench/tool-census/oracle.py --self-test
```

Run `oracle-1`: 36 sessions (13 P, 13 D, 5 PF, 5 DF), 80.8 s wall at concurrency 12, $5.73,
five-hour utilisation 0.39 to 0.45.
