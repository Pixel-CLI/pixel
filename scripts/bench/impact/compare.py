#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Compare two impact revisions with shared store/dependencies and alternating batches."""
import argparse
import hashlib
import json
import math
from pathlib import Path
import platform
import statistics
import subprocess
import sys
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--base', required=True)
parser.add_argument('--cargo', default='cargo')
parser.add_argument('--candidate', default='HEAD')
parser.add_argument('--output', type=Path, required=True)
args = parser.parse_args()
repo = Path(subprocess.check_output(['git', 'rev-parse', '--show-toplevel'], text=True).strip())
args.output.mkdir(parents=True)
source = 'crates/pixel-graph/src/impact.rs'
identity = {'platform': platform.platform(), 'command': sys.argv,
            'store_context_sha': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
            'cargo_lock_sha256': hashlib.sha256((repo / 'Cargo.lock').read_bytes()).hexdigest(),
            'harness_sha256': hashlib.sha256(Path(__file__).with_name('measure.rs').read_bytes()).hexdigest()}
with tempfile.TemporaryDirectory(prefix='pixel-impact-bench-') as temporary:
    scratch = Path(temporary)
    for label, revision in [('before', args.base), ('after', args.candidate)]:
        sha = subprocess.check_output(['git', 'rev-parse', revision], text=True).strip()
        content = subprocess.check_output(['git', 'show', f'{sha}:{source}'])
        (scratch / f'{label}.rs').write_bytes(content)
        identity[label] = {'sha': sha, 'impact_sha256': hashlib.sha256(content).hexdigest()}
    main = Path(__file__).with_name('measure.rs').read_text()
    for label in ['before', 'after']:
        main += f'\n#[allow(dead_code)]\n#[path = "{label}.rs"]\nmod {label};\n'
    (scratch / 'main.rs').write_text(main)
    # Freeze dependency versions to the repository's lockfile, including the store.
    (scratch / 'Cargo.lock').write_bytes((repo / 'Cargo.lock').read_bytes())
    (scratch / 'Cargo.toml').write_text(f'''[package]
name = "pixel-impact-comparison"
version = "0.0.0"
edition = "2024"
[[bin]]
name = "impact-comparison"
path = "main.rs"
[dependencies]
pixel-graph = {{ path = {json.dumps(str(repo / 'crates/pixel-graph'))} }}
rusqlite = {{ version = "0.41", features = ["trace"] }}
rustc-hash = "1"
serde = {{ version = "1", features = ["derive"] }}
serde_json = "1"
libc = "0.2"
[profile.release]
opt-level = 3
''')
    target = repo / 'target/impact-bench'
    command = [args.cargo, 'build', '--offline', '--release', '--manifest-path', str(scratch / 'Cargo.toml'), '--target-dir', str(target)]
    with (args.output / 'build.log').open('w') as log:
        subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True)
    binary = target / 'release/impact-comparison'
    identity['binary_sha256'] = hashlib.sha256(binary.read_bytes()).hexdigest()
    identity['rustc'] = subprocess.check_output(['rustc', '--version'], text=True).strip()
    result = subprocess.run([str(binary)], capture_output=True, text=True)
    (args.output / 'samples.jsonl').write_text(result.stdout)
    (args.output / 'measure.stderr').write_text(result.stderr)
    result.check_returncode()
if len(result.stdout.splitlines()) != 10:
    raise RuntimeError('expected all ten fixture results')
rows = []
for line in result.stdout.splitlines():
    row = json.loads(line)
    before, after = zip(*row['samples_ns'])
    ratios = [math.log(b / a) for a, b in row['samples_ns']]
    upper = math.exp(statistics.mean(ratios) + 1.655 * statistics.stdev(ratios) / math.sqrt(len(ratios)))
    # One-sided 95% paired t interval, df=160; 3% is this lane's sensitivity.
    cpu_before, cpu_after = zip(*row['cpu_samples_ns'])
    cpu_ratios = [math.log(b / a) for a, b in row['cpu_samples_ns']]
    cpu_upper = math.exp(statistics.mean(cpu_ratios) + 1.655 * statistics.stdev(cpu_ratios) / math.sqrt(len(cpu_ratios)))
    passed = upper <= 1.03 and cpu_upper <= 1.03
    summary = {'case': row['case'], 'before_p50_ns': statistics.median(before),
               'after_p50_ns': statistics.median(after), 'before_p95_ns': sorted(before)[38],
               'after_p95_ns': sorted(after)[38], 'ratio_upper_95': upper,
               'sql_statements': row['sql_statements'], 'pass': passed,
               'cpu_before_p50_ns': statistics.median(cpu_before),
               'cpu_after_p50_ns': statistics.median(cpu_after), 'cpu_ratio_upper_95': cpu_upper}
    rows.append(summary)
    print(f"{'PASS' if passed else 'FAIL'} {row['case']}: median {summary['before_p50_ns']/1000:.1f} -> {summary['after_p50_ns']/1000:.1f} us; wall upper95={upper:.4f}, CPU upper95={cpu_upper:.4f}")
identity['results'] = rows
(args.output / 'results.json').write_text(json.dumps(identity, indent=2) + '\n')
print(f"{sum(row['pass'] for row in rows)}/{len(rows)} cases within the declared 3% regression sensitivity; SQL counts identical.")
raise SystemExit(0 if all(row['pass'] for row in rows) else 1)
