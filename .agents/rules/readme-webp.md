# README animated WebP — the "perfect" recipe

The README embeds `docs/examples/<name>.webp` (GitHub renders animated images
inline where it will not play a video). The verified parameters that give
maximum quality at README size (1600×1000 displayed at 800px, ~7.9 MB for
PixelScope, 120 frames):

```bash
cd docs/motion
bun install --frozen-lockfile --silent
bunx remotion render src/index.ts <CompositionId> /tmp/<name>.mp4 \
  --codec=h264 --crf=10 --log=error
t=$(mktemp -d)
ffmpeg -loglevel error -y -i /tmp/<name>.mp4 \
  -vf "fps=15,scale=1600:1000:flags=lanczos" "$t/%04d.png"
img2webp -loop 0 -lossless -m 6 -d 67 "$t"/*.png \
  -o ../examples/<name>.webp
```

- Render source at `--crf=10` so the intermediate mp4 carries near-lossless
  frames (crf=14 already shows faint artifacts).
- Keep the file at **1600×1000** and embed it `<img width="800">` in the
  README — an 800px-wide file stretched by `width="100%"` looks blurry on
  retina displays; 1600px shown at 800 is sharp everywhere.
- Encode **lossless** (`img2webp -lossless -m 6`): maximum quality, ~7.9 MB.
  Do NOT use `-q 70`-`q 85` lossy — banding is visible on the dark UI.
- Never produce the README webp by transcoding an already-lossy webp; always
  re-render from Remotion.
- Composition id → file name map lives in `docs/motion/scripts/render.sh`
  (`name_of`); e.g. `PixelScope` → `pixel-scope-comparison.webp`.
- A recording replay (not a Remotion composition) may crop each pane to
  fewer terminal columns and show the pair across the README's full column
  width, so the text renders larger: see
  `docs/motion/prompt-submit/README.md`, whose two ~80-column panes become
  two ~40-column 800×1000 crops displayed at `width="1000"`.
- Cheaper fallback if size must shrink: `-lossy -q 95 -m 6` at 1600×1000
  (~3.5 MB, visually near-lossless).
