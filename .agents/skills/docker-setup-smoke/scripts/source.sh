#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

set -eu
git init -q /source
cd /source
git remote add origin https://github.com/Pixel-CLI/pixel.git
git fetch --depth 1 origin "$PIXEL_SOURCE_REF"
git checkout --detach FETCH_HEAD
sha=$(git rev-parse HEAD)
printf 'source-ref: %s\nsource-sha: %s\n' "$PIXEL_SOURCE_REF" "$sha" > /evidence/source.txt
rustc --version
cargo --version
# Match the shipped Linux feature set; omit debug info for a smaller smoke build.
export CARGO_PROFILE_DEV_DEBUG=0
echo 'Building: cargo build --locked -j 4 -p pixel-cli --no-default-features --features model2vec'
cargo build --locked -j 4 -p pixel-cli --no-default-features --features model2vec
install -o tester -g tester -m 755 target/debug/pixel /home/tester/.local/bin/pixel
/home/tester/.local/bin/pixel --version > /evidence/binary-version.txt
cat /evidence/binary-version.txt
grep -qx "commit: $sha" /evidence/binary-version.txt
printf 'PIXEL_BIN_DIR=/home/tester/.local/bin\nPIXEL_OWNS_BINARY=1\n' > /etc/pixel-smoke.env
