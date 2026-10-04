#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

set -eu
# The published installer always resolves the latest release, so this mode
# tests whatever that is today; the evidence records the version it chose.
url=https://github.com/Pixel-CLI/pixel/releases/latest/download/install.sh
curl --fail --silent --show-error --location --max-time 120 "$url" -o /evidence/install.sh
dir=/home/tester/opt/pixel/bin
# Run as the user it installs for, into a directory outside the default.
su - tester -s /bin/sh -c "PIXEL_INSTALL_DIR=$dir sh /evidence/install.sh" > /evidence/installer.log 2>&1 \
    || { cat /evidence/installer.log; exit 1; }
cat /evidence/installer.log
grep -qx "Installed pixel to $dir/pixel" /evidence/installer.log
"$dir/pixel" --version > /evidence/binary-version.txt
cat /evidence/binary-version.txt
printf 'PIXEL_BIN_DIR=%s\nPIXEL_OWNS_BINARY=1\n' "$dir" > /etc/pixel-smoke.env
