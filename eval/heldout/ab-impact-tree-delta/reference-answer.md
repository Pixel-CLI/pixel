No: production code does not call the uncached `tree_delta`. The daemon uses `tree_delta_cached` (`crates/pixel-daemon/src/api.rs` `ensure_graph_inner` via `bridge::tree_delta_cached`); both share `tree_delta_with`.

`tree_delta` is called only by the bench `crates/pixel-bench/benches/tree_delta.rs` (lines 34, 132, 150) and by pixel-graph's own unit tests in `crates/pixel-graph/src/build.rs`. Deleting it breaks that bench and those tests, nothing shipped.
