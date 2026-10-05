#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Contract for the opt-in pre-push mutants gate: by default no campaign runs
# (CI is the verdict); `PIXEL_MUTANTS_GATE=local` runs the CI-equivalent
# campaign against the hook's base and blocks on its exit code; any other
# value refuses the push instead of silently skipping the gate.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/pixel-mutants-push-gate-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
fixture="$tmp/repo"
mkdir -p "$fixture/scripts"
cp "$repo/scripts/mutants-push-gate.sh" "$fixture/scripts/"
git -C "$fixture" init -q

# The campaign stub records what it was asked to judge and exits with the
# status the case hands it, the way cargo-mutants' exit codes come back.
cat > "$fixture/scripts/mutants-preflight.sh" <<'STUB'
#!/bin/sh
printf 'preflight %s base=%s\n' "$*" "${PIXEL_MUTANTS_BASE:-missing}" >> "$CALL_LOG"
exit "${CAMPAIGN_STATUS:-0}"
STUB

gate() {
    (cd "$fixture" && CALL_LOG="$tmp/calls.log" sh scripts/mutants-push-gate.sh)
}

# Unset and `off`: nothing runs, the push goes through.
for value in unset off; do
    : > "$tmp/calls.log"
    if [ "$value" = unset ]; then
        out=$(unset PIXEL_MUTANTS_GATE; gate)
    else
        out=$(PIXEL_MUTANTS_GATE=off gate)
    fi
    test ! -s "$tmp/calls.log" || { echo "gate $value ran a campaign" >&2; exit 1; }
    case "$out" in *"automatic mutations run nightly on main"*) ;; *)
        echo "gate $value did not say CI keeps the verdict: $out" >&2; exit 1 ;;
    esac
done

# `local`: the campaign runs against the hook's base, and its verdict is the
# push's -- a survivor (2), a timeout (3) or a broken baseline (4) blocks it.
for status in 0 2 3 4; do
    : > "$tmp/calls.log"
    got=0
    PIXEL_MUTANTS_GATE=local PIXEL_MUTANTS_BASE=fork-point CAMPAIGN_STATUS=$status \
        gate > /dev/null || got=$?
    test "$got" -eq "$status" || {
        echo "local gate exited $got for a campaign that exited $status" >&2; exit 1; }
    test "$(cat "$tmp/calls.log")" = 'preflight --run base=fork-point'
done

# A value the gate does not know -- `on` from the retired gate host included --
# refuses the push rather than reading as `off`.
: > "$tmp/calls.log"
if PIXEL_MUTANTS_GATE=on gate > "$tmp/unknown.out" 2>&1; then
    echo "an unknown PIXEL_MUTANTS_GATE value let the push through" >&2
    exit 1
fi
grep -q "neither off nor local" "$tmp/unknown.out"
test ! -s "$tmp/calls.log"

echo "mutants push gate contract: ok"
