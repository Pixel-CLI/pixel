# Long Test Campaigns

Always loaded: how to run the long gates without losing an afternoon.

- **Local campaigns are optional.** Use a focused local test when it helps
  diagnosis; full suites are CI's responsibility before merge. For a chosen
  long local check, use an unchanged snapshot, one Cargo writer per target,
  and record its SHA, command, log and exit status. Do not clean active builds.
- **Mutants run only in scheduled CI on main.** No local, PR or manual
  campaigns. Diagnose survivors with ordinary tests and inspect the next
  nightly's report after merge.
- **Count against the merge base, with three dots.** `git diff
  origin/main...HEAD` diffs the merge base against HEAD — what the branch
  changed. `git diff origin/main..HEAD` diffs the two commits and adds
  everything `main` gained since you branched; `git diff origin/main`, with
  no second revision, compares `main` to the working tree instead, so it adds
  uncommitted edits on top. Either of the last two has you mutating code you
  did not write: 113 mutants where the PR owed 103, ten of them in a file
  another pull request had just rewritten.
- **`--in-diff` compares the diff to the working tree, not to HEAD.** With an
  uncommitted edit it prints `Diff content doesn't match source file` and
  lists zero mutants, which reads like good news. Commit first, then write
  the diff.
- **Do not throttle the CLI suite.** `RUST_TEST_THREADS=4` made the
  process-spawning `pixel-cli` tests four times slower per mutant; the
  default thread count is right, and a flaky baseline is re-run, not
  worked around.
- **Watch the disk.** Every mutant rebuilds incrementally; `target/debug`
  reached 34 GB and the run aborted on a full disk, leaving a mutated file
  behind. `df -h .` before a run; `target/debug/incremental`, `target/release`
  and `target/dev-release` are safe to delete between campaigns. `just disk`
  prices every worktree's build output, index and scratch in one list, and
  `just clean` takes the build output of all of them (CONTRIBUTING.md,
  "Reclaiming disk"); a `mutants.out` left by an aborted run is in that list.
- **Finish with a daemon check.** `pgrep -fl "target/.*/pixel daemon"` must
  print nothing; a fixture that left a daemon serving it is a test bug.
