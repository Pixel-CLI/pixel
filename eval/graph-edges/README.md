# Judged Rust edges

160 call and reference edges of this repository at `85bede7d`, each judged
`correct` or `wrong` against the source, for
[`scripts/bench-graph-edges.py`](../../scripts/bench-graph-edges.py): which of
them the graph holds with each Rust source (`PIXEL_GRAPH_SOURCE_RUST=v1`,
this graph's own resolver, or pixel-mvp's index, the default). Results and
how to reproduce them: [`docs/bench/graph-edges.md`](../../docs/bench/graph-edges.md).

The edges are the random samples (seed 20261014) of a comparison of the two
graphs made on 2026-10-14 by the pixel-mvp project: 60 call and 20 reference
edges only this graph's resolver linked, 40 call and 20 reference edges only
pixel-mvp linked, 20 call edges both linked. Each was judged by one reader of
the source at that commit: `correct` when Rust dispatches to, or names,
exactly that definition as far as the code shows; `wrong` when something else
is called (a std method, another type's). An edge to the only definition of
a name counts as correct only when the receiver is shown to be that type.

| Column | Meaning |
| --- | --- |
| `id` | `rs-<stratum>-NN`, stable |
| `kind` | `call` or `reference` |
| `src_path`, `src` | the caller: file and qualified name as the graph stores them |
| `dst_path`, `dst` | the callee |
| `line` | the site's line in `src_path` |
| `verdict`, `reason` | the judgment and its one-line reason |

Most edges pixel-mvp alone linked start in Rust test code, which this graph
does not extract: the bench scores only the edges whose two ends the graph
has, and says how many that is.
