#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

set -eu
# Homebrew's prefix belongs to linuxbrew and is not readable by other users,
# so that user runs both the formula install and the checks, as on a Mac
# where the person who installed Homebrew runs pixel.
brew=/home/linuxbrew/.linuxbrew/bin/brew
chown linuxbrew:linuxbrew /evidence
su linuxbrew -c "$brew install LivioGama/tap/pixel" > /evidence/brew-install.log 2>&1 \
    || { cat /evidence/brew-install.log; exit 1; }
tail -3 /evidence/brew-install.log
su linuxbrew -c "$brew deps --installed LivioGama/tap/pixel" > /evidence/brew-deps.txt
su linuxbrew -c "$brew info --json=v2 LivioGama/tap/pixel" > /evidence/brew-info.json
# On a host whose glibc or libstdc++ is older than Homebrew's CI, Homebrew
# gives every formula, bottled or not, an implicit gcc and glibc: the
# formula cannot opt out. A control formula with no dependency measures what
# this host adds to any formula; only a dependency beyond it is pixel's.
su linuxbrew -c "$brew ruby -e 'puts \"system glibc: #{OS::Linux::Glibc.system_version}\",
    \"ci glibc: #{OS::LINUX_GLIBC_CI_VERSION}\",
    \"needs libc formula: #{DevelopmentTools.needs_libc_formula?}\",
    \"needs compiler formula: #{DevelopmentTools.needs_compiler_formula?}\"'" \
    2>/dev/null | grep -E '^(system|ci|needs) ' > /evidence/brew-host.txt
cat /evidence/brew-host.txt
su linuxbrew -c "$brew tap-new local/smoke --no-git" > /dev/null
cat > /tmp/control.rb <<'RB'
class Control < Formula
  desc "Formula without dependencies: what the host alone adds to any formula"
  homepage "https://example.com"
  url "https://example.com/control-1.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  def install; end
end
RB
su linuxbrew -c "cp /tmp/control.rb \"\$($brew --repository)/Library/Taps/local/homebrew-smoke/Formula/\""
su linuxbrew -c "$brew deps local/smoke/control" | sort -u > /evidence/brew-host-deps.txt
own=$(sort -u /evidence/brew-deps.txt | comm -23 - /evidence/brew-host-deps.txt)
if [ -n "$own" ]; then
    echo "FAIL the formula pulled dependencies of its own: $own" >&2
    exit 1
fi
if [ -s /evidence/brew-deps.txt ]; then
    echo "NOTE Homebrew gives this host's toolchain to every formula, pixel's $(wc -l < /evidence/brew-deps.txt) dependencies included (brew-host.txt, brew-host-deps.txt)"
fi
echo 'PASS the Homebrew formula pulls no dependency of its own'
dir=/home/linuxbrew/.linuxbrew/bin
"$dir/pixel" --version > /evidence/binary-version.txt
cat /evidence/binary-version.txt
printf 'PIXEL_BIN_DIR=%s\nPIXEL_OWNS_BINARY=0\n' "$dir" > /etc/pixel-smoke.env
