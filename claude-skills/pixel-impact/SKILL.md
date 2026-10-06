---
disable-model-invocation: true
name: pixel-impact
description: >
  Find callers or change impact for a named code symbol across files or
  call levels. Skip routine lookup, local renames, and broad implementation
  or policy questions.
license: MIT
---

# Pixel impact

Use Pixel for an unresolved caller or blast radius question about a known
symbol. When a private helper and its references are already visible in one
file, native search is sufficient. File listing, literal lookup, UI-label
renames, API research, and general explanations stay with the usual tools.

For a relationship question spanning files or call levels, query the symbol
once: `pixel impact '<symbol>' --no-refresh --depth 2 --json --metrics off`.
Use `--depth 1` when only direct callers are requested. Treat graph edges as
candidates and inspect the cited source; empty or incomplete results do not
prove that no callers exist. Distinguish production callers, test callers,
and textual references in the answer, citing the requested evidence.

If Pixel is unavailable, rejects `--no-refresh`, or returns missing, stale,
ambiguous, empty, or unhelpful results, continue with native tools immediately.
Do not repair, refresh, retry, or expand the query during the task. Loading
this skill does not require a Pixel call when the source already answers it.
