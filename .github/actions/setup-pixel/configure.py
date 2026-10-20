#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Bound the cache to rebuildable indexes, never agent configuration or logs."""
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys

# prepare-repo's text shards and graph; include SQLite journals for consistency.
# History, vectors and the auxiliary MVP index can be rebuilt on demand.
INDEX_FILES = ('base.shard', 'delta.shard', 'state.json', 'graph.v2.db',
               'graph.v2.db-wal', 'graph.v2.db-shm')


def configure():
    for name in ('PREPARE', 'CACHE', 'INSTALL_HOOKS'):
        if os.environ[f'PIXEL_ACTION_{name}'] not in ('true', 'false'):
            raise ValueError(f'{name.lower()} must be true or false')
    agents = os.environ.get('PIXEL_ACTION_HOOKS_AGENT', 'all')
    if not re.fullmatch(r'[a-z][a-z-]*(,[a-z][a-z-]*)*', agents):
        raise ValueError('hooks-agent must be all or a comma-separated list of agent names')
    version = os.environ['PIXEL_ACTION_VERSION']
    if not re.fullmatch(r'v[0-9]+\.[0-9]+\.[0-9]+', version):
        raise ValueError('version must be an exact release tag: vX.Y.Z')
    minimum = os.environ['PIXEL_ACTION_MINIMUM_SYMBOLS']
    if not re.fullmatch(r'[0-9]+', minimum):
        raise ValueError('minimum-symbols must be a non-negative integer')
    workspace = Path(os.environ['GITHUB_WORKSPACE']).resolve()
    path = (workspace / os.environ['PIXEL_ACTION_PATH']).resolve(strict=True)
    if not path.is_dir() or any(c in str(path) for c in '\r\n*?[]!'):
        raise ValueError('path must be a directory without newlines or glob characters')
    cache_dir = path / '.pixel'
    for entry in (cache_dir, *(cache_dir / name for name in INDEX_FILES)):
        if entry.is_symlink() or (entry.exists() and entry != cache_dir and not entry.is_file()):
            raise ValueError(f'refusing non-regular index path: {entry}')
    if os.environ['PIXEL_ACTION_PREPARE'] == 'true':
        root = subprocess.check_output(['git', '-C', str(path), 'rev-parse', '--show-toplevel'], text=True).strip()
        if Path(root).resolve() != path:
            raise ValueError('path must name the checkout root')
        tracked = subprocess.check_output(['git', '-C', str(path), 'ls-files', '--', '.pixel'], text=True)
        if tracked:
            raise ValueError('refusing to restore over tracked .pixel files')
    identity = json.dumps([os.path.relpath(path, workspace), os.environ['PIXEL_ACTION_CACHE_VERSION']])
    digest = hashlib.sha256(identity.encode()).hexdigest()[:24]
    prefix = f'pixel-index-v1-{os.environ["RUNNER_OS"]}-{os.environ["RUNNER_ARCH"]}-{version}-{digest}-'
    day = datetime.datetime.now(datetime.timezone.utc).date().isoformat()
    with open(os.environ['GITHUB_OUTPUT'], 'a') as output:
        output.write(f'path={path}\ncache-prefix={prefix}\ncache-key={prefix}{day}\n')
        output.write('cache-paths<<PIXEL_INDEX_PATHS\n')
        output.write('\n'.join(str(cache_dir / name) for name in INDEX_FILES))
        output.write('\nPIXEL_INDEX_PATHS\n')


def validate_report(filename):
    with open(filename) as source:
        report = json.load(source)
    symbols = report.get('graph', {}).get('symbols')
    minimum = int(os.environ['PIXEL_ACTION_MINIMUM_SYMBOLS'])
    if type(symbols) is not int or symbols < minimum:
        raise ValueError(f'graph has {symbols!r} symbols; requires at least {minimum}')


if __name__ == '__main__':
    try:
        if len(sys.argv) == 3 and sys.argv[1] == 'validate-report':
            validate_report(sys.argv[2])
        else:
            configure()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f'::error::{error}', file=sys.stderr)
        sys.exit(1)
