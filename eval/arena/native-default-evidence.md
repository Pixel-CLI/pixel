# Codex native-default experiment (Task 700)

The target is a routing contract: ordinary repository questions should receive
no Pixel retrieval instructions or forced Pixel command. The current package
removes the classifier and caller-facts hook; the experiments below explain why.
Only an explicitly requested impact skill or Pi extension command remains.
This removes a systematic source of extra model work, but cannot guarantee
identical answers, tokens or latency across stochastic model runs. Independent
task hooks and optional skill metadata still have a cost.

## Measurement corrections

`rank.py` now selects the requested tasks, arms and repetitions from one output
directory. It reports failures and missing runs separately and compares complete
arm pairs. Each metric uses the same eligible repetitions for every arm.
Required-pattern coverage is normalized within each scenario before aggregation;
it is not a semantic correctness judge. Missing usage remains unknown. Cached
input is a subset of reported input tokens and is not added again to the total.

The original all-directory `8.3/16` versus `7.7/16` result mixed scenarios with
different denominators and incomplete runs. It is not evidence of a measured
semantic-quality difference. The generic-task cost regression remains a reason
to change the default.

## Candidate 1: shorten permanent guidance

Source: HEAD `411635116787a66fd1bc713dd319a8dfdd817978` plus tracked diff
`b6cfb9a37263125645db241326e9e8dc8efdca294608e0e662ce0e0a4d72751b`;
new classifier hash
`f60b678713be090b5628b3bb70bb9f3558041f065477b0f4b53fea31e841c707`.
Pixel image:
`sha256:41a5a7283fd70ce3443aa28034226d3cdd0002975fcbab32dac0580eac516542`.
Codex `0.160.0`, `gpt-5.6-terra`, medium effort, foreign `architech-t` snapshot.

The one-repetition `g1-native-default-r1-pair` run is diagnostic only: raw's
container exited 1 after producing an answer because of the shell wrapper.
Its apparent improvement must not be counted as a successful comparison.

The clean `g3-native-default-r1-pair` was ranked with:

```bash
rtk python3 eval/arena/rank.py \
  --results eval/arena-results/g3-native-default-r1-pair \
  --scenarios-dir eval/scenarios --arms raw pixel \
  --tasks g3-rename-modal --reps 1
```

| Measure | Raw | Pixel |
| --- | ---: | ---: |
| Complete paired runs | 1 | 1 |
| Required-pattern coverage | 11/11 | 11/11 |
| Input tokens | 40,667 | 94,618 |
| Generated tokens | 615 | 1,178 |
| Total tokens | 41,282 | 95,796 |
| Cached input tokens (already included) | 36,096 | 85,504 |
| Wall seconds | 25 | 48 |
| Pixel calls | 0 | 0 |
| Native commands | 7 | 19 |

Neither arm reported dollar cost or cache-creation usage. One pair does not
identify the cause of the larger Pixel run, but it does reject a success claim
for that candidate. Removing forced calls alone was insufficient.

## Candidate 2: no permanent Codex retrieval context

Installation now removes retired Pixel blocks from global/project Codex
`developer_instructions` and project `AGENTS.md`, preserving foreign text.
Other agents keep their existing dedicated prompts. Generic Codex prompt hooks
emit nothing; compaction does not reintroduce the old target packet; native
search remains native under every Pixel policy mode. Foreign hook decisions
remain effective.

The installed macOS binary was exercised with each scenario prompt through
`pixel run-hook prompt-submit --provider codex`, using valid JSON on stdin.
The three generic scenarios emitted zero bytes and exited 0; the natural
`g4-transfer-callers` prompt emitted a valid optional graph hint and exited 0.
Each invocation took 13–14 ms in this single local probe. This is hook
startup/decision latency, not an arena speedup. The binary hash and exact
responses are in `target/native-default-evidence/verified-installed-hooks.json`.
Both Codex config files and the project AGENTS file were separately checked
for absence of the retired blocks.

The `g3-native-default-r1-candidate2` pair used Codex `0.160.0`, Terra medium,
and image
`sha256:ec9f12a8e0720ea4413aa9d713b84a01df7c8697d9a7489b9c763d43f499af27`.
The immutable image identifies this benchmark; the run's `pixel_source_id`
field is only `local`, so it does not establish an exact final source revision.

```bash
rtk python3 eval/arena/rank.py \
  --results eval/arena-results/g3-native-default-r1-candidate2 \
  --scenarios-dir eval/scenarios --arms raw pixel \
  --tasks g3-rename-modal --reps 1 --assert-context-parity
```

| Measure | Raw | Pixel |
| --- | ---: | ---: |
| Complete paired runs | 1 | 1 |
| Required-pattern coverage | 11/11 | 11/11 |
| Input tokens | 40,427 | 40,762 |
| Generated tokens | 454 | 536 |
| Total tokens | 40,881 | 41,298 |
| Cached input tokens (already included) | 35,072 | 34,048 |
| Wall seconds | 20 | 26 |
| Pixel calls | 0 | 0 |
| Native commands | 6 | 7 |

Static instruction manifests matched, and neither arm called Pixel. Tokens
were approximately equal (+1.0% for Pixel); wall time was higher in the Pixel
run. This is evidence of matching static context and native retrieval, not a
demonstrated speedup or a universal non-regression claim.

Source inspection also checked the two answers' central claim: each app's
client page imports its own alias-resolved component, both component files have
identical bytes, and they are separate files. Both tsconfig aliases resolve
`@/*` to that app's `./*`. The answers agree on this distinction; the rubric
alone would not establish it.

The first `g4-transfer-callers-candidate2` pair had no graph database. The hook
therefore correctly abstained: both arms made zero Pixel calls and scored
18/18. Raw used 79,182 tokens / 26 seconds; Pixel used 77,922 / 25 seconds.
This is another abstention control, not evidence of graph benefit. A graph-ready
pair must prepare the graph before model execution and record that setup cost.

Static manifest parity does not by itself prove equal dynamic host context;
installed hook responses and transcripts are separate evidence.

The graph-prepared `g4-transfer-callers-graph-prepped-candidate2` pair created
a 1,880,064-byte graph in 1,045 ms before model execution. Both answers covered
18/18 patterns; raw used 81,215 tokens / 31 seconds and Pixel used 98,012 /
38 seconds. Both made zero Pixel calls. Investigation found that the disposable
Codex home had not trusted its installed hooks. A separate direct invocation
returned the expected hint, but that does not show delivery to the model.
This pair cannot establish whether the routing intervention helps or hurts.

Codex 0.160.0 requires trust for new or changed non-managed hooks. Subsequent
hook-routing experiments must verify the hook actually executes and retain its
returned context separately from the static instruction manifest. Any isolated
trust bypass must be restricted to enumerated, reviewed hook commands and used
equally in both arms; it must not change the user's real hook trust.

The first trust-bypass diagnostic, `g4-trusted-hook-pair`, completed with
18/18 coverage in both arms. Raw used 80,765 tokens / 39 seconds and Pixel
66,053 / 29 seconds, including one `who-calls 'transferPageToGhost'` call.
However, its Pixel context manifest contained 1,446 characters of permanent
developer instructions while raw contained none, and no instrumented prompt-hook
response was captured. It therefore does not validate the native-default
candidate or isolate the classifier's contribution. Investigation confirmed
that the temporary runner recorded candidate 2's digest but launched the mutable
`pixel-arena:pixel` tag, which still pointed to candidate 1. Its recorded image
identity is invalid. A no-model probe of each immutable image confirmed that
candidate 1 creates the static instructions and candidate 2 does not. The
corrected runner must assert the actual container image before model execution.

Independent source review found both answers correct: the API POST route and
CLI main are the two direct callers, the status gate is “Ready to Publish,”
and title matching selects `updatePost` versus `createDraft`, which lead to
`posts.edit` versus `posts.add`. The four relevant source files had identical
hashes between snapshots. Pixel's graph listed the two callers as probable;
native search and source reads verified the answer.

The corrected `g4-candidate2-trusted-hook-r2` pair verified the actual Pixel
container image against the pinned candidate 2 digest. Both static instruction
manifests were empty. Graph preparation took 578 ms, and the runtime hook
receipt confirmed delivery of the 367-byte optional hint, identical to the
installed binary's response. Raw used 77,934 total tokens (77,199 input,
65,280 cached input, 735 generated) in 34 seconds; Pixel used 61,133
(60,314 input, 50,176 cached input, 819 generated) in 32 seconds. Neither
arm called Pixel. Coverage was 18/18 versus 16/18: the Pixel answer omitted
the helper name `findPostsByTitle` while correctly explaining exact-title
matching, both callers, the status gate, and the create/update Ghost methods.
Both answers were substantively correct on the requested behavior. This one
pair shows the optional hint was delivered and ignored, not a demonstrated
retrieval benefit.

## Candidate 3: require a graph call (rejected)

The next override asked the model to run `who-calls` once. The first trial
exposed a scenario defect: `/apps/notion-to-ghost` was interpreted as an absolute
working directory. The scenario now says `apps/notion-to-ghost under the
repository root`. Its remaining question and rubric are unchanged. Results
before and after that correction are not pooled.

The two corrected trials used the same pinned candidate 2 image, empty static
context, and an audited hook override. Receipts confirm that the 376-byte
instruction reached the model and each Pixel run successfully called
`who-calls`. These are prompt-override experiments, not a new production image.

| Run directory in `eval/arena-results/` | Raw tokens / seconds / coverage | Pixel tokens / seconds / coverage |
| --- | --- | --- |
| `candidate3-g4-path-corrected` | 62,534 / 30 / 14 of 18 | 65,563 / 27 / 18 of 18 |
| `candidate3-g4-confirmation` | 63,095 / 25 / 18 of 18 | 85,047 / 29 / 18 of 18 |

Median total tokens increased from 62,814.5 to 75,305 (19.9%). Both arms
described the requested behavior correctly; raw's first answer omitted the
full repository-relative paths required by four rubric patterns. A forced
tool call did not demonstrate a substantive quality benefit and was rejected.

## Candidate 4: supply bounded caller facts

Instead of requiring another model tool turn, the hook supplied two caller
locations from a real graph query. The exact 217-byte context is recorded with
hash `a93d7416` (prefix) in the runtime receipts. It labels the callers as
incomplete indexed candidates and asks the model to verify source. Static
context stayed empty. The image, model and corrected scenario were held fixed;
only the dynamic context changed.

| Run directory in `eval/arena-results/` | Raw tokens / seconds / coverage | Pixel tokens / seconds / coverage |
| --- | --- | --- |
| `candidate4-g4-facts-r1` | 62,062 / 24 / 14 of 18 | 47,501 / 23 / 14 of 18 |
| `candidate4-g4-confirmation` | 67,092 / 31 / 18 of 18 | 48,511 / 29 / 14 of 18 |

The precompute query took 179 ms, separately from install (52 ms) and graph
preparation (682 ms). All source snapshots used commit `5c478747` (prefix).
The precompute and source-verification receipt is
`target/arena-g4-hook-audit/candidate4-query/output/candidate4-precompute.json`.
Each Pixel run made zero model-time Pixel calls; transcripts show native search
and source reads. Median total tokens fell from 64,577 to 48,006 (25.7%);
median model wall time fell from 27.5 to 26 seconds, before the separately
measured lookup. Gross tokens are not billed dollars: cached input is included
and prices were not measured.

All four answers were substantively correct on callers, publication status,
title matching and Ghost create/update methods. Both Pixel answers and the
first raw answer abbreviated file paths, losing four required-pattern points.
This does not establish equal rubric quality. A separate citation-format
experiment must retain its own result and context hash.

Reproduce any row's coverage with:

```bash
rtk python3 eval/arena/rank.py \
  --results eval/arena-results/<run-directory> \
  --scenarios-dir eval/scenarios --arms raw pixel \
  --tasks g4-transfer-callers --reps 1
```

The direct-facts experiments used a precomputed hook override. They do not
establish production-hook timing or correctness; the bounded read-only
implementation requires separate installed validation.

## Candidate 5: preserve citation paths (mixed result)

`candidate5-g4-citations` changed only the dynamic prefix to ask for full
repository-relative citations and a search for additional callers. Its exact
297-byte context has hash prefix `d2305bf9`. Both answers covered 18/18
patterns and correctly explained the source. Raw used 47,745 total tokens
(47,026 input, 38,144 cached input, 719 generated) in 44 seconds. Pixel used
67,696 (66,942 input, 56,320 cached input, 754 generated) in 29 seconds,
with zero model-time Pixel calls. Setup took 882 ms; the original 179 ms
precompute was reused, not remeasured. Runtime receipts confirm delivery,
empty static context, the pinned image and matching source snapshots.

Equal rubric coverage came with more gross tokens and less wall time in this
pair. Together with candidate 4, the evidence is mixed; it does not justify
enabling caller facts automatically. The default therefore remains native,
and the bounded implementation is retained only behind explicit experimental
opt-in. No candidate demonstrates universal non-regression, and no result is
being reported as a dollar-cost saving.

## Earlier candidate validation

The final rebuild and self-update exited 0. Global installation reported 13
green steps; absolute-path repository installation reported 8. Configuration
generation and the history index rebuild exited 0. Final
`pixel doctor . --fix --fail-on yellow --json` exited 0 with 32 green checks,
0 yellow and 0 red; it repaired facts freshness and left no unresolved repair.
The complete local receipts are under `target/native-default-evidence/`.

Focused checks passed: 474 install tests, 122 Codex guard/hook tests, and
formatting. These are separate from the full workspace and CI gates.

The first full run on `e5d53296f53df998d2126cd31c3c32defd4ee0ad` found six
stale migration/search contract assertions and four timing-sensitive failures.
The assertions were corrected; all four timing cases passed serially with their
original thresholds and no retries. On frozen commit
`9fd435bf08f3146efa4d366c02be4d0811fe8037`,
`NEXTEST_TEST_THREADS=2 CARGO_BUILD_JOBS=2 NEXTEST_RETRIES=0 rtk proxy bash scripts/gates.sh`
exited 0: 3,863 tests passed, six skipped, and formatting, Clippy, doctests and
the script contracts passed. The same commit's review gate exited 0 with no
BLOCKER or CONCERN; its lower-severity findings were capped at 200, so it is
not an exhaustive review. The remote campaign on that commit tested 94 mutants:
65 caught, 13 unviable, 16 missed, zero timeouts (exit 2). Nine classifier and
seven installation coverage gaps were addressed in `a4984278`; focused tests
passed. That is not a passing mutation verdict. New implementation changes and
CI require fresh validation.

## HISTORICAL / SUPERSEDED: Installed native default and experimental caller facts

This section records an earlier installed candidate and its probes. It is
superseded by the current explicit-only skill distribution and the retirement
of the caller-facts hook. Preserve the measurements as historical evidence;
do not treat them as behavior of the current package.

After the final routing decision, `pixel self-update --repo . --build
"rtk cargo build --profile dev-release -p pixel-cli" --metrics off` rebuilt
and installed the binary in 97.52 seconds (exit 0). Its SHA-256 is
`7fdea364119ea4656b43beb5879242dcb1f8fe1e14a5c12b8d8ec717eef65c25`.
The source identity and build output are in
`target/native-default-evidence/facts-build.json` and `facts-build.log`.
At that historical source identity, this candidate was installed for the
recorded probes; it is distinct from both the arena overrides and the current
explicit-only package.

The history-index track indexed 1,045 base files, 18 overlay files and
2,238 commits, exiting 0. In parallel, `build-agent-config` exited 0;
global installation returned 13 green steps and absolute-path repository
installation returned eight. The subsequent
`pixel doctor . --fix --fail-on yellow --json --metrics off` exited 0 with
32 green checks, zero yellow/red/skipped checks, and no repairs needed.

The probe read the exact command registered in `~/.codex/hooks.json` and
invoked that installed command with valid Codex events against the foreign
repository. All ten probes passed: g1–g4 emitted zero context by default;
with `PIXEL_CODEX_CALLER_FACTS=1`, only g4 emitted the two verified caller
locations; explicit native-only and unknown-symbol questions emitted nothing.
Single-call local timings ranged from 10.49 to 14.78 ms, including process
startup. This is hook-level verification, not a model benchmark or guarantee.
The exact commands, payload hashes, outputs and binary hash are retained in
`target/native-default-evidence/installed-facts-proof.json`.

The production packet quotes names and paths and explicitly labels them as
repository data; its ordering and text differ from the precomputed benchmark
overrides. The earlier token results must not be attributed to this exact
production packet. Its default is disabled; regression tests and final gates
are tracked separately below.

Scope: these runs exercise `pixel install`, not native plugin installation.
That historical candidate's generated general Pixel skill had a broad
`.pixel/`-presence trigger.
Adding narrower skills alongside it would not establish native-default
behavior for plugin users. Skills can improve discovery and defer their bodies,
but semantic activation is not reliable command interception; any separate
skill-routing experiment must account for its always-visible metadata and
on-demand loading cost.

## Preliminary installed proof and Claude pilot status

The preliminary installed proof recorded 14 global and eight repository install
checks, with 33 doctor checks green and no automatic Claude or Codex retrieval
callbacks. The saved manifest is
`target/native-default-evidence/skills-first-install/installed-native-manifest.json`;
its installed binary SHA-256 is
`bbe297066a628a02084e9b95d646f6b05ea0643eea4ab7d258c97b78b41297b4`. This
predates the corrected approval check and final validation below. These are
installation checks, not model benchmark results.

The separate Claude g5 pair was not run: zero paid calls. A private isolated
auth check found an expired access token and expired refresh token. Claude's
local `auth status` reported stored login state but did not validate or renew
that credential, so it is not treated as usable authentication. The pair stays
blocked until a fresh OAuth login is available. Anthropic documents macOS
Keychain storage, per-config-directory credentials, and the `/login` renewal
path in its [authentication guide](https://code.claude.com/docs/en/authentication).

## Skill-only pilot: g3 negative control and g5 structural question

These two one-repetition Codex pairs test the packaged `pixel-impact` skill
without installing Pixel hooks or permanent retrieval instructions. Raw and
candidate used the same prepared foreign repository, pinned Pixel image, model
and effort. Raw had no staged skill; the candidate staged a copy with
`allow_implicit_invocation: true`. Other base context was equal and contained
zero Pixel-reference lines. Both runs' `skill-stage-*.json` and `context-*.json`
receipts confirm that distinction. The separate, unrun Claude comparison uses
disabled skill metadata in its raw arm and is not evidence from these pairs.
A candidate-discovered skill is not proof that its full body was read or followed.

Provenance shared by both runs:

- Pixel source snapshot tree SHA-256:
  `3aff229d9781ab0dce14b64bf1c83d7a5ee8be15dff685060ddcda1429a969fe`
  (1,165 files, 49,559,495 bytes), copied to
  `target/native-default-evidence/arena-build-context` before the Docker build.
  The copy excludes generated results, `.git`, build output, and local virtual
  environments. The exact copied tree, not later live-checkout edits, identifies
  the build input. Source checkout HEAD was `ac35266086d1bfba54e3370007246a9e0f389dca`.
- Image ID `sha256:57526239cc4628d824e4944476d6f10441126725710c83d0c00b2cc50a39c23f`,
  based on `sha256:a858f1356b2fb318023c9610ae873cf0f1e156ca1516b0cc7604fa1a9fb27cce`.
  Pixel binary SHA-256:
  `a3614721e10c46a09bcb13aa07af21fbcea004a35eaddca3b1001e058d7ab8f2`;
  Pixel `0.7.0`, Codex CLI `0.160.0`.
- Packaged skill tree SHA-256:
  `850aa3e96222a1c43ee32852f517a3850ce42a55e2784c1216d5767fc30fbd7f`;
  staged implicit-invocation copy SHA-256:
  `c3d47e3897e8a17e9efcdd202cd691ce81a08bb075e2335d9cb6d40100699ed2`.
- Foreign repository `architech-t`, commit
  `5c47874700c23a6c9e976de3f553ffe75bac39d8`; only generated `.next` and
  `.turbo` artifacts were dirty, and the runner clones the committed tree.
- Build receipt and full build log:
  `target/native-default-evidence/arena-build-context-receipt.json` and
  `target/native-default-evidence/arena-snapshot-build.log`.

The selected task/run identities and exact arena commands were:

```bash
REPO_SNAPSHOT=/Users/livio/Documents/architech-t \
PIXEL_SRC=local \
PIXEL_SOURCE_ID=3aff229d9781ab0dce14b64bf1c83d7a5ee8be15dff685060ddcda1429a969fe \
PIXEL_IMAGE_SOURCE=existing \
PIXEL_ARENA_IMAGE=sha256:57526239cc4628d824e4944476d6f10441126725710c83d0c00b2cc50a39c23f \
CODEX_MODEL=gpt-5.6-terra CODEX_EFFORT=medium \
RUN_ID=g3-skill-negative-r1 \
rtk bash eval/arena.sh --arms "raw pixel" --tasks g3-rename-modal --reps 1 \
  --skill-candidate-dir skills/pixel-impact \
  --results-dir eval/arena-results/g3-skill-negative-r1

REPO_SNAPSHOT=/Users/livio/Documents/architech-t \
PIXEL_SRC=local \
PIXEL_SOURCE_ID=3aff229d9781ab0dce14b64bf1c83d7a5ee8be15dff685060ddcda1429a969fe \
PIXEL_IMAGE_SOURCE=existing \
PIXEL_ARENA_IMAGE=sha256:57526239cc4628d824e4944476d6f10441126725710c83d0c00b2cc50a39c23f \
CODEX_MODEL=gpt-5.6-terra CODEX_EFFORT=medium \
RUN_ID=g5-skill-structural-r1 \
rtk bash eval/arena.sh --arms "pixel raw" --tasks g5-transfer-status-impact --reps 1 \
  --skill-candidate-dir skills/pixel-impact \
  --results-dir eval/arena-results/g5-skill-structural-r1
```

| Pair / arm | Required-pattern score | Input / cached input | Output | Gross tokens | Model time | Model tool calls | Pixel calls / skill reads | Prep time |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| g3 raw | 11/11 | 40,844 / 36,096 | 542 | 41,386 | 23s | 2 | 0 / 0 | 496ms |
| g3 skill | 11/11 | 55,205 / 49,152 | 789 | 55,994 | 29s | 3 | 0 / 0 | 579ms |
| g5 raw | 16/16 | 67,259 / 55,296 | 1,186 | 68,445 | 45s | 3 | 0 / 0 | 897ms |
| g5 skill | 14/16 | 96,852 / 73,472 | 1,485 | 98,337 | 80s | 4 | 1 / 1 | 1,323ms |

All four containers exited successfully; each pair has one complete observation.
Gross tokens are reported input plus generated tokens; cached input is already
included in input. Dollar cost and cache-creation usage were not reported.
Setup preparation is separate from model time. These are noisy single pairs,
not causal estimates or a no-regression guarantee.

On g3, both answers were substantively correct: they identified the root
consumer and definition, the separate sanity-check consumer/copy, and the
independent alias resolution. Raw used 41,386 tokens and 23 seconds; the skill
arm used 55,994 and 29 seconds, a 35.3% token increase with equal pattern
coverage. The candidate skill was discovered but had zero file-read commands
and zero model-time Pixel calls.

On g5, source inspection confirmed the API route checks `Ready to Publish`
while the CLI bypasses that check; both call `transferPageToGhost`. That shared
method performs exact-title lookup through `findPostsByTitle`, then updates an
existing post or creates a draft. Raw named the lookup helper and scored 16/16.
The skill answer correctly described the callers, policy gap, shared enforcement
boundary, and Ghost update/create branches, but called the lookup only “title
lookup” and did not name `findPostsByTitle`, scoring 14/16. Its one model-time
Pixel call was `pixel impact 'transferPageToGhost' --no-refresh --depth 2 --json
--metrics off`; it also read the candidate skill once. Despite that structural
lookup, the skill arm used 98,337 tokens and 80 seconds versus raw’s 68,445 and
45 seconds. The result does not meet the proposed automatic-promotion bar.

The g5 scenario marks `human_review_required: true`. The answer check above is
an assistant's source-grounded review of the transcript against the checked-out
files, not human approval; that requirement remains outstanding. Full transcripts,
manifests, per-arm usage, staging and setup receipts are in the two result
directories. Scenario hashes are g3
`3d1a5762cca72f93ef2f3be7f7838c3ccdd2d300aa321027ce5829e4ce922c08` and g5
`0127bd4abb2064d150df4697f1f9f0aabc7fd192005f17f1beb9449dbb311fb9`.

Neither pair justifies automatic skill activation. Keep the packaged skill
explicit-only by default; use more controlled, separately reviewed pairs before
making a promotion decision.

## Initial installed candidate verification (2026-10-05)

Run identity: `target/native-default-evidence/frozen-install/`. This verified
the stable working-tree candidate based on `ac35266086d1bfba54e3370007246a9e0f389dca`,
including its then-uncommitted changes; the base SHA alone does not reproduce
this binary. Installed binary SHA-256:
`34da805fa0ed2d026392958f01ffe732a67458d644e02f317502fbb5b9f2ffc7`.

`pixel self-update --repo . --build "cargo build --profile dev-release -p pixel-cli"`,
`pixel build-index --history .`, `build-agent-config`, `pixel install`,
`pixel install --repo .`, and `pixel doctor . --fix --fail-on yellow --json`
all exited 0. The doctor result is 33 green, zero yellow, red, skipped or
repairs. Each global Claude/Codex config contains nine task-event callbacks
and zero automatic retrieval/metrics callbacks; repository configs contain
no Pixel callbacks. Pi's explicit extension is present and its retired
automatic prompt is absent.

`cargo test -p pixel-install --no-fail-fast` exited 0: 365 unit tests and
124 integration tests passed. The arena parser/runner suite passed 46 tests.
These scoped results precede the final committed-candidate workspace gates;
their logs and installed callback/hash manifest are retained in that run directory.

An independent migration review subsequently found that the doctor's hook-review
check accepted any saved approval hash without comparing the current definition.
The 33-green result above therefore proves configuration presence, not current
Codex approval. A read-only `hooks/list` request to Codex 0.160.0 on candidate
`c8c71d697e81fa9fd4da6b939867c85f37c5c712` reported all nine global Pixel task
hooks enabled: seven trusted and two modified (`PostToolUse` and
`UserPromptSubmit`). The receipt is
`target/native-default-evidence/c8c71d69-gates/codex-hook-discovery.json`.
No user approval was written by this verification. The corrected installed
doctor reports 32 green checks and one yellow check for those same two hooks;
`--fail-on yellow` correctly exits 1 pending user review. Project task-hook
deduplication requires current-definition approval and preserves registrations
when profile overrides cannot be verified. Project paths aliasing the global
hook file are left untouched, including an absent leaf in a symlinked directory.

Nine portable hash vectors, using `/usr/local/bin/pixel`, were independently
obtained from Codex 0.160.0 through `initialize`, `initialized`, and `hooks/list`
in an isolated fixture. No model request or trust write was made. The sanitized
receipt is `target/native-default-evidence/codex-trust-probe/receipt.json`;
the fixture hook configuration has SHA-256
`814a9a674967becfa9b7cad837ec22d926e8811803c172fa64610e9eb4994290`.

## Final implementation validation

Frozen implementation commit: `e50538a17df65c6338cd84b825da6a7b6ed7b44e`.
`scripts/gates.sh` exited 0: 3,904 workspace tests passed, six skipped, with
formatting, Clippy, documentation tests and harness contracts passing.
`scripts/mutants-remote-gate.sh` exited 0 using the passing cache; no new mutants
were tested. Production code is unchanged from `328e835a`, whose remote retry
caught both remaining mutants with zero misses or timeouts. `cargo deny check`
passed against the same lockfile. Logs and exit receipts are under
`target/native-default-evidence/e50538a1-gates/` (dependency-check evidence is in
`target/native-default-evidence/bedeb743-gates/`).

Self-update, index refresh, configuration generation, global install and
repository install all exited 0. The final doctor invocation exited 1 with
32 green checks, one yellow and no red checks: the two modified Codex task hooks
still require the user's `/hooks` review. This is an outstanding local approval
step, not a passing installation checklist; no trust state was written.
