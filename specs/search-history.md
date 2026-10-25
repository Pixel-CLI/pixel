# History search

Status: normative for `pixel search-history`. The facts index it reads, its
freshness and its on-disk file are ARCHITECTURE.md, "Indexes and freshness" and
"On-disk state"; the wire op is `ops.md`.

`pixel search-history <QUERY> [PATH] [--facet message|path|diff|all] [--limit N]
[--read-only] [--json]` searches the commit messages, the paths and the added or
removed lines of a git repository's history. It is a built-in command: it stays
in the core binary and needs no plugin (`stays_in_core` in `pixel-plugin`), because
the brief's next step names it.

## Input

- `QUERY`: double-quoted phrases stay whole; every other word is a term split on
  anything that is not an ASCII letter, digit or `_`, and a term of one character
  is dropped (`sanitize_query`). Matching is case-insensitive.
- `PATH`: any path inside the repository (default `.`).
- `--facet`: `message`, `path`, `diff` or `all` (default). A value outside this
  list is refused by the argument parser, exit 2, before anything runs.
- `--limit`: the number of hits; the default and the ceiling are both 200, and a
  larger value is lowered to 200.

## Output

Text, one block per hit, best first:

```
<first 10 characters of the oid> <date> <subject> [<kind>]
    <path>                       (only when the hit has one)
    <up to six snippet lines>
```

and `no commits match` (exit 0) when there is nothing. `--json` prints one
document:

| Key | Content |
|---|---|
| `candidates` | the hits: `oid` (12-digit abbreviation), `at`, `subject`, `author`, `kind` (`message`, `path` or `diff`), `path` and `snippet` (null when the kind has none), `files_touched`, `score` |
| `facet`, `query` | the request as understood |
| `pass`, `degraded_to_or` | `"and"` and `false`: every term must match; there is no fallback to OR |
| `index_state` | `commits_indexed`, `total_commits`, `phase`, `fresh`, `diff_indexed_pct`, `diff_coverage_since`, `diffs_evicted`, `schema_version` |

- **Ranking.** A hit is the occurrence count of the terms, then the newer commit,
  then the smaller oid. One hit per `(oid, kind)`: a commit that matches by its
  message and by its diff appears twice, once per kind.
- **A snippet is cut at 500 characters** and says so
  (`… [snippet truncated at 500 chars]`); the full diff is `pixel diff <oid>`.
- **A result is a lower bound while `index_state.fresh` is false** or
  `diff_indexed_pct` is under 1: the diffs the index does not hold cannot answer.
  `index_state` is returned so a caller can tell an empty answer from an
  incomplete one; the facet `diff` is the one that suffers.

## Reading and building the index

| Mode | Behaviour |
|---|---|
| default | opens the facts index and catches up with the commits it lacks before answering; the first query on a repository builds it, and a daemon, when one runs, keeps the store open between requests |
| `--read-only` | answers from the index exactly as it stands: it never ingests, never creates the file and starts no warmer. With no index it exits 1 with `pixel: facts index absent or not built` and creates no `history.db` |

`search-history` does not change the working tree or the refs. Run by an agent it
counts as a read (`read_only_invocation`), so it is not an edit.

## Failure behaviour

A failure of the request (the index absent under `--read-only`, an index that
cannot be opened) is a `pixel: <reason>` line on stderr and exit 1. Observed on
this head: a one-commit repository with no match prints `no commits match`,
exit 0, and with `--json` an empty `candidates` and `index_state.fresh: true`.
The failure shape under `--json` was not exercised.

## Tests that prove it

`search_history_cli` (a term is found by the facet that indexes it and by no other,
the text shape, `no commits match`, `--read-only` on a repository never built, the
command is the core's and not a moved one), `history_text` tests in `main.rs`
(six snippet lines at most), the `search` tests in `pixel-facts` and
`contract_tests` in `pixel-daemon` (the same error text over the wire), and
`specs_drift` (the facets above are the ones `--help` lists).
