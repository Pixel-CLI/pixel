# Coverage floor of the relevance gate on a held-out window (2026-10-18, #1222)

**Labels are AI labels, not human ones.** Two label files, both private:

- **reference**: the #1220 file (SHA-256 `302a272b…`, 218 replayed rows:
  54 project, 40 follow-up, 124 other). #1220's rules were written on it.
- **held-out**: written for this report, before any verdict was read
  (SHA-256 `05a472f4…`, 205 rows, 191 replayed: 29 project, 66 follow-up,
  96 other). Prompts typed after 2026-10-18 19:00 (7) and from 2026-10-14
  16:00 to 2026-10-16 22:10 (242), minus the 44 whose opening 60 characters
  match a reference row (resumed sessions copy their turns with a later
  time stamp). The window after 19:00 alone was too small to measure.

## Result

| window | binary | briefed | project recall | follow-up recall | precision | hit@8 |
|---|---|---:|---:|---:|---:|---:|
| reference | `main` @ `1f9f8423` | 134 | **89 %** (48/54) | **82 %** (33/40) | **60 %** (81/134) | 58 % (22/38) |
| reference | this PR | 124 | **89 %** (48/54) | **82 %** (33/40) | **65 %** (81/124) | 53 % (20/38) |
| held-out | before #1220 @ `7887b9e6` | 120 | 69 % (20/29) | 70 % (46/66) | 55 % (66/120) | 64 % (9/14) |
| held-out | `main` @ `1f9f8423` | 119 | 69 % (20/29) | 70 % (46/66) | **55 %** (66/119) | 64 % (9/14) |
| held-out | this PR | 109 | 69 % (20/29) | 67 % (44/66) | **59 %** (64/109) | 64 % (9/14) |

Rows that moved from `main` to this PR: reference, 10 `other` refused, one
follow-up refused (two shared words of three) and one project row refused
and another admitted, which no rule here can do (replay noise: the rule only
removes briefs); held-out, 8 `other` and 2 follow-ups refused. The hit@8
drop on the reference is the project row that swapped.

Three passes per run, the last reported; every repository served by a daemon
of the binary under test at the end, 0 of another binary.

```sh
python3 -I scripts/bench/brief-recall.py score --labels <labels-2026-10-18.jsonl> --bin <pixel> --out <dir> --label ref-<arm>
python3 -I scripts/bench/brief-recall.py score --labels <labels-2026-10-18-heldout.jsonl> --bin <pixel> --out <dir> --label heldout-<arm>
```

## #1220 does not hold on the held-out window

#1220's operation, steer and filler words refused 20 `other` prompts of the
reference window. On the held-out window they refuse one (120 → 119 briefs,
precision 55.0 % → 55.5 %). The words were drawn from the reference rows;
the held-out prompts that operate a pull request or steer the agent use
other words. A word list fitted on one window is a development result.

## Where the remaining `other` briefs come from

`brief --json` on every row of both windows, joined with the replay: most
briefed `other` prompts are not admitted by the model's score but by the
coverage floor (`admitted_by_coverage`): the model scores them off topic
and one structural co-file holds half of their informative keywords. On the
reference window that path carries 22 `other` against 10 relevant `lookup`
prompts; on the held-out window 18 against 13. A status question or a steer
of the agent ("Le replay a bientôt fini ?", "Merge those PRs") shares two
words with a repository that uses them (`replay` and `fini`, `merge` and
`prs`); at three the floor keeps the long prompts and refuses those.

## Tried and dropped (prototype, simulated on the coverage path)

Each a filter on the coverage admission, measured as precision / project
recall on the reference and precision on the held-out window:

- Shared words counted among the heaviest structural co-file's structural
  keywords (names and symbols) only, ≥ 2: 73 % but project 76 %; held-out
  56 %. Dropped: refuses project prompts whose words sit in a file's text.
- The co-file must be code, not documentation: 62 %, project 65 %.
- Shared words must be a symbol name somewhere in the repository, ≥ 2:
  65 %, project 81 %; held-out 57 %.
- No coverage admission at all: 70 %, project 59 %; held-out 57 %.
- A score floor on the coverage path (≥ −1.48): 66 %, project 89 %, but
  held-out 59 % at the cost of 6 follow-ups (46 → 40): worse than three
  words for the same precision.

The repository's own vocabulary is already the gate's input (per-keyword
document frequency, symbol and file-name matches); weighing it differently
did not separate the prompts. In Pixel's own repository `replay`, `agent`,
`prompt`, `merge` and `artifact` are symbols: conversation about the agent
and work on the code share the words.

## What is left above 59 %

Of the 53 `other` prompts `main` briefs on the held-out window, about 20
come through the coverage path (8 refused here) and most of the rest never
reach the model: a strong signal (a path, a symbol, a code span) or a prose
prompt left unjudged. Reaching 80 % needs an input that is not the prompt's
words: the session's previous turn, or what the agent did after the brief.
