#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Opt-in private receipt for one native Pixel plan call; installs no hooks."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

QUERIES = ['dead-interactive', 'dead-code', 'hotspots', 'recent-changes', 'by-concept']


class CaptureError(Exception):
    pass


def sha(data):
    return hashlib.sha256(data).hexdigest()


def file_sha(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def context(binary, repo):
    result = subprocess.run([binary, 'repo-state', str(repo), '--json'], cwd=repo,
                            env={**os.environ, 'PIXEL_METRICS': '0'}, capture_output=True, timeout=30)
    if result.returncode:
        raise CaptureError(f'repository-state command failed with exit {result.returncode}')
    state = json.loads(result.stdout)
    required = {'head', 'branch', 'root', 'dirty', 'dirty_count'}
    if (not isinstance(state, dict) or not required.issubset(state) or state.get('truncated')
            or not isinstance(state['head'], str) or not state['head']
            or not isinstance(state['root'], str) or not isinstance(state['dirty'], list)
            or type(state['dirty_count']) is not int):
        raise CaptureError('repository-state response is incomplete')
    if len(state['dirty']) != state['dirty_count'] or Path(state['root']).resolve() != repo:
        raise CaptureError('repository-state is capped or does not describe the requested root')
    return {'head': state['head'], 'branch': state['branch'], 'dirty_count': state['dirty_count'],
            'dirty_digest': sha(json.dumps(state['dirty'], sort_keys=True).encode())}


def capture(args):
    repo = args.repo.resolve()
    executable = shutil.which(args.pixel)
    if executable is None:
        raise CaptureError('Pixel executable not found; no installer will be run')
    # Preserve argv[0]/shim behavior, just as venv interpreter symlinks must be preserved.
    binary = str(Path(executable).absolute())
    output = args.output.parent.resolve() / args.output.name
    if output.is_relative_to(repo):
        ignored = subprocess.run(['git', '-C', str(repo), 'check-ignore', '-q', '--', str(output)])
        if ignored.returncode != 0:
            raise CaptureError('receipt must be outside the source tree or in a git-ignored path')
    output.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    command = [binary, 'plan']
    if args.prompt is not None:
        command.append(args.prompt)
    for option in ['query', 'tag', 'limit', 'format', 'max_todos', 'metrics']:
        value = getattr(args, option)
        if value is not None:
            command.extend(['--' + option.replace('_', '-'), str(value)])
    if args.json:
        command.append('--json')
    if args.no_verify:
        command.append('--no-verify')
    receipt = {'schema': 1, 'purpose': args.purpose, 'privacy': 'local-only; exact prompt and paths recorded',
               'request': {'prompt': args.prompt, 'explicit_query': args.query,
                           'input_sha256': sha(args.prompt.encode()) if args.prompt is not None else None},
               'repository': str(repo), 'argv': command, 'started_at_ns': time.time_ns(),
               'plan_started': False, 'capture_status': 'in_progress',
               'serving_backend': 'not_attested_cli_may_delegate_to_existing_daemon',
               'output_storage': 'hashes_and_byte_counts_only',
               'actor': 'not_inferred', 'gold': None}
    stdout, stderr, code = b'', b'', 2
    try:
        version = subprocess.run([binary, '--version'], capture_output=True, timeout=30, check=True)
        receipt['launcher'] = {'path': binary, 'sha256': file_sha(binary),
                               'version_stdout': version.stdout.decode('utf-8', errors='strict')}
        receipt['before'] = context(binary, repo)
        start = time.perf_counter()
        try:
            result = subprocess.run(command, cwd=repo, capture_output=True, timeout=args.timeout)
            stdout, stderr, code = result.stdout, result.stderr, result.returncode
            receipt['plan_started'] = True
            receipt['native_exit_code'] = code
            receipt['timed_out'] = False
        except subprocess.TimeoutExpired as error:
            stdout, stderr, code = error.stdout or b'', error.stderr or b'', 124
            receipt['plan_started'] = True
            receipt['native_exit_code'] = None
            receipt['timed_out'] = True
        receipt['elapsed_seconds'] = time.perf_counter() - start
        receipt['completion'] = {'stdout_sha256': sha(stdout), 'stdout_bytes': len(stdout),
                                 'stderr_sha256': sha(stderr), 'stderr_bytes': len(stderr)}
        try:
            receipt['after'] = context(binary, repo)
            receipt['launcher_after_sha256'] = file_sha(binary)
            receipt['before_after_equal'] = receipt['before'] == receipt['after']
            receipt['launcher_unchanged'] = receipt['launcher']['sha256'] == receipt['launcher_after_sha256']
            receipt['capture_status'] = 'complete'
        except (CaptureError, OSError, ValueError, subprocess.SubprocessError) as error:
            # The native call already completed: retain its exit status and disclose
            # incomplete provenance rather than substituting a successful snapshot.
            receipt['capture_status'] = 'incomplete_postflight'
            receipt['capture_error_type'] = type(error).__name__
            if isinstance(error, CaptureError):
                receipt['capture_error'] = str(error)
            print('record-plan-request: postflight capture failed; receipt is incomplete', file=sys.stderr)
    except (CaptureError, OSError, ValueError, subprocess.SubprocessError) as error:
        receipt['capture_status'] = 'failed'
        receipt['capture_error_type'] = type(error).__name__
        if isinstance(error, CaptureError):
            receipt['capture_error'] = str(error)
        print(f'record-plan-request: capture failed ({type(error).__name__}); inspect the private receipt', file=sys.stderr)
    finally:
        receipt['finished_at_ns'] = time.time_ns()
        try:
            with os.fdopen(fd, 'w') as stream:
                json.dump(receipt, stream, indent=2, allow_nan=False)
                stream.write('\n')
        except (OSError, ValueError):
            print('record-plan-request: receipt write failed; no valid capture was saved', file=sys.stderr)
        finally:
            sys.stdout.buffer.write(stdout)
            sys.stderr.buffer.write(stderr)
    return code if code >= 0 else 128 - code


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--pixel', required=True, help='explicit executable path/name; identity is recorded')
    parser.add_argument('--repo', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True, help='NEW private receipt; never overwritten')
    parser.add_argument('--purpose', choices=['observed', 'mechanism-fixture'], default='observed')
    parser.add_argument('--prompt')
    parser.add_argument('--query', choices=QUERIES)
    parser.add_argument('--tag')
    parser.add_argument('--limit', type=int)
    parser.add_argument('--format', choices=['markdown', 'json', 'compact'])
    parser.add_argument('--max-todos', type=int)
    parser.add_argument('--metrics', choices=['on', 'off'])
    parser.add_argument('--json', action='store_true')
    parser.add_argument('--no-verify', action='store_true')
    parser.add_argument('--timeout', type=float, default=60)
    args = parser.parse_args()
    if args.prompt is None and args.query is None:
        parser.error('provide --prompt or --query')
    if not args.repo.is_dir() or not 0 < args.timeout <= 300:
        parser.error('repository must exist and timeout must be in (0, 300] seconds')
    try:
        return capture(args)
    except (CaptureError, FileExistsError) as error:
        print(f'record-plan-request: {error}', file=sys.stderr)
        return 2


if __name__ == '__main__':
    raise SystemExit(main())
