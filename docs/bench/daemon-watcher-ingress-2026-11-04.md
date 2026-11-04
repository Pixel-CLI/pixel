# Daemon watcher ingress retention

## Measurement

The baseline is upstream `main` at `9dd036279766e1454a135543e3c1566f9fbc97ea`.
The candidate is that base plus the Rust diff with SHA-256
`43620c973429bee6b0bc7b23d61c794b4e849af99603bec807936bb07fb68ae9`.
The runs were on `Darwin 27.0.0 arm64`.

To reproduce the source identity on this branch, run:

```sh
rtk proxy git diff --binary 9dd036279766e1454a135543e3c1566f9fbc97ea -- crates/pixel-daemon/src/daemon.rs crates/pixel-daemon/src/daemon/contract_tests.rs crates/pixel-daemon/src/recall_service.rs | rtk proxy shasum -a 256
```

Both arms ran the same ignored unit-test workload in three fresh test-binary
processes: 200,000 synthetic `notify::Event` values, each with one distinct
path. The baseline scratch checkout appended a measurement-only test that
used the exact old forwarding callback operation, `tx.send(Msg::Fs(event))`.
The candidate test calls the production `WatchIngress::event` method used by
the notify callback. The event values and path strings were generated the same
way in each arm. The baseline harness was temporary and is not part of the
baseline commit.

Baseline build, after adding the temporary test shown in the appendix:

```sh
rtk env CARGO_TARGET_DIR=/tmp/pixel-daemon-memory-20261104/target CARGO_BUILD_JOBS=2 cargo test -p pixel-daemon --lib --no-run
```

Candidate build:

```sh
rtk env CARGO_TARGET_DIR=/tmp/pixel-daemon-memory-candidate-target-20261104 CARGO_BUILD_JOBS=2 cargo test -p pixel-daemon --lib --no-run
```

Cargo printed these test executables, whose locations are also found by the
exact discovery commands below:

- Baseline: `/tmp/pixel-daemon-memory-20261104/target/debug/deps/pixel_daemon-94e8a498e4bc9de0`
- Candidate: `/tmp/pixel-daemon-memory-candidate-target-20261104/debug/deps/pixel_daemon-94e8a498e4bc9de0`

The executable discovery commands were:

```sh
rtk find /tmp/pixel-daemon-memory-20261104/target/debug/deps -maxdepth 1 -type f -name 'pixel_daemon-*' -perm -111
rtk find /tmp/pixel-daemon-memory-candidate-target-20261104/debug/deps -maxdepth 1 -type f -name 'pixel_daemon-*' -perm -111
```

The fresh-process run commands, each repeated three times, were:

```sh
rtk env /usr/bin/time -l /tmp/pixel-daemon-memory-20261104/target/debug/deps/pixel_daemon-94e8a498e4bc9de0 --exact daemon::tests::watcher_legacy_memory_measurement --ignored --nocapture
rtk env /usr/bin/time -l /tmp/pixel-daemon-memory-candidate-target-20261104/debug/deps/pixel_daemon-94e8a498e4bc9de0 --exact daemon::tests::watcher_ingress_memory_measurement --ignored --nocapture
```

Cargo was not included in the timed processes.

| Measurement | Baseline | Candidate |
| --- | ---: | ---: |
| Events passed through callback path | 200,000 | 200,000 |
| Raw path-string bytes generated | 23,688,890 | 23,688,890 |
| Retained event/path entries after burst | 200,000 | 0 paths |
| Retained path-string bytes after burst | 23,688,890 | 0 |
| Loop wake messages | 200,000 | 1 |
| Run 1 elapsed / max RSS | 106 ms / 120,995,840 B | 1,017 ms / 11,059,200 B |
| Run 2 elapsed / max RSS | 360 ms / 120,995,840 B | 739 ms / 11,042,816 B |
| Run 3 elapsed / max RSS | 116 ms / 121,028,608 B | 1,841 ms / 11,026,432 B |
| Median elapsed | 116 ms | 1,017 ms |
| Maximum RSS range | 120,995,840–121,028,608 B | 11,026,432–11,059,200 B |

The maximum RSS range is about 90.9% lower for this isolated event-retention
workload. The candidate median elapsed time was 8.8 times the baseline median
in these runs, with substantial scheduling variance in both arms. The
measurement includes constructing event values and executing the respective
callback ingestion path. It does not measure a live OS watcher, corpus scan
latency, total daemon RSS, or long-running daemon behavior.

## Scope of the bound

The implementation caps the callback ingress map, loop debounce buffer, and
file paths retained while extending a watch through a newly created subtree at
4,096 distinct paths. The subtree walk continues establishing directory
watches after its file-list cap, then overflow clears deltas and requests an
authoritative corpus rescan. The pruned watcher repairs only directory paths
whose device/inode identity changed. Regression tests cover backend overflow
with no paths, repeated ingress drains overflowing the debounce buffer, an
oversized created subtree, changes arriving during rescan, index and graph
freshness after edit/rename/delete, and a recreated directory receiving a
later live edit.

The writer's shared ordered job queue is unchanged and remains unbounded. This
measurement and implementation do not establish a hard bound on total daemon
memory under a writer backlog.

## Baseline harness appendix

To reproduce the baseline arm, create a detached worktree from the pinned
commit, then insert this test inside
`crates/pixel-daemon/src/daemon.rs`'s existing `mod tests`, before its closing
brace:

```sh
rtk git worktree add --detach /tmp/pixel-daemon-memory-baseline-20261104 9dd036279766e1454a135543e3c1566f9fbc97ea
```

The test uses the old callback's `Msg::Fs` channel operation; the measured
implementation remains the pinned baseline source.

```rust
#[test]
#[ignore = "manual baseline/candidate watcher retention measurement"]
fn watcher_legacy_memory_measurement() {
    use notify::event::{DataChange, ModifyKind};

    const EVENT_COUNT: usize = 200_000;
    let (_scratch, root) = scratch_root("watcher-memory-measurement");
    let (tx, rx) = mpsc::channel();
    let mut path_bytes = 0;
    let started = Instant::now();
    for index in 0..EVENT_COUNT {
        let path = root.join(format!("burst-{index}.rs"));
        path_bytes += path.as_os_str().to_string_lossy().len();
        let event = notify::Event::new(notify::EventKind::Modify(ModifyKind::Data(DataChange::Any)))
            .add_path(path);
        tx.send(Msg::Fs(event)).unwrap();
    }
    let elapsed = started.elapsed();
    let queued_messages = rx.try_iter().count();
    eprintln!(
        "watcher-memory baseline events={EVENT_COUNT} raw_path_bytes={path_bytes} retained_paths={queued_messages} retained_path_bytes={path_bytes} wake_messages={queued_messages} elapsed_ms={}",
        elapsed.as_millis()
    );
    assert_eq!(queued_messages, EVENT_COUNT);
}
```

Then build with the baseline command above and run its exact executable three
times with `--exact daemon::tests::watcher_legacy_memory_measurement --ignored
--nocapture`. The candidate harness is included in the candidate Rust diff and
is run with `--exact daemon::tests::watcher_ingress_memory_measurement
--ignored --nocapture`.
