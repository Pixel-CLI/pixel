# Prompt-submit recording replay

This README animation is a uniformly sped-up excerpt of the supplied
`Pixel Good case.mp4`, not a redraw. It keeps the two recorded CLI panes,
outputs, and status text intact; no annotations or extra footer are added.
The excerpt starts at 8.000s (after the shared prompt is submitted) and runs
through the source's 126.125s end at 6× speed (19.6875s), followed by a 1.5s
hold on the actual final frame so the completed right-hand answer is visible.
At 15fps the resulting loop is 21.2s. The source is already compressed video; CRF 10
preserves a high-quality intermediate, and the WebP encoding is lossless from
those generated frames. This is not a lossless re-encode of the original
recording and makes no token-saving or benchmark claim.

From the repository root, pass the original recording path and a local MP4
destination, then extract 1600×1000 frames and encode the README asset:

```bash
rtk bun docs/motion/prompt-submit/render-recording.ts \
  "/path/to/Pixel Good case.mp4" \
  /tmp/pixel-recorded-replay/pixel-cli-recorded-replay.mp4
rtk mkdir -p /tmp/pixel-recorded-replay/frames
rtk ffmpeg -loglevel error -y -i /tmp/pixel-recorded-replay/pixel-cli-recorded-replay.mp4 \
  -vf "fps=15,scale=1600:1000:flags=lanczos" \
  /tmp/pixel-recorded-replay/frames/%04d.png
rtk img2webp -loop 0 -lossless -m 6 -d 67 \
  /tmp/pixel-recorded-replay/frames/*.png \
  -o docs/examples/pixel-prompt-submit-comparison.webp
```

The MP4 is a local preview; only the lossless WebP belongs in the root README.
