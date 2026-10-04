#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Write the homebrew-core formula of a release: pixel built from source.

    python3 scripts/homebrew-core-formula.py <tag> <source-tarball> <out-file>

`<source-tarball>` is the archive GitHub serves for the tag
(`https://github.com/Pixel-CLI/pixel/archive/refs/tags/<tag>.tar.gz`); its
sha256 goes into the formula. `PIXEL_SOURCE_URL` replaces that URL, so CI can
build the formula from a `git archive` of the tree under test.

Why a second formula: the tap installs the release's prebuilt binary, which
homebrew-core does not accept (it builds every formula from source and makes
its own bottles). This one is what a homebrew-core pull request carries:

- the tag's source archive, checksummed, never a branch;
- `cargo install` with `--no-default-features --features model2vec`: the
  default `fastembed` feature downloads a prebuilt ONNX Runtime while it
  builds and vendors OpenSSL, which homebrew-core refuses; model2vec is the
  pure-Rust backend the Linux release already ships;
- a test that indexes and searches a file offline, with
  `PIXEL_DAEMON_AUTO_START=0` so no daemon outlives `brew test`;
- built with `PIXEL_UPDATE_CHECK=off`: that binary never checks for a
  release nor offers to upgrade itself, Homebrew owning updates;
- no caveats and nothing written outside the keg: `pixel install`, which
  wires the agents in the user's home, stays the user's step.

`.github/workflows/homebrew-core.yml` builds it from source on macOS and
Linux, runs `brew test` and `brew audit --strict --new`, and
`scripts/test-homebrew-core-formula.py` pins its text.
"""

import hashlib
import os
import sys
from pathlib import Path

DEFAULT_SOURCE_URL = "https://github.com/Pixel-CLI/pixel/archive/refs/tags/{tag}.tar.gz"


def formula(url: str, sha256: str) -> str:
    return f'''class Pixel < Formula
  desc "Local control layer for coding agents: deterministic retrieval and git engine"
  homepage "https://pixel-cli.dev/"
  url "{url}"
  sha256 "{sha256}"
  license "MIT"
  head "https://github.com/Pixel-CLI/pixel.git", branch: "main"

  livecheck do
    url :stable
    strategy :github_latest
  end

  depends_on "rust" => :build

  def install
    # Homebrew owns updates: no release check, notice or upgrade offer.
    ENV["PIXEL_UPDATE_CHECK"] = "off"
    # The default `fastembed` feature downloads a prebuilt ONNX Runtime at
    # build time; `model2vec` is the pure-Rust embeddings backend.
    system "cargo", "install", "--no-default-features", "--features", "model2vec",
           *std_cargo_args(path: "crates/pixel")
  end

  test do
    # Commands start a background daemon by default; a test must not leave one.
    ENV["PIXEL_DAEMON_AUTO_START"] = "0"
    ENV["PIXEL_METRICS"] = "0"
    ENV["PIXEL_NO_UPDATE_CHECK"] = "1"
    (testpath/"m.py").write <<~PYTHON
      def helper_one(x):
          return x + 1


      class Box:
          def open(self):
              return helper_one(2)
    PYTHON

    assert_match "def helper_one(x):", shell_output("#{{bin}}/pixel list-signatures m.py")
    assert_match "m.py:7:        return helper_one(2)",
                 shell_output("#{{bin}}/pixel search-content -F helper_one .")
    assert_match "pixel #{{version}}", shell_output("#{{bin}}/pixel --version")
  end
end
'''


def main(argv: list) -> int:
    if len(argv) != 3 or not argv[0].startswith("v"):
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        return 2
    tag, tarball, out = argv[0], Path(argv[1]), Path(argv[2])
    if not tarball.is_file():
        print(f"{tarball}: no such source archive", file=sys.stderr)
        return 1
    url = os.environ.get("PIXEL_SOURCE_URL") or DEFAULT_SOURCE_URL.format(tag=tag)
    digest = hashlib.sha256(tarball.read_bytes()).hexdigest()
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(formula(url, digest))
    print(out)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
