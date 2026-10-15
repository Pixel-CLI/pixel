#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Copy pixel-mvp's index crates, or its French lexicon, into this workspace
# at one commit.
#
#   scripts/dev/vendor-mvp.sh <pixel-mvp checkout> <commit> <part>...
#   scripts/dev/vendor-mvp.sh ../pixel-mvp eae8201 model store index lexicon
#
# Each <part> is a crate's short name, `model`, `store` or `index`, or
# `lexicon`. For a crate, pixel-mvp's
# crates/pixel-<crate>/ at <commit> replaces crates/mvp-<crate>/ here, except
# Cargo.toml and VENDORED.md, which belong to this workspace. The copy is
# mechanical, and every sync repeats it the same way:
#
#   - read from git at <commit> (`git archive`), never from the working tree;
#   - the crate names are renamed: pixel-model, pixel-store and pixel-index
#     become mvp-model, mvp-store and mvp-index (this workspace already has a
#     pixel-index, the text index), in identifiers and in prose alike;
#   - every .rs file gets the SPDX header scripts/ci/check-spdx.py asks for;
#   - `cargo fmt` formats the copy: the shorter names reorder some imports;
#   - VENDORED.md is rewritten with the commit.
#
# `lexicon` is data, not a crate: pixel-mvp's
# crates/pixel-adapter/lexicon/fr-en.tsv at <commit> replaces
# crates/mvp-lexicon/fr-en.tsv here byte for byte (ADR-0032 there: French
# words and the English words code names them with), and
# crates/mvp-lexicon/VENDORED.md is rewritten with the commit. Nothing in it
# is renamed or formatted.
#
# ADR-0035 of the pixel-mvp research repository decided the copy: pixel-mvp
# stays the source of these crates, a fix lands there first, and each sync
# names its commit.
set -eu

if [ "$#" -lt 3 ]; then
    sed -n '5,9p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
fi
src=$1
commit=$2
shift 2

full=$(git -C "$src" rev-parse --verify "$commit^{commit}")
short=$(printf '%s' "$full" | cut -c1-7)
root=$(git rev-parse --show-toplevel)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

# The lexicon: one data file, copied as it is.
vendor_lexicon() {
    from=crates/pixel-adapter/lexicon/fr-en.tsv
    to=$root/crates/mvp-lexicon
    git -C "$src" archive --format=tar "$full" "$from" | tar -x -C "$scratch"
    mkdir -p "$to"
    cp "$scratch/$from" "$to/fr-en.tsv"
    cat >"$to/VENDORED.md" <<EOF
# mvp-lexicon (vendored)

\`fr-en.tsv\` is a copy of \`crates/pixel-adapter/lexicon/fr-en.tsv\` of
pixel-mvp at commit \`$full\` (\`$short\`), made by
\`scripts/dev/vendor-mvp.sh\`, byte for byte. It is data, not a crate: one
French form per line, a tab, and the English words code names its notion
with, or \`-\` for a word that carries no meaning in a request. ADR-0032 of
the pixel-mvp research repository decided the lexicon; ADR-0036 moves it
into Pixel's prompt-submit brief.

- **Do not edit \`fr-en.tsv\` here.** pixel-mvp rebuilds it with
  \`crates/pixel-adapter/lexicon/generate.py\` and measures a change on its
  French development set before it ships: a change lands there first, then
  comes here through the next sync, which names its commit.
- **Sync:** \`scripts/dev/vendor-mvp.sh <pixel-mvp checkout> <commit> lexicon\`,
  then the gates, in a pull request that names the commit.
EOF
}

for crate in "$@"; do
    case $crate in
        model | store | index) ;;
        lexicon)
            vendor_lexicon
            continue
            ;;
        *)
            echo "vendor-mvp: unknown part '$crate' (model, store, index or lexicon)" >&2
            exit 2
            ;;
    esac
    from=crates/pixel-$crate
    to=$root/crates/mvp-$crate
    git -C "$src" archive --format=tar "$full" "$from" | tar -x -C "$scratch"
    [ -d "$scratch/$from/src" ] || {
        echo "vendor-mvp: $from/src is not in $short" >&2
        exit 1
    }
    rm -f "$scratch/$from/Cargo.toml"
    mkdir -p "$to"
    # Everything the workspace does not own goes, so a file deleted
    # upstream is deleted here too.
    find "$to" -mindepth 1 -maxdepth 1 ! -name Cargo.toml ! -name VENDORED.md \
        -exec rm -rf {} +
    cp -R "$scratch/$from/." "$to/"
    find "$to" -type f ! -name Cargo.toml ! -name VENDORED.md -print | while read -r file; do
        perl -pi -e 's/\bpixel([-_])(model|store|index)\b/mvp$1$2/g' "$file"
    done
    find "$to" -type f -name '*.rs' -print | while read -r file; do
        if ! head -n 6 "$file" | grep -q 'SPDX-License-Identifier'; then
            {
                printf '%s\n' '// SPDX-FileCopyrightText: The Pixel contributors' \
                    '// SPDX-License-Identifier: MIT' ''
                cat "$file"
            } >"$file.vendor" && mv "$file.vendor" "$file"
        fi
    done
    cat >"$to/VENDORED.md" <<EOF
# mvp-$crate (vendored)

This crate is a copy of \`crates/pixel-$crate\` of pixel-mvp, the
experimental successor of Pixel's index, at commit \`$full\`
(\`$short\`), made by \`scripts/dev/vendor-mvp.sh\`. ADR-0035 of the pixel-mvp
research repository decided the copy: the MVP's Rust and Ruby index
becomes the source of Pixel's code graph for the languages where it is
shown to be right, behind a switch.

- **Do not edit \`src/\` here.** A fix lands in pixel-mvp first, then comes
  here through the next sync, which names its commit. The only
  differences from the source are mechanical: the crate names
  (\`pixel-model\`, \`pixel-store\`, \`pixel-index\` read \`mvp-model\`,
  \`mvp-store\`, \`mvp-index\`), the SPDX header of each \`.rs\` file,
  and \`cargo fmt\` over the result.
- **\`Cargo.toml\` is this workspace's**: the dependency versions follow
  the workspace's \`Cargo.lock\`, and the crate keeps pixel-mvp's own lint
  table instead of the workspace's, so that a sync stays a copy. The
  nightly mutation campaign leaves the crate out (\`.cargo/mutants.toml\`):
  a survivor could only be fixed in pixel-mvp.
- **Sync:** \`scripts/dev/vendor-mvp.sh <pixel-mvp checkout> <commit> $crate\`,
  then the gates, in a pull request that names the commit.
EOF
done
# rustfmt, not `cargo fmt -p`: a crate new to the workspace is no member
# until its manifest is written, and rustfmt follows the module tree from
# the crate root either way.
for crate in "$@"; do
    [ "$crate" = lexicon ] || rustfmt --edition 2024 "$root/crates/mvp-$crate/src/lib.rs"
done
echo "vendored $* at $short"
