---
title: "Will my agent actually use the tool I give it?"
description: "Why coding agents fall back to grep and whole-file reads, what a prompt-submit brief and a skill change about that, and what is not measured."
answer: "will-my-agent-use-the-tool"
---

<!-- Figures: data/answers.toml. Every number here must be in the /benchmarks/ sections the entry names; the build fails otherwise. -->

## Why a tool goes unused

An agent is trained on the tools every repository has: search, read, the shell. A new tool it is told about once, in a file it may or may not load, competes with the habits it was trained on, and the habits often win. Listing a command in a README is not enough.

## What made the difference

Pixel does not wait for the agent to discover it. `pixel setup` hooks the prompt-submit event of the agents it supports, so a code question arrives with a source-cited brief the agent did not have to ask for, and it installs a `pixel` skill whose description names the moments an agent greps instead: who calls X, where X is defined, what breaks before a rename. Neither blocks a search or a read, so the agent can still grep.

Check actual use in your own sessions: the brief and the skill have no quantified adoption rate here, and tool use alone does not establish a time or cost gain.

## Check it on your machine

`pixel brief "<a question about your code>" .` prints the brief your agent receives with that prompt. `pixel doctor` reports whether the hooks are in place for each agent, and `pixel action-log` (a [command plugin](../../docs/#command-plugins)) reads the local log of every Pixel command that ran, so you see whether your agent called it. The [page for your agent](../../for/) lists what `pixel setup` writes for it.
