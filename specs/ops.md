# Daemon ops and the wire

Status: normative for the request line a client sends to a repository daemon,
the envelope it gets back, the single absolute deadline, and the failures that
are part of the contract. The daemon's structure (the loop, the writer, the read
replicas, the watcher) and the CLI's routing are ARCHITECTURE.md, "Daemon and
wire contract" and "Request path from the CLI"; the ops are enumerated in code,
`pixel_proto::Op` and `SESSION_CAPABILITIES`, and this file does not copy that
list.

## Transport

- One Unix-domain socket per repository root: `pixel-<xxh3 of the canonical
  root, 16 hex digits>.sock` in `$XDG_RUNTIME_DIR` when it exists, otherwise
  `~/.cache/pixel/sockets/` (0700), shortened under a per-home directory when the
  path would pass the 103 bytes `bind` accepts (`socket_path`). A `.pid` file
  with the same stem sits beside it.
- **NDJSON.** One request per line, one envelope per line, in order, on the same
  connection. A request line is at most 65,536 bytes (`MAX_REQUEST_LINE`); a
  connection serves at most 64 requests (`MAX_REQUESTS_PER_CONN`) and lives at
  most 5 s (`CONNECTION_DEADLINE`), 32 connection threads at once.
- A line that is not valid UTF-8, is too long, or does not parse as an `Op` is
  answered with a failure envelope (`request is not valid UTF-8`, `request line
  too long`, `bad request: <serde error>`); a too-long line and the request limit
  also close the connection. An empty line is ignored.
- A handler that panics answers its own request with `request panicked: <message>`
  and the daemon goes on.

## Request

`{"op": "<snake_case tag>", …fields}` (`pixel_proto::Op`, serde tag `op`). The
tag of a variant is its `op_name()`; the daemon refuses an unknown tag as a bad
request. `SESSION_CAPABILITIES` is every tag a client may send; `shutdown` is the
one internal admin op and is not in it. Several tags name another command:
`reindex` is `pixel build-index`, `update` is `pixel fast-forward`, `sync` is
`pixel fetch`, `history_op` is `pixel commit-history`, `history` is `pixel
search-history` (`search-history.md`), `excavate` is `pixel dig-history`.

## Response: the envelope

```
{ ok, op, protocol, requestId?, snapshot?, epistemics?, budget?, result, error, warnings? }
```

- `protocol` is `ENVELOPE_PROTOCOL_VERSION` (1) and versions only this shape.
- `ok: true` carries a `result` and no `error`; `ok: false` carries an `error`
  `{code, message, details?, ambiguity?}` and no `result`. `Envelope::validate`
  holds this, a non-empty `op` and a non-empty `error.message`, and the daemon
  asserts it on every response in a debug build.
- `error.code` is one of `ErrorCode`'s SCREAMING_SNAKE_CASE names. The daemon
  produces ten: `INVALID_INPUT` (the default for any message it does not
  classify), `NOT_FOUND` (`no symbol named …`, `no symbol with uid …`),
  `BUSY_REPOSITORY` (a message that starts `repository is busy`), the six codes
  that `pixel-ops` names in a `"<CODE>: …"` message prefix (`NON_FAST_FORWARD`,
  `STALE_STATE`, `UNSUPPORTED_STATE`, `REF_EXISTS`, `GIT_FAILED`,
  `NETWORK_AMBIGUITY`) and `INVARIANT_VIOLATION` (the serialize-failure
  fallback). The other variants exist in the type and are never sent: a caller
  must not branch on them being possible.
- `snapshot`, `epistemics` and `budget` describe what a retrieval op read, how
  complete it is and which cap fired. `ping` carries none of them.
  A caller that needs "is this answer complete" reads `epistemics`, not the
  absence of an error.

## Protocol version

`PROTOCOL_VERSION` (`pixel_daemon::api`, 17 on this head) versions the socket
contract and travels in the `ping` result as `protocol_version`. It moves
whenever an older daemon could not safely serve a newer CLI (a new op, a new
request field the daemon must honour). The CLI compares it with its own after a
ping (`classify_ping`):

| Ping says | Treated as | Then |
|---|---|---|
| its own version, `ok` | current | the request is sent |
| an older or missing version, or a failed ping | stale | `pixel daemon start` asks it to `shutdown`, waits up to 2 s for the socket to go quiet and starts one of its own version; it must not serve this CLI |
| a newer version | newer | the daemon is left alone (`pixel daemon start` refuses with a message); a request runs in the CLI process (`serve` reason `NewerDaemon`) |
| nothing answers | absent | a request starts one when `daemon_auto_start` allows, else runs in process |

A ping is answered without waiting behind the writer's queue (`pong`). A request
that is not served by a daemon runs in the CLI process with the same code; the
answer is the same and `serve` in `actions.jsonl` records the route
(`metrics.md`).

## One absolute deadline

A caller that knows when its answer stops being worth anything fixes that instant
once and sends it with every request: a top-level `deadline_unix_ms` member of
the request line (`REQUEST_DEADLINE_FIELD`), milliseconds since the Unix epoch,
beside the op and not inside it, so every op can carry it without a field of
its own. `pixel_proto::request_line(op, deadline)` writes it and
`request_deadline_unix_ms(line)` reads it; a value that is absent, not a number
or negative reads as no deadline.

Contract:

1. **The daemon runs nothing for a request whose deadline has passed when the
   writer or a read replica takes it off its queue.** The answer is a failure envelope with code
   `BUSY_REPOSITORY` and message `repository is busy: the request's deadline passed
   before it started`. A request with no deadline, or one in the future, runs.
2. **The check is at the start only.** A request already running is not
   interrupted by the deadline; its bound is the op's own caps and the client's
   socket timeout. The brief's evidence reader sets that timeout to the time left
   until the same instant, the CLI's ordinary stream to 5 s.
3. **An older daemon ignores the member** and runs the request to the end. The
   CLI therefore sends it only to a daemon that has just answered a ping on this
   `PROTOCOL_VERSION`.
4. **`BUSY_REPOSITORY` is the code a caller can skip or retry.** The brief's
   evidence reader reports any failed daemon request as that probe's failure,
   and the brief then goes on with what it has (`brief.md`, "Failure
   behaviour").

Today the one sender is the brief's evidence reader (`Live::ask`): it uses the
daemon only when a ping on this protocol answers within the brief's deadline,
and otherwise reads the published index and graph in process. The deadline it
sends is the hook's answer deadline (`hooks.md`, `brief.md`). The CLI's ordinary
route (`try_daemon`) sends none.

## Failure behaviour

A client can rely on: a daemon that is absent, stale, slow to answer a ping or
unreachable never turns a command into a different answer, only into a
different route; a daemon failure is an envelope, not a closed socket, unless the
connection limit or a too-long line closed it; and nothing here starts a build
of the index or the graph on behalf of a read (`search-history.md` for the one
op whose default does catch up).

## Tests that prove it

`contract_tests` in `pixel-daemon` (the ping result, the envelope shape of every
op, the error codes and their messages), `daemon` tests
(`a_request_past_its_deadline_should_run_nothing`, the queued-request case, the
line and request caps), `deadline` tests in `pixel-proto` (the member is written,
read, and ignored when malformed), `Envelope::validate` tests, `daemon_ping_tests`
in `pixel-cli` (stale, newer, absent and current daemons), the capability test
that compares `SESSION_CAPABILITIES` with the deserializer's tags, and
`specs_drift` (the protocol version and the deadline member named here are the
code's).
