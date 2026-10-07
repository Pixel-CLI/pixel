---
name: pixel-question-evidence
description: Use Pixel for bounded evidence when a code question asks for an exact symbol definition or export, or the files affected by a rename. Do not use for unrelated questions.
---

# Retrieve evidence for code questions

Use one direct Pixel query for the requested evidence. Do not run `classify` for these recognizable question types.

For an exact definition or export, search the literal identifier with a bounded result count, then read the actual declaration and nearby initializer from the source file. Treat matches as leads; a search snippet or import does not prove what a symbol exports.

```sh
pixel search-content -F '<identifier>' --context 2 --limit 20 .
```

For a rename or cross-file impact question, ask for one shallow upstream impact result, then cross-check literal references so tests, imports, and other textual consumers are not missed by the graph:

```sh
pixel impact '<identifier>' . --direction upstream --depth 1 --no-refresh
pixel search-content -F '<identifier>' --context 1 --limit 100 .
```

Read the relevant source lines for each candidate file and report only imports or calls that the task asks about. A graph path is a lead, not proof of a call edge or a complete inventory. If Pixel is unavailable, the graph is missing or stale, or the results do not answer the question, stop querying Pixel and use a bounded native search and source read. Never refresh or build an index to answer one question. Keep multi-part requests intact and verify every requested fact from source.
