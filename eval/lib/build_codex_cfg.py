# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Build a per-arm Codex config from the real one.

Usage: build_codex_cfg.py <input-config.toml> <output-config.toml> <mode> [payload-file]

mode baseline: drop the pixel-managed block from developer_instructions entirely.
mode payload:  swap the managed block's content for the payload file's text.
mode keep:     copy the config unchanged (the installed block).
Everything outside the markers stays byte-identical.
"""
import sys

BEGIN = "<!-- pixel:managed:begin -->"
END = "<!-- pixel:managed:end -->"


def main() -> None:
    src, dst, mode = sys.argv[1], sys.argv[2], sys.argv[3]
    text = open(src, encoding="utf-8").read()
    if mode == "keep":
        # The installed block as `pixel install` wrote it (quiet/full arms).
        open(dst, "w", encoding="utf-8").write(text)
        return
    b = text.find(BEGIN)
    e = text.find(END)
    if b == -1 or e == -1 or e < b:
        print(f"no pixel:managed block in {src}", file=sys.stderr)
        sys.exit(1)
    e += len(END)
    if mode == "baseline":
        block = ""
    else:
        payload = open(sys.argv[4], encoding="utf-8").read().strip()
        block = BEGIN + "\n" + payload + "\n" + END
    open(dst, "w", encoding="utf-8").write(text[:b] + block + text[e:])


if __name__ == "__main__":
    main()
