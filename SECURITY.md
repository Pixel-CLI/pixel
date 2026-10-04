# Security Policy

## Supported versions

Only the latest release is supported with security fixes.

## Reporting a vulnerability

Open a private security advisory on GitHub:
https://github.com/Pixel-CLI/pixel/security/advisories/new

Do **not** open a public issue for security vulnerabilities.

Please include:
- A description of the vulnerability and its impact
- Steps to reproduce or a proof of concept
- Affected versions (if known)

You will receive a response within 48 hours.

## Verifying a release

Every release archive and its `install.sh` carry a signed build-provenance
attestation from `.github/workflows/release-build.yml`, the reusable workflow
that builds and signs the release on its tag on a GitHub-hosted runner. The
`.sha256` file beside each archive only proves the download is intact; the
attestation proves who built it. With the GitHub CLI:

```bash
gh attestation verify pixel-vX.Y.Z-aarch64-apple-darwin.tar.gz --repo Pixel-CLI/pixel \
  --signer-workflow Pixel-CLI/pixel/.github/workflows/release-build.yml \
  --source-ref refs/tags/vX.Y.Z --deny-self-hosted-runners
```

The same command verifies `install.sh` before you pipe it into `sh`.
Releases before the first attested one have no attestation.

Releases after v0.6.1 also attach the attestation itself as
`pixel-vX.Y.Z.intoto.jsonl`: the Sigstore bundle of that one attestation,
covering every archive, Linux bottle and `install.sh` of the release.
`--bundle` checks a file against it instead of fetching the attestation
from GitHub, with the same identity flags; adding a trusted root saved
beforehand with `gh attestation trusted-root > trusted_root.jsonl` makes the
check fully offline:

```bash
gh attestation verify pixel-vX.Y.Z-aarch64-apple-darwin.tar.gz --repo Pixel-CLI/pixel \
  --bundle pixel-vX.Y.Z.intoto.jsonl --custom-trusted-root trusted_root.jsonl \
  --signer-workflow Pixel-CLI/pixel/.github/workflows/release-build.yml \
  --source-ref refs/tags/vX.Y.Z --deny-self-hosted-runners
```

Signing in a reusable workflow is what makes this SLSA Build Level 3: the
signing identity belongs to `release-build.yml`, which holds no secret, and
`release.yml`, which publishes the release and holds the Homebrew tap token,
cannot sign. Releases up to v0.6.1 were built and signed by `release.yml`
itself (Build Level 2); verify them with `release.yml` as the signer
workflow, under the owner below.

v0.6.0 and v0.6.1 were signed before the repository moved from
`LivioGama/pixel` to `Pixel-CLI/pixel`, and their attestations stayed with
the former owner: `--repo` answers `HTTP 404` for them, under either name,
since it looks attestations up by the repository's id, which now belongs to
`Pixel-CLI/pixel`. Verify those two by the owner they were built under,
the signer workflow still pinning the repository:

```bash
gh attestation verify pixel-v0.6.1-aarch64-apple-darwin.tar.gz --owner LivioGama \
  --signer-workflow LivioGama/pixel/.github/workflows/release.yml \
  --source-ref refs/tags/v0.6.1 --deny-self-hosted-runners
```

### VirusTotal reports

Each release's notes end with a `VirusTotal` section linking the report of
every archive, by its sha256, once the release workflow has submitted them
(from the first release after v0.6.1, when the repository's `VT_API_KEY`
secret is set). A scan says what antivirus engines think of the file; the
attestation above is what proves where it came from.

## Security model

Pixel runs locally and processes repository data. Key security boundaries:

- **Daemon socket**: per-user directory (0700) on Linux, per-user TMPDIR on macOS. Socket file is 0600. No cross-user access.
- **History database**: a `_pixel_marker` table proves the db was created by pixel. A db planted by a hostile repo (e.g. `git add -f .pixel/history.db`) is detected and wiped before any data is trusted.
- **Sidecar files** (`.pixel/`): directory is 0700, flow files and action logs are 0600. Flow files may contain fill values (passwords, OTPs) from flow replay.
- **Command/argument injection**: all user input passed to shell commands is sanitized via `ref_guard`. Path traversal in `pixel-install` is blocked.
- **No telemetry**: pixel sends no usage data anywhere. The one request it makes on its own is the release check below, which carries no repository or usage data.
- **Network access**: limited to first-use model downloads (Hugging Face), explicit Git remote operations (`pixel fetch`, `pixel push`, `pixel commit-and-push`), and two opt-in commands that send their input out: `pixel classify` sends its question to the model endpoint you configure (OpenRouter, Ollama Cloud or a local server), and `pixel web-search` sends its query to the SearXNG endpoint you configure (`PIXEL_WEB_SEARCH_URL`) and nowhere else, even when it answers thinly or not at all; without one, it sends the query to DuckDuckGo, then to Wikipedia while the hits are fewer than `--limit` (8 by default). At most once a day, when a command's stderr is a terminal, one `HEAD` request to `https://github.com/Pixel-CLI/pixel/releases/latest` reads the latest release tag (GitHub sees the IP address and a `pixel-cli/<version>` user agent); it never runs for hooks, agents or under `CI`, and `PIXEL_NO_UPDATE_CHECK=1` turns it off. The index and its sidecar files never leave the machine.

## Known limitations

- The daemon does not perform peer-credential checking on incoming socket connections. The 0700 directory permission is the primary access control. On a shared system where another user can bypass directory permissions (e.g. root), additional hardening may be needed.
- The `fastembed` feature downloads models from Hugging Face at runtime. This is opt-in (enabled by default, can be disabled with `--no-default-features`).
