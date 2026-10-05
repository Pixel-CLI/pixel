---
name: validation-loop
description: "Run Pixel's efficient Rust edit-to-PR validation workflow: choose scoped local feedback, freeze one candidate, use the remote mutation gate, and triage failed gates precisely. Use when implementing Rust changes, preparing a pull request, or reducing compile and mutation-fix round trips."
---

# Pixel validation loop

Use this skill with the always-on `validation-loop.md` rule. The rule defines
the obligations; this procedure chooses the smallest useful check at each
stage.

## 1. Establish one candidate

Find or create the tracked task, use a clean branch/worktree from the fetched
actual base. Rebase later only on a conflict or a needed change on the base. Keep one behavior change in one
pull request. A stacked branch uses its immediate base, supplied to the remote
gate with `PIXEL_MUTANTS_BASE` when needed.

## 2. Get the first failure locally

After a Rust edit, choose the narrowest check that can disprove it:

| Change boundary | First signal |
| --- | --- |
| One crate | `cargo check -p <crate>` |
| Finished behavior | Its focused contract and consumer tests |
| Cross-crate interface | Checks and tests for the producer and each changed consumer |
| Workspace/lint/config input | The affected Clippy or full local gate |

Use a watcher only when it owns the same scoped check and test. Do not run a
second Cargo writer in the same `target/`. A red result identifies the edit to
fix; do not push it merely to learn whether CI agrees.

## 3. Freeze and validate

Commit the reviewable candidate. Run `scripts/gates.sh` once for Rust-affecting
work (use `--force` only when a non-Rust input can affect compiled tests). Keep
long runs on an unchanged checkout or a committed separate worktree with its
own `target/`; retain the SHA, command, complete log and exit status.

Review the mutant surface in the committed diff. Do not run a full local
mutation campaign: the pre-push hook sends the exact committed diff to the
remote gate host with a warm outcome cache. A bounded single-function local
run remains an explicit request only.

Fetch and run `pixel review-gate` as required by `review-gate.md`.
Push the same candidate once. For Rust changes, the hook performs the
all-target baseline compile before the remote mutation campaign; that baseline
is a required rung, and the remote verdict is trusted only for a candidate
that passed it. A green push has already received the remote mutation verdict;
CI independently validates the current PR head.

## 4. Triage instead of retrying blindly

| Result | Next action |
| --- | --- |
| Baseline failed / never judged | Run the relevant local check, fix the compilation or test failure, then revalidate the candidate. |
| `MISSED` | Add an assertion on the observable contract that fails under that exact mutation, or use a documented narrow skip only when the rule permits it. |
| `TIMEOUT` | Bound the test or loop so a broken behavior fails quickly; do not treat it as a passed mutant. |
| CI ran an older SHA | Wait for or trigger the workflow for the current candidate; older green runs are evidence only for their own SHA. |
| Remote host unavailable | Repair the host or use the documented explicit bypass; CI still remains required. |

Watch CI in the background while advancing independent work. Before calling a
PR ready, verify all required workflows completed for its current head.

## 5. Improve the loop with evidence

Measure first edit to first useful failure, first edit to a fully validated
PR, push count, and remote baseline/mutant duration. Record a run identity,
command and baseline beside each number. Change one part of the workflow at a
time, then compare against that baseline.
