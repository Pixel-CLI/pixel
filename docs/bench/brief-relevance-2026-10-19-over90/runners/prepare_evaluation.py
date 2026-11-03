#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Prepare matched, isolated benchmark arms and freeze supplemental questions."""
import hashlib
import json
import subprocess
from pathlib import Path

HERE = Path('/tmp/pixel-brief-over-90')
REPO = Path('/Users/livio/Documents/pixel-worktrees/brief-over-90')
OLD = Path('/tmp/pixel-brief-coverage-evidence')
SEED = OLD / 'state'
BIN = OLD / 'target/dev-release/pixel'

def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()

def inventory(root):
    rows = {}
    for path in sorted(root.rglob('*')):
        rel = path.relative_to(root).as_posix()
        if path.is_symlink():
            rows[rel] = {'symlink': str(path.readlink())}
        elif path.is_file():
            rows[rel] = {'sha256': digest(path), 'bytes': path.stat().st_size}
    encoded = json.dumps(rows, sort_keys=True, separators=(',', ':')).encode()
    return rows, hashlib.sha256(encoded).hexdigest()

def copy_once(source, dest):
    if dest.exists():
        raise SystemExit(f'Refusing to overwrite existing {dest}')
    subprocess.run(['rtk', 'cp', '-c', '-R', str(source), str(dest)], check=True)

def main():
    source_rows, source_hash = inventory(SEED)
    (HERE / 'seed-files-before.json').write_text(json.dumps(source_rows, indent=2)+'\n')
    home_rows, home_hash = inventory(OLD / 'paired-candidate-home')
    (HERE / 'home-files-before.json').write_text(json.dumps(home_rows, indent=2)+'\n')
    for arm in ('baseline', 'candidate'):
        copy_once(SEED, HERE / f'{arm}-root')
        copy_once(OLD / 'paired-candidate-home', HERE / f'{arm}-home')
        rows, tree_hash = inventory(HERE / f'{arm}-root')
        assert tree_hash == source_hash, arm
        rows, tree_hash = inventory(HERE / f'{arm}-home')
        assert tree_hash == home_hash, arm
        print(f'{arm}: seed and frozen home match byte hashes', flush=True)
    questions = REPO / 'eval/brief-bench/questions.jsonl'
    receipt = {'binary': str(BIN), 'binary_sha256': digest(BIN),
               'baseline_source': '412d302f42157532b10092f81c4c2ba8f52b7e63',
               'seed_root': str(SEED), 'seed_tree_sha256': source_hash,
               'home_seed': str(OLD / 'paired-candidate-home'), 'home_tree_sha256': home_hash,
               'questions_sha256': digest(questions), 'seed_files': len(source_rows),
               'home_files': len(home_rows), 'verified_equal_before_any_run': True}
    (HERE / 'paired-preparation.json').write_text(json.dumps(receipt, indent=2)+'\n')
    print(json.dumps(receipt, indent=2))

if __name__ == '__main__':
    main()
