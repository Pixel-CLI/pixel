# Security Policy

## Supported versions

Only the latest release is supported with security fixes.

## Reporting a vulnerability

Email: Open a private security advisory on GitHub (https://github.com/Pixel-CLI/pixel/security/advisories/new).

Do **not** open a public issue for security vulnerabilities.

Please include:
- A description of the vulnerability and its impact
- Steps to reproduce or a proof of concept
- Affected versions (if known)

You will receive a response within 48 hours.

## Verifying a release

Every release archive and its `install.sh` carry a signed build-provenance
attestation from `.github/workflows/release.yml`, run on the release tag on a
GitHub-hosted runner. The `.sha256` file beside each archive only proves the
download is intact; the attestation proves who built it. With the GitHub CLI:

```bash
gh attestation verify pixel-vX.Y.Z-aarch64-apple-darwin.tar.gz --repo Pixel-CLI/pixel \
  --signer-workflow Pixel-CLI/pixel/.github/workflows/release.yml \
  --source-ref refs/tags/vX.Y.Z --deny-self-hosted-runners
```

The same command verifies `install.sh` before you pipe it into `sh`.
Releases before the first attested one have no attestation.

v0.6.0 and v0.6.1 were signed before the repository moved from
`LivioGama/pixel` to `Pixel-CLI/pixel`, and their attestations stayed with
the former owner: `--repo Pixel-CLI/pixel` answers `HTTP 404` for them.
Verify those two under the name they were built as:

```bash
gh attestation verify pixel-v0.6.1-aarch64-apple-darwin.tar.gz --repo LivioGama/pixel \
  --signer-workflow LivioGama/pixel/.github/workflows/release.yml \
  --source-ref refs/tags/v0.6.1 --deny-self-hosted-runners
```

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
