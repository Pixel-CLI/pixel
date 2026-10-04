#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Evaluate frozen routers on independently annotated, captured real requests."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import random
import resource
import sys
import time

START = time.perf_counter()
for key in ['OMP_NUM_THREADS', 'OPENBLAS_NUM_THREADS', 'MKL_NUM_THREADS', 'VECLIB_MAXIMUM_THREADS']:
    os.environ[key] = '1'


def load_script(filename, name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def save(path, value):
    with Path(path).open('x') as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write('\n')


def validate_gold(requests, gold):
    if len(gold) != len(requests) or len({g['id'] for g in gold}) != len(gold):
        raise ValueError('gold must cover each request exactly once')
    by_id = {g['id']: g for g in gold}
    if set(by_id) != {r['id'] for r in requests}:
        raise ValueError('gold/request ID mismatch')
    allowed = {'by-concept', 'dead-code', 'dead-interactive', 'hotspots', 'recent-changes'}
    for row in requests:
        g = by_id[row['id']]
        if not g['evidence_quote'] or g['evidence_quote'] not in row['text']:
            raise ValueError('nonliteral gold evidence: ' + row['id'])
        if len(set(g['labels'])) != len(g['labels']) or set(g['labels']) - allowed:
            raise ValueError('unknown or duplicated gold label')
        if not g['labels'] and not (g['ambiguous'] or g['unsupported_composition']):
            raise ValueError('supported gold needs at least one query')
        if not g['family'] or not g['rationale']:
            raise ValueError('missing gold rationale/family')
        if type(g['ambiguous']) is not bool or type(g['unsupported_composition']) is not bool:
            raise ValueError('gold flags must be boolean')
        if hashlib.sha256(row['text'].encode()).hexdigest() != row['text_sha256']:
            raise ValueError('captured request changed')
    return [dict(r, **{k: v for k, v in by_id[r['id']].items() if k != 'id'}) for r in requests]


def mechanism(mixed, socket_path):
    request = 'Locate classify_prompt; rank files with highest fan-in; review recent churn'
    plan = mixed.route(request)
    if [j['query'] for j in plan['jobs']] != ['by-concept', 'hotspots', 'recent-changes']:
        raise ValueError('mixed mechanism routing failed')
    if plan['jobs'][0]['prompt'] != 'classify_prompt':
        raise ValueError('concept received wrong clause')
    outputs = mixed.execute(plan, lambda req: mixed.rpc(socket_path, req))
    for index, row in enumerate(outputs):
        if row['response']['result']['queries'] != [plan['jobs'][index]['query']]:
            raise ValueError('daemon executed a different query')
        findings = row['response']['result']['findings']
        cap = [20, 10, 20][index]
        if not findings or len(findings) > cap:
            raise ValueError('missing mechanism findings or operation cap exceeded')
        if row['response']['epistemics']['closed_world']:
            raise ValueError('static graph unexpectedly claims closed-world proof')
    if not any(f['file'] == 'crates/pixel-graph/src/plan.rs' for f in outputs[0]['response']['result']['findings']):
        raise ValueError('concept did not retrieve its source implementation')
    unsupported = mixed.route('Find unused functions only in recently changed files')
    if unsupported['status'] != 'unsupported' or unsupported['jobs']:
        raise ValueError('unsupported intersection was treated as union')
    return {'request': request, 'plan': plan, 'outputs': outputs, 'unsupported': unsupported,
            'role': 'purpose-built software contract check, not quality corpus gold'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ['requests', 'gold', 'candidate-freeze', 'model', 'socket', 'output']:
        parser.add_argument('--' + key, required=True)
    args = parser.parse_args()
    if sys.platform != 'darwin':
        parser.error('this pilot reports Darwin ru_maxrss bytes')
    out = Path(args.output)
    out.mkdir(parents=True, exist_ok=False)
    frozen = json.loads(Path(args.candidate_freeze).read_text())
    for path, digest in frozen.items():
        if sha(path) != digest:
            raise ValueError('frozen artifact changed: ' + path)
    requests = json.loads(Path(args.requests).read_text())['requests']
    gold = [json.loads(line) for line in Path(args.gold).read_text().splitlines() if line.strip()]
    rows = validate_gold(requests, gold)
    mixed = load_script('bench-plan-mixed.py', 'mixed_router')
    old = load_script('bench-plan-routing.py', 'original_pilot')
    save(out/'freeze.json', {'inputs': {p: sha(p) for p in [args.requests, args.gold, args.candidate_freeze, args.model, __file__]},
                           'candidate_freeze': frozen, 'command': sys.argv,
                           'performance_validated': False, 'source_head': old.BASE})
    save(out/'mechanism.json', mechanism(mixed, args.socket))
    import joblib
    import numpy as np
    imports_end = time.perf_counter()
    model = joblib.load(args.model)
    load_seconds = time.perf_counter()-imports_end
    records = []
    with (out/'raw.jsonl').open('x') as stream:
        for row in rows:
            t = time.perf_counter()
            request = {'op': 'plan', 'prompt': row['text']}
            envelope = mixed.rpc(args.socket, request)
            if not envelope['ok'] or envelope.get('error') is not None:
                save(out/'failed-rpc.json', {'id': row['id'], 'request': request, 'response': envelope})
                raise ValueError('reference Plan failed: ' + row['id'])
            baseline = sorted(set(envelope['result']['queries']))
            if baseline != old.baseline(row['text']):
                save(out/'parity-failure.json', {'id': row['id'], 'request': request, 'response': envelope})
                raise ValueError('current-source/port parity failure')
            rpc_ms = 1000*(time.perf_counter()-t)
            t = time.perf_counter()
            plan = mixed.route(row['text'])
            rule_ms = 1000*(time.perf_counter()-t)
            t = time.perf_counter()
            probabilities = model.predict_proba([row['text']])[0].tolist()
            linear_ms = 1000*(time.perf_counter()-t)
            record = dict(row, baseline=baseline, rules=sorted({j['query'] for j in plan['jobs']}),
                          linear=old.decode(probabilities), constant=['by-concept'],
                          plan=plan, probabilities=probabilities, rpc_ms=rpc_ms,
                          rules_ms=rule_ms, linear_ms=linear_ms, request=request, response=envelope)
            stream.write(json.dumps(record)+'\n')
            stream.flush()
            records.append(record)
    eligible = [r for r in records if not r['ambiguous'] and not r['unsupported_composition']]
    if not eligible:
        raise ValueError('no unambiguous supported evaluation requests')
    keys = ['baseline', 'rules', 'linear', 'constant']
    summary = {key: old.metrics(eligible, key) for key in keys}
    summary['repositories'] = {repo: {key: old.metrics([r for r in eligible if r['repository'] == repo], key)
                                     for key in keys} for repo in sorted({r['repository'] for r in eligible})}
    support = {label: sum(label in r['labels'] for r in eligible) for label in old.ALL}
    mixed_families = sorted({r['family'] for r in eligible if len(r['labels']) > 1})
    grouped = {}
    for row in eligible:
        grouped.setdefault((row['repository'], row['family']), []).append(row)
    families = sorted(grouped)
    ci = {}
    for key in keys[1:]:
        rng = random.Random(42)
        deltas = []
        for _ in range(2000):
            sample = [r for group in rng.choices(families, k=len(families)) for r in grouped[group]]
            deltas.append(sum(int(set(r[key]) == set(r['labels'])) - int(set(r['baseline']) == set(r['labels'])) for r in sample)/len(sample))
        ci[key] = np.percentile(deltas, [2.5, 97.5]).tolist()
    summary.update(support=support, mixed_families=mixed_families,
                   coverage_gate=all(support[label] >= 5 for label in old.LABELS) and len(mixed_families) >= 5,
                   excluded=[{'id': r['id'], 'ambiguous': r['ambiguous'], 'unsupported_composition': r['unsupported_composition']}
                             for r in records if r not in eligible],
                   delta_ci95=ci, corpus_rows=len(records), families=len(families),
                   serialized_load_seconds=load_seconds, imports_and_mechanism_seconds=imports_end-START,
                   process_peak_rss_bytes=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
                   timings={key: {'first': records[0][key], 'p50': float(np.percentile([r[key] for r in records[1:]],50)),
                                  'p95': float(np.percentile([r[key] for r in records[1:]],95))}
                            for key in ['rpc_ms','rules_ms','linear_ms']},
                   performance_validated=False)
    save(out/'summary.json', summary)


if __name__ == '__main__':
    main()
