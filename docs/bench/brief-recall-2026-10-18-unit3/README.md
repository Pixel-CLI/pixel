# Brief admission heuristics on the recall replay (2026-10-18, #1032 unit 3)

**Labels are AI labels, not human ones**, the same file as unit 2
(SHA-256 `302a272b…dbce98ed`, 218 replayed rows: 54 project, 40 follow-up,
124 other). The heuristics below were written after reading the briefed
`other` rows of that file, so the gain is measured on the rows that suggested
it: there is no held-out window. Treat it as a development result.

## Result

| run | binary | briefed | project recall | follow-up recall | precision | hit@8 |
|---|---|---:|---:|---:|---:|---:|
| baseline | `7887b9e6`, SHA-256 `c41d123b…` | 152 | **89 %** (48/54) | **80 %** (32/40) | **53 %** (80/152) | 55 % (21/38) |
| candidate | this PR, SHA-256 `4fbae569…` | 133 | **89 %** (48/54) | **82 %** (33/40) | **61 %** (81/133) | 58 % (22/38) |

The candidate refuses 20 prompts the baseline briefed, all labelled `other`,
and refuses no project or follow-up prompt. One follow-up is briefed by the
candidate and not by the baseline: no rule here can add a brief, so it is
the replay's noise (3 rows changed between the last two passes of each run).
Precision clears the issue's guard (≥ 59 %), not its target (≥ 80 %).

Both runs: three passes, the last reported, and every repository served by
a daemon of the binary under test at the end (`daemons at the end: 9 of the
binary under test, 0 of another binary`). The candidate binary was built from
`904d9570` with this PR's production code; only tests and docs changed after
the build.

```sh
python3 -I scripts/bench/brief-recall.py score --labels labels-2026-10-18.jsonl --bin <pixel @7887b9e6> --out <dir> --label baseline
python3 -I scripts/bench/brief-recall.py score --labels labels-2026-10-18.jsonl --bin <pixel @candidate> --out <dir> --label candidate
```

## The baseline moved: the unit 2 run may have measured another binary

Unit 2 reported 85 % project recall and 53 % precision on 147 briefs from the
same binary and labels. While preparing this unit, the daemons serving
`pixel` and `yespark-rails` turned out to be an installed 0.8.0 (`9a8adca4`,
older than #1180's facts fix): a brief from the binary under test asked those
daemons, which closed 93 prose prompts as `relevance: facts: index_stale` in
a command-route pass. `brief-recall.py score` did not stop or check them, so
it cannot be told which binary served the unit 2 run. The script now stops
each repository's daemon before its first row and counts, at the end, the
repositories another binary serves (one makes the run invalid and the exit
status 1). The baseline above is that corrected run.

## The heuristics

All three act before any evidence is asked for, on the typed text:

1. **A plural acronym is one keyword.** `PRs` was split at its case change
   into `p` and `rs`, and `rs` (the Rust extension) is a short technical
   keyword: "Merge les PRs" had two content words, one of them not an
   operation. `fold_plural_acronyms` writes `PRs`, `APIs`, `IDs` in capitals
   before tokenizing.
2. **Operations, steers and fillers.** `OPS_WORDS` (a prompt made only of
   these is refused) gains pull-request and issue words (`pr`, `prs`,
   `issue`, `merger`, `close`, `clore`, `admin`, `head`, `verte`, `rerun`,
   `worktree`, `creer`), the words that steer the agent's run (`next`,
   `stop`, `pause`, `again`, `retry`, `ahead`) and the function and address
   words a short French or English prompt leaves when its language is not
   detected (`les`, `peux`, `vas`, `you`, `est`, `ordre`, `actuel`).
3. **Codex's request heading.** Codex's IDE extension writes the attached
   files above `## My request:`. Only the text under the first such heading
   is the user's: `## My request:\nGo ahead` is the acknowledgement
   `Go ahead`, and `request` is no longer a keyword.

## Tried and not kept

- Removing every French stopword whatever the detected language, simulated
  on the unit 2 verdicts: the same refusals plus one follow-up lost ("Sur
  claude:" and a paste fell under two content words). Only the address and
  function words above were kept.
- `those` as a filler: it refused "Please fix those issues", which is a
  follow-up in a session that holds the issues. Left out.

## What is left above 61 %

The 52 `other` prompts still briefed are mostly conversation about another
system (a Datadog notebook, a Slack monitor, retention filters), status
questions about the agent ("is the replay done?") and requests to write a
prompt. Their words are content words; telling them apart needs the
relevance gate to weigh the repository's own vocabulary, not a word list.
