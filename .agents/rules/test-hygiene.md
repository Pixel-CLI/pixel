---
paths:
  - "crates/**/*.rs"
---

# Test Hygiene That Keeps Suites Deterministic

Loaded when a Rust source file is in play. Companion of `mutation-gate.md`.

- **Test modules come last in their file.** Every `#[cfg(test)] mod`
  (inline, or declared as `mod x;` with its body in another file) follows
  the file's last production item: the source walks in
  `crates/pixel-git/tests/boundary.rs` and `docs_drift` stop reading at the
  first test module, and `pixel/src/main.rs` once hid about 5 800
  production lines behind twelve interleaved ones (#528). A new test module
  goes to the end of the file, never next to the function it tests.
- **One git fixture helper per crate**, in `#[cfg(test)] pub(crate) mod
  testutil` at the crate root: `git()` with author/committer identity and
  `GIT_CONFIG_GLOBAL=/dev/null`, `init_repo()`, `commit(root, files, msg)`.
  Real `git` in a temp dir, never a mocked runner: the parsers wrap real
  plumbing output.
- **Environment variables are process-global.** Under `cargo test` the tests
  of one binary share a process. A test that sets a variable uses a name
  unique to itself (`PIXEL_TEST_FLAG_{pid}_{line}`) with a `// SAFETY:`
  comment, or takes the crate-wide mutex when the variable has one owner
  (`store::ENV_MUTEX` for `PIXEL_FLOW_DIR`). Never read `PATH`, `HOME` or
  a config path inside the function under test when a parameter can carry it.
- **Spawn the CLI through `support::pixel_command()`**, or set
  `current_dir` to a fixture: cargo runs a test from `crates/<crate>`, and a
  `pixel` started there with no path (or a missing one) appends its action
  log to this checkout's `.pixel/actions.jsonl`. CI fails the run when
  that file exists after the tests. It also points `HOME` at an empty
  directory (`support::neutral_home()`): a test that spawns the binary
  itself sets `HOME` too, or a machine with another release's
  `pixel setup` adds a stale-prompt note to every stderr it reads.
- **A fixture's commit is not unique.** Two fixtures with the same tree,
  committed in the same second by the same identity, get the same OID, and
  the base-shard cache (`$XDG_CACHE_HOME/pixel/shards`, else under the
  shared test `HOME`) then answers the second one. A test that asserts how
  an index was built (`base: built_from_git`) points `XDG_CACHE_HOME` at its
  own scratch directory: #309's contract test read `shared_cache` in a CI
  baseline and passed everywhere else.
- **Compare canonical paths.** macOS's temp dir is a symlink
  (`/var` to `/private/var`); anything that stores `root.canonicalize()`
  will not match `temp_dir().join(..)`. Canonicalize the expectation.
- **Fixtures are literal.** Build multi-line protocol fixtures (blame
  porcelain, NDJSON, `git log -z` records) from an explicit list of lines
  joined with `"\n"`, not from a continued string literal: an indentation
  or escape mistake there parses as valid input and tests the wrong thing.
- **A "must not happen" test rules out every form it takes.** When the
  claim is that X never occurs, assert the absence of each observable shape
  of X: no edge of any kind, no `unresolved_calls` row, no stray file, no
  line on stderr. #262 checked that an alias left no unresolved row while a
  false `References` edge would still have passed.
- **Assert the value, not that there is one.** `!is_empty()`, `is_some()`
  and `len() > 0` let most mutants through; compare with the expected value
  or the expected list (#222's preset tests only checked non-emptiness).
- **Test the path production takes.** A test that builds its object by hand
  when production reaches it through an opener, an adapter or a constructor
  (`open_repo`, the real transport) proves the hand-built one: #229's test
  wrote the download marker itself, so `open_repo` could stop writing it
  unseen.
- **A new input gets one case per path that consumes it.** Adding a site
  line, a scope or a name to a decision means one test through the full
  build and one through each incremental route that re-derives it
  (`change-propagation.md`); the full build alone passed on #262 while the
  incremental update lost the edge.
- **Exercise consecutive publications without a warm-up read.** A stale-read
  test must include a second watcher batch with no writer request between
  batches: the first batch must publish both its generation and envelope
  snapshot. Verified 2026-10-20 (#1203): single-batch tests passed while the
  second batch could still return no brief because its snapshot was missing.
