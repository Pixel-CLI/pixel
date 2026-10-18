#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of scripts/ci/fetch-embedding-model.sh, which pins the ranking
lane's embedding model to one Hugging Face revision.

The NDCG floors are only comparable across runs when every run embeds with
the same bytes. So the script must lay out exactly what hf-hub reads from its
default cache (`refs/main` naming the pinned sha, `snapshots/<sha>/<file>`
under `~/.cache/huggingface/hub`) plus pixel's potion marker, refuse bytes whose sha256 differs (an upstream re-upload),
and touch nothing on the network when a restored cache already holds the
right files. A fake hub is served over file:// from a temp directory.
"""
from __future__ import annotations

import hashlib
import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "ci" / "fetch-embedding-model.sh"
TEXT = SCRIPT.read_text()
REPO = re.search(r"^REPO=(\S+)$", TEXT, re.M).group(1)
REVISION = re.search(r"^REVISION=(\S+)$", TEXT, re.M).group(1)
FILES = re.search(r"^FILES=\((.*)\)$", TEXT, re.M).group(1).split()
FOLDER = "models--" + REPO.replace("/", "--")


def run(script: Path, home: Path, hub: Path | None) -> subprocess.CompletedProcess[str]:
    env = dict(os.environ)
    # An unreachable endpoint: any fetch it attempts fails the run.
    env["PIXEL_MODEL_BASE_URL"] = hub.as_uri() if hub else "http://127.0.0.1:9"
    return subprocess.run(
        ["bash", str(script), str(home)],
        capture_output=True,
        text=True,
        env=env,
        check=False,
    )


class FetchEmbeddingModel(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.home = self.root / "home"
        self.script = SCRIPT

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def hub(self) -> Path:
        return self.home / ".cache" / "huggingface" / "hub" / FOLDER

    def marker(self) -> Path:
        return self.home / ".local/share/pixel/models" / f"potion-{REPO.replace('/', '--')}.ok"

    def serve(self, contents: dict[str, bytes]) -> Path:
        """A fake hub holding `contents` under <repo>/resolve/<revision>/."""
        hub = self.root / "hub"
        resolved = hub / REPO / "resolve" / REVISION
        resolved.mkdir(parents=True, exist_ok=True)
        for name, data in contents.items():
            (resolved / name).write_bytes(data)
        return hub

    def pin(self, contents: dict[str, bytes]) -> None:
        """Rewrite the script's sha256 table to `contents`, in a temp copy."""
        sums = "\n".join(
            f"    {hashlib.sha256(contents[f]).hexdigest()}" for f in FILES
        )
        copy = self.root / "fetch.sh"
        copy.write_text(re.sub(r"SHA256=\((.*?)\)", f"SHA256=(\n{sums}\n)", TEXT, flags=re.S))
        self.script = copy

    def test_pinned_files_should_land_where_hf_hub_reads_its_cache(self) -> None:
        contents = {f: f"bytes of {f}".encode() for f in FILES}
        self.pin(contents)
        out = run(self.script, self.home, self.serve(contents))
        self.assertEqual(out.returncode, 0, out.stderr)
        hub = self.hub()
        self.assertEqual((hub / "refs" / "main").read_text(), REVISION)
        for name, data in contents.items():
            self.assertEqual((hub / "snapshots" / REVISION / name).read_bytes(), data)
        marker = self.marker()
        self.assertEqual(marker.read_text(), REPO)

    def test_a_restored_cache_should_need_no_network(self) -> None:
        contents = {f: f"bytes of {f}".encode() for f in FILES}
        self.pin(contents)
        self.assertEqual(run(self.script, self.home, self.serve(contents)).returncode, 0)
        again = run(self.script, self.home, None)
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertEqual(again.stdout.count("kept "), len(FILES), again.stdout)
        self.assertNotIn("fetched", again.stdout)

    def test_a_reuploaded_file_should_fail_and_leave_no_marker(self) -> None:
        contents = {f: f"bytes of {f}".encode() for f in FILES}
        self.pin(contents)
        served = dict(contents)
        served[FILES[-1]] = b"re-uploaded weights"
        out = run(self.script, self.home, self.serve(served))
        self.assertNotEqual(out.returncode, 0)
        self.assertIn(f"{FILES[-1]} at {REVISION} has sha256", out.stderr)
        self.assertFalse(self.marker().exists())
        self.assertFalse((self.hub() / "refs" / "main").exists())
        snapshot = self.hub() / "snapshots" / REVISION
        self.assertEqual(sorted(p.name for p in snapshot.iterdir()), sorted(FILES[:-1]))

    def test_cache_key_should_name_the_repo_and_the_pinned_revision(self) -> None:
        out = subprocess.run(
            ["bash", str(SCRIPT), "--cache-key"], capture_output=True, text=True, check=True
        )
        self.assertEqual(
            out.stdout.strip(),
            f"embedding-model-{REPO.replace('/', '--')}-{REVISION}",
        )

    def test_repo_should_match_the_code_search_model(self) -> None:
        source = (ROOT / "crates/pixel-recall/src/code_search.rs").read_text()
        self.assertIn(f'CODE_SEARCH_MODEL_REPO: &str = "{REPO}";', source)


if __name__ == "__main__":
    unittest.main()
