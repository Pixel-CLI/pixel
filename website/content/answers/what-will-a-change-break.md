---
title: "How do I know what a change will break before the agent edits?"
description: "Finding the callers of a function before a coding agent changes it: what a code graph answers, how many callers it found on real repositories, and what it cannot see."
answer: "what-will-a-change-break"
---

<!-- Figures: data/answers.toml. Every number here must be in the /benchmarks/ sections the entry names; the build fails otherwise. -->

## Ask before the edit, not after the tests

An agent that changes a function's signature, its return value or what it throws needs to know who depends on it. Without that list it edits, runs the tests, and fixes what failed; whatever the tests do not cover ships broken. A search for the name finds some callers and many lines that are not calls.

## What the graph answers

Pixel parses the repository with tree-sitter into a graph of symbols, imports and calls. `pixel who-calls` returns the functions that call a symbol, `pixel impact` follows them upstream to the blast radius, and `pixel call-path` shows how one function reaches another. Each answer says how complete it is: a call the graph could not resolve is reported as such rather than dropped, and the answer carries a warning that static analysis never sees every dynamic call.

Run it before the edit and give the list to the agent as the scope of the change, or let the agent run it: the protocol `pixel setup` deploys tells it to.
