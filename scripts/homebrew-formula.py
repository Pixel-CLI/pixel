#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Write the Homebrew formula of a release and its Linux bottles.

    python3 scripts/homebrew-formula.py <tag> <artifacts-dir> <out-dir>

`<artifacts-dir>` holds the three release archives and their `.sha256` files
as the build job uploads them (`pixel-<tag>-<target>.tar.gz`). The script
writes into `<out-dir>`:

- `pixel.rb`, the formula `LivioGama/homebrew-tap` serves;
- `pixel-<version>.arm64_linux.bottle.tar.gz` and
  `pixel-<version>.x86_64_linux.bottle.tar.gz`, uploaded beside the archives.

Why bottles: without one, Homebrew treats the formula as a build from source
and, on a Linux host without a C compiler, stops at "No developer tools
installed", although `install` only copies a static musl binary. A bottle is
that binary laid out as a keg (`pixel/<version>/bin/pixel`), which Homebrew
pours with no compiler. The bottles are built from the archives' bytes with
fixed metadata, so the same archive always gives the same bottle and the same
digest in the formula. macOS has no bottle and installs from its archive as
before. Homebrew's own gcc and glibc on hosts older than its CI are a separate
matter no formula can change (README, "Install").

`PIXEL_RELEASE_URL` overrides the download base
(`https://github.com/Pixel-CLI/pixel/releases/download`), so a local Homebrew
can be pointed at a directory served over HTTP.
"""

import gzip
import hashlib
import io
import os
import sys
import tarfile
from pathlib import Path

DEFAULT_RELEASE_URL = "https://github.com/Pixel-CLI/pixel/releases/download"

# Homebrew bottle tag of each Linux release target, arm64 first as
# `brew style` orders bottle tags.
LINUX_BOTTLES = (
    ("aarch64-unknown-linux-musl", "arm64_linux"),
    ("x86_64-unknown-linux-musl", "x86_64_linux"),
)

# Files of the archive that go into the keg: the binary, and the licence and
# readme Homebrew copies into the keg of a formula installed from its url.
KEG_FILES = ("bin/pixel", "LICENSE", "README.md")


def archive_name(tag: str, target: str) -> str:
    return f"pixel-{tag}-{target}.tar.gz"


def sha256_of(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def published_sha256(artifacts: Path, tag: str, target: str) -> str:
    """The digest the build job published for one archive, checked against
    the archive itself: a formula must never name bytes it was not given."""
    archive = artifacts / archive_name(tag, target)
    line = (artifacts / f"{archive.name}.sha256").read_text().split()
    if not line or line[0] != sha256_of(archive):
        raise SystemExit(f"{archive}: its .sha256 does not match the archive")
    return line[0]


def bottle_bytes(archive: Path, version: str) -> bytes:
    """The keg of `archive` as a bottle tarball, byte-identical for the same
    archive: owner, mode and time come from the archive, gzip has no mtime."""
    top = archive.name.removesuffix(".tar.gz")
    out = io.BytesIO()
    with tarfile.open(archive, "r:gz") as src:
        members = {m.name: m for m in src.getmembers()}
        keg = {}
        for rel in KEG_FILES:
            member = members.get(f"{top}/{rel}")
            if member is None or not member.isfile():
                raise SystemExit(f"{archive}: no {top}/{rel}")
            keg[rel] = member
        with gzip.GzipFile(filename="", mode="wb", fileobj=out, mtime=0) as gz:
            with tarfile.open(fileobj=gz, mode="w", format=tarfile.PAX_FORMAT) as dst:
                for directory in (f"pixel/{version}", f"pixel/{version}/bin"):
                    info = tarfile.TarInfo(directory)
                    info.type = tarfile.DIRTYPE
                    info.mode = 0o755
                    info.mtime = keg["bin/pixel"].mtime
                    dst.addfile(info)
                for rel, member in keg.items():
                    info = tarfile.TarInfo(f"pixel/{version}/{rel}")
                    info.size = member.size
                    info.mode = 0o755 if rel.startswith("bin/") else 0o644
                    info.mtime = member.mtime
                    dst.addfile(info, src.extractfile(member))
    return out.getvalue()


def formula(tag: str, base: str, digests: dict, bottles: dict) -> str:
    """The formula text. Everything outside the `bottle do` block is the
    formula releases shipped before bottles, so macOS installs as before."""
    url = f"{base}/{tag}"
    width = max(len(bottle) for _, bottle in LINUX_BOTTLES)
    bottle_lines = "\n".join(
        f'    sha256 cellar: :any_skip_relocation, {bottle}:{" " * (width - len(bottle) + 1)}"{bottles[bottle]}"'
        for _, bottle in LINUX_BOTTLES
    )
    mac = digests["aarch64-apple-darwin"]
    return f'''class Pixel < Formula
  desc "Local control layer for coding agents — deterministic retrieval + git engine"
  homepage "https://pixel-cli.dev/"
  # Homebrew validates a URL for every simulated OS/arch at tap
  # time; the per-target URLs in the on_* blocks are the ones
  # actually used. This top-level URL satisfies the check.
  url "{url}/{archive_name(tag, "aarch64-apple-darwin")}"
  sha256 "{mac}"
  license "MIT"

  # Linux only: the static musl binary laid out as a keg, poured without
  # the C compiler a build from source would require. macOS installs from
  # its archive below.
  bottle do
    root_url "{url}"
{bottle_lines}
  end

  on_macos do
    on_intel do
      # No prebuilt Intel binary; refuse rather than install the
      # arm64 tarball. Build from source: cargo build --release -p pixel-cli
      depends_on arch: :arm64
    end

    on_arm do
      url "{url}/{archive_name(tag, "aarch64-apple-darwin")}"
      sha256 "{mac}"
    end
  end

  on_linux do
    on_arm do
      url "{url}/{archive_name(tag, "aarch64-unknown-linux-musl")}"
      sha256 "{digests["aarch64-unknown-linux-musl"]}"
    end
    on_intel do
      url "{url}/{archive_name(tag, "x86_64-unknown-linux-musl")}"
      sha256 "{digests["x86_64-unknown-linux-musl"]}"
    end
  end

  def install
    bin.install "bin/pixel"
  end

  # An upgrade replaces the binary only; the agent wiring it
  # deployed lives in the user's home and is refreshed by
  # running pixel install.
  def caveats
    <<~EOS
      Homebrew upgrades replace the binary only. The agent prompt, shell
      wrapper and per-agent config keys are written by "pixel install"
      into your home, not by Homebrew, so they keep the old release's
      text until refreshed:

        pixel install
        pixel doctor .

      "pixel doctor" reports missing or stale wiring.
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{{bin}}/pixel --version")
  end
end
'''


def main(argv: list) -> int:
    if len(argv) != 3 or not argv[0].startswith("v"):
        print(__doc__.split("\n\n")[1], file=sys.stderr)
        return 2
    tag, artifacts, out = argv[0], Path(argv[1]), Path(argv[2])
    version = tag.removeprefix("v")
    base = os.environ.get("PIXEL_RELEASE_URL", DEFAULT_RELEASE_URL).rstrip("/")
    targets = ("aarch64-apple-darwin",) + tuple(t for t, _ in LINUX_BOTTLES)
    digests = {t: published_sha256(artifacts, tag, t) for t in targets}
    # Every bottle is built before anything is written: a bad archive leaves
    # no partial set behind for the upload step to publish.
    built = {b: bottle_bytes(artifacts / archive_name(tag, t), version) for t, b in LINUX_BOTTLES}
    bottles = {b: hashlib.sha256(data).hexdigest() for b, data in built.items()}
    out.mkdir(parents=True, exist_ok=True)
    for bottle, data in built.items():
        # One dash: Homebrew's name for a bottle under a plain root_url.
        (out / f"pixel-{version}.{bottle}.bottle.tar.gz").write_bytes(data)
    (out / "pixel.rb").write_text(formula(tag, base, digests, bottles))
    for name in sorted(p.name for p in out.glob("pixel*")):
        print(name)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
