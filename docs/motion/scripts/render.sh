#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Renders every composition into docs/examples/, in two forms:
#   <name>.mp4   1600x1000 H.264 for the website, with a <name>.jpg poster
#   <name>.webp  800x500 animated WebP at 15 fps for the README; set
#                README_WEBP_SCALE=2 for 1600x1000 retina output
#
# Usage: scripts/render.sh [CompositionId ...]   (default: all seven)
# Set README_WEBP_ONLY=1 to update only the README image; README_RENDER_CRF
# (default 26) tunes the intermediate video used to extract its frames.
# Needs ffmpeg and img2webp (brew install ffmpeg webp).
set -euo pipefail

cd "$(dirname "$0")/.."
# A fresh clone or a `just clean` leaves no node_modules; the lockfile pins
# the Remotion version every render uses.
bun install --frozen-lockfile --silent
examples=../examples
readme_scale=${README_WEBP_SCALE:-1}
render_crf=${README_RENDER_CRF:-26}
webp_only=${README_WEBP_ONLY:-0}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

name_of() {
  case $1 in
    AgentDemo) echo pixel-agent-demo ;;
    PixelComparison) echo pixel-measured-comparison ;;
    PixelImpact) echo pixel-impact-comparison ;;
    PixelScope) echo pixel-scope-comparison ;;
    PixelRollback) echo pixel-rollback-comparison ;;
    PixelPublish) echo pixel-publish-comparison ;;
    PixelRewrite) echo pixel-rewrite-comparison ;;
    *) echo "unknown composition $1" >&2; exit 1 ;;
  esac
}

ids=("$@")
[ ${#ids[@]} -gt 0 ] || ids=(AgentDemo PixelScope PixelComparison PixelImpact PixelRewrite PixelRollback PixelPublish)

for id in "${ids[@]}"; do
  name=$(name_of "$id")
  echo "== $id -> $name"
  bunx remotion render src/index.ts "$id" "$tmp/$name.mp4" --codec=h264 --crf="$render_crf" --log=error
  if [ "$webp_only" != 1 ]; then
    ffmpeg -loglevel error -y -i "$tmp/$name.mp4" -c copy -movflags +faststart "$examples/$name.mp4"
    # The poster is the last frame: the finished state, for reduced motion and
    # for the moment before the video starts.
    ffmpeg -loglevel error -y -sseof -0.1 -i "$tmp/$name.mp4" -frames:v 1 -q:v 3 "$examples/$name.jpg"
    # The agent demo waits on a Play button, so the site shows its first frame
    # (both clocks at 0:00) until the visitor starts it.
    if [ "$id" = AgentDemo ]; then
      ffmpeg -loglevel error -y -i "$tmp/$name.mp4" -frames:v 1 -q:v 3 "$examples/$name-start.jpg"
    fi
  fi
  mkdir -p "$tmp/$name"
  ffmpeg -loglevel error -y -i "$tmp/$name.mp4" -vf "fps=15,scale=$((800 * readme_scale)):$((500 * readme_scale)):flags=lanczos" "$tmp/$name/%04d.png"
  img2webp -loop 0 -lossless -m 6 -d 67 "$tmp/$name"/*.png -o "$examples/$name.webp" >/dev/null
  if [ "$webp_only" = 1 ]; then
    ls -lh "$examples/$name.webp" | awk '{print "   ", $5, $9}'
  else
    ls -lh "$examples/$name.mp4" "$examples/$name.webp" | awk '{print "   ", $5, $9}'
  fi
done
