#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Arena --watch: Herdr pane choreography, codified so it isn't rediscovered.

Standing procedure (operator rules):
1. Close stale arena panes first — .watch-panes receipts AND any pane in the
   current tab labelled arena-*/ab-* — even if not created by this run.
   The operator pane (the one running this) is never closed.
2. One column per container, all columns equal width on a single row.
   Herdr only nests binary splits, so columns are equalized afterwards by
   probing the layout (`pane resize --amount 0` returns geometry without
   moving anything) and dragging each column boundary to i/N of the width.
3. Rename the tab and each pane (arena-<arm> or arena-<arm>-r<rep>).
4. Run interactive codex pinned to the arena model in every pane.
5. Auto-answer the trust dialogs until each pane lands on the prompt.

Env: WATCH_CONTAINERS (space-separated docker names), WATCH_RESULTS,
CODEX_MODEL, CODEX_EFFORT, HERDR_PANE_ID (own pane, optional).
"""

import json
import os
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor

CONTAINERS = os.environ["WATCH_CONTAINERS"].split()
RESULTS = os.environ["WATCH_RESULTS"]
MODEL = os.environ.get("CODEX_MODEL", "gpt-5.6-terra")
EFFORT = os.environ.get("CODEX_EFFORT", "medium")
SELF_PANE = os.environ.get("HERDR_PANE_ID", "")
STALE_LABEL_PREFIXES = ("arena-", "ab-")


def herdr(*args: str) -> dict | None:
    try:
        out = subprocess.run(
            ["herdr", *args], capture_output=True, text=True, timeout=20
        )
        return json.loads(out.stdout)
    except Exception:
        return None


def pane_id(resp: dict | None) -> str:
    try:
        return resp["result"]["pane"]["pane_id"]
    except (TypeError, KeyError):
        return ""


def list_panes() -> list[dict]:
    resp = herdr("pane", "list")
    return resp["result"]["panes"] if resp else []


def layout(pane: str) -> dict:
    # A zero-delta resize reports the current layout without changing it.
    resp = herdr("pane", "resize", "--pane", pane, "--direction", "left",
                 "--amount", "0")
    try:
        return resp["result"]["resize"]["layout"]
    except (TypeError, KeyError):
        return {}


def close_stale(keep: set[str]):
    receipts = []
    try:
        receipts = open(f"{RESULTS}/.watch-panes").read().split()
    except OSError:
        pass
    for pid in receipts:
        if pid and pid not in keep:
            herdr("pane", "close", pid)
    for pane in list_panes():
        pid = pane.get("pane_id", "")
        label = pane.get("label") or pane.get("terminal_title_stripped") or ""
        if pid in keep or pid == SELF_PANE:
            continue
        if any(label.startswith(p) for p in STALE_LABEL_PREFIXES):
            herdr("pane", "close", pid)


def equalize_columns(anchor: str, want: int):
    """Drag column boundaries until every pane is ~1/want of the row width."""
    for _ in range(want * 3):
        lay = layout(anchor)
        panes = sorted(lay.get("panes", []), key=lambda p: p["rect"]["x"])
        if len(panes) != want:
            break
        width = lay["area"]["width"]
        moved = False
        # boundaries left→right; a fresh probe each drag keeps math honest
        for k in range(len(panes) - 1):
            actual = panes[k + 1]["rect"]["x"]
            target = round(width * (k + 1) / want)
            delta_cols = actual - target
            if abs(delta_cols) <= 1:
                continue
            direction = "left" if delta_cols > 0 else "right"
            herdr("pane", "resize", "--pane", panes[k + 1]["pane_id"],
                  "--direction", direction,
                  "--amount", str(round(abs(delta_cols) / width, 3)))
            moved = True
            break  # one drag per probe; geometry shifts under nested splits
        if not moved:
            break


def read_pane(pane: str) -> str:
    out = subprocess.run(["herdr", "pane", "read", pane],
                         capture_output=True, text=True, timeout=15)
    try:
        return json.loads(out.stdout)["result"]["content"]
    except Exception:
        return out.stdout  # pane read prints raw screen text


def accept_trust(pane: str):
    for _ in range(60):
        screen = read_pane(pane)[-800:]
        if "Usage limit" in screen and "› 1. Close" in screen:
            herdr("pane", "send-keys", pane, "Enter")  # dismiss, then retry loop
        elif "Trust and continue" in screen:
            herdr("pane", "send-keys", pane, "Enter")
        elif "Trust all and continue" in screen:
            herdr("pane", "send-keys", pane, "Down")
            time.sleep(1)
            herdr("pane", "send-keys", pane, "Enter")
            return
        elif "Ask Codex" in screen:
            return
        time.sleep(2)


def main() -> int:
    panes = list_panes()
    if not panes:
        print("watch: no panes visible; not in Herdr?", file=sys.stderr)
        return 1
    own = SELF_PANE or next((p["pane_id"] for p in panes if p.get("focused")),
                            panes[0]["pane_id"])
    tab = panes[0]["tab_id"]
    keep = {own}
    close_stale(keep)

    spawned: list[tuple[str, str]] = []  # (pane_id, container)
    for c in CONTAINERS:
        resp = herdr("pane", "split", "--current", "--direction", "right",
                     "--no-focus")
        pid = pane_id(resp)
        if not pid:
            print(f"WARNING: pane split failed; {c} via 'docker logs -f {c}'",
                  file=sys.stderr)
            continue
        spawned.append((pid, c))

    if not spawned:
        return 1
    equalize_columns(own, len(spawned) + 1)

    arms = []
    for pid, c in spawned:
        parts = c.split("-")          # arena-<arm>-<run_id>-<rep>
        arm = parts[1]
        rep = parts[-1]
        label = f"arena-{arm}" if len(CONTAINERS) == len(set(
            x.split("-")[1] for x in CONTAINERS)) else f"arena-{arm}-r{rep}"
        arms.append(arm)
        herdr("pane", "rename", pid, label)
        herdr("pane", "run", pid,
              f"docker exec -it {c} codex -m {MODEL} "
              f"-c model_reasoning_effort={EFFORT}")
    herdr("tab", "rename", tab, f"arena: {'|'.join(dict.fromkeys(arms))}")

    with open(f"{RESULTS}/.watch-panes", "w") as fh:
        fh.write("\n".join(pid for pid, _ in spawned) + "\n")

    with ThreadPoolExecutor(max_workers=len(spawned)) as pool:
        list(pool.map(accept_trust, [pid for pid, _ in spawned]))
    print(f"watching: {len(spawned)} herdr panes, codex {MODEL} {EFFORT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
