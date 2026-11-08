# TokenEconomizer's ideas, tested on Pixel's own sessions (2026-11-08)

Issue #1383. [TokenEconomizer](https://github.com/microsoft/TokenEconomizer) reports 18.4–30.9% lower billed cost on
277 SWE-Bench tasks from three mechanisms that learn across a repository's sessions. Each was replayed over past
transcripts before any Rust was written (`.agents/rules/prototype-first.md`), against a threshold fixed before the run.
None reached its threshold, so none was ported.

Data: 375 Claude Code transcripts of this repository; the repo-facts run also read three other repositories.
One user and one host, Codex and pi untested. The scripts were throwaway and are not committed.

| Idea | Predicted | Threshold | Measured | Result |
| --- | --- | --- | --- | --- |
| Repo facts (`X` fails here, `Y` works) | facts pre-empt failing Bash calls in later sessions | covers ≥5% of held-out failures, ≥80% precision | 0 of 155 held-out failures covered; 0 facts reached 3 confirming sessions (best: 2) | dropped |
| Loop guard, repeated failure (same failing call ≥3× in a turn) | fires on stuck turns | ≤5% false positives on good turns, ≥20% recall, ≥3 calls left | fired on 0 of 695 turns | dropped |
| Loop guard, call count (> 2.5× the repo's median) | fires on runaway turns | same | fired on 36.8% of 664 good turns | dropped |
| Tool pruning (deny tools loaded but unused) | saves ≥1% of billed cost | median per-session saving ≥1% | 0.97% (aggregate spend 0.68%, per-turn median 0.46%) | dropped |

Limits, none of which reverses a row:

- **Repo facts.** The split had 227 training and 98 held-out sessions, but 294 of 325 sessions share one date (2026-08-30), so
  the training set is thin. Held-out failures were 155 of 4,346 Bash calls (3.6%). With hindsight, a perfect miner would
  have covered 5.2–17.0% of those failures, at most about 0.6% of Bash calls.
- **Loop guard.** Only 3 turns were labelled stuck, so its recall is not a real estimate. The two results in the table do
  not depend on it.
- **Tool pruning.** The gate form matters: the ratio of medians gives 1.55%, sessions with 10+ turns 1.01%, and bytes ÷ 3
  instead of ÷ 4 gives 1.29%. Claude Code already defers MCP tools, so a pruned name saves only the name. Replayed, 13
  of the 21 sessions that used a deferred tool would have hit a deny, which is the larger cost of this feature.

What did ship from this work: `eval/lib/usage.py`, which bills a transcript by token kind at listed prices, used by the
arena receipts, `eval/score.py` and `scripts/bench/token-ab/tokens.py`.
