#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# bench-read-savings.sh — what reaches an agent that needs to know what a
# large file contains: the whole file, or `pixel list-signatures` on it.
#
# Same method as the "Reading code" table of website/content/benchmarks.md:
# UTF-8 bytes divided by four on both sides, no second model. The files are
# well-known large files of popular repositories, each pinned to a commit so
# the counts can be re-derived; they are downloaded into a throwaway Git
# repository under $TMPDIR, never into this checkout.
#
# Output: one tab-separated row per file (name, repository, path, commit,
# lines, full tokens, Pixel tokens, saved %, signatures listed), then the
# median saving of the rows not marked "excluded". The signature count is
# the check that the extractor understood the file: a count far below the
# file's own definitions means the saving is overstated (React's work loop,
# written in Flow, lists 21 of its 125 top-level functions).
#
# Usage: scripts/bench-read-savings.sh            (needs curl and network)
set -euo pipefail

find_pixel() {
  # Absolute, since the measurements run from the throwaway repository.
  if [ -n "${PIXEL_BIN:-}" ]; then
    case "$PIXEL_BIN" in
      /*) echo "$PIXEL_BIN" ;;
      */*) echo "$PWD/$PIXEL_BIN" ;;
      *) command -v "$PIXEL_BIN" || { echo "bench-read-savings: PIXEL_BIN=$PIXEL_BIN not found" >&2; exit 1; } ;;
    esac
    return
  fi
  if command -v pixel >/dev/null 2>&1; then command -v pixel; return; fi
  for p in target/dev-release/pixel target/release/pixel; do
    if [ -x "$p" ]; then echo "$PWD/$p"; return; fi
  done
  echo "bench-read-savings: no pixel binary (set PIXEL_BIN)" >&2
  exit 1
}
PIXEL=$(find_pixel)

# name  repository  path  commit  status
FILES='
transformers huggingface/transformers src/transformers/trainer.py 98d39824ed30e684e5122d04a2d9564efffc4965 kept
fastapi fastapi/fastapi fastapi/routing.py d62354434b2e508fe89024213b220ca8e67dea5e kept
next vercel/next.js packages/next/src/server/base-server.ts 52788bdfe16de091f037748fc8a1837d770aef4f kept
langchain langchain-ai/langchain libs/core/langchain_core/language_models/chat_models.py ed0ad742e92c94c0285cebec7f5957efcddd72dc kept
django django/django django/db/models/query.py 6ade6258480fba84a7e895b4b1e1716dfc954778 kept
cpython python/cpython Lib/typing.py de2ea9aaefebd1b06e5302c1858d70e00cd63639 kept
vscode microsoft/vscode src/vs/editor/common/model/textModel.ts 227803b34283533344c393fd854a6fbcfe97a158 kept
tokio tokio-rs/tokio tokio/src/runtime/scheduler/multi_thread/worker.rs 7bb6f0734922cffa7e49049dfc4c10d84737db41 kept
react facebook/react packages/react-reconciler/src/ReactFiberWorkLoop.js cbb046ab92b66dfc4ad1e1ea30d4b8beae6f2c24 excluded
'

work=$(mktemp -d "${TMPDIR:-/tmp}/pixel-read-savings.XXXXXX")
trap 'rm -rf "$work"' EXIT
git -C "$work" init -q

"$PIXEL" --version | head -1 >&2
printf 'name\trepository\tpath\tcommit\tlines\tfull_tok\tpixel_tok\tsaved_pct\tsignatures\tstatus\n'
kept=()
while read -r name repo file sha status; do
  [ -n "$name" ] || continue
  local_name="$name-$(basename "$file")"
  curl -fsSL "https://raw.githubusercontent.com/$repo/$sha/$file" -o "$work/$local_name"
  lines=$(wc -l < "$work/$local_name" | tr -d ' ')
  full=$(( $(wc -c < "$work/$local_name") / 4 ))
  sigs_out=$(cd "$work" && "$PIXEL" list-signatures "$local_name" --metrics off 2>/dev/null)
  px=$(( $(printf '%s\n' "$sigs_out" | wc -c) / 4 ))
  sigs=$(( $(printf '%s\n' "$sigs_out" | wc -l) - 1 ))
  saved=$(awk -v f="$full" -v p="$px" 'BEGIN { printf "%.1f", 100 * (1 - p / f) }')
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$name" "$repo" "$file" "${sha:0:7}" "$lines" "$full" "$px" "$saved" "$sigs" "$status"
  [ "$status" = kept ] && kept+=("$saved")
done <<< "$FILES"

printf '%s\n' "${kept[@]}" | sort -n | awk '{ v[NR] = $1 } END { m = (NR % 2) ? v[(NR + 1) / 2] : (v[NR / 2] + v[NR / 2 + 1]) / 2; printf "median saved (%d kept files): %.1f%%\n", NR, m }' >&2
