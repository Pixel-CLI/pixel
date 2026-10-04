#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Contract for the remote host side of the pre-push mutation gate. It uses a
# disposable worker home and stubs only git: no SSH, Cargo, or mutation run.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/pixel-mutants-gate-host-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
home="$tmp/home"
worker="$home/workers/pixel"
mkdir -p "$worker/scripts" "$home/.cargo" "$tmp/bin" "$tmp/payload"
printf '# test fixture\n' > "$home/.cargo/env"
: > "$tmp/payload/push.bundle"
tar -cf "$tmp/payload.tar" -C "$tmp/payload" push.bundle

cat > "$worker/scripts/mutants-preflight.sh" <<'EOF'
#!/bin/sh
set -eu
printf 'base=%s\n' "$PIXEL_MUTANTS_BASE"
EOF
chmod +x "$worker/scripts/mutants-preflight.sh"

cat > "$tmp/bin/git" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$GIT_LOG"
EOF
chmod +x "$tmp/bin/git"

# macOS has no flock(1), while the Linux host uses it to serialize campaigns.
cat > "$tmp/bin/flock" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod +x "$tmp/bin/flock"

# GNU tar on the Linux host accepts this warning flag; BSD tar on macOS does
# not, so remove only that host-specific first argument in the fixture.
cat > "$tmp/bin/tar" <<'EOF'
#!/bin/sh
case "$1" in --warning=no-unknown-keyword) shift ;; esac
exec /usr/bin/tar "$@"
EOF
chmod +x "$tmp/bin/tar"

base=0123456789abcdef0123456789abcdef01234567
head=89abcdef0123456789abcdef0123456789abcdef
HOME="$home" PATH="$tmp/bin:$PATH" GIT_LOG="$tmp/git.log" \
    sh "$repo/scripts/mutants-gate-host.sh" "$base" "$head" \
    < "$tmp/payload.tar" > "$tmp/out"

grep -Fx "base=$base" "$tmp/out"
grep -Fx 'fetch --quiet origin main' "$tmp/git.log"
grep -Fx "checkout --quiet --detach $head" "$tmp/git.log"

echo "mutants gate host contract: ok"
