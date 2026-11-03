#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Archive fixed native outputs and source judgments without deriving relevance."""
import argparse
import gzip
import hashlib
import json
import shutil
from pathlib import Path

HERE = Path('/tmp/pixel-brief-over-90')
parser = argparse.ArgumentParser()
for arm in ('baseline', 'candidate', 'supplemental', 'repeat'):
    parser.add_argument('--' + arm, required=True)
parser.add_argument('--identity', required=True)
parser.add_argument('--out', required=True)
args = parser.parse_args()
out = Path(args.out)
out.mkdir(parents=True, exist_ok=True)

def digest(raw):
    return hashlib.sha256(raw).hexdigest()

def write_json(name, value):
    (out / name).write_text(json.dumps(value, indent=2) + '\n')

labels = {arm: getattr(args, arm) for arm in ('baseline', 'candidate', 'supplemental', 'repeat')}
labels.update({'first-candidate': 'candidate-0ddf2a0a',
               'first-supplemental': 'supplemental-0ddf2a0a',
               'diagnostic-baseline': 'baseline45-412d302f',
               'earlier-baseline': 'baseline45-r2-412d302f'})
run_manifest = {}
for arm, label in labels.items():
    directory = HERE / 'runs' / label
    report = json.loads((directory / 'report.json').read_text())
    write_json(arm + '.json', report)
    rows = []
    for session in report['sessions']:
        raw = (directory / 'briefs' / (session['id'] + '.txt')).read_bytes()
        rows.append({'id': session['id'], 'sha256': digest(raw), 'brief': raw.decode()})
    serialized = ''.join(json.dumps(row, sort_keys=True) + '\n' for row in rows).encode()
    (out / (arm + '-briefs.jsonl.gz')).write_bytes(gzip.compress(serialized, mtime=0))
    review = HERE / (label + '-review.jsonl')
    if review.exists():
        shutil.copy2(review, out / (arm + '-review.jsonl'))
    receipt = HERE / (label + '.receipt.json')
    if receipt.exists():
        shutil.copy2(receipt, out / (arm + '-receipt.json'))
    run_manifest[arm] = {'label': label, 'count': len(rows), 'status': report['result']}

copies = {
    'candidate-identity.json': Path(args.identity),
    'first-candidate-identity.json': HERE / 'candidate-identity-0ddf2a0a63b6.json',
    'paired-preparation.json': HERE / 'paired-preparation.json',
    'final-root-preparation.json': HERE / 'final-root-preparation.json',
    'final2-root-preparation.json': HERE / 'final2-root-preparation.json',
    'final3-root-preparation.json': HERE / 'final3-root-preparation.json',
    'host-contention.json': HERE / 'host-contention.json',
    'host-contention-final.json': HERE / 'host-contention-final.json',
    'supplemental-questions.jsonl': HERE / 'supplemental-questions.jsonl',
    'supplemental-identity.json': HERE / 'supplemental-identity.json',
    'supplemental-source-review.jsonl': HERE / 'supplemental-source-review.jsonl',
    'ci-results.json': HERE / 'final-ci.json',
    'unit-identity.json': HERE / 'unit-identity.json',
    'repeat-selection.json': HERE / 'repeat-selection.json',
    'flow-manifest-ci-failure.json': HERE / 'flow-manifest-ci-failure.json',
    'flow-native-loader.json': HERE / 'flow-native-loader.json',
    'pixel04-raw-offset-failure.json': HERE / 'pixel04-raw-offset-failure.json',
}
for name, source in copies.items():
    shutil.copy2(source, out / name)
diagnostic = HERE / 'caller-integrate-diagnostic/root/score/runs/caller-admission-diagnostic/report.json'
if diagnostic.exists():
    shutil.copy2(diagnostic, out / 'diagnostic-caller-admission.json')
extra = out / 'diagnostics'
extra.mkdir(exist_ok=True)
for directory in sorted((HERE / 'runs').iterdir()):
    if directory.name in labels.values() or not (directory / 'report.json').exists():
        continue
    report = json.loads((directory / 'report.json').read_text())
    target = extra / directory.name
    target.mkdir(exist_ok=True)
    shutil.copy2(directory / 'report.json', target / 'report.json')
    briefs = [{'id': row['id'], 'brief': (directory / 'briefs' / (row['id'] + '.txt')).read_text()}
              for row in report['sessions']]
    serialized = ''.join(json.dumps(row, sort_keys=True) + '\n' for row in briefs).encode()
    (target / 'briefs.jsonl.gz').write_bytes(gzip.compress(serialized, mtime=0))
    for suffix in ('.receipt.json', '-review.jsonl'):
        source = HERE / (directory.name + suffix)
        if source.exists():
            shutil.copy2(source, target / source.name)
for rel in ('caller-excalidraw-final/root/score/runs/caller-excalidraw-prefilter/report.json',
            'flow-callers/peer-replay-interrupted.json', 'flow-callers/peer-replay-timeout.log',
            'callers-fix/optimization-timing-record.md',
            'config-tests/replay-ripgrep-shortflag.json',
            'flow-final-direct.json', 'flow-excalidraw-profile.json'):
    source = HERE / rel
    if source.exists():
        shutil.copy2(source, extra / (rel.replace('/', '--')))
        if rel == 'flow-final-direct.json':
            direct = json.loads(source.read_text())
            for case in direct.get('cases', []):
                case_id = case.get('id', 'unknown')
                (extra / f'flow-final-direct-{case_id}.stdout.txt').write_text(case.get('stdout', ''))
                (extra / f'flow-final-direct-{case_id}.stderr.txt').write_text(case.get('stderr', ''))
runner_dir = out / 'runners'
runner_dir.mkdir(exist_ok=True)
for name in ('run_score.py', 'freeze_candidate.py', 'prepare_evaluation.py',
             'prepare_review.py', 'prepare_supplemental_review.py', 'capture_ci.py',
             'capture_unit_identity.py', 'repeat_changed.py', 'build_archive.py', 'write_readme.py'):
    shutil.copy2(HERE / name, runner_dir / name)
shutil.copy2(HERE / 'verify_archive.py', out / 'verify.py')
write_json('runs.json', run_manifest)
write_json('artifact-hashes.json', {
    path.relative_to(out).as_posix(): digest(path.read_bytes())
    for path in sorted(out.rglob('*')) if path.is_file()
    and path.name not in ('README.md', 'artifact-hashes.json')
})
print(json.dumps({'archive': str(out), 'runs': run_manifest}, indent=2))
