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

### Reproducing a release build

Release binaries are built reproducibly: the same tag, rebuilt with the same
Rust toolchain and the same `cross` version, gives a byte-identical `pixel`.
`scripts/release-build-env.sh` sets what would otherwise vary between two
builds: `SOURCE_DATE_EPOCH`, the commit's time, for the build date
`pixel --version` prints, and `RUSTFLAGS` remapping the checkout and
`CARGO_HOME` out of the binary's panic messages and debug information.
`release-build.yml` builds every release with it, and
`.github/workflows/reproducible-build.yml` builds the
`x86_64-unknown-linux-musl` binary twice, from two checkouts at different
paths, and fails unless the two match. To rebuild a Linux release yourself
(Docker, a Rust toolchain, and the `rustc` version that release's
`pixel --version` prints):

```bash
git clone https://github.com/Pixel-CLI/pixel && cd pixel && git switch --detach vX.Y.Z
cargo install cross --locked --version 0.2.5   # the version release-build.yml pins
env_lines=$(scripts/release-build-env.sh) && eval "$env_lines"
cross +1.NN.N build --release --locked --no-default-features --features model2vec \
  --target x86_64-unknown-linux-musl -p pixel-cli
sha256sum target/x86_64-unknown-linux-musl/release/pixel
gh release download vX.Y.Z --repo Pixel-CLI/pixel \
  --pattern 'pixel-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz'
tar -xzOf pixel-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz \
  pixel-vX.Y.Z-x86_64-unknown-linux-musl/bin/pixel | sha256sum
```

Compare the binary, not the archive: `tar` records file times. The
`aarch64-unknown-linux-musl` binary rebuilds the same way with its target;
the `aarch64-apple-darwin` one is built with the same environment, but no CI
job compares two of its builds. Releases cut before this script existed
embed their build day and the runner's paths, and do not reproduce.

### Software bill of materials

Releases after v0.6.1 attach a CycloneDX 1.5 JSON SBOM beside each archive,
`pixel-vX.Y.Z-<target>.cdx.json`. It lists the crates that target's binary
compiles, with their versions, licences and package URLs, and their
dependency graph: the Linux archives are built with `--no-default-features
--features model2vec`, the macOS one with the default features (fastembed
and its ONNX Runtime included), so the two SBOMs differ. Build-time crates
(build scripts, procedural macros) are listed; dev-dependencies are not. The
Linux bottles hold the binary of the matching Linux archive, so that
archive's SBOM describes them. `release-build.yml` writes it with
cargo-cyclonedx, then keeps only the crates `cargo tree -p pixel-cli`
resolves for that target and feature set: `cargo metadata`, which
cargo-cyclonedx reads, also lists optional dependencies the build never
compiles. Native code a crate bundles (the ONNX Runtime `ort-sys` links on
macOS) appears as that crate, not as a component of its own; the Rust
standard library and the target's C library are not listed.

Each SBOM is a subject of the release's provenance attestation, so the
commands above verify it like an archive, with or without `--bundle`:

```bash
gh attestation verify pixel-vX.Y.Z-aarch64-apple-darwin.cdx.json --repo Pixel-CLI/pixel \
  --signer-workflow Pixel-CLI/pixel/.github/workflows/release-build.yml \
  --source-ref refs/tags/vX.Y.Z --deny-self-hosted-runners
```

A verified SBOM can then go to any CycloneDX consumer, a vulnerability
scanner for instance: `grype sbom:pixel-vX.Y.Z-aarch64-apple-darwin.cdx.json`.

### VirusTotal reports

Each release's notes end with a `VirusTotal` section linking the report of
every archive, by its sha256, once the release workflow has submitted them
(from the first release after v0.6.1, when the repository's `VT_API_KEY`
secret is set). A scan says what antivirus engines think of the file; the
attestation above is what proves where it came from.

## Security model

Pixel runs locally and processes repository data. [docs/threat-model.md](docs/threat-model.md) is the full threat model and attack surface analysis: actors, trust boundaries, each threat with its mitigation in the code and its residual risk. [docs/assurance-case.md](docs/assurance-case.md) argues from it why the requirements below hold: the secure design principles applied and the common weaknesses countered. Key security boundaries:

- **Daemon socket**: per-user directory (0700) on Linux, per-user TMPDIR on macOS. Socket file is 0600. No cross-user access.
- **A `.pixel/` that comes from the repository**: `.pixel/` holds pixel's derived state and is git-ignored, but a repository can still commit it, symbolic links included. Before the index, graph and history stores read anything there (and before `pixel index unpack` installs into it), pixel refuses a `.pixel` that is a symbolic link or that holds files git tracks (`git ls-files .pixel` prints something), and names the command that removes it. The integrity checks inside those files (the history db's `_pixel_marker` table, extractor ids, the graph freshness signature) only tell pixel's current files from stale or foreign ones; they can be forged and are not a trust check.
- **Sidecar files** (`.pixel/`): directory is 0700, flow files and action logs are 0600. Flow files may contain fill values (passwords, OTPs) from flow replay. Pixel never writes, truncates or chmods through a symbolic link there: files are opened with `O_NOFOLLOW` or created fresh and renamed into place, permissions are set on the open file, a linked directory is refused, and the SQLite databases are opened with `SQLITE_OPEN_NOFOLLOW`.
- **Paths read from the index or the graph**: a stored path is used only when it is a plain relative path whose directory resolves inside the repository; search never reads a file outside it, and `pixel rename` and `pixel plan-rollback --apply` only write inside it, never through a link. Search results leave out files named like credentials (`.env*`, keys, `credentials`, `.netrc`, `.git-credentials`, …).
- **Command/argument injection**: all user input passed to shell commands is sanitized via `ref_guard`.
- **No telemetry**: pixel sends no usage data anywhere. The one request it makes on its own is the release check below, which carries no repository or usage data.
- **Network access**: limited to first-use model downloads (Hugging Face), explicit Git remote operations (`pixel fetch`, `pixel push`, `pixel commit-and-push`), and two opt-in commands that send their input out: `pixel classify` sends its question to the model endpoint you configure (OpenRouter, Ollama Cloud or a local server), and `pixel web-search` sends its query to the SearXNG endpoint you configure (`PIXEL_WEB_SEARCH_URL`) and nowhere else, even when it answers thinly or not at all; without one, it sends the query to DuckDuckGo, then to Wikipedia while the hits are fewer than `--limit` (8 by default). At most once a day, when a command's stderr is a terminal, one `HEAD` request to `https://github.com/Pixel-CLI/pixel/releases/latest` reads the latest release tag (GitHub sees the IP address and a `pixel-cli/<version>` user agent); it never runs for hooks, agents or under `CI`, and `PIXEL_NO_UPDATE_CHECK=1` turns it off. The index and its sidecar files never leave the machine.

## Known limitations

- The tracked-file check runs when a store opens. The small state files under `.pixel/` that other commands and hooks read (the task map, repository settings) are not checked against git on each read: pixel never writes through a link there, but before running pixel in a clone you do not trust, `git ls-files .pixel` should print nothing; if it prints anything, delete `.pixel/`.

- The daemon does not perform peer-credential checking on incoming socket connections. The 0700 directory permission is the primary access control. On a shared system where another user can bypass directory permissions (e.g. root), additional hardening may be needed.
- The `fastembed` feature downloads models from Hugging Face at runtime. This is opt-in (enabled by default, can be disabled with `--no-default-features`).

## Static analysis findings

CodeQL (Rust, GitHub Actions, Python, JavaScript/TypeScript) runs on every
pull request, every push to `main` and weekly; Clippy runs with warnings
denied on every pull request. The remediation threshold:

- A pull request does not merge with a new CodeQL finding of medium severity
  or higher: it is fixed in the pull request, or dismissed in code scanning
  with a written reason when it is a false positive or unreachable.
- A finding of high or critical severity found on `main` (a new query, a
  scan of older code) is fixed within 14 days, a medium one within 30 days;
  low and note findings are fixed or dismissed with a reason at the next
  release.
- Clippy has no threshold: any warning fails CI.
- Dependency findings (cargo-deny, osv-scanner) follow the dependency policy
  in CONTRIBUTING.md.
