# Brief vs oracle coverage (issue #906)

How much of the context a vanilla agent actually looked at does a Pixel brief reproduce? `scripts/tool-census/score_brief.py`
scores any `pixel` binary offline against the 20 census sessions of issue #905 (13 answer, 7 edit). No model runs: the
oracle is read off the census `calls`, the brief is parsed from `pixel brief`.

## Method

- **Oracle regions** of a session are what its calls printed before the final answer: `Read` ranges (whole file clipped to
  the 2 000 lines Read returns), the `(file, line)` hits and context lines of `Grep`/`grep`/`rg` outputs, `sed -n A,Bp` /
  `cat` / `head` / `tail` ranges, `git log`/`git show` commit ids and touched files, `ls`/`find`/`wc` listings. Paths are
  normalised to the repo at the session sha. Edit sessions use every exploration call, before and after the edit.
- **Brief regions** are every `path:line` / `path:a-b` the brief prints (`read:`, `also:`, `files:`, `defined:` or any other
  label), excerpt blocks headed `path:a-b` (numbered or not), and headerless fenced blocks whose lines are matched back to
  the clone. `line cov` counts pointers (a `read:` range is a region); `inline cov` counts only lines the brief quotes.
- **Metrics** per session: line coverage, inline coverage, region coverage (mean over oracle regions), file recall, commit
  recall, brief bytes, and `answer cov` = line coverage of the `path:line` mentions in the census final answer (the "would
  answer" proxy; `n/a` when the answer cites none).
- **State.** The scorer clones `cache/<repo>` at the session sha, runs `prepare-repo .`, one `search-meaning` and throwaway
  briefs until the output settles (the first briefs after a daemon start come back empty or lexical-only while the daemon
  builds chunk vectors in the background), then `brief "<prompt>"` per session. Two consecutive runs of each binary gave
  byte-identical briefs. `--cold` scores the first prompt of a fresh index instead.
- **Binaries.** `main-883`: a frozen build of PR #891, pixel 0.7.1 commit `1cfc4b1e` (PR #891, the brief
  logic of `main`), `brief --json`. `installed`: `~/.cargo/bin/pixel`, pixel 0.7.1 commit `457a56a2-dirty`, built 2026-10-09,
  plain `brief` (no `--json`, so no gate column).

Reproduce:

```bash
python3 -I scripts/tool-census/score_brief.py --self-test
python3 -I scripts/tool-census/score_brief.py --bin <pixel> --sessions all --label <name>   # score/runs/<name>/report.{json,md}
python3 -I scripts/tool-census/score_brief.py compare a/report.json b/report.json --labels a,b
```

## Result: today's briefs reproduce almost none of the oracle

Per session (`line cov` counts pointers, `inline` quoted lines, `answer cov` the cited final-answer lines):

| session | category | mode | main-883: line cov | main-883: inline | main-883: file recall | main-883: answer cov | main-883: bytes | installed: line cov | installed: inline | installed: file recall | installed: answer cov | installed: bytes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| excalidraw-1 | change-location | edit | 0% | 0% | 0% | 0% | 0 | 0% | 0% | 0% | 0% | 0 |
| excalidraw-2 | diagnosis | edit | 0% | 0% | 0% | 0% | 0 | 0% | 0% | 0% | 0% | 0 |
| excalidraw-3 | configuration | answer | 0% | 0% | 0% | 0% | 0 | 0% | 0% | 0% | 0% | 432 |
| excalidraw-4 | review | answer | 0% | 0% | 0% | 0% | 0 | 0% | 0% | 0% | 0% | 275 |
| excalidraw-5 | flow | answer | 0% | 0% | 0% | 0% | 0 | 0% | 0% | 0% | 0% | 0 |
| pixel-1 | change-location | edit | 0% | 0% | 0% | 0% | 0 | 0% | 0% | 0% | 0% | 0 |
| pixel-2 | configuration | answer | 0% | 0% | 8% | 0% | 1845 | 0% | 0% | 5% | 0% | 284 |
| pixel-3 | overview | answer | 0% | 0% | 0% | n/a | 0 | 0% | 0% | 0% | n/a | 383 |
| pixel-4 | lookup | answer | 31% | 0% | 100% | 50% | 1919 | 0% | 0% | 0% | 0% | 351 |
| pixel-5 | error-origin | answer | 0% | 0% | 0% | 0% | 0 | 1% | 0% | 100% | 17% | 244 |
| requests-1 | change-location | edit | 0% | 0% | 0% | 0% | 0 | 0% | 0% | 0% | 0% | 0 |
| requests-2 | diagnosis | edit | 0% | 0% | 0% | n/a | 0 | 0% | 0% | 0% | n/a | 0 |
| requests-3 | overview | answer | 0% | 0% | 0% | n/a | 0 | 0% | 0% | 2% | n/a | 197 |
| requests-4 | impact | answer | 14% | 0% | 4% | 33% | 940 | 11% | 0% | 4% | 33% | 310 |
| requests-5 | tests | answer | 0% | 0% | 0% | n/a | 811 | 0% | 0% | 50% | n/a | 279 |
| ripgrep-1 | change-location | edit | 0% | 0% | 0% | 0% | 0 | 0% | 0% | 0% | 0% | 0 |
| ripgrep-2 | change-location | edit | 0% | 0% | 60% | n/a | 1780 | 0% | 0% | 0% | n/a | 0 |
| ripgrep-3 | diagnosis | answer | 0% | 0% | 0% | 0% | 0 | 1% | 0% | 40% | 0% | 285 |
| ripgrep-4 | lookup | answer | 16% | 0% | 50% | 36% | 1648 | 0% | 0% | 0% | 0% | 277 |
| ripgrep-5 | history | answer | n/a | n/a | 100% | n/a | 826 | n/a | n/a | 0% | n/a | 0 |

Per category and overall (means over sessions; `answer cov` averages the sessions that cite lines):

| group | n | main-883: line cov | main-883: inline | main-883: file recall | main-883: answer cov | installed: line cov | installed: inline | installed: file recall | installed: answer cov |
|---|---|---|---|---|---|---|---|---|---|
| answer sessions | 13 | 5% | 0% | 20% | 13% | 1% | 0% | 16% | 6% |
| edit sessions | 7 | 0% | 0% | 9% | 0% | 0% | 0% | 0% | 0% |
| all | 20 | 3% | 0% | 16% | 9% | 1% | 0% | 10% | 4% |
| change-location | 5 | 0% | 0% | 12% | 0% | 0% | 0% | 0% | 0% |
| configuration | 2 | 0% | 0% | 4% | 0% | 0% | 0% | 3% | 0% |
| diagnosis | 3 | 0% | 0% | 0% | 0% | 0% | 0% | 13% | 0% |
| error-origin | 1 | 0% | 0% | 0% | 0% | 1% | 0% | 100% | 17% |
| flow | 1 | 0% | 0% | 0% | 0% | 0% | 0% | 0% | 0% |
| history | 1 | n/a | n/a | 100% | n/a | n/a | n/a | 0% | n/a |
| impact | 1 | 14% | 0% | 4% | 33% | 11% | 0% | 4% | 33% |
| lookup | 2 | 24% | 0% | 75% | 43% | 0% | 0% | 0% | 0% |
| overview | 2 | 0% | 0% | 0% | n/a | 0% | 0% | 1% | n/a |
| review | 1 | 0% | 0% | 0% | 0% | 0% | 0% | 0% | 0% |
| tests | 1 | 0% | 0% | 0% | n/a | 0% | 0% | 50% | n/a |

Overall means, cold index for `main-883`: line cov 1%, file recall 10%, answer cov 2% (answer sessions 1% / 12% / 4%), 7 of 20 briefs made.

## Which oracle regions Pixel misses

| type | main-883: coverage of oracle items; sessions >= half | installed: coverage of oracle items; sessions >= half |
|---|---|---|
| definition bodies (code read) | 1% of 3054; 0/15 sessions | 1% of 3054; 0/15 sessions |
| usage / search-hit lines | 3% of 414; 1/9 sessions | 0% of 414; 0/9 sessions |
| tests | 0% of 519; 0/9 sessions | 0% of 519; 0/9 sessions |
| config | 0% of 92; 0/2 sessions | 0% of 92; 0/2 sessions |
| docs | 0% of 1028; 0/9 sessions | 0% of 1028; 0/9 sessions |
| git | 4% of 28; 0/5 sessions | 0% of 28; 0/5 sessions |
| list | 0% of 72; 0/4 sessions | 1% of 72; 0/4 sessions |

- **Nothing is quoted.** Inline coverage is 0% for both binaries: every brief is a list of `files:` hits and `read:` pointers, so even a
  perfect pointer costs the agent a Read.
- **Definition bodies are the bulk and the biggest miss**: 3 054 of 5 107 oracle lines (60%), present in 15 of 20 sessions, covered 1%, and
  no session reaches half. The pointers that exist are one `read:` range of 20 to 40 lines on a keyword function, not the
  definition the agent read plus its neighbours.
- **Docs** are 1 028 lines (20%), 904 of them the whole-file read of `CONTRIBUTING.md` in pixel-1, in 9 sessions; **tests** 519 lines
  in 9 sessions (requests-4 and requests-5 read tests; the `tests: none` of requests-4 is a miss); **usage / search-hit lines** 414
  lines in 9 sessions (only pixel-4 reaches half, one hit line, through `main-883`); **config** 92 lines in 2 sessions (excalidraw-3's `.env.*` files and
  `vite.config.mts`, requests-3's `pyproject.toml`); **git** 28 items in 5 sessions (commit ids and touched files; ripgrep-5 and excalidraw-4 hold 16 of them); listings 72 files in 4 sessions.
- **The gate is the first miss.** `main-883` makes a brief for 7 of 20 prompts and none for any edit prompt but ripgrep-2;
  the mean line coverage of the briefs that exist is 10% (6 sessions with an oracle). `installed` makes 11 and covers 1% of them.
  File recall is the strongest number (16% and 10% overall): the right file is sometimes named without the lines (`main-883`: pixel-4 100%,
  ripgrep-5 100%, ripgrep-2 60%; `installed`: pixel-5 100%).

## Caveats

- Line coverage is line-weighted, so pixel-1 (974 lines, 904 of them one doc), excalidraw-5 (1 072) and excalidraw-4 (447) dominate
  the pooled counts; the macro means, region coverage and the per-session rows are the fairer view.
- A search hit counts when its exact line is in a brief region, as the issue asked; a `files:` hit one line away from the oracle hit does not.
- Edit-session oracles include calls made after the first edit, whose line numbers can sit a few lines off the pinned file.
- `answer cov` uses `path:line` mentions only; answers that cite a function name or a prose range are `n/a`.
