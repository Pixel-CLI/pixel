# Brief A/B on a live agent: protocol and results

Issue [#883](https://github.com/Pixel-CLI/pixel/issues/883), the ship criterion:
does the prompt-submit brief make a real Claude Code agent search the
repository less before it answers, with answers that are as good or better?
[`brief-gate.md`](brief-gate.md) measures the brief itself (gate, file recall,
latency) with no agent; this document is the agent-side measurement, driven by
[`scripts/ab-brief-live.py`](../../scripts/ab-brief-live.py) and counted with
the rules of `scripts/excavation-count.py`.

## Arms

All three run the same prompts on the same commit; they differ in one thing.

| arm | binary | fixture and daemon | the hook |
| --- | --- | --- | --- |
| `off` | `old` side | `old/pixel` | the same `run-hook task-event` hook with `PIXEL_BRIEF=0` in the environment: it runs, answers `{}` |
| `old` | `old` side: `pixel 0.7.1 @ 85bede7d` (the baseline) | `old/pixel` | brief on |
| `new` | `new` side: the candidate (PR #887) | `new/pixel` | brief on |

A side is a frozen copy of one binary (`setup` copies it, records its SHA-256 and
refuses a different file later; `run` re-hashes it when it ends), a detached
clone of `Pixel-CLI/pixel` at the labelled commit
`85bede7d9a3c4e385e0a2045241a6466efbd8cbd` with the `origin` remote removed
(`eval/brief-gate/` is not in that tree, so a prompt cannot find itself), indexed
with `prepare-repo`, and that binary's own daemon. The two clones have the same
tree hash and differ only in their path. Daemons of different protocol versions
must not share a root; both daemons are running for every arm, because the
candidate's file quality depends on its daemon. `setup` then calls the hook until
every warm-up prompt (a `dev` row and an identifier, never a `test` row) returns
the same complete brief twice in a row: the first calls answer
`coverage: 1/2 ops answered` while the meaning vectors build.

`off` against `old` is a null contrast on the prompts where the baseline gate
stays silent (nothing is injected, so the two arms are the same configuration):
the report prints that group, and it measures how much two identical arms differ
by chance, the yardstick for every other contrast.

## The agent

The real binary, headless, one session per prompt, the prompt on stdin
(`claude` in an interactive shell is a wrapper function and is not used).
Auth is the user's OAuth in the keychain; `ANTHROPIC_API_KEY` is unset in every
subprocess and `--bare` (which needs an API key) is not used.

```text
claude -p --output-format stream-json --verbose --include-hook-events \
  --setting-sources project,local --settings <arm>.json \
  --strict-mcp-config --disable-slash-commands --no-session-persistence \
  --permission-mode dontAsk --tools Read,Grep,Glob,Bash --allowedTools Read,Grep,Glob,Bash \
  --disallowedTools Edit,Write,NotebookEdit,Agent,Task --max-budget-usd 2
```

Model and effort are not pinned: both are the CLI's own defaults, and the
`init` event of every run records the model (`claude-opus-5-5` for the run
below). `<arm>.json` holds one `UserPromptSubmit` hook,
`'<binary of the side>' run-hook task-event --provider claude --event prompt-submit`
(timeout 10), and nothing else.

| Choice | Why |
| --- | --- |
| `--setting-sources project,local` | drops user-level settings, so the hooks in `~/.claude/settings.json` (which call another `pixel` by absolute path) never fire; it also keeps the user's `~/.claude/CLAUDE.md` out of the context (probed: the model sees the project's `AGENTS.md` and `.claude/rules`, not the global file) |
| `--tools Read,Grep,Glob,Bash` | the questions are read-only and the agent must be free to excavate natively; no edit tool exists, and subagents are off so no exploration happens where the counter cannot see it |
| `--permission-mode dontAsk` with the same four tools allowed | `-p` has nobody to answer a prompt; anything outside the four is denied (`permission_denials` is recorded) |
| `--strict-mcp-config`, `--disable-slash-commands` | no MCP server, no skill: the same blank toolbox in every arm |
| scrubbed environment | only `HOME USER LOGNAME SHELL TMPDIR LANG LC_ALL LC_CTYPE TERM TZ __CF_USER_TEXT_ENCODING NODE_USE_SYSTEM_CA` pass through, plus `PATH` with the side's `bin/` first (an agent that runs `pixel` runs the arm's binary), `DISABLE_AUTOUPDATER=1`, `PIXEL_DAEMON_AUTO_START=0`, and `PIXEL_BRIEF=0` in the `off` arm only. A shell launched by a desktop host carries `CLAUDE_EFFORT`, `CLAUDE_CODE_*`, `BASH_ENV` and API keys that would otherwise leak into the experiment |

Not controlled, and the same in every arm: the fixture's own `AGENTS.md` and
`.claude/rules/*` load into the context (about 21 000 tokens of context in total
for a one-line prompt); the model is what the CLI picks; the machine is shared.

### Isolation evidence

`ab-brief-live.py probe` (4 sessions for three arms, no repository question) must print only
`PASS`:

* **delivery**: with the brief arms the model is asked to copy the `anchors:`
  line of any `[PIXEL:BRIEF]` block in its context and quotes it; with `off` it
  answers `NONE` and the hook returned `{}`;
* **isolation**: the stream holds exactly one hook event, our `UserPromptSubmit`;
  the tool list is `Bash Glob Grep Read`; no MCP server, no skill;
* **sensitivity**: with harmless `SessionStart`, `PreToolUse`, `PostToolUse` and
  `Stop` hooks added to the settings, all four appear in the stream, so their
  absence from the real arms means the user's hooks did not fire.

Every run also carries the check: a run whose hook events are not exactly
`[UserPromptSubmit]`, whose tool list differs, or whose `off` arm received a
brief is marked invalid and excluded.

## Prompts

`eval/brief-gate/prompts.jsonl` (218 rows, SHA-256
`bc6ac042d64b1c96c56d1c83b4052853e0947a3dd7ca0c9b2b655e6f036276ad`), `split: "test"`,
English only. The `dev` rows are for dry runs: the `test` rows are read once per
candidate.

* **On-topic, 16**: `kind: "plain"` (no code-shaped identifier: the prompts the
  lexical gate cannot see and the candidate is meant to help) with
  `expected_files`. 23 rows qualify; the 16 kept are the first by
  `sha256("brief-ab-883:" + id)`, so the pick is neither the author's taste nor
  the file order.
* **Off-topic, 4**: one per kind in the order other-repo, generic-code, chat, ops,
  the first by the same hash among the rows that ask for no action (a deny list
  of verbs such as pull, push, install, release, fix: the agent has Bash). They
  are the cost check: a brief on a prompt it should not touch must not add
  exploration.

`ab-brief-live.py select --split test` prints the rows. The text is submitted
exactly as in the set.

## Runs

Two repetitions per prompt and arm (120 sessions). The order is randomised with
`--seed 883`: repetition by repetition (so an early stop leaves a complete first
repetition), prompts shuffled inside a repetition and arms inside a prompt, at
most four sessions at once. A session is killed after 900 s or at USD 2 of list
cost. The driver records the 1-minute load average at each launch, stops
launching at 0.9 of the five-hour usage window (the number is in every
`rate_limit_event`; the run resumes with the same `--run-id`), and checks before
every session that the side's daemon is alive.

## What is measured, per run

Read from the `stream-json` structure; the transcript prose is never
pattern-matched (the one text test is for `[PIXEL:BRIEF]` at the start of the
`additionalContext` the hook returned).

| Metric | Definition |
| --- | --- |
| `native` | Grep, Glob, Read and Bash calls that search or read (`grep`, `rg`, `find`, `cat`, `ls`, `sed -n`, `git grep`, ...) before the first edit or the closing answer: `scripts/excavation-count.py`'s classifier and stop rule |
| `tools_total`, `pixel` | every tool call; the calls to the `pixel` CLI, apart (a drop in `native` bought with `pixel` calls is a move, not a saving) |
| tokens | `input_total` = input + cache creation + cache read summed over the session's requests, and `output_tokens`, from the `result` event; `cost_usd` is the list price the CLI reports |
| wall time | the driver's own clock, process start to exit |
| `brief_fired`, `brief_tier` | the hook's `additionalContext` starts with `[PIXEL:BRIEF]`; `confidence: high` or `low` is the tier (the baseline prints none); `coverage: n/m ops answered` is kept |
| `cites_expected` | the final answer names at least one `expected_files` path, exact after normalisation (`./`, the fixture root, `:line`, trailing punctuation); `cites_basename` is the looser diagnostic. The quality proxy: it does not read the answer |

A run is **valid** when the result is `success`/`completed`, the process exited
0, the rate limit was not hit, no hook but ours ran, the tools are the four,
and the fixture was not left dirty by it. Invalid runs stay in `runs.jsonl` with
their reasons and are left out of every table.

## Reading it

`report` pairs the arms by prompt: the mean of the prompt's valid runs in each
arm, the difference per prompt, the mean of those differences with a 95%
bootstrap interval over prompts (5 000 resamples, seed 883), and how many
prompts got lighter, equal or heavier. Groups: on-topic, off-topic, the
on-topic prompts where the brief fired, and `new` by tier against `off` on the
same prompts. The criteria, fixed before the run:

1. **Searches less**: on-topic `native` difference (`new` minus `off`) below 0
   with the whole interval below 0.
2. **Answers not worse**: the on-topic citation-rate difference at least
   -0.05 (a run is 0.03 of the rate).
3. **No cost on prompts it should not touch**: off-topic `native` difference at
   most +0.5.

The same three are printed for `new` against `old` and for `old` against `off`.
They are a mechanical reading, not a ship decision: criterion 3 compares a mean of
four prompts, three of which no brief touches, so it moves with noise (see the
results).

## Reproduce

```bash
W=/private/tmp/claude-501/883-ab                       # any scratch directory
python3 scripts/ab-brief-live.py setup --work $W --side old --binary <baseline pixel> --expect-sha 85bede7d
python3 scripts/ab-brief-live.py setup --work $W --side new --binary <candidate pixel> --expect-sha <commit>
python3 scripts/ab-brief-live.py probe --work $W --run-id probe --arms off,old,new
python3 scripts/ab-brief-live.py run   --work $W --run-id <id> --arms off,old,new --reps 2 --split test --concurrency 4
python3 scripts/ab-brief-live.py report --work $W --run-id <id> --receipt docs/bench/brief-ab/<id>.json
```

The raw streams stay in `eval/arena-results/<id>/` (gitignored). The receipt is
the manifest (binary hashes, fixture, prompt ids, flags, claude version and
hash, machine, times) and one row of numbers per run; it holds no transcript or
answer, and the work directory is written `$WORK`. Do not rebuild a binary
while a campaign runs.

## Limits

* **One agent, one model, one repository.** The result is for Claude Code at its
  default model on this repository; it says nothing about pi, Codex or another
  codebase.
* **Citation is a proxy.** `expected_files` is a short list a human labelled; an
  answer that is correct and names another file scores a miss, and one that names
  the file with a wrong claim scores a hit. Equal citation does not prove equal
  quality.
* **16 plain prompts, 2 repetitions.** A prompt moves a mean difference by one
  sixteenth; read the interval, not the point.
* **Temperature and tool choice vary between repetitions**, which is why every
  contrast is read against `off` versus `old`.
* **The machine is shared** with other agents; the load at each launch is
  recorded, and the brief's 750 ms deadline can be missed under load
  (`coverage` below `n/n`).
* **No local daemon state is reset between runs**: the daemon and the index
  persist, and the hook writes session records under the fixture's `.pixel/`.
* **Cost**: a session is about USD 0.3 at list price on the default model; the
  five-hour window of a subscription is shared with everything else the user
  runs.

## Dry run (dev rows, before the campaign)

`probe` printed ten `PASS` lines (delivery, isolation and sensitivity for the three
arms). Then 9 sessions on `dev` rows (bg-056 and bg-085 on-topic, bg-217 off-topic;
three arms, one repetition, so no `test` row was read before the run): all valid,
brief fired on `new` only (tiers low, high, low), 1 to 7 native calls, USD 2.71.
They also exposed two things fixed before the campaign: the inherited `CLAUDE_*`
environment (a first manual session carried the host's `CLAUDE_EFFORT=xhigh` and
listed 25 tools), and the need to warm the candidate's meaning vectors. A number
formatting bug in the report (`1990` printed as `199`) was found after the run and
fixed in the report code only.

## Results: `ab-20261009`

Run identity.

| | |
| --- | --- |
| started / ended | 2026-10-09 11:29Z to 11:41Z, 733 s, 120 sessions, concurrency 4; 120 valid, 0 invalid |
| agent | Claude Code 2.1.295 (`~/.local/share/claude/versions/2.1.295`, SHA-256 `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`), model `claude-opus-5-5` for every session, effort not pinned, `apiKeySource: none` |
| `old` binary | `pixel 0.7.1`, commit `85bede7d`, SHA-256 `6a8f3d7080e5d540bbe305c50334ef0ed9d17122e3febde93ab7178b9e216788` |
| `new` binary | `pixel 0.7.1`, commit `44bbaaac9f88609070df363fb8a4886ecbd1b66e` (PR #887), SHA-256 `ac2dc892f43c1f06919c5cefc75300d3b2f9b50270c2004b2e6448270b3c21ea`; hashes identical at the start and at the end of the run |
| fixtures | `Pixel-CLI/pixel` at `85bede7d9a3c4e385e0a2045241a6466efbd8cbd`, tree `8b95943aa1bd077f1690c3f3d7aa07283e75b97c` in both; clean at the end; both daemons alive during the run (0 restarts); `new` brief settled after 6 warm-up calls; of its 28 fired briefs 24 print `coverage: 2/2 ops answered` and the 4 `low` ones print no coverage line |
| prompts | the 16 + 4 `test` rows above (`bg-015 bg-026 bg-032 bg-037 bg-048 bg-086 bg-093 bg-110 bg-120 bg-128 bg-130 bg-161 bg-178 bg-203 bg-209 bg-215`; off-topic `bg-040 bg-145 bg-177 bg-205`), set SHA-256 `bc6ac042...6ad`, order seed 883 |
| driver | `scripts/ab-brief-live.py` at commit `91c51e56` (SHA-256 `af5baadd8c1292e0882841f263f2bb7739779a9aaade444fd9a81bb6bd2ac095`); the report code was extended after the run (per-prompt table, groups, number formatting) and re-run on the same `runs.jsonl` |
| machine | macOS arm64, 16 CPUs, shared with other agents; 1-minute load 2.8 to 5.6 at launch (mean 4.4); Ollaya not warm; list cost USD 24.43 (off 8.22, old 7.74, new 8.48); the five-hour usage window went from 0.55 to 0.82 |
| commands | `ab-brief-live.py setup` x2, `probe`, `run --run-id ab-20261009 --arms off,old,new --reps 2 --split test --concurrency 4`, `report --run-id ab-20261009 --receipt docs/bench/brief-ab/ab-20261009.json` |

Per arm (valid sessions only; `native` is counted before the first answer; tokens
are summed over the session's requests, cache reads included; `cites` is the share
of sessions whose answer names an expected file):

| arm / group | sessions | brief fired | native mean | native median | native p90 | tool calls mean | input tok mean | output tok mean | wall s mean | wall s median | cites |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| off, on-topic | 32 | 0 | 4.50 | 5 | 7 | 4.50 | 124 279 | 1 678 | 22.7 | 21.7 | 0.78 |
| old, on-topic | 32 | 0.44 | 4.12 | 4 | 6 | 4.12 | 107 503 | 1 581 | 20.2 | 19.4 | 0.81 |
| new, on-topic | 32 | 0.88 | 4.03 | 4 | 7 | 4.03 | 119 279 | 1 597 | 19.9 | 20.2 | 0.84 |
| off, off-topic | 8 | 0 | 2.25 | 0.5 | 8 | 5.25 | 182 186 | 2 232 | 39.7 | 20.8 | n/a |
| old, off-topic | 8 | 0.25 | 1.38 | 0 | 6 | 3.50 | 125 289 | 1 703 | 30.6 | 18.0 | n/a |
| new, off-topic | 8 | 0.25 | 2.00 | 0 | 8 | 4.00 | 138 514 | 1 990 | 39.9 | 21.6 | n/a |

`new` on-topic sessions by the brief it received, against `off` on the same
prompts:

| `new` group | sessions | native mean | native median | `off` native mean | `off` native median | wall s | cites | `off` cites |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| tier `high` | 24 | 3.83 | 3 | 4.54 | 4.5 | 19.9 | 0.79 | 0.71 |
| tier `low` | 4 | 3.50 | 3.5 | 2.75 | 2.5 | 16.4 | 1 | 1 |
| no brief | 4 | 5.75 | 5.5 | 6.00 | 6 | 23.6 | 1 | 1 |
| brief names an expected file in its first 8 paths | 24 | 3.92 | 3.5 | 4.50 | 4.5 | 19.5 | 0.79 | 0.71 |
| brief fired, names none | 4 | 3.00 | 2.5 | 3.00 | 2.5 | 18.6 | 1 | 1 |

Off-topic `new`: the brief fired on both repetitions of bg-145 ("check which github
actions are failing right now", tier `high`: a false positive of the gate, the
same as the baseline's) and on nothing else.

Paired by prompt (mean of a prompt's two sessions per arm; difference `to` minus
`from`; 95% bootstrap interval over prompts; lighter / equal / heavier prompts):

| from to | group | prompts | native difference | lighter / equal / heavier | wall s | input tok | cites |
| --- | --- | ---: | --- | --- | ---: | ---: | ---: |
| off to new | on-topic | 16 | -0.47 [-1.59, +0.72] | 8 / 5 / 3 | -2.9 | -5 | +0.06 |
| off to new | on-topic, brief fired | 14 | -0.50 [-1.75, +0.82] | 7 / 4 / 3 | -3.4 | -3 138 | +0.07 |
| off to new | off-topic | 4 | -0.25 [-0.75, 0] | 1 / 3 / 0 | +0.2 | -43 672 | n/a |
| off to old | on-topic | 16 | -0.38 [-1.19, +0.53] | 8 / 4 / 4 | -2.5 | -16 776 | +0.03 |
| off to old | on-topic, brief never fired (null contrast) | 9 | -0.28 [-0.78, +0.17] | 4 / 3 / 2 | -0.8 | -7 198 | 0 |
| old to new | on-topic | 16 | -0.09 [-1.09, +1.00] | 8 / 1 / 7 | -0.3 | +11 776 | +0.03 |
| old to new | off-topic | 4 | +0.62 [0, +1.50] | 0 / 2 / 2 | +9.3 | +13 225 | n/a |

Run-to-run standard deviation of `native` between the two sessions of one prompt
and arm: 1.43 on-topic, 0.46 off-topic. Mechanical criteria: `new` against `off`
searches less **not met** (the interval holds 0), answers not worse met (+0.06),
off-topic cost met (-0.25); `new` against `old`: not met, met, and **not met**
(+0.62, which is bg-040 alone: no brief fired in either arm, 6 and 5 native calls
for `old` against 8 and 7 for `new`, the size of the noise).

Per prompt (`native` of each repetition; brief: tier, `y` fired with no tier, `-`
silent; cites: Y named an expected file, n did not):

| prompt | off | old | new | old brief | new brief | cites off/old/new |
| --- | ---: | ---: | ---: | --- | --- | --- |
| bg-015 | 5,3 | 4,3 | 5,3 | -,- | low,low | YY/YY/YY |
| bg-026 | 3,4 | 7,9 | 4,7 | y,y | high,high | nn/Yn/nY |
| bg-032 | 8,4 | 6,6 | 7,4 | -,- | high,high | nn/nn/nn |
| bg-037 | 2,1 | 3,2 | 4,2 | -,- | low,low | YY/YY/YY |
| bg-048 | 4,5 | 3,2 | 3,1 | y,y | high,high | YY/YY/YY |
| bg-086 | 5,12 | 10,4 | 3,3 | -,- | high,high | YY/YY/YY |
| bg-093 | 6,7 | 4,3 | 4,3 | y,y | high,high | YY/YY/YY |
| bg-110 | 5,7 | 3,4 | 3,7 | y,y | high,high | nY/nY/nn |
| bg-120 | 7,5 | 5,5 | 7,5 | -,- | -,- | YY/YY/YY |
| bg-128 | 3,3 | 1,1 | 3,3 | y,y | high,high | YY/YY/YY |
| bg-130 | 6,6 | 5,5 | 5,6 | -,- | -,- | YY/YY/YY |
| bg-161 | 2,2 | 2,5 | 2,2 | y,y | high,high | YY/YY/YY |
| bg-178 | 1,1 | 1,1 | 7,6 | -,- | high,high | nn/nn/YY |
| bg-203 | 6,5 | 5,6 | 4,2 | y,y | high,high | YY/YY/YY |
| bg-209 | 3,3 | 4,3 | 3,3 | -,- | high,high | YY/YY/YY |
| bg-215 | 5,5 | 5,5 | 4,4 | -,- | high,high | YY/YY/YY |
| bg-040 (off) | 8,7 | 6,5 | 8,7 | -,- | -,- | |
| bg-145 (off) | 1,2 | 0,0 | 0,1 | y,y | high,high | |
| bg-177 (off) | 0,0 | 0,0 | 0,0 | -,- | -,- | |
| bg-205 (off) | 0,0 | 0,0 | 0,0 | -,- | -,- | |

### Reading

* **The candidate's brief fires on twice as many plain prompts** (28 of 32
  on-topic sessions, 14 of 16 prompts, against 14 of 32 and 7 of 16 for the
  baseline), and a hook latency of 176 ms median (baseline 117 ms).
* **The direction is the expected one, the size is inside the noise.** On-topic,
  `new` makes 4.03 native calls against 4.50 with no brief (median 4 against 5,
  8 prompts lighter, 3 heavier), but the interval is -1.59 to +0.72, and the
  baseline, which injected nothing on 9 of those prompts, differs from `off` by
  -0.28 on them. With a per-session standard deviation of 1.43 and 16 prompts
  the run could only have shown a drop of about 1.2 calls or more. It neither
  shows the candidate to help nor rules out a drop of about half a call.
* **Where it helped**: bg-086 (5,12 to 3,3), bg-203 (6,5 to 4,2), bg-215, bg-093,
  bg-048. **Where it cost**: bg-178, whose two `off` sessions answered after one
  call without naming `envfile.rs`, and whose `new` sessions (the brief names
  `envfile.rs`, tier high) read around for 6 and 7 calls and named it: more
  work, a better answer by the proxy. bg-026 (4,7 against 3,4) is the other
  prompt it made heavier by more than a call.
* **Answers did not get worse** by the proxy: 0.84 against 0.78 (two sessions of
  32). The one prompt that went the other way is bg-110: `off` cited an expected
  file in one of two sessions, `new` in none.
* **The brief's usefulness depends on naming the file.** The 24 sessions whose
  brief names an expected file in its first 8 paths made 3.92 calls against 4.50;
  tier `high` made 3.83 against 4.54. Tier `low` (4 sessions) and the 4 sessions
  with no brief (bg-120, bg-130) are too few to read.
* **No extra tokens or time**: input tokens per prompt -5 (mean, cache reads
  included), wall time -2.9 s, list cost +USD 0.01. The off-topic cost check is
  noise-bound (4 prompts, 2 of them without any tool call).
* **What this cannot say**: that the brief makes the agent lighter at equal
  quality in general. The per-prompt differences have a standard deviation of 2.4
  calls, of which session noise (1.43 per session) explains little: the effect
  differs by prompt (bg-086 against bg-178). More repetitions of the same prompts
  would not narrow the interval much; halving it takes about four times the
  prompts (about 64), and the set holds 23 plain prompts with expected files over
  both splits. Every prompt costs 6 sessions (3 arms, 2 repetitions), about USD 1.2
  and 1.3 points of the five-hour usage window.
