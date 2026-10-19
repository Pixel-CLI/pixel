#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Bind each tested review branch to the frozen candidate's runtime files."""
import hashlib
import json
import subprocess
from pathlib import Path

root = Path('/Users/livio/Documents/pixel-worktrees')
here = Path('/tmp/pixel-brief-over-90')
source = root / 'brief-over-90'
units = [
    (1255, 'brief-test-admission', 'test-bodies'),
    (1256, 'brief-history-subjects', 'history-fallback'),
    (1257, 'brief-config-evidence', 'config-registry'),
    (1258, 'brief-diagnostic-evidence', 'error-origin'),
    (1259, 'brief-module-evidence', 'overview-answer-evidence'),
    (1262, 'brief-flow-bindings', 'flow-rank'),
    (1263, 'brief-caller-bindings', 'cand-j-callers'),
    (1265, 'brief-observe-scoring', None),
]
rows = []
for number, tree, probe in units:
    path = root / tree
    def git(*args):
        return subprocess.run(['rtk', 'git', *args], cwd=path, text=True,
                              capture_output=True, check=True).stdout.strip()
    assert not git('status', '--porcelain'), tree
    changed_files = git('diff', '--name-only', 'origin/main...HEAD').splitlines()
    files = [rel for rel in changed_files
             if (rel.startswith('probes/') and Path(rel).name in ('probe', 'pixel-plugin.toml'))
             or rel == 'scripts/bench/tool-census/score_brief.py']
    assert files, (number, changed_files)
    hashes = {}
    for rel in files:
        raw = (path / rel).read_bytes()
        assert raw == (source / rel).read_bytes(), (number, rel)
        hashes[rel] = hashlib.sha256(raw).hexdigest()
    rows.append({'pr': number, 'head': git('rev-parse', 'HEAD'),
                 'base_for_diff': git('merge-base', 'origin/main', 'HEAD'),
                 'changed_files': changed_files,
                 'worktree': str(path), 'runtime_files': hashes})
target = here / 'unit-identity.json'
target.write_text(json.dumps(rows, indent=2) + '\n')
print(json.dumps(rows, indent=2))
