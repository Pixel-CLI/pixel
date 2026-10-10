# Brief gate prompt set

A labelled set of prompts for measuring the prompt-submit brief
(`pixel brief`, the command `pixel run-hook task-event --event prompt-submit`
goes through): does it fire when it should, stay quiet when it should, and
name the files the prompt needs. It belongs to
[issue #883](https://github.com/Pixel-CLI/pixel/issues/883); the baseline
numbers and the way to reproduce them are in
[`docs/bench/brief-gate.md`](../../docs/bench/brief-gate.md).

```bash
python3 scripts/bench-brief-gate.py --check-set                       # schema, balance, privacy lint
python3 scripts/bench-brief-gate.py --check-set --repo <fixture>      # + every expected file exists there
python3 scripts/bench-brief-gate.py --pixel <pixel> --repo <fixture> --split dev
```

## Files

| File | What |
| --- | --- |
| `prompts.jsonl` | the set, one JSON object per line |
| `../../scripts/bench-brief-gate.py` | the runner, the metrics and `--check-set` / `--self-test` |

## Schema

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | string | `bg-NNN`, stable once published: never renumber, never reuse |
| `text` | string | the prompt, verbatim as the harness would submit it |
| `lang` | `en` \| `fr` | language of the prompt |
| `on_topic` | bool | whether the brief should fire (rule below) |
| `source` | string | where the row came from (table below) |
| `kind` | string | `plain`, `identifier` when on-topic; `ops`, `chat`, `paste`, `generic-code`, `other-repo`, `meta` when not |
| `split` | `dev` \| `test` | `dev` is for tuning, `test` is for the final read |
| `expected_files` | string[] (optional) | repo-relative paths a good brief names; on-topic rows only |
| `evidence` | string (optional) | why those files: `file:line` and what it shows; present with `expected_files` |

## Contents

| | rows |
| --- | ---: |
| total | 218 |
| on-topic / off-topic | 122 / 96 |
| English / French | 154 / 64 |
| dev / test | 109 / 109 |
| with `expected_files` | 85 (63 English, 22 French; 43 dev, 42 test) |

| `source` | rows | What |
| --- | ---: | --- |
| `synthetic` | 140 | written for this set, in the voice of the real prompts |
| `intent-eval` | 37 | verbatim from `eval/intent/task-intent-100.jsonl`: all `ops` and `none` rows (off-topic seeds), a few generic bugfix and investigate rows about a codebase that is not this one, and four questions about pixel itself |
| `session-context` | 16 | verbatim from `.pixel/tasks/session-context/*.json`, the first 4096 characters of real prompts the task hooks stored |
| `real-paraphrased` | 13 | a real prompt from the transcript corpus (`pixel recall`), reworded so nothing identifying survives |
| `recall` | 12 | verbatim from `pixel recall search --role user --human-only`, short, about pixel or plain git/IDE chatter |

## What a row says

**`on_topic`** is true when answering the prompt means reading or changing
code, configuration, tests or docs of *this repository*, so that a block of
evidence about it (files, definitions, callers) can save the agent a search.
It is false for everything else:

| `kind` | Off-topic because | Example |
| --- | --- | --- |
| `ops` | git, release, CI, PR or machine operation: no code to find | `go to branch main and pull`, `release 0.6.2` |
| `chat` | greetings, thanks, small talk, requests with no code in them | `thanks that worked`, `what's the weather going to be like tomorrow` |
| `paste` | a pasted thread with "thoughts?": the pasted block is someone else's text (the gate already ignores `<pasted_content>` blocks, so the untagged ones are the test) | a Slack thread about a red job |
| `generic-code` | a programming question that needs no file of this repository | `how do I center a div` |
| `other-repo` | a bug or question about some other application's code | `the login endpoint returns 500 since yesterday, find and fix it` |
| `meta` | about the session itself: continue, summarise, clipboard | `ok go ahead and continue` |

On-topic rows are `identifier` when the typed text carries a code-shaped token
(`snake_case`, `CamelCase`, `a::b`, a path or a source file) and `plain`
otherwise: plain-language questions and tasks about this repository, the case a
lexical gate cannot see.

Borderline prompts were left out rather than guessed: a fix request with no
anchor (`fix the flaky test`), a request that needs `gh` or `git log` to be
answered, a bare command to run. A prompt that asks for a pull request by URL
(`fix https://github.com/.../pull/876`) is `ops`: the work starts on GitHub,
not in the index.

**`expected_files`** lists the files whose reading answers the prompt (one to
three, the primary ones, not everything related). Each was found by reading the
code at the fixture SHA, not by running the brief, and `evidence` says where
(`daemon.rs:29 IDLE_TIMEOUT = 30 minutes`). A row without the field is on-topic
but its answer is not a small set of files (a vague bug report, a design
question, a task that touches the whole install). The paths are relative to the
fixture checkout and may move in later commits: `--check-set --repo <fixture>`
is what re-verifies them, never the live tree.

## Fixture

Labels were read against `Pixel-CLI/pixel` at
`85bede7d9a3c4e385e0a2045241a6466efbd8cbd` (`FIXTURE_SHA` in the runner). Run
the set against a detached checkout of that commit, indexed with
`pixel prepare-repo`, so the repository under test does not move between a
before and an after. This directory is not in that tree: a checkout of a later
commit would index the set and let a prompt find its own text, and the runner
says so when `eval/brief-gate/` exists in the repository under test.

## Splits

`split` is stratified by `on_topic`, `lang`, `kind` and whether the row has
`expected_files`, then dealt alternately by the hash of the text, so each
stratum is within one row of even (`--check-set` enforces it for `on_topic` ×
`lang`). Tune on `dev`. Read `test` once per candidate, after the choice is made:
a threshold chosen to move `test` is a threshold fitted to the answer key.

## Privacy

The repository is public. Real prompts can name other projects, clients,
people, hosts, paths and credentials, so a row is either a prompt about pixel
or something generic, and anything else was reworded into a `real-paraphrased`
or `synthetic` row. `--check-set` refuses an email address, a home path, an
IPv4 address, something shaped like a credential or a long token, and a URL
that is not this project's. Names in the pasted-thread rows are invented.
Adding a row: write it, then run `--check-set` and read the row once more as a
stranger would.

## Adding rows

Keep the strata balanced (add rows in pairs, one for each split), give the row a
fresh `id`, and for `expected_files` read the code first and write the evidence.
A new row changes the set's SHA-256, which the runner records: a before and an
after must use the same file.
