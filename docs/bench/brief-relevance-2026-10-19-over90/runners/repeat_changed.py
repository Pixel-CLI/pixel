#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Repeat exactly the public cases whose saved brief differs from baseline."""
import hashlib
import json
import subprocess
from pathlib import Path

HERE = Path('/tmp/pixel-brief-over-90')
BASE = 'final-baseline-r3-412d302f'
CANDIDATE = 'final-candidate-df9f39fa'
LABEL = 'final-repeat-df9f39fa'
report = json.loads((HERE / 'runs' / CANDIDATE / 'report.json').read_text())
def digest(label, key):
    return hashlib.sha256((HERE / 'runs' / label / 'briefs' / (key + '.txt')).read_bytes()).hexdigest()
changed = [row['id'] for row in report['sessions'] if digest(BASE, row['id']) != digest(CANDIDATE, row['id'])]
command = ['rtk', 'bypass', 'python3', str(HERE / 'run_score.py'), 'candidate', '--label', LABEL,
           '--root', str(HERE / 'final3-candidate-root'), '--only', ','.join(changed)]
(HERE / 'repeat-selection.json').write_text(json.dumps({'baseline': BASE, 'candidate': CANDIDATE,
    'repeat': LABEL, 'ids': changed, 'command': command}, indent=2) + '\n')
print(json.dumps({'repeat': LABEL, 'count': len(changed), 'ids': changed}), flush=True)
raise SystemExit(subprocess.run(command).returncode)
