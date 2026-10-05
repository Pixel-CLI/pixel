---
description: publish one current, validated candidate instead of using CI as the first compiler
---

# Candidate validation loop

This rule is always loaded. It makes validation results useful before a pull
request exists.

- Work one concern at a time. A branch and pull request represent one frozen
  candidate; requested fixes update that candidate rather than opening another
  pull request for the same change.
- Start from the fetched actual base. For main-targeted work that is
  `origin/main`; stacked work uses its immediate base. Do not use a local
  `main` or tracking ref until it has been fetched. Do not rebase a pushed
  candidate just to catch up with its base: each rebase re-runs the whole CI.
  Rebase on a conflict, or when the change needs something newer on the base
  (`review-gate.md`).
- While editing Rust, run the smallest compilation and contract/consumer tests
  that cover the changed behavior. A compile failure is a local fix, never a
  reason to wait for CI or the mutation gate.
- Only one Cargo command writes to a `target/` at a time. Parallel checks use
  separate build directories and an explicit CPU and memory budget.
- Freeze a reviewable unit in a commit, then run its full applicable local
  gates once. Record the SHA, command, complete log and exit status. Reuse the
  result only while the candidate and gate inputs remain unchanged; rerun after
  a behavior- or gate-affecting edit.
- Before pushing Rust, the tracked hook fetches the base, requires
  `cargo check --all-targets`, then runs the remote mutation campaign against
  that exact base and candidate. Do not bypass it to discover ordinary compile
  failures in CI. CI remains the independent merge verdict. The baseline rung is
  required before the remote verdict is trusted: a push blocked on a compile
  failure carries no remote verdict, and a remote verdict counts only for a
  candidate whose baseline passed.
- Classify red results before editing: a mutation **baseline failure** means
  compilation or tests failed before mutants were judged; `MISSED` needs a
  contract test or justified skip; `TIMEOUT`, unviable and infrastructure
  failures have distinct remedies. Never report an unjudged baseline as a
  mutant survivor.

For the operator procedure, load `validation-loop`. For mutation-specific test
shape, follow `mutation-gate.md`; for long-run isolation and evidence, follow
`test-campaigns.md`.
