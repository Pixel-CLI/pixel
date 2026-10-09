# Injection-balance eval loop

Measures whether pixel's injected guidance helps or hurts a coding harness,
and gates any candidate payload so it can **never** score worse than running
with no pixel guidance at all.

## Shape

- `scenarios/*.json` — one natural task each. Answer tasks carry a pattern
  rubric (`must` = earned points, `never` = penalties for known-wrong
  claims); edit tasks (`"mode": "edit"`) carry a held-out `verifier`.
  `lib/scenario.py` documents every field and `check`s a corpus.
- `heldout/<id>/` — the edit tasks' verifiers, and each answer task's
  `reference-answer.md`, which its rubric must score at full marks without
  a penalty (the offline test enforces it). run.sh copies a verifier in only
  after the agent exits; the scratch worktree never contains `eval/`.
- `run.sh` — builds a scratch worktree + Claude/Codex config per arm, deploys
  the arm's payload to `~/.local/share/pixel/agent-prompt.md` (backed up,
  restored on exit), runs the CLIs in counterbalanced arm order, runs the
  verifiers, then scores.
- `score.py` — mechanical scoring of transcripts (answered? pattern hits?
  verifier verdict? turns/tokens/cost, pixel and native-search calls, read
  width after a pixel hit). No LLM judge: reproducible.
- `report.py` — host × task-class win/tie/loss against `baseline`, paired by
  scenario and repetition, plus pixel adoption per arm.
- `gate.py --candidate <arm> --baseline baseline` — PASS only if the
  candidate answers every scenario, scores >= baseline everywhere, and burns
  <= 1.5x baseline turns. Any single regression fails.
- `brief-gate/` — a labelled prompt set for the prompt-submit brief's gate and file recall (`scripts/bench-brief-gate.py`), apart from the arms above; see `brief-gate/README.md`.
- `fixtures/` — offline stand-ins for `claude`, `codex` and `pixel`;
  `scripts/test-agent-ab-harness.py` drives the whole pipeline with them.

## Arms

| arm | hooks | AGENTS.md block | session-start payload | pixel binary + `.pixel` index |
| --- | --- | --- | --- | --- |
| `baseline` | none | stripped | n/a | hidden (`BASELINE_PIXEL=hidden`, default): `pixel` on PATH answers "command not found", no index, no pixel skill |
| `quiet` | the installed set filtered to `run-hook session-start` / `post-compaction` (global and repo-local) | `pixel install --repo` | the deployed prompt | present |
| `full` | every hook the operator's `pixel install` wrote + `pixel install --repo` (Claude guard, Codex composed guard) | `pixel install --repo` | the deployed prompt | present |
| `on` | one clean pixel set | committed (current `RULES_BODY`) | `variants/frozen-main` | present |
| `v<name>` | one clean pixel set | `variants/<name>/rules-body.md` | `variants/<name>/agent-prompt.md` | present |

`quiet` and `full` differ in the hook set alone (prompt-submit task packet,
metrics relay, post-edit relay, guard, task events). `baseline` and `quiet`
differ in the whole pixel integration: that is the "Opus alone vs Opus +
Pixel" comparison. `BASELINE_PIXEL=present` gives the earlier baseline
(binary and index reachable, no doctrine), which the tables below used.

For Codex, `quiet` keeps the installed `developer_instructions` block and the
repo-local `session-start` hook; its global hooks file has no session-start
entry, so none of it survives the filter.

## Run

```bash
CLIS=claude ARMS="baseline on" eval/run.sh          # defaults: 3 scenarios, 12-turn budget
CLIS=claude ARMS="vslim" eval/run.sh                # then:
python3 eval/gate.py --results eval/results --scenarios-dir eval/scenarios --candidate vslim
```

`REPS=n` repeats every cell; `ORDER_SEED` (default: `SUITE`, else `eval`)
fixes the arm order. Each (rep, host, scenario) cell runs its arms in the
order `lib/arm_order.py` derives — the controlled runner's
sha256-permutation-and-rotation, generalised to any arm count — so over
every `len(ARMS)` reps each arm holds each position once.

## CLIs

`claude` is the scored default (headless `-p`, stream-json,
`--model "$CLAUDE_MODEL"`, default `claude-opus-5-5`). `agy` works via
`-p --output-format stream-json` (its global pixel plugin is toggled off for
non-`on` arms and restored). `codex` runs `exec --json --ephemeral
--model "$CODEX_MODEL" -c model_reasoning_effort="$CODEX_REASONING"`
(defaults `gpt-5.6-terra`, `medium`) with `--dangerously-bypass-hook-trust`
(the per-arm hooks file sits at a scratch path no `/hooks` review trusted)
and, by default, `--dangerously-bypass-approvals-and-sandbox` — the same
authority Claude's `--dangerously-skip-permissions` run has; a read-only
sandbox blocks the edit tasks' builds and pixel's own `.pixel/` writes
(`CODEX_SANDBOX=read-only` restores the old behaviour). build_arm renders a
CODEX_HOME whose `developer_instructions` pixel block is stripped
(baseline), swapped for the arm payload (variants) or kept (quiet/full),
plus a copy of `auth.json` and the arm's `hooks.json`. `pi` runs `-p` as a
**coverage arm**: its pixel integration is a global extension with no
per-arm config isolation yet, so pi rows measure the installed state rather
than arm variants. Any CLI can also plug in by dropping an executable
`eval/clis/<name>.sh` that reads `$WT`, `$CFG`, `$PROMPT`, `$OUT`,
`$MAX_TURNS` and writes the run's transcript to `$OUT`; run.sh picks it up
automatically and skips the CLI with rc 9 until it exists.

## Results

Transcripts land in `results/` (gitignored; `RESULTS=` points elsewhere).
`results/scores.json` is the machine-readable scoreboard consumed by
`gate.py` and `report.py`.

```
results/
  .identity              the campaign identity; a different campaign refuses to reuse the dir
  campaign.json          arms, hosts, scenarios, reps, seed, models, host and pixel versions
  setup.jsonl            per pinned commit: index build and prepare-repo time, .pixel size
  rep-N/<scenario>-<arm>.<cli>.jsonl        the host's native transcript
  rep-N/<scenario>-<arm>.<cli>.run.json     run identity: commit, model, cli_version, position,
                                            wall_ms, warm_ms, exit_code, hook-file hashes
  rep-N/<scenario>-<arm>.<cli>.verify.json  edit tasks: passed, rc, wall_ms, heldout_sha256
  rep-N/<scenario>-<arm>.<cli>.{err,verify.log,warm.log,actions.jsonl}
  scores.json            one row per run (score.py)
```

A row that cannot be measured stays `null`: Codex reports no cost, a
transcript without usage has no token counts, and `report.py` counts a pair
with an unknown quality as `unknown`, never as a tie.

## The #626 round: Pixel against no Pixel, per host and task class

Scenarios with `"suite": "ab626"` (14) are pinned to commits of this
repository; the answers and line numbers hold for those trees only.

| scenario | task_class | mode | pinned | verifier |
| --- | --- | --- | --- | --- |
| `ab-exact-protocol-version` | exact-identifier | answer | `cdca0c3` | rubric |
| `ab-exact-inline-limit` | exact-identifier | answer | `cdca0c3` | rubric |
| `ab-concept-output-cap` | concept | answer | `cdca0c3` | rubric |
| `ab-concept-graph-freshness` | concept | answer | `cdca0c3` | rubric |
| `ab-impact-failure-response` | callers-impact | answer | `cdca0c3` | rubric (includes a test site the graph does not list) |
| `ab-impact-tree-delta` | callers-impact | answer | `cdca0c3` | rubric |
| `ab-rename-graph-db-file` | rename | edit | `cdca0c3` | no word `GRAPH_DB_FILE` left in tracked files, 13 files renamed, value kept, `cargo check` of 3 crates |
| `ab-rename-mutants-per-shard` | rename | edit | `cdca0c3` | no word `MUTANTS_PER_SHARD` left (Python, YAML, Markdown), shard arithmetic unchanged |
| `ab-bugfix-ts-overflow` | bugfix | edit | `e99939b` (parent of `313bb53`) | held-out test file + crate unit tests |
| `ab-bugfix-global-excludes` | bugfix | edit | `4aa6790` (parent of `2732fa2`) | held-out test file + crate unit tests |
| `ab-feature-ts-units` | feature | edit | `cdca0c3` | held-out test file + help/error strings + crate unit tests |
| `ab-git-shard-cap` | git-ops | answer | `cdca0c3` | rubric |
| `ab-config-nextest` | non-code | answer | `cdca0c3` | rubric |
| `ab-explain-daemon-route` | explanation | answer | `cdca0c3` | rubric |

Each held-out verifier was checked before freezing: the two bug-fix tests
fail at their pinned parent and pass with the historical fix applied, and the
feature test fails at `cdca0c3` and passes with a reference implementation.
Each answer rubric was checked the other way: its reference answer, written
against the pinned tree, earns every point, and a wrong claim it names
(`ENVELOPE_PROTOCOL_VERSION` is 13, "the CI run passes", a daemon calling the
uncached `tree_delta`) costs points. `lib/scenario.py check` refuses a pinned
commit that already contains its own scenario or verifier.

### Commands

One campaign per host, so each has its own identity and results directory
and the two are never pooled. A shared `SCRATCH` reuses the per-commit
indexes and the warm build caches between them. `pixel install` must have
run on the machine: `quiet` and `full` start from the installed hooks.

```bash
SCRATCH=/tmp/pixel-ab626 RESULTS=eval/results/ab626-claude \
  CLIS=claude CLAUDE_MODEL=claude-opus-5-5 \
  ARMS="baseline quiet full" SUITE=ab626 REPS=3 eval/run.sh
python3 eval/report.py --results eval/results/ab626-claude --json eval/results/ab626-claude/report.json

SCRATCH=/tmp/pixel-ab626 RESULTS=eval/results/ab626-codex \
  CLIS=codex CODEX_MODEL=gpt-5.6-terra CODEX_REASONING=medium \
  ARMS="baseline quiet full" SUITE=ab626 REPS=3 eval/run.sh
python3 eval/report.py --results eval/results/ab626-codex --json eval/results/ab626-codex/report.json
```

An interrupted campaign resumes with the same command: a cell whose
transcript, `.run.json` and identity match is skipped. Changing the model,
a host version, the arms, the reps or the seed changes the identity, and
run.sh refuses the old directory instead of mixing the two. When done:
`rm -rf /tmp/pixel-ab626 && git worktree prune`.

The offline proof of the same pipeline, no model call (about 3 minutes):

```bash
python3 scripts/test-agent-ab-harness.py
```

### Reading the report

For each host, one row per (task class, arm): `n` paired runs, wins / ties /
losses against the baseline run of the same scenario and repetition (tie
margin ±0.05 quality, `--tie-margin`), the mean quality difference with its
min–max, an exact sign test on wins against losses, and the median per-pair
ratio of wall time, tokens and tool calls (with how many pairs had both
sides measured). The `all` row pools the classes of that host only. With
2 scenarios × 3 reps = 6 pairs per cell, the smallest two-sided sign-test
p-value is 0.03 (6 wins, 0 losses); 5–1 gives 0.22. A cell verdict is a
direction to look at, not a finding: read it beside the min–max and the
transcripts.

The adoption table counts, per arm, runs that called pixel at all, pixel
calls against native searches (`grep`, `rg`, `git grep`, Grep, Glob), and
the width of every read that followed a pixel `path:line` hit (median lines,
and how many read the whole file).

### Cost estimate (not measured on this corpus)

Basis: the 22 Opus 5.5 runs of `docs/bench/problem-trace/runs.json` (Claude
Code 2.1.286, `claude-opus-5-5`, effort medium, one locate-only task on this
repository): `total_cost_usd` median $0.36 (range $0.28–$0.53), 8–11 turns,
about 45 s each. At Opus 5.5's list price ($4 / $20 per million input /
output tokens, cache reads $0.20) such a run is roughly 0.3–0.4 M input
tokens, mostly cache reads, and a few thousand output tokens. The edit tasks
have no measured basis; they are assumed to cost 2–5× an answer task, 3× in
the central figure (more turns, build and test output in context). Codex:
the October n1 round ("Earlier results" below) measured 163–341 K input
tokens per answer run on the same repository; `codex exec` reports no price,
and with ChatGPT sign-in (`auth.json`) the runs draw on the plan's limits
rather than a per-token bill.

| per host, 14 scenarios × 3 arms × 3 reps = 126 runs | answer runs (81) | edit runs (45) | total |
| --- | --- | --- | --- |
| Claude, $ (Claude Code's own figure) | ≈ $29 ($23–43) | ≈ $49 ($32–81) | **≈ $80 ($55–125)** |
| Claude, input tokens | ≈ 28 M | ≈ 45 M | ≈ 75 M, mostly cache reads |
| Codex, input tokens | ≈ 20 M | ≈ 35 M | ≈ 55 M; $ unknown |
| wall time, sequential | ≈ 1 h | ≈ 3–4 h | ≈ 4–5 h per host, plus index builds and cold `cargo` builds (one build cache per arm and pinned commit; keep about 30 GB of disk free) |

Replace this estimate with the measured `cost_usd` and token sums of
`scores.json` once the round has run.

### Known confounds

- The repository's own `AGENTS.md`, `.agents/rules` and `.claude/rules`
  mention pixel in every arm; only the managed block is stripped from the
  baseline. A baseline pixel attempt fails fast and shows up in its
  transcript and adoption row.
- Operator-wide settings that are not pixel's travel into every arm alike:
  `~/.claude/CLAUDE.md`, non-pixel Claude skills and plugins, and the whole
  `~/.codex/config.toml` (including any `model_instructions_file`).
- The verifiers are hidden from the scratch worktree, not from the
  machine: an agent that searched the harness checkout by absolute path
  could find them.
- `ab-bugfix-ts-overflow` and `ab-feature-ts-units` touch the same function
  at different commits.

## Earlier results

### Measured on this branch (claude / deepseek-v4-flash, 20-turn budget, Sept 2026)

| arm | hooks | s1 score | s2 score | s3 score | mean of shown |
| --- | --- | --- | --- | --- | --- |
| `baseline` | none | 15.5 (n2) | 10.5–11.5 (n2–3) | 12.0 (n2–3) | 12.8–13.0 |
| `on` (14.3 KB doc, full hooks) | packet + relays | 11.5 (n2) | 6.5 (n2) | 11.5 (n2) | 9.8 |
| `vslim` (1.1 KB doc, full hooks) | packet + relays | 9.5 (n2) | 10.5 (n2) | 11.0 (n2) | 10.3 |
| `vquiet` (1.1 KB doc, session-start only) | none mid-session | 16.0 (n1) | 10.0 (n1) | 12.0 (n1) | 12.7 |
| **`vfinal` (shipped 2.8 KB, session-start only)** | none mid-session | **16.0** | **13.0** | **12.0** | **13.7** |

Final gate (`vfinal` vs `baseline`, turns slack ×1.5): **PASS** — s1 16.0 vs 15.0,
s2 13.0 vs 11.5 (2 turns vs 20), s3 12.0 vs 12.0.

### First 4-CLI round (codex + pi, 20-turn budget, n1 per cell, Oct 2026)

| cli | arm | s1 | s2 | s3 | mean |
| --- | --- | --- | --- | --- | --- |
| codex | baseline (payload stripped) | 8/16 | 10/13 | 12/12 | 10.0 |
| codex | vfinal (2.8 KB payload) | 8/16 | 12/13 | 9/12 | 9.7 |
| pi | coverage (installed state) | 13/16 | 12–13/13 | 12/12 | 12.3–12.7 |

Read: codex answers in a single front-loaded turn (163–341K input tokens — no
agentic loop). At n1 it nets slightly below its own baseline on the mean
(9.7 vs 10.0): +2 on the retrieval-heavy scenario, −3 on the rename scenario
where the one-turn read loses exact line numbers — inside single-run
variance, needing n≥2 before any conclusion. pi
(the QA fleet's harness) scores strong across the board with zero
configuration. Codex n≥2 reps and per-arm pi isolation are the open
follow-ups.

Two findings survived repetition:

1. The **prompt-submit task packet and mid-session relays** — not the
   session-start text — drag answers down on mechanism questions: arms with
   the full hook set cluster below baseline on s1 regardless of payload size;
   the quiet arm matches or beats baseline everywhere it has runs, at lower
   cost (it injects ~2 KB once and saves exploration).
2. The pre-#475 payload was measured losing ~4 KB of doctrine to Claude
   Code's hook truncation; pixel's own `CLAUDE_INLINE_CONTEXT_LIMIT` (10 000
   UTF-16 units, #443) had already been exceeded by growth.

### Quiet profile (no code change needed)

```bash
pixel config task-boundary off   # silences the prompt-submit task packet
pixel config metrics off         # silences the PostToolUse metrics relay
```

`post-tool-use` (edit blast-radius) has no config toggle yet — tracked as
follow-up together with making the quiet set the install default.

## Controlled task trials (schema 1)

`pixel task evaluate suite.json --json` explicitly runs a frozen suite through
`controlled.ts`. It uses this directory's existing `score.py` rubrics and
`gate.py` candidate checks. It does not replace the injection-balance loop or
the `arena/` scorer, and ordinary task tracking/replay never launches trials.
This command can spend model credits when the suite selects a model gateway.
The implementation and fake-run tests establish runner behavior, **not measured
performance improvements from the new policies**.

Each suite compares exactly three arms: `retrieval`, `gates`, and
`gates_classifier`. Every host has its own baseline; Claude, Codex and Pi rows
are never pooled. Repetitions use the same source, prompt, contract, image,
host version, model configuration, permissions, budgets and heldout verifier.
The backend fingerprints these inputs and records the actual version command's
output. `order_seed` (defaulting to the suite ID) determines a per-host/scenario
permutation, rotated across each three repetitions so every arm occupies every
position. Reports retain the seed, algorithm version and actual arm order.
Runners must be preinstalled in the image, including the Pixel binary
and the native extension/hooks for that host. No package installation happens
inside a measured attempt.

The JSON schema is represented by `pixel_task::evaluation::EvaluationSuite`:

| Field | Required value |
| --- | --- |
| `schema_version`, `id` | `1`; unique simple identifier |
| `image`, `gateway_image` | Preloaded images pinned as `repository@sha256:<64 hex>` |
| `output_dir` | Fresh results directory; an existing suite identity is rejected |
| `order_seed` | Optional fixed nonempty string; defaults to `id` |
| `repetitions` | Integer 1–100 |
| `timeout_ms`, `max_interactions` | Positive per-attempt wall and model-request budgets |
| `arms` | Exactly `retrieval`, `gates`, `gates_classifier` |
| `runners` | One to three unique `claude`, `codex`, `pi` runner definitions |
| `cases` | Unique scenario IDs, prompt, copied source, contract, rubric, heldout verifier |
| `network` | `{"kind":"offline"}` or the fixed inference gateway described below |

A runner contains `host`, exact `version`, `version_argv`, `argv`, `model`,
`model_config`, `permissions`, `environment`, and `files`. Commands are argv
arrays passed directly to processes. `{prompt}` and `{gateway}` substitutions
replace literal argv/environment text without shell evaluation. Runner files
appear under `/runner`; source files under `/workspace`. File manifests are
arrays of `{"path":"relative/path","contents":"text","executable":false}`.
Paths cannot escape their target or contain symlinks. The image must provide
`chmod` and `cat` as well as the selected runner. Private volume initialization
supports images with a non-root configured user.

The following **offline fixture** is a complete suite: it tests the runner,
scorer and gate without invoking any agent or model. Save it as `suite.json`
and preload its pinned Alpine image before running the command above.

```json
{
  "schema_version": 1,
  "id": "offline-example",
  "image": "alpine@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6",
  "gateway_image": "oven/bun@sha256:8956c7667fa17beb6e3c664115e66bdacfe502da5d99603626e74c197bdef160",
  "output_dir": "eval/results/offline-example",
  "repetitions": 1,
  "timeout_ms": 5000,
  "max_interactions": 5,
  "arms": ["retrieval", "gates", "gates_classifier"],
  "network": {"kind": "offline"},
  "runners": [{
    "host": "pi",
    "version": "fixture-1",
    "version_argv": ["sh", "-c", "printf fixture-1"],
    "argv": ["sh", "/runner/fake.sh", "{prompt}"],
    "model": "fixture",
    "model_config": {},
    "permissions": {"sandbox": "private-container"},
    "environment": {},
    "files": [{"path": "fake.sh", "contents": "set -eu\nprintf fixed > /workspace/answer.txt\nprintf '%s\\n' '{\"schema_version\":1,\"event_id\":\"end\",\"task_id\":\"fixture\",\"attempt_id\":\"one\",\"span_id\":\"root\",\"occurred_ms\":0,\"kind\":\"coverage\",\"complete\":true,\"child_spans\":[],\"missing\":[]}' > /telemetry/events.jsonl\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"fixed\"}],\"stopReason\":\"stop\",\"usage\":{\"input\":0,\"output\":0}}}' '{\"type\":\"agent_end\"}'\n"}]
  }],
  "cases": [{
    "id": "fixture",
    "prompt": "Write fixed into answer.txt",
    "source_files": [{"path": "answer.txt", "contents": "before"}],
    "contract": {"objective": "Write fixed into answer.txt"},
    "rubric": {"must": [{"pattern": "fixed", "points": 1}]},
    "verifier": {
      "argv": ["sh", "/heldout/check.sh"],
      "timeout_ms": 1000,
      "files": [{"path": "check.sh", "contents": "test \"$(cat /workspace/answer.txt)\" = fixed\n"}]
    }
  }]
}
```

### Real host bootstrap

An image's original HOME is hidden by the fresh `/home/trial` volume. Hook and
provider files installed there while building the image therefore do not reach
the measured session. Freeze their contents in `runner.files`, or keep a vetted
bootstrap and its assets elsewhere in the pinned image. The bootstrap copies
only those declared files into the private HOME and then executes the host.
Its bytes, configuration and permission choices must be identical across arms.
Redirect bootstrap diagnostics to stderr so stdout remains the native JSON
stream. No package download or host-home copy belongs in this bootstrap.

These invocation surfaces were checked against local host help. The image must
contain these exact versions, or the operator must verify and freeze another
version's interface. Replace `MODEL_ID` with the runner's actual pinned model
ID; only `{prompt}` and `{gateway}` are backend substitutions.

| Host and exact `version` | Host argv after bootstrap | Private configuration |
| --- | --- | --- |
| Claude: `2.1.286 (Claude Code)` | `["claude","--print","--output-format","stream-json","--verbose","--include-hook-events","--forward-subagent-text","--no-session-persistence","--permission-mode","bypassPermissions","--settings","/runner/claude-settings.json","--model","MODEL_ID","{prompt}"]` | `claude-settings.json` contains the installed native Pixel hooks and vetted settings. Include provider endpoint settings in the frozen runner environment. Do not use `--bare` or `--safe-mode`, which skip hooks. |
| Codex: `codex-cli 0.160.0` | `["codex","exec","--json","--ephemeral","--skip-git-repo-check","--dangerously-bypass-approvals-and-sandbox","--dangerously-bypass-hook-trust","--model","MODEL_ID","{prompt}"]` | Copy vetted `config.toml` and `hooks.json` into `/home/trial/.codex`; set `CODEX_HOME=/home/trial/.codex`. Keep the hook paths valid inside the container. Provider configuration belongs in this config. |
| Pi: `0.87.1` | `["pi","--print","--mode","json","--no-extensions","--extension","/runner/pixel-guard.ts","--no-skills","--no-prompt-templates","--no-themes","--no-context-files","--provider","PROVIDER_ID","--model","MODEL_ID","{prompt}"]` | Put the installed, rendered Pixel extension at `/runner/pixel-guard.ts` and explicitly copy provider files such as `models.json` to `/home/trial/.pi/agent`. `--no-extensions` still loads the explicit extension. Replace `PROVIDER_ID` with that configured provider. |

Use `version_argv: ["claude","--version"]`, `["codex","--version"]` or
`["pi","--version"]` respectively. Permission bypass in the example argv is
an **explicit operator choice for these disposable, isolated containers**;
record it in `permissions`. It does not apply to the operator's normal host
sessions. Codex 0.160.0 documents `--dangerously-bypass-hook-trust` for automation
that has already vetted hook sources. The frozen suite must explicitly opt in;
the backend does not add it or fabricate persisted trust hashes. An alternative
is an operator-reviewed trust fixture valid at its exact final container paths.
For project-scoped Codex configuration, the private user config must also mark
`[projects."/workspace"]` with `trust_level = "trusted"`.

Generate fixtures using the Pixel version being evaluated. In particular, the
source `pi-pixel.ts` asset contains an unrendered binary placeholder and is not
a runnable extension. A bootstrap may instead run the image's pinned
`pixel install --repo /workspace` before the host: it writes
`.claude/settings.local.json`, `.codex/config.toml`, `.codex/hooks.json`, and
`.pi/extensions/pixel-guard.ts`. Then point the host at those generated files
instead of the `/runner` examples, and retain the same bootstrap in all arms.
Private Codex project trust still needs the user configuration described above.

These examples establish invocation and file placement, not a verified live
provider combination. The provider configuration must route inference through
`{gateway}` using the fixed path/model below; hosts needing other endpoints
cannot silently fall back to unrestricted networking. Actual native hook
telemetry is required to demonstrate activation. Copied configuration or a
successful exit alone does not establish complete host coverage.

The `gates_classifier` arm also needs Pixel classification enabled in the
private configuration and an already warm local classifier bundled in the
image and started by the same frozen bootstrap in every arm. The backend does
not start or download a classifier implicitly. Inspect recorded classifier
calls and rankings before claiming an active classifier comparison; a disabled
or unavailable classifier exercises deterministic fallback only.

`PIXEL_TASK_POLICY` and `PIXEL_TASK_CONTRACT` are explicit operator settings
injected by the evaluator, never model tool arguments. Changing a policy arm
does not grant the model authority to change its contract.

### Model gateway and isolation

A real-model suite uses a narrow registered OAuth/provider credential available
to the evaluator's environment. The credential's **name**, not its value, is
part of the suite, for example:

```json
{
  "kind": "model_gateway",
  "endpoint": "https://provider.example/v1/messages",
  "request_path": "/v1/messages",
  "model": "the-exact-runner-model-id",
  "credential_env": "CLAUDE_CODE_OAUTH_TOKEN",
  "auth_header": "authorization",
  "auth_prefix": "Bearer "
}
```

The trusted Bun gateway joins an internal worker network and an outbound
network. Only POST requests to the exact configured inference path, with the
configured model ID, are forwarded to the fixed HTTPS endpoint. Redirects,
other methods, paths and models are rejected. Caller authentication is stripped;
the gateway inserts its own credential. The worker has no outbound network,
host bind mounts, Docker socket, gateway credential or inherited image
environment secrets.
Inherited image environment settings are cleared except PATH. Images with the
banned `ANTHROPIC_API_KEY` setting are rejected. Host tools requiring a local
authentication placeholder may receive the literal `gateway-placeholder` for
`CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_AUTH_TOKEN` or `OPENAI_API_KEY`; these values
are not provider credentials. Other secret-looking runner environment keys are
rejected. The operator supplies a trusted image and explicit file manifests;
the runner does not copy the host's home, credentials or source tree implicitly.
Claude's supported OAuth environment interface is documented in its
[environment reference](https://code.claude.com/docs/en/env-vars).

Docker stores source, HOME and telemetry in private disposable volumes. Heldout
verifier files enter a fresh offline container only after the agent exits.
Timeouts and observed interaction-budget overruns kill the agent container;
interrupting the evaluator kills active containers and cleans its resources.
The backend has no unrestricted networking fallback. Docker, Bun and Python 3
(for the existing scorer/gate) must be installed, and both selected image
digests must be available locally when their corresponding mode needs them.

### Evidence and candidate checks

Results retain the existing `rep-N/scenario-arm.cli.jsonl` naming, alongside
stderr, a `.controlled.json` sidecar, adapter `.telemetry`, verifier logs and
the final `.workspace` snapshot. `scores.json` remains the scorer's output;
`controlled-report.json` records all trials and both candidate gates. A sidecar
is accepted only when its transcript SHA-256 matches. Exit code 0 alone is
insufficient: host semantic success and the heldout verifier must both pass.
Pi JSON provider errors are explicitly treated as failed attempts.

The primary metric counts distinct model-issued tool requests, including
blocked calls, retries and observed child agents. Repeated notifications are
deduplicated. Model turns, tokens, wall time, coordinator calls and classifier
calls remain separate. Native stdout alone cannot establish complete blocked
request and child-span coverage; the adapter's versioned telemetry must close
every span. Missing coverage remains `null`/partial, never zero. Unobservable
native or child paths therefore prevent a complete interaction comparison even
when quality checks pass. Budget detection observes issued batches; a model
can issue several requests before the next observation kills the container.

The existing quality and <=1.5x-turn checks now run independently for every
host represented in the selected arms. Controlled rows additionally need
matching frozen identities/repetitions, complete telemetry, heldout success on
both sides, and no increase in mean model-issued requests. Missing comparison
data fails the gate. These are observed comparisons; neither policy replay nor
an empirical best attempt establishes a counterfactual outcome or global
shortest path. A regex rubric should be paired with a substantive heldout check
for coding tasks.

Run the unpaid fixture suite, including all three native stream parsers,
three policy arms, timeout/request-budget failures and actual gateway-network
isolation:

```bash
docker pull alpine@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6
docker pull oven/bun@sha256:8956c7667fa17beb6e3c664115e66bdacfe502da5d99603626e74c197bdef160
PIXEL_EVAL_DOCKER_TEST_IMAGE=alpine@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6 PIXEL_EVAL_GATEWAY_TEST_IMAGE=oven/bun@sha256:8956c7667fa17beb6e3c664115e66bdacfe502da5d99603626e74c197bdef160 bun test eval/controlled.test.ts
```

Without those two optional test-image variables, the unit fixtures run and
Docker cases are explicitly skipped. No test sends a request to a model API.
