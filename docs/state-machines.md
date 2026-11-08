# Runtime state machines

Audited on 2026-11-08 from `7c9f8063d57b96c70f3e71cf4db0c8249657f305`.
Tracking: [#1388](https://github.com/Pixel-CLI/pixel/issues/1388).
This is the single inventory for explicit enums and implicit multi-step runtime
protocols. State names below describe reachable storage/control combinations;
an enum alone does not enforce its table. Tables describe the intended contract;
the findings distinguish violations in the audited baseline from that contract.

Scope: the core CLI and its 20 workspace crates. Extractor/parser enums, query
result classifications and pure ranking functions are values, not lifecycle
machines. Extracted flow/ultraflow plugins are not in this workspace. CI, release
orchestration and external agent internals are outside this runtime audit.
Source inspection is not a proof of all interleavings. Regression evidence below
is narrower: deterministic operation probes and real temporary Git repositories.

## Confirmed violations, highest impact first

| ID / impact | Reachable baseline execution and violated invariant | Fix group / status |
| --- | --- | --- |
| M1 / P1: live index/history rollback | Publish A pauses at `journal:index_staged`; duplicate A reads Resume outside the lock and `restore_snapshot` overwrites A's staged index. Rewrite's `RefUpdateStarted` resume likewise resets HEAD without a lock. | [#1389](https://github.com/Pixel-CLI/pixel/issues/1389), implemented; validation below |
| M2 / P1: two lock owners | Two contenders read one dead `owner.json`; A removes/recreates the directory, then B's stale decision removes A's live directory. `try_recover_stale` has no atomic claim. | #1389, implemented; validation below |
| M3 / P1: recovery targets wrong index/history | Linked worktree `.git` is a file; `index_path` nevertheless appends `index`. Capture treats its read error as absence. Separately, a retry after another commit/stage/checkout blindly restores the old index and current HEAD. | [#1390](https://github.com/Pixel-CLI/pixel/issues/1390), snapshot fix implemented; validation below |
| M8 / P1: rewrite recovery erases later history | After a crash at `reset:done`, a later commit or checkout is followed by retry; unconditional `reset --soft old_head` rewinds the new current branch. | #1390, implemented; validation below |
| M4 / P2: retry changes completed operation | At `CommitObserved`, retry reads today's HEAD and reports it as the original commit; `push=true` becomes `pushed=false`. At Started with a snapshot, first recovery deletes it; second retry sees no snapshot and executes again. | #1390, implemented; validation below |
| M5 / P2: wrong ownership domain on retry | `push_with_state` Started resume uses global `state_root()` for its lock, while the journal remains under the supplied root. All journaled operations read admission before locking, permitting a stale phase decision. | #1389, implemented; validation below |
| M6 / P2: half-created branch | `git branch new target` succeeds, checkout rejects overlapping dirty changes, retry encounters `REF_EXISTS`. | [#1391](https://github.com/Pixel-CLI/pixel/issues/1391), pending regression/fix |
| M7 / P2: applied fast-forward cannot resume | Process exits after successful merge but before terminal journal write; HEAD is target, yet retry requires the old expected HEAD and returns `STALE_STATE`. | #1391, pending regression/fix |

P1 means potential corruption of staged intent or branch history. P2 means a
wrong receipt, missing requested work, or an operation stranded after failure.
These are concrete control-flow counterexamples, not findings based merely on
representable enum combinations. The fixes and failing/passing tests are recorded
here as they are implemented; an open PR is not a merged or deployed fix.

## 1. Repository ownership (implicit)

A persistent `locks/<hash>.guard` kernel lock now owns the directory protocol;
release removes only the owned directory, never the guard inode.

Owner: [lock.rs](../crates/pixel-ops/src/lock.rs),
[repo.rs](../crates/pixel-ops/src/repo.rs).
States: free, acquiring, held, abandoned, unreadable owner.
Identity is the canonical Git common directory, shared across linked worktrees.

| From | Event / guard | To / effect |
| --- | --- | --- |
| Free | Exclusive acquisition | Held with owner PID and token |
| Held | Another caller | Busy; no Git or recovery mutation |
| Held | Owner releases/drops | Free, only if token matches |
| Held | Owner process dies | Abandoned |
| Abandoned | Exclusive recovery confirms dead owner | Held by one replacement |
| Unreadable owner | Ownership cannot be proven | Busy, fail closed |

Invariant: admission, journal reads, effects, recovery and completion share one
ownership interval. Never delete a replacement owner's lock using an earlier
observation. Native Git and older processes are separate concurrency actors;
Pixel's lock does not make arbitrary external Git commands transactional. M1/M2/M5.

## 2. Durable request journal (explicit)

Owner: [journal.rs](../crates/pixel-ops/src/journal.rs).
Key: common-directory identity + request ID; operation/input hash must match.
States: absent, Started, IndexStaged, CommitObserved, PushStarted,
RefUpdateStarted, Terminal; unreadable/unsupported data is an error.

| From | Event / guard | To / result |
| --- | --- | --- |
| Absent | Valid ID, create-new succeeds | Started / Start |
| Any existing | Different operation or input | Idempotency conflict |
| Intermediate | Matching request, exclusive ownership | Resume at durable phase |
| Started | Commit stages / push begins / rewrite begins | IndexStaged / PushStarted / RefUpdateStarted |
| IndexStaged or RefUpdateStarted | Commit observed durably | CommitObserved |
| CommitObserved | Requested push begins | PushStarted |
| Nonterminal | Persist result after effects | Terminal |
| Terminal | Matching retry | Replay stored result, no repeated effects |
| Terminal | Retention exceeds 30 days or 1,000 records | Removed |

Writes use temp + fsync + rename + directory fsync. Nonterminal records are never
pruned. Retention bounds idempotency: reuse of a pruned ID is a new operation.
`transition` is a storage primitive, not a validated transition API; callers own
the graph. Missing/corrupt records must not be interpreted as proof of no effect.

## 3. Commit and recovery snapshot (two coupled machines)

Owners: [publish.rs](../crates/pixel-ops/src/publish.rs),
[recovery.rs](../crates/pixel-ops/src/recovery.rs).
Journal states above couple with absent / Snapshotted / CommitStarted recovery.
`RecoveryPhase::IndexStaged` is representable but the current producer does not
write it. Inputs include expected HEAD, file fingerprints, amend and push.

| Journal / recovery | Event | Resulting state / required behavior |
| --- | --- | --- |
| Started / absent | HEAD/fingerprint guards pass | Capture exact index and HEAD before staging |
| Started / Snapshotted | Stage requested paths | IndexStaged / Snapshotted |
| IndexStaged / Snapshotted | Persist commit intent | IndexStaged / CommitStarted |
| IndexStaged / CommitStarted | Git commit succeeds; persist OID | CommitObserved; remove obsolete recovery |
| CommitObserved | No push requested | Terminal, recorded commit OID |
| CommitObserved | Push requested | PushStarted before network I/O |
| PushStarted | Push returns success | Terminal, pushed=true |
| Started / absent | Retry under ownership | May execute fresh |
| Started or IndexStaged / snapshot | Retry | Restore only owned state; otherwise retain evidence and refuse |
| CommitObserved | Retry | Return recorded OID; if push was requested, explicitly refuse as incomplete and name that OID for a new push request |
| PushStarted | Retry cannot prove remote outcome | NETWORK_AMBIGUITY; never blindly push |

Invariants: unrelated staged/unstaged/untracked data survives; restore uses the
actual worktree Git index; HEAD/branch/worktree drift never authorizes rollback;
a repeated recovery failure must not turn into fresh execution. M1/M3/M4.
The snapshot is not a distributed transaction with Git: an interrupted effect
can remain ambiguous and must be reported as such.

## 4. Push and commit-and-push (implicit)

Owners: [push.rs](../crates/pixel-ops/src/push.rs),
[ship.rs](../crates/pixel-ops/src/ship.rs).

| From | Event / guard | To / result |
| --- | --- | --- |
| Started | Validate remote/refspec, resolve source OID | PushStarted with source OID before network |
| PushStarted | Push succeeds | Terminal |
| PushStarted | Error/crash | Ambiguous; keep evidence |
| Ambiguous | Retry observes exact recorded OID remotely | Terminal, confirmed success |
| Ambiguous | Remote differs or cannot be read | NETWORK_AMBIGUITY |
| Ship uncommitted | Publish using `<id>-pub` | Committed |
| Ship committed | Push using `<id>-push` | Shipped |

The push source is the refspec source, not necessarily HEAD. Leased pushes compare
against an observed remote OID. Ship is two independently journaled operations,
not one atomic commit-and-network transaction; local success can precede push
failure. External ref movement is possible between them. M5 covers retry locking.

## 5. Branch creation and fast-forward (implicit)

Owners: [branch.rs](../crates/pixel-ops/src/branch.rs),
[update.rs](../crates/pixel-ops/src/update.rs).

| From | Event / guard | To / result |
| --- | --- | --- |
| Branch absent | Valid name/base, safe checkout | Created and checked out together |
| Branch already exists | New request | REF_EXISTS; never claim it was checked out |
| Create/switch refused | Dirty target overlap or Git failure | Prior refs, index and working files retained |
| Update Started | HEAD=expected, target descendant, dirty paths safe | Record intent, fast-forward, Terminal |
| Update retry after effect | HEAD=recorded target | Terminal, original from/to receipt |
| Update | HEAD unrelated to intent | STALE_STATE |
| Update | Diverged ancestry or overlapping dirty paths | NON_FAST_FORWARD / UNSUPPORTED_STATE |

Repeated terminal requests replay; they do not switch back after the user later
changes branch. M6/M7 cover partial effects before a terminal journal write.

## 6. Reconcile and rewrite (implicit + explicit journal)

Owners: [reconcile.rs](../crates/pixel-ops/src/reconcile.rs),
[rewrite.rs](../crates/pixel-ops/src/rewrite.rs).

| From | Event / guard | To / result |
| --- | --- | --- |
| Unknown relation | Fetch and compare ahead/behind | Up-to-date, ahead, behind, diverged |
| Behind | Safe fast-forward | Integrated |
| Diverged | Selected strategy, clean enough tree | Rebased/integrated, or conflict |
| Conflict | Git rebase/merge remains active | Refuse competing integration; preserve user resolution |
| Integrated/ahead | Push requested, lease still valid | Pushed |
| Lease rejected | Re-read remote once | Reclassified result; no blind second push |
| Rewrite Started | Guards pass; backup ref saved | RefUpdateStarted before reset/commit |
| RefUpdateStarted | Commit recorded | CommitObserved |
| RefUpdateStarted | Retry | Require recorded branch and pre/reset HEAD; compare-and-swap that named ref, otherwise retain evidence and refuse |
| CommitObserved | Optional leased push | PushStarted then Terminal |
| PushStarted | Retry | Ambiguity rather than force-push |

Default-branch/published-history protection, expected HEAD, clean index and
backup refs constrain rewrite. Reconcile's conflict marker is diagnostic, not the
source of truth for Git's merge/rebase state. Empty request IDs get fresh UUIDs,
so only explicit stable IDs support replay. M1 applies to rewrite recovery.

## 7. Task contract, verification and completion (explicit)

Owners: [model.rs](../crates/pixel-task/src/model.rs),
[store.rs](../crates/pixel-task/src/store.rs), [policy.rs](../crates/pixel-task/src/policy.rs),
[runner.rs](../crates/pixel-task/src/runner.rs), [sandbox.rs](../crates/pixel-task/src/sandbox.rs).
Phases: Contracted, Prepared, Editing, Verifying, Reviewing, Complete, Blocked,
Cancelled, Incomplete. Blocked is representable; denial normally returns an error
rather than persisting that phase. Running lease is an independent dimension.

| From | Event / guard | To / effect |
| --- | --- | --- |
| Absent | Valid contract and binding | Contracted |
| Nonterminal | Contract update; no running verification; no unauthorized weakening | Contracted; invalidate preparation/review |
| Nonterminal | Capture current source + matching observations | Prepared |
| Nonterminal | Edited event | Editing; invalidate source/observations/review |
| Nonterminal, prepared | Verify selected known checks; no active run | Verifying + durable run ownership |
| Active run | Child finishes/fails/times out/source changes | Receipts; Reviewing unless Cancelled |
| Nonterminal | Review of current prepared source | Reviewing + source/contract-bound receipt |
| Eligible | Finish gate passes | Complete |
| Any | Cancel | Cancelled; active lease remains until completion/recovery |
| Interrupted run | Owner dead; child recovery safe | Incomplete (or retain Cancelled), clear lease |
| Nonterminal | Bounded correction | Same phase; increment continuation/repeat budget |

Updates serialize under task lock, compare revision and deduplicate request/input.
Verification runs outside the transaction; finalization rechecks run identity.
Finish requires fresh source, required passing receipts, acceptance criteria and
review according to contract. Claims are not evidence. Editing/Prepare during a
run can change phase, but the independent running marker still denies admission;
source/contract IDs prevent old receipts from satisfying changed work.

## 8. Daemon lifecycle, transport and request scheduling (implicit)

Owners: [daemon.rs](../crates/pixel-daemon/src/daemon.rs),
[wire.rs](../crates/pixel-daemon/src/wire.rs),
[readers.rs](../crates/pixel-daemon/src/daemon/readers.rs).

| From | Event / guard | To / effect |
| --- | --- | --- |
| Stopped/stale socket | Exclusive daemon start | Starting, then listening |
| Listening | Connection accepted within cap | Read bounded NDJSON frame |
| Frame partial | Deadline/EOF/oversize | Reject/close; never dispatch partial request |
| Valid request | Deadline live | Queue writer or eligible read replica |
| Queued | Deadline expires before dispatch | Expired response; no late mutation |
| Replica | Cannot answer/coherent generation unavailable | Requeue on writer |
| Serving | Stop, missing root, idle expiry | Shutdown and owned transport cleanup |
| Watch pending | Quiet debounce or max wait | Queue bounded batch; overflow asks full reconciliation |

Only eligible reads bypass writer work. Refresh jobs block ordinary bypass;
`stale_ok` can read the prior generation with an explicit lag stamp. Connection,
request-count and queue deadlines bound resource ownership. A client timeout is
not evidence that a dispatched mutation did not execute; mutation IDs govern retry.

## 9. Text index / graph publication and freshness (implicit)

Owners: [api.rs](../crates/pixel-daemon/src/api.rs),
[indexset.rs](../crates/pixel-index/src/indexset.rs),
[build.rs](../crates/pixel-graph/src/build.rs).

| From | Event / guard | To / effect |
| --- | --- | --- |
| Absent/incompatible | Open/build | Indexed base + delta, graph at extractor version |
| Healthy generation g | Watch/HEAD change | Batch pending; old generation may serve stale reads |
| Pending | Stage changed files and begin publication | Publication guarded/unhealthy during risky work |
| Publishing | Index/graph commit + snapshot | Healthy generation g+1 |
| Publishing | Failure | Unhealthy or graph-lagging; refuse coherent graph claim |
| Replica g | Published generation changed | Reopen/invalidate cached graph; retry through writer if needed |

Invariant: a response's index, graph and envelope snapshot refer to one published
generation; lag is disclosed, never silently called fresh. Failed publication is
not repaired merely by clearing a flag. Existing consecutive-batch contracts in
`api/contract_tests.rs` and `daemon/contract_tests.rs` exercise this boundary.

## 10. Resident semantic index (implicit)

Owner: [meaning.rs](../crates/pixel-daemon/src/meaning.rs).
State tuple: optional ready generation, building flag, optional failure, latest
requested generation; embedder ownership is separate.

| From | Event / guard | To / result |
| --- | --- | --- |
| Cold/stale | ensure, no build and no failure backoff | Warming/refreshing, one builder |
| Building | New generation | Raise latest; builder loops up to bounded run cap |
| Building | Success | Ready at built generation; clear failure |
| Building | Error/panic in unwind build | Failed/model-missing; clear building |
| Failed | Backoff expires | Retry |
| Model missing | Next request | Retry immediately |
| Ready | New watcher generation | Refresh only if previously requested |
| Ready | Answer races embedder taken by builder | Unavailable/refreshing, no mismatched search |

Release uses panic=abort: a panic terminates the process rather than executing
catch_unwind recovery. Auto-start is process recovery, not evidence a request ran.

## 11. History facts and transcript recall (checkpoint machines)

Owners: [ingest.rs](../crates/pixel-facts/src/ingest.rs),
[store.rs](../crates/pixel-recall/src/store.rs),
[sources/mod.rs](../crates/pixel-recall/src/sources/mod.rs),
[recall_service.rs](../crates/pixel-daemon/src/recall_service.rs).

| From | Event / guard | To / effect |
| --- | --- | --- |
| History absent/stale | Writer tick | A: enumerate/insert commit metadata |
| A complete | Budget remains | B: blob sizes and policy exclusions |
| B progress | Eligible diffs, budget remains | C: diff/hunk ingestion |
| Partial | Budget/deadline reached | Persist cursor; next tick resumes |
| Complete | Refs/window change | Stale; new bounded ingest |
| Stored facts | Retention/budget eviction | Keep coverage limits explicit; reclaim space |
| Transcript unknown/rewritten | Source change | Replace session and ingest cursor transactionally |
| Transcript appended | Matching offset/tail identity | Append turns + cursor transactionally |
| Pending embedding | Model available, chunk batch | Persist vectors and mark embedded |
| Ingest error | Background retry | Backoff; queries disclose available coverage |

A cursor must not certify rows that were never committed. Missing model, partial
history, policy skips and embedding backlog differ from an empty complete corpus.
Read-only status does not initiate ingestion. Index data is rebuildable; user
transcripts are not owned or deleted by these caches.

## 12. Setup/uninstall and plugins (ownership machines)

Owners: [receipt.rs](../crates/pixel-setup/src/receipt.rs),
[install.rs](../crates/pixel-setup/src/install.rs),
[plugin/install.rs](../crates/pixel-plugin/src/install.rs),
[plugin/state.rs](../crates/pixel-plugin/src/state.rs).

| From | Event / guard | To / effect |
| --- | --- | --- |
| Foreign/absent config | Capture before setup | Original ownership snapshot |
| Captured | Setup writes, finish receipt | Installed fingerprint + original retained |
| Installed | Reinstall after user edit | New original baseline preserves that edit |
| Installed | Uninstall and fingerprint/location still match | Restore original or remove owned output |
| User-modified installed output | Uninstall | Preserve changed content according to receipt rules |
| Plugin absent | Validate manifest/skill/source, stage copy | Staged |
| Staged | Rename to destination + record source | Installed; binary may still be unavailable |
| Installed | Enable/disable/trust | Configuration changes; trust tied to digest |
| Installed | Remove | Files/config removed through plugin owner |

Receipt and plugin workflows contain several filesystem writes, not one atomic
transaction. A failed command is not a proof that nothing was installed. Source
inspection here establishes the states and guards; crash/fault injection across
all installer writes was not part of the regression campaign below.

## 13. Brief delivery, session evidence and advisory bookkeeping

Owners: [brief_parts.rs](../crates/pixel/src/brief_parts.rs),
[sent.rs](../crates/pixel/src/execution_brief/sent.rs),
[feedback.rs](../crates/pixel/src/execution_brief/feedback.rs),
[call_guard.rs](../crates/pixel/src/call_guard.rs),
[plan_state.rs](../crates/pixel/src/plan_state.rs).

| From | Event / guard | To / effect |
| --- | --- | --- |
| Brief unjudged | Intent/relevance/budget evaluation | Open, closed, denied, declined or unjudged gate |
| Part cache absent | Part 1 computes and atomically stores | Payload available (possibly empty) |
| Payload available | Part 1 writes delivery marker | Done or missing-head |
| Follower waiting | Payload/marker or deadline | Bounded slice, head compensation, or empty |
| Sent range absent | Delivered region | Session remembers bounded hashed range |
| Sent range known | Repeat region | Omit while dedupe enabled; expiry/reset permits resend |
| Feedback prompt open | Tool events then stop/new boundary | Bounded turn summary |
| Call history below threshold | Repeated scoped read | Warning threshold crossed; retrieval still executes |
| Plan item open/done | Mark, re-plan, prune | Persist checklist; not task-completion evidence |

Brief cache keys use session+prompt, not a globally unique invocation. Concurrent
identical prompts and best-effort read/modify/write logs are not exactly-once
ledgers. Do not use their counts to authorize edits or certify completion. Session
clear/compact invalidates sent ranges; resume retains them. Advisory I/O fails open.

## Regression evidence and maintenance

Baseline and fixed runs will be added here with exact commands, revisions and
results. Do not mark a finding fixed solely because a unit helper passes: exercise
the production entry point, and retain negative controls for drift and busy owners.
When adding a lifecycle or phase, update its table and its producer/readers in this
file in the same PR. Existing state-machine tests remain part of normal CI; no
local or manually triggered mutation campaign is required or run by this audit.

### Ownership regression evidence (#1389)

`cargo test -p pixel-ops --test all fail_closed:: --no-fail-fast` on baseline
`7d113211` plus regressions: 3 passed, 2 failed. The duplicate publish changed the
staged index; the busy push created `refs/heads/result` remotely. Fixed regressions
call the real entry points from the live publish/rewrite probe; they assert exact
index/journal/HEAD preservation and a busy result. The stale-lock test holds the
persistent guard over a dead-owner directory and verifies a contender preserves
it, then verifies successful exclusive recovery after release. No sleeps.

Fixed ownership candidate: `cargo test -p pixel-ops --no-fail-fast` passed
149 unit + 62 integration tests; `cargo clippy -p pixel-ops --all-targets -- -D warnings`
and `cargo fmt --all -- --check` passed. Full current-head workspace gates run in CI.

### Snapshot recovery evidence (#1390, first layer)

At `895599bc` plus new tests, `cargo test -p pixel-ops --test all fail_closed::recovery_ --no-fail-fast`
failed all five cases: wrong linked-worktree index, lost later work, second retry
re-execution, wrong commit receipt and silently skipped push. This layer fixes the
first three; the next fixes the two receipt cases. Schema 2 binds the snapshot to
its actual index path and branch and persists its post-staging checksum. Recovery
holds native `index.lock`, refuses HEAD/branch/worktree/index drift, and retains the
snapshot after refusal. A v1 snapshot fails closed for manual recovery. Successful
commits still remove their snapshots. Raw external ref updates do not honor the
index lock; this is not a transaction across arbitrary concurrent native Git.

Snapshot layer validation: `cargo test -p pixel-ops --no-fail-fast` passed
149 unit + 66 integration tests. `cargo clippy -p pixel-ops --all-targets -- -D warnings`
and `cargo fmt --all -- --check` passed. This includes native index-lock contention,
unreadable index rejection and repeated crash recovery. Workspace CI is required.

### Recovery identity evidence (#1390, second layer)

`cargo test -p pixel-ops recovery --no-fail-fast` at `e17cdf8a` plus regressions:
7 integration cases passed, 3 failed (wrong OID, omitted requested push, rewrite
rewinding a later commit). Publish now reads its durable observed OID. A requested
push not yet started stays an explicit incomplete error, naming that OID and a
new push request as recovery; it is never a successful no-push receipt. Rewrite
checks the recorded branch and pre/reset OID, then compare-and-swaps that named
ref. A newer commit/other branch is a recovery conflict. Existing reset-window
recovery still restores the pre-rewrite head and leaves index/worktree untouched.

Identity layer validation: `cargo test -p pixel-ops --no-fail-fast` passed
149 unit + 69 integration tests; `cargo clippy -p pixel-ops --all-targets -- -D warnings`
and `cargo fmt --all -- --check` passed. Full current-head CI remains required.
