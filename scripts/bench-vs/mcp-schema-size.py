#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Measure what an MCP server costs in context before it answers anything.

Drives a real stdio handshake (initialize -> notifications/initialized ->
tools/list) and reports the serialized size of the tool schemas the model has to
carry on every turn. Reading the numbers off the source would miss whatever the
server composes at runtime, so the server is actually started.

Token figures are bytes/4, pixel's own accounting convention -- an estimate,
not a tokenizer count.

Usage: mcp-schema-size.py <label> -- <command> [args...]
"""

import json
import os
import selectors
import subprocess
import sys
import time

# Overridable so the deadline path itself can be tested cheaply.
TIMEOUT_S = float(os.environ.get("MCP_TIMEOUT_S", 90))


def read_line(stream, deadline):
    """One line, or None once the deadline passes.

    `readline()` blocks with no way out: the old loop checked its deadline only
    after a complete line came back, so a server that held stdout open without
    writing a newline pinned the benchmark indefinitely and never reached the
    kill. Waiting on the file descriptor first makes the deadline real.
    """
    sel = selectors.DefaultSelector()
    sel.register(stream, selectors.EVENT_READ)
    try:
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return None
            if not sel.select(timeout=min(remaining, 1.0)):
                continue
            line = stream.readline()
            return line or None      # empty string means EOF
    finally:
        sel.close()


def handshake(p, deadline):
    """initialize -> notifications/initialized -> tools/list."""
    def send(obj):
        p.stdin.write(json.dumps(obj) + "\n")
        p.stdin.flush()

    send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
          "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                     "clientInfo": {"name": "bench", "version": "1"}}})
    if read_line(p.stdout, deadline) is None:
        return None
    send({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
    send({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
    while True:
        line = read_line(p.stdout, deadline)
        if line is None:
            return None
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue           # servers that print a banner before the protocol
        if msg.get("id") == 2:
            return msg.get("result", {}).get("tools")


def main():
    label = sys.argv[1]
    cmd = sys.argv[sys.argv.index("--") + 1:]
    deadline = time.monotonic() + TIMEOUT_S
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL, text=True, bufsize=1)
    try:
        tools = handshake(p, deadline)
    finally:
        # A server that opens stdout and never writes a newline would otherwise
        # hang the benchmark forever, and killing without waiting leaves a
        # zombie that outlives this process.
        p.terminate()
        try:
            p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait()

    if tools is None:
        print(json.dumps({"label": label, "error": "no tools/list response "
                          f"within {TIMEOUT_S}s"}))
        sys.exit(1)

    payload = json.dumps(tools)
    print(json.dumps({
        "label": label,
        "tools": len(tools),
        "bytes": len(payload),
        "approx_tokens": len(payload) // 4,
        "per_tool": sorted(
            ({"name": t.get("name"), "bytes": len(json.dumps(t))} for t in tools),
            key=lambda d: -d["bytes"]),
    }, indent=2))


if __name__ == "__main__":
    main()
