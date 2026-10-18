#!/usr/bin/env bash
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Lay the code-search embedding model into the caches pixel reads, at one
# pinned Hugging Face revision, so the ranking lane never resolves `main` on
# the hub.
#
# `pixel-recall` opens the model through model2vec-rs 0.3, which takes no
# revision and builds its hub client with `Api::new()`: that client ignores
# the HF_HOME pixel sets and reads hf-hub's default cache,
# `~/.cache/huggingface/hub`. There, `<repo>/refs/main` names a commit and
# `snapshots/<commit>/<file>` is served from disk without a request; only a
# miss downloads. Writing that layout at REVISION, each file checked against
# its sha256, pins the bytes the NDCG floors are measured on. The potion
# marker under `~/.local/share/pixel/models` is the one `potion_cached`
# reads. Already-valid files are kept, so a restored cache costs no network.
#
# usage: fetch-embedding-model.sh <home>      (CI: $HOME)
#        fetch-embedding-model.sh --cache-key prints the actions/cache key
#
# PIXEL_MODEL_BASE_URL replaces https://huggingface.co (tests serve file://).
set -euo pipefail

# minishlab/potion-code-16M-v2 is `CODE_SEARCH_MODEL_REPO` in
# crates/pixel-recall/src/code_search.rs; keep the two equal.
REPO=minishlab/potion-code-16M-v2
REVISION=e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b
FILES=(config.json tokenizer.json model.safetensors)
SHA256=(
    148e5691a6fcc553437156859701fba017a1ba5d340b170f17e0f3668fb861a7
    107bbdcbad4bff1d299b7a4c3a2fb17c52890688b7dd0e4c9deab79d3c4f3d45
    75cf7a6c2171b230ad19b1e7d8e0b1aee86da5a02af8e7cacedd9921d227623c
)

if [[ ${1:-} == --cache-key ]]; then
    echo "embedding-model-${REPO//\//--}-${REVISION}"
    exit 0
fi
if [[ $# -ne 1 ]]; then
    echo "usage: fetch-embedding-model.sh <home> | --cache-key" >&2
    exit 2
fi

home=$1
base=${PIXEL_MODEL_BASE_URL:-https://huggingface.co}
models="$home/.local/share/pixel/models"
hub="$home/.cache/huggingface/hub/models--${REPO//\//--}"
snapshot="$hub/snapshots/$REVISION"
marker="$models/potion-${REPO//\//--}.ok"

sha256_of() {
    if command -v sha256sum >/dev/null; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

mkdir -p "$snapshot" "$hub/refs" "$models"
for i in "${!FILES[@]}"; do
    file=${FILES[$i]}
    want=${SHA256[$i]}
    dest="$snapshot/$file"
    if [[ -f $dest && $(sha256_of "$dest") == "$want" ]]; then
        echo "kept $file ($want)"
        continue
    fi
    curl --fail --silent --show-error --location --retry 3 \
        --output "$dest.part" "$base/$REPO/resolve/$REVISION/$file"
    got=$(sha256_of "$dest.part")
    if [[ $got != "$want" ]]; then
        rm -f "$dest.part"
        echo "error: $file at $REVISION has sha256 $got, expected $want" >&2
        exit 1
    fi
    mv "$dest.part" "$dest"
    echo "fetched $file ($want)"
done
printf '%s' "$REVISION" >"$hub/refs/main"
printf '%s' "$REPO" >"$marker"
echo "pinned $REPO at $REVISION under $home"
