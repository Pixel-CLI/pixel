#!/bin/sh
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# decide-local-setup.sh — set up the local 4B decision backend for
# `pixel classify --remote-preset local`.
#
# The `local` preset points the same OpenAI-compatible adapter at a
# llama.cpp server (or plain Ollama) on localhost, so a 4B model runs the
# decision on your own CPU/GPU with $0 marginal cost per decision.
#
# This script downloads the GGUF and prints the launch command. It does not
# start a long-running server in the foreground (you run that yourself in a
# terminal or as a service).
#
# Usage: decide-local-setup.sh [llama|ollama]   (default: llama)
# Requires: a llama.cpp build (`llama-server` on PATH) OR Ollama, and curl.
set -eu

# The Qwen model this repo's decision recipe targets: the public Qwen3-4B
# GGUF repository. Override with the env vars to pick a different GGUF.
MODEL_REPO="${QWEN_GGUF_REPO:-Qwen/Qwen3-4B-GGUF}"
MODEL_FILE="${QWEN_GGUF_FILE:-Qwen3-4B-Q4_K_M.gguf}"
MODEL_DIR="${QWEN_GGUF_DIR:-$HOME/.cache/pixel/decide-local}"

# Which runtime to prepare: `llama` or `ollama`.
RUNTIME="${1:-llama}"

case "$RUNTIME" in
  llama|ollama) ;;
  *)
    echo "error: unknown runtime '$RUNTIME' (expected 'llama' or 'ollama')." >&2
    exit 2
    ;;
esac

if [ "$RUNTIME" = "ollama" ]; then
  # Ollama path: `ollama pull qwen3:4b && ollama serve`
  if ! command -v ollama >/dev/null 2>&1; then
    echo "error: 'ollama' not found. Install Ollama, then re-run." >&2
    exit 1
  fi
  echo "Pulling qwen3:4b (this can take a few minutes on first run)…"
  ollama pull qwen3:4b
  echo
  echo "Next, run in a terminal:"
  echo "  ollama serve"
  echo
  echo "Then classify with:"
  echo "  pixel classify '...' --label a --label b --remote-preset local --remote-model qwen3:4b"
  echo
  echo "Model cached for Ollama. The '--remote-model' above matches the Ollama tag."
  exit 0
fi

if ! command -v llama-server >/dev/null 2>&1; then
  echo "error: 'llama-server' not found on PATH." >&2
  echo "Build llama.cpp (or install a packaged llama-server) first, e.g.:" >&2
  echo "  cmake -B build && cmake --build build -j --config Release" >&2
  echo "then add build/bin to PATH." >&2
  exit 1
fi
if ! command -v curl >/dev/null 2>&1; then
  echo "error: 'curl' not found." >&2
  exit 1
fi

mkdir -p "$MODEL_DIR"
MODEL_PATH="$MODEL_DIR/$MODEL_FILE"

if [ ! -f "$MODEL_PATH" ]; then
  URL="https://huggingface.co/$MODEL_REPO/resolve/main/$MODEL_FILE"
  echo "Downloading $URL → $MODEL_PATH (this can be a few GB)…"
  curl -L --fail --progress-bar -o "$MODEL_PATH.tmp" "$URL"
  mv "$MODEL_PATH.tmp" "$MODEL_PATH"
fi

# printf, not echo: a POSIX sh `echo` may interpret the backslashes below.
printf '\n'
printf 'Model ready at: %s\n' "$MODEL_PATH"
printf '\n'
printf 'Launch the server in a terminal:\n'
printf "  llama-server -m '%s' --port 11434 -c 8192\n" "$MODEL_PATH"
printf '\n'
printf 'Verify it answers:\n'
printf '%s\n' "  curl -s http://localhost:11434/v1/chat/completions -H 'Content-Type: application/json' \\"
printf '%s\n' "    -d '{\"model\":\"qwen3-4b\",\"messages\":[{\"role\":\"user\",\"content\":\"ping\"}]}'"
printf '\n'
printf 'Then classify with:\n'
printf '%s\n' "  pixel classify '...' --label a --label b --remote-preset local --remote-model qwen3-4b"
