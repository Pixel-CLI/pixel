#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Preserve frozen controls and inherit only identical positive-source reviews."""
import argparse
import hashlib
import json
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('label')
parser.add_argument('--against', default='/tmp/pixel-brief-over-90/supplemental-0ddf2a0a-review.jsonl')
args = parser.parse_args()
root = Path('/tmp/pixel-brief-over-90')
directory = root / 'runs' / args.label
report = json.loads((directory / 'report.json').read_text())
old = {row['id']: row for row in map(json.loads,
    Path(args.against).read_text().splitlines())}
rows, pending, packet = [], [], []
for session in report['sessions']:
    key = session['id']
    previous = old[key]
    assert previous['repo_sha'] == session['sha'], key
    raw = (directory / 'briefs' / (key + '.txt')).read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    row = dict(previous, run=args.label, actual_bytes=len(raw), brief_sha256=digest,
               native_status=session['status'])
    if previous['expected_empty']:
        row.update(passed_empty=not raw, relevant=None,
                   evidence=f'Saved brief is {len(raw)} bytes.',
                   basis='Frozen empty-output control; excluded from positive relevance denominator.')
    elif digest == previous['brief_sha256'] and session['status'] == 'ok':
        row['basis'] = 'same-bytes-and-source-pin'
    else:
        row.update(relevant=None, evidence='', reason='', basis='pending-source-review')
        pending.append(key)
        packet.append(f'# {key}\n\n{session["prompt"]}\n\nNative status: {session["status"]}\n\n' + raw.decode())
    rows.append(row)
target = root / (args.label + '-review.jsonl')
assert not target.exists(), target
target.write_text(''.join(json.dumps(row, sort_keys=True) + '\n' for row in rows))
(root / (args.label + '-review-packet.md')).write_text('\n\n'.join(packet))
print(json.dumps({'run': args.label, 'total': len(rows), 'pending': pending,
                  'empty_controls': sum(row.get('passed_empty') is True for row in rows)}, indent=2))
