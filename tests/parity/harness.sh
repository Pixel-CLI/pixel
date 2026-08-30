#!/usr/bin/env bash
# Golden parity harness: compare old gitpixel binary vs new pixel binary.
#
# M1 gate per PLAN.md: "golden parity vs old gitpixel binary (identical hit
# sets/graph edges/target tiers; order may differ deliberately)."
#
# This harness is NOT invoked by `cargo test` directly -- it requires both
# binaries to be built. Run it explicitly:
#
#   tests/parity/harness.sh
#
# It IS wired into the Rust test suite as an ignored integration test; see
# crates/pixel-bench/tests/parity.rs for the opt-in `cargo test` entrypoint.
#
# Exit 0 = parity verified (every check either passed or was never reached
# because of an earlier hard failure that already made the run fail).
# Exit 1 = parity broken -- at least one comparison recorded a FAIL.
#
# Design rule enforced throughout this script: a comparison NEVER "passes"
# just because both sides errored, both sides returned nothing, or JSON
# failed to parse. Any of those is an explicit FAIL, never a silent SKIP.
#
# What it compares (always-on, against a small hardcoded fixture):
#   1. Search hit sets (path+line pairs, order-independent)
#   2. Target tiers (path -> tier mapping)
#   3. Graph aggregate stats (edges/files/symbols/unresolved counts)
#   4. Symbol lookups (full symbol record sets, order-independent)
#   5. Impact / blast-radius (d1/d2/d3 affected-symbol sets, order-independent)
#   6. Uses / callers-callees (edge sets, order-independent)
#   7. `search --scope code` hit-SET parity against the old binary's
#      unranked baseline (order is expected to differ; the set must not)
#   8. The daemon code path (search/symbol/targets served through a running
#      daemon instead of in-process)
#
# Optional (set PARITY_REPO=<path-to-a-real-git-repo> to also run a reduced
# version of tests 1-2 and 4-7 against a real, larger codebase -- e.g.
# PARITY_REPO=/Users/livio/Documents/gitpixel):
#   9. Same family of checks against a real repo snapshot, with symbol
#      names/uids discovered dynamically via `targets` instead of hardcoded.
#
# Ordering is allowed to differ (pixel may rank deliberately). Only the
# sets and tier/risk assignments must match.

set -uo pipefail
# NOTE: deliberately NOT `set -e`. This script runs dozens of independent
# comparisons and must keep going (and keep counting failures) after any
# single one errors. Every command whose exit status matters is checked
# explicitly below.

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

# Daemons started during Test 8 (and, optionally, nowhere else) are stopped
# here unconditionally, even if an earlier comparison failed or the script
# is interrupted. Safe/idempotent: "daemon stop" on a root with no daemon
# running is a no-op error we discard.
cleanup() {
    if [ -n "${FIXTURE_OLD:-}" ]; then
        (cd "$FIXTURE_OLD" 2>/dev/null && "$OLD_BIN" daemon stop . >/dev/null 2>&1) || true
    fi
    if [ -n "${FIXTURE_NEW:-}" ]; then
        (cd "$FIXTURE_NEW" 2>/dev/null && "$NEW_BIN" daemon stop . >/dev/null 2>&1) || true
    fi
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

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

    # Subshell: never let this leak a `cd` into the calling script's cwd.
    (
        cd "$dir"
        git init -q
        git config user.email "t@t"
        git config user.name "t"
        git add .
        git commit -qm "fixture"
    )
}

create_fixture "$FIXTURE_OLD"
create_fixture "$FIXTURE_NEW"

# ---------------------------------------------------------------------------
# Normalizer scripts. Each reads JSON (or ndjson) on stdin and prints a
# sorted, order-independent textual representation on stdout -- or the
# literal line "ERROR" (and a non-zero exit) if the input isn't parseable.
# "ERROR" is a hard sentinel: compare_values() below treats it as an
# automatic FAIL, never as a value that can coincidentally match the other
# side's "ERROR".
# ---------------------------------------------------------------------------

cat > "$WORKDIR/norm_search.py" <<'PYEOF'
import sys, json
hits = set()
saw_line = False
parsed_any = False
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    saw_line = True
    try:
        m = json.loads(line)
        hits.add((m.get('path', ''), m.get('line', 0)))
        parsed_any = True
    except Exception:
        pass
if saw_line and not parsed_any:
    # Non-empty output that contains no valid JSON at all -- garbage from a
    # broken/fake binary. Never let this masquerade as "0 hits".
    print("ERROR")
    sys.exit(1)
for h in sorted(hits):
    print(f"{h[0]}:{h[1]}")
PYEOF

cat > "$WORKDIR/norm_targets.py" <<'PYEOF'
import sys, json
raw = sys.stdin.read()
try:
    d = json.loads(raw)
except Exception:
    print("ERROR")
    sys.exit(1)
rows = [(t.get('path', ''), t.get('tier', '?')) for t in d.get('targets', [])]
for path, tier in sorted(rows):
    print(f"{path}\t{tier}")
PYEOF

cat > "$WORKDIR/norm_graph.py" <<'PYEOF'
import sys, json
raw = sys.stdin.read()
try:
    d = json.loads(raw)
except Exception:
    print("ERROR")
    sys.exit(1)
print(f"edges={d.get('edges', 0)} files={d.get('files', 0)} "
      f"symbols={d.get('symbols', 0)} unresolved={d.get('unresolved', 0)}")
PYEOF

cat > "$WORKDIR/norm_symbol.py" <<'PYEOF'
import sys, json
raw = sys.stdin.read()
try:
    d = json.loads(raw)
except Exception:
    print("ERROR")
    sys.exit(1)
rows = []
for s in d.get('symbols', []):
    rows.append((
        s.get('path', ''), s.get('start_line', 0), s.get('end_line', 0),
        s.get('name', ''), s.get('kind', ''), s.get('qualified', ''),
        s.get('uid', ''),
    ))
for r in sorted(rows):
    print('\t'.join(str(x) for x in r))
PYEOF

cat > "$WORKDIR/norm_impact.py" <<'PYEOF'
import sys, json
raw = sys.stdin.read()
try:
    d = json.loads(raw)
except Exception:
    print("ERROR")
    sys.exit(1)
if 'candidates' in d:
    # Ambiguous name resolved to multiple symbols instead of one -- the
    # harness should always pass an exact uid or an unambiguous name for
    # this comparison to be meaningful.
    print("ERROR")
    sys.exit(1)
rows = []
for depth_key in ('d1_will_break', 'd2_likely_affected', 'd3_may_need_tests'):
    for item in d.get(depth_key, []):
        rows.append(f"{depth_key}:{item.get('uid', '')}")
print(f"risk={d.get('risk', '')}")
print(f"direction={d.get('direction', '')}")
for r in sorted(rows):
    print(r)
PYEOF

cat > "$WORKDIR/norm_uses.py" <<'PYEOF'
import sys, json
raw = sys.stdin.read()
try:
    d = json.loads(raw)
except Exception:
    print("ERROR")
    sys.exit(1)
if 'candidates' in d:
    print("ERROR")
    sys.exit(1)
rows = []
for e in d.get('edges', []):
    sym = e.get('symbol', {})
    rows.append(f"{e.get('site_line', '')}:{sym.get('uid', '')}")
print(f"role={d.get('role', '')}")
print(f"total_edges={d.get('total_edges', 0)}")
for r in sorted(rows):
    print(r)
PYEOF

cat > "$WORKDIR/discover_symbols.py" <<'PYEOF'
# Extract up to N (name, uid) pairs from a `targets --json` payload, for use
# as real, task-discovered symbols against an arbitrary (PARITY_REPO) repo
# whose contents this script doesn't know ahead of time.
import sys, json
n = int(sys.argv[1]) if len(sys.argv) > 1 else 3
raw = sys.stdin.read()
try:
    d = json.loads(raw)
except Exception:
    sys.exit(1)
out = []
seen = set()
for t in d.get('targets', []):
    for s in t.get('symbols', []):
        uid = s.get('uid', '')
        if uid and uid not in seen:
            seen.add(uid)
            out.append((s.get('name', ''), uid))
    if len(out) >= n:
        break
for name, uid in out[:n]:
    print(f"{name}\t{uid}")
PYEOF

# ---------------------------------------------------------------------------
# Pass/fail bookkeeping.
# ---------------------------------------------------------------------------

FAIL_COUNT=0
PASS_COUNT=0

record_pass() {
    PASS_COUNT=$((PASS_COUNT + 1))
    echo "  PASS: $1"
}

record_fail() {
    FAIL_COUNT=$((FAIL_COUNT + 1))
    echo "  FAIL: $1"
}

# compare_values <label> <old_value> <new_value>
# ERROR on either side is an automatic, unconditional FAIL -- two errors are
# NOT "identical" for the purposes of this harness.
compare_values() {
    local label="$1" old="$2" new="$3"
    if [ "$old" = "ERROR" ] || [ "$new" = "ERROR" ]; then
        record_fail "$label -- could not obtain a comparable result (old_ok=$([ "$old" = "ERROR" ] && echo no || echo yes), new_ok=$([ "$new" = "ERROR" ] && echo no || echo yes))"
        return 1
    fi
    if [ "$old" = "$new" ]; then
        record_pass "$label"
        return 0
    fi
    record_fail "$label -- values differ"
    echo "    --- old ---"
    printf '%s\n' "$old" | sed 's/^/    /'
    echo "    --- new ---"
    printf '%s\n' "$new" | sed 's/^/    /'
    return 1
}

# count_lines <value> -- number of non-blank lines, safe for empty input
# (never the double-"0\n0" grep -c bug: awk always prints exactly one line).
count_lines() {
    printf '%s' "$1" | awk 'NF{c++} END{print c+0}'
}

# ---------------------------------------------------------------------------
# Core comparison helpers. Every one of these:
#   - captures the invoked binary's own exit code separately from any JSON
#     parsing that happens afterward (a non-zero exit is ALWAYS "ERROR",
#     regardless of what -- if anything -- landed on stdout);
#   - never uses `|| true` / `|| echo ERROR-that-happens-to-match` in a way
#     that lets two broken binaries agree with each other.
# ---------------------------------------------------------------------------

search_hitset() {
    local bin="$1" root="$2" pattern="$3"
    shift 3
    local extra=("$@")
    "$bin" index "$root" >/dev/null 2>&1 || true
    local raw rc
    # bash 3.2 (macOS default) treats "${extra[@]}" on a zero-length array
    # as an unbound-variable error under `set -u`; the `+"${extra[@]}"`
    # idiom is the standard safe expansion across bash versions.
    #
    # --limit 10000 (the documented hard cap) is mandatory here: with no
    # --limit, `search` silently truncates to its own internal default
    # (observed: 100 results). Ranking (--scope code) reorders matches
    # BEFORE that truncation is applied, so on any pattern with >100
    # matches, comparing an unranked top-100 against a ranked top-100
    # produces two genuinely different subsets even though the full
    # underlying hit set is identical -- a truncation artifact, not a
    # real parity break. Always requesting the full cap on both sides
    # makes "hit-SET equality" mean what it says.
    raw=$("$bin" search "$pattern" "$root" --json --no-daemon --limit 10000 ${extra[@]+"${extra[@]}"} 2>/dev/null)
    rc=$?
    if [ $rc -ne 0 ]; then
        echo "ERROR"
        return
    fi
    printf '%s\n' "$raw" | python3 "$WORKDIR/norm_search.py"
}

target_tiers() {
    local bin="$1" root="$2" task="$3"
    local raw rc
    raw=$(cd "$root" && "$bin" targets "$task" --json --no-manifest . 2>/dev/null)
    rc=$?
    if [ $rc -ne 0 ]; then
        echo "ERROR"
        return
    fi
    printf '%s' "$raw" | python3 "$WORKDIR/norm_targets.py"
}

graph_stats() {
    local bin="$1" root="$2"
    local raw rc
    raw=$(cd "$root" && "$bin" graph . --json 2>/dev/null)
    rc=$?
    if [ $rc -ne 0 ]; then
        echo "ERROR"
        return
    fi
    printf '%s' "$raw" | python3 "$WORKDIR/norm_graph.py"
}

symbol_norm() {
    local bin="$1" root="$2" name="$3"
    local raw rc
    raw=$(cd "$root" && "$bin" symbol "$name" . --json 2>/dev/null)
    rc=$?
    if [ $rc -ne 0 ]; then
        echo "ERROR"
        return
    fi
    printf '%s' "$raw" | python3 "$WORKDIR/norm_symbol.py"
}

impact_norm() {
    local bin="$1" root="$2" target="$3" direction="$4"
    local raw rc
    raw=$(cd "$root" && "$bin" impact "$target" . --direction "$direction" --json 2>/dev/null)
    rc=$?
    if [ $rc -ne 0 ]; then
        echo "ERROR"
        return
    fi
    printf '%s' "$raw" | python3 "$WORKDIR/norm_impact.py"
}

uses_norm() {
    local bin="$1" root="$2" target="$3" role="$4"
    local raw rc
    raw=$(cd "$root" && "$bin" uses "$target" . --role "$role" --json 2>/dev/null)
    rc=$?
    if [ $rc -ne 0 ]; then
        echo "ERROR"
        return
    fi
    printf '%s' "$raw" | python3 "$WORKDIR/norm_uses.py"
}

# ---------------------------------------------------------------------------
# Test 1: Search hit set parity
# ---------------------------------------------------------------------------

echo "=== Test 1: Search hit set parity ==="
PATTERNS=("login_user" "session" "pad_left" "start_session" "logout_user")
# This pattern is known (by construction of the fixture above) to match at
# least one line. If a comparison ever "passes" by both sides returning an
# empty set for it, that is a broken harness, not a broken-but-passing
# system -- fail loudly instead.
NONEMPTY_PATTERN="login_user"

for pat in "${PATTERNS[@]}"; do
    OLD_HITS=$(search_hitset "$OLD_BIN" "$FIXTURE_OLD" "$pat")
    NEW_HITS=$(search_hitset "$NEW_BIN" "$FIXTURE_NEW" "$pat")
    COUNT=$(count_lines "$OLD_HITS")

    if [ "$pat" = "$NONEMPTY_PATTERN" ]; then
        if [ "$OLD_HITS" != "ERROR" ] && [ "$COUNT" -eq 0 ]; then
            record_fail "search pattern='$pat' -- old binary returned zero hits for a pattern the fixture guarantees at least one hit for"
        fi
        NEW_COUNT=$(count_lines "$NEW_HITS")
        if [ "$NEW_HITS" != "ERROR" ] && [ "$NEW_COUNT" -eq 0 ]; then
            record_fail "search pattern='$pat' -- new binary returned zero hits for a pattern the fixture guarantees at least one hit for"
        fi
    fi

    compare_values "search pattern='$pat' hit set ($COUNT hits)" "$OLD_HITS" "$NEW_HITS"
done

# ---------------------------------------------------------------------------
# Test 2: Target tier parity
# ---------------------------------------------------------------------------

echo "=== Test 2: Target tier parity ==="
TASKS=("fix login_user auth flow" "fix session management" "fix pad_left utility")
for task in "${TASKS[@]}"; do
    OLD_TIERS=$(target_tiers "$OLD_BIN" "$FIXTURE_OLD" "$task")
    NEW_TIERS=$(target_tiers "$NEW_BIN" "$FIXTURE_NEW" "$task")
    compare_values "target tiers task='$task'" "$OLD_TIERS" "$NEW_TIERS"
done

# ---------------------------------------------------------------------------
# Test 3: Graph aggregate stats parity
# ---------------------------------------------------------------------------

echo "=== Test 3: Graph aggregate stats parity ==="
OLD_GRAPH=$(graph_stats "$OLD_BIN" "$FIXTURE_OLD")
NEW_GRAPH=$(graph_stats "$NEW_BIN" "$FIXTURE_NEW")
compare_values "graph aggregate stats" "$OLD_GRAPH" "$NEW_GRAPH"

# ---------------------------------------------------------------------------
# Test 4: Symbol lookup parity
# ---------------------------------------------------------------------------

echo "=== Test 4: Symbol lookup parity ==="
SYMBOLS=("login_user" "start_session" "pad_left")
for name in "${SYMBOLS[@]}"; do
    OLD_SYM=$(symbol_norm "$OLD_BIN" "$FIXTURE_OLD" "$name")
    NEW_SYM=$(symbol_norm "$NEW_BIN" "$FIXTURE_NEW" "$name")
    compare_values "symbol lookup name='$name'" "$OLD_SYM" "$NEW_SYM"
done

# ---------------------------------------------------------------------------
# Test 5: Impact (blast radius) parity
# ---------------------------------------------------------------------------

echo "=== Test 5: Impact parity ==="
# login_user has one known upstream caller (start_session) in the fixture;
# start_session and pad_left have none. Exercising both a non-empty and an
# empty case catches an engine that always reports "no impact".
IMPACT_TARGETS=("login_user" "start_session" "pad_left")
for name in "${IMPACT_TARGETS[@]}"; do
    OLD_IMPACT=$(impact_norm "$OLD_BIN" "$FIXTURE_OLD" "$name" upstream)
    NEW_IMPACT=$(impact_norm "$NEW_BIN" "$FIXTURE_NEW" "$name" upstream)

    if [ "$name" = "login_user" ] && [ "$OLD_IMPACT" != "ERROR" ]; then
        EDGE_LINES=$(printf '%s\n' "$OLD_IMPACT" | grep -c '^d[123]_' || true)
        if [ "${EDGE_LINES:-0}" -eq 0 ]; then
            record_fail "impact target='login_user' -- expected at least one upstream caller (start_session) in the OLD binary's own output but found none"
        fi
    fi

    compare_values "impact target='$name' direction=upstream" "$OLD_IMPACT" "$NEW_IMPACT"
done

# ---------------------------------------------------------------------------
# Test 6: Uses (callers/callees) parity
# ---------------------------------------------------------------------------

echo "=== Test 6: Uses (callers/callees) parity ==="
UsesPairs=("login_user:callers" "start_session:callees" "pad_left:callers")
for pair in "${UsesPairs[@]}"; do
    name="${pair%%:*}"
    role="${pair##*:}"
    OLD_USES=$(uses_norm "$OLD_BIN" "$FIXTURE_OLD" "$name" "$role")
    NEW_USES=$(uses_norm "$NEW_BIN" "$FIXTURE_NEW" "$name" "$role")

    if [ "$name" = "login_user" ] && [ "$role" = "callers" ] && [ "$OLD_USES" != "ERROR" ]; then
        TOTAL=$(printf '%s\n' "$OLD_USES" | grep '^total_edges=' | cut -d= -f2)
        if [ "${TOTAL:-0}" -eq 0 ] 2>/dev/null; then
            record_fail "uses target='login_user' role='callers' -- expected at least one caller edge in the OLD binary's own output but found none"
        fi
    fi

    compare_values "uses target='$name' role='$role'" "$OLD_USES" "$NEW_USES"
done

# ---------------------------------------------------------------------------
# Test 7: `search --scope code` hit-SET parity
#
# `--scope code` is a new-binary-only ranked mode; the old binary has no
# such flag. What must hold is: the ranked hit SET (ignoring order, which is
# expected to legitimately differ) equals the old binary's unranked
# baseline set -- ranking must never drop or duplicate a match.
# ---------------------------------------------------------------------------

echo "=== Test 7: search --scope code hit-set parity ==="
for pat in "${PATTERNS[@]}"; do
    OLD_BASELINE=$(search_hitset "$OLD_BIN" "$FIXTURE_OLD" "$pat")
    NEW_SCOPED=$(search_hitset "$NEW_BIN" "$FIXTURE_NEW" "$pat" --scope code)
    compare_values "scope=code hit-set parity pattern='$pat'" "$OLD_BASELINE" "$NEW_SCOPED"
done

# ---------------------------------------------------------------------------
# Test 8: Daemon code path parity
# ---------------------------------------------------------------------------

echo "=== Test 8: daemon code path parity ==="

start_daemon() {
    local bin="$1" root="$2"
    (cd "$root" && "$bin" daemon start . >/dev/null 2>&1) || true
}

stop_daemon() {
    local bin="$1" root="$2"
    (cd "$root" && "$bin" daemon stop . >/dev/null 2>&1) || true
}

daemon_search_hitset() {
    # Same as search_hitset but deliberately WITHOUT --no-daemon, so this
    # actually exercises the daemon wire format instead of the in-process
    # path. Same --limit 10000 rationale as search_hitset() above.
    local bin="$1" root="$2" pattern="$3"
    local raw rc
    raw=$(cd "$root" && "$bin" search "$pattern" . --json --limit 10000 2>/dev/null)
    rc=$?
    if [ $rc -ne 0 ]; then
        echo "ERROR"
        return
    fi
    printf '%s' "$raw" | python3 "$WORKDIR/norm_search.py"
}

start_daemon "$OLD_BIN" "$FIXTURE_OLD"
start_daemon "$NEW_BIN" "$FIXTURE_NEW"
sleep 1

DAEMON_STATUS_OLD=$(cd "$FIXTURE_OLD" && "$OLD_BIN" daemon status . 2>&1)
DAEMON_STATUS_NEW=$(cd "$FIXTURE_NEW" && "$NEW_BIN" daemon status . 2>&1)
if printf '%s' "$DAEMON_STATUS_OLD" | grep -qi running; then
    record_pass "old daemon reports running"
else
    record_fail "old daemon does not report running ('$DAEMON_STATUS_OLD')"
fi
if printf '%s' "$DAEMON_STATUS_NEW" | grep -qi running; then
    record_pass "new daemon reports running"
else
    record_fail "new daemon does not report running ('$DAEMON_STATUS_NEW')"
fi

for pat in "login_user" "session"; do
    OLD_DHITS=$(daemon_search_hitset "$OLD_BIN" "$FIXTURE_OLD" "$pat")
    NEW_DHITS=$(daemon_search_hitset "$NEW_BIN" "$FIXTURE_NEW" "$pat")
    compare_values "daemon: search hit set pattern='$pat'" "$OLD_DHITS" "$NEW_DHITS"
done

OLD_DSYM=$(symbol_norm "$OLD_BIN" "$FIXTURE_OLD" "login_user")
NEW_DSYM=$(symbol_norm "$NEW_BIN" "$FIXTURE_NEW" "login_user")
compare_values "daemon: symbol lookup name='login_user'" "$OLD_DSYM" "$NEW_DSYM"

OLD_DTIERS=$(target_tiers "$OLD_BIN" "$FIXTURE_OLD" "fix login_user auth flow")
NEW_DTIERS=$(target_tiers "$NEW_BIN" "$FIXTURE_NEW" "fix login_user auth flow")
compare_values "daemon: target tiers task='fix login_user auth flow'" "$OLD_DTIERS" "$NEW_DTIERS"

# Stop explicitly here (in addition to the EXIT trap, which is the actual
# guarantee against a leaked daemon if anything above dies unexpectedly).
stop_daemon "$OLD_BIN" "$FIXTURE_OLD"
stop_daemon "$NEW_BIN" "$FIXTURE_NEW"

# ---------------------------------------------------------------------------
# Test 9 (opt-in): PARITY_REPO -- same family of checks against a real,
# larger repo instead of the tiny hardcoded fixture.
# ---------------------------------------------------------------------------

if [ -n "${PARITY_REPO:-}" ]; then
    echo ""
    echo "=== Test 9: PARITY_REPO larger-repo comparisons ($PARITY_REPO) ==="
    if [ ! -d "$PARITY_REPO/.git" ]; then
        record_fail "PARITY_REPO='$PARITY_REPO' is not a git repository (needs a committed HEAD to snapshot)"
    else
        REPO_OLD="$WORKDIR/repo-old"
        REPO_NEW="$WORKDIR/repo-new"
        mkdir -p "$REPO_OLD" "$REPO_NEW"

        # Snapshot the repo's committed HEAD tree into two independent,
        # from-scratch git repos (avoids copying build artifacts / .git
        # history and avoids either binary's index/graph state leaking
        # into the source repo under test).
        snapshot_repo() {
            local src="$1" dest="$2"
            (cd "$src" && git archive HEAD) | tar -x -C "$dest"
            (
                cd "$dest"
                git init -q
                git config user.email "t@t"
                git config user.name "t"
                git add -A
                git commit -qm "parity-repo snapshot" >/dev/null
            )
        }
        snapshot_repo "$PARITY_REPO" "$REPO_OLD"
        snapshot_repo "$PARITY_REPO" "$REPO_NEW"

        "$OLD_BIN" index "$REPO_OLD" >/dev/null 2>&1 || true
        "$NEW_BIN" index "$REPO_NEW" >/dev/null 2>&1 || true
        "$OLD_BIN" graph "$REPO_OLD" --json >/dev/null 2>&1 || true
        "$NEW_BIN" graph "$REPO_NEW" --json >/dev/null 2>&1 || true

        DISCOVER_TASK="fix bug in error handling"
        DISCOVERY_RAW=$(cd "$REPO_OLD" && "$OLD_BIN" targets "$DISCOVER_TASK" --json --no-manifest . 2>/dev/null)
        DISCOVERY_RC=$?

        SYMS=()
        if [ $DISCOVERY_RC -eq 0 ]; then
            while IFS=$'\t' read -r dname duid; do
                if [ -n "$duid" ]; then
                    SYMS+=("${dname}:::${duid}")
                fi
            done < <(printf '%s' "$DISCOVERY_RAW" | python3 "$WORKDIR/discover_symbols.py" 3)
        fi

        if [ ${#SYMS[@]} -eq 0 ]; then
            record_fail "PARITY_REPO discovery -- could not extract any real symbol name/uid from 'targets \"$DISCOVER_TASK\"' output; nothing to compare"
        else
            echo "  discovered symbols: ${SYMS[*]}"

            # 9a: search hit-set parity, generic patterns (assumes a
            # Rust-ish repo, since that's what pixel/gitpixel itself is).
            for pat in "fn " "struct " "impl "; do
                OLD_H=$(search_hitset "$OLD_BIN" "$REPO_OLD" "$pat")
                NEW_H=$(search_hitset "$NEW_BIN" "$REPO_NEW" "$pat")
                compare_values "parity-repo: search hit set pattern='$pat'" "$OLD_H" "$NEW_H"
            done

            # 9b: target tier parity for the same discovery task.
            OLD_T=$(target_tiers "$OLD_BIN" "$REPO_OLD" "$DISCOVER_TASK")
            NEW_T=$(target_tiers "$NEW_BIN" "$REPO_NEW" "$DISCOVER_TASK")
            compare_values "parity-repo: target tiers task='$DISCOVER_TASK'" "$OLD_T" "$NEW_T"

            # 9c/9d/9e: symbol / impact / uses parity for discovered symbols.
            # impact/uses use the exact uid (never the bare name) because a
            # real repo can easily have multiple symbols sharing a name,
            # which would otherwise resolve to an ambiguous "candidates"
            # response instead of a comparable result.
            for entry in "${SYMS[@]}"; do
                dname="${entry%%:::*}"
                duid="${entry##*:::}"

                OLD_SYM=$(symbol_norm "$OLD_BIN" "$REPO_OLD" "$dname")
                NEW_SYM=$(symbol_norm "$NEW_BIN" "$REPO_NEW" "$dname")
                compare_values "parity-repo: symbol lookup name='$dname'" "$OLD_SYM" "$NEW_SYM"

                OLD_IMP=$(impact_norm "$OLD_BIN" "$REPO_OLD" "$duid" upstream)
                NEW_IMP=$(impact_norm "$NEW_BIN" "$REPO_NEW" "$duid" upstream)
                compare_values "parity-repo: impact uid='$duid'" "$OLD_IMP" "$NEW_IMP"

                OLD_USE=$(uses_norm "$OLD_BIN" "$REPO_OLD" "$duid" callers)
                NEW_USE=$(uses_norm "$NEW_BIN" "$REPO_NEW" "$duid" callers)
                compare_values "parity-repo: uses(callers) uid='$duid'" "$OLD_USE" "$NEW_USE"
            done

            # 9f: scope parity on the larger corpus too.
            #
            # NOTE (observed against gitpixel itself, ~550 matches for "fn "):
            # this can genuinely FAIL at this scale even with --limit 10000
            # requested on both sides. The mismatch isn't line-count
            # truncation (both sides return a similar total) -- entire
            # files' matches are present on one side and absent on the
            # other, which points at --scope code applying some internal
            # candidate-window/threshold before reranking, not just
            # reordering the full hit set as its --help text promises. The
            # small always-on fixture (Test 7) never approaches whatever
            # that window is, so it stays green regardless. Do not "fix"
            # this by loosening the comparison -- a real discrepancy here
            # is exactly what the opt-in larger-repo mode exists to catch;
            # treat a failure as a genuine finding to investigate in the
            # search/rank implementation, not a flaky harness.
            for pat in "fn " "struct "; do
                OLD_BASELINE=$(search_hitset "$OLD_BIN" "$REPO_OLD" "$pat")
                NEW_SCOPED=$(search_hitset "$NEW_BIN" "$REPO_NEW" "$pat" --scope code)
                compare_values "parity-repo: scope=code hit-set parity pattern='$pat'" "$OLD_BASELINE" "$NEW_SCOPED"
            done
        fi
    fi
else
    echo ""
    echo "=== Test 9: PARITY_REPO larger-repo comparisons -- SKIPPED (set PARITY_REPO=<path> to enable) ==="
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo ""
echo "Results: $PASS_COUNT passed, $FAIL_COUNT failed"
if [ "$FAIL_COUNT" -eq 0 ]; then
    echo "PARITY VERIFIED: all tests passed"
    exit 0
else
    echo "PARITY BROKEN: one or more tests failed"
    exit 1
fi
