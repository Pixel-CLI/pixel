# Change Propagation

> Issue and PR numbers (`#262`, …) in this file refer to Pixel's history, where each lesson was learned.

Always loaded: a change is finished when every path that produces or reads
the value it changed agrees with it, not when the path you started from does.

- **List every producer and reader before the first test.** When a change
  adds an input to a value — a scope, an alias, a cap, a phase, a filter, a
  route — run `pixel who-calls <entry point>` on the function that computes
  it and `pixel search-content` on the field it stores, and write the list
  down. Each path on it either takes the new input or says why it does not.
  Missed on #264: the unresolved-site diagnostic still called `decide`
  without a site line, and the reference prefilter `names_a_symbol` ignored
  the import's scope, although `resolve_calls` passed it. Missed on #254:
  `search-content -l` bypassed the stdout byte cap and printed a path once
  per root, although the line-printing path already did both.
- **A variant without the new input is a bug until it says otherwise.** A
  sibling that recomputes the value from what it had before (`decide` beside
  `decide_at`, a demotion that rebuilds a call's name from the target's
  `dst.name`) keeps the old behaviour on its path. Route it through the new
  input, or give it a doc comment naming what it cannot know and what it does
  instead. #262 lost the `leased()` → `push` edge on the next incremental
  update because `write_rows` demoted incoming edges under `sym.name`.
- **Stored rows that change need their version bump.** Rows an unchanged
  source now produces differently are stale until rebuilt: bump the version
  constant that gates them (`EXTRACTOR_VERSION` in `pixel-graph`, the concept
  extractor's version) in the same commit, with a line in its history
  comment. #255 shipped without it and every existing graph kept the old
  unresolved edges (fixed in #258).
- **A named constant is the only spelling.** When a constant exists for a
  path, a limit or a default (`GRAPH_DB_FILE`, `SEARCH_DEFAULT_ROWS`), every
  other site uses it; a literal copy drifts the day the constant moves (#222
  wrote the graph file name by hand in index bundles; the first version of
  #258 gave a filtered search 10 000 rows where the daemon default is 100).
- **A new secret reaches every sink masked.** A flag or positional that
  carries a credential is masked in the action log (`logged_args` in
  `crates/pixel/src/main.rs`), metrics, recall transcripts and error messages
  before it ships, with a test that reads the sink back. #222 recorded
  `config remote-key <preset> <key>` in clear text in `.pixel/actions.jsonl`.
- **Measure from where the cost starts.** A timer or a counter added to one
  route is placed around what that route actually spends, and a sibling
  route that skips a step records the step as absent, not as zero. #241 and
  #242 each needed a fix for a probe or an open timed outside its bracket.
