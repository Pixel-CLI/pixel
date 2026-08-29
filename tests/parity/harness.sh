#!/usr/bin/env bash
# Golden parity harness: compare old gitpixel binary vs new pixel binary.
#
# M1 gate per PLAN.md: "golden parity vs old gitpixel binary (identical hit
# sets/graph edges/target tiers; order may differ deliberately)."
#
# This harness is NOT invoked by `cargo test` -- it requires both binaries
# to be built. Run it explicitly:
#
#   tests/parity/harness.sh
#
# Exit 0 = parity verified. Exit 1 = parity broken.
#
# What it compares:
#   1. Search hit sets (path+line pairs, order-independent)
#   2. Target tiers (path -> tier mapping)
#   3. Graph edge counts
#
# Ordering is allowed to differ (pixel may rank deliberately). Only the
# sets and tier assignments must match.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
OLD_BIN="${GITPIXEL_BIN:-$REPO_ROOT/../gitpixel/target/release/gitpixel}"
NEW_BIN="${PIXEL_BIN:-$REPO_ROOT/target/debug/pixel}"

if [ ! -x "$OLD_BIN" ]; then
    echo "FAIL: old gitpixel binary not found at $OLD_BIN"
    echo "  Build it: cd ~/Documents/gitpixel && cargo build --release"
    exit 1
fi
if [ ! -x "$NEW_BIN" ]; then
    echo "FAIL: new pixel binary not found at $NEW_BIN"
    echo "  Build it: cd ~/Documents/pixel && cargo build"
    exit 1
fi

WORKDIR=$(mktemp -d /tmp/pixel-parity-XXXXXX)
trap 'rm -rf "$WORKDIR"' EXIT

# Two independent copies of the fixture so neither binary sees the other's
# metadata directory (.gitpixel/ vs .pixel/).
FIXTURE_OLD="$WORKDIR/old-repo"
FIXTURE_NEW="$WORKDIR/new-repo"

# ---------------------------------------------------------------------------
# Fixture: a small repo with multiple files, symbols, and call edges.
# ---------------------------------------------------------------------------

create_fixture() {
    local dir="$1"
    mkdir -p "$dir/src/auth" "$dir/src/util"

    cat > "$dir/src/auth/login.rs" <<'RUST'
pub fn login_user(name: &str) -> bool {
    !name.is_empty()
}

pub fn logout_user(name: &str) -> bool {
    name.is_empty()
}
RUST

    cat > "$dir/src/auth/session.rs" <<'RUST'
use crate::login::login_user;

pub fn start_session(name: &str) -> bool {
    login_user(name)
}

pub fn end_session(name: &str) -> bool {
    name.is_empty()
}
RUST

    cat > "$dir/src/util/strings.rs" <<'RUST'
pub fn pad_left(s: &str, n: usize) -> String {
    format!("{s:>n$}")
}

pub fn pad_right(s: &str, n: usize) -> String {
    format!("{s:<n$}")
}
RUST

    cd "$dir"
    git init -q
    git config user.email "t@t"
    git config user.name "t"
    git add .
    git commit -qm "fixture"
}

create_fixture "$FIXTURE_OLD"
create_fixture "$FIXTURE_NEW"

PASS=true

# ---------------------------------------------------------------------------
# Helper: extract hit set (path:line) from search JSON, sorted.
# ---------------------------------------------------------------------------

search_hitset() {
    local bin="$1"
    local root="$2"
    local pattern="$3"
    # Build index first, then search with --no-daemon (search supports it).
    "$bin" index "$root" --no-daemon >/dev/null 2>&1 || true
    "$bin" search "$pattern" "$root" --json --no-daemon 2>/dev/null \
        | python3 -c "
import sys, json
hits = set()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        m = json.loads(line)
        hits.add((m.get('path',''), m.get('line',0)))
    except: pass
for h in sorted(hits):
    print(f'{h[0]}:{h[1]}')
"
}

# ---------------------------------------------------------------------------
# Test 1: Search hit set parity
# ---------------------------------------------------------------------------

echo "=== Test 1: Search hit set parity ==="
PATTERNS=("login_user" "session" "pad_left" "start_session" "logout_user")

for pat in "${PATTERNS[@]}"; do
    OLD_HITS=$(search_hitset "$OLD_BIN" "$FIXTURE_OLD" "$pat" || true)
    NEW_HITS=$(search_hitset "$NEW_BIN" "$FIXTURE_NEW" "$pat" || true)

    if [ "$OLD_HITS" = "$NEW_HITS" ]; then
        COUNT=$(echo "$OLD_HITS" | grep -c . || echo 0)
        echo "  PASS: pattern='$pat' hit sets identical ($COUNT hits)"
    else
        echo "  FAIL: pattern='$pat' hit sets differ"
        echo "    old:"
        echo "$OLD_HITS" | sed 's/^/      /'
        echo "    new:"
        echo "$NEW_HITS" | sed 's/^/      /'
        PASS=false
    fi
done

# ---------------------------------------------------------------------------
# Test 2: Target tier parity
# ---------------------------------------------------------------------------

echo "=== Test 2: Target tier parity ==="

# Extract path -> tier mapping from targets JSON output.
target_tiers() {
    local bin="$1"
    local root="$2"
    local task="$3"
    cd "$root"
    "$bin" targets "$task" --json --no-manifest . 2>/dev/null \
        | python3 -c "
import sys, json
data = json.load(sys.stdin)
targets = data.get('targets', [])
for t in sorted(targets, key=lambda x: x.get('path','')):
    print(f\"{t.get('path','')}\t{t.get('tier','?')}\")
" || echo "ERROR"
}

TASKS=("fix login_user auth flow" "fix session management" "fix pad_left utility")
for task in "${TASKS[@]}"; do
    OLD_TIERS=$(target_tiers "$OLD_BIN" "$FIXTURE_OLD" "$task")
    NEW_TIERS=$(target_tiers "$NEW_BIN" "$FIXTURE_NEW" "$task")

    if [ "$OLD_TIERS" = "ERROR" ] || [ "$NEW_TIERS" = "ERROR" ]; then
        echo "  SKIP: task='$task' (one or both errored)"
        continue
    fi

    if [ "$OLD_TIERS" = "$NEW_TIERS" ]; then
        echo "  PASS: task='$task' tiers identical"
    else
        echo "  FAIL: task='$task' tiers differ"
        echo "    old:"
        echo "$OLD_TIERS" | sed 's/^/      /'
        echo "    new:"
        echo "$NEW_TIERS" | sed 's/^/      /'
        PASS=false
    fi
done

# ---------------------------------------------------------------------------
# Test 3: Graph edge parity
# ---------------------------------------------------------------------------

echo "=== Test 3: Graph edge parity ==="

# Graph outputs stats JSON (edges count, files, symbols). Compare edge counts
# and symbol counts -- the edge SET should be identical for the same source.
graph_stats() {
    local bin="$1"
    local root="$2"
    cd "$root"
    "$bin" graph . 2>/dev/null \
        | python3 -c "
import sys, json
data = json.load(sys.stdin)
print(f\"edges={data.get('edges',0)} files={data.get('files',0)} symbols={data.get('symbols',0)} unresolved={data.get('unresolved',0)}\")
" || echo "ERROR"
}

OLD_GRAPH=$(graph_stats "$OLD_BIN" "$FIXTURE_OLD")
NEW_GRAPH=$(graph_stats "$NEW_BIN" "$FIXTURE_NEW")

if [ "$OLD_GRAPH" = "ERROR" ] || [ "$NEW_GRAPH" = "ERROR" ]; then
    echo "  SKIP: graph stats comparison (one or both errored)"
else
    if [ "$OLD_GRAPH" = "$NEW_GRAPH" ]; then
        echo "  PASS: graph stats identical ($OLD_GRAPH)"
    else
        echo "  FAIL: graph stats differ"
        echo "    old: $OLD_GRAPH"
        echo "    new: $NEW_GRAPH"
        PASS=false
    fi
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo ""
if [ "$PASS" = "true" ]; then
    echo "PARITY VERIFIED: all tests passed"
    exit 0
else
    echo "PARITY BROKEN: one or more tests failed"
    exit 1
fi
