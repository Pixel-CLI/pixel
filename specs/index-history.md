# Text index and history

Status: normative for `pixel-index` and the git runner `pixel-io`.

## pixel-io: the git runner

All git access goes through `GitRunner` (lifted `pixel-git`): timeout, output
cap enforced during the read, redacted stderr, `cat-file --batch`, typed
plumbing (HEAD, branch, porcelain status), `nofollow` (files opened with
`O_NOFOLLOW`, a fresh temp file renamed over the name, permissions set on the
descriptor), `sidecar` (the trust boundary of `.pixel/`: refuses a `.pixel` that is
a link or that git tracks, creates owner-only directories without following a
link), `repo_path` (confines a stored path to the repository root) and
`ref_guard`. `tests/boundary.rs` fails the build on a `Command::new("git")` in
any other crate's non-test code. mvp's own bounded spawner is rebased on it so
no exemption remains.

## Text index

Git-anchored trigram index (lifted `pixel-index`): base shard at a pinned
commit, delta shard for files changed since, in-memory overlay for the dirty
tree; mmapped shards; regexes planned into trigram queries and every candidate
verified; `policy_walk` is the single file-set authority (ignore rules, the
credential-path filter, generated and binary files). Configuration is a struct
(`IndexConfig`: budget ms, max bytes, default ignores); there are no
process-environment knobs inside the library. Dropped: sparse n-grams and the
frequency-table weigher (production used trigrams only), `gitsync`.

## History

`.pixel/history.db` (lifted `pixel-facts`): commit metadata, diff text, lifecycle
and contentless FTS5 trigram indexes over diffs and paths. **Demand-driven**:
the daemon never ingests at startup, `status` and `doctor` never create the db;
the first history query runs a bounded catch-up (3 s) and starts the
low-priority keep-fresh loop. `read_only: true` queries (the hooks) never
ingest: an absent db answers `unavailable`, a stale one answers from existing
rows. Diff text is kept for the last 365 days within 256 MiB of used pages,
newest first; older diffs are evicted, metadata never. Poison paths are learned.
Schema version in `PRAGMA user_version`; another version is rebuilt.

## Tests that prove it

Lifted shard, posting, plan, verify, delta, overlay, lock and cache suites;
`boundary.rs`; sidecar refuses a linked or tracked `.pixel`; history eviction
keeps metadata; the hooks' read-only mode never creates the db.
