---
name: validation-loop
description: "Publish Pixel PRs promptly with optional local diagnosis and required CI validation."
---

# Pixel validation loop

Follow the always-on `validation-loop.md` rule. Local checks are optional;
mutation and coverage campaigns belong only to scheduled CI on main.

1. Find or create the tracked task and prepare one reviewable candidate from
   the fetched actual base. Use the immediate parent for a stacked PR.
2. Choose local tests, compilation, lint or deterministic review only if they
   help diagnose a problem or reduce uncertainty. For a focused run that
   stops at the first failure, use `cargo nextest run -P fast -p <crate>`
   (stable; the default profile has no fail-fast). `scripts/gates.sh` remains
   available for a deliberate full local run. No local run is required.
3. Commit, push and open the PR. State checks actually performed and checks
   deferred to CI. Rebuilding, reinstalling, indexing and doctor are optional
   unless local deployment was requested; use AGENTS.md's safe procedure then.
4. Watch CI in the background while doing independent work. Fix failures on
   the current candidate; verify the final head SHA before reporting green.
   Tests, lint and required reviews remain prerequisites to merge.
5. For nightly mutation findings, distinguish a failed baseline from a
   survivor, missing results, timeouts or infrastructure failures. Fix an
   observable contract with an ordinary test; the next main nightly verifies
   mutations. Do not launch local or manual mutation campaigns.
