#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Narrow a cargo-cyclonedx SBOM to what one release archive compiles.

    python3 scripts/release-sbom.py <raw.cdx.json> <cargo-tree.txt> <tag> <target> <out.cdx.json>

`<raw.cdx.json>` is `cargo cyclonedx --describe binaries --format json` for
the `pixel` binary, run with the target and the feature flags of the build.
`<cargo-tree.txt>` is `cargo tree -p pixel-cli --target <target> <features>
-e normal,build --prefix none --format '{p}'` with the same flags: one
`<name> v<version> ...` line per crate cargo compiles into that binary.

Why both: cargo-cyclonedx reads `cargo metadata`, which resolves features for
the whole workspace and over-includes optional dependencies (rust-lang/cargo
#7754 and #10718, CycloneDX/cyclonedx-rust-cargo#766). On 0.6.1 the Linux
SBOM listed the fastembed, ort and OpenSSL stack the musl build never
compiles: a scanner would report OpenSSL advisories against a binary with no
OpenSSL in it. `cargo tree -p` resolves features as `cargo build -p` does, so
it is the authority on membership; cargo-cyclonedx keeps the licences, purls
and the dependency graph.

The script writes `<out.cdx.json>` with the components outside the tree
removed and the dependency graph pruned to the components kept. It refuses,
writing nothing:

- an SBOM that does not describe `pixel` at the tag's version, or for another
  target than `<target>` (a wrong flag in the release job);
- a compiled crate the SBOM does not list: a smaller SBOM than the binary is
  the one error worse than a larger one;
- a tree with no crate besides `pixel-cli` (an empty or truncated `cargo tree`
  output).
"""

import json
import sys
from pathlib import Path

BINARY = "pixel"
PACKAGE = "pixel-cli"
TARGET_PROPERTY = "cdx:rustc:sbom:target:triple"


class SbomError(Exception):
    """The inputs do not describe the release archive; nothing is written."""


def compiled_crates(tree_text):
    """`(name, version)` of every crate in `cargo tree --prefix none --format
    '{p}'` output, the root package excluded (it is the SBOM's subject, not a
    component). A line reads `name v1.2.3`, then an optional source and the
    `(*)` / `(proc-macro)` markers."""
    crates = set()
    for line in tree_text.splitlines():
        fields = line.split()
        if not fields:
            continue
        if len(fields) < 2 or not fields[1].startswith("v"):
            raise SbomError(f"unreadable cargo tree line: {line!r}")
        if fields[0] != PACKAGE:
            crates.add((fields[0], fields[1][1:]))
    if not crates:
        raise SbomError(f"cargo tree listed no crate besides {PACKAGE}")
    return crates


def check_subject(bom, version, target):
    """The SBOM describes the `pixel` binary of this version, for this target."""
    if bom.get("bomFormat") != "CycloneDX":
        raise SbomError(f"not a CycloneDX document (bomFormat {bom.get('bomFormat')!r})")
    metadata = bom.get("metadata", {})
    subject = metadata.get("component", {})
    if subject.get("name") != BINARY or subject.get("version") != version:
        raise SbomError(
            f"the SBOM describes {subject.get('name')!r} {subject.get('version')!r}, "
            f"not {BINARY!r} {version!r}"
        )
    triples = [p.get("value") for p in metadata.get("properties", []) if p.get("name") == TARGET_PROPERTY]
    if triples != [target]:
        raise SbomError(f"the SBOM was generated for {triples}, not [{target!r}]")


def narrow(bom, compiled):
    """`bom` with only the compiled components, and its dependency graph
    pruned to them. Returns the new document and the dropped `name@version`s."""
    components = bom.get("components", [])
    kept = [c for c in components if (c.get("name"), c.get("version")) in compiled]
    listed = {(c.get("name"), c.get("version")) for c in kept}
    missing = sorted(f"{n}@{v}" for n, v in compiled - listed)
    if missing:
        raise SbomError(f"compiled but absent from the SBOM: {', '.join(missing)}")
    refs = {c["bom-ref"] for c in kept}
    refs.add(bom["metadata"]["component"]["bom-ref"])
    dependencies = [
        {**d, "dependsOn": [r for r in d.get("dependsOn", []) if r in refs]}
        for d in bom.get("dependencies", [])
        if d.get("ref") in refs
    ]
    dropped = sorted(
        f"{c.get('name')}@{c.get('version')}"
        for c in components
        if (c.get("name"), c.get("version")) not in compiled
    )
    return {**bom, "components": kept, "dependencies": dependencies}, dropped


def main(argv):
    if len(argv) != 5:
        print(__doc__.strip().splitlines()[2].strip(), file=sys.stderr)
        return 2
    raw, tree, tag, target, out = argv
    try:
        bom = json.loads(Path(raw).read_text())
        check_subject(bom, tag.removeprefix("v"), target)
        narrowed, dropped = narrow(bom, compiled_crates(Path(tree).read_text()))
    except SbomError as err:
        print(f"release-sbom: {err}", file=sys.stderr)
        return 1
    Path(out).write_text(json.dumps(narrowed, indent=2, ensure_ascii=False) + "\n")
    print(f"{out}: {len(narrowed['components'])} components for {target}")
    if dropped:
        print(f"dropped, not compiled for {target}: {' '.join(dropped)}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
