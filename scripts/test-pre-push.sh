#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Publishing must work without a toolchain, local review or network fetch.
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/pixel-pre-push-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
for tool in git cargo pixel pixel-dev python3; do
    cat > "$tmp/$tool" <<'EOF'
#!/bin/sh
printf '%s\n' "$0 $*" >> "$CALL_LOG"
exit 1
EOF
    chmod +x "$tmp/$tool"
done
CALL_LOG="$tmp/calls" PATH="$tmp:/usr/bin:/bin" PIXEL_MUTANTS_GATE=local \
    /bin/sh "$repo/.githooks/pre-push"
test ! -e "$tmp/calls"
echo "pre-push contract: publication starts no local checks"
