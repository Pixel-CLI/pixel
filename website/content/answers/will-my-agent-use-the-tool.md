---
title: "Will my agent actually use the tool I give it?"
description: "Why coding agents fall back to grep and whole-file reads, what made Claude Code call Pixel in every recorded run, and what is not measured."
answer: "will-my-agent-use-the-tool"
---

<!-- Figures: data/answers.toml. Every number here must be in the /benchmarks/ sections the entry names; the build fails otherwise. -->

## Why a tool goes unused

An agent is trained on the tools every repository has: search, read, the shell. A new tool it is told about once, in a file it may or may not load, competes with the habits it was trained on, and the habits often win. Listing a command in a README is not enough.

## What made the difference

Pixel does not wait for the agent to discover it. `pixel setup` puts its protocol into every session of the agents it supports, and on Claude Code it does so through lifecycle hooks, so the guidance is there on the first turn of every session. `pixel setup --repo` adds a guard to one repository that flags untargeted reads of large files; it advises and never blocks.

Check actual use in your own sessions: the current installation has no quantified adoption rate here, and tool use alone does not establish a time or cost gain.

## Check it on your machine

`pixel doctor` reports whether the hooks and the protocol are in place for each agent, and `pixel action-log` reads the local log of every Pixel command that ran, so you see whether your agent called it. The [page for your agent](../../for/) lists what `pixel setup` writes for it.
