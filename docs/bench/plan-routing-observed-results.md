# Observed planning traffic: stop model work, capture better provenance

2026-09-22. Source HEAD `5c894b1b3a5db713615d906dc8604805c73813ae`, branch
`experiment/coding-routing-pilot`. Existing experiments and defaults preserved.

**Decision: pause further routing-model evaluation/integration.** The bounded
indexed history supplies no independently supported implicit specialized or
mixed-intent families. The two observed specialized scans already use explicit
`--query`, which bypasses classification. A new opt-in request recorder was
implemented and executed so future genuine calls can carry better provenance.
No additional model score, training run or synthetic positive corpus was produced.

## What the history actually contains

Retrieval followed [the frozen availability protocol](plan-routing-observed-protocol.md):
Pixel recall only, last 30 days, upper bound `2026-09-22T00:00:00Z`, before the
prior model experiments. First search was repo-scoped/tool-role; it mostly found
quoted documentation. Second searched assistant tool-call turns in this repo.
One final widening removed the repo filter. The final query returned **39 hits,
uncapped**. No fourth reformulation or transcript-store grep followed.

```sh
PIXEL_METRICS=0 pixel recall search 'pixel plan ' --since 30d \
  --until 2026-09-22T00:00:00Z --role assistant --limit 200 --json
```

A bounded extractor reads each named turn with `pixel recall show`, parses actual
Bash tool-call JSON and literal direct CLI argv, then looks for nearby plan output
and matching action events. It rejects help, shell-generated code/demos, dynamic
or unresolvable shell shapes, and capped input turns. It does not execute retrieved
commands or convert explicit flags into invented natural-language requests.

| Retained population | Count |
|---|---:|
| Search hits | 39 |
| Literal planning invocations | 17 |
| Excluded capped/missing turns | 11 |
| Other hits without an admissible literal call | 11 |
| Implicit task prompts | 15 |
| Explicit query calls | 2 |
| Implicit task families after grouping | 9 |
| Implicit specialized families | **0** |
| Implicit mixed-query families | **0** |

The 17 records span four working-directory contexts representing Pixel, dotfiles,
a Pi-extension project and a temporary worktree. They are agent tool invocations,
not proven human-authored planning requests. Related root/child remediation,
implementation/review and same-session configuration tasks are grouped rather
than counted as independent samples. The two explicit audit calls form another
shared family and request dead-code and hotspots respectively.

This is **not exhaustive usage history**. The exact query misses aliases, global
flags inserted between command words, direct daemon calls, unindexed sources,
older sessions and lost/truncated records. The action-log read returned 1,436
events including 20 plan calls; only five relevant successful events predate the
cutoff. Its rotation and flattened/bounded argv limit independent reconstruction.
A missing indexed positive is not evidence that nobody uses implicit audits.

## Independent review and provenance limits

A read-only reviewer checked all 17 records and bounded source context. All were
admitted as actual invocations. Real code-remediation tasks arising from benchmark
findings were kept; they are not themselves model-benchmark probes. The current
model experiments, documentation examples, help and timing probes were excluded.

One initial gold label was challenged and corrected: targeted removal of named
extensions was initially classified using the existing `remove` keyword rule.
Inspection of the actual dead-code operation showed why that was not relevance
gold: the operation discovers uncalled functions, rather than locating already
named features. Final implicit gold is **15 by-concept requests / 9 families**.
Both initial and corrected reports are retained; no predictions informed correction.

Completion evidence has different strengths:

- Five unique action matches record `outcome: ok`. Source inspection confirms
  the event is written from the result **after** `run_command`; this is evidence
  of successful completion, not merely an invocation timestamp.
- Four further rows have adjacent completion-shaped output, without a call-ID
  join. Those associations remain ambiguous. Two of the five action-matched rows
  also have adjacent output, giving six adjacent-output rows in total.
- Eight rows have only the recorded invocation. Completion is unknown.

Action joins match cwd, flattened argv and nearby timestamp, not a transcript call
ID. A later tightening of the extractor's cwd condition was checked against all
17 retained records and changed no joins. No historical repository HEAD is proven
by this extraction. That is a limit of this bounded audit, not a claim that no
additional archaeology could ever recover one. The present HEAD was not imputed
to historical calls.

Because the preregistered support gate fails, **no model replay was run**. Another
all-concept accuracy table would not validate specialized recall or composition.
The earlier candidate artifacts and conclusions remain unchanged.

## Executed deliverable: private opt-in capture

`scripts/bench/record-plan-request.py` wraps one native `pixel plan` invocation. It
installs no hook and runs only when explicitly invoked. It records:

- literal prompt/argv, an explicit-query flag, input hash and caller-supplied
  purpose (`observed` or `mechanism-fixture`);
- launcher path/hash/version and repository HEAD, branch, dirty count/fingerprint
  before and after the native call;
- native exit status, timeout status, stdout/stderr hashes and byte counts;
- whether before/after state and launcher bytes agree.

It writes a **new mode-0600 receipt**, refuses overwrite/symlink redirection and
requires an external or git-ignored destination. Native stdout/stderr bytes are
forwarded, buffered per stream; native exit status is retained. Recorder failures
produce explicit diagnostics, not invented successful snapshots. A failed
postflight is marked incomplete; receipt-write failure does not hide the native
output. Raw findings and unrelated environment variables are not stored. The
tool does not resolve secrets; it does store the exact prompt, which must not
contain credentials.

`purpose` is caller metadata, not proof of naturalness. `before_after_equal` is
not an atomic snapshot guarantee. The recorded binary is the **launcher**: native
Pixel may delegate to an existing daemon, whose serving binary this utility does
not attest. It is an input-provenance collector, not a source-build benchmark
harness. Future gold review and dedicated runtime verification remain necessary.

### Actual source-CLI mechanism checks

Used the already-built current-source launcher, SHA256
`c866661ca42c19ac2424d2a8e93d90a6f494283c2c3784bd1768c2bbf482e8a7`, against the
isolated source runtime worktree. Automatic daemon startup was disabled for these
checks; no user daemon was stopped or reconfigured.

1. An implicit lookup recorded native exit 0 and retrieved the expected source
   file. HEAD stayed `5c894b1...`, launcher hash stayed unchanged, mode was 0600,
   and the saved completion hashes matched both forwarded streams.
2. An explicit hotspots call retained its empty prompt, explicit query and limit
   of one, with native exit 0 and one finding.
3. A deliberately invalid by-concept call without a prompt retained native exit
   **1**, stderr/stdout hashes and a complete *receipt*. Receipt completeness does
   not imply command success.

All three are marked **mechanism-fixture** and excluded from the observed corpus.
No new affirmative/mixed data was fabricated from these software checks. No
accuracy or validated speed claim is made; elapsed receipt fields are incidental
native-call wall clocks, not model benchmarks.

## Use on a future genuine task

Run this only when there is an actual planning request to execute, with a fresh
receipt filename:

```sh
python3 scripts/bench/record-plan-request.py \
  --pixel .pixel/experiments/plan-routing-current-source-v1/pixel-current \
  --repo "$PWD" --output .pixel/plan-requests/NEW-ID.json \
  --prompt '<actual coding request>' --json
```

An explicit audit can instead use `--query dead-code` (no invented prompt). Keep
that lane separate from implicit-routing training/evaluation. No startup script,
agent rule, global configuration or default command behavior was changed.

## Checks, artifacts and disposition

Executed **14 extractor/recorder tests**, Python compilation and the three native
source-CLI mechanism checks. Tests cover literal-vs-quoted calls, explicit-query
separation, cwd-aware action joins, output/exit preservation, snapshot changes,
invalid metadata, incomplete postflight, timeouts, receipt write failure,
overwrite refusal and symlink refusal. `scripts/ci/gates.sh` passed **67 tests**
(36+12+9+10), explicitly skipping Cargo gates for docs/bench-only changes. No Rust
edit, build, install/doctor, mutants, learned-router replay/training, push or PR.

New source files:

- `scripts/bench/bench-plan-observed-corpus.py`
- `scripts/bench/record-plan-request.py`
- `scripts/test-plan-request-capture.py`

Private derived inputs, source references, exclusions, both audit passes, final
gold, mechanism receipts and gate logs are under
`.pixel/experiments/plan-routing-observed-v1/` (directory mode 0700). No historical
request text or transcript was added to tracked fixtures. Discovery snippets and
unrelated turn text were minimized after review; their hashes and source references
remain. Re-running retrieval needs the retained Pixel recall index; this archive
is not a full transcript backup. A private mode-0600 archive is retained at
`~/code/pixel-experiment-artifacts/plan-routing-observed-v1.tar.gz`.

**Keep the current product behavior and pause routing-model work.** The available
observations do not justify another model, another all-negative comparison or a
default switch. Resume evaluation only when naturally occurring, independently
reviewed implicit audits and mixed intents meet the frozen coverage requirements.
The recorder is ready to collect that evidence during real work; it does not
manufacture the missing demand.
