---
title: "How do I reduce Claude Code's token usage?"
description: "Where Claude Code's tokens go on a coding task, what changed when it could ask an index instead of reading files, and how to measure it on your own sessions."
answer: "reduce-claude-code-tokens"
---

<!-- Figures: data/answers.toml. Every number here must be in the /benchmarks/ sections the entry names; the build fails otherwise. -->

## What the agent spends tokens on

Before Claude Code edits anything, it finds out where to work: it searches, opens files, follows imports and opens more files. Each file it opens stays in the context and is read again, as input, on every later turn. The fewer whole files enter, the less every turn costs.

## What changes with an index

With Pixel wired in, the agent asks instead of opening: `pixel scope-task` for the files a task touches, `pixel list-signatures` for what a file contains, `pixel who-calls` for the callers of a function. Each answer is fitted to a budget and says whether it is complete. An outline locates definitions; an edit still needs their bodies. File/signature volumes do not establish token or cost savings for a whole session.

`pixel setup` wires it into Claude Code (lifecycle hooks in `~/.claude/settings.json`), and `pixel setup --repo` adds a guard in the repository that flags untargeted reads of large files. The [Claude Code page](../../for/claude-code/) lists every file it writes and how to remove it.

## Measure it on your sessions

`pixel token-savings` reads the local action log and reports measured Pixel output volumes and versioned workflow estimates. It does not observe all agent reads, model context or billed cost. Run it after a few days of normal work; it tells you more about your code than any benchmark of ours.
