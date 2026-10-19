#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Overlay an immutable candidate and record every runtime input."""
import hashlib
import json
import shutil
import subprocess
from pathlib import Path

HERE = Path('/tmp/pixel-brief-over-90')
REPO = Path('/Users/livio/Documents/pixel-worktrees/brief-over-90')
HOME = HERE / 'candidate-home'

def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def git(*args):
    return subprocess.run(['rtk','git','-C',str(REPO),*args],capture_output=True,text=True,check=True).stdout.strip()

commit = git('rev-parse','HEAD')
if git('status','--porcelain','--untracked-files=no'):
    raise SystemExit('Freeze requires a committed clean tracked snapshot')
manifest_path = HOME / 'probe-home.json'
manifest = json.loads(manifest_path.read_text())
previous = dict(manifest['probe_files'])
changed = []
for name in manifest['probes']:
    for filename in ('probe','pixel-plugin.toml'):
        source = REPO / 'probes' / name / filename
        destination = HOME / '.pixel/plugins' / name / filename
        key = f'{name}/{filename}'
        digest = sha(source)
        if previous.get(key) != digest:
            changed.append(key)
        shutil.copy2(source,destination)
        manifest['probe_files'][key] = digest
manifest['commit'] = commit
manifest_path.write_text(json.dumps(manifest,indent=2)+'\n')
errors = [key for key,digest in manifest['probe_files'].items()
          if sha(HOME/'.pixel/plugins'/key) != digest]
assert not errors, errors
identity = {'source_repo':str(REPO), 'source_commit':commit,
            'changed_runtime_files':changed, 'probe_files':manifest['probe_files'],
            'runtime_manifest_sha256':sha(manifest_path),
            'model_onnx_sha256':sha(HOME/'.local/share/pixel/models/verdict/model.onnx'),
            'classify_config_sha256':sha(HOME/'.pixel/config.yaml'),
            'manifest_file_errors':errors}
target = HERE / f'candidate-identity-{commit[:12]}.json'
if target.exists():
    raise SystemExit(f'Identity already recorded: {target}')
target.write_text(json.dumps(identity,indent=2)+'\n')
print(json.dumps(identity,indent=2))
