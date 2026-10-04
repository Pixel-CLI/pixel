# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Shared configuration for the comparison harnesses.

Kept in one place because the three benchmarks are meant to be re-run by readers
on their own machines, where nothing sits at the author's paths.
"""
import os
import shutil
import sys


def gitnexus_cli(argv=None):
    """Resolve how to invoke the GitNexus CLI, as an argv prefix.

    Order: --gitnexus-cli on the command line, then $GITNEXUS_CLI, then a
    `gitnexus` executable on PATH. A path ending in .js is run through node,
    which is how a source checkout exposes it; an installed `gitnexus` is
    executed directly. Exits with an actionable message rather than failing
    later inside a timing loop, where it would look like a tool result.
    """
    argv = sys.argv if argv is None else argv
    path = None
    if "--gitnexus-cli" in argv:
        path = argv[argv.index("--gitnexus-cli") + 1]
    path = path or os.environ.get("GITNEXUS_CLI") or shutil.which("gitnexus")
    if not path:
        sys.exit(
            "GitNexus CLI not found. Pass --gitnexus-cli <path>, set "
            "$GITNEXUS_CLI, or put `gitnexus` on PATH.\n"
            "  source checkout: --gitnexus-cli /path/to/GitNexus/gitnexus/dist/cli/index.js\n"
            "  installed:       npm i -g gitnexus"
        )
    return ["node", path] if path.endswith(".js") else [path]


def positional(argv=None):
    """Command-line arguments with the --gitnexus-cli pair removed."""
    argv = (sys.argv if argv is None else argv)[1:]
    if "--gitnexus-cli" in argv:
        i = argv.index("--gitnexus-cli")
        argv = argv[:i] + argv[i + 2:]
    return argv
