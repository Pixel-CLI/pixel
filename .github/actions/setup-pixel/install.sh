#!/usr/bin/env bash
set -euo pipefail

fail() { echo "::error::$*" >&2; exit 1; }
version=${PIXEL_ACTION_VERSION:-}
[[ "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail 'version must be an exact release tag: vX.Y.Z'
case "${PIXEL_ACTION_PREPARE:-true}" in
  true|false) ;;
  *) fail 'prepare must be true or false' ;;
esac
case "${RUNNER_OS:?}/${RUNNER_ARCH:?}" in
  Linux/X64) target=x86_64-unknown-linux-musl ;;
  Linux/ARM64) target=aarch64-unknown-linux-musl ;;
  macOS/ARM64) target=aarch64-apple-darwin ;;
  *) fail "No Pixel release for $RUNNER_OS/$RUNNER_ARCH" ;;
esac

# Each invocation owns its directory; no sudo or replacement of a runner's CLI.
staging=$(mktemp -d "${RUNNER_TEMP:?}/pixel-setup.XXXXXX")
archive="pixel-$version-$target.tar.gz"
base="https://github.com/Pixel-CLI/pixel/releases/download/$version"
for file in "$archive" "$archive.sha256"; do
  curl --fail --silent --show-error --location --retry 3 \
    --proto '=https' --tlsv1.2 "$base/$file" -o "$staging/$file"
done
(
  cd "$staging"
  if command -v sha256sum >/dev/null; then
    sha256sum -c "$archive.sha256"
  else
    shasum -a 256 -c "$archive.sha256"
  fi
  tar xzf "$archive" "pixel-$version-$target/bin/pixel"
)
bin="$staging/pixel-$version-$target/bin/pixel"
[[ "$("$bin" -V)" == "pixel ${version#v}" ]] || fail 'Downloaded binary version does not match the requested release'
printf '%s\n' "$(dirname "$bin")" >> "${GITHUB_PATH:?}"
printf 'version=%s\nbin=%s\n' "$version" "$bin" >> "${GITHUB_OUTPUT:?}"
# Subsequent agent steps also stay daemon-free.
printf 'PIXEL_DAEMON_AUTO_START=0\n' >> "${GITHUB_ENV:?}"
# shellcheck disable=SC2016 # Markdown backticks are literal.
printf '### Pixel\nInstalled `%s` for `%s` (SHA-256 verified).\n' \
  "$version" "$target" >> "${GITHUB_STEP_SUMMARY:?}"
