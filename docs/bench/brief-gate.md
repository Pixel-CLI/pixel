# Brief gate: baseline and A/B protocol

Issue [#883](https://github.com/Pixel-CLI/pixel/issues/883), part 1. Nothing in
the tool changed: this is the labelled prompt set, the command that measures
it, the numbers of the behaviour as it stands, and the metric for the
agent-side question the brief exists for. The parts that change the gate and
the retriever are read with the same command, so a before and an after differ
only in `--pixel`.

The prompt-submit brief (`pixel brief`, the command `pixel run-hook task-event
--event prompt-submit` goes through) raises three questions, and the numbers
keep them apart:

1. **Gate**: does it fire on a prompt about this repository and stay silent on
   one that is not? `code_signal` (`crates/pixel/src/execution_brief.rs`) is
   lexical: a code-shaped token fires it, a word of an English list asks the
   intent judge (`pixel classify`, only when its local server is already warm),
   anything else gets no brief.
2. **Relevance**: when it fires, does it name the files the prompt needs?
3. **Excavation**: does the agent then search less? Measured by
   `scripts/excavation-count.py`; the live run waits for the part that changes
   the brief (protocol below).

## Run identity

| | |
| --- | --- |
| repository under test | `Pixel-CLI/pixel` at `85bede7d9a3c4e385e0a2045241a6466efbd8cbd`: a detached, clean worktree of that commit, indexed with `pixel prepare-repo --no-daemon` (the fixture; `fixture_match` is `true` in every result) |
| binary | `pixel 0.7.1 / commit: 85bede7d9a3c4e385e0a2045241a6466efbd8cbd`; `CARGO_PROFILE_DEV_DEBUG=0 cargo build --profile dev-release -p pixel-cli` in a clean checkout of that commit |
| prompt set | `eval/brief-gate/prompts.jsonl`, 218 rows, SHA-256 `bc6ac042d64b1c96c56d1c83b4052853e0947a3dd7ca0c9b2b655e6f036276ad` |
| runner | `scripts/bench-brief-gate.py`, `--repeat 3`: every prompt run three times, the output identical each time (0 unstable rows over the four runs) and the latency the median of three |
| machine | `macOS-27.0-arm64-arm-64bit-Mach-O`, Python 3.14.7, shared with other agents: 1-minute load average 11 to 21 over the four runs (16 CPUs) |
| intent judge | no server listening on the Ollaya port (`ollaya_warm: false`): `pixel classify --if-warm` ran on 53 weak-signal prompts and returned no verdict, so the heuristic plan decided every one. A warm server can silence a brief the heuristic would have sent; the runner prints a note when it sees one |

## Reproduce

```bash
FIXTURE=/path/to/fixture
git worktree add --detach "$FIXTURE" 85bede7d9a3c4e385e0a2045241a6466efbd8cbd
PIXEL=target/dev-release/pixel                    # the binary under test
"$PIXEL" prepare-repo "$FIXTURE" --no-daemon

python3 scripts/bench-brief-gate.py --check-set --repo "$FIXTURE"     # labels still hold in the fixture
for split in dev test all; do
  python3 scripts/bench-brief-gate.py --pixel "$PIXEL" --repo "$FIXTURE" \
    --split "$split" --daemon off --repeat 3 --out "gate-$split-off.json"
done
python3 scripts/bench-brief-gate.py --pixel "$PIXEL" --repo "$FIXTURE" \
  --split all --daemon on --repeat 3 --out gate-all-on.json
```

`--daemon on|off` starts or stops the fixture's daemon around the run and puts
it back after. The runner forces the brief on (`PIXEL_BRIEF=1`, so a
`brief: false` in a pixel config cannot silence it), refuses to start when a
prompt that names a code token gets no brief (an unindexed fixture), and warns
when the fixture's HEAD is not the labelled commit or when `eval/brief-gate/`
exists in it (the set would index itself).

## Baseline

What each column means: **gate** columns score "a brief came back" against
`on_topic` (precision = of the briefs sent, how many were wanted; recall = of
the on-topic prompts, how many got one). **FP** is the share of off-topic rows
that got a brief. **hit@8** is the share of rows with `expected_files` whose
first eight brief paths (`defined:`, `files:`, `callers`, `tests`, `targets` in
the order the brief prints them) hold at least one expected file; a row with no
brief counts as a miss, **when fired** leaves those rows out. Latency is the
wall time of `pixel brief`, process start to exit.

| run | prompts | gate precision | gate recall | F1 | recall, en | recall, fr | FP, off-topic | FP, ops+chat | hit@8 | hit@8 when fired | latency p50 / p95, all (ms) | latency p50 / p95, fired (ms) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `dev`, daemon off | 109 | 72.5% | 47.5% | 57.4% | 62.8% | 11.1% | 22.9% | 18.2% | 20.9% | 42.9% | 25 / 123 | 83 / 128 |
| `test`, daemon off | 109 | 76.5% | 42.6% | 54.7% | 54.8% | 15.8% | 16.7% | 9.4% | 31.0% | 68.4% | 24 / 122 | 84 / 132 |
| `all`, daemon off | 218 | 74.3% | 45.1% | 56.1% | 58.8% | 13.5% | 19.8% | 13.8% | 25.9% | 55.0% | 23 / 120 | 79 / 127 |
| `all`, daemon on | 218 | 74.3% | 45.1% | 56.1% | 58.8% | 13.5% | 19.8% | 13.8% | 24.7% | 52.5% | 23 / 79 | 68 / 89 |

### Split `all`, daemon off

| gate | n | TP | FP | FN | TN | precision | recall | F1 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| overall | 218 | 55 | 19 | 67 | 77 | 74.3% | 45.1% | 56.1% |
| lang=en | 154 | 50 | 18 | 35 | 51 | 73.5% | 58.8% | 65.4% |
| lang=fr | 64 | 5 | 1 | 32 | 26 | 83.3% | 13.5% | 23.3% |
| source=intent-eval | 37 | 2 | 9 | 2 | 24 | 18.2% | 50.0% | 26.7% |
| source=real-paraphrased | 13 | 3 | 1 | 5 | 4 | 75.0% | 37.5% | 50.0% |
| source=recall | 12 | 2 | 2 | 3 | 5 | 50.0% | 40.0% | 44.4% |
| source=session-context | 16 | 6 | 2 | 2 | 6 | 75.0% | 75.0% | 75.0% |
| source=synthetic | 140 | 42 | 5 | 55 | 38 | 89.4% | 43.3% | 58.3% |

| on-topic recall | rows | fired | recall |
| --- | ---: | ---: | ---: |
| kind=identifier | 18 | 18 | 100.0% |
| kind=plain | 104 | 37 | 35.6% |

| off-topic false positives | rows | fired | FP rate |
| --- | ---: | ---: | ---: |
| all off-topic | 96 | 19 | 19.8% |
| ops + chat | 65 | 9 | 13.8% |
| kind=chat | 26 | 2 | 7.7% |
| kind=generic-code | 10 | 2 | 20.0% |
| kind=meta | 6 | 0 | 0.0% |
| kind=ops | 39 | 7 | 17.9% |
| kind=other-repo | 8 | 5 | 62.5% |
| kind=paste | 7 | 3 | 42.9% |

| files (rows with expected_files) | value |
| --- | ---: |
| rows / fired | 85 / 40 |
| recall@8 (unfired = 0) | 23.5% |
| hit@8 (unfired = miss) | 25.9% |
| recall@8 when fired | 50.0% |
| hit@8 when fired | 55.0% |
| `files:` line only, recall@8 | 21.2% |
| `files:` line only, hit@8 | 23.5% |
| any path in the brief, hit | 25.9% |

| by prompt kind | rows | fired | hit@8 | hit@8 when fired |
| --- | ---: | ---: | ---: | ---: |
| kind=identifier | 16 | 16 | 87.5% | 87.5% |
| kind=plain | 69 | 24 | 11.6% | 33.3% |

| latency (ms) | n | p50 | p95 | max |
| --- | ---: | ---: | ---: | ---: |
| all | 218 | 23 | 120 | 188 |
| fired | 74 | 79 | 127 | 188 |

### What the numbers say

1. **The gate sees identifiers, not intent.** All 18 on-topic
   prompts with a code-shaped token got a brief (100.0%); of the
   104 plain-language ones, 37 did (35.6%).
   French is the extreme: 5 of 37 on-topic French prompts
   (13.5%), because the word lists are English.
2. **It fires on things that are not about this repository.** 19 of
   96 off-topic prompts got a brief (19.8%):
   5 of 8 prompts about some other codebase,
   9 of 65 ops and chat prompts, 2 of 10 generic programming
   questions. `fix <URL of a pull request>` and `what do you think about the new
   iPhone` both fire (`fix ` opens a code question; `iPhone` has an inner
   capital). A pasted thread is silent when it is wrapped in `<pasted_content>`
   (0 of 4) and fires when it is not (3 of 3).
3. **When it fires on a plain prompt, it is right about a third of the time.**
   hit@8 when fired is 87.5% on identifier prompts and
   33.3% on plain ones; end to end (no brief counts as a miss) plain
   prompts reach 11.6%. The tail of a `files:` line is usually filename
   matches on one word of the prompt: for `okay so how does the copilot hook get
   installed`, `hook` brought `hook_input.rs`, `task_hook.rs` and
   `eval/arena/hook_audit.py` in beside `copilot_config.rs`.
4. **The daemon changes the answer, not the gate.** Fired or not is identical
   with the daemon on (45.1% recall, 74.3% precision), but 69 of the
   74 briefs differ in text: 48 name a different set of files, 1 the same
   files in another order, 20 differ elsewhere in the block. hit@8 is
   24.7% against 25.9%. The two evidence routes (daemon, local
   readers) are not equivalent, so a number is only comparable with the same
   `--daemon`.
5. **Latency is a few tens of milliseconds, inside the 750 ms window.** p50
   23 ms over all prompts (most do not fire), 79 ms when a brief is built
   (p95 127). The machine was loaded (above), so read p50 as an upper
   bound and compare only runs taken back to back.

## After (PR 3): the relevance gate and the two-tier brief

Part 3 of the issue ([#883](https://github.com/Pixel-CLI/pixel/issues/883)):
a plain-language prompt gets a brief when a fixed four-feature model finds the
repository talks about it, a full one (high tier) or a compact "possibly
related" one (low tier), and none otherwise; a weakly code-shaped prompt goes
through the same gate; a code-shaped prompt is not gated. The model is English
only (French is information in every table below). It was fitted on the 78
English **dev** rows (`scripts/research-gate/results/gate-model.json`), so the
dev numbers are the fitting set and the **test** numbers are the read: the
test split was run once, after the candidate was frozen, and nothing was
tuned on it.

### Run identity

| | |
| --- | --- |
| candidate | `feat/883-brief-prose-gate` at `44bbaaac9f88609070df363fb8a4886ecbd1b66e` (stacked on #884, #885 at `bb6f2191`, #886 at `d4d98d31`); binary `pixel 0.7.1 / commit: 44bbaaac…`, `CARGO_PROFILE_DEV_DEBUG=0 cargo build --profile dev-release -p pixel-cli` from that commit, clean tree |
| baseline | `pixel 0.7.1 / commit: 85bede7d9a3c4e385e0a2045241a6466efbd8cbd`, built the same way in a clean clone of that commit, run in the same session as the candidate |
| repository under test | the same fixture as the baseline above: a clean clone checked out at `85bede7d`, indexed with `pixel prepare-repo --no-daemon` (`fixture_match: true` in every result). Not the candidate's own tree: it carries `eval/brief-gate/`, which the runner warns about (the set would index itself), and the labels were read against `85bede7d` |
| prompt set | `eval/brief-gate/prompts.jsonl`, 218 rows, SHA-256 `bc6ac042d64b1c96c56d1c83b4052853e0947a3dd7ca0c9b2b655e6f036276ad`; test split: 109 rows, 76 English (42 on-topic, 34 off-topic) and 33 French |
| runner | `scripts/bench-brief-gate.py`, `--repeat 3 --warmup 3`; 0 unstable rows and 0 errors in the four test runs; order baseline off, candidate off, baseline on, candidate on |
| daemon on | the fixture's daemon started with the binary under test; for the candidate the `meaning` vectors were warmed first (`pixel brief --json` on a plain prompt until two probes answered), because the first calls answer `unavailable` while they build |
| machine | `macOS-27.0-arm64-arm-64bit-Mach-O`, Python 3.14.7, 16 CPUs, shared with other agents: 1-minute load average 5 to 12 across the test runs; compare latencies of runs taken back to back only |
| intent judge | no server on the Ollaya port: the judge ran on weak and plain prompts and returned no verdict. The gate does not use it for plain prompts; a warm server can still silence a weak prompt (`none`, 0.5 or more) |

### Reproduce

```bash
git clone -q --no-checkout "$REPO" "$FIXTURE" && git -C "$FIXTURE" checkout -q 85bede7d
"$PIXEL" prepare-repo "$FIXTURE" --no-daemon
python3 scripts/bench-brief-gate.py --check-set --repo "$FIXTURE"
for daemon in off on; do
  python3 scripts/bench-brief-gate.py --pixel "$PIXEL" --repo "$FIXTURE" \
    --split test --daemon "$daemon" --repeat 3 --warmup 3 --out "gate-test-$daemon.json"
done
```

`$PIXEL` is the candidate for the "PR 3" rows and the `85bede7d` build for the
"main" rows. The "any brief" and "high only" columns are the same run read two
ways: a low-tier brief counts as shown in the first and as not shown in the
second (the runner reads the tier from `.pixel/brief-decisions.jsonl`, the
decision log the candidate writes).

### Results, English rows

Gate columns score "a brief came back" against `on_topic`. **FP** is the share
of the off-topic rows that got a brief. **hit@8** is the share of the 31 test
(32 dev) rows with `expected_files` whose first eight brief paths hold one,
no brief counting as a miss; **when fired** leaves the unfired out. Latency is
the wall time of `pixel brief` over all 109 prompts of the split (French
included), the median of three calls per prompt.

| split | run | brief | TP | FP | FN | TN | precision | recall | F1 | FP rate |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| test | main `85bede7d` | any | 23 | 7 | 19 | 27 | 76.7% | 54.8% | 63.9% | 20.6% |
| test | PR 3 | any | 33 | 5 | 9 | 29 | 86.8% | 78.6% | 82.5% | 14.7% |
| test | PR 3 | high only | 29 | 3 | 13 | 31 | 90.6% | 69.0% | 78.4% | 8.8% |
| dev | main `85bede7d` | any | 27 | 11 | 16 | 24 | 71.1% | 62.8% | 66.7% | 31.4% |
| dev | PR 3 | any | 33 | 6 | 10 | 29 | 84.6% | 76.7% | 80.5% | 17.1% |
| dev | PR 3 | high only | 31 | 4 | 12 | 31 | 88.6% | 72.1% | 79.5% | 11.4% |

The gate is the same with the daemon on and off; only the files differ. Recall
on plain-language prompts (the case the gate is for; identifier prompts stay
at 100% everywhere): test 42.4% to 72.7% (any) or 60.6% (high only); dev 52.9%
to 70.6% or 64.7%.

| split | run | brief | hit@8, daemon off (when fired) | hit@8, daemon on (when fired) | latency p50 / p95 ms, off | latency p50 / p95 ms, on |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| test | main | any | 41.9% (68.4%) | 38.7% (63.2%) | 22 / 120 | 22 / 76 |
| test | PR 3 | any | 38.7% (46.2%) | 64.5% (76.9%) | 165 / 202 | 98 / 135 |
| test | PR 3 | high only | 38.7% (50.0%) | 61.3% (79.2%) | same runs | same runs |
| dev | main | any | 28.1% (42.9%) | 28.1% (42.9%) | 25 / 119 | 25 / 81 |
| dev | PR 3 | any | 53.1% (60.7%) | 71.9% (82.1%) | 163 / 203 | 113 / 155 |
| dev | PR 3 | high only | 50.0% (61.5%) | 68.8% (84.6%) | same runs | same runs |

False positives by kind of off-topic prompt (fired / rows, English):

| split | run | ops | chat | paste | generic-code | other-repo | meta |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| test | main | 3/15 | 0/9 | 0/2 | 1/3 | 3/4 | 0/1 |
| test | PR 3, any | 4/15 | 0/9 | 0/2 | 0/3 | 1/4 | 0/1 |
| test | PR 3, high only | 2/15 | 0/9 | 0/2 | 0/3 | 1/4 | 0/1 |
| dev | main | 4/15 | 2/10 | 2/2 | 1/2 | 2/4 | 0/2 |
| dev | PR 3, any | 3/15 | 1/10 | 0/2 | 1/2 | 1/4 | 0/2 |
| dev | PR 3, high only | 3/15 | 1/10 | 0/2 | 0/2 | 0/4 | 0/2 |

Test, English, where the briefs went (candidate, by the signal that started
them and the tier the gate gave): plain language 9 high and 4 low on topic, 1
low off topic, 6 on-topic and 18 off-topic prompts off; weak 12 high on topic
and 3 high off topic, 1 low off topic, 3 on-topic and 3 off-topic off; 8
code-shaped on-topic prompts (not gated).

**Weak prompts the gate silences** (they fired on the baseline). Test: 6 of
22, three right (`bg-075`, `bg-125`, `bg-143`) and three on-topic prompts lost
(`bg-033`, `bg-139`, `bg-164`). Dev: 11 of 25, seven right (`bg-011`, `bg-101`,
`bg-112`, `bg-114`, `bg-159`, `bg-201`, `bg-202`) and four on-topic prompts lost
(`bg-022`, `bg-036`, `bg-158`, `bg-212`). With the gate off for weak prompts
(`ENFORCE_GATE_ON_WEAK` false, dev only) F1 was 79.6% against 80.5% and the
false-positive rate 37.1% against 17.1%.

**French, information only** (the model was not fitted on it): every dev prompt
is off; on the test split 1 of 19 on-topic prompts gets a brief (5.3% recall,
against 15.8% on main) and no off-topic one does (main: 1 of 14).

### What the numbers say

1. **The gate does what it was fitted for, and the read confirms it.** On the
   untouched test split, English precision rises from 76.7% to 86.8% and recall
   from 54.8% to 78.6% (F1 63.9% to 82.5%) while the false-positive rate falls
   from 20.6% to 14.7%, 8.8% with only the full briefs counted. The dev numbers
   (the fitting set) are within about three points of the test numbers on every
   column.
2. **The remaining false positives are weak and ops prompts.** Test: three
   weak prompts at the high tier (`how does the auth middleware decide between
   jwt and session`, `can you list everything we have to test since the latest
   release`, `check which github actions are failing right now`), a weak and a
   plain one at the low tier. Ops prompts are the one category that did not
   improve on test (4 of 15 against 3 of 15): this tool's own vocabulary is
   git, release and CI.
3. **Without the daemon the files are no better than before** (the hooks now
   start one, so this is the first prompt after a break at worst: see "Cold
   start" below). hit@8 is 38.7% on test against 41.9% (46.2% when fired
   against 68.4%): the in-process route
   has no `meaning` leads, only the co-file order, and the gate now fires on
   plain prompts the baseline did not answer. With the daemon, hit@8 is 64.5%
   against 38.7%.
4. **The gate costs time.** p50 over all prompts goes from 22 ms to 98 ms with
   the daemon (165 ms without), p95 from 76 ms to 135 ms (202 ms): every plain
   or weak prompt now asks for the relevance block. All of it sits inside the
   750 ms window, and the machine was loaded.

### Cold start: the hooks keep a daemon behind the brief

The "Without the daemon" rows above are what a prompt gets after a break (the
daemon exits after thirty minutes idle) or an upgrade (a protocol bump leaves an
older daemon unusable), because the brief only pinged the socket. A session
starting, and a briefed prompt that finds no daemon, now start the repository's
daemon in the background (`execution_brief/autostart.rs`, "Keeping a daemon
behind the brief" in `ARCHITECTURE.md`); the prompt that found none keeps the
in-process route, the next one gets the daemon. This section measures what that
buys. It is the **dev** split only (the test split was read once, above, and is
not touched again), English rows for the headline.

| | |
| --- | --- |
| binary | `pixel 0.7.1 / commit: 0b298428a3b3a9bb6bae55f35a0ae7a9d891febe`, clean tree, `CARGO_PROFILE_DEV_DEBUG=0 cargo build --profile dev-release -p pixel-cli` into a target directory of its own; the code is that of `5e586c12` (the next commit changes only the bench script) |
| repository, set | the fixture of the runs above (clean clone at `85bede7d`, `fixture_match: true`), set SHA-256 `bc6ac042…6276ad`; dev split: 109 rows, 78 English (43 on-topic, 35 off-topic); 0 errors and 0 unstable rows in all ten runs |
| modes | `on`: daemon started and the `meaning` vectors warmed first (`--warm-meaning`); `off`: daemon stopped and auto-start disabled; `cold`: daemon stopped, auto-start on, no canary and no warm-up, the 109 prompts one after another in file order, back to back (the worst case: a person types for seconds between prompts); `cold-session`: as `cold` after a `SessionStart` hook and 3 s; `cold-nocache`: as `cold` with `.pixel/code-vectors/` deleted first, so the first `meaning` build embeds every chunk |
| runs | two sequences, `on`, `off`, `cold`, `cold-session`, `cold-nocache`, one session, `--repeat 1` (a cold prompt can only be taken once); load average 6 to 8 on 16 CPUs, shared with other agents: compare latencies within a sequence |

```bash
B=<the binary above>; F=<the fixture>
python3 scripts/bench-brief-gate.py --pixel "$B" --repo "$F" --split dev --daemon on --warm-meaning --out on.json
python3 scripts/bench-brief-gate.py --pixel "$B" --repo "$F" --split dev --daemon off  --compare on.json --out off.json
python3 scripts/bench-brief-gate.py --pixel "$B" --repo "$F" --split dev --daemon cold --compare on.json --out cold.json
python3 scripts/bench-brief-gate.py --pixel "$B" --repo "$F" --split dev --daemon cold --session-start --session-gap 3 --compare on.json --out cold-session.json
rm -rf "$F/.pixel/code-vectors"
python3 scripts/bench-brief-gate.py --pixel "$B" --repo "$F" --split dev --daemon cold --compare on.json --out cold-nocache.json
```

English dev rows (78; hit@8 over the 32 with `expected_files`; any brief).
Latency is sequence 1, with sequence 2 in parentheses; every other column was
the same in both:

| mode | P | R | F1 | FP rate | hit@8 | hit@8 when fired | latency p50 / p95 ms | briefs in process / daemon | in process before the daemon answered | gate identical to `on` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| on | 84.6% | 76.7% | 80.5% | 17.1% | 71.9% | 82.1% | 106 / 170 (103 / 159) | 0 / 106 | 0 | n/a |
| off | 84.6% | 76.7% | 80.5% | 17.1% | 53.1% | 60.7% | 166 / 199 (160 / 199) | 106 / 0 | 106 | 109 of 109 |
| cold | 84.6% | 76.7% | 80.5% | 17.1% | 71.9% | 82.1% | 109 / 177 (105 / 164) | 1 / 105 | 1 | 109 of 109 |
| cold, after a session start | 84.6% | 76.7% | 80.5% | 17.1% | 71.9% | 82.1% | 106 / 171 (108 / 174) | 0 / 106 | 0 | 109 of 109 |
| cold, no vector cache | 84.6% | 76.7% | 80.5% | 17.1% | 71.9% | 82.1% | 111 / 172 (111 / 179) | 1 / 105 | 1 | 109 of 109 |

"Gate identical" is asserted by `--compare` (printed as `GATE IDENTICAL … 109/109
prompts, same fired, gate, tier and score`, exit status 2 otherwise): every
prompt took the same `fired`, `gate`, `tier` and `score` in every mode, so the
daemon changes the files and never the decision. Latency is the wall time of
`pixel brief` over the English rows, one call per prompt; briefs counted are
those that had a route (a declined prompt has none).

The hook itself (`pixel run-hook task-event --provider claude --event <event>`
with the JSON payload on stdin, the fixture's daemon stopped before each call,
15 calls per row, scratch script, wall time in ms):

| hook | auto-start off, no daemon | auto-start on, no daemon (starts one) | auto-start on, daemon running |
| --- | ---: | ---: | ---: |
| `SessionStart`, median / max | 22 / 29 | 24 / 29 | 24 / 30 |
| `UserPromptSubmit`, median / max | 186 / 201 | 191 / 204 | 185 / 208 |

What it says:

1. **A cold start costs one local brief.** In both sequences the first
   prompt that was briefed ran in process, started a daemon, and the next
   brief, 0.18 to 0.20 s later, found it. After a `SessionStart` none ran in
   process. hit@8 is 71.9% in every cold mode, against 53.1% in process
   throughout (`off`), and equal to a daemon that had been up all along.
2. **The `meaning` vectors come back fast when their cache is on disk.**
   Every cold brief text equals the `on` brief text, byte for byte
   (`stdout_sha`, 0 of 106 differ), because the daemon asked its first
   `meaning` question at start and read the vectors from `.pixel/code-vectors/`
   before the third prompt (0.4 s). Without that cache (a repository the
   daemon has never served) two briefs, `bg-006` and `bg-018`, went out without
   leads (their file order differs); both still named the expected file, so
   hit@8 did not move. A larger repository builds slower than this fixture's
   1 161 files, and the briefs that arrive meanwhile are the in-process ones.
3. **The hook pays 2 to 5 ms for it.** The decision is a few local socket
   probes on a thread of its own, bounded by 100 ms, and the start itself is a
   detached process; the differences above are inside the run-to-run spread
   (the max column), not a measured cost.
4. **Not measured.** A real stale-protocol daemon (an older binary running
   against this one) is covered by unit tests with a scripted daemon
   (retired, then launched), not by a run; Linux; a repository whose daemon
   takes longer than 0.2 s to bind; the live agent A/B. The numbers are the
   dev split, the set the gate was fitted on: the gate is unchanged by this
   work (asserted above), the files are what moved.

## Excavation: the agent-side metric

*Excavation* is the native exploration an agent does to find the code a prompt
is about, before it edits or answers. `scripts/excavation-count.py` reads the
agent's own transcript (Claude Code JSONL or pi session JSONL), takes one
prompt (the first by default) and walks the tool calls that follow it, reading
each `tool_use` / `toolCall` input. Transcript prose is never pattern-matched; the one
text test is for the brief's tag inside a hook attachment (below).

| Counted | What |
| --- | --- |
| `native` | Grep, Glob, Read, pi's `read`/`grep`/`find`/`ls`, and Bash calls whose command starts with a search or read (`grep`, `rg`, `ag`, `ack`, `find`, `fd`, `cat`, `bat`, `head`, `tail`, `ls`, `tree`, `nl`, `sed -n`, `git grep`, `git ls-files`), through `rtk`, `cd x &&`, env assignments, the wrappers that run another command with their options skipped (`timeout -s KILL 5`, `env -i`, `nice -n 10`, `time -p`, `sudo -u bob`, `stdbuf -oL`, `xargs -n1`, `command -p`; `command -v` only looks a command up), `sh -c` / `bash -lc`, pipelines (only a pipeline's first command counts: `cargo test \| grep FAIL` filters, it does not search) |
| `pixel` | calls to the pixel CLI: its own retrieval, reported apart so a run that swaps `rg` for `pixel search-content` does not look like less work |
| `delegated` | subagent spawns (`Agent`, `Task`, pi's `subagent`); their exploration is in their own transcripts |

One Bash call is one call, bucketed in the order edit, native, pixel. The
headline count stops at whichever comes first of the first **edit** (Edit,
Write, MultiEdit, NotebookEdit, or a Bash command that writes a file: `sed -i`,
`tee`, `>`, `git apply`) and the **answer** (the assistant's closing text of the
turn, the one no tool call follows). The counts at the first assistant text of
any kind, the first edit, the answer and the end of the turn are in the JSON
too (`at.*`), because narration between tool calls makes "before the first text"
a strict reading nobody wants as the headline.

### A/B protocol (the live run comes after the part that changes the brief)

One question: with the same agent, repository and prompt, does having the
brief reduce `native` before the first edit or answer?

| | `off` | `on` |
| --- | --- | --- |
| environment | `PIXEL_BRIEF=0` in the environment of the agent process (the hook and `pixel brief` inherit it; `feature_enabled` reads it first) | `PIXEL_BRIEF` unset |
| everything else | the same | the same |

1. **Prompts**: the `split: "test"` rows with `expected_files` (relevance is
   known), plus the off-topic `test` rows as a cost check (a brief on a prompt
   it should not touch must not add exploration or tokens). Tune on `dev`, read
   `test` once.
2. **Repository**: a fresh detached worktree of the fixture per run, indexed
   with `pixel prepare-repo`, the same installed hooks in both arms (only the
   environment differs), the daemon in the same state.
3. **Agent**: one harness, model and effort per comparison (Claude Code and pi
   are the two the counter reads), headless, the prompt text exactly as in the
   set, a turn limit that is the same in both arms.
4. **Order and repeats**: at least three runs per prompt and arm, arms
   interleaved (off, on, on, off, ...) so a rate limit or a model change falls
   on both. Keep every transcript.
5. **Validity**: an `on` run counts only if its transcript carries the injected
   brief and an `off` run only if it does not. The counter reads that from the
   transcript's structure (a Claude Code `hook_additional_context` attachment,
   a pi `pixel-brief` custom message) and `--require on=brief --require
   off=no-brief` drops the runs that disagree; it reports how many it dropped.
   A run that ends in an error or a refusal is dropped from both arms with its
   pair.
6. **Count**:
   ```bash
   python3 scripts/excavation-count.py --require on=brief --require off=no-brief \
     --arm off='runs/off/*.jsonl' --arm on='runs/on/*.jsonl' --json excavation.json
   ```
   The report gives per arm the mean, median and p90 of `native`, the means of
   `pixel` and `delegated`, and the difference of the arms paired by prompt
   (how many prompts the brief made lighter, equal, heavier).
7. **Read**: the paired difference of `native`, the share of prompts made
   heavier (the cost check), and `pixel` and `delegated` beside it. A drop in
   `native` bought with more `pixel` or `delegated` calls is a move, not a
   saving. Report n, the commands, the binary, the fixture SHA and the set's
   SHA-256 beside every number (`.agents/rules/measuring.md`).

The counter was exercised on the local transcript stores (hundreds of Claude
Code and pi sessions) to check it parses real files; no agent was run for this
document.

## Limits of this baseline

- **218 prompts, one labeller.** A row moves a percentage by about half a
  point overall and by two or three on `fr` or a `source`; the `dev` and `test`
  tables differ by more than that (hit@8 20.9% against 31.0%), so
  read `all` for the level and the two splits for the spread.
- **The labels are one reader's.** `on_topic` follows the rule in
  `eval/brief-gate/README.md`; a prompt a maintainer would call on-topic and the
  set calls `ops` (or the other way) moves the gate numbers, not the files.
- **`expected_files` is a short list.** A brief that names another file that
  would also have answered the prompt scores a miss; hit@8 is a floor on
  usefulness, not a measure of it.
- **The fixture indexes more than `crates/`.** 83 of the
  303 `files:` entries the briefs returned are outside `crates/`
  (`eval/`, `.agents/`, `scripts/`, `docs/`, changelog fragments): that is how
  the tool behaves on this repository, and a reason these numbers do not
  transfer to a repository of another shape.
- **A warm intent judge changes the weak-signal path.** Not measured here
  (see the run identity).
