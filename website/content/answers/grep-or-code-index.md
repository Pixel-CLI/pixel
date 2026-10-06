---
title: "Grep or a code index for a coding agent?"
description: "What grep does well for a coding agent, what it costs once the agent opens the file behind a match, and where a code index answers instead."
answer: "grep-or-code-index"
---

<!-- Figures: data/answers.toml and data/read_savings.toml. Every number here must be in the /benchmarks/ sections the entry names; the build fails otherwise. -->

## What grep is good at

Grep, ripgrep and the agent's own search tool find a string anywhere, right away, with nothing to install or keep up to date. For "where is this error message" or "which files mention this flag" nothing beats it, and Pixel keeps it: `pixel search-content` takes the same regular expressions.

## Where it stops

A match is a line, not an answer. To learn what the function around it does, what else the file defines or who calls it, the agent opens the whole file, and on a large file that read costs far more than the search that led to it. Grep also cannot tell a call from a comment, a string or another symbol with the same name: every line that spells the name comes back.

A code index answers those questions directly. `pixel list-signatures` gives a file's definitions without its bodies, `pixel who-calls` and `pixel impact` give callers from a graph parsed with tree-sitter, and `pixel find-symbol` goes to a definition by name.

## Use both

Keep grep for strings and a code index for structure. Pixel's bundled agent prompt, which you can copy into an agent's instructions, says which `pixel` command replaces which search or read, and when the native tool is still the right one.
