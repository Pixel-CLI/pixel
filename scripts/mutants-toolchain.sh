#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# The toolchain every mutation campaign runs on, sourced by the lanes that
# run mutants: scripts/mutants-preflight.sh --run (also what the remote gate
# host executes through scripts/mutants-gate-host.sh) and scripts/gates.sh
# --mutants. CI installs the same pin through dtolnay/rust-toolchain in the
# shard jobs (.github/workflows/mutants.yml, mutants-nightly.yml).
#
# Why a nightly: .cargo/mutants.toml hands libtest `-Zunstable-options
# --fail-fast` so a caught mutant's suite stops at the first failing test
# instead of finishing every remaining one; stable libtest rejects the flag
# ("only accepted on the nightly compiler"). Nightlies after 2025-09-18
# carry it (rust-lang/rust#142859); the maintainer of cargo-mutants
# measured about half the campaign time on their own tree
# (sourcefrog/cargo-mutants#531). The pin is dated so a campaign one month
# apart runs the same compiler; move it with the workflow's cargo-mutants
# pin, never on its own.
#
# Sourcing installs the toolchain when missing (a one-time download, like
# `cargo install cargo-mutants` already is for the lanes) and exports
# RUSTUP_TOOLCHAIN, which rustup's cargo proxy honors ahead of the default
# toolchain -- so the campaign runs under the pin without touching any
# other cargo use on the machine. `cargo mutants --list` never runs tests
# and stays on whatever toolchain is active.
PIXEL_MUTANTS_TOOLCHAIN=nightly-2026-05-12

if [ "${PIXEL_MUTANTS_TOOLCHAIN_SOURCED:-}" = 1 ]; then
    return 0 2>/dev/null || exit 0
fi
PIXEL_MUTANTS_TOOLCHAIN_SOURCED=1

command -v rustup >/dev/null 2>&1 || {
    echo "mutants toolchain: rustup is required to run the campaign" >&2
    exit 2
}
if ! rustup toolchain list | grep -q "^$PIXEL_MUTANTS_TOOLCHAIN"; then
    echo "mutants toolchain: installing $PIXEL_MUTANTS_TOOLCHAIN (one-time)"
    rustup toolchain install --profile minimal "$PIXEL_MUTANTS_TOOLCHAIN"
fi
export RUSTUP_TOOLCHAIN="$PIXEL_MUTANTS_TOOLCHAIN"
