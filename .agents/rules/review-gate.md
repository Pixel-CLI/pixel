---
description: pre-push review loop — fetch the remote default, then fix every review-gate finding at CONCERN or above; rebase only on a conflict or a needed change
---

# Review gate before pushing

`pixel review-gate` is the deterministic pre-review: it diffs the change set
(uncommitted diff on a dirty tree; the whole branch diff against the
merge-base with the remote default on a clean one) and lists findings as
`BLOCKER`/`CONCERN`/`SUGGESTION`/`NIT` with file:line, witness and fix.

Before every `git push` of a feature branch, in this order:

1. `git fetch origin` — fetch the actual PR base (`origin/main` for
   main-targeted work; the immediate base for a stack) and diff against the
   fetched ref, never a stale local `main` or tracking ref. Do not rebase by
   default: `main` does not require branches to be up to date, the review and
   normal CI judge the merge-base diff, and every rebase re-runs the
   whole CI. Rebase only when the pull request conflicts with the base, or
   when the change needs something that landed on it since; then resolve
   conflicts hunk by hunk before the review runs.
2. `pixel review-gate .` — read every finding.
3. Fix each `BLOCKER` and `CONCERN` (the finding's `fix:` line names the
   move). `SUGGESTION` and `NIT` items are judgement calls — fix the cheap
   ones, record why the rest stay.
4. Re-run until no `BLOCKER` or `CONCERN` remains, then push.

`risk-climb` and `unresolved-callers-lower-bound` are SUGGESTION on purpose:
they describe what the change touches (a hub's blast radius, a name defined
twice), no edit clears them, and at CONCERN they alone refused 7 of the last 12
merged pull requests on that alone (#555). Read them as "review this part
as a whole", and say in the pull request that you did.

The tracked pre-push hook fetches the remote default, judges the branch
against its merge-base with it (a branch behind it is not refused), then runs
`pixel review-gate . --base <merge-base> --fail-on concern` and refuses the
push on any BLOCKER or CONCERN finding. A stacked branch pushes with
`PIXEL_MUTANTS_BASE=<immediate base>`: the baseline and the
review then judge the merge-base with that base, so the parent's commits stay
out. `git push --no-verify` is the explicit bypass.
