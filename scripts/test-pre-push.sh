#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Contract for the pre-push order: a Rust baseline must compile before the
# remote mutation campaign starts, while non-Rust pushes avoid Cargo entirely.
# A branch behind origin's default is judged against its merge-base, never
# refused for not being rebased.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/pixel-pre-push-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
fixture="$tmp/repo"
mkdir -p "$fixture/.githooks" "$fixture/scripts" "$tmp/bin"
cp "$repo/.githooks/pre-push" "$fixture/.githooks/"

cat > "$fixture/scripts/mutants-remote-gate.sh" <<'EOF'
#!/bin/sh
printf 'remote %s\n' "${PIXEL_MUTANTS_BASE:-missing}" >> "$ORDER_LOG"
EOF
chmod +x "$fixture/scripts/mutants-remote-gate.sh"

cat > "$tmp/bin/git" <<'EOF'
#!/bin/sh
case "$*" in
    "rev-parse --show-toplevel") printf '%s\n' "$PRE_PUSH_REPO" ;;
    "fetch --quiet origin HEAD") : ;;
    "rev-parse --verify --quiet FETCH_HEAD") printf '%s\n' "${FETCHED_TIP:-base}" ;;
    "merge-base HEAD FETCH_HEAD") printf '%s\n' "${MERGE_BASE:-base}" ;;
    "merge-base HEAD parent-branch") printf '%s\n' parent-fork ;;
    "diff --name-only ${MERGE_BASE:-base}...HEAD") printf '%b' "${CHANGED_PATHS:-}" ;;
    *) echo "unexpected git invocation: $*" >&2; exit 2 ;;
esac
EOF
cat > "$tmp/bin/cargo" <<'EOF'
#!/bin/sh
printf 'cargo %s\n' "$*" >> "$ORDER_LOG"
test "${CARGO_FAIL:-0}" != 1
EOF
cat > "$tmp/bin/pixel" <<'EOF'
#!/bin/sh
# The hook probes `review-gate --help` first; only the real call is logged.
case "$*" in
    "review-gate --help") : ;;
    review-gate*) printf 'review %s\n' "$*" >> "$ORDER_LOG" ;;
esac
EOF
chmod +x "$tmp/bin/git" "$tmp/bin/cargo" "$tmp/bin/pixel"

run() {
    PATH="$tmp/bin:/usr/bin:/bin" PRE_PUSH_REPO="$fixture" ORDER_LOG="$tmp/order.log" \
        CHANGED_PATHS="$1" sh "$fixture/.githooks/pre-push"
}

: > "$tmp/order.log"
run 'crates/demo/src/lib.rs\n'
test "$(sed -n '1p' "$tmp/order.log")" = 'cargo check --all-targets'
test "$(sed -n '2p' "$tmp/order.log")" = 'remote base'

: > "$tmp/order.log"
run 'docs/guide.md\n'
test "$(sed -n '1p' "$tmp/order.log")" = 'remote base'
test "$(sed -n '2p' "$tmp/order.log")" = 'review review-gate . --base base --fail-on concern'

: > "$tmp/order.log"
if CARGO_FAIL=1 run 'crates/demo/src/lib.rs\n' > "$tmp/failed.out" 2>&1; then
    echo "expected a failed baseline to block the push" >&2
    exit 1
fi
grep -q 'cargo check --all-targets failed' "$tmp/failed.out"
test "$(cat "$tmp/order.log")" = 'cargo check --all-targets'

# Behind main: no refusal, and both gates judge the merge-base, not the tip.
# (POSIX sh may keep an assignment made before a function call: reset it.)
CARGO_FAIL=0
: > "$tmp/order.log"
if ! FETCHED_TIP=newer MERGE_BASE=older run 'crates/demo/src/lib.rs\n' > "$tmp/behind.out" 2>&1; then
    cat "$tmp/behind.out" >&2
    echo "expected a branch behind origin's default to push" >&2
    exit 1
fi
! grep -q 'not rebased' "$tmp/behind.out"
test "$(sed -n '1p' "$tmp/order.log")" = 'cargo check --all-targets'
test "$(sed -n '2p' "$tmp/order.log")" = 'remote older'

# Stacked: an explicit immediate base wins over origin's default for the
# baseline diff, the remote gate and the review alike.
: > "$tmp/order.log"
if ! PIXEL_MUTANTS_BASE=parent-branch MERGE_BASE=parent-fork run 'crates/demo/src/lib.rs\n' > "$tmp/stacked.out" 2>&1; then
    cat "$tmp/stacked.out" >&2
    echo "expected a stacked branch to push against its parent" >&2
    exit 1
fi
test "$(sed -n '2p' "$tmp/order.log")" = 'remote parent-fork'
test "$(sed -n '3p' "$tmp/order.log")" = 'review review-gate . --base parent-fork --fail-on concern'

echo "pre-push contract: ok"
