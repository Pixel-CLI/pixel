#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Serialize native scoring and retain the command, log and exit status."""
import argparse
import fcntl
import json
import os
import subprocess
import time
from pathlib import Path

HERE = Path('/tmp/pixel-brief-over-90')
REPO = Path('/Users/livio/Documents/pixel-worktrees/brief-over-90')
parser = argparse.ArgumentParser()
parser.add_argument('arm', choices=('baseline', 'candidate'))
parser.add_argument('--label', required=True)
parser.add_argument('--questions', default=str(REPO / 'eval/brief-bench/questions.jsonl'))
parser.add_argument('--only')
parser.add_argument('--root', help='an independently prepared source/index root')
args = parser.parse_args()
command = ['rtk', 'bypass', 'python3', '-I', str(REPO / 'scripts/bench/tool-census/score_brief.py'),
           '--root', args.root or str(HERE / f'{args.arm}-root'),
           '--bin', '/tmp/pixel-brief-coverage-evidence/target/dev-release/pixel',
           '--label', args.label, '--probe-home', str(HERE / f'{args.arm}-home'),
           '--questions', args.questions, '--out', str(HERE / 'runs'), '--route', 'hook']
if args.only:
    command += ['--only', args.only]
log = HERE / f'{args.label}.log'
queued = time.time()
with Path('/tmp/pixel-benchmark.lock').open('a') as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    started = time.time()
    load_start = os.getloadavg()
    with log.open('w') as output:
        result = subprocess.run(command, cwd=REPO, stdout=output, stderr=subprocess.STDOUT)
    ended = time.time()
    load_end = os.getloadavg()
receipt = {'command': command, 'log': str(log), 'queued_unix': queued,
           'started_unix': started, 'ended_unix': ended, 'exit_code': result.returncode,
           'load_average_start': load_start, 'load_average_end': load_end,
           'logical_cpus': os.cpu_count()}
(HERE / f'{args.label}.receipt.json').write_text(json.dumps(receipt, indent=2)+'\n')
print(json.dumps(receipt, indent=2))
raise SystemExit(result.returncode)
