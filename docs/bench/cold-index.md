# Cold index of a large repository

Measured 2026-09-27 on an Apple M2 (8 cores), macOS, on a private Rails
application of 18,782 tracked files.

## What is measured

The first Pixel command on a repository builds everything from nothing: the
text index from the blobs of HEAD's tree, then the code graph. Each run below
starts from a fresh local clone with no `.pixel/` and an empty shard cache
(`XDG_CACHE_HOME` pointed at a new directory), so nothing is reused:

```bash
rm -rf .pixel
XDG_CACHE_HOME="$(mktemp -d)" pixel prepare-repo --no-daemon --metrics off --json \
  | jq -c '{total: .timings.total_ms, index: .timings.index.base_ms, graph: .timings.graph.elapsed_ms, symbols: .graph.symbols}'
```

`index` is the text index (`timings.index.base_ms`, with `base` reading
`built_from_git`), `graph` the full graph build, `total` the whole command.

## Results

| Cold, from nothing | Run 1 | Run 2 | Run 3 | Median |
| --- | --- | --- | --- | --- |
| Text index | 2,013 ms | 2,097 ms | 1,676 ms | 2,013 ms |
| Code graph | 10,684 ms | 9,403 ms | 9,526 ms | 9,526 ms |
| Whole command | 13,386 ms | 12,021 ms | 11,765 ms | 12,021 ms |

The graph holds 59,609 symbols on every run.

## Weekly CI tripwire

`.github/workflows/perf.yml` runs the same command every Monday on the
public `excalidraw/excalidraw` corpus pinned in `.github/perf-baseline.json`
(708 files, 3,937 symbols), from nothing and then after one appended line.
On a GitHub `ubuntu-26.04` runner, the highest of three runs on 2026-10-19:

| Phase | Cold | Incremental |
| --- | --- | --- |
| Text index | 387 ms | — |
| Code graph | 2,145 ms | 39 ms |
| Whole command | 3,029 ms | 71 ms |

The lane fails a run at three times these numbers. Refreshing them is a pull
request that edits `.github/perf-baseline.json` and this table together.

## Caveats

- One repository, one machine; re-run the command above on yours.
- A warm start is a different number: with `.pixel/` restored, later commands
  reuse the text index and re-read only the files that changed, and the graph
  updates incrementally.
