#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Inherit only byte-identical, same-pin source reviews; leave all others pending."""
import argparse
import hashlib
import json
from pathlib import Path

HERE=Path('/tmp/pixel-brief-over-90')
parser=argparse.ArgumentParser()
parser.add_argument('label')
parser.add_argument('--against',required=True)
args=parser.parse_args()
directory=HERE/'runs'/args.label
report=json.loads((directory/'report.json').read_text())
previous={row['id']:row for row in map(json.loads,Path(args.against).read_text().splitlines())}
rows=[]; pending=[]; packets=[]
for case in report['sessions']:
    path=directory/'briefs'/f'{case["id"]}.txt'
    raw=path.read_bytes(); digest=hashlib.sha256(raw).hexdigest()
    old=previous.get(case['id'])
    same=bool(old and old['brief_sha256']==digest and old['repo_sha']==case['sha']
              and case['status']=='ok' and old.get('relevant') is not None)
    row={'id':case['id'],'run':args.label,'repo_sha':case['sha'],
         'brief_sha256':digest,'native_status':case['status'],
         'relevant':old['relevant'] if same else None,
         'evidence':old['evidence'] if same else '',
         'reason':old['reason'] if same else '',
         'basis':'same-bytes-and-source-pin' if same else 'pending-source-review'}
    rows.append(row)
    if not same:
        pending.append(case['id'])
        packets.append(f'# {case["id"]}\n\n{case["prompt"]}\n\nNative status: {case["status"]}; source: {case["sha"]}\n\n'+raw.decode())
output=HERE/f'{args.label}-review.jsonl'
if output.exists():
    raise SystemExit(f'Refusing to overwrite existing review {output}')
output.write_text(''.join(json.dumps(row,sort_keys=True)+'\n' for row in rows))
(HERE/f'{args.label}-review-packet.md').write_text('\n\n'.join(packets))
print(json.dumps({'run':args.label,'total':len(rows),'inherited':len(rows)-len(pending),
                  'pending':pending,'output':str(output)},indent=2))
