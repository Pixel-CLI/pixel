# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Read-only deterministic fact packets for the fact-pack experiment.

The automatic arm's packet comes from `pixel scope-task --read-only`: a pure
read of a compatible warm daemon's already-published index and graph. It never
starts a daemon, builds or refreshes an index, or writes a manifest. An
unavailable, stale, malformed or timed-out response contributes no packet.

The frozen inputs (the `targets_facts` input record) identify a fact result:
task, limit, index base/delta/overlay/tombstone state, graph
generation/signature, algorithm version and the disabled activity-reranking
and semantic-fallback flags. They are recorded beside the packet, never
inside the injected bytes.
"""

import json
import subprocess

from arms import PACKET_BUDGET_BYTES


class FactPacket:
    """One fact result: its frozen inputs, its full facts, and the injected packet.

    `packet_text` is None when the result is unavailable or the arm injects
    no packet. `packet_bytes` is the length of `packet_text` in bytes, so the
    1 KiB budget is checked on bytes, not characters.
    """

    def __init__(self, status, inputs=None, facts=None, packet_text=None,
                 kept_targets=None, total_targets=None, error=None, pair_id=None):
        self.status = status  # "available" | "unavailable"
        self.inputs = inputs  # frozen TargetsFactsInputs dict, or None
        self.facts = facts  # full facts object, or None
        self.packet_text = packet_text  # injected packet text, or None
        self.packet_bytes = len(packet_text.encode("utf-8")) if packet_text is not None else 0
        self.kept_targets = kept_targets
        self.total_targets = total_targets
        self.error = error
        self.pair_id = pair_id  # task id this packet was served for, or None

    @property
    def available(self) -> bool:
        return self.status == "available"

    def within_budget(self, budget: int) -> bool:
        return self.packet_bytes <= budget


def _render_packet(facts: dict, budget: int):
    """Render a compact text packet from `facts`, capped at `budget` bytes.

    The packet carries the task's keywords and the prioritized target list
    with their reasons — the scoping board an agent needs. The full evidence
    text is not injected; it is recorded with the frozen input. Targets are
    dropped from the end (least important first) until the packet fits.
    Returns (text, kept, total).
    """
    targets = facts.get("targets", []) if isinstance(facts, dict) else []
    total = len(targets)
    keywords = facts.get("keywords", []) if isinstance(facts, dict) else []
    header = []
    if keywords:
        header.append("keywords: " + ", ".join(keywords))
    header.append("targets:")
    for keep in range(total, -1, -1):
        body = list(header)
        for target in targets[:keep]:
            path = target.get("path", "?")
            reasons = "; ".join(target.get("reasons", []))
            body.append(f"- {path} -- {reasons}" if reasons else f"- {path}")
        text = "\n".join(body) + "\n"
        if len(text.encode("utf-8")) <= budget:
            return text, keep, total
    return "", 0, total


def retrieve(task: str, limit: int, pixel_bin: str,
             budget: int | None = PACKET_BUDGET_BYTES,
             timeout: float = 30.0) -> FactPacket:
    """Retrieve the deterministic fact packet for `task`, capped at `budget` bytes.

    `budget=None` returns the full JSON fact result uncapped (the on-demand
    arm). A non-zero exit, malformed JSON, an unavailable status or a timeout
    all yield an unavailable packet that contributes no injected bytes.
    """
    cmd = [pixel_bin, "scope-task", "--read-only", task, "--json", "--limit", str(limit)]
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except FileNotFoundError:
        return FactPacket("unavailable", error=f"pixel binary not found: {pixel_bin}")
    except subprocess.TimeoutExpired:
        return FactPacket("unavailable", error=f"timed out after {timeout}s")
    if proc.returncode != 0:
        return FactPacket("unavailable",
                          error=(proc.stderr.strip() or f"exit {proc.returncode}"))
    try:
        data = json.loads(proc.stdout)
    except json.JSONDecodeError as error:
        return FactPacket("unavailable", error=f"malformed JSON: {error}")
    if not isinstance(data, dict) or data.get("status") != "available":
        status = data.get("status") if isinstance(data, dict) else "malformed"
        reason = data.get("reason") if isinstance(data, dict) else None
        return FactPacket("unavailable", error=f"status={status!r} reason={reason!r}")
    inputs = data.get("inputs")
    facts = data.get("facts")
    if not isinstance(inputs, dict) or not isinstance(facts, dict):
        return FactPacket("unavailable", error="available result missing inputs or facts")
    targets = facts.get("targets", [])
    total = len(targets) if isinstance(targets, list) else 0
    if budget is None:
        text = json.dumps(facts, separators=(",", ":"), sort_keys=True)
        return FactPacket("available", inputs=inputs, facts=facts, packet_text=text,
                          kept_targets=total, total_targets=total)
    text, kept, total = _render_packet(facts, budget)
    return FactPacket("available", inputs=inputs, facts=facts, packet_text=text,
                      kept_targets=kept, total_targets=total)
