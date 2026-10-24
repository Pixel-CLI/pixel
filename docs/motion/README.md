# motion

[Remotion](https://www.remotion.dev) sources for the animations in the root
README and on the website, all rendered into `docs/examples/`. They share the
website's identity (`src/theme.ts`: navy ground, orange for the cost that still happens,
indigo for what PIXEL hands back, Handjet for display text);
`src/fonts.ts` loads the three faces from `public/fonts/` before any frame
renders.

| Composition ID | Source | Output basename |
|---|---|---|
| `PIXELvsRTK` | `src/Turns.tsx` | `pixel-vs-rtk` |
| `EditGuard` | `src/Turns.tsx` | `pixel-edit-guard` |

`PIXELvsRTK` draws the same question as a timeline of model turns, without tools,
with an output filter and with PIXEL's brief; `EditGuard` draws the three edit
moments (the edit card, stale callers, the single Stop block). Both are
illustrations of a mechanism at 1600×1000, 30 fps, 8 s: no timing or token
figure appears in them, and their captions say so. The README and the website
embed the renders; render them with the recipe of
`.agents/rules/readme-webp.md` (crf 10 source, lossless 1600×1000 WebP). In a
container without network, point Remotion at a local headless shell:
`bunx remotion still src/index.ts PIXELvsRTK out.png --frame=235
--browser-executable=<headless_shell>`.

## The agent demo

`AgentDemo` replays two recorded Claude Code runs side by side on one clock:
the same task and model in the archived Sonnet recording, with an appended
PIXEL prompt on one side, without the install hooks. The script now uses
the hooks `pixel setup` writes; that is a different protocol. Nothing in it is written by hand: every
command, time and token count comes from a recording.

1. `scripts/record-demo.sh <dir> [reps] [model]` (Opus at medium effort by
   default, what most people run) checks out a pinned ref (`REF`, default
   `v0.5.0`) in a throwaway repository that holds only that ref's history,
   without `docs/motion`, so no agent can read the demo's own traces or a
   later commit, indexes it, then runs both arms `reps` times,
   each pair started together, and stores every stream-json event with its
   arrival time. The script's header lists what the two arms share. Both are
   told to answer in English: the account's organization instructions would
   otherwise leak into both.
2. `bun scripts/trace.ts <dir>` keeps each arm's median-time run (never the
   best) as `src/demo/{vanilla,pixel}.json`, writes every run to
   `src/demo/runs.json` and copies the recording's `meta.txt` (commit,
   model, CLI and PIXEL versions, prompt hash, task).
3. Tool-result tokens are estimates (each result's UTF-8 bytes ÷ 4, rounded),
   not total model context. Cost is Claude Code's `total_cost_usd`, not an
   independently checked invoice. The summary at the end shows the median of each metric over all runs, and
   its headline follows those medians rather than assuming a win.

The source recording predates this protocol: Claude Sonnet 5, PIXEL
0.5.0 with its agent prompt appended instead of its hooks, in the source
tree itself (`src/demo/meta.txt`). Three of its 22 runs read the demo's own
files, which is why the script now works in a separate worktree. An Opus 5.5
medium-effort re-recording with the hooks used 11 runs per arm on the same
low-research scoping task. The hooks arm had higher medians for duration
(42.9 s without PIXEL, 47.6 s with it; +11%), tokens read (11,945 vs. 13,350;
+12%) and provider-reported API cost ($0.283 vs. $0.372; +31%). It made 0–4
PIXEL calls per run, versus zero without PIXEL. All 22 responses were in
English, none read the demo files, and the worktree was clean at `v0.5.0`.
The raw bundle is not archived, so its per-run values, exact date, exact
CLI/harness versions beyond that tag and token-counting method cannot be
reconstructed. This is a limited negative result on one low-research task,
not evidence of general slowdown or speedup; see the [trial note](../bench/opus-install-hook-trial.md).

Any future re-recording should keep the task and report all outcomes, not select
a task for a favourable result. Re-record after a release that changes the agent prompt or the commands it
names, with that release installed and `REF` set to its tag, and update
`recorded` and `modelName` in `src/Root.tsx`.

## Render

```bash
cd docs/motion
bun install
bunx remotion studio src/index.ts        # preview
scripts/render.sh                        # every composition
scripts/render.sh PIXELScope AgentDemo   # some of them
```

`scripts/render.sh` runs `bun install` first, needs `ffmpeg` and `img2webp`
on `PATH`, and writes three files per composition into `docs/examples/`:

- `<name>.mp4`: 1600×1000 H.264, played by the website;
- `<name>.jpg`: its last frame, the website's poster;
- `<name>.webp`: 800×500 at 15 fps, embedded by the root README, since GitHub
  renders an animated image inline but not a video. For a sharper README asset,
  set `README_WEBP_SCALE=2` (1600×1000), `README_RENDER_CRF=18` and
  `README_WEBP_ONLY=1`. README WebP frames use lossless encoding; the last
  option leaves the website video and poster untouched.
