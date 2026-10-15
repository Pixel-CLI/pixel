# Abstention of a whole-change negative (`evaluate diff-reaches`, d2-5)

Measured 2026-09-26, before Delivery 2 of `pixel evaluate`
([design note](../archive/design/evaluate.md), decision C3). The question is how
often `diff-reaches` would have to answer `unknown` rather than
`absent_in_snapshot` because part of a change has no symbol in the graph. If
the answer is "almost always", the whole-change promise leaves no real use
and the note's own rule applies: narrow the predicate to mapped symbols,
never hide the omissions.

## Protocol

For each of the last *n* non-merge commits of a repository, oldest first:
check the commit out in a dedicated worktree, then run

```
pixel what-changed --base <commit>^ --json .
```

which refreshes the graph to the checked-out tree and maps the commit's diff
onto it (`changes::detect`). A commit **abstains** when the report carries a
non-empty `uncovered_changes` or `unanchored`: under C3 either one turns any
`diff-reaches` negative into `unknown`. The scripts are in
[`scripts/bench/bench-abstention/`](../../scripts/bench/bench-abstention/):
`run.sh <name> <tree> <n>`, `rerun.sh` for checkouts that failed, and
`aggregate.py <name>=<clone> …`.

Binary: `pixel` 0.5.1 built from `4f40fdc`. Its `pixel-graph` and
`pixel-daemon` are identical to `main` at `10b9dbd`
(`git diff --stat 4f40fdc 10b9dbd -- crates/pixel-graph crates/pixel-daemon`
prints nothing).

| repository | language | commits | window | tip |
| --- | --- | ---: | --- | --- |
| pixel | Rust | 200 | 2026-09-15 → 09-26 | `10b9dbd` |
| a private Rails monolith | Ruby | 200 | 2026-09-22 → 09-26 | — |
| [GitNexus](https://github.com/abhigyanpatwari/GitNexus) | TypeScript | 200 | 2026-08-27 → 09-21 | `7376347` |
| [openclaw](https://github.com/openclaw/openclaw) | TypeScript | 50 | 2026-09-25 → 09-26 | `f63fea5` |

openclaw was cut at 50 commits because each call cost about 100 s (see
Cost below). "With a changed symbol" restricts to commits where
`symbols_total > 0`: 123, 175, 130 and 38 commits respectively.

`outside_symbol` residues are split by reading the lines at the commit (new
side) or its parent (old side):

- **test code**: a test path, or Rust lines after the file's `#[cfg(test)]`;
  the extractor keeps tests out of the runtime graph on purpose
  (`rust_is_test_container`);
- **comments and blank lines**;
- **imports and attributes**;
- **other module-level code**, which includes mixed blocks.

`unsupported_language` is split by file kind.

## Results

| abstention, all commits / commits with a changed symbol | pixel | Rails | GitNexus | openclaw |
| --- | --- | --- | --- | --- |
| strict rule (every motif downgrades) | 100 % / 100 % | 68 % / 66 % | 98 % / 99 % | 96 % / 95 % |
| docs no longer downgrade | 86 % / 98 % | 62 % / 61 % | 94 % / 98 % | 96 % / 95 % |
| + test code | 76 % / 89 % | 62 % / 61 % | 92 % / 95 % | 78 % / 82 % |
| + comments and blank lines | 64 % / 69 % | 62 % / 61 % | 91 % / 94 % | 78 % / 82 % |
| + imports and attributes | 60 % / 63 % | 62 % / 61 % | 90 % / 92 % | 68 % / 71 % |
| + style and assets | 56 % / 62 % | 60 % / 60 % | 90 % / 92 % | 68 % / 71 % |
| narrowed to mapped symbols: only `unanchored` downgrades | 10 % / 15 % | 8 % / 9 % | 16 % / 25 % | 36 % / 42 % |

| motif, share of commits where present | pixel | Rails | GitNexus | openclaw |
| --- | --- | --- | --- | --- |
| unsupported_language: docs | 80 % | 15 % | 28 % | 26 % |
| outside_symbol: test code | 49 % | 5 % | 64 % | 82 % |
| outside_symbol: comments, blank lines | 46 % | 9 % | 53 % | 42 % |
| outside_symbol: imports, attributes | 26 % | 0 % | 42 % | 62 % |
| unsupported_language: style, assets | 12 % | 12 % | 0 % | 2 % |
| outside_symbol: other module-level code | 26 % | 8 % | 58 % | 52 % |
| unsupported_language: config | 31 % | 22 % | 59 % | 12 % |
| unsupported_language: templates | 0 % | 38 % | 0 % | 0 % |
| unsupported_language: SQL | 0 % | 8 % | 0 % | 0 % |
| unsupported_language: shell | 10 % | 1 % | 2 % | 2 % |
| unanchored (deleted symbols) | 10 % | 8 % | 16 % | 36 % |
| non_text_change | 4 % | 4 % | 1 % | 0 % |

`excluded_by_file_cap` never appeared. openclaw's graph held 43 839 files,
under `DEFAULT_GRAPH_MAX_FILES` (50 000), although git tracks 50 117.

## Reading

- Under the strict rule `diff-reaches` could almost never say "absent" on
  three of the four repositories. It stays unusable even after relaxing every
  category that cannot move a call path (docs, tests, comments, imports,
  attributes, assets): 56 % to 90 % of commits still abstain. What remains is
  legitimate. Module-level code outside any symbol (constants, CLI builder
  chains) and configuration (`package.json`, `Cargo.toml`, CI workflows) can
  change behaviour, and the call graph cannot see either. No tuning of the
  motif list saves the whole-change promise.
- Narrowed to the changed symbols the graph maps, only deleted symbols still
  block a negative: 8 % to 42 % of commits. The unmapped portions stay listed
  in the answer instead of downgrading it.
- The Rails monolith fares best under the strict rule: most of its changes sit
  inside methods. It loses them to ERB templates and SQL, which no extractor
  maps.

## Cost

On openclaw, the first `what-changed` (a full build of 43 839 files) took
769 s. Each later commit took 12 to 20 s of incremental graph update and about
100 s end to end. The design targets < 300 ms for an agent call on the
reference repository, so on a repository of this size the command belongs to
neither a synchronous hook nor a tight agent loop.

## Caveats

- The windows are short: four days for the Rails monolith, and 50 consecutive
  commits for openclaw.
- The unit is a commit, not a pull request. A pull request's diff usually
  spans more files than any one of its commits, so its strict-rule abstention
  is likely higher, though commits that undo each other can shrink it.
- The split of `outside_symbol` is a line-level heuristic. A block that mixes
  imports, constants and comments counts as other code, which overstates what
  a relaxation could recover rather than understating it.
- A measurement artefact was found and removed. The pixel under test appended
  `.pixel/` to tracked `.gitignore` files, which added one spurious
  `unsupported_language` entry to the commits it touched and blocked 168
  checkouts. The blocked checkouts were re-run with `checkout --force`, and
  the aggregator drops `.gitignore` entries for commits that do not change
  the file. The behaviour itself is fixed in #307.
