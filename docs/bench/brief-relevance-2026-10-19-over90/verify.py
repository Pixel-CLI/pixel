#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Verify archived identities and recount judgments; no automated relevance claim."""
import gzip
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent

def read_json(name):
    return json.loads((ROOT / name).read_text())

def rows(name):
    path = ROOT / name
    raw = gzip.decompress(path.read_bytes()) if name.endswith('.gz') else path.read_bytes()
    items = [json.loads(line) for line in raw.decode().splitlines()]
    result = {row['id']: row for row in items}
    assert len(result) == len(items), name
    return result

def arm(name, count, require_complete=True):
    report = read_json(name + '.json')
    sessions = {row['id']: row for row in report['sessions']}
    briefs = rows(name + '-briefs.jsonl.gz')
    review = rows(name + '-review.jsonl')
    assert len(sessions) == count and sessions.keys() == briefs.keys() == review.keys(), name
    if require_complete:
        assert report['result']['exit_code'] == 0, name
    print(f'{name} native result: {report["result"]}')
    probe_failures = [(row['id'], probe['name'], probe['outcome'], probe.get('ms'))
                      for row in report['sessions']
                      for probe in (row.get('brief_meta') or {}).get('probes', [])
                      if probe.get('outcome') in ('failed', 'timeout')]
    print(f'{name} component probe failures/timeouts: {probe_failures}')
    for key, judgment in review.items():
        digest = hashlib.sha256(briefs[key]['brief'].encode()).hexdigest()
        assert digest == briefs[key]['sha256'] == judgment['brief_sha256'], key
        assert judgment['repo_sha'] == sessions[key]['sha'], key
        assert judgment['native_status'] == sessions[key]['status'], key
        assert judgment['reason'] and judgment['basis'], key
        if judgment.get('expected_empty'):
            assert judgment['relevant'] is None, key
            assert judgment['passed_empty'] == (briefs[key]['brief'] == ''), key
        else:
            assert type(judgment['relevant']) is bool, key
    positive = [row for row in review.values() if not row.get('expected_empty')]
    relevant = sum(row['relevant'] for row in positive)
    print(f'{name}: {relevant}/{len(positive)} relevant ({100 * relevant / len(positive):.2f}%)')
    controls = [row for row in review.values() if row.get('expected_empty')]
    if controls:
        print(f'  empty controls: {sum(row["passed_empty"] for row in controls)}/{len(controls)}')
    return report, review, briefs

for rel, expected in read_json('artifact-hashes.json').items():
    assert hashlib.sha256((ROOT / rel).read_bytes()).hexdigest() == expected, rel
baseline, base_review, base_briefs = arm('baseline', 56, require_complete=False)
candidate, cand_review, cand_briefs = arm('candidate', 56, require_complete=False)
assert baseline['binary']['sha256'] == candidate['binary']['sha256']
assert baseline['scorer']['sha256'] == candidate['scorer']['sha256']
assert baseline['questions'] == candidate['questions']
assert baseline['route'] == candidate['route'] == 'hook'
gains = [key for key in base_review if not base_review[key]['relevant'] and cand_review[key]['relevant']]
losses = [key for key in base_review if base_review[key]['relevant'] and not cand_review[key]['relevant']]
print(f'Gains ({len(gains)}): {", ".join(gains)}; losses ({len(losses)}): {", ".join(losses) or "none"}')
supp, supp_review, _ = arm('supplemental', 88, require_complete=False)
assert sum(bool(row.get('expected_empty')) for row in supp_review.values()) == 70
arm('first-candidate', 56)
arm('first-supplemental', 88, require_complete=False)
repeat = read_json('repeat.json')
repeated = rows('repeat-briefs.jsonl.gz')
arm('repeat', len(repeated), require_complete=False)
changed = {key for key in base_review if base_briefs[key]['sha256'] != cand_briefs[key]['sha256']}
assert repeated.keys() == changed
assert repeat['probe_home']['probe_files'] == candidate['probe_home']['probe_files']
identical = [key for key, row in repeated.items() if row['brief'] == cand_briefs[key]['brief']]
print(f'Changed briefs identical on repeat: {len(identical)}/{len(changed)}')
print(f'Repeat native result: {repeat["result"]}')
print(f'Nonidentical repeated briefs: {sorted(set(repeated) - set(identical))}')
identity = read_json('candidate-identity.json')
assert identity['probe_files'] == candidate['probe_home']['probe_files']
assert identity['source_commit'] == candidate['probe_home']['commit']
ci = read_json('ci-results.json')
assert len(ci) == 8
units = {row['pr']: row for row in read_json('unit-identity.json')}
for item in ci:
    assert item['current_head_checks_pass'] and item['ci_runs'], item['pr']['number']
    unit = units[item['pr']['number']]
    assert unit['head'] == item['pr']['headRefOid']
    expected_runtime = {rel for rel in unit['changed_files']
                        if (rel.startswith('probes/') and Path(rel).name in ('probe', 'pixel-plugin.toml'))
                        or rel == 'scripts/bench/tool-census/score_brief.py'}
    assert set(unit['runtime_files']) == expected_runtime, item['pr']['number']
    for rel, digest in unit['runtime_files'].items():
        if rel.startswith('probes/'):
            assert candidate['probe_home']['probe_files'][rel.removeprefix('probes/')] == digest
        else:
            assert candidate['scorer']['sha256'] == digest
    for run in item['ci_runs'].values():
        assert run['headSha'] == item['pr']['headRefOid']
        assert run['status'] == 'completed' and run['conclusion'] == 'success'
print(f'Current-head CI receipts verified: {len(ci)}/{len(ci)}')
print('Hashes, denominators and recorded judgments verified; relevance requires source review.')
