---
description: optional deterministic local review, with current-base evidence
---

# Optional local review

`pixel review-gate` is available when deterministic feedback helps diagnosis.
It is not required before publishing or merging a PR; pre-push runs no checks.
CI tests, lint and PR review remain required before merge.

For a useful local review, fetch the actual base and compare its merge-base
with HEAD (`pixel review-gate . --base <merge-base>`). Use the immediate parent
for a stacked branch. Rebase only on conflict or when a needed base change
requires it. Investigate findings instead of weakening the checks; report
which candidate was reviewed and any unresolved findings.
