#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Local gate runner: the CI gates (fmt, clippy, test) with laptop-safe defaults.
#
#   scripts/gates.sh            # skip when nothing Rust-affecting changed
#   scripts/gates.sh --force    # run even when nothing changed
#
# Why a script instead of three commands:
# - Skip-if-untouched: outside CI, when neither the diff against `main` nor
#   the working tree touches a Rust-affecting path (*.rs, Cargo.*, build.rs,
#   .cargo/, rust-toolchain*, rustfmt.toml, clippy.toml), the CARGO gates
#   cannot change outcome, so the script stops without compiling anything. A
#   docs-only turn costs seconds, not a workspace build. The release prepare
#   contract is not a cargo gate and runs either way: it compiles nothing, and
#   a change to prepare.sh or to its own test must not need --force to be
#   checked.
# - Laptop safety: every cargo invocation runs under `nice` and with
#   CARGO_BUILD_JOBS defaulting to ncpu-2 (two cores stay free, peak rustc
#   memory drops with the job count) and RUST_TEST_THREADS to ncpu/2 (the
#   integration tests each spawn a pixel binary plus git; eight at once is
#   what pushed a 16 GB machine into swap). An explicit value in the
#   environment always wins.
# - Fail open: when git cannot answer (not a repo, no `main`), the gates run.
#
# CI=1 (set by GitHub Actions) disables the skip and the nice/jobs defaults so
# the workflow keeps running exactly the documented commands.
set -eu

FORCE=0
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        -h|--help) sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "gates.sh: unknown argument: $arg" >&2; exit 2 ;;
    esac
done

REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"

# Paths whose change can alter a gate's outcome. Anything else (docs, prompts,
# scripts, workflows) cannot make fmt/clippy/test go red.
rust_affecting() {
    grep -E -q '(^|/)(Cargo\.toml|Cargo\.lock|build\.rs|rust-toolchain(\.toml)?|rustfmt\.toml|clippy\.toml)$|\.rs$|(^|/)\.cargo/'
}

# Prints "run" when a gate could change outcome, "skip" when none can. Any
# git failure prints "run" (fail open).
gate_decision() {
    # Prefer the fetched remote default. A local `main` may be stale while
    # HEAD already contains newer upstream Rust changes, which would make a
    # scripts-only branch recompile the workspace for someone else's diff.
    base="$(git merge-base origin/main HEAD 2>/dev/null)" \
        || base="$(git merge-base main HEAD 2>/dev/null)" \
        || { echo run; return; }
    committed="$(git diff --name-only "$base" HEAD 2>/dev/null)" || { echo run; return; }
    dirty="$(git status --porcelain --untracked-files=all 2>/dev/null | cut -c4-)" || { echo run; return; }
    if printf '%s\n%s\n' "$committed" "$dirty" | rust_affecting; then echo run; else echo skip; fi
}

if [ -z "${CI:-}" ]; then
    ncpu="$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)"
    # Negative CARGO_BUILD_JOBS means "ncpu + value" (cargo ≥ 1.66).
    export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:--2}"
    half=$((ncpu / 2)); [ "$half" -lt 2 ] && half=2
    export RUST_TEST_THREADS="${RUST_TEST_THREADS:-$half}"
    NICE="nice -n ${GATES_NICE:-10}"
else
    NICE=""
fi

step() {
    name="$1"; shift
    printf '\n==> %s\n' "$name"
    start=$(date +%s)
    if $NICE "$@"; then
        printf '<== %s ok (%ss)\n' "$name" "$(( $(date +%s) - start ))"
    else
        code=$?
        printf '<== %s FAILED (exit %s, %ss)\n' "$name" "$code" "$(( $(date +%s) - start ))" >&2
        exit "$code"
    fi
}

# The script contracts CI runs as their own jobs. test-prepare.py stubs gh
# and cargo in a disposable repository and also runs `prepare.sh --check`
# against this tree, so it catches a fragment named for a section that does not
# exist, an entry left under ## [Unreleased], and a --check that refuses the
# tree release preparation produces; test-gates.py is this script's own
# contract; test-mutants-config.py refuses an exclude_globs entry that would
# take a module out of the mutation gate; test-clean.py holds clean.sh to what
# it must not remove (a tracked file, a tree behind a symlink, the index under
# the default scope, the daemon sockets beside the shard cache). None compiles
# anything (17 s for the four, from this script's own step timings), which is
# why they run
# above the cargo skip rather than under it: none of the paths they cover is
# Rust-affecting, so under the skip a change to prepare.sh, to this file or to
# either test would still have needed --force to be checked. 0.4.0's release
# pull request went red twice on gates no local run could reach.
step "release prepare contract" python3 scripts/test-prepare.py
step "release candidate contract" python3 scripts/test-release-candidate.py
step "gate runner contract" python3 scripts/test-gates.py
step "pre-push contract" sh scripts/test-pre-push.sh
step "CodeQL Rust scope contract" python3 scripts/test-codeql-rust-scope.py
step "nightly diff checkpoint contract" python3 scripts/test-mutants-nightly-range.py
step "mutants config contract" python3 scripts/test-mutants-config.py
step "clean contract" python3 scripts/test-clean.py
step "harness recorder contract" python3 scripts/test-harness-recorder.py

if [ "$FORCE" -eq 0 ] && [ -z "${CI:-}" ] && [ "$(gate_decision)" = skip ]; then
    echo
    echo "gates.sh: no Rust-affecting change against main or in the working tree; skipping the cargo gates (use --force to run them)."
    exit 0
fi

step "cargo fmt --check" cargo fmt --all -- --check
step "cargo clippy" cargo clippy --workspace --all-targets -- -D warnings
# nextest (what CI runs, .config/nextest.toml) when installed, else cargo
# test. --no-fail-fast: a red test binary must not hide the ones after it.
if cargo nextest --version >/dev/null 2>&1; then
    step "cargo nextest" cargo nextest run --workspace --locked --no-fail-fast
    step "cargo test --doc" cargo test --workspace --locked --doc
else
    step "cargo test" cargo test --workspace --locked --no-fail-fast
fi


echo
echo "gates.sh: all gates passed"
