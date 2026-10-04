# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Whether a host run failed before reaching the model.

A run that never reached the model (no login, an API error before the first
turn) must stop the campaign rather than be scored as a failed answer. The
verdict reads the host's own status events and its stderr, never the model's
answer text: an answer that quotes "Not logged in" or "401 Unauthorized" was
produced by a reached model.

- Claude (`stream-json`): a `result` event whose `terminal_reason` is
  `api_error`, or an error result whose text starts with "Not logged in".
- Codex (`exec --json`): an `error` or `turn.failed` event raised before any
  item completed.
- Either host: stderr naming a missing login or a 401.

Usage: host_reached.py <out.jsonl> [<stderr file>]
Exit 0 when the model was reached, 1 when it was not.
"""
import json
import re
import sys

STDERR_FAILURE = re.compile(r"Not logged in|401 Unauthorized")


def events(text):
    for line in text.splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if isinstance(event, dict):
            yield event


def unreached(out_text, err_text=""):
    if STDERR_FAILURE.search(err_text):
        return True
    item_completed = False
    for event in events(out_text):
        kind = event.get("type")
        if kind == "result":
            if event.get("terminal_reason") == "api_error":
                return True
            if event.get("is_error") and str(event.get("result", "")).startswith("Not logged in"):
                return True
        elif kind == "item.completed":
            item_completed = True
        elif kind in ("error", "turn.failed") and not item_completed:
            return True
    return False


def read(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as handle:
            return handle.read()
    except OSError:
        return ""


def main(argv):
    if len(argv) not in (2, 3):
        print(__doc__.strip().splitlines()[-2], file=sys.stderr)
        return 2
    err = read(argv[2]) if len(argv) == 3 else ""
    return 1 if unreached(read(argv[1]), err) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
