#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of scripts/release-sbom.py, the step release-build.yml runs to
write the SBOM published beside each release archive, checked without a tag.

What must hold for the SBOM to describe the archive (OSPS-QA-02.02):

- a crate cargo-cyclonedx lists but `cargo tree -p pixel-cli` does not compile
  is gone, with every dependency edge to it: a scanner reading the Linux SBOM
  must not report OpenSSL in a binary that has none;
- every crate the build compiles stays, matched by name *and* version, so a
  second major of a crate is not mistaken for the one the build uses;
- a compiled crate the SBOM does not list, an SBOM of another version, target
  or binary, and an empty or unreadable tree stop the release job and write
  nothing: the release then has no SBOM rather than a wrong one.
"""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "release-sbom.py"
TAG = "v9.8.7"
TARGET = "x86_64-unknown-linux-musl"
ROOT_REF = "path+file:///work/crates/pixel#pixel-cli@9.8.7"


def ref(name, version):
    return f"registry+https://github.com/rust-lang/crates.io-index#{name}@{version}"


def component(name, version):
    return {
        "type": "library",
        "bom-ref": ref(name, version),
        "name": name,
        "version": version,
        "licenses": [{"expression": "MIT"}],
        "purl": f"pkg:cargo/{name}@{version}",
    }


def raw_bom(version="9.8.7", target=TARGET, name="pixel"):
    """The shape of `cargo cyclonedx --describe binaries --format json`: the
    binary as metadata.component, the target as a property, the over-included
    OpenSSL stack and two majors of hashbrown among the components."""
    crates = [("clap", "4.6.7"), ("hashbrown", "0.15.5"), ("hashbrown", "0.16.1"),
              ("openssl", "0.10.81"), ("openssl-sys", "0.9.117"), ("serde", "1.0.228")]
    return {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": "urn:uuid:00000000-0000-4000-8000-000000000000",
        "version": 1,
        "metadata": {
            "tools": [{"vendor": "CycloneDX", "name": "cargo-cyclonedx", "version": "0.5.9"}],
            "component": {"type": "application", "bom-ref": ROOT_REF, "name": name, "version": version},
            "properties": [{"name": "cdx:rustc:sbom:target:triple", "value": target}],
        },
        "components": [component(n, v) for n, v in crates],
        "dependencies": [
            {"ref": ROOT_REF, "dependsOn": [ref("clap", "4.6.7"), ref("openssl", "0.10.81"), ref("serde", "1.0.228")]},
            {"ref": ref("clap", "4.6.7"), "dependsOn": [ref("hashbrown", "0.15.5")]},
            {"ref": ref("openssl", "0.10.81"), "dependsOn": [ref("openssl-sys", "0.9.117"), ref("hashbrown", "0.16.1")]},
            {"ref": ref("openssl-sys", "0.9.117"), "dependsOn": []},
            {"ref": ref("hashbrown", "0.15.5"), "dependsOn": []},
            {"ref": ref("hashbrown", "0.16.1"), "dependsOn": []},
            {"ref": ref("serde", "1.0.228"), "dependsOn": []},
        ],
    }


# `cargo tree --prefix none --format '{p}'` as the release job runs it: the
# root with its path, repeated crates marked `(*)`, a proc-macro marker.
TREE = """\
pixel-cli v9.8.7 (/work/crates/pixel)
clap v4.6.7
hashbrown v0.15.5
serde v1.0.228
serde_derive v1.0.228 (proc-macro)
hashbrown v0.15.5 (*)
"""


class ReleaseSbomContract(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix="pixel-sbom-contract-")
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.out = self.root / f"pixel-{TAG}-{TARGET}.cdx.json"

    def run_script(self, bom, tree, tag=TAG, target=TARGET):
        raw = self.root / "pixel_bin.cdx.json"
        raw.write_text(json.dumps(bom))
        tree_file = self.root / "tree.txt"
        tree_file.write_text(tree)
        return subprocess.run(
            [sys.executable, str(SCRIPT), str(raw), str(tree_file), tag, target, str(self.out)],
            capture_output=True, text=True, check=False,
        )

    def written(self):
        return json.loads(self.out.read_text())

    def with_proc_macro(self, **subject):
        """A raw SBOM listing every crate TREE compiles: whatever the script
        refuses in it, it refuses for the subject, not for a missing crate."""
        bom = raw_bom(**subject)
        bom["components"].append(component("serde_derive", "1.0.228"))
        return bom

    def test_drops_what_the_build_does_not_compile_and_every_edge_to_it(self):
        result = self.run_script(self.with_proc_macro(), TREE)
        self.assertEqual(result.returncode, 0, result.stderr)
        bom = self.written()
        names = sorted(f"{c['name']}@{c['version']}" for c in bom["components"])
        self.assertEqual(names, ["clap@4.6.7", "hashbrown@0.15.5", "serde@1.0.228", "serde_derive@1.0.228"])
        kept_refs = {c["bom-ref"] for c in bom["components"]} | {ROOT_REF}
        for dep in bom["dependencies"]:
            self.assertIn(dep["ref"], kept_refs)
            for target in dep["dependsOn"]:
                self.assertIn(target, kept_refs, f"{dep['ref']} still depends on dropped {target}")
        root = next(d for d in bom["dependencies"] if d["ref"] == ROOT_REF)
        self.assertEqual(root["dependsOn"], [ref("clap", "4.6.7"), ref("serde", "1.0.228")])
        self.assertIn("openssl@0.10.81", result.stdout)

    def test_keeps_the_document_around_the_components(self):
        bom = self.with_proc_macro()
        self.assertEqual(self.run_script(bom, TREE).returncode, 0)
        written = self.written()
        for key in ("bomFormat", "specVersion", "serialNumber", "version", "metadata"):
            self.assertEqual(written[key], bom[key], key)
        self.assertEqual(written["components"][0], component("clap", "4.6.7"))

    def test_a_compiled_crate_missing_from_the_sbom_fails(self):
        result = self.run_script(raw_bom(), TREE)
        self.assertEqual(result.returncode, 1)
        self.assertIn("serde_derive@1.0.228", result.stderr)
        self.assertFalse(self.out.exists())

    def test_the_version_is_matched_not_only_the_name(self):
        tree = TREE.replace("hashbrown v0.15.5", "hashbrown v0.16.1")
        self.assertEqual(self.run_script(self.with_proc_macro(), tree).returncode, 0)
        versions = [c["version"] for c in self.written()["components"] if c["name"] == "hashbrown"]
        self.assertEqual(versions, ["0.16.1"])

    def test_an_sbom_of_another_version_target_or_binary_fails(self):
        cases = {
            "version": (self.with_proc_macro(version="9.8.6"), TAG, TARGET),
            "tag": (self.with_proc_macro(), "v9.8.8", TARGET),
            "target": (self.with_proc_macro(target="aarch64-apple-darwin"), TAG, TARGET),
            "requested target": (self.with_proc_macro(), TAG, "aarch64-unknown-linux-musl"),
            "binary": (self.with_proc_macro(name="pixel-cli"), TAG, TARGET),
        }
        for case, (bom, tag, target) in cases.items():
            with self.subTest(case):
                result = self.run_script(bom, TREE, tag=tag, target=target)
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn("release-sbom:", result.stderr)
                self.assertFalse(self.out.exists())

    def test_an_empty_or_unreadable_tree_fails(self):
        cases = {
            "empty": "\n",
            "root only": "pixel-cli v9.8.7 (/work/crates/pixel)\n",
            "unreadable": "error: package `pixel-cli` not found\n",
        }
        for case, tree in cases.items():
            with self.subTest(case):
                result = self.run_script(self.with_proc_macro(), tree)
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertFalse(self.out.exists())

    def test_wrong_arity_prints_the_usage(self):
        result = subprocess.run([sys.executable, str(SCRIPT)], capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 2)
        self.assertIn("release-sbom.py <raw.cdx.json> <cargo-tree.txt>", result.stderr)


if __name__ == "__main__":
    unittest.main()
