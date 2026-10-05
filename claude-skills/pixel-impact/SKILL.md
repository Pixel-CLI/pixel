---
disable-model-invocation: true
name: pixel-impact
description: >
  Assess callers and change impact for a known symbol before a requested
  structural code change. Use only when caller completeness or blast radius
  matters.
license: MIT
---

# Pixel impact

Use this skill only when explicitly invoked or when the user asks for callers,
blast radius, or the impact of changing a known symbol. Ordinary repository
questions, textual lookup, known-file reading, and API research stay with the
usual tools.

Query the known symbol once against the existing graph:
`pixel impact '<symbol>' --no-refresh --depth 2 --json --metrics off`. Treat
results as candidates: cite their paths and lines, inspect the relevant
source, and account for the graph's open-world limits. If Pixel is unavailable,
does not accept `--no-refresh`, the graph is missing or stale, or the result is
empty or unhelpful, continue with native search immediately; do not repair,
refresh, or repeat the query.
