# Pixel brief: question-kind and tool-chain experiment

Date: 2026-10-07. Tracking: [#877](https://github.com/Pixel-CLI/pixel/issues/877);
candidate: [#876](https://github.com/Pixel-CLI/pixel/pull/876).
This report replaces the earlier continuation checklist with measured results.

## Finding

Pixel can retrieve useful evidence for all eight selected question kinds.
That does not make a compulsory Pixel tool chain efficient. Two explicit
skill treatments recovered missing evidence on three foreign-repository tasks,
but used more model tokens than native retrieval in every task group.

The useful unit is a compact packet containing the **requested facts**:
a real signature, export and initializer, reference paths, or named test
assertions. File locations alone can save tokens while producing incomplete
or false answers. Classification is optional routing help, not evidence;
the remote classifier confused two of nine selected prompts.

These findings do not establish general superiority over native tools.
The source-aware eight-kind probe measures retrieval only. Live answer
comparisons cover three tasks, not eight kinds or unseen repositories.

## Runs and measurement boundaries

| Run | Scope | Identity and control |
| --- | --- | --- |
| Historical delivered hooks | 3 tasks × 3 paired repetitions = 18 answers | Foreign snapshot `5c47874700c23a6c9e976de3f553ffe75bac39d8`; nine valid delivery receipts |
| Stale local index | Eight direct brief requests | Pixel source/binary `8fe6e10cedd2ebecd3bde78ba84c910cd1cd62e1`; base shard still at `14feba4`, no delta |
| Prepared local retrieval v3 | 8 kinds × 3 repetitions × 2 arms = 48 records, 57 CLI invocations | Same `8fe6e10` binary and isolated source snapshot; no model |
| Classifier probe | Nine prompts | Same binary; remote Jev preset, `jev-latest`, temperature 0 |
| Skill A | 3 tasks × 3 paired repetitions = 18 answers | Foreign snapshot above; frozen skill, same container image in both arms |
| Skill B | 3 tasks × 2 paired repetitions = 12 answers | Same snapshot/image; typed source search, separate frozen skill |
| Packet C | Two lookup tasks; native agent and two precomputed-packet arms | See the packet comparison below |

All live answer comparisons use Codex 0.160.0, `gpt-5.6-terra`, medium
reasoning and fresh sessions. A/B keep task wording identical within a pair;
C appends source evidence to the original prompt in its packet arms.
The new skill runs reuse Linux image
`sha256:1426219784d53f6e86e4a305a96544a2baf5a648d12e894801ef8461b7f4ebd7`.
It reports Pixel 0.7.1; its source commit is **unverified**. Do not attribute
these Linux results to the exact local macOS binary or the final PR head.
The harness checkout was based on `e4068530dd9bc428f2a709c8d8fb2363ac6b18d4`.

The prepared local binary is Pixel 0.7.1 at `8fe6e10`, SHA-256
`a1b87d4e8de80c380958aa43d65365a8e0f5e22c0c3ffa076379a5fa9417d6e8`.
No global installation or hook configuration was changed.

Definitions used below:

- Tokens are input plus generated tokens. Cached input is already included;
  it is not added a second time. These are usage counts, not dollar costs.
- Paired saving is `100 × (raw − pixel) / raw` for each matched repetition,
  then the median. It is not the ratio of group medians.
- Arena seconds surround the Codex command, including a live hook when used.
  They exclude image, container, install, index, and preflight setup.
- Pattern scores are coverage signals. The semantic review below checks
  the actual answer against source; a passing regex cannot prove correctness.
- These are small, selected samples. There are no confidence intervals,
  held-out routing evaluation, or claims of calibrated classifier confidence.

## Historical hook evidence: delivery verified, completeness uneven

[Historical receipts, rows and hashes](bench/brief-question-kinds-2026-10-07/historical.json)
retain all 18 trials. Every Pixel receipt has return code 0,
`response_valid`, `forwarded_to_codex`, and `emitted_context` true, with
`hook_event_name: UserPromptSubmit`. Context contents, byte counts and
hashes are retained. Snapshot/static baseline parity was checked on the
first repetition of each task.

| Task | Raw scores | Pixel scores | Median paired token saving | Median seconds, raw → Pixel |
| --- | --- | --- | ---: | ---: |
| g4: rename impact | 12, 12, 12 / 12 | 12, 12, 12 / 12 | 35.3% | 26 → 15 |
| g7: handleError definition/signature | 10, 10, 10 / 10 | 3, 10, 10 / 10 | 2.4% | 11 → 13 |
| g8: CustomMenu definition/initializer | 7, 7, 10 / 10 | 7, 7, 7 / 10 | 67.6% | 8 → 6 |

The g4 group-median token ratio would show 52.4%; that is a different
statistic from the reported 35.3% median paired saving.

All nine Pixel transcripts contain **zero Pixel CLI calls**. This measures
injected brief context, not explicit tool chaining or classification.

- **g4:** the 706-byte context supplied useful file references despite
  unavailable graph evidence. Answers identified the definition and eight
  consumer files. This is the strongest historical result — its shared
  `apps/(site|project)/…Contact` rubric pattern credits either consumer,
  so a full pattern score does not prove the answer listed every affected
  file the prompt asked for.
- **g7:** the first 665-byte-context answer mistook seven consumer files for
  definitions and omitted the real declaration at
  `packages/ui/handleError.ts:4`. Its 3/10 score still credited a generic
  signature pattern. Later answers used native search and bounded reads,
  and correctly recovered the declaration.
- **g8:** all three 374-byte-context answers omitted
  `defineMultiStyleConfig({ variants })`. Repetition 2 additionally called
  `CustomMenu` a default export; it is a named export. The 7/10 score
  conceals that false statement. Raw repetition 3 read both files and
  answered completely.

Thus the largest token reduction is not automatically a quality win.

Historical raw and Pixel images differ:
`sha256:16e4f6d87b0705fe34c15f0f6869acb89c15ab8273f21590ba36f357650550e3`
and
`sha256:6c713ea2d70fbb80f5dd927615ce3aacf780669f656e5a02bad81df190cbdf86`.
Receipt image identities matched their expected values, but this is not the
same-image control used for the new skill runs.

The earlier `openai-brief-arena-20261007-01` and `-03` runs had invalid
hook receipts and no emitted context. Their scores are excluded. The
historical graph files were absent; this report does not pretend to replay
impact against them without preparation.

The frozen rubrics carry two known weak spots, kept verbatim: a `never`
pattern matching `modified` can penalize the honest sentence "No files
were modified", and g4's shared Contact pattern credits either consumer
rather than requiring both. Historical scores are read with those limits
in mind.

## Fresh index: useful evidence, not yet an efficient packet

Eight initial requests against the stale index exited successfully with
**zero stdout bytes**. Shard existence was not enough: the base named an
older HEAD and no delta covered the current commit. Those empty outputs
cannot establish that the eight question kinds are unanswerable.

The corrected v3 probe prepared an isolated `8fe6e10` checkout with
`pixel prepare-repo "${SNAPSHOT}" --no-daemon --json`: **8,001 ms**,
1,133 indexed files; graph 539 files, 8,887 symbols, 21,525 edges and 44,654
unresolved references. Preparation is separate from query timing.

[Fresh probe inputs, exact argv, outputs and timings](bench/brief-question-kinds-2026-10-07/fresh-probes.json)
record all 48 trials and 57 invocations. Each baseline is
`pixel brief "$PROMPT" "${SNAPSHOT}" --metrics off`. Each route is the
listed sequence of evidence commands; its time and output bytes are summed.

| Kind | Current brief evidence | Source-aware route evidence | Median ms, brief → route | Stdout bytes, brief → route |
| --- | --- | --- | ---: | ---: |
| Lookup | Unrelated file locations | Three budget constants and checks | 89 → 137 | 272 → 3,338 |
| Impact + tests | Definition/production location | Production call and named same-file provider/event test | 89 → 141 | 381 → 3,315 |
| Flow | Config/test file locations | Registration, event mapping, processing, attachment and host envelope | 101 → 150 | 251 → 36,228 |
| Diagnosis | Unrelated daemon files | Guard, fallback, no-rebuild behavior and test assertions | 409 → 148 | 259 → 14,055 |
| Configuration | Config/doctor locations | Exact dynamic task-event command and Codex event mapping | 457 → 148 | 276 → 15,106 |
| Tests | Definition/nearby locations | Pi exclusion, provider cases and envelope assertions | 73 → 289 | 381 → 10,947 |
| History | Empty | Recorded constraints for the requested commit | 30 → 48 | 0 → 4,229 |
| Architecture | Several file locations | Ownership signatures and source handoffs | 373 → 218 | 285 → 10,056 |

All invocations exited 0 and none reported result truncation. Output was
identical within each kind/arm across three repetitions. Repeated-query
advisories appeared on stderr but did not alter results.

The routes use `search-content` with source context, `list-signatures`,
and `commit-history`. **Identifiers and paths were selected after source
inspection.** This tests evidence availability once locations are known,
not automatic discovery, model answer accuracy, hook delivery, or overall
agent efficiency. Five routes were slower; every route emitted more text.
The long flow/configuration output is particularly unsuitable for direct
injection into a 2 KiB hook.

Rehearsal v1 used a removed impact identifier and an invalid absolute path
for `list-signatures`; v2's context window omitted the dynamic command.
They were corrected before v3 and are not pooled. Impact now uses the live
`start_brief` symbol, not the removed `build_decisions_request`.
This is a tuned development set, not a held-out evaluation.

The diagnosis boundary is conditional: the text index must cover HEAD and
at least one evidence operation must answer. A stale/missing graph can
leave text evidence available; zero successful operations render no brief.
The probe read the relevant tests; it did not deliberately stale its
prepared graph or establish a live application's root cause.

Eight frozen scenarios are saved as `eval/scenarios/qk-*.json`, with a
[semantic gold ledger](bench/brief-question-kinds-2026-10-07/expected-facts.json).
The impact prompt deliberately requests tests too: it is a mixed evidence
case, not a pure impact label. The diagnosis rubric scores the requested
behavior, not unasked regression-test names. Scenario structure and regex
compilation were checked. One live Arena run now covers every scenario, but
the Pixel prompt hook did not deliver context. Treat it as a no-hook control,
not as evidence for the brief's effect; details follow.

## Live question-kind Arena control: no brief delivered

One paired repetition ran all eight `qk-*` scenarios against the Pixel repo:
16 completed Codex answers, with zero failed or missing pairs. This closes
the live-matrix execution gap only for the no-hook control. The Pixel
`UserPromptSubmit` preflight receipt has `response_valid: false` and
`context_bytes: 0`; Arena warned that live Pixel brief evidence was disabled.
The helper still calls the retired `pixel run-hook prompt-submit --provider
codex` command, while the current installed command is
`pixel run-hook task-event --provider codex --event prompt-submit`.

| Kind | Regex score, raw → Pixel | Median tokens, raw → Pixel | Seconds, raw → Pixel |
| --- | ---: | ---: | ---: |
| Lookup | 4/8 → 4/8 | 66,132 → 120,089 | 25 → 27 |
| Impact | 6/9 → 4/9 | 73,285 → 79,395 | 21 → 16 |
| Flow | 10/10 → 10/10 | 355,244 → 363,041 | 66 → 51 |
| Diagnosis | 6/8 → 6/8 | 325,707 → 263,023 | 43 → 40 |
| Configuration | 8/9 → 8/9 | 274,683 → 192,412 | 35 → 23 |
| Tests | 8/10 → 8/10 | 145,926 → 203,218 | 14 → 34 |
| History | 6/11 → 8/11 | 78,593 → 131,820 | 26 → 24 |
| Architecture | 9/9 → 9/9 | 113,661 → 243,366 | 14 → 36 |

Across these one-shot rows, the macro regex score was 76.9% raw and 76.4%
Pixel; the mean of task-median tokens was 179,154 and 199,546, and mean
task-median time was 30.5 and 31.4 seconds. All eight Pixel answers made
zero Pixel CLI calls. Direct inspection found the 16 answers broadly
consistent with the requested source facts, so the regex percentages are
coverage signals, not semantic accuracy; no independent second scorer or
held-out sample was used.

This is not a controlled comparison of brief delivery. The captured context
manifests differ: raw has zero Pixel-reference lines and no Codex hook
configuration, while Pixel has 32 Pixel-reference lines and a hook
configuration. Raw stderr also records six repository skills rejected by
Codex for missing frontmatter descriptions. The prompt hook was not verified
as delivered, and the two arms do not have context parity. Do not pool these
numbers with the historical hook or skill runs, or use them to claim a
speedup or a Pixel effect. The run used headless `codex exec`; it did not
validate a Herdr TUI session.

Identity, per-row usage, scenario hashes, the preflight receipt, and harness
hashes are in the [no-hook Arena receipt](bench/brief-question-kinds-2026-10-07/qk-live-no-hook.json).
Raw transcripts remain in the local, gitignored
`eval/arena-results/qk-live-20261007-02/` directory. The Pixel image was
`sha256:acb729d951f9ea9772a896534011ebaff6336578846a2d3a4bb513aa888fd130`;
the raw image was
`sha256:16e4f6d87b0705fe34c15f0f6869acb89c15ab8273f21590ba36f357650550e3`.
Both arms used Codex 0.160.0, `gpt-5.6-terra`, medium effort, and one fresh
snapshot per arm. Arena wall time was 357 seconds, including the local Pixel
image build. A portable reproduction command is retained in the receipt.

## Classify: cheap routing hint, not a mandatory first step

The [classifier receipt](bench/brief-question-kinds-2026-10-07/classify.json)
contains the exact nine-label rubric, prompts, and winning decisions.
One remote invocation took 237 ms; an eight-prompt JSONL batch took
2,575 ms in total. The batch has no per-question timing or token/cost
receipt, and temperature 0 does not make a remote model deterministic.
The exact invocation argv and full label distributions were not retained;
the saved latency is diagnostic evidence, not a fully replayable benchmark.

Agreement with the frozen human routing labels was **7/9**:

| Prompt intention | Expected route | Returned route | Returned confidence |
| --- | --- | --- | ---: |
| Why a stale graph still leaves text evidence | diagnosis | history | 0.62 |
| Rename a symbol and find its direct tests | mixed_or_other | tests | 0.81 |
| Seven remaining prompts | Their single evidence kind | Agreed | See receipt |

The mixed request needs both impact and tests regardless of a top label.
No measured deterministic router comparison was run. These nine prompts
are too few to claim general accuracy, useful confidence thresholds, or a
net latency benefit.

`pixel classify --task-intent` uses broader task-intent labels; it does not
select these eight evidence routes. The hook's classifier path is
warm-local-only with a 400 ms child budget inside the 750 ms total window.
The remote Jev probe is not a measurement of that hook path.

## Explicit tool-chain skills: correct answers cost more

Skill A routes an exact symbol lookup through one literal
`search-content` call and a source read. Rename impact uses one
`impact --direction upstream --depth 1 --no-refresh` plus a literal search.
It asks for every requested fact and allows native fallback when graph
evidence is unavailable. No classify call, index maintenance, or hook runs
inside the model session.

Skill B adds `--type ts` to literal searches. That filter includes TSX,
MTS and CTS as well as TS. Both frozen skills disable implicit invocation
in the repository; the arena enables only its disposable candidate copy.

| Treatment/task | Pairs | Median tokens, raw → Pixel | Median paired token increase | Median seconds, raw → Pixel |
| --- | ---: | ---: | ---: | ---: |
| A / g4 impact | 3 | 42,974 → 67,256 | 56.5% | 33 → 37 |
| A / g7 signature | 3 | 42,033 → 73,969 | 75.5% | 10 → 16 |
| A / g8 initializer | 3 | 41,329 → 57,635 | 39.5% | 14 → 12 |
| B / g4 impact | 2 | 51,081 → 92,186.5 | 94.9% | 29 → 42.5 |
| B / g7 signature | 2 | 42,087 → 74,240 | 76.4% | 10 → 23.5 |
| B / g8 initializer | 2 | 41,172.5 → 56,068.5 | 36.2% | 15.5 → 17 |

All **30 answers** completed. A scored full marks on 16/18 answers:
Pixel g4 repetition 3 scored 9/12 and raw g8 repetition 3 scored 7/10.
B scored full marks on 12/12. The retained final answers allow semantic
review alongside the pattern scores; neither score nor token savings
should be treated as answer-wide confidence.
Pixel's partial g4 answer omitted the login consumer. Its score also lost
one point because “No files were modified” matched the broad `modified`
penalty: that point is a rubric false positive, not evidence of an edit.
Raw's partial g8 answer described the named configuration and variants but
omitted the `defineMultiStyleConfig` initializer. Original scores are kept.
Receipts: [A](bench/brief-question-kinds-2026-10-07/skill-a1.json),
[B](bench/brief-question-kinds-2026-10-07/skill-b1.json).

Raw A used two command tool calls per answer. Pixel A used 2–5, in addition
to reading its skill, and sometimes needed native follow-up. Output noise
also increased. Filtering generated JSON matches reduced the diagnostic
g4 query output from 11,867 to 9,007 bytes and g8 from 4,479 to 1,085 bytes;
g7 stayed at 5,012 bytes. It did not recover the model-token overhead.

A and B have different skill names as well as the filter change. Their
model comparison is not a strict one-variable ablation. The byte reduction
is a direct query comparison; an A-to-B token change cannot be attributed
only to filtering.

Frozen source:
[skill A](../eval/arena/skills/pixel-question-evidence/SKILL.md)
(SHA-256 `2df428f71b8a63e23d8ffa5f6113c63e55b518aa8dd497b0b233f5dbe95dbe4c`);
[skill B](../eval/arena/skills/pixel-question-evidence-filtered/SKILL.md)
(`ecfc93e79a54373114b94ae63ab7207eb7d9747cd81d03be08fb6fc3c1f6b6b4`).
These are experimental fixtures, not a new default agent skill.

## Precomputed packet comparison

The third treatment moves retrieval ahead of the first model turn. It
compares native-agent (original prompt), native-packet, and Pixel-packet
on g7/g8, with two repetitions per task/arm: **12 answers**.

The [separate Bun runner](../eval/brief-packet-pair.ts) extracts a backtick
identifier, searches matching TS-family file paths, then uses the **same
custom source packer** in both packet arms. The packer reads declarations
and local constants directly from files. Pixel supplies paths through
`search-content -F ... --type ts --files-with-matches`; Pixel itself does
not produce the final source packet. Native retrieval supplies those paths
through `rg -l -F`. No skill or prompt hook is added.

The complete emitted packets are byte-identical across backends: **1,025
bytes** for g7 and **1,045 bytes** for g8, below the 2,048-byte cap. They
contain the real `handleError` declaration and signature, and the named
`CustomMenu` initializer plus local variants. Matching paths, packet text,
hashes, prompts, answers and per-call timings are retained in
[packet C receipts](bench/brief-question-kinds-2026-10-07/packet-c1.json).

| Task / arm | Median tokens | Median charged seconds | Model tool calls per answer |
| --- | ---: | ---: | ---: |
| g7 / native-agent | 41,735 | 12.68 | 2 |
| g7 / native-packet | 13,399.5 | 7.59 | 0 |
| g7 / Pixel-packet | 13,399.5 | 7.87 | 0 |
| g8 / native-agent | 40,966.5 | 13.15 | 2 |
| g8 / native-packet | 13,396 | 6.23 | 0 |
| g8 / Pixel-packet | 13,394.5 | 7.75 | 0 |

All 12 answers scored 10/10. Independent transcript review confirmed the
requested location/signature or named export/initializer. The eight packet
answers needed no model tool calls.

Averaging the two task medians gives 41,350.75 tokens / 12.92 seconds for
native-agent; 13,397.75 / 6.91 for native-packet; and 13,397 / 7.81 for
Pixel-packet. Against native-agent, those aggregate ratios imply about
**67.6% fewer tokens** for both packet arms, and **46.5% / 39.5% less
charged time**, respectively. These are ratios of task-median averages,
not the median paired savings used in the historical/A/B tables.

Charged time includes query, packing, and the Docker model invocation.
Pixel search also starts a Docker container, while native search runs
on the host. The approximately 0.90-second difference between packet
arms is therefore a measured workflow difference, **not an isolated
Pixel-versus-ripgrep engine latency comparison**. Shared snapshot/graph
preparation and image acquisition are excluded. This is a prepared
fixture experiment, not cold-start total deployment cost.

The result supports preturn evidence packaging for these two selected
lookup questions. The native control shows that the gain is not unique
to Pixel. It comes from eliminating discovery turns while retaining
requested facts; making Pixel relevant here would mean providing this
bounded source-packet behavior directly, without requiring an agent to
read a skill and perform a serial discovery chain.

The runner is an experiment prototype, not a general TypeScript parser or
a production hook. Its declaration extraction and local-dependency scan
were checked on this frozen fixture, not on arbitrary syntax, ambiguous
symbols, no-match prompts, or eight question kinds. Current graph/context
commands were also probed: capped `pack-context` was truncated for
`handleError`, and `CustomMenu` was absent from that graph. Those failures
are why C tests literal path retrieval plus the shared source packer;
it does not demonstrate that the existing locate recipe solves the case.


## What this establishes about the product

The current prompt brief travels through `task_hook::process`,
`start_brief`, the execution-brief chain, `Pending::finish`/render, and
`with_brief` into host additional context. Pi is excluded. The hook keeps
its existing four-operation, 750 ms shared deadline and 2,048-byte output
limits; it does not initialize a daemon or refresh indexes.

The experiment identifies concrete information losses without claiming
they are all fixed by this documentation change:

- Literal matching text becomes locations. Distinct same-file sites
  collapse, hiding production and inline Rust test evidence.
- Six-word concept extraction can discard later requested constraints;
  the first anchor dominates retrieval.
- Evidence-presence confidence does not measure answer completeness.
- “Open a file only if it contradicts you” discourages reads needed to
  obtain a missing signature, initializer, or second requested fact.
- A broad generated-JSON exclusion also needs care for real configuration
  questions. The TS filter is suitable for the selected TS tasks only.

Existing source-rich commands are useful explicit follow-ups:
`run-recipe --kind locate --budget 300 --json` can resolve, collect source
context and find caller-derived test files; `pack-context` accepts a unique
symbol name or UID. Neither is a drop-in replacement for the read-only,
750 ms hook. Context budgets are approximate; enforce a serialized byte
cap and inspect truncation rather than assuming every required fact fits.
Graph-based test discovery also misses Rust inline tests.

A packet must distinguish observed references from exhaustive impact,
test discovery from a test run, and recorded history from inferred intent.
Missing evidence warrants a bounded source read even when existing facts
are correct. The measured skill regressions do not justify making every
agent start with classification and several Pixel calls.

## Reproduction and retained evidence

The committed JSON receipts retain trial rows, final answers, settings,
scenario contents/hashes, hook contents, normalized source outputs, and
original transcript hashes. Raw model
transcripts remain in the local `eval/arena-results/` run directories;
a hash provides identity, not public access to the underlying transcript.
No credentials or complete agent environments are published.

Recompute historical paired statistics from retained arena directories:

```sh
rtk proxy bun eval/brief-question-kinds-summary.ts \
  eval/arena-results/brief-hooks-g4-rename-impact \
  eval/arena-results/brief-hooks-g7-lookup-handleerror \
  eval/arena-results/brief-hooks-g8-lookup-custommenu
```

Replay a skill treatment with the foreign repository snapshot available.
Use a new results directory. Keep the image and source snapshot pinned:

```sh
rtk env REPO_SNAPSHOT=/path/to/architech-t \
  PIXEL_IMAGE_SOURCE=existing \
  PIXEL_ARENA_IMAGE=sha256:1426219784d53f6e86e4a305a96544a2baf5a648d12e894801ef8461b7f4ebd7 \
  CODEX_MODEL=gpt-5.6-terra CODEX_EFFORT=medium \
  bash eval/arena.sh --arms "raw pixel" \
  --tasks "g4-rename-impact g7-lookup-handleerror g8-lookup-custommenu" \
  --reps 3 --skill-candidate-dir eval/arena/skills/pixel-question-evidence \
  --results-dir eval/arena-results/question-evidence-new-run
```

For B, use the filtered skill directory and `--reps 2`.
For C, the standalone runner uses the pinned image above and checks out
the pinned foreign fixture commit in disposable copies:

```sh
rtk proxy bun eval/brief-packet-pair.ts \
  --repo /path/to/architech-t --auth /path/to/codex/auth.json \
  --graph /path/to/prepared/graph.v2.db \
  --results /path/to/new-packet-results
```

`--preflight-only` verifies matching paths and packet facts without calling
the model. `--graph` is optional for the runner; the frozen C run supplied
a prepared graph. Post-run portability/provenance fixes are listed beside
the original measured runner hash in the receipt; they were preflighted
without rerunning the 12 model answers.

The current reviewed-hook arena mode still refers to the retired
prompt-submit command; it does not reproduce the historical delivered-hook
runs without a separate harness repair. The new skill trials avoid that
mode and do not claim to validate host hook delivery.

Fresh-probe JSON replaces host paths with `${PIXEL_BIN}` and
`${SNAPSHOT}`; resolve those to the identified binary and an isolated
prepared checkout, then replay each recorded argv. Output hashes in that
file cover the normalized text. Repeated route outputs are stored once by
hash. The fixture scenarios and gold ledger expose the requirements for a
future live matrix without reporting unrun trials as results.

## Continuation 2026-10-07: brief-delivery path repaired and isolated validation

The arena's brief-delivery path called the retired `pixel run-hook
prompt-submit --provider codex` verb (`RetiredHookCmd::PromptSubmit` in
`crates/pixel/src/main.rs:1891`: accepted, does nothing, exits 0). This
continuation repairs the two call sites inside `eval/arena/entrypoint.sh`
and validates the stdin-to-stdout contract standalone; no model was run.

### What was repaired (eval/arena/entrypoint.sh only)

- `verify_pixel_brief`: invocation changed to the live verb
  `pixel run-hook task-event --provider codex --event prompt-submit`,
  the spelling `pixel install` registers for Codex `UserPromptSubmit`
  (`crates/pixel-install/src/routing.rs:214,303`). The stdin payload keeps
  `session_id`/`prompt`/`cwd`/`hook_event_name`; the hook reads `prompt`
  (`task_hook.rs:1091`) and `cwd` (`task_hook.rs:1038-1042`). The probe
  prompt changed from "How should a contributor approach this repository's
  setup and tests?" to "where is README.md and how is the project tested?"
  because the old text is a Weak signal that abstains on this repository
  (measured below); `README.md` is a Strong anchor (`names_code` accepts a
  source extension, `execution_brief.rs:388-404`) and `tested` is a code
  word, so an indexed repo can render real evidence.
- Receipt schema: added `emitted_context` (non-empty `additionalContext`),
  keeping `hook_event_name` and `brief_present`. `response_valid` now
  mirrors `hook_audit.py:run_hook` semantics: non-empty stdout AND (no
  `hookSpecificOutput` key — Codex's `{}` abstention — OR the correct
  `hookEventName` with a string-or-absent `additionalContext`). Plumbing
  validity and evidence presence are now recorded as separate fields; an
  abstention no longer masquerades as a broken pipe.
- `install_live_brief_hook`: the audit-wrapped command written into
  `hooks.json` changed to `/usr/local/bin/pixel run-hook task-event
  --provider codex --event prompt-submit`, the argv `hook_audit.py`
  allowlists (`ALLOWED`, `PROMPT_SHAPES`; `eval/arena/hook_audit.py:21-35`).
- Out of scope, still broken: `--review-pixel-hooks` mode requires the
  retired `run-hook metrics` command in `REQUIRED_NON_PROMPT`, which the
  native-default install no longer writes; `eval/arena.sh:415-431` still
  pre-rejects it. The reviewed-mode audit also wraps in place instead of
  appending; neither path was touched.

`git diff eval/arena/entrypoint.sh`: 1 file changed, 27 insertions(+),
8 deletions(-), including comments; no other file was modified by this
repair.

### Validation evidence (no Codex, no arena run)

Worktree binary: `cargo build -p pixel-cli --profile dev-release` →
`target/dev-release/pixel`, `pixel 0.7.1 commit c481dfc4…-dirty` (this
branch head plus the pre-existing uncommitted pixel-recall edits).
Installed binary for contrast: `/Users/livio/.cargo/bin/pixel`,
`pixel 0.7.1 commit 3db32a37…-dirty` — older provider list
(`claude|codex|pi`) and no `brief` subcommand, so installed-binary results
carry a protocol-version caveat.

Index: `./target/dev-release/pixel build-index .` →
`indexed via daemon: base_files=1156 delta_files=0 overlay_files=5`;
`.pixel/base.shard` exists (the brief's `Gate.indexed` check,
`chain.rs:154-157`). Caveat measured below: the index must be written by a
compatible binary — the new binary abstained (`{}`) against the
installed binary's shard until the shard was rebuilt.

Command (the exact repaired entrypoint invocation):

```sh
printf '%s' '{"session_id":"arena-brief-1","prompt":"where is README.md and how is the project tested?","cwd":"<repo>","hook_event_name":"UserPromptSubmit"}' \
  | pixel run-hook task-event --provider codex --event prompt-submit
```

Observed, worktree binary, cwd=this worktree (verbatim, wrapped for print):

```json
{"hookSpecificOutput":{"additionalContext":"[PIXEL:BRIEF]\nfiles: crates/pixel-install/src/pi_project.rs:47 crates/pixel-install/src/install.rs:310 crates/pixel-install/src/uninstall.rs:239\nconfidence: medium | ops: 1/4\nAnswer from this evidence; open a file only if it contradicts you. 0 hits or 0 callers: verify with rg before concluding.","hookEventName":"UserPromptSubmit"}}
```

Same 296-byte context, `context_sha256
f2bece39222348567d55b5a5b2417bd78cc3f252beead209948bc2d9066410d7`, from
three independent paths: the extracted `verify_pixel_brief` function run
verbatim, the installed binary, and the audit-wrapper passthrough
(`hook_audit.py run_hook` with `PIXEL` repointed at the local binary).
Wrapper receipt row (verbatim):

```json
{"returncode": 0, "stderr": "", "response_valid": true, "forwarded_to_codex": true, "emitted_context": true, "hook_event_name": "UserPromptSubmit", "additional_context_bytes": 296, "additional_context_sha256": "f2bece39222348567d55b5a5b2417bd78cc3f252beead209948bc2d9066410d7"}
```

Repaired `verify_pixel_brief` receipt (verbatim, temp-dir receipt path):

```json
{"arm": "pixel", "rep": "1", "provider": "codex", "event": "prompt-submit",
 "response_valid": true, "emitted_context": true,
 "hook_event_name": "UserPromptSubmit", "brief_present": true,
 "execution_route_present": false, "context_bytes": 296,
 "context_sha256": "f2bece39222348567d55b5a5b2417bd78cc3f252beead209948bc2d9066410d7"}
```

Boundaries measured (commands identical, argv/prompt/cwd varied):

| Case | Observed stdout | Meaning |
| --- | --- | --- |
| Retired `run-hook prompt-submit --provider codex`, same payload | empty, rc 0 | the bug signature: accepted, emits nothing |
| Unindexed `/tmp/unindexed-repo`, new verb | `{}`, rc 0 | `Gate.indexed` false → graceful abstention |
| `PIXEL_BRIEF=0`, new verb, indexed | `{}`, rc 0 | env opt-out honored (`feature_enabled`, `config_cmd.rs:57-67`) |
| Old preflight prompt, indexed repo | `{}` (installed binary) | Weak prompt, no evidence — abstention, not a pipe failure |
| Malformed stdin (unescaped inner quotes) | `{}`, rc 0 | `process` parse failure → fail-closed abstention |
| Unregistered provider/event spelling | clap error, rc != 0 | only `task-event --provider … --event …` is live |

`install_live_brief_hook` was run verbatim against a scratch
`CODEX_HOME`; observed `hooks.json` (verbatim):

```json
{"hooks": {"UserPromptSubmit": [{"hooks": [{"type": "command",
 "command": "python3 /usr/local/lib/arena-hook-audit.py --receipt /out/pixel-hook-1.jsonl -- /usr/local/bin/pixel run-hook task-event --provider codex --event prompt-submit"}]}]}}
```

The wrapper's allowlist gate accepts this argv
(`tuple in ALLOWED` and `"prompt-submit" == argv[-1]` verified by
importing `eval/arena/hook_audit.py` and re-evaluating
`run_hook`'s predicate). `python3 -m pytest test_hook_audit.py
test_entrypoint_prep_failure.py -q` → 20 passed.

Not validated: a real `codex exec` turn, Codex's actual stdin payload
shape, and multi-hook context merging. In the default arm, `pixel
install`'s direct `UserPromptSubmit` hook and the appended audit-wrapped
duplicate can both fire — the same duplication the historical
delivered-hook runs had; whether Codex concatenates or de-duplicates
`additionalContext` across same-event hooks is unmeasured. The arena's
`--arms "raw pixel"` warning at `eval/arena.sh:94-96` still prints for
this mode; it refers to the audit-mode harness, not to this repair.

### Frozen question matrix (11 rows)

Runnable per row, in an indexed repository
(`pixel build-index <repo>`; the row's evidence column assumes this
worktree's index — `c481dfc4`, base_files=1156). Payloads must be real
JSON — inner quotes escaped — or the hook abstains (`{}`); build them
with `json.dumps`, not shell string interpolation:

```sh
python3 -c 'import json,sys; print(json.dumps(
    {"session_id":"m","prompt":sys.argv[1],
     "cwd":sys.argv[2],"hook_event_name":"UserPromptSubmit"}))' \
  "<PROMPT>" "<REPO>" \
| ./target/dev-release/pixel run-hook task-event --provider codex --event prompt-submit
```

An answer counts as *delivered* only when the envelope carries
`hookSpecificOutput.additionalContext` containing `[PIXEL:BRIEF]`; `{}`
is an abstention and proves nothing about evidence.

| # | Kind | Exact prompt | Expected evidence ops | Unsupported claim |
| --- | --- | --- | --- | --- |
| 1 | Lookup | What is the maximum rendered byte budget for the prompt-submitted Pixel execution brief, and what operation/time limits does it enforce? | Literal anchors `BRIEF_BYTES`, `MAX_OPS`, `BRIEF_WINDOW` → `files_with` + `line_at` | Naming limits not present in source (2048 B / 4 ops / 750 ms) or citing file locations as values |
| 2 | Change impact | Trace start_brief: where is it defined, where is it called in production, and which same-file test exercises its provider/event behavior? Name the test and the cases it checks. | `find-symbol` uid + `impact` callers + test-file location | Naming a test that does not exist, or a call site absent from `task_hook.rs` (def :1097, call :1068) |
| 3 | Flow | Trace a Codex prompt from the installed hook through prompt-submit to the context returned to the host. Name the modules and the key handoff functions/fields in order. | `files_with` on `UserPromptSubmit`/`task-event` + ordered handoffs | Skipping `with_brief`/`envelope`, or claiming `run-hook prompt-submit` is the installed command |
| 4 | Diagnosis | If the code graph is stale or unavailable but a text index exists, does the prompt brief disappear? Explain the fallback, where its reason is reported, and whether the hook rebuilds anything. | Fallback-condition text + `unresolved` reason + no-rebuild assertion | Claiming the hook rebuilds index/graph, or that a stale graph alone kills the brief |
| 5 | Configuration/install | Which exact hook command does Pixel install for Codex prompt context, and what host event and provider flags does it pass? | Literal command-string search in `routing.rs`/`codex_config.rs` | Answering the retired `run-hook prompt-submit --provider codex` spelling as the live one |
| 6 | Tests/validation | Is Pi included in start_brief, and which tests prove the Pi exclusion and Codex host envelope shape? Give test names and the important assertions/fields. | `files_with`/`find-symbol` on the three named tests + assertion text | Inventing test names, or claiming Pi starts a brief (`brief_prompt` excludes it, `task_hook.rs:1081`) |
| 7 | History/rationale | What design constraints did commit 14feba456b479be165ab40ec14dde3958a865368 record for the prompt brief? Separate its limits, read-only behavior, and host coverage. | `commit-history` on the named commit | Asserting constraints not in that commit's record, or dating them |
| 8 | Architecture | Describe the code boundary for the prompt brief from host hook receipt to rendered evidence context. Which module owns retrieval and which owns the host envelope? | Module/function locations: `task_hook` vs `execution_brief::chain` | Attributing the `hookSpecificOutput` envelope to the chain, or retrieval ops to task_hook |
| 9 | ≥2-symbol flow | how does start_brief reach the host envelope through with_brief? | Anchors `start_brief`+`with_brief` → `find-symbol` + `files_with`; measured 668 B, ops 2/4 | Claiming a chain hop no evidence names, or that multi-anchor prompts are unhandled |
| 10 | Pure paraphrase | where does the code cap the size of injected prompt context? | Weak signal → `judge` verdict → `concept` route only; measured NON-DETERMINISTIC: 5 runs → 4 briefs (289–313 B, `intent: question (0.80)` line present once), 1 abstention `{}` | Claiming delivery on an abstained run, or determinism — the judge's 750 ms-window subprocess decides |
| 11 | Config-in-JSON | in the hook response {"hookSpecificOutput":{"hookEventName":"UserPromptSubmit"}} which field carries the rendered brief? | Quoted anchors `hookSpecificOutput`,`hookEventName`,`UserPromptSubmit` → `files_with`; measured 783 B | Naming a field outside the emitted schema, or claiming JSON blocks are ignored (they are anchors) |

Rows 1–8 are the frozen `eval/scenarios/qk-*.json` prompts verbatim;
their `must`/`never` regexes remain the scoring contract. Rows 9–11 were
measured live against the worktree binary during this repair; only
observed outputs are recorded, not scenario scores.
