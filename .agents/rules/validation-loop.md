---
description: publish promptly, keep local diagnosis optional, validate the PR head in CI
---

# Candidate validation loop

- Work one reviewable concern per PR. Start from the fetched actual base;
  stacked work uses its immediate parent. Rebase only on conflict or when
  the work needs a newer base change.
- Local compilation, tests, lint, review and installation are optional.
  Choose a focused check when it helps diagnosis; no local result is needed
  to publish. Do not add a full local suite or rebuild as a ritual.
- Commit and publish the candidate, then let CI run the required checks.
  The pre-push hook runs no checks. A PR is ready to merge only after CI
  validates its current head and review findings are addressed.
- If a local check is useful, keep Cargo writers sequential per target,
  preserve the tested snapshot, and record SHA, command and result. Do not
  claim that an older passing result validates later changes.
- Mutations and coverage execute only in scheduled main CI. Mutations cover
  unjudged commits; coverage skips a SHA already measured successfully.
  Neither is a PR publication gate.
- Diagnose CI failures precisely. A mutation baseline failure is not a
  surviving mutant; distinguish missing results, infrastructure, timeouts
  and actual survivors before changing code.
