# Judged Rust edges: this resolver against pixel-mvp's index

What the graph holds of 160 judged Rust edges of this repository at
`85bede7d` (`eval/graph-edges/`), with each Rust source.

```bash
git clone https://github.com/Pixel-CLI/pixel /tmp/pixel-85 && git -C /tmp/pixel-85 checkout 85bede7d
cargo build --profile dev-release -p pixel-cli
python3 scripts/bench-graph-edges.py --pixel target/dev-release/pixel --repo /tmp/pixel-85
```

Run on 2026-10-14, the binary built from this stack's last pull request
(pixel-mvp `d5c7cf7`), `dev-release` profile, macOS on Apple Silicon:

| Source | Correct edges held | Wrong edges held |
| --- | ---: | ---: |
| `v1` (this resolver) | 29 of 38 | 72 of 72 |
| pixel-mvp (default) | 35 of 38 | 0 of 72 |

"Of 38" and "of 72": the judged edges whose caller and callee both exist in
the graph; the other 50 are pixel-mvp edges from Rust test code, which the
graph does not extract. The script exits 1 when pixel-mvp holds a judged-wrong
edge.

The comparison the set comes from (pixel-mvp project, 2026-10-14, same
commit) estimated, on the whole population and with 95 % Wilson intervals:
this resolver's Rust call precision 52.8 % (51.1–57.4), pixel-mvp's at least
93.9 % (0 wrong of 59 judged calls), and pixel-mvp holding 95.0 % (87.5–98.2)
of this resolver's correct call edges. `pixel graph-shadow` gives the
population counts on any tree.
