# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Measure autonomous graph freshness while a writer churns ignored .pixel/ files.

Use the same command with the baseline and candidate binaries. JSONL records
three repetitions by default. The writer waits 25 ms between writes. Observation
uses database file timestamps and read-only SQLite, with no daemon requests
after the source edit. Two quiet edits separate startup catch-up from the watcher.
The cap is a censored observation, not a claim about eventual completion.
"""
import argparse, json, os, pathlib, sqlite3, subprocess, tempfile, threading, time

def run(binary, label, repetition, cap):
    with tempfile.TemporaryDirectory(prefix='pixel-debounce-measure-') as scratch:
        base = pathlib.Path(scratch)
        root = base / 'repo'
        root.mkdir()
        (base / 'home').mkdir()
        source = root / 'source.ts'
        source.write_text('export function baseline() { return 1; }\n')
        (root / '.gitignore').write_text('.pixel/\n')
        env = dict(os.environ, HOME=str(base / 'home'), GIT_CONFIG_GLOBAL='/dev/null', GIT_AUTHOR_NAME='Test', GIT_COMMITTER_NAME='Test', GIT_AUTHOR_EMAIL='test@example.com', GIT_COMMITTER_EMAIL='test@example.com', PIXEL_DAEMON_AUTO_START='0', PIXEL_METRICS='0')
        for args in [['git', 'init', '-q'], ['git', 'add', '.'], ['git', 'commit', '-qm', 'fixture']]:
            subprocess.run(args, cwd=root, env=env, check=True, stdout=subprocess.DEVNULL)
        subprocess.run([binary, 'prepare-repo', str(root), '--no-daemon', '--json'], cwd=root, env=env, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=60)
        graph = root / '.pixel/graph.v2.db'

        def version():
            return tuple(((p.stat().st_mtime_ns, p.stat().st_size) if p.exists() else None for p in [graph, pathlib.Path(str(graph) + '-wal')]))

        def wait_symbol(name, deadline):
            previous = None
            while time.monotonic() < deadline:
                current = version()
                if current != previous:
                    previous = current
                    with sqlite3.connect('file:' + str(graph) + '?mode=ro', uri=True, timeout=1) as db:
                        if db.execute('select count(*) from symbols where name=?', (name,)).fetchone()[0] == 1:
                            return time.monotonic()
                threading.Event().wait(0.01)
            return None
        log = open(base / 'daemon.log', 'w')
        daemon = subprocess.Popen([binary, 'daemon', 'start', str(root), '--foreground'], cwd=root, env=env, stdout=log, stderr=log)
        stop = threading.Event()
        ready = threading.Event()
        writes = [0]

        def noise():
            while not stop.is_set():
                (root / '.pixel/noise').write_text(str(writes[0]))
                writes[0] += 1
                ready.set()
                stop.wait(0.025)
        worker = None
        try:
            source.write_text('export function warmup() { return 2; }\n')
            warm_started = time.monotonic()
            warm = wait_symbol('warmup', warm_started + 10)
            if warm is None:
                raise RuntimeError('watcher failed its quiet readiness edit')
            source.write_text('export function settled() { return 2; }\n')
            warm_started = time.monotonic()
            warm = wait_symbol('settled', warm_started + 10)
            if warm is None:
                raise RuntimeError('watcher failed its second quiet readiness edit')
            worker = threading.Thread(target=noise)
            worker.start()
            assert ready.wait(2)
            started = time.monotonic()
            source.write_text('export function measured() { return 3; }\n')
            fresh = wait_symbol('measured', started + cap)
            count = writes[0]
            stop.set()
            worker.join(timeout=2)
            stopped = time.monotonic()
            recovered = fresh if fresh is not None else wait_symbol('measured', stopped + 5)
            return dict(label=label, repetition=repetition, cap_s=cap, quiet_ready_ms=round((warm - warm_started) * 1000, 1), fresh_while_writing_ms=None if fresh is None else round((fresh - started) * 1000, 1), fresh_after_stop_ms=None if fresh is not None or recovered is None else round((recovered - stopped) * 1000, 1), writes=count)
        finally:
            stop.set()
            if worker:
                worker.join(timeout=2)
            try:
                subprocess.run([binary, 'daemon', 'stop', str(root)], cwd=root, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
            except subprocess.TimeoutExpired:
                daemon.terminate()
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait()
            log.close()
parser = argparse.ArgumentParser()
parser.add_argument('binary')
parser.add_argument('label')
parser.add_argument('--reps', type=int, default=3)
parser.add_argument('--cap', type=float, default=5)
args = parser.parse_args()
for i in range(args.reps):
    print(json.dumps(run(str(pathlib.Path(args.binary).resolve()), args.label, i + 1, args.cap)), flush=True)
