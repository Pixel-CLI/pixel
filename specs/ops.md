# Daemon, wire contract and ops

Status: normative for `pixel-proto` and `pixel-daemon`.

## Shape

One daemon per repository, a Unix socket, NDJSON: one JSON `Op` per line in,
one JSON `Envelope` line out. `Service::handle(Op) → Envelope<Value>` is the
single entry point; the socket server and the in-process fallback both call it.
The thread that owns the `Service` is the only writer; reads are served from
replicas (`readers.rs`) of the last published generation (`plane.rs` classifies
every op as read, write or repo-write with no wildcard arm).

Socket, pid and lock live in `$XDG_RUNTIME_DIR` when set, else
`~/.cache/pixel/sockets/` (0700), named `pixel-<xxh3 of canonical repo path>.sock`;
`TMPDIR` is never read. A path longer than `sockaddr_un` moves to
`/tmp/pixel-<hash of HOME>/` (0700, owner-checked). Versions: `PROTOCOL_VERSION`
in `pixel-proto`; a version skew is answered with an error naming both sides and
the CLI falls back to in-process.

## Envelope

`Envelope{ok, data | error, epistemics, snapshot, budget, warnings, recovery}`
(lifted from `pixel-proto`: `envelope`, `error`, `epistemics`, `snapshot`,
`budget`, `warning`, `recovery`). `epistemics` states `closed_world: false`,
`lower_bound`, staleness, and why a result is partial. A capped answer says
which cap fired.

## The 15 ops

| Group | Op | Notes |
|---|---|---|
| Admin | `Ping`, `Status`, `Reindex`, `Shutdown` | `Status` reports index, graph and history freshness and the daemon's generation |
| Hooks | `Brief{prompt, session, host, parts}` | runs `pixel-brief` inside the daemon, on a read replica (it takes its whole window and never holds the writer). Result: `{text, declined, paths, record}`: the block or `null`, why there is none (`continuation`, `not_about_code`, `no_worker`; the caller adds `disabled` when `PIXEL_BRIEF` is off), the repository files the block names, and the journal's `brief` record for the caller to append with its own provenance |
| | `EditCheck{path, proposed?, phase, session}` | `hooks.md`. `phase` is `before` (default: the card, and the session's baseline for the file) or `after` (the stale-caller set). Reads the generation and writes only the session file |
| | `StopCheck{session}` | `hooks.md` |
| Queries | `Search` (regex/literal over the trigram index), `Symbol`, `Callers` (callers and callees by uid and tier), `Impact` (blast radius, includes tests), `Changes` (working tree or a base), `History`, `Meaning` (feature `semantic`), `Map` (repo map and signatures) | |

Not wire ops, only in-process calls on the `Engine`: context packing, trace,
rename, concept resolution, evaluation. The previous Pixel engine's other 27 ops (git writes,
recall, lifecycle, journal, publish, push, ship, branch, update, sync, note,
diff, review…) are not carried; 14 of that engine's 42 had no client at all.

## Daemon behaviour

- **Publisher.** The watcher thread debounces edits (quiet period extended only
  for content events, capped from the first pending edit) and the single writer
  runs watch → plan → extract → link → publish. A batch that arrives while a
  writer holds `index.lock` is skipped and the next op repairs the drift.
- **Auto-start.** `SessionStart` and a briefed prompt start the daemon in the
  background (no wait beyond 100 ms); `PIXEL_DAEMON_AUTO_START=0` disables it.
- **Idle lifetime.** The daemon reads `PIXEL_DAEMON_IDLE_SECS` once when its
  service loop starts. A trimmed positive integer sets the idle timeout in
  seconds; missing, empty, invalid, zero or overflowing values use 1,800 seconds
  (30 minutes). The same duration bounds its wait and decides when it exits.
- **Index freshness.** The trigram index is git-anchored: a base shard for all
  tracked files at a pinned commit, a delta shard for files changed since, and
  an in-memory overlay for the dirty tree. Catch-up after a ref moves is
  bounded (2,000 paths) and a wider move leaves the set behind and says so.
- **Brief window.** `Brief` answers within the caller's deadline or returns the
  partial brief marked cut. It reads like a `stale_ok` caller: when the graph
  lags the working tree the evidence is used and the block says it may predate
  the last edit. A repository whose graph was never built gets the text
  evidence only, until `pixel prepare` (or a graph query) builds it; semantic
  leads answer `unavailable` (with the reason) until the embedding model is on disk
  (`specs/semantic.md`); the concept lookup (`find-code`) reads the text index
  and needs no build.
- **Trust boundary.** The socket is owner-only; any client of it can query the
  repository, nothing can mutate it (there are no write ops over the socket).

## Tests that prove it

Frame codec round trips (lifted `wire.rs` tests); plane classification is
exhaustive; two consecutive watcher batches with no writer request between them
both publish generation and envelope snapshot (test-hygiene rule, from Pixel
#1203); a version-skewed client falls back in process; every op returns the
envelope fields above; an oversized request is refused with a typed error.
