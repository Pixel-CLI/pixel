# Version-pinned reference corpora — design and first-slice decision

Issue #623. This document records the design of `pixel reference` and the
decisions taken on the acceptance criteria that are measurements or
judgement calls rather than unit-testable behaviour.

## Design

A reference corpus is a local checkout of a dependency (or any other
repository) pinned to an exact revision, registered in
`.pixel/references.json`. The manifest entry carries:

| Field | Meaning |
| --- | --- | --- |
| `id` | Stable corpus ID (e.g. `serde-1.0.188`) |
| `role` | `target` (the project being worked on) or `reference` (dependency material) |
| `source_url` | Where the corpus was cloned from |
| `revision` | User-facing pinned revision (tag, branch, or SHA) |
| `revision_sha` | Resolved commit SHA at setup time; the query verifies HEAD against it |
| `path` | Local source location (canonicalized at add time) |
| `license` | Licence identifier (e.g. `MIT OR Apache-2.0`) |
| `package` / `package_version` | Package identity, so multiple versions of one dependency coexist |
| `provenance` | How the corpus was obtained |

### Setup vs. querying

`pixel reference setup` is the only operation that may clone, fetch,
checkout or build. It clones (or fetches) the source URL, checks out the
pinned revision, resolves the commit SHA into the manifest, and builds the
text index.

`pixel reference query` is cold and read-only. It walks each corpus's local
source tree (using the same `policy_walk` the index uses) and runs a regex
over every line. It never fetches, clones, builds or modifies source. A
corpus whose HEAD does not match its pinned revision is disclosed as a
failure in the coverage report and skipped — the query never silently falls
back to a different revision.

### Bounded fan-out and fusion

The query fans out over the registered corpora with a per-corpus quota
(`--per-corpus-limit`, default 20) and a total limit (`--limit`, default 50).
The fusion policy is explicit: target-role corpora outrank reference-role
ones (the project being worked on is more relevant than dependency
material); within a role, hits are ordered by corpus ID, then path, then
line. Raw regex match scores are equal across corpora (each hit is a
match), so no cross-corpus score comparison is needed.

### Coverage and partial results

Every query returns a coverage report: one entry per corpus that was
attempted, with `ok`, `error`, `hits` and `capped`. A corpus that is
missing, not a git repository, or at the wrong revision is reported
individually with its error. The `partial` flag is true when any corpus
failed. The remaining corpora still answer — a multi-corpus failure never
loses the usable results.

### Reference text is untrusted evidence

A hit is a citation to read and verify, not a command to run. The query
output includes the corpus ID, role, repository, revision, path and line so
the agent can check the source itself. Reference text is never presented as
executable instructions.

## Acceptance criteria decisions

### Two pinned versions coexist and never exchange citations or snippets

**Implemented and tested.** The manifest holds multiple entries with the
same `package` but different `revision`/`path`. Each query hit carries the
corpus ID and revision it came from. Test:
`two_pinned_versions_coexist_without_cross_contamination`.

### Every result identifies target/reference role, repository, exact revision and source location

**Implemented and tested.** Every `ReferenceHit` carries `corpus_id`,
`role`, `repository`, `revision`, `path`, `line` and `snippet`. Test:
`query_returns_attributed_hits`.

### A missing, wrong-revision or incompatible corpus is disclosed; exact-version queries cannot silently use latest

**Implemented and tested.** `search_corpus` verifies the path exists, is a
git repository, and is at the pinned revision (via `revision_sha`). A
mismatch is disclosed in the coverage report. Tests:
`query_discloses_a_missing_corpus`, `query_discloses_a_wrong_revision`,
`query_never_uses_latest_when_revision_mismatches`.

### Cold query behavior cannot fetch, clone, build or modify source without an explicit setup action

**Implemented and tested.** The query path only reads files and runs a
regex. It never calls `git clone`, `git fetch`, `git checkout` or
`build-index`. A corpus that has not been set up (missing path) is
disclosed as a failure, not silently cloned. Test:
`query_discloses_a_missing_corpus`.

### Multi-corpus failures and caps preserve usable results and disclose partial coverage

**Implemented and tested.** The query continues past failing corpora and
reports coverage for each. Test: `multi_corpus_failure_preserves_usable_results`.

### A fixture with conflicting symbol names proves source attribution and scoped selection

**Implemented and tested.** Two corpora both define `fn process()`; each
hit carries the corpus it came from. Test:
`conflicting_symbol_names_prove_source_attribution`.

### Measure setup time, storage, cold/warm latency and total agent task cost against per-repo Pixel and native tools

**Decision: deferred to the measurement protocol in #626.** The first slice
implements the manifest and query workflow; the measurement harness
(setup time, storage, cold/warm latency, agent task cost vs. per-repo Pixel
and native tools) is a separate experiment that needs a controlled
environment and real dependency checkouts. The `pixel reference` commands
record timing in the action log (`pixel action-log`) so the measurement
can be run against the shipped commands.

### Include unfamiliar/version-changed APIs and familiar-code controls; publish neutral or negative results

**Decision: deferred to the case-study protocol in #626.** The first slice
provides the infrastructure (version-pinned corpora, attributed hits); the
experiment that uses it (unfamiliar/version-changed APIs vs. familiar-code
controls) is a separate study. The design supports it: multiple versions of
the same dependency can coexist, and hits are attributed to the exact
revision they came from.

### Decide from the first slice whether manifest ergonomics alone suffice before building distribution infrastructure

**Decision: manifest ergonomics alone suffice for the first slice.** The
manifest + explicit query workflow delivers the core value (searching
approved reference repositories at pinned dependency versions with full
attribution) without distribution infrastructure. Pack reuse for immutable
references (cryptographic digests, trusted publisher keys, format
compatibility, coherent snapshots, extraction limits) is deferred until
the manifest proves useful in practice. No automatic cloning, corpus
downloading or index changes are authorized by this issue; the first slice
adds none.
