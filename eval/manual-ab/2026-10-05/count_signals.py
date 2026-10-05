#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Count Rust public-function declarations under pixel-rank/src."""

from pathlib import Path
import re


ROOT = Path(__file__).resolve().parents[1]
SOURCE_DIR = ROOT / "pixel-rank" / "src"
PUBLIC_FN = re.compile(r"^\s*pub\s+fn\s+")


def main() -> None:
    counts = [
        (sum(bool(PUBLIC_FN.match(line)) for line in path.read_text().splitlines()), path)
        for path in SOURCE_DIR.rglob("*.rs")
    ]
    for count, path in sorted(counts, key=lambda item: (-item[0], str(item[1]))):
        print(f"{count} {path.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
