---
name: extract-probes
description: "Mine this project's local agent transcripts for the tool sequences a brief probe could replace: /extract-probes, 'mine transcripts for probes', 'which tool sequences could the brief replace'. Ranks the most common pre-answer call sequences by question kind and maps each to an existing probe or a candidate."
---

# Which tool sequences could the brief replace?

`scripts/bench/extract-probes.py` reads the transcripts this machine already holds and ranks what agents do before they
answer or edit. The ranking tells which probe to write next; the `probe` skill writes and proves it.

```bash
python3 -I scripts/bench/extract-probes.py [--since 30d] [--agent all|claude|codex|pi] [--top 20] [--kinds question|all|a,b]
```

## Procedure

1. **Mine** (the script, mechanical). Claude Code `~/.claude/projects/<slug>/*.jsonl` for this project and its
   worktrees; Codex `~/.codex/sessions/**/rollout-*.jsonl` whose `cwd` is under the project (including its JS `exec`
   tool, which batches shell commands). Pi has no reader yet and is skipped. Other readers worth reusing when
   widening this: the `pixel-retro` skill's transcript readers and the census-extract worktree's `extract.py` /
   `classify.py`.
2. **Segment**: a user turn, then the tool calls before the first edit (`Edit`, `Write`, `apply_patch`) or the end
   of the turn. Each call is normalised to a shape with paths reduced to roles: `read(code|test|doc|config)`,
   `grep`, `glob`, `bash:git-log`, `bash:--help`, `bash:pixel`, `tool:js`, and so on.
3. **Rank** n-grams of 1 to 4 calls and the first six calls of a long turn by turns x calls saved, per question kind
   (history, review, flow, config, tests, overview, lookup; the keyword heuristic of the brief). `--kinds all`
   adds the task turns (`other`).
4. **Map** each row to the probe that covers it (`history-fallback` for `git log/show/diff`, `doc-sections` for
   documentation reads, `config-registry` for config reads, `name-files` for listings) or mark it a candidate.
5. **Validate** a candidate with `scripts/bench/probe-loop.py --prompt "<a sample prompt of that cluster>"`: the current
   brief first, 1 live run, stop if it is already zero-call. Write the probe as a generic rule (the `probe` skill's
   overfitting guard applies: no literal path or word from one session).
6. **Report** the ranked table: pattern -> turns -> calls it costs -> covering probe or candidate -> validation.

## Privacy

Transcripts and prompts stay on this machine. The script keeps nothing: it prints counts and abstract shapes only, no
prompt, path, command, output or secret, and writes no file. Do not paste a transcript, a prompt or a command from
one into a PR, an issue or a probe; quote the shape and the count. If a secret shows up while debugging a reader,
do not print it.

## Limits

A keyword heuristic assigns the question kind, and most turns are tasks (`other`), not questions: the ranking says
what is common, not what is avoidable. Many `bash:pixel` rows are calls an installed brief already prompts. Treat
the table as a lead for `probe-loop`, which measures, not as a verdict.
