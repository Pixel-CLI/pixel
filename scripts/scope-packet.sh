#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Reproduces the task packet the Pixel arm of the Problem chapter's recording
# received (docs/bench/problem-trace.md), and prints it with the index size.
#
# The recording kept no packet text, only whether each run got one
# (packets.txt). The packet is a pure function of the binary, the indexed
# tree and the prompt, so this rebuilds the recorded repository the way
# docs/motion/scripts/record-demo.sh does (REF's history only, docs/motion
# dropped in one commit), indexes it, and feeds the recorded prompt to the
# prompt-submit hook. Run it with the recorded binary (meta.txt `pixel=`):
# another version may rank differently.
#
# Usage: scripts/scope-packet.sh [docs/bench/problem-trace] > docs/bench/problem-trace/packet.txt
set -euo pipefail

root=${1:-docs/bench/problem-trace}
src=$(git rev-parse --show-toplevel)
ref=$(sed -n 's/^ref=\([^ ]*\).*/\1/p' "$root/meta.txt")
task=$(sed -n 's/^task=//p' "$root/meta.txt")
want=$(sed -n 's/^pixel=pixel \([^ ]*\) commit: \([0-9a-f]*\).*/\1 \2/p' "$root/meta.txt")
have=$(pixel --version | awk '/^pixel /{v=$2} /^commit:/{c=$2} END{print v, c}')
if [ "$want" != "$have" ]; then
  echo "scope-packet: recorded with pixel $want, running $have" >&2
  exit 1
fi

work=$(mktemp -d)
repo="$work/repo"
trap 'pixel daemon stop "$repo" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT
git init --quiet "$repo"
echo "$(git -C "$src" rev-parse --path-format=absolute --git-common-dir)/objects" > "$repo/.git/objects/info/alternates"
git -C "$repo" update-ref refs/heads/main "$(git -C "$src" rev-parse "$ref^{commit}")"
git -C "$repo" checkout --quiet --force main
rm -rf "$repo/docs/motion"
git -C "$repo" add -A
git -C "$repo" diff --cached --quiet ||
  git -C "$repo" -c user.name=demo -c user.email=demo@localhost commit --quiet -m "demo: drop the demo's own sources"
(cd "$repo" && pixel prepare-repo . >/dev/null 2>&1 && pixel scope-task "warm up the index" --metrics off >/dev/null 2>&1)

echo "# ref=$ref pixel=$have"
echo "# index $(cd "$repo" && pixel status | sed -n 's/^index: .*\(base_files=[0-9]*\).*/\1/p')"
echo "# tracked=$(git -C "$repo" ls-files | wc -l | tr -d ' ')"
python3 -c 'import json, sys; print(json.dumps({"session_id": "scope-packet", "prompt": sys.argv[1], "cwd": sys.argv[2], "hook_event_name": "UserPromptSubmit", "transcript_path": "/dev/null"}))' "$task" "$repo" |
  (cd "$repo" && pixel run-hook prompt-submit --provider claude) |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["hookSpecificOutput"]["additionalContext"])'
