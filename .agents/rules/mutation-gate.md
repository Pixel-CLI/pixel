---
paths:
  - "crates/**/*.rs"
---

# Code That Passes the Mutation Gate on the First Run

Loaded when a Rust source file is in play. The gate is `cargo mutants --in-diff`,
run first by the remote pre-push campaign and again by the CI `Mutants` job
(90-minute limit) on every PR; it is not run on the laptop, so the code must
come out clean before a push and remain clean in CI.

For every function with a line in the diff, the gate generates the
mutants that sit on the changed lines (operators, match arms, guards) plus
the replacement of the whole body (`Ok(Default::default())`, `vec![]`, `()`).
A one-token change in an untested function is therefore enough to get its
body replaced, and a reformatted line brings every operator on it. Measure
the exposure before pushing, in seconds and without building:
`git diff <base>...HEAD > target/pr.diff && cargo mutants --list --in-diff
target/pr.diff` (a file, not `<(…)`: fish has no process substitution),
then read it line by line: every listed mutant names the test that fails
under it, or gets one before the push: each failed remote or CI run costs a
round trip, and 42 of 121 failed from 2026-09-21 to 26. Two
settings in `.cargo/mutants.toml` shape the answer: `test_workspace = false` runs only
the mutated crate's tests (a CLI contract test never kills a library
mutant), and `crates/*/build.rs` is excluded: cargo build scripts only, so
`crates/pixel-graph/src/build.rs` stays under the gate. Rules that make the
first `cargo mutants` run come back clean:

- **Read the function's tests before touching it.** No test that would fail
  if the body were replaced by `Default::default()`? Write one first, on the
  observable contract (returned value, written file, emitted line), then edit.
  The budget of a "lint only" change is the missing tests, not the lint.
- **Give every loop a bound a test can set.** A function that loops "until
  fresh/ready/done" gets a sibling taking the wall-clock cap
  (`ingest_until_fresh_within(store, opts, cap)`); production calls it with
  the production cap, tests with a cap under 5 s. A mutant that breaks the
  loop body then fails in seconds instead of hanging until the timeout.
  Same for scanning loops: always advance past the current item so a wrong
  bound cannot spin (`pos = close.max(start) + tag.len()`).
- **Keep the test cap under 20 s.** CI runs cargo-mutants with its automatic
  timeout: five times the baseline test time, never below 20 s. A test that
  waits 30 s for a broken loop is reported TIMEOUT in CI while it passes
  locally with `--timeout 300`. Run locally with `cargo mutants --timeout 60`
  (`--timeout 20` also caps the baseline, which the doctest rustdoc compile
  alone pushes past 20 s on this workspace) and keep every test's own wait
  under 20 s.
- **Iterate with `for`, never with a hand-advanced index.** `while j < n {
  …; j += 1 }` has three survivors per increment (`-=`, `*=`, and the
  comparison) and a `-=` one is an infinite loop that costs the full
  timeout. Precompute the block boundaries (`ruby::blocks(lines, is_start)`)
  and `for line in &block[1..]`: a wrong slice bound gives a wrong record a
  test can see, never a hang.
- **Fake servers poll with a deadline.** A test that `accept()`s blockingly
  hangs forever under a mutant that never connects. `set_nonblocking(true)`,
  loop with a 5 s deadline, return on expiry so the assertion fails instead.
- **Put a seam where the code meets the outside.** Spawning a browser, a
  daemon socket, `PATH`, the embedding model: hide the call behind a trait
  (`Browser { run, pause }`), a parameter (`find_in_paths(name, path)`),
  or a pure helper (`script_block_bounds(text, start)`), and test the seam.
  `#[cfg_attr(test, mutants::skip)]` is for the one-line adapter over the
  real process, with a one-line reason, never for the logic behind it.
- **Name a comparison that a test cannot reach.** `t < cutoff` buried in a
  loop over git output becomes `is_stale(committer_unix, cutoff)` and gets
  the four cases: below, at, above, unknown. The equality edge is the one
  the gate flips (`>` to `>=`): a blob exactly at the cap is not over it, a
  branch exactly at the cutoff is not stale, a tie keeps the first region.
- **Write literal constants without operators.** `1 | 2 | 4` and
  `256 * 1024` survive as `^` and `+` because nothing can tell the values
  apart; `0b111` and `262_144 // 256 KiB` leave nothing to mutate.
- **Time helpers get the bracket test.** For `now_ms`/`now_unix`/`iso_now`:
  read the clock, call the helper, read it again, assert the value sits
  between the two and above a fixed floor (2020-01-01). It kills `0`, `1`
  and `"xyzzy"` at once.
- **A survivor that shows the code is wrong is a bug report.** When the test
  written for a mutant proves the function never worked (a `git cat-file
  --batch-check <object>` that git rejects, so every blob measured 0 bytes),
  fix the bug in its own PR with a `changelog.d/` fragment, below the PR that
  found it. Do not bend the test to the broken behaviour.
- **Fix from the push-gate or CI report, verify locally only per function.** Read
  the `MISSED`/`TIMEOUT` lines (the `local` push gate reports them before a blocked
  push; `Mutants in diff` gathers every CI shard's), write the test, and if asked to
  check before pushing run `cargo mutants --in-diff <diff> -F <function>`
  (minutes). Never the full in-diff run: it is the job's work. Never edit
  the tree while a run is in flight: it mutates files in place. After a
  killed or crashed run, `grep -rl "changed by cargo-mutants" crates/` and
  restore before doing anything else.
