---
title: "Pixel vs RTK"
description: "Pixel and RTK both save an agent's tokens, on different sides: RTK compresses what shell commands print, Pixel answers from an index so the agent reads less. They run together in Claude Code."
tool: "rtk"
---

## How they differ

RTK sits between the agent and its shell: `cargo build`, `git status` or a test run go through it, and the agent gets a short summary instead of the full log. Pixel works before that, when the agent looks for code: `pixel scope-task` names the files a task starts in, `pixel list-signatures` outlines a large file, and `pixel impact` lists a symbol's callers and callees, so the agent opens fewer whole files.

## Running both

Install both, in either order. In Claude Code, only one `PreToolUse` hook should rewrite a shell call, and `pixel setup` registers none: its hooks add the brief to a prompt, follow the task lifecycle and relay metrics, so RTK's hook stays the only rewriter. Pixel looks through an `rtk` prefix, so `rtk pixel impact …` still counts as a Pixel call.

## Choosing

Pick RTK if your agent's context fills with build and test output. Pick Pixel if it fills with files opened to find its way, and for callers, blast radius and Git history. If both happen, run both.
