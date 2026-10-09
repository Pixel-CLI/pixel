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

## Excavation: the agent-side metric

*Excavation* is the native exploration an agent does to find the code a prompt
is about, before it edits or answers. `scripts/excavation-count.py` reads the
agent's own transcript (Claude Code JSONL or pi session JSONL), takes one
prompt (the first by default) and walks the tool calls that follow it, reading
each `tool_use` / `toolCall` input. Transcript prose is never pattern-matched; the one
text test is for the brief's tag inside a hook attachment (below).

| Counted | What |
| --- | --- |
| `native` | Grep, Glob, Read, pi's `read`/`grep`/`find`/`ls`, and Bash calls whose command starts with a search or read (`grep`, `rg`, `ag`, `ack`, `find`, `fd`, `cat`, `bat`, `head`, `tail`, `ls`, `tree`, `nl`, `sed -n`, `git grep`, `git ls-files`), through `rtk`, `cd x &&`, env assignments, `time`, `sh -c`, pipelines (only a pipeline's first command counts: `cargo test \| grep FAIL` filters, it does not search) |
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
