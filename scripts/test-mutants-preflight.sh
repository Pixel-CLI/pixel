#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Contract for the fast mutation-exposure preflight, using a disposable repo
# and a fake cargo so no mutation campaign or build runs.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/pixel-mutants-preflight-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
fixture="$tmp/repo"
mkdir -p "$fixture/scripts" "$fixture/crates/demo/src" "$fixture/.github/workflows" "$tmp/bin"
cp "$repo/scripts/mutants-preflight.sh" "$repo/scripts/mutants-version-check.sh" \
    "$repo/scripts/mutants-toolchain.sh" "$fixture/scripts/"
printf '      - uses: taiki-e/install-action@x\n        with:\n          tool: cargo-mutants@27.1.0\n' \
    > "$fixture/.github/workflows/mutants.yml"

# The campaign lane runs on the pinned nightly (libtest --fail-fast needs
# it), installed and exported by scripts/mutants-toolchain.sh; the stub
# records what the lane asked rustup for and reports the pin as installed,
# so the contract below can pin both halves.
cat > "$tmp/bin/rustup" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$RUSTUP_LOG"
case "$*" in
    "toolchain list") echo "nightly-2026-05-12-x (default)" ;;
    "toolchain install"*) : ;;
    *) echo "rustup stub: unexpected: $*" >&2; exit 9 ;;
esac
EOF
chmod +x "$tmp/bin/rustup"

cat > "$tmp/bin/cargo" <<'EOF'
#!/bin/sh
printf '%s\n' "${RUSTUP_TOOLCHAIN:-unset} $*" >> "$CARGO_LOG"
case "$*" in
    "mutants --version")
        echo "cargo-mutants ${CARGO_MUTANTS_VERSION:-27.1.0}"
        exit 0
        ;;
    "mutants --list"*)
        if [ "${CARGO_FAIL:-0}" = 1 ]; then
            echo "fake cargo failure" >&2
            exit 7
        fi
        ;;
    *)
        if [ "${CARGO_RUN_FAIL:-0}" = 1 ]; then
            echo "fake cargo run failure" >&2
            exit 5
        fi
        ;;
esac
printf '%b' "${CARGO_LISTING:-}"
EOF
chmod +x "$tmp/bin/cargo"

git -C "$fixture" init -q
git -C "$fixture" config user.email test@example.com
git -C "$fixture" config user.name test
printf 'fn base() {}\n' > "$fixture/crates/demo/src/lib.rs"
git -C "$fixture" add .
git -C "$fixture" commit -qm base
git -C "$fixture" branch -M main
git -C "$fixture" update-ref refs/remotes/origin/main HEAD
printf 'fn changed() {}\n' >> "$fixture/crates/demo/src/lib.rs"
git -C "$fixture" add crates/demo/src/lib.rs
git -C "$fixture" commit -qm rust-change

run() {
    PATH="$tmp/bin:$PATH" CARGO_LOG="$tmp/cargo.log" RUSTUP_LOG="$tmp/rustup.log" \
        CARGO_LISTING='crates/demo/src/lib.rs:2:1: replace changed -> ()\n' \
        sh "$fixture/scripts/mutants-preflight.sh" "$@"
}

if (cd "$fixture" && run --check > "$tmp/blocked.out" 2>&1); then
    echo "expected an unacknowledged Rust change to block" >&2
    exit 1
fi
grep -q 'scripts/mutants-preflight.sh --ack' "$tmp/blocked.out"
# The listing lane runs on whatever toolchain is active: no pin prefix.
grep -q '^unset mutants --list --in-diff ' "$tmp/cargo.log"

(cd "$fixture" && run --ack > "$tmp/ack.out")
(cd "$fixture" && run --check > "$tmp/allowed.out")
grep -q 'reviewed receipt matches' "$tmp/allowed.out"

printf 'fn another() {}\n' >> "$fixture/crates/demo/src/lib.rs"
git -C "$fixture" add crates/demo/src/lib.rs
git -C "$fixture" commit -qm another-rust-change
if (cd "$fixture" && run --check > "$tmp/stale.out" 2>&1); then
    echo "expected a receipt for an older commit to block" >&2
    exit 1
fi
grep -q 'push blocked' "$tmp/stale.out"

git -C "$fixture" checkout -q -b docs-only refs/remotes/origin/main
printf 'docs\n' > "$fixture/README.md"
git -C "$fixture" add README.md
git -C "$fixture" commit -qm docs
: > "$tmp/cargo.log"
(cd "$fixture" && run --check > "$tmp/docs.out")
grep -q 'not applicable' "$tmp/docs.out"
test ! -s "$tmp/cargo.log"

git -C "$fixture" checkout -q -b dirty-rust refs/remotes/origin/main
printf 'fn unstaged() {}\n' >> "$fixture/crates/demo/src/lib.rs"
: > "$tmp/cargo.log"
if (cd "$fixture" && run --check > "$tmp/dirty.out" 2>&1); then
    echo "expected tracked, uncommitted Rust to block before listing" >&2
    exit 1
fi
grep -q 'commit or stash' "$tmp/dirty.out"
test ! -s "$tmp/cargo.log"

git -C "$fixture" add crates/demo/src/lib.rs
git -C "$fixture" commit -qm dirty-rust-change
if (cd "$fixture" && CARGO_FAIL=1 run --check > "$tmp/cargo-failure.out" 2>&1); then
    echo "expected a failed cargo-mutants listing to block" >&2
    exit 1
fi
grep -q 'cargo mutants --list failed' "$tmp/cargo-failure.out"

: > "$tmp/cargo.log"
: > "$tmp/rustup.log"
(cd "$fixture" && run --run > "$tmp/run.out")
grep -q 'local run caught every listed mutant' "$tmp/run.out"
# Every campaign cargo call runs under the pinned nightly; the listing
# lanes above ran with no RUSTUP_TOOLCHAIN (the pin would prefix their
# cargo log lines -- the --check greps above would have failed).
grep -q '^nightly-2026-05-12 mutants -vV --no-shuffle --in-place --iterate --in-diff ' "$tmp/cargo.log"
if grep -q '^unset mutants -vV' "$tmp/cargo.log"; then
    echo "expected the campaign to run under the pinned nightly" >&2
    exit 1
fi
# .cargo/mutants.toml owns --all-targets and --locked for every lane.
if grep -Eq -- '--all-targets|--locked' "$tmp/cargo.log"; then
    echo "expected the local run to leave --all-targets and --locked to .cargo/mutants.toml" >&2
    exit 1
fi

: > "$tmp/cargo.log"
if (cd "$fixture" && CARGO_MUTANTS_VERSION=26.0.0 run --run > "$tmp/run-version.out" 2>&1); then
    echo "expected a cargo-mutants other than CI's pin to block the local run" >&2
    exit 1
fi
grep -q "local cargo-mutants is '26.0.0', CI runs 27.1.0" "$tmp/run-version.out"
if grep -q '^mutants -vV' "$tmp/cargo.log"; then
    echo "expected the version check to stop before any mutant ran" >&2
    exit 1
fi

: > "$tmp/cargo.log"
(cd "$fixture" && run --run 'changed|other' > "$tmp/run-filter.out")
grep -q -- "-F 'changed|other'\|-F changed|other" "$tmp/cargo.log" \
    || grep -q 'F.*changed|other' "$tmp/cargo.log"
# A filtered run judges a different slice: it must not iterate off the full
# run's outcomes.
if grep -q -- '--iterate' "$tmp/cargo.log"; then
    echo "expected a filtered run to skip --iterate" >&2
    exit 1
fi

if (cd "$fixture" && CARGO_RUN_FAIL=1 run --run > "$tmp/run-fail.out" 2>&1); then
    echo "expected a failed local run to exit nonzero" >&2
    exit 1
fi
grep -q 'cargo mutants exited 5' "$tmp/run-fail.out"
test ! -d "$fixture/tree"
git -C "$fixture" worktree list | grep -c . | grep -q '^1$'

echo "mutants preflight contract: ok"
