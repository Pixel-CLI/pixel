---
title: "MCP server or CLI: what does an agent tool cost per turn?"
description: "What an MCP server and a command-line tool each put into a coding agent's context on every turn, measured on GitNexus, Pixel, semble and stacklit."
answer: "mcp-or-cli-context-cost"
---

<!-- Figures: data/answers.toml. Every number here must be in the /benchmarks/ sections the entry names; the build fails otherwise. -->

## Two ways to pay before the first answer

An MCP server announces its tools to the agent with a schema per tool: a name, a description and the shape of every argument. The agent carries those schemas in its context on every turn, whether it calls the tool or not. A command-line tool has no schema; the agent needs a prompt instead, the text that tells it which command answers which question, and that prompt is carried on every turn too.

Either way the cost is fixed and paid before anything is asked, so it is worth measuring on its own, apart from what each answer costs.

## What the measurement shows

GitNexus serves its graph as MCP tools and adds a block to `CLAUDE.md`; Pixel is a CLI whose agent prompt, copied by hand into an agent's instructions, rides along in each session (`pixel install` no longer deploys it). The first costs several times the second. But semble and stacklit, both MCP servers with a handful of small tools, cost less than Pixel's prompt: the number of tools and the length of what each one says decide the bill, not the protocol that carries them.

So the question to ask of any agent tool is how many tokens it keeps in the context, and what it returns for them.
