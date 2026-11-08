# Impact comparison

Run from the repository root, with two committed revisions:

```sh
python3 scripts/bench/impact/compare.py --base BASE_SHA --candidate HEAD --output /tmp/impact-results
```

Use a new output directory so previous evidence cannot be overwritten.
Requires macOS or Linux, Cargo, Python 3.10+, and the workspace's cached Cargo dependencies
(`cargo fetch --locked` if needed). Each arm compiles the exact `impact.rs`
from its revision into one optimized binary, using the current checkout's
store and dependency versions. This isolates changes to impact; it is not a
benchmark of changes to storage, indexing, the daemon, or process startup.
The generated Cargo project lives outside the checkout; its build cache is
`target/impact-bench`. No graph files or installed binaries are modified.

Ten in-memory SQLite fixtures cover empty/single-caller graphs, short chains, wide and branching
graphs, duplicate call sites, diamonds, cached flows, depth 1 and downstream
traversal. Each query includes JSON serialization and the 20-item output cap.
Setup is excluded. Prepared-statement caches are cleared per query to prevent
carryover between arms; the connection and SQLite pages stay warm. The harness checks count/file parity and SQL statement
counts separately from timing, warms both arms, and alternates their order
across 41 paired batches of approximately 20 ms per arm. Reported p50/p95
are percentiles of batch-average query times, not individual tail latency.

Each batch records wall time and this thread's CPU time. CPU time helps
diagnose scheduling interference without dropping a wall-time regression.
The exit status rejects either one-sided 95% paired log-ratio interval whose
upper bound exceeds 1.03 (t critical value 1.685, 40 degrees of freedom).
This is a declared 3% regression sensitivity, not a claim of identical
execution time. Keep `samples.jsonl`, `results.json` and `build.log`: the
results identify the revisions, source hashes, binary hash and compiler.
Run with the same revision in both arms to check measurement noise.
