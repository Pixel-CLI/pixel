#!/usr/bin/env bash
# SPDX-FileCopyrightText: The RTO contributors
# SPDX-License-Identifier: MIT
#
# Records what an agent host really sends `pixel hook` and what comes back, for
# the "to verify" cells of specs/hosts.md.
#
#   scripts/verify/capture.sh on  [PATH_TO_PIXEL]   # default: the pixel on PATH
#   ...use the agent for a few turns (prompt, an edit, a stop)...
#   scripts/verify/capture.sh show
#   scripts/verify/capture.sh off [PATH_TO_PIXEL]
#
# `on` moves the binary aside to `pixel.real` and puts a wrapper in its place.
# A hook command the installer wrote is `<that path> hook <event> --host <h>`,
# so the wrapper sees every call whatever file shape the host keeps. Each call
# leaves, in ~/.pixel-capture/, `<nanos>-<event>-<host>.in` (stdin),
# `.out` (stdout), `.err` and `.code`. The files hold the typed prompt: review
# them before sharing, and run `off` when done.
set -euo pipefail

mode="${1:-}"
bin="${2:-$(command -v pixel || true)}"
dir="${PIXEL_CAPTURE_DIR:-$HOME/.pixel-capture}"

case "$mode" in
on)
  [ -n "$bin" ] && [ -x "$bin" ] || { echo "no pixel binary: pass its path" >&2; exit 2; }
  [ ! -e "$bin.real" ] || { echo "$bin.real exists: already on?" >&2; exit 2; }
  mkdir -p "$dir"
  mv "$bin" "$bin.real"
  cat >"$bin" <<WRAP
#!/usr/bin/env bash
# pixel capture wrapper (scripts/verify/capture.sh off restores the binary)
real="$bin.real"
if [ "\${1:-}" != hook ]; then exec "\$real" "\$@"; fi
stamp="\$(date +%s%N)-\${2:-event}-\$(printf '%s\n' "\$@" | grep -A1 -- --host | tail -1)"
mkdir -p "$dir"
cat >"$dir/\$stamp.in"
set +e
"\$real" "\$@" <"$dir/\$stamp.in" >"$dir/\$stamp.out" 2>"$dir/\$stamp.err"
code=\$?
echo "\$code" >"$dir/\$stamp.code"
cat "$dir/\$stamp.out"
exit "\$code"
WRAP
  chmod +x "$bin"
  echo "capturing into $dir (restore with: $0 off $bin)"
  ;;
off)
  [ -n "$bin" ] && [ -e "$bin.real" ] || { echo "$bin.real is missing: not on" >&2; exit 2; }
  mv -f "$bin.real" "$bin"
  echo "restored $bin; captures stay in $dir"
  ;;
show)
  [ -d "$dir" ] || { echo "nothing captured in $dir" >&2; exit 2; }
  for input in "$dir"/*.in; do
    [ -e "$input" ] || { echo "nothing captured in $dir" >&2; exit 2; }
    base="${input%.in}"
    printf '== %s (exit %s)\n' "$(basename "$base")" "$(cat "$base.code" 2>/dev/null || echo '?')"
    printf 'in : %s\n' "$(head -c 600 "$input")"
    printf 'out: %s\n' "$(head -c 600 "$base.out" 2>/dev/null)"
  done
  ;;
*)
  sed -n '5,17p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
  ;;
esac
