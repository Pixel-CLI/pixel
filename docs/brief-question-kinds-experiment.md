# Pixel brief: question-kind experiments and continuation

Date: 2026-10-07
Repository: /Users/livio/Documents/pixel
Inspected HEAD: 14feba456b479be165ab40ec14dde3958a865368
Branch during investigation: fix/brief-evidence-871
Status: investigation complete; adaptations not implemented

## Main conclusion

The brief is worth extending. The initial recommendation to drop most
question kinds was premature: it assessed the current brief's output rather
than testing whether existing evidence could support better output.

Useful evidence exists for lookup, impact, flow, tests, configuration,
bounded diagnosis, recorded rationale, and bounded architecture questions.

The current brief loses information during prompt parsing, evidence
aggregation, and rendering. Correcting those losses is promising, but we
have not demonstrated automatic retrieval quality or live latency for the
proposed routes.

## What the current prompt brief does

This is the `[PIXEL:BRIEF]` returned by the prompt-submit task-event hook.
It is distinct from the explicit `pixel execution-brief` command.

The source path is:

Codex/Claude prompt-submit
→ task_hook::process
→ start_brief
→ chain::start
→ prompt/config/index gates
→ chain::run
→ Pending::finish
→ render
→ with_brief
→ host additionalContext

Pi is excluded from this prompt-brief path.

The brief requires an enabled feature, an existing index shard, a
discoverable repository, and a qualifying code-related prompt.

Limits:

- Four index operations.
- One shared 750 ms deadline.
- 2,048 rendered bytes.
- Up to four extracted anchors, but retrieval primarily uses the first.
- Up to six words in the concept query.

Current retrieval:

1. Literal search for the first anchor.
2. Concept search if no usable file was found.
3. Symbol lookup.
4. Direct upstream callers when change/caller intent is detected.
5. A source-line read for the selected definition, outside the index-op count.

Evidence comes from a compatible warm daemon or read-only local stores.
Local concept lookup requires a daemon. The brief does not start a daemon,
build indexes, or refresh graphs.

A stale/missing graph can leave text evidence available, with graph failure
reported under `unresolved`. If every operation fails, no brief is rendered.

## Question kinds and revised assessment

| Kind | Useful evidence | Remaining limitation |
| --- | --- | --- |
| Definition/value lookup | Declaration location and source line | Multiline or computed values may need more context |
| Change impact | Definition, direct caller, direct test references | Graph and text evidence do not prove exhaustive impact |
| Code flow | Ordered sequence supported by call sites and source | A function signature alone does not establish the sequence |
| Diagnosis | Guard conditions, error sites, fallback behavior | Static evidence does not establish an arbitrary live root cause |
| Configuration/install | Registrations, install destinations, setting precedence | Relevant evidence spans config and install code |
| Tests/validation | Test names, assertions, workflow commands | Discovery does not prove tests ran or current CI passed |
| History/rationale | Recorded commit messages and documentation | Authorship is not ownership; undocumented intent stays unknown |
| Architecture | Component boundaries and bounded dependency paths | A retrieved path is not a complete system model |

None of these kinds should be declared generally solved from this sample.
The revised finding is that none is demonstrated to be a dead end.

## Concrete information loss

### Matching source text is discarded

Search already supplies matching text. The adapter retains path/line
locations rather than the matching content.

Sources:

- crates/pixel/src/execution_brief/evidence.rs:
  local_files, search_files, file_rows
- crates/pixel-index/src/verify.rs: MatchLine

Preserving short excerpts could help without requiring another search.

### Same-file matches collapse

Brief::absorb deduplicates by file path. That hides separate production and
test references when they occur in one Rust file.

For `build_decisions_request`:

- Definition: decide_remote.rs:538.
- Production call: decide_remote.rs:341.
- Direct test call: decide_remote.rs:1268.
- Direct test call: decide_remote.rs:1351.

The corresponding direct tests are:

- the_decisions_request_maps_labels_to_choice_values_with_their_criteria
  — decide_remote.rs:1261.
- the_openai_preset_posts_to_decisions_not_chat_completions
  — decide_remote.rs:1334.

Indirect behavior coverage also exists through
decide_routes_the_openai_preset_through_the_decisions_wire_end_to_end.

These references show why the earlier file-only impact brief was incomplete.

### Concept extraction drops important clauses

A source-derived reproduction of the six-word extraction produced:

| Prompt kind | Retained query | Important discarded words |
| --- | --- | --- |
| Flow | codex prompt reach pixel evidence brief | step |
| Diagnosis | pixel prompt brief report stale graph | failure, surface |
| Configuration | pixel install codex prompt hook config | layers, control |
| Validation | tests cover start brief routing stale | graph, workspace, checks, release |
| Architecture | boundaries prompt brief cross hook graph | evidence, downstream, readers, depend |

This verifies query information loss. It does not measure resulting search
quality.

### Confidence describes evidence presence, not answer completeness

Any nonempty callers list produces `high` confidence. A file or definition
produces `medium`.

The label does not establish that every requested subquestion was answered,
all tests were found, or the graph is complete.

### The footer restricts useful follow-up

The fixed footer instructs the agent to open a file only if it contradicts
the brief. That is too restrictive for evidence that supplies navigation
without enough content to answer the question.

An incomplete packet needs an explicit follow-up allowance, even when its
existing facts are correct.

### Generated-file filtering needs question-aware review

The brief excludes JSON as generated/data evidence. Some configuration and
hook questions concern meaningful JSON files. Reconsidering that exclusion
requires care around credential paths and secret values.

## Existing capabilities worth reusing

Pixel already contains:

- A composed locate recipe with resolve, source context, and caller-derived
  test-file discovery.
- Direct caller/callee and trace operations.
- Repository mapping and context facilities.
- Commit history and provenance operations.
- Searchable architecture, workflow, and configuration documentation.

The locate recipe is not a drop-in replacement for the hook chain: it can
perform additional calls and has different execution/budget behavior.

Its test discovery also has an explicit limitation: Rust inline `#[test]`
functions are not indexed. Test-path naming heuristics miss tests embedded
in production files.

History can establish recorded changes and constraints. Provenance can
establish authorship. Neither alone establishes the reason for a decision
or its current owner.

## Source-derived prototype packets

Five packets were manually assembled in memory from inspected source and
a verified commit message. UTF-8 size was measured using
`len(packet.encode())`.

| Packet | Bytes |
| --- | ---: |
| Rename and direct tests | 539 |
| Ordered prompt-to-context flow | 520 |
| Stale-graph guard explanation | 456 |
| Named brief regression tests | 446 |
| Recorded historical constraints | 517 |

All fit the 2,048-byte cap.

These are feasibility examples, not emitted hook outputs or automatically
retrieved answers. They use manually selected source locations. They do
not prove a four-operation route, 750 ms latency, or generalization to
unfamiliar repositories.

Recorded rationale was verified with:

```sh
rtk proxy git show -s --format='%h%n%B' \
  14feba456b479be165ab40ec14dde3958a865368
```

That commit records the four-operation, 750 ms, read-only, 2 KiB design.
It references #865 and includes documentation corrections for Pi exclusion
and stale-graph fallback.

## Earlier arena runs and hook-delivery problem

The arena uses two Codex arms: raw and pixel. The runner, not ad hoc panes,
should create the labeled Herdr sessions.

Historical runs reported:

| Run directory suffix | Raw score | Pixel score |
| --- | ---: | ---: |
| openai-brief-arena-20261007-01 | 78.6% | 85.7% |
| openai-brief-arena-20261007-03 | 85.7% (12/14) | 100% (14/14) |

However, the Pixel receipt reported:

- response_valid: false
- emitted_context: false
- hook_event_name: null

Therefore these scores do not establish a benefit from brief delivery.

The arena entrypoint still invokes the legacy prompt-submit hook command.
Current source documents that retired hook verbs exit successfully without
output; the live brief is delivered through task-event prompt-submit.

Two entrypoint lines were temporarily changed during an earlier attempt,
then restored after the user objected. Do not silently reapply that change.
Any future repair must be explicit, minimal, and independently validated.

Relevant files:

- eval/arena.sh
- eval/arena/entrypoint.sh
- eval/arena/README.md
- eval/arena-results/openai-brief-arena-20261007-03

## Previous local checks

Earlier in the session, these focused suites were reported passing:

| Command | Passed |
| --- | ---: |
| cargo test -p pixel-cli --bin pixel classify:: | 54 |
| cargo test -p pixel-cli --bin pixel decide_remote:: | 31 |
| cargo test -p pixel-cli --bin pixel execution_brief:: | 74 |
| cargo test -p pixel-cli --test cli config_cli::classify_accepts_the_openai_preset_and_names_its_key_when_missing | 1 |
| cargo test -p pixel-cli --test cli prompt_brief_cli:: | 9 |

These are historical session results. They do not validate a future
adaptation, current-head CI, installation, or arena hook delivery.

## How to continue

### 1. Establish a valid delivery baseline

Confirm the arena's exact hook command and host envelope against current
source. Obtain one captured Codex prompt where:

- The correct task-event prompt-submit path runs.
- A valid additional-context response contains `[PIXEL:BRIEF]`.
- The transcript shows the agent actually received the block.

Record emission, host relay, receipt, and model use separately.

Keep this harness correction separate from question-kind adaptations so
the experiment does not change both delivery and retrieval simultaneously.

### 2. Freeze a reproducible question matrix

Use the eight question kinds above. Include:

- Exact identifiers and natural-language prompts.
- Multiple requirements in one prompt.
- Same-file Rust unit tests.
- Missing and stale graphs.
- No matching code.
- Ambiguous symbol names.
- Configuration files.
- A rationale question with no recorded rationale.

For each case, record expected evidence and what would count as an
unsupported claim. Avoid equating a file hit with a complete answer.

### 3. Compare parallel and ordered execution separately

Use isolated raw/pixel Codex arms with identical model settings, repository
snapshot, and task wording.

Run kinds concurrently in isolated sessions for throughput. Run a separate
ordered pass to check consistency and any effects of session history.

Do not compare a fresh session against a session carrying earlier answers.
Label question kind and raw/pixel arm visibly in Herdr.

### 4. Adapt evidence selection before adding broad new providers

Evaluate the smallest changes independently:

- Retain bounded matching text.
- Preserve distinct reference sites within a file.
- Preserve important later clauses and multiple requested evidence types.
- Distinguish direct test references from indirect behavior coverage.
- Report missing evidence and coverage rather than answer-wide confidence.
- Allow bounded follow-up reads when the packet is insufficient.

Use the existing locate/context/trace/history machinery where appropriate,
but preserve the hook's read-only behavior and explicit deadline.

### 5. Measure the complete behavior

For every run record:

- Snapshot/commit, binary version, task, model, and settings.
- Exact brief and rendered byte count.
- Operations attempted and answered.
- End-to-end hook latency.
- Files and ranges read afterward.
- Correct requested facts and unsupported claims.
- Whether failure paths continue safely.
- Tokens and commands, alongside answer quality.

Keep a route when it improves supported answers or reduces necessary
follow-up without increasing false confidence. Require more than one
hand-selected example before calling a question kind solved.

### 6. Implement and validate a focused candidate

Preserve existing dirty work. Read CONTRIBUTING.md, applicable scoped
rules, and the Rust guidelines before editing.

Delegate independent work with disjoint write ownership. Use ordinary
focused regression tests for the observed contracts. Publish a reviewable
candidate; required current-head validation belongs to CI.

Do not run local/manual mutation campaigns. Do not reinstall global hooks
unless installed-behavior verification requires it. An installation check
must follow the repository's documented workflow and be reported separately.

## Workspace and operational constraints

- Existing user changes remain in the working tree.
- No adaptation edits or rebase were performed during the deeper investigation.
- The branch was last observed behind origin/main; fetch had occurred, but
  integration was not performed.
- Preserve unrelated Herdr panes and the main operator pane.
- User requested Codex, with visibly labeled raw/pixel arena arms.
- The current session is read-only, blocking document persistence and
  implementation.
- No claim of a completed eight-kind live arena experiment is warranted.

## Primary continuation sources

- crates/pixel/src/execution_brief/chain.rs
- crates/pixel/src/execution_brief/evidence.rs
- crates/pixel/src/execution_brief.rs
- crates/pixel/src/task_hook.rs
- crates/pixel-proto/src/query.rs
- crates/pixel/src/main.rs — run_locate and LOCATE_TESTS_NOTE
- crates/pixel/src/decide_remote.rs
- crates/pixel/tests/cli/prompt_brief_cli.rs
- crates/pixel-install/src/codex_config.rs
- crates/pixel-install/src/install.rs
- crates/pixel-ops/src/history.rs
- crates/pixel-ops/src/provenance.rs
- ARCHITECTURE.md — hooks and prompt brief
- CONTRIBUTING.md
- .agents/rules/measuring.md
- .agents/skills/agent-session-debugging/SKILL.md
