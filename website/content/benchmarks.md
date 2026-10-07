---
title: "Benchmarks"
description: "Every number on the home page, with its method, its sample size and the cases where Pixel loses."
---

<!-- Figures come from docs/bench/; every section links its source. crates/pixel/tests/cli/docs_drift.rs reads this page, so every `pixel <command>` quoted here must exist. -->

Figures below link to their available evidence and protocols. The Opus trial's raw runs are not archived; its known setup and limits are stated alongside the result. Losses sit next to wins.

<!-- Each command in this box was run on a fresh clone of a third-party repository (psf/requests) before it was written here: `pixel audit` and `pixel list-signatures` at requests 611c616 with pixel 0.5.1, the others with pixel 0.5.0. Re-run them when an output or a prerequisite changes. -->
<aside class="measure" aria-labelledby="measure-it-on-your-own-code">

## Measure it on your own code

Four checks, quickest first. Each needs `pixel` on your PATH ([install](../docs/#install)); the first three run inside a Git repository of yours, where the first Pixel command builds its index in `.pixel/` and adds that folder to `.gitignore`.

**Your largest files, now.** `pixel audit` compares each of the twenty largest source files the index holds with its outline, one row per file, then prints the total, the median file and the coverage per language, counted as below (bytes divided by four, rounded down):

```bash
pixel audit
```

On Requests (commit `611c616`), a first run on a fresh clone built the graph and read 95,693 tokens for the twenty files whole against 11,447 for their outlines (−88%): 89% saved on the median file, from 61% to 98%. It left `docs/conf.py` out, a Sphinx configuration with no function or class to outline, rather than count an empty outline as a saving. `--top N` measures more files and `--json` prints every count; nothing leaves your machine.

**One large file, the same count.** `pixel list-signatures` prints the file's outline, then a report on stderr that compares it with reading the whole file, counted as below (bytes divided by four, rounded down):

```bash
pixel list-signatures path/to/a/large/file.py
```

On Requests' `src/requests/models.py` (1,184 lines) the report's last row read `full read 10365 tok, pixel answer 641 tok (-94%)`, in well under a second: the same two counts as that file's row in `pixel audit`. The two counts are `wc -c` of the file and of the outline, divided by four; the report itself is left out of them.

**Your agent's local operations, after a few days.** `pixel token-savings` reads `.pixel/actions.jsonl`: measured output volumes and versioned token/workflow estimates, each labelled. It does not observe all agent reads, provider usage, billed cost or end-to-end agent time:

```bash
pixel token-savings
```

**Our table, on your machine.** `scripts/bench-read-savings.sh` re-measures the well-known files below. It needs a clone of Pixel's repository for the script, plus `curl` and the network: it downloads each file at its pinned commit into a throwaway folder, never into your code, and takes a few seconds.

```bash
git clone --depth 1 https://github.com/Pixel-CLI/pixel.git && cd pixel
scripts/bench-read-savings.sh
```

To turn the rate into a monthly figure for your team, the [savings estimate](../savings/) multiplies it by your own numbers.

</aside>

## Reading code

Historical read-volume comparison on Pixel's own repository (138K lines of Rust), first published at commit `632b3685` (exact binary version not recorded there). Counts are estimated tokens: UTF-8 bytes divided by four, not tokenizer or provider usage. No second model reads the files instead.

| Scenario | Lines | Full reads | With Pixel | Saved |
| --- | --- | --- | --- | --- |
| Single large file | 4,661 | 48,465 tok | 2,172 tok | 95.5% |
| Multi-file cross-read | 7,106 | 68,934 tok | 3,768 tok | 94.5% |
| Source and test pair | 5,289 | 48,978 tok | 1,890 tok | 96.1% |
| Code-write context | 5,289 | 48,978 tok | 1,910 tok | 96.1% |

These are file/answer volumes, not an invoice or an end-to-end task comparison. The historical maintainer log reported 41 to 83% across 798 operations; its snippet-versus-candidate-pool ratio is a different baseline, not a measured reduction in session tokens or cost. The scenarios replay the shape of [shunt](https://github.com/spotify/portal-ai-plugins/tree/main/plugins/shunt)'s benchmark, which reaches a similar ratio by rerouting reads through a paid second model. [Method and figures, as first published](https://github.com/Pixel-CLI/pixel/blob/632b3685a97e941476cb42aa75333b79f0ed8955/README.md#-token-savings--measured-no-second-model)

### Well-known files

The same measurement on large files of popular projects, each pinned to a commit: the whole file against `pixel list-signatures` on it, with pixel 0.5.0 in September 2026. The home page's token wall shows the Transformers row: read whole, that one file takes {{% read-savings "window" %}}, before the agent has written anything. The signatures output is {{% read-savings "summary" %}} in estimated tokens (UTF-8 bytes ÷ 4, rounded down); the files with the most signatures per line (VS Code's text model, CPython's `typing.py`) save the least.

{{% read-savings %}}

React's work loop is left out of the range: it is written in Flow, and the JavaScript grammar lists 20 of its 125 top-level functions, so its 99.4% measures a parse failure, not a saving. The Signatures column is the check for that on every row: signature counts provide a coarse coverage check, not proof of complete parsing; the Python counts are compared with module- and class-level definitions (Transformers: 90 for 89 `def` and `class` lines). Re-run it with `scripts/bench-read-savings.sh`. [Method and raw output](https://github.com/Pixel-CLI/pixel/blob/main/docs/bench/read-savings.md)

## Cold start on a large repository

The first Pixel command on a repository builds everything from nothing: the text index from the committed files, then the code graph. On a private Rails application of 18,782 tracked files, from a fresh clone with an empty cache, Apple M2, September 2026, median of three runs:

| Cold, from nothing | Time |
| --- | --- |
| Text index | **2.0 s** |
| Text index and code graph (59,609 symbols) | **12 s** |

Later commands reuse the index and read only what changed. One repository on one machine: re-run the command on yours. [Method and raw runs](https://github.com/Pixel-CLI/pixel/blob/main/docs/bench/cold-index.md)

## Against GitNexus

The jobs both tools do: 29 blast-radius cases on four repositories in Rust, TypeScript and Ruby, with callers found by grep as the ground truth. GitNexus 1.6.12 and Pixel 0.4.0, same machine, September 2026.

| | Pixel | GitNexus |
| --- | --- | --- |
| Callers found (recall, 29 cases) | 0.86 | 0.84 |
| Median time per answer | **153 ms** | 432 ms |
| Mean answer size | **4.5 KB** | 11.0 KB |
| Context cost on every turn | **~4,160 estimated tokens** | ~19,700 tokens |
| Cold index of Pixel's repository | **9.3 s, 8.6 MB** | 28.7 s, 184 MB |
| Git history and Git operations | **Yes** | No |
| Cypher queries, taint analysis, API route maps | No | **Yes** |
| Callers in Ruby (two repositories) | 0.90 and 0.56 | **1.00 and 0.68** |
| Licence | **MIT** | PolyForm Noncommercial |

Recall is a tie at this sample size: Pixel finds every caller in Rust and TypeScript, GitNexus does better on Ruby. The index comparison covers one repository only. [Full method and raw rows](https://github.com/Pixel-CLI/pixel/blob/main/docs/bench/vs-gitnexus.md)

## On whole agent tasks

An Opus 5.5 medium-effort trial compared 11 runs without Pixel with 11 runs
using Pixel install hooks on the same low-research scoping task:

| Metric | Without Pixel | With Pixel hooks | Change |
| --- | ---: | ---: | ---: |
| Duration | 42.9 s | 47.6 s | +11% |
| Tokens read | 11,945 | 13,350 | +12% |
| Provider-reported API cost | $0.283 | $0.372 | +31% |
| Pixel calls per run | 0 | 0–4 observed | — |

No speed gain on this task; the hooks arm was slower and had more read volume
and higher provider-reported cost. Pixel calls ranged from zero to four per
hooked run; that range is not a median. This single low-research task does not
establish a general slowdown, speedup, token or cost result. The raw runs are
held outside the repository and are not archived, so their date, exact model
and Pixel versions, sample size and cost, the per-run distributions, the
exact CLI/harness versions beyond the `v0.5.0` worktree tag, and the
token-counting method cannot be independently checked. The reported API cost
is not invoice-verified, and the token count is not necessarily billed-token
usage. [Known setup and provenance limits](https://github.com/Pixel-CLI/pixel/blob/main/docs/bench/opus-install-hook-trial.md) and the [available protocol note](https://github.com/Pixel-CLI/pixel/blob/main/docs/motion/README.md#the-agent-demo)

Read volume, estimated tokens, elapsed duration and provider-reported cost are different quantities. Adoption rates with the current install hooks are not quantified here; inspect `pixel action-log` on your own sessions.

## Where a specialist wins

- **Natural-language search, the top 10:** on 45 plain-English queries, semble has the right file in its top 10 for 100%, Pixel 0.6.0 for 96%. Pixel puts it first more often: 87%, against 64% for semble and 69% for WarpGrep, Morph's search subagent, whose top 10 reaches 71%. Average answer time 0.7 s for Pixel, 1.6 s for semble, 6.8 s for WarpGrep. The queries are each repository's own doc comments, which favours Pixel's chunks, cut along symbols with their comments. WarpGrep's figures are from its one run on the same queries, with Pixel 0.5.2; it is paid per search and sends the lines it reads to Morph's API. [Method and every row](https://github.com/Pixel-CLI/pixel/blob/main/docs/bench/vs-landscape.md#natural-language-retrieval--pixel-060-four-arms-45-queries)
- **Compact repository map:** stacklit covers more directories for fewer tokens on 3 of 4 repositories.
- **Context cost:** Pixel is 4.7× lighter than GitNexus, but heavier than semble (~980 tokens) and stacklit (~420).

They combine: semble for search and Pixel for the graph, history and Git costs about 5,100 always-on tokens. [Full comparison](https://github.com/Pixel-CLI/pixel/blob/main/docs/comparison.md)

## Coding decisions

`pixel classify` answers a bounded question with a decision model you configure. It returns one probability per label. The local Ollaya engine uses decision-model outputs; the remote engine asks a chat model to generate probabilities.

### Published decision-model measurements

Source: [Ollaya’s published benchmark](https://ollaya.dev/), checked 2026-09-28. Ollaya describes the accuracy evaluation as the typed-decisions test split: 400 states and 2,000 questions, scored by argmax against the majority label. It reports its own model results and attributes Jev’s result to Winnow’s benchmark on the same questions. These are upstream reports, not a Pixel reproduction; no shared run artifact is archived here.

| Model | Typed-decisions accuracy | Latency | Measurement |
| --- | --- | --- | --- |
| winnow:e4b on Ollaya | 0.722 | **89 ms** | RTX 4090, five questions end to end |
| TypeSafe Jev | **0.738** | 236–276 ms | Hosted API, median request |
| laya | — | about 10 ms | Runs well on a CPU |

The latency figures use different environments and requests: local RTX 4090 inference versus a hosted API including network time. They do not establish a speed winner. The accuracy sample is separate from the five-question latency request. The laya latency is also an upstream measurement, not a CPU measurement by Pixel.
