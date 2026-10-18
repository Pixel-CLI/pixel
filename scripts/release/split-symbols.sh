#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Split a release binary into what users download and what a backtrace needs:
# archive the unstripped binary as the symbols artifact, then strip the
# binary in place. Release builds run unstripped (release-build-env.sh sets
# CARGO_PROFILE_RELEASE_STRIP=none) so one build yields both; stripping moves
# no code, so the archived copy names the frames of the shipped one (same
# Mach-O UUID, same text section).
#
# The weight is the symbol table, not DWARF (the release profile builds no
# debug info): on aarch64-apple-darwin 76 135 896 bytes unstripped,
# 63 425 448 stripped.
#
# Tools: Apple's `strip` on macOS, which keeps the linker's ad-hoc signature
# valid (an arm64 binary with a broken one is killed at exec); llvm-strip
# from the toolchain's llvm-tools component elsewhere, which strips any
# target's ELF from the build host. PIXEL_STRIP names another tool.
#
# usage: split-symbols.sh <binary> <symbols-archive.tar.xz>
set -eu

if [ $# -ne 2 ]; then
  echo "usage: split-symbols.sh <binary> <symbols-archive.tar.xz>" >&2
  exit 2
fi
bin=$1
archive=$2
if [ ! -f "$bin" ]; then
  echo "split-symbols: no binary at $bin" >&2
  exit 2
fi

strip_tool() {
  if [ -n "${PIXEL_STRIP:-}" ]; then
    echo "$PIXEL_STRIP"
    return
  fi
  if [ "$(uname -s)" = Darwin ]; then
    echo /usr/bin/strip
    return
  fi
  sysroot=$(rustc --print sysroot)
  host=$(rustc -vV | sed -n 's/^host: //p')
  tool="$sysroot/lib/rustlib/$host/bin/llvm-strip"
  if [ -x "$tool" ]; then
    echo "$tool"
    return
  fi
  echo "split-symbols: no llvm-strip at $tool; run: rustup component add llvm-tools" >&2
  return 1
}

tool=$(strip_tool)
before=$(wc -c <"$bin" | tr -d ' ')

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp "$bin" "$stage/pixel"
tar -C "$stage" -cJf "$archive" pixel

case "$tool" in
  *llvm-strip) "$tool" --strip-all "$bin" ;;
  *) "$tool" "$bin" ;;
esac
if [ "$(uname -s)" = Darwin ] && ! codesign --verify "$bin" 2>/dev/null; then
  echo "split-symbols: $bin lost a valid code signature" >&2
  exit 1
fi

after=$(wc -c <"$bin" | tr -d ' ')
if [ "$after" -ge "$before" ]; then
  echo "split-symbols: $bin did not shrink ($before -> $after bytes)" >&2
  exit 1
fi
echo "stripped $bin: $before -> $after bytes; symbols in $archive"
