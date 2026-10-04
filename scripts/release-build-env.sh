#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# The build environment that makes a release binary reproducible: the same
# commit, built with the same toolchain and the same cross image, gives the
# same bytes wherever the checkout and CARGO_HOME sit.
#
# - SOURCE_DATE_EPOCH: the commit's committer time. crates/pixel/build.rs
#   embeds the build date `pixel --version` prints; without it the date is
#   the day of the build.
# - RUSTFLAGS: --remap-path-prefix for the checkout and CARGO_HOME, the two
#   absolute paths rustc writes into panic messages and debug information
#   (the release profile keeps symbols: strip = "none"). Under cross the
#   container sees the checkout at its host path and CARGO_HOME at /cargo,
#   which the second flag already names; cross forwards RUSTFLAGS by itself
#   and Cross.toml forwards SOURCE_DATE_EPOCH.
#
# release-build.yml, cross-build.yml (whose cache the release build restores:
# rust-cache hashes RUSTFLAGS into its key) and reproducible-build.yml all run
# it from the checkout's root. SECURITY.md, "Reproducing a release build".
#
# Usage, from anywhere in the checkout (the remap names its root):
#   env_lines=$(scripts/release-build-env.sh) && eval "$env_lines"
#   scripts/release-build-env.sh --github-env   # appends to $GITHUB_ENV
# Assign first, then eval: `eval "$(...)"` runs an empty string, and
# succeeds, when the script fails.
set -eu

# The worktree root, physical: rustc sees the real path, and a remap of the
# caller's subdirectory or of a symlinked spelling would match nothing.
top=$(git rev-parse --show-toplevel)
root=$(cd "$top" && pwd -P)
cargo_home=${CARGO_HOME:-$HOME/.cargo}

# RUSTFLAGS splits on whitespace, and the export lines quote with '.
case "$root$cargo_home" in
  *[[:space:]\']*)
    echo "release-build-env: a path holds whitespace or a quote, which RUSTFLAGS cannot carry: $root, $cargo_home" >&2
    exit 1
    ;;
esac

epoch=$(git log -1 --format=%ct HEAD)
flags="--remap-path-prefix=$root=/pixel --remap-path-prefix=$cargo_home=/cargo"
if [ -n "${RUSTFLAGS:-}" ]; then
  flags="$RUSTFLAGS $flags"
fi

case "${1:-}" in
  --github-env)
    printf 'SOURCE_DATE_EPOCH=%s\nRUSTFLAGS=%s\n' "$epoch" "$flags" >> "$GITHUB_ENV"
    ;;
  "")
    printf "export SOURCE_DATE_EPOCH='%s'\nexport RUSTFLAGS='%s'\n" "$epoch" "$flags"
    ;;
  *)
    echo "usage: scripts/release-build-env.sh [--github-env]" >&2
    exit 2
    ;;
esac
