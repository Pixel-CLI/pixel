#!/bin/bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
#
# One first-party plugin release, installed and run the way a user does, with
# a pixel built from the current tree (.github/workflows/plugins-install.yml).
# This is the lane the issue asks for: it keeps "the released plugins work
# with the version on main" true continuously, and fails when a release is
# missing, a checksum mismatches or a command exits unexpectedly.
#
#   usage: plugins-smoke.sh <pixel-binary> <repo> <plugin-name>
#     <pixel-binary>  the freshly built binary (never an installed one: pass
#                     PATH through deliberately or the lane tests a stale
#                     install).
#     <repo>          the plugin repository, e.g. pixel-git.
#     <plugin-name>   the manifest's `name`, e.g. git.
#
# Steps, each fatal on failure:
#
#   hint      With the plugin absent, `pixel <word>` refuses with status 2
#             and names the repository that owns the command.
#   install   `pixel install github:Pixel-CLI/<repo>` reports the plugin and
#             downloads the release asset over the repository's launcher —
#             `(sha256 verified)`; no "not runnable" wording may appear
#             (#1045's regression).
#   identity  The installed file's hash equals the published `.sha256`, so
#             what the host serves is the released asset, not the launcher.
#   run       `pixel <command>` answers for every command the manifest lists,
#             and that list is exactly what `invoke.rs` moves to this
#             repository — drift in either direction fails here.
#   reads     The read commands answer a real invocation: the git plugin in a
#             fixture repository, telemetry on a fresh HOME. The others are
#             networked or interactive and are validated by `--help` alone.

set -uo pipefail

fail() {
  printf '::error::%s\n' "$*"
  exit 1
}

[ "$#" -eq 3 ] || fail "usage: $0 <pixel-binary> <repo> <plugin-name>"
pixel="$1"
repo="$2"
plugin="$3"

pixel="$(cd "$(dirname "$pixel")" && pwd)/$(basename "$pixel")"
[ -x "$pixel" ] || fail "not executable: $pixel"

# The lane runs in a checkout, so the two tables that must never drift can be
# held against each other here instead of being copied: the commands
# `invoke.rs` names for this repository, and the ones its release declares.
root="$(cd "$(dirname "$0")/../.." && pwd)"
invoke="$root/crates/pixel-plugin/src/invoke.rs"
moved_list="$(sed -n 's/^[[:space:]]*("\([^"]*\)", "\([^"]*\)"),$/\2 \1/p' "$invoke" \
  | awk -v repo="$repo" '$1 == repo {print $2}')"
[ -n "$moved_list" ] \
  || fail "$invoke declares no command for $repo — a plugin release has nothing to smoke"
moved_count="$(printf '%s\n' "$moved_list" | wc -l | tr -d ' ')"

S="$(mktemp -d)"
trap 'rm -rf "$S"' EXIT
mkdir -p "$S/home"
export HOME="$S/home"

case "$(uname -s)" in
  Linux) os=unknown-linux-musl ;;
  Darwin) os=apple-darwin ;;
  *) fail "no plugin asset triple for $(uname -s)" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) fail "no plugin asset arch for $(uname -m)" ;;
esac
asset="pixel-$plugin-$arch-$os"
base="https://github.com/Pixel-CLI/$repo/releases/latest/download"

manifest="$HOME/.pixel/plugins/$plugin/pixel-plugin.toml"

manifest_version() {
  sed -n -E 's/^version = "([^"]*)".*/\1/p' "$manifest" | head -n 1
}

# --- hint: the core itself says where the command moved
# `check-release` is deliberately absent: the core keeps it
# (`available.rs::stays_in_core`), so the first remaining moved word is asked.
hint_word="$(printf '%s\n' "$moved_list" | grep -vx check-release | head -n 1)"
[ -n "$hint_word" ] || fail "every command invoke.rs moves for $repo is kept by the core"
if hint_lines="$( "$pixel" "$hint_word" 2>&1 )"; then
  printf '%s\n' "$hint_lines" | sed 's/^/    /'
  fail "pixel $hint_word without the plugin exited 0: the moved hint is not a refusal"
fi
hint_want="\"$hint_word\" moved to a plugin: pixel install github:Pixel-CLI/$repo"
printf '  hint    %s\n' "$hint_lines"
printf '%s\n' "$hint_lines" | grep -qF "$hint_want" \
  || fail "the hint for $hint_word is not exactly '$hint_want': $hint_lines"

# --- install: the released asset replaces the repository's launcher
if install_lines="$( "$pixel" install "github:Pixel-CLI/$repo" 2>&1 )"; then
  :
else
  printf '%s\n' "$install_lines" | sed 's/^/    /'
  fail "pixel install github:Pixel-CLI/$repo exited nonzero"
fi
printf '%s\n' "$install_lines" | sed 's/^/    /'
printf '%s\n' "$install_lines" | grep -qF "installed plugin $plugin " \
  || fail "the install did not report the plugin: $install_lines"
printf '%s\n' "$install_lines" | grep -qF "(sha256 verified)" \
  || fail "the install downloaded nothing checksum-verified; the release asset was not installed over the launcher: $install_lines"
printf '%s\n' "$install_lines" | grep -qiF "not runnable" \
  && fail "the core reported the installed program not runnable: $install_lines"

[ -f "$manifest" ] || fail "the manifest never landed at $manifest"
version="$(manifest_version)"
[ -n "$version" ] || fail "$manifest states no version"

# --- identity: the installed file is the published asset, not the launcher
run="$(sed -n -E 's/^run = "([^"]*)".*/\1/p' "$manifest" | head -n 1)"
[ -n "$run" ] || fail "$manifest states no run"
installed="$HOME/.pixel/plugins/$plugin/$run"
[ -f "$installed" ] || fail "nothing at the run path $installed"
curl -fsSL "$base/$asset.sha256" -o "$S/asset.sha256" \
  || fail "$asset carries no published .sha256 at $base"
want_hash="$(awk 'NR==1 {print $1}' "$S/asset.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  got_hash="$(sha256sum "$installed" | awk 'NR==1 {print $1}')"
else
  got_hash="$(shasum -a 256 "$installed" | awk 'NR==1 {print $1}')"
fi
[ "$got_hash" = "$want_hash" ] \
  || fail "the installed file is not the released asset: sha256 $got_hash, $asset published $want_hash"

# --- enable, version, routing
"$pixel" plugin enable "$plugin" >/dev/null \
  || fail "pixel plugin enable $plugin failed"
want_version="pixel-$plugin $version"
got_version="$("$installed" --version)"
[ "$got_version" = "$want_version" ] \
  || fail "$installed prints '$got_version', the manifest says '$want_version'"

manifest_commands="$(awk '/^commands = \[/ {inside = 1; next} inside && /^\]/ {inside = 0} inside {gsub(/[",]/, ""); print}' "$manifest" | tr -d ' \t')"
[ -n "$manifest_commands" ] || fail "$manifest lists no commands"
for command in $manifest_commands; do
  "$pixel" "$command" --help >/dev/null 2>&1 \
    || fail "pixel $command --help did not route through the core to the installed plugin"
done

sorted_manifest="$(printf '%s\n' "$manifest_commands" | sort)"
sorted_moved="$(printf '%s\n' "$moved_list" | sort)"
if ! cmp -s <(printf '%s\n' "$sorted_manifest") <(printf '%s\n' "$sorted_moved"); then
  fail "the tables drift: $manifest lists [$(printf '%s' "$sorted_manifest" | tr '\n' ' ')]; invoke.rs moves for $repo [$(printf '%s' "$sorted_moved" | tr '\n' ' ')"
fi

# --- reads: the read commands answer a real invocation
reads=0
if [ "$repo" = pixel-git ]; then
  f="$S/fixture"
  if ! mkdir -p "$f"; then fail "mkdir $f failed"; fi
  cd "$f" || fail "cd $f failed"
  git init -q .
  git config user.email plugin-smoke@invalid.example
  git config user.name "pixel plugin smoke"
  printf 'one\n' > file.txt
  git add file.txt
  git commit -qm "the first commit"
  for read_word in repo-state commit-history list-branches; do
    "$pixel" "$read_word" --json >/dev/null 2>&1 \
      || fail "pixel $read_word --json failed in the fixture repository"
    reads=$((reads + 1))
  done
elif [ "$repo" = pixel-telemetry ]; then
  # The lane runs in the pixel checkout, whose state under HOME is fresh.
  # All three answer the empty case with status 0 ('space' reports the absent
  # index and stops).
  for read_word in action-log token-savings space; do
    "$pixel" "$read_word" >/dev/null 2>&1 \
      || fail "pixel $read_word failed on a fresh HOME"
    reads=$((reads + 1))
  done
fi

printf '  %s %s (%s): hint, install sha256 verified, %s commands routed and equal to invoke.rs, %s real reads\n' \
  "$repo" "$plugin" "$version" "$moved_count" "$reads"
