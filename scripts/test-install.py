#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Exercise the real installer with disposable homes and local release substitutes."""

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

# Tools install.sh reaches through PATH. A test that cares about which checksum
# tool exists (or does not) symlinks these plus its chosen fakes into a private
# bin dir, the way a busybox/Alpine image has sha256sum but no shasum. python3
# is there for the `#!/usr/bin/env python3` of the fake curl and hash tools.
SYSTEM_TOOLS = ("awk", "chmod", "cp", "gzip", "mkdir", "mktemp", "mv", "python3", "rm", "tar")


class InstallContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="pixel-install-contract-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.fake = self.root / "fake"
        self.fake.mkdir()
        stage = self.root / "pixel-v9.8.7-aarch64-apple-darwin"
        (stage / "bin").mkdir(parents=True)
        binary = stage / "bin/pixel"
        binary.write_text("#!/bin/sh\necho isolated-pixel-9.8.7\n")
        binary.chmod(0o755)
        archive = self.root / "fixture.tar.gz"
        with tarfile.open(archive, "w:gz") as tar:
            tar.add(stage, arcname=stage.name)
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (self.root / "sha").write_text(digest + "  fixture.tar.gz\n")
        (self.fake / "uname").write_text(
            "#!/bin/sh\ncase $1 in -s) echo Darwin;; -m) echo arm64;; esac\n"
        )
        # The fake answers like github.com: releases/latest redirects to the
        # tag page (FIXTURE_LATEST=tag), to the releases list when there is no
        # release (none), or the host cannot be reached (unreachable). The
        # anonymous REST API answers 403, as it does once a shared IP has used
        # its 60 requests an hour. The .sha256 file is served normally unless
        # FIXTURE_CHECKSUM=unreachable asks for the failure a blocked or
        # missing release asset gives (404). Every URL asked is appended to
        # curl.log.
        (self.fake / "curl").write_text("""#!/usr/bin/env python3
import os, sys, shutil
from pathlib import Path
args = sys.argv
url = next(arg for arg in args if arg.startswith('https:'))
root = Path(os.environ['FIXTURE_ROOT'])
with open(root / 'curl.log', 'a') as log:
    log.write(url + '\\n')
if url.startswith('https://api.github.com/'):
    sys.stderr.write('curl: (22) The requested URL returned error: 403\\n')
    sys.exit(22)
if url.endswith('/releases/latest'):
    mode = os.environ.get('FIXTURE_LATEST', 'tag')
    if mode == 'unreachable':
        sys.stderr.write('curl: (6) Could not resolve host: github.com\\n')
        sys.exit(6)
    page = 'releases/tag/v9.8.7' if mode == 'tag' else 'releases'
    sys.stdout.write('https://github.com/Pixel-CLI/pixel/' + page)
elif url.endswith('.sha256'):
    if os.environ.get('FIXTURE_CHECKSUM', 'ok') == 'unreachable':
        sys.stderr.write('curl: (22) The requested URL returned error: 404\\n')
        sys.exit(22)
    print((root / 'sha').read_text())
else:
    shutil.copyfile(root / 'fixture.tar.gz', args[args.index('-o') + 1])
""")
        # The hash tools print what coreutils and shasum print, digest then two
        # spaces then the path. shasum defaults to SHA-1 without `-a 256`, so a
        # missing `-a 256` gives a digest the installer rejects instead of a
        # silently passing test.
        (self.fake / "sha256sum").write_text("""#!/usr/bin/env python3
import hashlib, os, sys
if os.environ.get('FIXTURE_HASH') == 'fail':
    sys.stderr.write('sha256sum: fixture cannot read the archive\\n')
    sys.exit(1)
for path in sys.argv[1:]:
    digest = hashlib.sha256(open(path, 'rb').read()).hexdigest()
    sys.stdout.write(digest + '  ' + path + '\\n')
""")
        (self.fake / "shasum").write_text("""#!/usr/bin/env python3
import hashlib, sys
args = sys.argv[1:]
algorithm = 'sha1'
if args[:1] == ['-a']:
    algorithm, args = 'sha256' if args[1] == '256' else 'sha1', args[2:]
for path in args:
    digest = getattr(hashlib, algorithm)(open(path, 'rb').read()).hexdigest()
    sys.stdout.write(digest + '  ' + path + '\\n')
""")
        for executable in self.fake.iterdir():
            executable.chmod(0o755)
        self.destination = self.root / "destination"
        self.env = {
            **os.environ,
            "PATH": str(self.fake) + os.pathsep + os.environ["PATH"],
            "FIXTURE_ROOT": str(self.root),
            "PIXEL_INSTALL_DIR": str(self.destination),
            "HOME": str(self.root / "home"),
        }

    def install(self):
        # An absolute shell: a restricted PATH (restricted_install) does not
        # carry one.
        return subprocess.run(
            ["/bin/sh", str(Path(__file__).with_name("install.sh"))],
            env=self.env, capture_output=True, text=True, timeout=15,
        )

    def restricted_install(self, *fakes):
        """Install with a PATH of exactly `fakes` plus the host's own tools, so
        the test decides whether the installer finds shasum or sha256sum."""
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        for tool in SYSTEM_TOOLS:
            resolved = shutil.which(tool)
            if resolved is None:
                self.fail(f"the host lacks {tool}, which install.sh calls")
            (bin_dir / tool).symlink_to(resolved)
        for tool in fakes:
            (bin_dir / tool).symlink_to(self.fake / tool)
        self.env["PATH"] = str(bin_dir)
        return self.install()

    def test_install_and_reinstall_real_archive(self):
        for _ in range(2):
            result = self.install()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("isolated-pixel-9.8.7", result.stdout)
            installed = subprocess.check_output(
                [str(self.destination / "pixel")], text=True, timeout=5
            )
            self.assertEqual(installed.strip(), "isolated-pixel-9.8.7")
            self.assertEqual(list(self.destination.glob(".pixel.tmp.*")), [])

    def test_a_binary_that_does_not_run_is_not_installed(self):
        stage = self.root / "pixel-v9.8.7-aarch64-apple-darwin"
        binary = stage / "bin/pixel"
        binary.write_text("#!/bin/sh\nexit 1\n")
        archive = self.root / "fixture.tar.gz"
        with tarfile.open(archive, "w:gz") as tar:
            tar.add(stage, arcname=stage.name)
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (self.root / "sha").write_text(digest + "  fixture.tar.gz\n")
        self.destination.mkdir()
        installed = self.destination / "pixel"
        installed.write_bytes(b"previous installation")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("The downloaded binary did not run.", result.stderr)
        self.assertEqual(installed.read_bytes(), b"previous installation")
        self.assertEqual(list(self.destination.glob(".pixel.tmp.*")), [])

    def test_github_actions_receives_the_install_directory(self):
        path_file = self.root / "github_path"
        self.env["GITHUB_PATH"] = str(path_file)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(path_file.read_text().splitlines(), [str(self.destination)])

    def test_destdir_stages_the_install_and_leaves_the_final_directory_alone(self):
        # A package build or image layer installs into a staging root: the
        # binary goes under DESTDIR, the real INSTALL_DIR is never touched,
        # and PATH still names the directory the binary will live in.
        staging = self.root / "staging"
        path_file = self.root / "github_path"
        self.env["DESTDIR"] = str(staging)
        self.env["GITHUB_PATH"] = str(path_file)
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        staged = Path(str(staging) + str(self.destination)) / "pixel"
        installed = subprocess.check_output([str(staged)], text=True, timeout=5)
        self.assertEqual(installed.strip(), "isolated-pixel-9.8.7")
        self.assertFalse(self.destination.exists(), "DESTDIR install wrote INSTALL_DIR")
        self.assertEqual(list(staged.parent.glob(".pixel.tmp.*")), [])
        self.assertEqual(path_file.read_text().splitlines(), [str(self.destination)])
        self.assertIn(f"for {self.destination}/pixel", result.stdout)

    def test_checksum_failure_preserves_previous_install(self):
        self.destination.mkdir()
        installed = self.destination / "pixel"
        installed.write_bytes(b"previous installation")
        (self.root / "sha").write_text("0" * 64 + "  fixture.tar.gz\n")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Checksum mismatch!", result.stderr)
        self.assertEqual(installed.read_bytes(), b"previous installation")
        self.assertEqual(list(self.destination.glob(".pixel.tmp.*")), [])

    def test_a_checksum_that_cannot_be_downloaded_is_not_a_checksum_mismatch(self):
        self.env["FIXTURE_CHECKSUM"] = "unreachable"
        self.destination.mkdir()
        installed = self.destination / "pixel"
        installed.write_bytes(b"previous installation")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "Could not download https://github.com/Pixel-CLI/pixel/releases/"
            "download/v9.8.7/pixel-v9.8.7-aarch64-apple-darwin.tar.gz.sha256",
            result.stderr,
        )
        self.assertNotIn("Checksum mismatch", result.stderr)
        self.assertEqual(installed.read_bytes(), b"previous installation")

    def test_a_checksum_asset_without_a_sha256_digest_is_not_a_checksum_mismatch(self):
        self.destination.mkdir()
        installed = self.destination / "pixel"
        installed.write_bytes(b"previous installation")
        for content in ("not a digest\n", "0" * 63 + "\n", "A" * 64 + "  fixture.tar.gz\n"):
            with self.subTest(content=content):
                (self.root / "sha").write_text(content)
                result = self.install()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("Could not read a SHA-256 digest", result.stderr)
                self.assertNotIn("Checksum mismatch", result.stderr)
        self.assertEqual(installed.read_bytes(), b"previous installation")

    def test_a_hash_tool_that_fails_is_not_a_checksum_mismatch(self):
        self.env["FIXTURE_HASH"] = "fail"
        result = self.restricted_install("uname", "curl", "sha256sum")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Could not compute the SHA-256", result.stderr)
        self.assertNotIn("Checksum mismatch", result.stderr)
        self.assertFalse((self.destination / "pixel").exists())

    def test_a_path_with_sha256sum_and_no_shasum_verifies_the_archive(self):
        result = self.restricted_install("uname", "curl", "sha256sum")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Checksum OK.", result.stdout)
        installed = subprocess.check_output(
            [str(self.destination / "pixel")], text=True, timeout=5
        )
        self.assertEqual(installed.strip(), "isolated-pixel-9.8.7")

    def test_a_path_with_shasum_and_no_sha256sum_verifies_the_archive(self):
        result = self.restricted_install("uname", "curl", "shasum")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Checksum OK.", result.stdout)
        installed = subprocess.check_output(
            [str(self.destination / "pixel")], text=True, timeout=5
        )
        self.assertEqual(installed.strip(), "isolated-pixel-9.8.7")

    def test_a_path_without_either_hash_tool_fails_explicitly(self):
        self.destination.mkdir()
        installed = self.destination / "pixel"
        installed.write_bytes(b"previous installation")
        result = self.restricted_install("uname", "curl")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Neither sha256sum nor shasum is installed", result.stderr)
        self.assertNotIn("Checksum mismatch", result.stderr)
        self.assertEqual(installed.read_bytes(), b"previous installation")

    def requested_urls(self):
        log = self.root / "curl.log"
        return log.read_text().splitlines() if log.exists() else []

    def test_latest_tag_comes_from_the_release_page_redirect_not_the_rate_limited_api(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        urls = self.requested_urls()
        self.assertEqual(
            [url for url in urls if "api.github.com" in url], [],
            "the anonymous API is rate limited per IP",
        )
        self.assertIn(
            "https://github.com/Pixel-CLI/pixel/releases/download/v9.8.7/"
            "pixel-v9.8.7-aarch64-apple-darwin.tar.gz",
            urls,
        )
        self.assertIn("pixel v9.8.7 (aarch64-apple-darwin)", result.stdout)

    def test_a_repository_without_release_points_to_a_source_install(self):
        self.env["FIXTURE_LATEST"] = "none"
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("No prebuilt release found for Pixel-CLI/pixel.", result.stderr)
        self.assertIn("cargo install --git", result.stderr)
        self.assertFalse((self.destination / "pixel").exists())

    def test_an_unreachable_github_shows_the_curl_error_and_keeps_the_install(self):
        self.env["FIXTURE_LATEST"] = "unreachable"
        self.destination.mkdir()
        installed = self.destination / "pixel"
        installed.write_bytes(b"previous installation")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Could not resolve host: github.com", result.stderr)
        self.assertIn("Could not reach https://github.com/Pixel-CLI/pixel/releases/latest", result.stderr)
        self.assertNotIn("No prebuilt release found", result.stderr)
        self.assertEqual(installed.read_bytes(), b"previous installation")


if __name__ == "__main__":
    unittest.main()
