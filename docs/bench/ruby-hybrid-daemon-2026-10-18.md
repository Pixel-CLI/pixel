# The Ruby hybrid in the daemon, replayed on main (2026-10-18)

Refs #1040. A rerun of pixel-research-mvp's
`evaluation/results/2026-10-16-ruby-hybrid-daemon.md` (commit `69dbba4`)
on current main, with the same script, edits, prompts and gumroad pin, to
measure separately what #1040 asks for before splitting it: lost briefs,
refresh cost, and daemon serving latency. No agent ran.

## Verdict

The hybrid still loses every brief sent 1 s after an edit on gumroad
(0/20, as on 2026-10-16), and its graph is fresh after 6.9 s (p50) against
v1's 2.4 s, over the 5 s bar for 16 of 20 edits. The default stays off.

| Bar (gumroad) | 2026-10-16, `0311e626`: v1 / hybrid | 2026-10-18, `d102ca5b`: v1 / hybrid |
| --- | --- | --- |
| briefs with content, prompt 1 s after the edit | 20/20 / **0/20** | 20/20 / **0/20** |
| hook p95, prompt 1 s after the edit | 2 774 / 3 122 ms | 2 305 / 3 029 ms |
| hook p95, prompts 3 s and 5 s after | 924–976 / 1 152–3 024 ms | 1 316 / 2 490 ms |
| graph fresh after the edit, watcher path, p50 / p95 | 3.29 / 4.68 s / **8.00 / 9.89 s** | 2.39 / 2.62 s / **6.90 / 7.51 s** |
| `find-symbol` right after the write, p50 / p95 | 2.81 / 3.40 s / 7.65 / 9.44 s | 2.14 / 2.51 s / 6.46 / 7.40 s |
| edits over 5 s (watcher path, of 20) | 1 / 20 | **0 / 16** |
| daemon CPU s per edit (median, watcher path) | 3.34 / 8.46 | 1.92 / 6.24 |
| daemon peak RSS | 617 MB / 1 707 MB | 1 264 MB / 2 730 MB |
| `prepare-repo` cold | 6.6 / 13.7 s | 5.5 / 11.8 s |

## The three units #1040 names

**(a) Serving responsiveness: not solved.** Every hybrid prompt sent 1 s
after an edit reached the daemon (`route: daemon` in all 20 decision rows)
and got no answer inside the 3 s window (`answered: 0`): the hook prints an
empty brief and its latency sits at the window (p50 3 015 ms). v1's update
fits in the window (p50 1 987 ms at 1 s, 20/20 briefs) but still adds about
1.5 s to a prompt that follows an edit. #1087's read replicas did not take
the update off this prompt's path.

**(b) Incremental refresh cost: improved, still over the bar.** One
inserted method costs the hybrid 6.2 CPU s and 6.9 s to freshness (v1 1.9
CPU s, 2.4 s), down from 8.5 CPU s and 8.0 s on 2026-10-16 under a heavier
machine load. Peak RSS is higher on both arms than on 2026-10-16.

**(c) Resolver correctness: not measured here.** MVP #111's edge counts
predate the fixes since; nothing in this run reassesses them.

## A watcher bug found on the way (#1194)

A first replay ran from clones under `~/.cache/`. There, no edit reached
the index or the graph in 60 s on either arm, and the hybrid's briefs all
came back (20/20): nothing was updating, so nothing blocked the hook.
`record_event` matched the ignored directory names (`.cache`, `build`,
`vendor`, …) against the event's absolute path, so a repository under such
a directory got no watcher update at all. Fixed by #1195. The numbers above
come from the second replay, from clones under `$TMPDIR` (no ignored name
in the path), with the unfixed binary.

## Pins

| What | Pin |
| --- | --- |
| Pixel | main `d102ca5b`, `cargo build --release --locked -p pixel-cli` |
| gumroad | `b996417e6fe55b984a8a36b1a2780ef0ca742724`, `git fetch --depth 1`, one copy per arm (`cp -c`) |
| Machine | Apple Silicon, macOS 27.0, shared: 1-minute load 3.1 to 9.6 (median 5.5) |

## Method

```sh
D=docs/bench/ruby-hybrid-daemon-2026-10-18
python3 -I $D/daemonbench.py <pixel> <v1-clone> <hybrid-clone> <out> 20 \
  app/models/link.rb app/models/purchase.rb app/models/user.rb \
  app/models/subscription.rb app/controllers/links_controller.rb
python3 -I $D/summarize.py <out>
```

The script is the 2026-10-16 one, unchanged apart from its SPDX header:
each arm runs `prepare-repo`, then 20 alternated cycles of an inserted
method followed by a hook prompt at 1.0, 3.1 and 5.2 s, then 20 edits per
arm watched passively (graph database stat) and 20 actively
(`find-symbol`). The hybrid arm carries `PIXEL_GRAPH_SOURCE_RUBY=hybrid`.
`runs/gumroad/events.jsonl` holds the rows (no prompt text, no paths),
`runs/gumroad/summary.txt` the summary.

## What is not verified

- **Shallow clones.** The 2026-10-16 run used full clones; the graph does
  not read history, but `prepare-repo`'s history index saw one commit.
- **One machine, shared**, lighter load than on 2026-10-16 (median 5.5
  against 8.4): the absolute times are not comparable one to one; the
  arms alternated so both saw the same load.
- **once-campfire** was not rerun.
- **Why (a) still fails** is not diagnosed: whether the replica refuses a
  lagging graph, the writer flushes the pending batch first, or the brief's
  daemon calls queue behind the update.
