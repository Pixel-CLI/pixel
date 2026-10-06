Closes #<issue number>

<!-- `Closes #<n>` links this PR to its issue: GitHub closes the issue on
merge and board-sync.yml moves it on project 3. Use `Refs #<n>` when this
PR does only part of the issue. Only a typo fix, a CI rerun or an
emergency revert goes without an issue: then replace the line with why.
Keep prose brief; delete these comments. See CONTRIBUTING.md,
"Commits and pull requests". -->

## Summary

<!-- One or two sentences: what changes and why (the bug reproduced or the
user-visible effect). Then, when it helps, the smallest sketch that makes
the point: a call tree, a file tree or pseudocode, as a `diff` when the
shape already exists. Build a call tree from `pixel impact <symbol>`, not
from memory. -->

## Evidence

<!-- Before / after: the test that fails without the change and passes
with it, or the output with the command that produced it. Say what CI ran
and what you ran locally, and name what was not run (e.g. the musl
cross-build, the smoke test, an installed-hook check). -->

- **Before:**
- **After:**

## Merge Danger

<!-- Door: two-way when a revert undoes it; one-way when it does not —
e.g. EXTRACTOR_VERSION or PROTOCOL_VERSION bumps, a file format under
.pixel/ or ~/.pixel/, what `pixel install` writes into agent configs,
a release tag, a deleted or rewritten user file.
Blast radius: one word, then what a bad merge would break and for whom. -->

**Door:**

**Blast radius:**

## Docs touched

<!-- changelog.d/, ARCHITECTURE.md, agent prompt, README — or why none. -->
