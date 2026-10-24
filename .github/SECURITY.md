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

Every release archive, its SBOM and its `install.sh` carry a signed
build-provenance attestation from `.github/workflows/release-build.yml`, the
reusable workflow that builds and signs the release on its tag on a
GitHub-hosted runner. The `.sha256` file beside each archive only proves the
download is intact; the attestation proves who built it. With the GitHub CLI:

```bash
gh attestation verify pixel-vX.Y.Z-aarch64-apple-darwin.tar.gz --repo Pixel-CLI/pixel \
  --signer-workflow Pixel-CLI/pixel/.github/workflows/release-build.yml \
  --source-ref refs/tags/vX.Y.Z --deny-self-hosted-runners
```

The same command verifies `install.sh` before you pipe it into `sh`.

Each release also attaches the attestation itself as `pixel-vX.Y.Z.intoto.jsonl`:
the Sigstore bundle of that one attestation, covering every archive, SBOM
and `install.sh` of the release. `--bundle` checks a file against it
instead of fetching the attestation from GitHub, with the same identity flags;
adding a trusted root saved beforehand with
`gh attestation trusted-root > trusted_root.jsonl` makes the check fully offline:

```bash
gh attestation verify pixel-vX.Y.Z-aarch64-apple-darwin.tar.gz --repo Pixel-CLI/pixel \
  --bundle pixel-vX.Y.Z.intoto.jsonl --custom-trusted-root trusted_root.jsonl \
  --signer-workflow Pixel-CLI/pixel/.github/workflows/release-build.yml \
  --source-ref refs/tags/vX.Y.Z --deny-self-hosted-runners
```

Signing in a reusable workflow is what makes this SLSA Build Level 3: the
signing identity belongs to `release-build.yml`, which holds no secret, and
`release.yml`, which publishes the release, cannot sign.

### Reproducing a release build

Release binaries are built reproducibly: the same tag, rebuilt with the same
Rust toolchain and the same `cross` version, gives a byte-identical `pixel`.
`scripts/release/release-build-env.sh` sets what would otherwise vary between two
builds: `SOURCE_DATE_EPOCH`, the commit's time, for the build date
`pixel --version` prints, `RUSTFLAGS` remapping the checkout and `CARGO_HOME` out of
the binary's panic messages and debug information, and
`CARGO_PROFILE_RELEASE_STRIP=none`: the build keeps its symbols, and
`scripts/release/split-symbols.sh` strips the copy the archive ships (it needs
`rustup component add llvm-tools` on Linux). `release-build.yml` builds every
release with it, and `.github/workflows/reproducible-build.yml` builds the
`x86_64-unknown-linux-musl` binary twice, from two checkouts at different paths,
and fails unless the two match. To rebuild a Linux release yourself (Docker, a
Rust toolchain, and the `rustc` version that release's `pixel --version` prints):

```bash
git clone https://github.com/Pixel-CLI/pixel && cd pixel && git switch --detach vX.Y.Z
cargo install cross --locked --version 0.2.5   # the version release-build.yml pins
env_lines=$(scripts/release/release-build-env.sh) && eval "$env_lines"
cross +1.NN.N build --release --locked --target x86_64-unknown-linux-musl -p pixel
scripts/release/split-symbols.sh target/x86_64-unknown-linux-musl/release/pixel /tmp/pixel.symbols.tar.xz
sha256sum target/x86_64-unknown-linux-musl/release/pixel
gh release download vX.Y.Z --repo Pixel-CLI/pixel \
  --pattern 'pixel-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz'
tar -xzOf pixel-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz \
  pixel-vX.Y.Z-x86_64-unknown-linux-musl/bin/pixel | sha256sum
```

Compare the binary, not the archive: `tar` records file times. The
`aarch64-unknown-linux-musl` binary rebuilds the same way with its target; the
`aarch64-apple-darwin` one is built with the same environment, but no CI job
compares two of its builds.

### Symbols for a backtrace

The binary in each archive is stripped of its symbol table. The same build,
before stripping, ships beside it as `pixel-vX.Y.Z-<target>.symbols.tar.xz`,
covered by the release's provenance attestation; stripping moves no code, so its
symbols name the frames of the shipped binary. With `RUST_BACKTRACE=full`, a
panic prints each frame's address; resolve them against the unstripped copy:

```bash
gh release download vX.Y.Z --repo Pixel-CLI/pixel --pattern 'pixel-vX.Y.Z-<target>.symbols.tar.xz'
tar -xJf pixel-vX.Y.Z-<target>.symbols.tar.xz   # ./pixel, unstripped
atos -o pixel -l <load address> <frame address>…           # macOS
addr2line -f -C -e pixel <frame address - load address>…   # Linux
```

On macOS the load address is the `__TEXT` start the crash report lists
(`0x100000000` without ASLR); both copies carry the same Mach-O UUID
(`dwarfdump --uuid`).

### Software bill of materials

Each release attaches a CycloneDX 1.5 JSON SBOM beside each archive,
`pixel-vX.Y.Z-<target>.cdx.json`. It lists the crates that target's binary
compiles, with their versions, licences and package URLs, and their dependency
graph. Build-time crates (build scripts, procedural macros) are listed;
dev-dependencies are not. `release-build.yml` writes it
with cargo-cyclonedx, then keeps only the crates `cargo tree -p pixel` resolves for
that target: `cargo metadata`, which cargo-cyclonedx reads, also lists optional
dependencies the build never compiles, and `scripts/release/release-sbom.py`
refuses an SBOM that omits a crate the build compiles. The Rust standard library
and the target's C library are not listed.

Each SBOM is a subject of the release's provenance attestation, so the commands
above verify it like an archive, with or without `--bundle`:

```bash
gh attestation verify pixel-vX.Y.Z-aarch64-apple-darwin.cdx.json --repo Pixel-CLI/pixel \
  --signer-workflow Pixel-CLI/pixel/.github/workflows/release-build.yml \
  --source-ref refs/tags/vX.Y.Z --deny-self-hosted-runners
```

A verified SBOM can then go to any CycloneDX consumer, a vulnerability scanner for
instance: `grype sbom:pixel-vX.Y.Z-aarch64-apple-darwin.cdx.json`.

## Security model

PIXEL runs locally and processes repository data, some of it untrusted.
[`docs/threat-model.md`](../docs/threat-model.md) is the full threat model:
actors, trust boundaries, and each threat with its mitigation in the code and its
residual risk; [`specs/state-and-threats.md`](../specs/state-and-threats.md) is
the normative list of what PIXEL writes and where. Key boundaries:

- **Daemon socket.** Per repository, in `$XDG_RUNTIME_DIR` when set, else
  `~/.cache/pixel/sockets/` created 0700; the socket file is set to 0600 after
  `bind`. The daemon serves read ops only and does not check the peer's
  credentials: the owner-only directory is the access control.
- **A `.pixel/` that comes from the repository.** `.pixel/` holds PIXEL's derived state
  and is git-ignored, but a repository can still commit it, links included. Every
  store refuses a `.pixel` that is a link or that git tracks before it reads
  anything there, and names the command that removes it. Files are written
  through `O_NOFOLLOW`, directories created owner-only, SQLite opened with
  `SQLITE_OPEN_NOFOLLOW`.
- **Paths read back from the index or the graph** are used only after they are
  confined to the repository root; search never reads a file outside it, and
  `pixel rename` writes only inside it, never through a link. Files named like
  credentials (`.env*`, keys, `credentials`, `.netrc`, …) are left out of search
  results and briefs.
- **Hook input** is read for at most 1 MiB and 1 s, parsed strictly, and a command
  in it is inspected for repository paths, never executed. A hook exits 0 whatever
  happens.
- **Git** is run only through `pixel_io::GitRunner` (bounded in time and output,
  refs validated, pathspecs after `--`); the brief's own git reads are limited to
  `log`, `show`, `rev-list` and `tag`.
- **Agent configuration.** `pixel setup` records a receipt, backs up every file it
  changes, edits JSON/TOML/YAML key-wise and atomically, keeps foreign hooks, and
  `--remove` restores the bytes it replaced.
- **Secrets.** A credential given to `pixel config` is masked in the action log, the
  journal and error messages; keys are stored 0600 in `~/.pixel/config.yaml` or
  fetched per invocation from Infisical.
- **No telemetry.** PIXEL sends no usage data anywhere and makes no request on its
  own. The index, the graph and the journals never leave the machine.
- **Network access** happens only on a command that asks for it: `pixel classify`
  sends its question to the engine you configure (off by default; nothing else in
  PIXEL calls a model); `pixel install github:…` and `pixel index unpack <url>` fetch
  what you name.
- **Brief probes** are programs you installed; a repository's own probes run only
  at the digest `pixel plugin trust` recorded, with a cleared environment, in their
  own process group, killed at their budget.

## Known limitations

- The daemon does not perform peer-credential checking on incoming socket
  connections. The 0700 directory is the primary access control. On a shared
  system where another user can bypass directory permissions (root), additional
  hardening may be needed.
- PIXEL's output is repository data in an agent's context. It quotes instead of
  paraphrasing, adds an untrusted-data trailer where the host has a place for one
  (the pi extension does), but a model can still be steered by what a file says;
  PIXEL cannot prevent that, only avoid adding instructions of its own.
- The tracked-file check of `.pixel/` runs when a store opens. Before running PIXEL in
  a clone you do not trust, `git ls-files .pixel` should print nothing; if it prints
  anything, delete `.pixel/`.

## Static analysis findings

CodeQL (Rust, GitHub Actions, Python, JavaScript/TypeScript) runs on every pull
request (Rust after merge), every push to `main` and nightly; Clippy runs with
warnings denied on every pull request. A pull request does not merge with a new
CodeQL finding of medium severity or higher: it is fixed in the pull request, or
dismissed in code scanning with a written reason when it is a false positive or
unreachable. A finding of high or critical severity found on `main` is fixed
within 14 days, a medium one within 30 days.
