# Audited nightly contract tests — #1070

Source baseline: `33a844cc22798654288604597c014d10b889295a` (`origin/main`, fetched 20 October 2026). One consolidated PR was explicitly requested; no child issues were created.

## Closed historical set

- [39006805262](https://github.com/Pixel-CLI/pixel/actions/runs/39006805262/job/117328982996), scheduled 17 October, head `b128f0aceea3427de83a33a4b001b67f3e6c00c3`: 2,003 complete recorded outcomes, 1,513 caught, **294 missed, 2 timeout**, 194 unviable, zero missing.
- [38689277239](https://github.com/Pixel-CLI/pixel/actions/runs/38689277239/job/116921178882), scheduled 15 October, head `75f92e92bd748d5dcc57d54544b202fa77be0405`: incomplete campaign. Only the two files explicitly required by #1070 are included: **71** `execution_brief/region_pack.rs` and **12** `extract/ruby_routes.rs` MISSED outcomes. Neither file occurs in the 17 October set.

The [machine-readable inventory](mutants-contracts-1070.json) retains all **379** unique `(run, path, line, column, mutation)` identities. `base_line` refers to the source baseline above, not to line numbers after adding these tests. Suite filters locate the related contracts; they are not a per-mutant replay result.

The complete campaigns of [18 October](https://github.com/Pixel-CLI/pixel/actions/runs/39102034431) (84 missed, 1 timeout) and [19 October](https://github.com/Pixel-CLI/pixel/actions/runs/39208435357) (10 missed) are newer, but their cumulative diffs are different. Their counts are not added here.

| Crate | Historical cases | Contract coverage |
| --- | ---: | --- |
| `mvp-index` | 167 | Ruby extraction, RSpec frames/shared groups/let chains, nullable values, fixtures, stubs, ancestor insertion order and lookup uncertainty, Git answers and refresh failure |
| `pixel` | 132 | BM25 numeric scores and symbol ties, region priorities/test bodies, source snapshot selection, hook delivery/cache boundaries and deadlines, history progress, probe group safety |
| `pixel-facts` | 11 | Accumulated phase timings, exact blob cap on both sides, bounded argv batches |
| `pixel-git` | 3 | Waiting without reaping, child versus nonchild errors |
| `pixel-graph` | 24 | Ruby source scopes, real MVP pairing, recorded merge restoration/reapplication, duplicate row fingerprints, Rails route limits and targets |
| `pixel-plugin` | 2 | Manifest I/O classification; existing regular-file/link/FIFO protections remain active |
| `pixel-recall` | 1 | Shared model-cache path outside the repository |
| `pixel-setup` | 31 | JSON token preservation, install selection, receipt error/path validation, skills, alternate executables, Pi removal and Antigravity skill validation |
| `mvp-store` | 8 | Refreshed/still-current index publication, cleanup and resolved caller rows |

## Progress and equivalent cases

- The two reported TIMEOUT sites were manual progress updates: the value scanner now uses the existing child iterator, and nested RSpec groups use an advancing queue with cycle rejection. Neither depends on a mutable index increment to advance.
- A combined Git answer validates its two nonempty paths and single root line through a pure predicate. Byte batching and history-progress boundaries have pure testable decisions too.
- The non-Unix `git_index::key` fallback is skipped narrowly: no supported release/test target compiles it, and it only disables index copying there. The Unix key remains subject to tests.
- The BM25 scheduling threshold alone is skipped. Empty input is handled before it; positive worker counts and nonempty chunks make either decision preserve document values and input order. Counting/ranking remain under mutation testing, and ordinary tests cover both sides of the threshold.
- The disjoint OS flag unions in plugin opening and `waitid` use `BitOr` rather than an operator whose OR→XOR replacement is equivalent. The I/O functions themselves remain under tests.
- Existing size-only blob protocol tests, added after the audited head, already exercise newline/trailing-CR rejection, empty/missing objects and capped responses; the new tests add exact caps and aggregation.

No file format, extractor version, command, installation layout or wire contract changes. The production changes preserve the existing decisions while exposing their boundaries or removing progress hazards. No changelog fragment is needed for these test/refactor-only changes, and no architecture section moves.

## Ordinary validation

The targeted command is:

```sh
cargo test -p mvp-index -p mvp-store -p pixel-cli -p pixel-facts -p pixel-git -p pixel-graph -p pixel-plugin -p pixel-recall -p pixel-setup audited
```

Formatting: `cargo fmt --all`. Lint/types: the same nine package selectors with `cargo clippy … --all-targets -- -D warnings`. CI validates the complete workspace on the PR head; a local result does not stand in for that head's CI.

## Requested negative controls

One selected observable contract per crate was temporarily removed, its ordinary test run, and the exact saved source restored in a `finally` block. All nine returned exit **101 with an assertion/test failure**, not a compiler error. The canonical tests were then rerun after restoring the sources. These are nine selected controls, not 379 independently replayed mutation verdicts.

### pixel-cli: BM25 document-frequency weight

`cargo test -p pixel-cli audited_bm25_should_preserve_exact_scores_document_frequency_and_length_normalization -- --nocapture`

```text
0.7268042347843697
exit: 101 (expected test failure)
```

### pixel-setup: untouched array separators

`cargo test -p pixel-setup array_edits_should_consume_duplicates_once_and_preserve_only_adjacent_separators -- --nocapture`

```text
called `Result::unwrap()` on an `Err` value: Error("JSON edit changed unrelated values", line: 0, column: 0)
exit: 101 (expected test failure)
```

### pixel-recall: shared model-cache location

`cargo test -p pixel-recall models_should_live_in_the_shared_user_cache_outside_the_repository -- --nocapture`

```text
assertion failed: actual.ends_with(".local/share/pixel/models")
exit: 101 (expected test failure)
```

### pixel-graph: duplicate row accounting

`cargo test -p pixel-graph row_fingerprints_should_count_duplicate_rows_without_collapsing_them -- --nocapture`

```text
assertion `left == right` failed
  left: 0
 right: 2
exit: 101 (expected test failure)
```

### pixel-facts: argv byte cap

`cargo test -p pixel-facts audited_tree_batches_should_include_exact_byte_and_path_caps_and_always_advance -- --nocapture`

```text
assertion `left == right` failed
  left: 2
 right: 1
exit: 101 (expected test failure)
```

### pixel-plugin: I/O error classification

`cargo test -p pixel-plugin manifest_io_errors_should_remain_io_errors_instead_of_invalid_manifest_errors -- --nocapture`

```text
assertion failed: matches!(load(&file, None), Err(Error::Io { .. }))
exit: 101 (expected test failure)
```

### pixel-git: non-child wait error

`cargo test -p pixel-git observing_a_nonchild_should_return_the_os_error -- --nocapture`

```text
assertion failed: wait_for_child_exit(u32::MAX).is_err()
exit: 101 (expected test failure)
```

### mvp-store: index publication preconditions

`cargo test -p mvp-store audited_git_index_should_publish_only_a_refreshed_still_current_copy -- --nocapture`

```text
assertion `left == right` failed
  left: true
 right: false
exit: 101 (expected test failure)
```

### mvp-index: complete child traversal

`cargo test -p mvp-index scanning_should_keep_all_siblings_but_not_locals_of_nested_scopes -- --nocapture`

```text
assertion `left == right` failed
  left: []
 right: ["first", "second", "third"]
exit: 101 (expected test failure)
```

## Limits

No local/PR/manual cargo-mutants campaign was run, no installation was changed, and no merge was requested. The 379 exact historical cases have **no new aggregate mutation verdict** in this PR. Scheduled CI mutates production diffs; unchanged historical sites do not automatically run again merely because their tests changed. Therefore even the next green nightly must be read against its actual listing, not presented as proof that all 379 historical cases were killed.
