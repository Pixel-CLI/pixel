# Brief recall replay (2026-10-18, #1032 unit 2)

**Labels are AI labels, not human ones.** The hand-labelled 14–16 October sample
(648 prompts) behind the issue's 33 % recall is not available, so a new window
was extracted and every prompt was labelled by Claude (Opus 5.5) from its text,
with written rules (one rule per prompt or pattern, first match wins). Treat the
figures as a reproducible baseline for the next units, not as a comparison with
the historical sample: other window, other labeller, replay instead of live
decisions.

## Result

`main` `7887b9e6` (the brief's code is the same at `7a8a8788`), dev-release,
SHA-256 `c41d123b…25cc96`. The window runs from 2026-10-16 18:10 to
2026-10-18 19:00 (Europe/Paris). It holds 221 prompts typed in Claude Code,
Codex and pi; 218 replayed, 3 in unindexed repositories left out.

| | rows | briefed | project recall | follow-up recall | precision | hit@8 |
|---|---:|---:|---:|---:|---:|---:|
| all | 218 | 147 | **85 %** (46/54) | **80 %** (32/40) | **53 %** (78/147) | **54 %** (20/37) |
| Claude Code | 139 | 98 | 91 % | 84 % | 58 % | 48 % |
| Codex | 61 | 34 | 64 % | 67 % | 44 % | 40 % |
| pi | 18 | 15 | 100 % | n/a | 40 % | 100 % (5) |

Labels of the replayed rows: 54 project, 40 follow-up, 124 other. Three passes, the last one
reported; 1 row changed between the last two. A second full run on the same
labels (before the last label fix) gave the same verdict on 217 of 218 rows.

**Precision is under the issue's guard (≥ 59 %).** Of the 69 briefed `other`
prompts, most are git, PR and CI operations ("merge the PRs with --admin",
"close the issue") or conversation outside the repository (Datadog, review
writing, a question about another tool). Short acknowledgements ("go next",
"Go ahead") also get a brief in this replay, because a fresh session carries no
history to judge them by. Unit 3 starts from these rows.

## Method

```sh
python3 -I scripts/bench/brief-recall.py extract --since 2026-10-16T18:10 --until 2026-10-18T19:00 --out rows.jsonl
# label every row: project | follow-up | other (here: AI rules, kept private with the labels)
python3 -I scripts/bench/brief-recall.py score --labels labels-2026-10-18.jsonl --bin <pixel> --out <dir> --label published
```

- **Extraction.** It keeps the prompts a person typed. It leaves out slash
  commands, hook, task and CI notifications, `<bash-input>` turns, compaction
  summaries, Codex app envelopes, `codex exec` and SDK runs, and sessions
  outside a git repository or under a temporary directory. Ambient
  browser context is stripped from the typed text. A prompt copied into a second
  transcript (a resumed or forked session) counts once. The files the agent read
  or edited before the next prompt are its turn's `touched` files.
- **Labels.** `project`: work on or a question about the repository's code.
  `follow-up`: continues the previous turn and still needs the repository.
  `other`: acknowledgements, git/PR/CI operations, meta, questions outside the
  repository. The labelled file (SHA-256 `302a272b…dbce98ed`) holds prompt text
  and stays private (`~/.local/share/pixel/brief-recall/`).
- **Replay.** Each row goes through
  `run-hook task-event --provider pi --event prompt-submit` with a `brief_only`
  payload. It runs in the row's repository as it is now, in a fresh session,
  with dedupe and logging off. Each repository is warmed until three briefs in
  a row agree. Three passes run: the first cold pass briefed 71 rows, the next
  ones 131 and then 147, and the third matched the second on every brief.
- **Metrics.** Project recall: briefed project prompts over project prompts.
  Follow-up recall: the same for follow-ups. Precision: briefed project and
  follow-up prompts over all briefed prompts. hit@8: among briefed project
  prompts that touched a file, the share whose brief names one of those files
  among its first 8 repository files.

## Limits

- The replay runs against today's tree, not the tree each prompt was typed
  against, and without session history. The follow-up lift cannot apply, so
  follow-up recall is a lower bound and the acknowledgements' precision cost an
  upper bound.
- The labels are one model's reading. A second review of the project and
  follow-up lists and of every `other` prompt over 25 words moved 12 prompts
  to project or follow-up (from 45/39/137 to 56/40/125): the routine dotfiles
  bump prompts and the long pasted audit prompts had fallen in a loose `other`
  rule about merges. No human re-labelled the set.
- hit@8 counts only what the agent touched in that turn. A brief naming the
  right file the agent then did not open counts as a miss.

`report.json` holds the per-row verdicts (hashed ids, labels, briefed, bytes,
named and touched counts, hit). It holds no prompt text.
