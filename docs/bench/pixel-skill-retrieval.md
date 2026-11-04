# Pixel skill retrieval: missing test evidence

Run date: 2026-11-04. This is a skill-body-only ablation. Both arms received the same frozen Pixel brief and the same request, and both explicitly invoked `Skill(pixel)`. It measures retrieval after the skill is invoked; it does not measure automatic skill adoption or an integrated brief-plus-skill change.

## Change evaluated

The candidate corrects the brief rule to answer from it only when it contains enough evidence. It treats test and symbol names as navigation and asks for relevant assertions when the brief gives a name without those assertions. It leaves direct literal and known-path reads available.

The candidate full skill was 4,575 bytes (SHA-256 `297abf046325b5d5297cdcdda4bf8c5be9604040e7da35b3707b84a0e7d404f3`); the baseline full skill was 4,211 bytes (`7381ab0a4b3f51687aa850dbd87ec3e6d567febe775f6c23a231b08bb3f3e597`). The corresponding source prompt asset is [pixel-agent-prompt.md](../../crates/pixel-setup/assets/pixel-agent-prompt.md). The generated full skill is pinned in [skill.md](../../tests/setup/.agents/skill.md).

## Protocol

- Frozen repository: Requests commit `611c6162cbc4ac2020a2f91c7cfa4f3abf9bbb60`, cloned to an isolated scratch path. Its index was prepared without a daemon by `/Users/livio/.local/bin/pixel`, version 0.10.0, commit `df4156e2bdf6ab966043a70c5161034c20de7795`, SHA-256 `9775764ec02282c4656e6a438c1389930db19058a320e4aa1169d219d1e5fab6`.
- Old brief: concatenated baseline hook context, 11,856 bytes, SHA-256 `e7b03ef783f769d3c38d7b4a708f1e3274cb3d2647e086558a5cc5b0d0987872`; byte-identical in every arm. The captured hook receipt identifies its producer as `/tmp/pixel-current-main`, version 0.10.0, commit `9dd036279766e1454a135543e3c1566f9fbc97ea`, SHA-256 `386528a32cd9007eb2738204c624f1d48f6d62e6af9f1571726466ea0217b00e`.
- User request: “Before answering, invoke the Skill tool for `pixel` and follow its instructions. Where are the tests for redirect handling and what scenarios do they cover? Read only; no network or edits.” Combined brief-plus-question prompt SHA-256 `b9e4fb3c237df4afd3addaec0639b42913d6790bdcb9cc2bf30ac27ae1c25614`.
- Claude Code 2.1.318, Sonnet 5.5, medium effort, plan mode, 20-turn cap, $1 session cap. Both arms used identical `--allowedTools` values: `Read`, `Grep`, `Glob`, `Bash(pixel *)`, and `Bash(rg *)`. `Edit`, `Write`, `NotebookEdit`, `WebFetch`, `WebSearch`, and `Agent` were explicitly disallowed. The captured source reads include native `Read` and a `Bash` invocation of `sed`. `Skill(pixel)` was called once per run; the receipt verified the matching skill-body marker.
- Exact run command: `rtk python /tmp/pixel-skill-body-20261104/run_requests_v2final3.py`; runner SHA-256 `c5e03d54078563633e4417639c82c9bebb0d4b01d27c06a1810b4b8ed05bf0c4`. The pair runner `run_requests_v2finalpair.py` has SHA-256 `af85c7e334d10d372d244c30478a7f65261699405686461fda7dd83aad705510`. Six private mode-0600 stream-json receipts and answer files remain under `/tmp/pixel-skill-body-20261104/results/requests-v2-final-*`.

## Results

The source oracle is `tests/test_requests.py:262-275`: the named `_WITH_PARAMS` test configures a session `max_redirects = 5`, sends a chain of 50 redirects, and expects the request and response URLs to stop at redirect 45 with a history length of 5. `src/requests/sessions.py:216-218` raises when the response history reaches the configured limit.

| Pair | Baseline calls / source reads | Candidate calls / source reads | Token proxy | Cost | Elapsed |
|---|---:|---:|---:|---:|---:|
| 1 | 1 / 0 | 3 / 2 | 55,648 → 100,939 | $0.10358 → $0.14710 | 13.4 → 21.2 s |
| 2 | 1 / 0 | 2 / 1 | 57,293 → 98,177 | $0.10922 → $0.13327 | 12.4 → 26.9 s |
| 3 | 1 / 0 | 2 / 1 | 62,075 → 94,813 | $0.12023 → $0.13458 | 14.3 → 13.4 s |
| **Total** | **3 / 0** | **7 / 4** | **175,016 → 293,929** | **$0.33302 → $0.41495** | **40.1 → 61.6 s** |

Token proxy is the sum of input, cache-read, cache-creation, and output tokens from the final Claude result. It is not a billed-token total. Candidate totals were 67.9% higher by this proxy, 24.6% higher in reported cost, and 53.7% slower. Each candidate invoked the Pixel skill; it used native `Read`/`sed` evidence, not a `pixel` CLI command. The candidate made one or two targeted source reads per run; the baseline relied on the brief without reading source.

Independent source grading found a narrow quality gain: baseline answers in pairs 1 and 2 were vague about the custom limit, and pair 3 incorrectly described `_WITH_PARAMS` as query-parameter coverage. All three candidate answers explained the custom redirect limit from the test source. This does not establish overall answer correctness. Candidate pair 2 incorrectly attributes a redirect path to `test_prepared_from_session`, which tests request preparation/auth behavior. Candidate pair 3 omits some redirect scenarios and overstates `pytest -k redirect` as complete for redirect coverage; method-rewrite tests at lines 277–313 do not contain “redirect” in their names.

## Dropped trials

- A semantic-first/body-follow-through variant on a different preservation task increased skill use but raised total calls, token proxy, cost, and elapsed time; it was dropped.
- An earlier Requests wording variant read the relevant test in two of three runs and skipped it in the third, so it was not used for this result.
- The unknown-Rust-receiver task did not improve active-path accuracy. Both arms missed that the default Rust path is MVP and did not trace persistence; those runs are excluded from the quality claim.
