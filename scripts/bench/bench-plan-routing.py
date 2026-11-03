#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Isolated, frozen sparse PlanQuery pilot. Never changes Pixel routing."""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import re
import resource
import socket
import sys
import threading
import time

START = time.perf_counter()
LABELS = ['dead-interactive', 'dead-code', 'hotspots', 'recent-changes']
ALL = LABELS + ['by-concept']
BASE = '5c894b1b3a5db713615d906dc8604805c73813ae'
SOURCE_HASH = 'f702761ba5141608f311f1832db1f2ef0abbec190ea1e088fe4f857731e1f9a1'


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def save(path, value):
    with Path(path).open('x') as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write('\n')


def baseline(text):
    # Name-set transcription of plan.rs; tag variants intentionally collapse.
    words = re.findall(r'[^\W_]+', text.lower())
    def has(word):
        return any(w == word or (w.endswith('s') and w[:-1] == word) for w in words)
    out = set()
    if any(has(w) for w in ['clickable', 'interactive', 'button', 'link', 'navigation', 'click']):
        out.add('dead-interactive')
    if ' dead code ' in ' ' + ' '.join(words) + ' ' or has('unused') or has('remove'):
        out.add('dead-code')
    if any(has(w) for w in ['refactor', 'hotspot', 'priority']):
        out.add('hotspots')
    if any(has(w) for w in ['recent', 'bug', 'regression']):
        out.add('recent-changes')
    return sorted(out or {'by-concept'})


def decode(probabilities):
    if len(probabilities) != 4 or any(not 0 <= p <= 1 for p in probabilities):
        raise ValueError('expected four finite binary probabilities')
    return sorted([label for label, p in zip(LABELS, probabilities) if p >= 0.5] or ['by-concept'])


def validate(rows):
    ids, texts = set(), set()
    for row in rows:
        if row['id'] in ids or not row['text'].strip():
            raise ValueError('duplicate ID or empty task')
        norm = ' '.join(re.findall(r'[^\W_]+', row['text'].lower()))
        if norm in texts:
            raise ValueError('duplicate normalized task')
        ids.add(row['id'])
        texts.add(norm)
        labels = row['labels']
        if not labels or len(set(labels)) != len(labels) or set(labels) - set(ALL):
            raise ValueError('invalid labels')
        for key in ['family', 'split', 'repository', 'commit', 'evidence', 'provenance']:
            if not row.get(key):
                raise ValueError('missing provenance: ' + key)


def train_rows(path):
    with Path(path).open() as stream:
        raw = list(csv.DictReader(stream, delimiter='\t'))
    rows = [dict(id=f'train-{i:03}', family=r['family'], text=r['text'],
                 labels=r['labels'].split(','), split='train', repository='Pixel-CLI/pixel',
                 commit=BASE, evidence='plan.rs:19-29 query semantics; protocol operational gold policy',
                 provenance='parent-authored synthetic scenario; not production traffic')
            for i, r in enumerate(raw)]
    validate(rows)
    return rows


def fit(args):
    import joblib
    import numpy as np
    from sklearn.feature_extraction.text import TfidfVectorizer
    from sklearn.linear_model import LogisticRegression
    from sklearn.multiclass import OneVsRestClassifier
    from sklearn.pipeline import FeatureUnion, Pipeline
    out = Path(args.output)
    out.mkdir(exist_ok=False, parents=True)
    rows = train_rows(args.train)
    features = FeatureUnion([
        ('word', TfidfVectorizer(ngram_range=(1, 2), sublinear_tf=True)),
        ('char', TfidfVectorizer(analyzer='char_wb', ngram_range=(3, 5), sublinear_tf=True))])
    model = Pipeline([('features', features), ('classifier', OneVsRestClassifier(
        LogisticRegression(C=1.0, class_weight='balanced', solver='liblinear',
                           max_iter=1000, random_state=42), n_jobs=1))])
    y = np.array([[int(label in row['labels']) for label in LABELS] for row in rows])
    before = time.perf_counter()
    model.fit([r['text'] for r in rows], y)
    elapsed = time.perf_counter() - before
    joblib.dump(model, out / 'model.joblib')
    save(out / 'train.json', rows)
    save(out / 'fit.json', dict(seconds=elapsed, startup_import_seconds=before-START,
         process_peak_rss_bytes=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
         rss_platform=platform.system(), rows=len(rows), families=len({r['family'] for r in rows}),
         features=len(model['features'].get_feature_names_out()),
         max_iterations=[int(e.n_iter_[0]) for e in model['classifier'].estimators_],
         model_sha256=digest(out / 'model.joblib'), script_sha256=digest(__file__),
         protocol_sha256=digest('docs/bench/plan-routing-pilot-protocol.md'),
         train_sha256=digest(args.train), source_sha256=digest('crates/pixel-graph/src/plan.rs'),
         python=sys.version, command=sys.argv, platform=platform.platform(), base=BASE))


def rpc(sock_path, text):
    request = {'op': 'plan', 'prompt': text}
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(60)
        sock.connect(sock_path)
        sock.sendall((json.dumps(request) + '\n').encode())
        with sock.makefile('rb') as stream:
            raw = stream.readline(8 * 1024 * 1024)
    answer = json.loads(raw)
    if not answer['ok'] or answer.get('error') is not None:
        raise ValueError('Pixel plan failed: ' + str(answer))
    return request, answer, sorted(set(answer['result']['queries']))


def metrics(rows, key):
    counts = {label: {'tp': 0, 'fp': 0, 'fn': 0} for label in ALL}
    correct = 0
    errors = []
    for row in rows:
        gold, pred = set(row['labels']), set(row[key])
        correct += gold == pred
        if gold != pred:
            errors.append(dict(id=row['id'], missing=sorted(gold-pred), extra=sorted(pred-gold)))
        for label in ALL:
            counts[label]['tp'] += int(label in gold and label in pred)
            counts[label]['fp'] += int(label not in gold and label in pred)
            counts[label]['fn'] += int(label in gold and label not in pred)
    f1 = [2*c['tp']/(2*c['tp']+c['fp']+c['fn']) if 2*c['tp']+c['fp']+c['fn'] else 0
          for c in counts.values()]
    return dict(n=len(rows), correct=correct, exact_set=correct/len(rows),
                macro_f1=sum(f1)/5, per_label=counts, errors=errors,
                weighted_cost=sum(c['fp']+2*c['fn'] for c in counts.values())/len(rows),
                omissions=sum(c['fn'] for c in counts.values()))


def evaluate(args):
    import joblib
    import numpy as np
    from difflib import SequenceMatcher
    model_dir, out = Path(args.model), Path(args.output)
    out.mkdir(exist_ok=False, parents=True)
    fit_info = json.loads((model_dir / 'fit.json').read_text())
    if digest(model_dir/'model.joblib') != fit_info['model_sha256']:
        raise ValueError('frozen model changed after fit')
    if digest('crates/pixel-graph/src/plan.rs') != SOURCE_HASH:
        raise ValueError('baseline source changed')
    train = json.loads((model_dir/'train.json').read_text())
    test = [json.loads(line) for line in Path(args.test).read_text().splitlines() if line.strip()]
    validate(train + test)
    if any(r['split'] != 'test' for r in test):
        raise ValueError('test has wrong split')
    overlap = {r['family'] for r in train} & {r['family'] for r in test}
    if overlap:
        raise ValueError('family overlap: ' + str(overlap))
    near = []
    for row in test:
        nearest = max(train, key=lambda t: SequenceMatcher(None, row['text'].lower(), t['text'].lower()).ratio())
        similarity = SequenceMatcher(None, row['text'].lower(), nearest['text'].lower()).ratio()
        a, b = set(row['text'].lower().split()), set(nearest['text'].lower().split())
        near.append(dict(test=row['id'], train=nearest['id'], character_similarity=similarity,
                         token_jaccard=len(a & b)/len(a | b)))
    save(out/'freeze.json', dict(test_sha256=digest(args.test), fit=fit_info,
         script_sha256=digest(__file__), command=sys.argv, near_pairs=near,
         protocol_sha256=digest('docs/bench/plan-routing-pilot-protocol.md'),
         performance_validated=False, reason='quality run only; no dedicated idle-host latency campaign'))
    before = time.perf_counter()
    model = joblib.load(model_dir/'model.joblib')
    load_seconds = time.perf_counter()-before
    records = []
    with (out/'raw.jsonl').open('x') as stream:
        for row in train + test:
            t = time.perf_counter()
            port = baseline(row['text'])
            port_ms = 1000*(time.perf_counter()-t)
            t = time.perf_counter()
            request, response, reference = rpc(args.socket, row['text'])
            rpc_ms = 1000*(time.perf_counter()-t)
            record = dict(row, request=request, response=response, baseline=reference,
                          baseline_port=port, port_ms=port_ms, rpc_ms=rpc_ms)
            if port != reference:
                stream.write(json.dumps(record)+'\n')
                stream.flush()
                raise ValueError('baseline parity failure: ' + row['id'])
            if row['split'] == 'test':
                t = time.perf_counter()
                probabilities = model.predict_proba([row['text']])[0].tolist()
                record.update(probabilities=probabilities, candidate=decode(probabilities),
                              resident_ms=1000*(time.perf_counter()-t))
                records.append(record)
            stream.write(json.dumps(record)+'\n')
            stream.flush()
    ref, candidate = metrics(records, 'baseline'), metrics(records, 'candidate')
    families = sorted({r['family'] for r in records})
    grouped = {f: [r for r in records if r['family'] == f] for f in families}
    rng, deltas = random.Random(42), []
    for _ in range(2000):
        sampled = [r for f in rng.choices(families, k=len(families)) for r in grouped[f]]
        deltas.append(sum(int(set(r['candidate']) == set(r['labels']))-
                          int(set(r['baseline']) == set(r['labels'])) for r in sampled)/len(sampled))
    risks = []
    for threshold in [0.5, 0.7, 0.9]:
        selected = [r for r in records if min(max(p, 1-p) for p in r['probabilities']) >= threshold]
        risks.append(dict(threshold=threshold, n=len(selected), coverage=len(selected)/len(records),
                          risk=(sum(set(r['candidate']) != set(r['labels']) for r in selected)/len(selected)
                                if selected else None)))
    brier = sum((p-int(label in r['labels']))**2 for r in records
                for label, p in zip(LABELS, r['probabilities']))/(len(records)*4)
    timing = {key: dict(first=records[0][key], p50=float(np.percentile([r[key] for r in records[1:]],50)),
                       p95=float(np.percentile([r[key] for r in records[1:]],95)))
              for key in ['port_ms', 'rpc_ms', 'resident_ms']}
    save(out/'summary.json', dict(baseline=ref, candidate=candidate, families=len(families),
         delta_ci95=np.percentile(deltas,[2.5,97.5]).tolist(), brier=brier, risk_coverage=risks,
         wins=sum(set(r['candidate']) == set(r['labels']) and set(r['baseline']) != set(r['labels']) for r in records),
         losses=sum(set(r['candidate']) != set(r['labels']) and set(r['baseline']) == set(r['labels']) for r in records),
         serialized_load_seconds=load_seconds, import_and_validation_seconds=before-START,
         observed_timings=timing, performance_validated=False,
         process_peak_rss_bytes=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
         rss_platform=platform.system(), parity_rows=len(train)+len(test),
         strata={s: {k: metrics([r for r in records if r['stratum'] == s], k)
                     for k in ['baseline', 'candidate']} for s in sorted({r['stratum'] for r in records})},
         novel_combinations={k: metrics([r for r in records if r['family'] in ['F06', 'F07', 'F08', 'F20']], k)
                             for k in ['baseline', 'candidate']}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='mode', required=True)
    fit_parser = sub.add_parser('fit')
    fit_parser.add_argument('--train', required=True)
    fit_parser.add_argument('--output', required=True)
    ev = sub.add_parser('evaluate')
    for key in ['model', 'test', 'output', 'socket']:
        ev.add_argument('--'+key, required=True)
    args = parser.parse_args()
    # macOS reports ru_maxrss in bytes; keep that platform restriction explicit.
    if platform.system() != 'Darwin':
        parser.error('RSS accounting in this bounded pilot requires macOS')
    resource.setrlimit(resource.RLIMIT_CPU, (900, 900))
    def watchdog():
        while True:
            if time.perf_counter()-START > 900 or resource.getrusage(resource.RUSAGE_SELF).ru_maxrss > 4*1024**3:
                print('pilot resource budget exceeded', file=sys.stderr, flush=True)
                os._exit(124)
            time.sleep(0.05)
    threading.Thread(target=watchdog, daemon=True).start()
    for key in ['OMP_NUM_THREADS', 'OPENBLAS_NUM_THREADS', 'MKL_NUM_THREADS', 'VECLIB_MAXIMUM_THREADS']:
        os.environ[key] = '1'
    (fit if args.mode == 'fit' else evaluate)(args)


if __name__ == '__main__':
    main()
