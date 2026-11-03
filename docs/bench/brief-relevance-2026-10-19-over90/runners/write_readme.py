#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Render archived native runs and source reviews without smoothing failures."""
import argparse
import gzip
import hashlib
import json
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('archive')
args = parser.parse_args()
root = Path(args.archive)


def data(name):
    return json.loads((root / name).read_text())


def ledger(name):
    path = root / (name + '-review.jsonl')
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def relevance(rows):
    return sum(row.get('relevant') is True for row in rows), len(rows)


def pct(count, total):
    return f'{100 * count / total:.1f}%' if total else 'n/a'


def run_status(report):
    result = report.get('result', {})
    counts = result.get('counts', {})
    return (result.get('status', 'unknown'), counts.get('ok', 0),
            counts.get('partial', 0), counts.get('malformed', 0), counts.get('abstained', 0))


def run_state(report):
    status, ok, partial, malformed, abstained = run_status(report)
    parts = [f'{ok} accepted responses']
    if partial: parts.append(f'{partial} partial')
    if malformed: parts.append(f'{malformed} malformed')
    if abstained: parts.append(f'{abstained} abstained')
    return f'{status}: ' + ', '.join(parts)


def metric(report, key):
    value = report.get('means', {}).get('answer', {}).get('latency', {}).get(key)
    return f'{value} ms' if value is not None else 'n/a'


def brief_hashes(name):
    path = root / (name + '-briefs.jsonl.gz')
    if not path.exists():
        return {}
    with gzip.open(path, 'rt') as stream:
        rows = [json.loads(line) for line in stream if line.strip()]
    result = {}
    for row in rows:
        brief = row.get('brief', '')
        result[row['id']] = row.get('sha256') or hashlib.sha256(brief.encode()).hexdigest()
    return result


def repeat_identity():
    baseline, candidate, repeat = (brief_hashes(name) for name in ('baseline', 'candidate', 'repeat'))
    shared = baseline.keys() & candidate.keys()
    changed = {key for key in shared if baseline[key] != candidate[key]}
    overlap = candidate.keys() & repeat.keys()
    same = sum(candidate[key] == repeat[key] for key in overlap)
    differs = sum(candidate[key] != repeat[key] for key in overlap)
    return (len(changed), len(changed & repeat.keys()), len(changed - repeat.keys()),
            len(repeat.keys() - changed), same, differs)


def probe_failures(report):
    failures = []
    for session in report.get('sessions', []):
        for probe in (session.get('brief_meta') or {}).get('probes') or []:
            outcome = probe.get('outcome', 'unknown')
            if outcome not in {'ok', 'empty', 'skipped'}:
                failures.append((session.get('id', '?'), probe.get('name', '?'),
                                 outcome, probe.get('ms', 'n/a')))
    return failures


def probe_failure_rows(runs):
    rows = []
    for name, report in runs:
        failures = probe_failures(report)
        if not failures:
            rows.append(f'| {name} | — | — | none recorded | — |')
        rows.extend(f'| {name} | `{question_id}` | `{probe_name}` | `{outcome}` | {elapsed} ms |'
                    for question_id, probe_name, outcome, elapsed in failures)
    return '\n'.join(rows)


base_rows, candidate_rows, supplemental_rows = (
    ledger(name) for name in ('baseline', 'candidate', 'supplemental'))
repeat_rows = ledger('repeat')
baseline = data('baseline.json')
candidate = data('candidate.json')
supplemental = data('supplemental.json')
repeat = data('repeat.json')
identity = data('candidate-identity.json')
ci = data('ci-results.json')
repeat_selection = data('repeat-selection.json')

def source_review_summary(rows, report, expected_ids=None):
    if expected_ids is None:
        expected_ids = {row.get('id') for row in report.get('sessions', []) if row.get('id')}
    by_id = {row.get('id'): row for row in rows if row.get('id')}
    reviewed = sum(by_id.get(case_id, {}).get('relevant') in (True, False) for case_id in expected_ids)
    relevant = sum(by_id.get(case_id, {}).get('relevant') is True for case_id in expected_ids)
    return relevant, len(expected_ids), reviewed, max(0, len(expected_ids) - reviewed)


baseline_count, baseline_total, baseline_reviewed, baseline_pending = source_review_summary(base_rows, baseline)
candidate_count, candidate_total, candidate_reviewed, candidate_pending = source_review_summary(candidate_rows, candidate)
repeat_count, repeat_total, repeat_reviewed, repeat_pending = source_review_summary(repeat_rows, repeat)

first_base_rows, first_candidate_rows = ledger('earlier-baseline'), ledger('first-candidate')
first_base_count, first_base_total = relevance(first_base_rows)
first_candidate_count, first_candidate_total = relevance(first_candidate_rows)
earlier_baseline = data('earlier-baseline.json')
first_candidate = data('first-candidate.json')
supplemental_positive = [row for row in supplemental_rows if not row.get('expected_empty')]
supplemental_controls = [row for row in supplemental_rows if row.get('expected_empty')]
supplemental_count, supplemental_total, supplemental_reviewed, supplemental_pending = source_review_summary(
    supplemental_positive, supplemental,
    expected_ids={row.get('id') for row in supplemental_positive if row.get('id')})
control_passed = sum(row.get('passed_empty') is True for row in supplemental_controls)
supp_review_note = (
    f'{supplemental_count}/{supplemental_total} relevant among positive questions; '
    f'{supplemental_reviewed}/{supplemental_total} reviewed; {supplemental_pending} pending.'
)

old = {row['id']: row for row in base_rows}
gains = [row for row in candidate_rows
         if row.get('relevant') is True and old.get(row['id'], {}).get('relevant') is not True]
losses = [row for row in candidate_rows
          if row.get('relevant') is not True and old.get(row['id'], {}).get('relevant') is True]
gain_rows = '\n'.join(f'| `{row["id"]}` | {row.get("evidence", "")} |' for row in gains)
miss_rows = '\n'.join(f'| `{row["id"]}` | {row.get("reason", "Unresolved under the fixed criterion.")} |'
                       for row in candidate_rows if row.get('relevant') is False)
supp_misses = '\n'.join(f'| `{row["id"]}` | {row.get("reason", "Unresolved under the fixed criterion.")} |'
                         for row in supplemental_positive if row.get('relevant') is False)
pending_candidate = '\n'.join(f'| `{row["id"]}` | Source review pending. |'
                              for row in candidate_rows if row.get('relevant') is None)
pending_supplemental = '\n'.join(
    f'| `{row["id"]}` | Source review pending. |'
    for row in supplemental_positive if row.get('relevant') is None)
candidate_pending_section = (
    f'\nCandidate source reviews still pending:\n\n| Question | Status |\n| --- | --- |\n{pending_candidate}\n'
    if pending_candidate else '')
supplemental_pending_section = (
    f'\nSupplemental positive source reviews still pending:\n\n| Question | Status |\n| --- | --- |\n{pending_supplemental}\n'
    if pending_supplemental else '')
pr_rows = '\n'.join(f'| [#{item["pr"]["number"]}]({item["pr"]["url"]}) | `{item["pr"]["headRefOid"]}` |'
                    for item in ci)

expected_repeat, repeated_expected, expected_not_repeated, unexpected_repeat, same_repeat, changed_repeat = repeat_identity()
base_status = run_state(baseline)
candidate_status = run_state(candidate)
supp_status = run_state(supplemental)
repeat_status = run_state(repeat)
probe_rows = probe_failure_rows((('baseline', baseline), ('candidate', candidate),
                                 ('supplemental', supplemental), ('repeat', repeat)))
base_partial = run_status(baseline)[2]
candidate_partial = run_status(candidate)[2]
partial_note = (f'The baseline has {base_partial} partial native responses; the candidate has '
                f'{candidate_partial}. Per-question source judgments and native reasons are '
                'preserved in the ledgers and run reports.')

diagnostic_note = ''
diagnostic_path = root / 'diagnostics/final-candidate-bf180014/report.json'
if diagnostic_path.exists():
    diagnostic = json.loads(diagnostic_path.read_text())
    diagnostic_review_path = root / 'diagnostics/final-candidate-bf180014/final-candidate-bf180014-review.jsonl'
    diagnostic_rows = ([json.loads(line) for line in diagnostic_review_path.read_text().splitlines()]
                       if diagnostic_review_path.exists() else [])
    diag_count, diag_total = relevance(diagnostic_rows)
    diagnostic_note = (
        f'An intermediate `bf180014` candidate scored {diag_count}/{diag_total} on source relevance. '
        'This was a real implementation regression: CI #1262 rejected Rust-incompatible lookaheads in '
        'the flow-rank manifest, so the loader omitted that probe metadata. The diagnostic run and its '
        'failure remain archived; later measurements use the corrected manifest.'
    )

first_supp = ledger('first-supplemental')
first_supp_positive = [row for row in first_supp if not row.get('expected_empty')]
first_supp_count, first_supp_total = relevance(first_supp_positive)
repeat_selection_count = len(repeat_selection.get('ids', []))

readme = f'''# Brief relevance beyond 90%, 2026-10-19

The final matched public benchmark source review is {baseline_count}/{baseline_total}
({pct(baseline_count, baseline_total)}) for baseline and {candidate_count}/{candidate_total}
({pct(candidate_count, candidate_total)}) for candidate, with
{len(gains)} gains and {len(losses)} losses against that baseline. These are source-
relevance judgments, separate from native hook completion and probe execution.

Native run status is separate from source relevance. The final baseline status is
**{base_status}**; candidate status is **{candidate_status}**. {partial_note} A useful
saved partial brief remains a partial native delivery and does not turn a timed-out
probe into a successful run. Latency is reported as observed, not as a statistical
performance claim.

Inputs and frozen homes were matched, but host load was uncontrolled. Recorded load
windows were 49→92 for the r3 baseline, 9.17→55.78 for the final candidate, and
40.63→24.93 for the supplemental run. These are confounded observations; they do not
establish a causal speed change, and an individual timeout cannot be attributed to
host load from these measurements. Final runtime can differ.

An overall accepted native response does not mean every probe succeeded. Probe-level
failures and timeouts are reported separately for all four arms below; `empty` means
an ordinary abstention.

The final candidate returned nonempty briefs for all 56 public questions; the
supplemental run returned nonempty briefs for all {supplemental_total} project-positive
questions, while all {len(supplemental_controls)} controls returned zero bytes. These
delivery counts do not imply relevance. The first supplemental development set scored
{first_supp_count}/{first_supp_total} ({pct(first_supp_count, first_supp_total)}) on its
positive questions. It then informed implementation changes, so it is not held-out
evidence. The later supplemental run has {supp_review_note} Its native status is
**{supp_status}**. The fixed set contains {len(supplemental_controls)}
continuation and unrelated controls; {control_passed} emitted zero brief bytes. This is
not a production-readiness or general zero-call claim. {diagnostic_note}

Recount judgments, verify identities and check the complete archived evidence:

```sh
rtk bypass python3 docs/bench/brief-relevance-2026-10-19-over90/verify.py
```

| Final matched native-hook measurement | Baseline | Candidate |
| --- | ---: | ---: |
| Source-reviewed relevant briefs (reviewed; pending) | {baseline_count}/{baseline_total} ({baseline_reviewed}; {baseline_pending}) | {candidate_count}/{candidate_total} ({candidate_reviewed}; {candidate_pending}) |
| Native run status | {base_status} | {candidate_status} |
| p95 observed elapsed time | {metric(baseline, 'p95_ms')} | {metric(candidate, 'p95_ms')} |
| Maximum observed elapsed time | {metric(baseline, 'max_ms')} | {metric(candidate, 'max_ms')} |

Earlier diagnostic runs recorded {first_base_count}/{first_base_total} relevant at the
baseline snapshot and {first_candidate_count}/{first_candidate_total} at the first
candidate snapshot ({run_state(earlier_baseline)}; {run_state(first_candidate)}).
They are chronology, not the final matched comparison. The repeat intentionally covers
only changed public-question briefs. Of {expected_repeat}
candidate briefs whose hashes differ from baseline, {repeated_expected} were repeated
and {expected_not_repeated} were not; it contains {unexpected_repeat} IDs outside that
changed set. The repeat source review is {repeat_count}/{repeat_total} relevant,
{repeat_reviewed} reviewed, {repeat_pending} pending. The repeat status is
**{repeat_status}**. Among {same_repeat + changed_repeat}
IDs shared by candidate and repeat, {same_repeat} are byte-identical and {changed_repeat}
differ. This is a subset repeat, not a full56 rerun. Repetition does not repair a
partial or failed native run.

| Run | Question | Probe | Outcome | Probe time |
| --- | --- | --- | --- | ---: |
{probe_rows}

Exact oracle-line overlap is retained in scorer reports; it is separate from source
relevance and does not determine the manual judgment. Intermediate deadline failures
remain archived, including failures during severe host contention. No native deadline
was enlarged to improve a score.

## Fixed relevance criterion

A brief passes when it contains an exact substantive excerpt that answers a requested
fact or correctly locates the requested source: an architecture anchor, configuration
behavior, exact test, caller, error-producing path, flow boundary, or requested commit.
Lexical neighbors, wrong features and uncertain cases fail. Partial useful evidence can
pass source review; nonempty output alone cannot. Source review does not erase native
timeouts or partial status. Reviews are assistant source reviews, not blinded human
evaluation, and passing does not mean every excerpt is useful or that an agent can
finish without tools.

## Recovered public questions

| Question | Source evidence in the native brief |
| --- | --- |
{gain_rows}

## Remaining public misses and rationale

| Question | Why it remains unresolved |
| --- | --- |
{miss_rows}

{candidate_pending_section}

Requests imports and catches urllib3's `MaxRetryError`; its declared range is
`urllib3>=1.26,<3`. Express imports `finalhandler` with range `^2.1.0` and passes its
fallback callback to routing. The exact message formatters are absent from the pinned
project checkouts. Project source can prove the local handoff, but cannot identify the
exact implementation of a range-selected dependency. Both exact-origin questions
Requests-07 and Express-07 remain failures under the fixed relevance criterion. A version-resolved
dependency-source index would be needed to inspect those formatters; fetching an
arbitrary current dependency would not prove the pinned project's runtime behavior.
No runtime network lookup or LLM dependency was added.

## Supplemental questions and controls

The supplemental set contains 11 rewordings of original misses, seven fresh questions
(one per project), and ten controls repeated across seven projects. Its SHA-256 is
`ac1618f591d8010835f4c911176346a3cdf1ce60a6ae011b3b67fefc8a4620be`. The later results
are development-set measurements, not held-out evidence. Negative prompts include
continue, ok, yes, acknowledgments, weather, and unrelated general-knowledge or
creative prompts.

Remaining supplemental misses:

| Question | Rationale |
| --- | --- |
{supp_misses}

{supplemental_pending_section}

The first supplemental scorer exited 1 because 42 empty controls were labeled
malformed when the Pi hook response `{{"decision":"observe"}}` omitted `context`.
All saved brief outputs were already empty. The scorer fix recognizes that specific
observe-without-context shape as abstention while retaining other validation rules.
The original rejected report is retained, not rewritten.

## Frozen inputs and verification

- Native binary source: `1d1c063de058fb132d84623e67a199aaf3308904`.
- Binary SHA-256: `{candidate['binary']['sha256']}`.
- Baseline probe source: `412d302f42157532b10092f81c4c2ba8f52b7e63`.
- Final candidate probe source: `{identity['source_commit']}`.
- Both final arms use the same binary, question file, scorer, model and configuration.
  Candidate probes are overlaid in an isolated home; no Rust build is substituted.
- Separate roots were copied from one prepared seed and verified equal before use.
  Root receipts, command lines, source pins and probe hashes are archived.
- The optional intent LLM classifier is disabled in both homes. The local
  embedding/ranking model and configuration are matched; retrieval additions use
  local source only.
- Repeated outputs are compared by ID and brief SHA-256 above. Earlier relevance
  judgments are reused only for byte-identical output at the same source commit.

Eight PRs have archived current-head CI receipts. Verify every PR head and required
check before treating the candidate as ready; a receipt is not a merge authorization.
Focused unit suites cover config selectors, exact option tests, imported factory
bindings, same-receiver methods/getters, source masking, caller identity, flow
handoffs, and admission controls. Named-import recovery is supported; default imports,
namespace imports and CommonJS `require` remain unsupported. No mutation campaign,
managed installation, release or merge was performed.

| Pull request | CI-tested head |
| --- | --- |
{pr_rows}

The broader [#906](https://github.com/Pixel-CLI/pixel/issues/906) equal-answer and
zero-tool-call objective remains open. These relevance counts do not satisfy it.

## Artifacts

The archive contains native reports, source-review ledgers, compressed full briefs,
repeat outputs, identity receipts, CI receipts, runner scripts and intermediate
diagnostics. The final direct flow receipt embeds full stdout/stderr and is also
expanded into per-case text artifacts; the Excalidraw profile receipt is preserved
beside it. `repeat-selection.json` records {len(repeat_selection.get('ids', []))} selected changed public cases and
the archived `repeat_changed.py` reproduces that selection. Use `artifact-hashes.json`
and `verify.py` to verify archived files.
The prior report remains unchanged in `../brief-relevance-2026-10-19/`.
'''
(root / 'README.md').write_text(readme)
print(root / 'README.md')
