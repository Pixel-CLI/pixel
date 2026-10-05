# Codex native-default experiment (Task 700)

The target is a routing contract: ordinary repository questions should receive
no Pixel retrieval instructions or forced Pixel command. Explicit structural
questions may receive one small, optional graph hint. Classification is local;
it makes no model request and does not build an index. This removes a systematic
source of extra model work, but cannot guarantee identical answers, tokens or
latency across stochastic model runs. Hook process startup still has a cost.

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

## Installed validation

The final rebuild and self-update exited 0. Global installation reported 13
green steps; absolute-path repository installation reported 8. Configuration
generation and the history index rebuild exited 0. Final
`pixel doctor . --fix --fail-on yellow --json` exited 0 with 32 green checks,
0 yellow and 0 red; it repaired facts freshness and left no unresolved repair.
The complete local receipts are under `target/native-default-evidence/`.

Focused checks passed: 474 install tests, 122 Codex guard/hook tests, and
formatting. These are separate from the full workspace and CI gates.
